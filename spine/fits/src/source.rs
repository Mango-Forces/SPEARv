//! Rust-owned asynchronous backing. No URL or HTTP parser enters CFITSIO.
use crate::{Error, Result};
use async_trait::async_trait;
use std::{
    collections::{HashMap, VecDeque},
    ffi::{c_char, c_int, c_long, c_void, CStr},
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tokio::{runtime::Handle, sync::Notify};

/// Cooperative cancellation; started blocking/native work is not killed.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    notify: Notify,
    workers: AtomicUsize,
    finished: Notify,
    io: Mutex<IoStats>,
}
impl Cancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
    pub fn check(&self) -> Result<()> {
        if self.0.cancelled.load(Ordering::Acquire) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    /// Number of queued/started blocking scopes still owning their leases.
    pub fn active_workers(&self) -> usize {
        self.0.workers.load(Ordering::Acquire)
    }
    /// Finalized handle totals for this job, including failures/cancellation.
    /// Peak is the maximum individual-handle cache peak, not an aggregate RSS.
    pub fn io_stats(&self) -> IoStats {
        *self.0.io.lock().unwrap_or_else(|p| p.into_inner())
    }
    /// After cancellation, wait for actual close/exit, not merely outer abort.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.0.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active_workers() == 0 {
                return;
            }
            notified.await;
        }
    }
    pub(crate) fn track(&self) -> Result<WorkerGuard> {
        self.check()?;
        self.0.workers.fetch_add(1, Ordering::AcqRel);
        let guard = WorkerGuard(self.clone());
        self.check()?;
        Ok(guard)
    }
    pub async fn until_cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check().is_err() {
                return;
            }
            notified.await;
        }
    }
    pub(crate) async fn wait<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::select! { biased;
            _ = self.until_cancelled() => Err(Error::Cancelled),
            result = work => { self.check()?; result }
        }
    }
}
pub(crate) struct WorkerGuard(Cancellation);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0 .0.workers.fetch_sub(1, Ordering::AcqRel);
        self.0 .0.finished.notify_waiters();
    }
}

/// Per-job attempt accounting survives failed or cancelled read futures.
/// Reader records each attempt before sending and bytes as each chunk arrives.
#[derive(Default)]
pub struct ReadTrace {
    attempts: AtomicU64,
    bytes: AtomicU64,
}
impl ReadTrace {
    pub fn attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn received(&self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}
/// Reader pins logical size. Condition content when a validator is available;
/// unconditioned reads stay job-local and must not claim exact content
/// identity or cross-job cache reuse. Validate HTTP status/interval/total and
/// applicable identity, and cap streamed bodies before returning.
/// Never replace an exact range with a full object. Retries belong here, not C.
#[async_trait]
pub trait ReadAt: Send + Sync + 'static {
    async fn read_at(
        &self,
        start: u64,
        len: usize,
        cancel: &Cancellation,
        trace: &ReadTrace,
    ) -> Result<Vec<u8>>;

    /// Pages of this object already read under the same validator and size.
    /// They stay pinned in the handle cache and count against `cache_bytes`.
    fn cached_pages(&self) -> Arc<[(u64, Arc<[u8]>)]> {
        Arc::from([])
    }
}

/// Per-handle counters, not subtraction of a shared source counter.
#[derive(Debug, Default, Clone, Copy)]
pub struct IoStats {
    pub callbacks: u64,
    pub backing_reads: u64,
    pub attempts: u64,
    pub transferred_bytes: u64,
    pub cache_hits: u64,
    pub resident_bytes: usize,
    pub peak_resident_bytes: usize,
    pub closes: u64,
}
struct Cursor {
    position: u64,
    pages: VecDeque<(u64, Arc<[u8]>, bool)>,
    stats: IoStats,
}
pub(crate) struct Context {
    pub id: i32,
    size: u64,
    reader: Arc<dyn ReadAt>,
    runtime: Handle,
    pub cancel: Cancellation,
    page_bytes: usize,
    cache_bytes: usize,
    cursor: Mutex<Cursor>,
    cause: Mutex<Option<Error>>,
    opened: AtomicBool,
    trace: ReadTrace,
    finalized: AtomicBool,
}
fn registry() -> &'static Mutex<HashMap<i32, Arc<Context>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<i32, Arc<Context>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}
impl Context {
    pub fn new(
        size: u64,
        reader: Arc<dyn ReadAt>,
        runtime: Handle,
        cancel: Cancellation,
        page_bytes: usize,
        cache_bytes: usize,
    ) -> Result<Arc<Self>> {
        static NEXT: AtomicI32 = AtomicI32::new(1);
        let id = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Bounds("native driver handle space exhausted".into()))?;
        let cached = reader.cached_pages();
        let mut pages = VecDeque::with_capacity(cached.len());
        let mut resident_bytes = 0usize;
        for (offset, bytes) in cached.iter() {
            if bytes.is_empty()
                || offset
                    .checked_add(bytes.len() as u64)
                    .is_none_or(|end| end > size)
                || resident_bytes
                    .checked_add(bytes.len())
                    .is_none_or(|n| n > cache_bytes)
            {
                return Err(Error::Bounds(
                    "cached page exceeds object or cache bounds".into(),
                ));
            }
            resident_bytes += bytes.len();
            pages.push_back((*offset, Arc::clone(bytes), true));
        }
        let context = Arc::new(Self {
            id,
            size,
            reader,
            runtime,
            cancel,
            page_bytes,
            cache_bytes,
            cursor: Mutex::new(Cursor {
                position: 0,
                pages,
                stats: IoStats {
                    resident_bytes,
                    peak_resident_bytes: resident_bytes,
                    ..IoStats::default()
                },
            }),
            cause: Mutex::new(None),
            opened: AtomicBool::new(false),
            trace: ReadTrace::default(),
            finalized: AtomicBool::new(false),
        });
        registry()
            .lock()
            .map_err(|_| Error::CallbackPanic)?
            .insert(id, context.clone());
        Ok(context)
    }
    pub fn finish(&self) -> IoStats {
        if let Ok(mut map) = registry().lock() {
            map.remove(&self.id);
        }
        if self.finalized.swap(true, Ordering::AcqRel) {
            return self.stats();
        }
        let stats = self.stats();
        {
            let mut cursor = self.cursor.lock().unwrap_or_else(|p| p.into_inner());
            cursor.pages.clear();
            cursor.stats.resident_bytes = 0;
        }
        let mut total = self.cancel.0.io.lock().unwrap_or_else(|p| p.into_inner());
        total.callbacks += stats.callbacks;
        total.backing_reads += stats.backing_reads;
        total.attempts += stats.attempts;
        total.transferred_bytes += stats.transferred_bytes;
        total.cache_hits += stats.cache_hits;
        total.closes += stats.closes;
        total.peak_resident_bytes = total.peak_resident_bytes.max(stats.peak_resident_bytes);
        self.stats()
    }
    pub fn stats(&self) -> IoStats {
        let mut stats = self.cursor.lock().unwrap_or_else(|p| p.into_inner()).stats;
        stats.attempts = self.trace.attempts.load(Ordering::Relaxed);
        stats.transferred_bytes = self.trace.bytes.load(Ordering::Relaxed);
        stats
    }
    pub fn pages(&self) -> Arc<[(u64, Arc<[u8]>)]> {
        self.cursor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pages
            .iter()
            .map(|(offset, bytes, _)| (*offset, Arc::clone(bytes)))
            .collect::<Vec<_>>()
            .into()
    }
    pub fn take_cause(&self) -> Option<Error> {
        self.cause.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
    fn fail(&self, error: Error) {
        let mut cause = self.cause.lock().unwrap_or_else(|p| p.into_inner());
        if cause.is_none() {
            *cause = Some(error);
        }
    }
    fn read(&self, output: &mut [u8]) -> Result<()> {
        self.cancel.check()?;
        // Only this exclusive handle's cursor is locked. The registry is not.
        let mut cursor = self.cursor.lock().map_err(|_| Error::CallbackPanic)?;
        cursor.stats.callbacks += 1;
        let end = cursor
            .position
            .checked_add(output.len() as u64)
            .filter(|end| *end <= self.size)
            .ok_or(Error::ShortRead)?;
        let mut copied = 0;
        while cursor.position < end {
            self.cancel.check()?;
            let index = cursor.pages.iter().position(|(offset, bytes, _)| {
                *offset <= cursor.position && cursor.position < *offset + bytes.len() as u64
            });
            let index = match index {
                Some(index) => {
                    cursor.stats.cache_hits += 1;
                    index
                }
                None => {
                    let start = cursor.position;
                    let next = cursor
                        .pages
                        .iter()
                        .map(|(offset, _, _)| *offset)
                        .filter(|offset| *offset > start)
                        .min()
                        .unwrap_or(self.size);
                    let len = (next - start).min(self.page_bytes as u64) as usize;
                    while cursor.stats.resident_bytes + len > self.cache_bytes {
                        let victim = cursor
                            .pages
                            .iter()
                            .position(|(_, _, pinned)| !*pinned)
                            .unwrap_or(0);
                        let (_, old, _) = cursor
                            .pages
                            .remove(victim)
                            .ok_or_else(|| Error::Bounds("cache cannot hold a page".into()))?;
                        cursor.stats.resident_bytes -= old.len();
                    }
                    cursor.stats.backing_reads += 1;
                    let bytes = self.runtime.block_on(self.cancel.wait(self.reader.read_at(
                        start,
                        len,
                        &self.cancel,
                        &self.trace,
                    )))?;
                    if bytes.len() != len {
                        return Err(Error::ShortRead);
                    }
                    // A reader returning an oversized allocation cannot bypass the cache charge.
                    if bytes.capacity() > self.page_bytes {
                        return Err(Error::Bounds(
                            "reader allocation exceeds admitted page capacity".into(),
                        ));
                    }
                    let bytes: Arc<[u8]> = bytes.into();
                    while cursor.stats.resident_bytes + bytes.len() > self.cache_bytes {
                        let victim = cursor
                            .pages
                            .iter()
                            .position(|(_, _, pinned)| !*pinned)
                            .unwrap_or(0);
                        let (_, old, _) = cursor.pages.remove(victim).ok_or_else(|| {
                            Error::Bounds("cache cannot hold reader allocation".into())
                        })?;
                        cursor.stats.resident_bytes -= old.len();
                    }
                    cursor.stats.resident_bytes += bytes.len();
                    cursor.stats.peak_resident_bytes = cursor
                        .stats
                        .peak_resident_bytes
                        .max(cursor.stats.resident_bytes);
                    cursor.pages.push_back((start, bytes, false));
                    cursor.pages.len() - 1
                }
            };
            let (start, ref page, _) = cursor.pages[index];
            let offset = (cursor.position - start) as usize;
            let n = (output.len() - copied).min(page.len() - offset);
            output[copied..copied + n].copy_from_slice(&page[offset..offset + n]);
            cursor.position += n as u64;
            copied += n;
        }
        Ok(())
    }
}
fn callback(id: i32, work: impl FnOnce(&Context) -> Result<()>) -> c_int {
    // Clone then release registry lock before I/O, cancellation or user reader code.
    let context = match registry().lock().ok().and_then(|map| map.get(&id).cloned()) {
        Some(context) => context,
        None => return 104,
    };
    match catch_unwind(AssertUnwindSafe(|| work(&context))) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            context.fail(error);
            108
        }
        Err(_) => {
            context.fail(Error::CallbackPanic);
            108
        }
    }
}
#[no_mangle]
extern "C" fn sv_rs_open(name: *mut c_char, mode: c_int, out: *mut c_int) -> c_int {
    // C always supplies NUL-terminated token storage and a valid integer output.
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let name = unsafe { CStr::from_ptr(name) }.to_str().ok()?;
        name.strip_prefix("spearvfits://")
            .unwrap_or(name)
            .parse::<i32>()
            .ok()
    }));
    let id = match parsed {
        Ok(Some(id)) => id,
        _ => return 104,
    };
    callback(id, |context| {
        context.cancel.check()?;
        if mode != 0 || context.opened.swap(true, Ordering::AcqRel) {
            return Err(Error::Bounds("driver is read-only and exclusive".into()));
        }
        unsafe {
            *out = id;
        }
        Ok(())
    })
}
#[no_mangle]
extern "C" fn sv_rs_close(id: c_int) -> c_int {
    // Close must succeed even after cancellation/error. Context survives in owner.
    callback(id, |context| {
        if !context.opened.swap(false, Ordering::AcqRel) {
            return Err(Error::Bounds("native driver double close".into()));
        }
        context
            .cursor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stats
            .closes += 1;
        registry()
            .lock()
            .map_err(|_| Error::CallbackPanic)?
            .remove(&id);
        Ok(())
    })
}
#[no_mangle]
extern "C" fn sv_rs_size(id: c_int, size: *mut i64) -> c_int {
    callback(id, |context| {
        unsafe {
            *size = context.size as i64;
        }
        Ok(())
    })
}
#[no_mangle]
extern "C" fn sv_rs_seek(id: c_int, pos: i64) -> c_int {
    callback(id, |context| {
        context.cancel.check()?;
        if pos < 0 || pos as u64 > context.size {
            return Err(Error::Bounds("seek outside logical object".into()));
        }
        context
            .cursor
            .lock()
            .map_err(|_| Error::CallbackPanic)?
            .position = pos as u64;
        Ok(())
    })
}
#[no_mangle]
extern "C" fn sv_rs_read(id: c_int, out: *mut c_void, len: c_long) -> c_int {
    callback(id, |context| {
        if len < 0 {
            return Err(Error::Bounds("negative native read".into()));
        }
        // CFITSIO owns a valid destination of len bytes for this synchronous call.
        context.read(unsafe { std::slice::from_raw_parts_mut(out.cast::<u8>(), len as usize) })
    })
}
