//! Process-wide admission control for memory-heavy work.
//!
//! Several independent subsystems share one process. Each bounding only
//! itself is not enough: together they can take the process past the
//! container limit, which kills every in-flight job at once because the state
//! is all in memory.
//!
//! [`WorkBudget`] is the one admission point. A job estimates its peak heap,
//! takes that many mebibytes from the shared [`Semaphore`], and releases them
//! when the job ends. Admission is non-blocking: a job that does not fit is
//! refused with [`Error::Busy`](crate::Error::Busy) (HTTP 429) instead of
//! queueing in a handler. Per-system caps remain as a second line.
//!
//! # Where the default comes from
//!
//! The host has about 32 GiB RAM. Twenty GiB is admitted to jobs; the
//! remaining headroom covers long-lived caches, the 32-entry response cache,
//! PNG buffers, the allocator's slack, and the HTTP and index clients.
//!
//! A deployment with a different memory limit overrides the total with
//! `SPEARV_WORK_BUDGET_MIB` (a number, parsed fail-fast like every other
//! `SPEARV_*` count). The code that sizes each job lives next to the job.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::Error;

/// Default total admission budget, MiB. See the module docs for the arithmetic.
pub const DEFAULT_MIB: u64 = 20 * 1024;

/// Admission granularity. Whole-job callers still charge whole mebibytes;
/// many small library objects (tile sums, snapshots, geometry) are charged in
/// KiB so per-object rounding cannot exhaust the budget while bytes still fit.
const KIB: u64 = 1024;
const KIB_PER_MIB: u64 = 1024;

/// Process-wide memory admission shared through [`Context`](crate::Context).
/// The semaphore counts KiB.
pub struct WorkBudget {
    available: Arc<Semaphore>,
    total_mib: u64,
}

impl Default for WorkBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MIB)
    }
}

impl WorkBudget {
    /// A budget of `total_mib` mebibytes. Clamped to at least one mebibyte so a
    /// zero-sized budget cannot deadlock every admission test.
    pub fn new(total_mib: u64) -> Self {
        let total_mib = total_mib.clamp(1, u32::MAX as u64);
        Self {
            available: Arc::new(Semaphore::new((total_mib * KIB_PER_MIB) as usize)),
            total_mib,
        }
    }

    /// The deployment's budget: `SPEARV_WORK_BUDGET_MIB`, or `DEFAULT_MIB`.
    ///
    /// # Panics
    /// When the variable is set but is not a number, like every other
    /// configuration count.
    pub fn from_env() -> Self {
        Self::new(crate::env::number("SPEARV_WORK_BUDGET_MIB", DEFAULT_MIB))
    }

    pub fn total_mib(&self) -> u64 {
        self.total_mib
    }

    /// Whole free mebibytes, rounded down.
    pub fn available_mib(&self) -> u64 {
        self.available.available_permits() as u64 / KIB_PER_MIB
    }

    /// Free bytes at the KiB admission granularity.
    pub fn available_bytes(&self) -> u64 {
        self.available.available_permits() as u64 * KIB
    }

    /// Take `mib` mebibytes without waiting. `None` means the request does not
    /// fit right now; the caller turns that into [`Error::Busy`].
    pub fn try_acquire(&self, mib: u64) -> Option<WorkPermit> {
        // A job always charges at least one mebibyte, so a zero-byte estimate
        // cannot bypass admission entirely.
        self.acquire_kib(mib.max(1).checked_mul(KIB_PER_MIB)?)
    }

    /// Take `bytes` rounded up to the KiB admission unit (at least one KiB),
    /// for many small objects whose MiB rounding would overstate their sum.
    pub fn try_acquire_exact(&self, bytes: u64) -> Option<WorkPermit> {
        self.acquire_kib(bytes.div_ceil(KIB).max(1))
    }

    fn acquire_kib(&self, kib: u64) -> Option<WorkPermit> {
        let permits = u32::try_from(kib).ok()?;
        let permit = Arc::clone(&self.available)
            .try_acquire_many_owned(permits)
            .ok()?;
        Some(WorkPermit { _permit: permit })
    }

    /// [`try_acquire`](Self::try_acquire) for a byte estimate.
    pub fn try_acquire_bytes(&self, bytes: u64) -> Option<WorkPermit> {
        self.try_acquire(mebibytes(bytes))
    }

    /// The 429 error for `bytes` that did not fit, naming the numbers so the
    /// caller knows whether to retry or to ask for less work.
    pub fn busy(&self, bytes: u64) -> Error {
        Error::Busy(format!(
            "the server is at its work budget: this job needs {} MiB and {} MiB of {} MiB are free; \
             retry when a running job finishes",
            mebibytes(bytes),
            self.available_mib(),
            self.total_mib
        ))
    }
}

/// Admission held until the job ends. Dropping it returns the bytes.
#[derive(Debug)]
pub struct WorkPermit {
    _permit: OwnedSemaphorePermit,
}

/// Bytes rounded up to the mebibyte admission unit.
pub fn mebibytes(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_budget_refuses_admission_until_a_permit_drops() {
        let budget = WorkBudget::new(4);
        let first = budget.try_acquire(3).expect("three of four");
        assert!(budget.try_acquire(2).is_none(), "two more do not fit in the last mebibyte");
        assert_eq!(budget.available_mib(), 1);
        drop(first);
        assert_eq!(budget.available_mib(), 4);
        assert!(budget.try_acquire(4).is_some());
    }

    #[test]
    fn a_tiny_job_still_charges_one_mebibyte() {
        let budget = WorkBudget::new(1);
        let _held = budget.try_acquire_bytes(1).expect("the one mebibyte");
        assert!(budget.try_acquire_bytes(0).is_none());
    }

    #[test]
    fn small_objects_are_charged_in_kib_not_whole_mebibytes() {
        // Six 153 KiB edge-tile snapshots fit in one MiB; MiB rounding would
        // admit only one.
        let budget = WorkBudget::new(1);
        let held: Vec<_> = (0..6)
            .map(|_| budget.try_acquire_exact(156_672).expect("153 KiB fits"))
            .collect();
        assert_eq!(budget.available_bytes(), 1024 * 1024 - 6 * 156_672);
        assert!(budget.try_acquire_exact(110 * 1024).is_none());
        assert!(budget.try_acquire_exact(106 * 1024).is_some());
        drop(held);
        assert_eq!(budget.available_mib(), 1);
        let _one = budget.try_acquire_exact(1).expect("one KiB");
        assert_eq!(budget.available_bytes(), 1023 * 1024);
        assert_eq!(budget.available_mib(), 0);
    }

    #[test]
    fn the_busy_error_names_the_request_and_the_remaining_budget() {
        let budget = WorkBudget::new(8);
        let _all = budget.try_acquire(8).expect("the whole budget");
        let text = budget.busy(2 * 1024 * 1024).to_string();
        assert!(text.contains("2 MiB"), "{text}");
        assert!(text.contains("0 MiB of 8 MiB"), "{text}");
    }
}
