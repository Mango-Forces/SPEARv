//! Byte-denominated ownership for adapter allocations and retained outputs.
use crate::{Error, Result};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Caller-selected ceiling. Caller integration must use the process admission
/// budget, not create a separate budget for every source/job.
#[derive(Clone)]
pub struct Budget(Arc<State>);
struct State {
    ceiling: u64,
    used: AtomicU64,
}
impl Budget {
    pub fn new(bytes: u64) -> Self {
        Self(Arc::new(State {
            ceiling: bytes,
            used: AtomicU64::new(0),
        }))
    }
    pub fn used_bytes(&self) -> u64 {
        self.0.used.load(Ordering::Acquire)
    }
    pub fn capacity_bytes(&self) -> u64 {
        self.0.ceiling
    }
    pub(crate) fn charge(&self, bytes: usize) -> Result<Charge> {
        self.0
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes as u64)
                    .filter(|n| *n <= self.0.ceiling)
            })
            .map_err(|_| Error::Budget {
                requested: bytes as u64,
                available: self.0.ceiling.saturating_sub(self.used_bytes()),
            })?;
        Ok(Charge {
            budget: self.clone(),
            bytes: bytes as u64,
        })
    }
}
#[derive(Debug)]
pub(crate) struct Charge {
    budget: Budget,
    bytes: u64,
}
impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("capacity", &self.capacity_bytes())
            .field("used", &self.used_bytes())
            .finish()
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
