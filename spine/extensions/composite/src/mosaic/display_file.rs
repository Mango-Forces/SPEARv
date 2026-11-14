//! Bounded PNG encoding and an owned disk cache. Full images never become a Vec.

use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use tempfile::NamedTempFile;
use tokio::sync::Semaphore;

use super::display_revision::{ArtifactState, Revision};
use super::Job;

const MAX_FILES: usize = 2;
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(super) const PREVIEW_EDGE: u32 = 2048;

pub(super) struct PublishedFile {
    pub path: PathBuf,
    pub bytes: u64,
    cache: Weak<DisplayCache>,
}

impl PublishedFile {
    pub(super) fn unlink(&self) -> Result<(), String> {
        self.cache
            .upgrade()
            .map_or(Ok(()), |cache| cache.unlink_owned(&self.path))
    }
}

impl Drop for PublishedFile {
    fn drop(&mut self) {
        let _ = self.unlink();
    }
}

struct Entry {
    revision: Weak<Revision>,
    path: PathBuf,
    bytes: u64,
}

#[derive(Default)]
struct CacheState {
    directory: Option<PathBuf>,
    owned_files: HashSet<PathBuf>,
    active_producers: usize,
}

pub(crate) struct DisplayCache {
    pub full_slot: Arc<Semaphore>,
    root: PathBuf,
    lifecycle: Mutex<()>,
    closed: AtomicBool,
    state: Mutex<CacheState>,
    idle: Condvar,
    entries: Mutex<VecDeque<Entry>>,
    next_file: AtomicU64,
    max_bytes: u64,
    max_files: usize,
}

impl DisplayCache {
    fn new(root: PathBuf, max_bytes: u64, max_files: usize) -> Self {
        Self {
            full_slot: Arc::new(Semaphore::new(1)),
            root,
            lifecycle: Mutex::new(()),
            closed: AtomicBool::new(false),
            state: Mutex::new(CacheState::default()),
            idle: Condvar::new(),
            entries: Mutex::new(VecDeque::new()),
            next_file: AtomicU64::new(0),
            max_bytes,
            max_files,
        }
    }

    pub(super) fn shared() -> Arc<Self> {
        static CACHE: OnceLock<Arc<DisplayCache>> = OnceLock::new();
        Arc::clone(CACHE.get_or_init(|| {
            Arc::new(Self::new(
                std::env::var_os("SPEARV_DISPLAY_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "data/display".into()),
                MAX_BYTES,
                MAX_FILES,
            ))
        }))
    }

    #[cfg(test)]
    pub(super) fn fixture(root: &Path, max_bytes: u64, max_files: usize) -> Arc<Self> {
        Arc::new(Self::new(root.into(), max_bytes, max_files))
    }

    #[cfg(test)]
    pub(super) fn owned_path(&self) -> PathBuf {
        self.directory().expect("owned display directory")
    }

    #[cfg(test)]
    fn directory(&self) -> Result<PathBuf, String> {
        let _lifecycle = self.lifecycle.lock().expect("display cache lifecycle");
        if self.closed.load(Ordering::Acquire) {
            return Err("display cache is closed".into());
        }
        let mut state = self.state.lock().expect("display cache state");
        if let Some(directory) = &state.directory {
            return Ok(directory.clone());
        }
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        let directory = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(&self.root)
            .map_err(|e| e.to_string())?
            .keep();
        state.directory = Some(directory.clone());
        Ok(directory)
    }

    fn begin_producer(&self) -> Result<Producer<'_>, String> {
        let _lifecycle = self.lifecycle.lock().expect("display cache lifecycle");
        if self.closed.load(Ordering::Acquire) {
            return Err("display cache is closed".into());
        }
        self.state
            .lock()
            .expect("display cache state")
            .active_producers += 1;
        Ok(Producer { cache: self })
    }

    fn temporary(&self) -> Result<NamedTempFile, String> {
        let _lifecycle = self.lifecycle.lock().expect("display cache lifecycle");
        if self.closed.load(Ordering::Acquire) {
            return Err("display cache is closed".into());
        }
        let mut state = self.state.lock().expect("display cache state");
        let directory = match &state.directory {
            Some(directory) => directory.clone(),
            None => {
                std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
                let directory = tempfile::Builder::new()
                    .prefix("run-")
                    .tempdir_in(&self.root)
                    .map_err(|e| e.to_string())?
                    .keep();
                state.directory = Some(directory.clone());
                directory
            }
        };
        let temporary = tempfile::Builder::new()
            .prefix("full-")
            .suffix(".part")
            .tempfile_in(directory)
            .map_err(|e| e.to_string())?;
        state.owned_files.insert(temporary.path().to_owned());
        Ok(temporary)
    }

    fn persist(&self, temporary: NamedTempFile, path: &Path) -> Result<(), String> {
        let temporary_path = temporary.path().to_owned();
        let _lifecycle = self.lifecycle.lock().expect("display cache lifecycle");
        if self.closed.load(Ordering::Acquire) {
            drop(temporary);
            self.forget_missing(&temporary_path);
            return Err("display cache is closed".into());
        }
        match temporary.persist_noclobber(path) {
            Ok(_) => {
                let mut state = self.state.lock().expect("display cache state");
                state.owned_files.remove(&temporary_path);
                state.owned_files.insert(path.to_owned());
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                drop(error.file);
                self.forget_missing(&temporary_path);
                Err(message)
            }
        }
    }

    fn forget_missing(&self, path: &Path) {
        if !path.exists() {
            self.state
                .lock()
                .expect("display cache state")
                .owned_files
                .remove(path);
        }
    }

    fn unlink_owned(&self, path: &Path) -> Result<(), String> {
        let mut state = self.state.lock().expect("display cache state");
        if !state.owned_files.contains(path) {
            return Ok(());
        }
        match std::fs::remove_file(path) {
            Ok(()) => {
                state.owned_files.remove(path);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                state.owned_files.remove(path);
                Ok(())
            }
            Err(error) => Err(format!(
                "could not remove owned display file {}: {error}",
                path.display()
            )),
        }
    }

    pub(super) fn close(&self) {
        let _lifecycle = self.lifecycle.lock().expect("display cache lifecycle");
        self.closed.store(true, Ordering::Release);
        self.full_slot.close();
    }

    pub(super) fn cleanup(&self) -> Result<(), Vec<String>> {
        let mut state = self.state.lock().expect("display cache state");
        while state.active_producers != 0 {
            state = self.idle.wait(state).expect("display cache state");
        }
        let files = state.owned_files.iter().cloned().collect::<Vec<_>>();
        drop(state);

        let mut errors = files
            .iter()
            .filter_map(|path| self.unlink_owned(path).err())
            .collect::<Vec<_>>();
        let mut state = self.state.lock().expect("display cache state");
        if state.owned_files.is_empty() {
            if let Some(directory) = state.directory.clone() {
                match std::fs::remove_dir(&directory) {
                    Ok(()) => state.directory = None,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        state.directory = None;
                    }
                    Err(error) => errors.push(format!(
                        "could not remove owned display directory {}: {error}",
                        directory.display()
                    )),
                }
            }
        }
        if errors.is_empty() {
            return Ok(());
        }
        Err(errors)
    }

    /// Rename first; only this method can publish Ready. Eviction unlinks owned
    /// paths, never truncates them, so previously opened Linux streams stay valid.
    pub(super) fn publish(
        &self,
        revision: &Arc<Revision>,
        file: Arc<PublishedFile>,
    ) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock().expect("display cache lifecycle");
        if self.closed.load(Ordering::Acquire) {
            return Err("display cache is closed".into());
        }
        let mut entries = self.entries.lock().expect("display cache");
        entries.retain(|entry| {
            entry.revision.upgrade().is_some_and(|revision| {
                revision
                    .artifacts
                    .lock()
                    .expect("display artifacts")
                    .file
                    .as_ref()
                    .is_some_and(|file| file.path == entry.path)
            })
        });
        if !revision.current.load(Ordering::Acquire) {
            return Ok(());
        }
        if file.bytes > self.max_bytes || self.max_files == 0 {
            return Err("display PNG exceeds the disk cache limit".into());
        }
        // Make room before marking the new file Ready. Metadata references held
        // by a GET must not postpone unlinking and defeat the pathname/byte cap.
        while entries.len() + 1 > self.max_files
            || entries.iter().map(|e| e.bytes).sum::<u64>() + file.bytes > self.max_bytes
        {
            let entry = entries.front().expect("cache is over budget");
            if let Some(revision) = entry.revision.upgrade() {
                let mut artifacts = revision.artifacts.lock().expect("display artifacts");
                if artifacts
                    .file
                    .as_ref()
                    .is_some_and(|file| file.path == entry.path)
                {
                    artifacts
                        .file
                        .as_ref()
                        .expect("known cached file")
                        .unlink()?;
                    artifacts.file = None;
                    artifacts.full.state = ArtifactState::Evicted;
                    artifacts.full.bytes = None;
                    artifacts.full.completed_rows = 0;
                }
            }
            entries.pop_front();
        }
        let mut artifacts = revision.artifacts.lock().expect("display artifacts");
        if !revision.current.load(Ordering::Acquire) {
            return Ok(());
        }
        artifacts.full.state = ArtifactState::Ready;
        artifacts.full.bytes = Some(file.bytes);
        artifacts.full.completed_rows = artifacts.full.total_rows;
        artifacts.file = Some(Arc::clone(&file));
        entries.push_back(Entry {
            revision: Arc::downgrade(revision),
            path: file.path.clone(),
            bytes: file.bytes,
        });
        Ok(())
    }

    pub(super) fn touch(&self, path: &Path) {
        let mut entries = self.entries.lock().expect("display cache");
        if let Some(index) = entries.iter().position(|entry| entry.path == path) {
            let entry = entries.remove(index).expect("known entry");
            entries.push_back(entry);
        }
    }

    pub(super) fn render_full(
        self: &Arc<Self>,
        job: &Job,
        revision: &Revision,
    ) -> Result<Arc<PublishedFile>, String> {
        let _producer = self.begin_producer()?;
        let mut temporary = self.temporary()?;
        let temporary_path = temporary.path().to_owned();
        let encoded = (|| {
            let limited = LimitedWriter {
                inner: temporary.as_file_mut(),
                written: 0,
                limit: self.max_bytes,
            };
            encode(
                job,
                revision,
                job.grid.width,
                job.grid.height,
                limited,
                false,
            )?;
            temporary.as_file().sync_all().map_err(|e| e.to_string())?;
            check_current(job, revision)?;
            temporary
                .as_file()
                .metadata()
                .map_err(|e| e.to_string())
                .map(|metadata| metadata.len())
        })();
        let bytes = match encoded {
            Ok(bytes) => bytes,
            Err(error) => {
                drop(temporary);
                self.forget_missing(&temporary_path);
                return Err(error);
            }
        };
        let directory = self
            .state
            .lock()
            .expect("display cache state")
            .directory
            .clone()
            .expect("producer has an owned directory");
        let path = directory.join(format!(
            "full-{}.png",
            self.next_file.fetch_add(1, Ordering::Relaxed)
        ));
        self.persist(temporary, &path)?;
        Ok(Arc::new(PublishedFile {
            path,
            bytes,
            cache: Arc::downgrade(self),
        }))
    }
}

struct Producer<'a> {
    cache: &'a DisplayCache,
}

impl Drop for Producer<'_> {
    fn drop(&mut self) {
        let mut state = self.cache.state.lock().expect("display cache state");
        state.active_producers -= 1;
        if state.active_producers == 0 {
            self.cache.idle.notify_all();
        }
    }
}

struct LimitedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}
impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written + bytes.len() as u64 > self.limit {
            return Err(io::Error::other(
                "display PNG exceeds the disk cache byte limit",
            ));
        }
        let count = self.inner.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(super) fn preview_dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= PREVIEW_EDGE {
        return (width, height);
    }
    let scaled = |dimension: u32| {
        ((u64::from(dimension) * u64::from(PREVIEW_EDGE) + u64::from(longest) / 2)
            / u64::from(longest))
        .max(1) as u32
    };
    (scaled(width), scaled(height))
}

pub(super) fn render_preview(job: &Job, revision: &Revision) -> Result<Arc<[u8]>, String> {
    let (width, height) = preview_dimensions(job.grid.width, job.grid.height);
    let mut bytes = Vec::new();
    encode(job, revision, width, height, &mut bytes, true)?;
    Ok(bytes.into())
}

fn check_current(job: &Job, revision: &Revision) -> Result<(), String> {
    if !revision.current.load(Ordering::Acquire) || job.cancel.load(Ordering::Relaxed) {
        return Err("display revision was superseded or its mosaic was evicted".into());
    }
    Ok(())
}

pub(super) fn encode(
    job: &Job,
    revision: &Revision,
    width: u32,
    height: u32,
    output: impl Write,
    preview: bool,
) -> Result<(), String> {
    let mut encoder = png::Encoder::new(output, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    let mut stream = writer
        .stream_writer_with_size(64 * 1024)
        .map_err(|e| e.to_string())?;
    // One RGB row, regardless of native raster size. PNG owns only bounded
    // compression/chunk buffers. The science field is borrowed under one row lock.
    let mut row = vec![0; width as usize * 3];
    for y in 0..height {
        check_current(job, revision)?;
        job.touch_activity();
        fill_row(job, revision, width, height, y, &mut row);
        stream.write_all(&row).map_err(|e| e.to_string())?;
        let mut artifacts = revision.artifacts.lock().expect("display artifacts");
        let artifact = if preview {
            &mut artifacts.preview
        } else {
            &mut artifacts.full
        };
        artifact.completed_rows = y + 1;
    }
    stream.finish().map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    check_current(job, revision)
}

fn fill_row(job: &Job, revision: &Revision, width: u32, height: u32, y: u32, row: &mut [u8]) {
    // Center-nearest display pixels, not averaged physical values or newly
    // estimated percentiles. At native size this selects each original pixel.
    // Published tiles are immutable; no progress or math lock is held here.
    let native_y =
        ((u64::from(y) * 2 + 1) * u64::from(job.grid.height) / (u64::from(height) * 2)) as u32;
    let grid_y = job.grid.height - 1 - native_y;
    let outputs = job.outputs.lock().expect("mosaic outputs");
    let tiles = job.tiles_len();
    for x in 0..width {
        let native_x =
            ((u64::from(x) * 2 + 1) * u64::from(job.grid.width) / (u64::from(width) * 2)) as u32;
        row[x as usize * 3..x as usize * 3 + 3].copy_from_slice(
            &revision.resolved.pixel([0, 1, 2].map(|channel| {
                Job::value_from(&outputs, job.cols, job.grid.height, channel, tiles, native_x, grid_y)
            })),
        );
    }
}
