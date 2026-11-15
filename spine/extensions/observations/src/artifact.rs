//! Per-boot numerical artifact ownership.
//!
//! An artifact is the calibrated value/variance frame plus its sparse count
//! sidecar and aggregate summary, keyed by the canonical sector fingerprint. It
//! never contains PNGs or a display mapping: each sequence freezes its own
//! mapping and encodes tiles from the shared frame under that mapping.
use crate::reduction::SectorImage;
use crate::sequence::{Frame, FrameFile};
use crate::types::{BandContribution, TileCoordinate};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

pub struct Artifact {
    directory: PathBuf,
    pub fingerprint: String,
    pub frame: Arc<Frame>,
    pub complete_pixels: u64,
    pub target_coverage: f64,
    pub data_tiles: Vec<TileCoordinate>,
    pub contributions: Vec<BandContribution>,
    pub count_bytes: u64,
}

/// A staging directory that removes itself unless it becomes an artifact. This
/// covers aborted sequence tasks as well as early build failures, so a deleted
/// sequence does not leave an unreferenced partial artifact under the root.
pub(crate) struct Staging {
    path: PathBuf,
    owned: bool,
}

impl Staging {
    pub(crate) fn create(root: &Path, fingerprint: &str) -> Result<Self, String> {
        let path = root.join(format!(
            "{}-{}-artifact-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            fingerprint
        ));
        std::fs::create_dir_all(&path).map_err(|error| {
            format!(
                "Could not create artifact directory {}: {error}",
                path.display()
            )
        })?;
        Ok(Self { path, owned: true })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn take(mut self) -> PathBuf {
        self.owned = false;
        self.path.clone()
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Finish a staged artifact. On success the returned `Artifact` owns the
/// directory; on failure the staging guard removes it.
pub(crate) fn create(
    staging: Staging,
    fingerprint: String,
    image: SectorImage,
    contributions: Vec<BandContribution>,
    count_bytes: u64,
    width: u32,
    height: u32,
) -> Result<Arc<Artifact>, String> {
    let counts = staging.path().join("counts.bin");
    if !counts.is_file() {
        return Err(format!(
            "Artifact {} has no count sidecar",
            staging.path().display()
        ));
    }
    let complete_pixels = image.complete_pixels;
    let target_coverage = image.target_coverage(width, height);
    let frame = Frame::Disk(FrameFile::create(
        &staging.path().join("frame.bin"),
        image.tiles,
    )?);
    let frame = Arc::new(frame);
    let data_tiles = frame.data_tiles();
    Ok(Arc::new(Artifact {
        directory: staging.take(),
        fingerprint,
        frame,
        complete_pixels,
        target_coverage,
        data_tiles,
        contributions,
        count_bytes,
    }))
}

impl Drop for Artifact {
    fn drop(&mut self) {
        // A sequence may still hold an open frame handle. Removing the directory
        // is safe on the supported Unix target; the open handle keeps working.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
