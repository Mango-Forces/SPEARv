//! One owner for an ordered set of sector frames on one common grid.
//!
//! Each sector is reduced from its exact member snippets with the approved rules
//! (`reduction`). Calibrated float frames are kept; one display mapping is
//! computed over every frame only after all are built, then frozen. Tiles are
//! refused until that mapping exists, so no frame is shown on its own stretch.
use crate::artifact::{self, Artifact};
use crate::counts::CountWriter;
use crate::preview::{encode_tile, percentile};
use crate::provenance;
use crate::reduction::{
    grid_pixel_rect, BandAccumulator, DataTile, DataTiles, Measurement, SectorBuilder, SectorImage,
};
use crate::types::{
    mjd_utc, BandContribution, MemberRef, ObservationRegion, SectorFrameInfo, SequenceInfo,
    SequenceSector, TileCoordinate, TILE_SIZE,
};
use crate::SnippetPlan;
use spearv_core::{sample_at, Cutout, Input, TanGrid};
use spearv_spherex::SpherexSource;
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const REDUCTION: &str = "IMAGE − ZODI; strict Spectrum mask; inverse-variance mean within each band; equal-weight mean across the sector's bands; pixel is no-data unless every band is present; nearest-pixel sampling; one shared display mapping across all frames.";

/// Locking rule: the `Mutex<Sequence>` guards only this small state. Nobody holds
/// it across disk I/O, a full-frame scan, the display-mapping computation or PNG
/// encoding. Readers clone an `Arc<Frame>` under the lock, release it, and do the
/// work on a blocking thread; the builder re-locks only to publish a result.
pub struct Sequence {
    pub info: SequenceInfo,
    /// Calibrated sector frames in grid PNG order, NaN = no-data. Index = frame.
    /// Only tiles touched by member support exist; a missing tile is no-data.
    pub frames: Vec<Option<Arc<Frame>>>,
    pub task: Option<tokio::task::AbortHandle>,
    /// Set when the sequence is released or expires. Work finishing after that
    /// is discarded instead of published.
    pub released: bool,
    /// Per-sequence scratch space. Frames are written here and removed on drop.
    directory: PathBuf,
}

/// One sector's calibrated values and variances. Unit tests keep a sparse tile
/// map in memory; production frames are written to a per-frame temp file, so a
/// *finished* frame keeps only its tile offsets in RAM. Building a frame is not
/// RAM-free: see `build_sector` for the per-pixel cost while a sector is built.
pub enum Frame {
    #[cfg(test)]
    Memory(DataTiles),
    Disk(FrameFile),
}

/// A frame on disk: each 256×256 tile is a value plane followed by a variance
/// plane, both little-endian `f32` in row-major PNG order. Only the offsets and
/// the ordered tile list stay in memory.
pub struct FrameFile {
    file: std::fs::File,
    offsets: BTreeMap<(u32, u32), u64>,
    data_tiles: Vec<TileCoordinate>,
}

const TILE_PIXELS: usize = (TILE_SIZE * TILE_SIZE) as usize;
const TILE_VALUES_BYTES: usize = TILE_PIXELS * 4;
const TILE_BYTES: usize = TILE_VALUES_BYTES * 2;

impl FrameFile {
    pub(crate) fn create(path: &Path, tiles: DataTiles) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|error| format!("Could not create {}: {error}", path.display()))?;
        let mut writer = std::io::BufWriter::new(&file);
        let mut offsets = BTreeMap::new();
        let mut data_tiles = Vec::with_capacity(tiles.len());
        let mut position = 0_u64;
        for ((x, y), tile) in tiles {
            if tile.value.len() != TILE_PIXELS || tile.variance.len() != TILE_PIXELS {
                return Err(format!(
                    "Data tile ({x}, {y}) is not {TILE_SIZE}×{TILE_SIZE}"
                ));
            }
            offsets.insert((x, y), position);
            data_tiles.push(TileCoordinate { x, y });
            for value in &tile.value {
                writer
                    .write_all(&value.to_le_bytes())
                    .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
            }
            for variance in &tile.variance {
                writer
                    .write_all(&variance.to_le_bytes())
                    .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
            }
            position += TILE_BYTES as u64;
        }
        writer
            .flush()
            .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
        drop(writer);
        data_tiles.sort_by_key(|tile| (tile.y, tile.x));
        Ok(Self {
            file,
            offsets,
            data_tiles,
        })
    }

    /// Positional read: frames are shared through `Arc` and read concurrently
    /// without a lock, so a shared file cursor (seek + read) would race.
    #[cfg(unix)]
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        self.file
            .read_exact_at(buffer, offset)
            .map_err(|error| error.to_string())
    }

    #[cfg(windows)]
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), String> {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buffer.len() {
            match self
                .file
                .seek_read(&mut buffer[done..], offset + done as u64)
            {
                Ok(0) => return Err("Unexpected end of frame file".into()),
                Ok(read) => done += read,
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }

    fn read_tile(&self, x: u32, y: u32) -> Result<Option<DataTile>, String> {
        let Some(&offset) = self.offsets.get(&(x, y)) else {
            return Ok(None);
        };
        let mut bytes = vec![0_u8; TILE_BYTES];
        self.read_at(offset, &mut bytes)?;
        Ok(Some(DataTile {
            value: decode_f32(&bytes[..TILE_VALUES_BYTES]),
            variance: decode_f32(&bytes[TILE_VALUES_BYTES..]),
        }))
    }

    /// Stream only the value plane of each tile, one tile at a time.
    fn for_each_value(&self, mut visit: impl FnMut(f32)) -> Result<(), String> {
        let mut bytes = vec![0_u8; TILE_VALUES_BYTES];
        for &offset in self.offsets.values() {
            self.read_at(offset, &mut bytes)?;
            for chunk in bytes.as_chunks::<4>().0 {
                visit(f32::from_le_bytes(*chunk));
            }
        }
        Ok(())
    }
}

fn decode_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

impl Frame {
    pub(crate) fn data_tiles(&self) -> Vec<TileCoordinate> {
        match self {
            #[cfg(test)]
            Self::Memory(tiles) => {
                let mut data_tiles: Vec<_> = tiles
                    .keys()
                    .map(|(x, y)| TileCoordinate { x: *x, y: *y })
                    .collect();
                data_tiles.sort_by_key(|tile| (tile.y, tile.x));
                data_tiles
            }
            Self::Disk(frame) => frame.data_tiles.clone(),
        }
    }

    fn read_tile(&self, x: u32, y: u32) -> Result<Option<DataTile>, String> {
        match self {
            #[cfg(test)]
            Self::Memory(tiles) => Ok(tiles.get(&(x, y)).cloned()),
            Self::Disk(frame) => frame.read_tile(x, y),
        }
    }

    fn for_each_value(&self, visit: impl FnMut(f32)) -> Result<(), String> {
        match self {
            #[cfg(test)]
            Self::Memory(tiles) => {
                for_each_memory_value(tiles, visit);
                Ok(())
            }
            Self::Disk(frame) => frame.for_each_value(visit),
        }
    }
}

#[cfg(test)]
fn for_each_memory_value(tiles: &DataTiles, mut visit: impl FnMut(f32)) {
    for tile in tiles.values() {
        for &value in &tile.value {
            visit(value);
        }
    }
}

/// Root for every sequence's scratch directory. A full-density 30-degree frame
/// is several GiB, so production points this at a real writable volume, not the
/// container's small /tmp tmpfs; the default keeps local runs on the temp dir.
pub(crate) fn sequence_root() -> PathBuf {
    std::env::var_os("SPEARV_SEQUENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("spearv-sequences"))
}

/// Remove scratch directories written by this process, including leftovers from
/// an earlier process that had the same PID. Only names this module creates and
/// whose PID field matches the current process are touched, so a shared root can
/// hold several engines without one deleting another's staging, sidecars, or
/// retained data. Directories from dead processes are intentionally left in
/// place: without a PID-liveness system, a stale directory is indistinguishable
/// from one owned by a live engine.
pub(crate) fn sweep_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let pid = std::process::id().to_string();
    let numeric = |part: Option<&str>| {
        part.is_some_and(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let mut parts = name.splitn(3, '-');
        let ours = parts.next() == Some(pid.as_str())
            && numeric(parts.next())
            && parts
                .next()
                .is_some_and(|id| id.starts_with("sequence-") || id.starts_with("artifact-"));
        if ours && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

pub(crate) fn sweep_sequences() {
    sweep_stale(&sequence_root());
}

impl Sequence {
    /// Creates the sequence's private scratch directory. The caller marks the
    /// request failed when this cannot be prepared.
    pub fn new(info: SequenceInfo, frames: Vec<Option<Frame>>) -> Result<Self, String> {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let directory = sequence_root().join(format!(
            "{}-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            info.sequence_id
        ));
        std::fs::create_dir_all(&directory).map_err(|error| {
            format!(
                "Could not create sequence directory {}: {error}",
                directory.display()
            )
        })?;
        Ok(Self {
            info,
            frames: frames
                .into_iter()
                .map(|frame| frame.map(Arc::new))
                .collect(),
            task: None,
            released: false,
            directory,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl Drop for Sequence {
    fn drop(&mut self) {
        // Close frame files before removing the directory, so cleanup also
        // works when open files block deletion.
        self.frames.clear();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub struct Admitted {
    pub region: ObservationRegion,
    pub sector: SequenceSector,
    pub members: Vec<Arc<SnippetPlan>>,
}

/// Why a composition was refused. An unknown (expired or never issued) snippet
/// reference is a 404; every other refusal is a 400.
#[derive(Debug)]
pub enum Refusal {
    Invalid(String),
    UnknownSnippet(String),
}

impl Refusal {
    #[cfg(test)]
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(message) | Self::UnknownSnippet(message) => message,
        }
    }
}

/// Nothing is deduplicated, moved or dropped.
pub fn admit(
    region: &ObservationRegion,
    sectors: Vec<SequenceSector>,
    lookup: impl Fn(&str) -> Option<Arc<SnippetPlan>>,
) -> Result<Vec<Admitted>, Refusal> {
    region.validate().map_err(Refusal::Invalid)?;
    if sectors.is_empty() {
        return Err(Refusal::Invalid("Add at least one sector".into()));
    }
    let mut ids = BTreeSet::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut admitted = Vec::new();
    for sector in sectors {
        if sector.id.trim().is_empty() || !ids.insert(sector.id.clone()) {
            return Err(Refusal::Invalid(format!(
                "Sector id '{}' is empty or repeated",
                sector.id
            )));
        }
        if sector.snippets.is_empty() {
            return Err(Refusal::Invalid(format!(
                "Sector '{}' is empty; remove it or join snippets before building",
                sector.id
            )));
        }
        let mut members = Vec::new();
        for reference in &sector.snippets {
            if let Some(owner) = owners.insert(reference.clone(), sector.id.clone()) {
                return Err(Refusal::Invalid(format!(
                    "Snippet {reference} appears in sector '{owner}' and sector '{}'; each snippet belongs to exactly one sector",
                    sector.id
                )));
            }
            let plan = lookup(reference).ok_or_else(|| {
                Refusal::UnknownSnippet(format!(
                    "Unknown snippet {reference} in sector '{}'; refresh discovery",
                    sector.id
                ))
            })?;
            if !region.same_grid(&plan.region) {
                return Err(Refusal::Invalid(format!(
                    "Snippet {reference} belongs to a different region"
                )));
            }
            members.push(plan);
        }
        admitted.push(Admitted {
            region: region.clone(),
            sector,
            members,
        });
    }
    Ok(admitted)
}

pub fn frame_info(admitted: &Admitted) -> Result<SectorFrameInfo, String> {
    let mjds = admitted.members.iter().map(|plan| plan.row.mjd);
    let start = mjds.clone().fold(f64::INFINITY, f64::min);
    let end = mjds.fold(f64::NEG_INFINITY, f64::max);
    let bands: BTreeSet<u16> = admitted.members.iter().map(|plan| plan.row.band).collect();
    let releases: BTreeSet<String> = admitted
        .members
        .iter()
        .map(|plan| plan.row.release.clone())
        .collect();
    let members = admitted
        .members
        .iter()
        .map(|plan| MemberRef {
            snippet: plan.row.clone(),
            s3_key: plan.file.key.clone(),
            etag: plan.file.etag.clone(),
            region: plan.region.clone(),
        })
        .collect();
    let fingerprint = provenance::fingerprint(&admitted.region, &admitted.members);
    Ok(SectorFrameInfo {
        id: admitted.sector.id.clone(),
        revision: admitted.sector.revision,
        state: "queued".into(),
        error: None,
        snippets: admitted.members.len(),
        members,
        fingerprint,
        bands: bands.into_iter().collect(),
        releases: releases.into_iter().collect(),
        mjd_start: start,
        mjd_end: end,
        utc_start: mjd_utc(start)?,
        utc_end: mjd_utc(end)?,
        complete_pixels: 0,
        target_coverage: 0.0,
        target_covered: false,
        contributions: None,
        count_bytes: None,
        bytes_read: 0,
        requests: 0,
        data_tiles: Vec::new(),
    })
}

/// Map one member cutout onto the grid in PNG order and accumulate its band.
/// Masking is left to the reducer so the approved strict policy is the only one.
pub fn sample_member(
    cutout: &Cutout,
    grid: &TanGrid,
    accumulator: &mut BandAccumulator,
    bounds: Option<[u32; 4]>,
) -> Result<(), String> {
    let zodi = cutout
        .zodi
        .as_ref()
        .ok_or("Sector build requires the ZODI plane")?;
    let input = Input {
        wcs: &cutout.wcs,
        image: &cutout.image.data,
        variance: &cutout.variance.data,
        flags: &cutout.flags.data,
        zodi: &zodi.data,
        wavelength: &cutout.wavelength,
        origin_fits: cutout.origin_fits,
        width: cutout.image.width as usize,
        flag_bits: cutout.flag_bits,
        mask: false,
        ignore_outlier: false,
    };
    let band = accumulator.band();
    // Bounds come from the exhaustive discovery scan, not a corner approximation.
    // Skipped pixels provably have no geometric support for this exact snippet.
    let [left, right, bottom, top] = bounds.unwrap_or([0, grid.width - 1, 0, grid.height - 1]);
    for png_y in grid.height - 1 - top..=grid.height - 1 - bottom {
        for x in left..=right {
            let (ra, dec) = grid.sky(f64::from(x) + 1.0, f64::from(grid.height - png_y));
            let Some(hit) = sample_at(&input, ra, dec, false) else {
                continue;
            };
            if hit.band != Some(band) {
                continue;
            }
            // flux and variance were f32 planes; these casts are exact.
            accumulator.add(
                grid_pixel_rect(x, png_y, grid.width, grid.height)?,
                &Measurement {
                    band,
                    value: hit.flux as f32,
                    variance: hit.variance as f32,
                    flags: hit.flags,
                    flag_bits: cutout.flag_bits,
                },
            );
        }
    }
    Ok(())
}

/// Reads members one band at a time. Only 256×256 tiles touched by member
/// support are allocated, but within that support the RAM cost per touched grid
/// pixel while one sector is built is:
/// - `SectorBuilder`: 18 B (two f64 sums + a u16 band count), for the union of
///   tiles touched by any band of the sector, held until the sector finishes;
/// - the band in flight (`BandAccumulator`): 24 B (two f64 sums + a u64 count),
///   for the tiles that band touches;
/// - `SectorBuilder::finish`: 8 B (f32 value + f32 variance) for every tile with
///   a complete pixel, held until `FrameFile::create` has written it to disk.
///
/// So a sector covering the whole grid peaks near 18 + 24 = 42 B per pixel during
/// accumulation and 18 + 8 = 26 B per pixel at finish; for a fully covered
/// 30-degree grid (≈3.1e8 pixels) that is ≈13 GB. Builds are serialized (one at a
/// time) for this reason. After a frame is stored, only its tile offsets stay in
/// RAM.
struct SectorBuild {
    image: SectorImage,
    contributions: Vec<BandContribution>,
    count_bytes: u64,
    counts_path: PathBuf,
    staging: artifact::Staging,
    bytes: u64,
    requests: u64,
}

enum BuildOutcome {
    Hit(Arc<Artifact>),
    Built(SectorBuild),
}

type BuildResult = Result<BuildOutcome, String>;

/// Fetch one member cutout. Tests substitute synthetic cutouts and a recorded
/// call order; production reads the exact S3 window.
async fn build_sector<F, Fut>(
    fetch: F,
    grid: &TanGrid,
    members: &[Arc<SnippetPlan>],
    staging: artifact::Staging,
) -> Result<SectorBuild, String>
where
    F: Fn(&Arc<SnippetPlan>) -> Fut,
    Fut: std::future::Future<Output = Result<Cutout, String>>,
{
    let counts_path = staging.path().join("counts.bin");
    match build_sector_inner(fetch, grid, members, &counts_path).await {
        Ok((image, contributions, count_bytes, bytes, requests)) => Ok(SectorBuild {
            image,
            contributions,
            count_bytes,
            counts_path,
            staging,
            bytes,
            requests,
        }),
        Err(error) => {
            // Early sampling/build failure: the partial sidecar is not a count result.
            let _ = std::fs::remove_file(&counts_path);
            Err(error)
        }
    }
}

async fn build_sector_inner<F, Fut>(
    fetch: F,
    grid: &TanGrid,
    members: &[Arc<SnippetPlan>],
    counts_path: &Path,
) -> Result<(SectorImage, Vec<BandContribution>, u64, u64, u64), String>
where
    F: Fn(&Arc<SnippetPlan>) -> Fut,
    Fut: std::future::Future<Output = Result<Cutout, String>>,
{
    let mut by_band: BTreeMap<u16, Vec<&Arc<SnippetPlan>>> = BTreeMap::new();
    members
        .iter()
        .for_each(|plan| by_band.entry(plan.support.band).or_default().push(plan));
    // The same canonical order the fingerprint hashes; otherwise reversing the
    // submitted members could change floating-point addition order for one key.
    for plans in by_band.values_mut() {
        provenance::canonicalize(plans);
    }
    let mut builder =
        SectorBuilder::new_rect(by_band.keys().copied().collect(), grid.width, grid.height)?;
    let mut counts =
        CountWriter::create_rect(counts_path, builder.bands(), grid.width, grid.height)?;
    let (mut bytes, mut requests) = (0, 0);
    for (band, plans) in by_band {
        let mut accumulator = BandAccumulator::new_rect(band, grid.width, grid.height);
        for plan in plans {
            let cutout = fetch(plan).await?;
            bytes += cutout.bytes_read;
            requests += cutout.s3_requests;
            let grid = grid.clone();
            let bounds = plan.support.grid_bounds;
            accumulator = tokio::task::spawn_blocking(move || {
                sample_member(&cutout, &grid, &mut accumulator, Some(bounds)).map(|_| accumulator)
            })
            .await
            .map_err(|error| error.to_string())??;
        }
        counts.write_band(&accumulator)?;
        builder.add_band(&accumulator)?;
    }
    let counts = counts.finish()?;
    let image = builder.finish()?;
    Ok((image, counts.contributions, counts.bytes, bytes, requests))
}

/// Below this many finite pixels the mapping stays exact; above it a fixed-size
/// histogram is used instead of duplicating every frame value.
const EXACT_MAPPING_LIMIT: u64 = 1 << 16;
const HISTOGRAM_BINS: usize = 1 << 18;

fn for_each_finite<F: Borrow<Frame>>(
    frames: &[Option<F>],
    mut visit: impl FnMut(f32),
) -> Result<(), String> {
    for frame in frames.iter().flatten() {
        frame.borrow().for_each_value(|value| {
            if value.is_finite() {
                visit(value);
            }
        })?;
    }
    Ok(())
}

/// One mapping over every finite value in every frame (2nd–99.5th percentile).
/// Small samples stay exact. Large samples stream once into a bounded histogram,
/// so a 30-degree frame never duplicates every value in memory. Disk frames are
/// read one tile at a time, never loaded whole. Runs without the sequence lock.
pub fn shared_mapping<F: Borrow<Frame>>(
    frames: &[Option<F>],
) -> Result<Option<(f32, f32)>, String> {
    let mut count = 0_u64;
    let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
    for_each_finite(frames, |value| {
        count += 1;
        min = min.min(value);
        max = max.max(value);
    })?;
    if count == 0 {
        return Ok(None);
    }
    if count <= EXACT_MAPPING_LIMIT {
        let mut values = Vec::with_capacity(count as usize);
        for_each_finite(frames, |value| values.push(value))?;
        values.sort_by(f32::total_cmp);
        return Ok(Some((
            percentile(&values, 0.02),
            percentile(&values, 0.995),
        )));
    }
    let rank = |fraction: f64| ((count - 1) as f64 * fraction).round() as u64;
    let (low_rank, high_rank) = (rank(0.02), rank(0.995));
    let span = (max - min) as f64;
    let mut bins = vec![0_u64; HISTOGRAM_BINS];
    for_each_finite(frames, |value| {
        let index = if span > 0.0 {
            ((((value - min) as f64) / span) * (HISTOGRAM_BINS - 1) as f64).round() as usize
        } else {
            0
        };
        bins[index.min(HISTOGRAM_BINS - 1)] += 1;
    })?;
    let quantile = |target: u64| -> f32 {
        let mut seen = 0_u64;
        for (index, &bin) in bins.iter().enumerate() {
            seen += bin;
            if seen > target {
                if span <= 0.0 {
                    return min;
                }
                let fraction = index as f64 / (HISTOGRAM_BINS - 1) as f64;
                return (min as f64 + span * fraction) as f32;
            }
        }
        max
    };
    Ok(Some((quantile(low_rank), quantile(high_rank))))
}

pub async fn run(
    sequence: Arc<Mutex<Sequence>>,
    grid: TanGrid,
    admitted: Vec<Admitted>,
    builds: Arc<tokio::sync::Semaphore>,
    jobs: Arc<crate::Jobs>,
) {
    let update = |change: &dyn Fn(&mut Sequence)| {
        publish(&sequence, change);
    };
    let _permit = match builds.acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => {
            return update(&|sequence| {
                sequence.info.state = "failed".into();
                sequence.info.error = Some(format!("Sequence build queue unavailable: {error}"));
            })
        }
    };
    let Some(directory) = snapshot(&sequence, |sequence| sequence.directory().to_path_buf()) else {
        return;
    };
    let root = sequence_root();
    // The source is constructed only on an actual miss; a Preview followed by
    // Prepare for the same recipe must not open S3 again.
    let mut source: Option<SpherexSource> = None;
    for (index, sector) in admitted.iter().enumerate() {
        update(&|sequence| sequence.info.sectors[index].state = "building".into());
        let Some(fingerprint) = snapshot(&sequence, |sequence| {
            sequence.info.sectors[index].fingerprint.clone()
        }) else {
            return;
        };
        if let Some(artifact) = jobs.cached_artifact(&fingerprint) {
            let result = Ok(BuildOutcome::Hit(artifact));
            if !store_frame(&sequence, &directory, index, result, &jobs).await {
                return;
            }
            continue;
        }
        let staging = match artifact::Staging::create(&root, &fingerprint) {
            Ok(staging) => staging,
            Err(error) => {
                let result = Err(format!(
                    "Sector '{}' artifact staging failed: {error}",
                    sector.sector.id
                ));
                if !store_frame(&sequence, &directory, index, result, &jobs).await {
                    return;
                }
                continue;
            }
        };
        let source = match ensure_source(&mut source) {
            Ok(source) => source,
            Err(error) => {
                let result = Err(format!(
                    "Sector '{}' source unavailable: {error}",
                    sector.sector.id
                ));
                if !store_frame(&sequence, &directory, index, result, &jobs).await {
                    return;
                }
                continue;
            }
        };
        let fetch = |plan: &Arc<SnippetPlan>| {
            let plan = plan.clone();
            async move {
                source
                    .exact_window_cutout(&plan.file, &plan.support.window)
                    .await
                    .map_err(|error| format!("{}: {error}", plan.file.key))
            }
        };
        let result = match build_sector(&fetch, &grid, &sector.members, staging).await {
            Ok(build) => Ok(BuildOutcome::Built(build)),
            Err(error) => Err(error),
        };
        if !store_frame(&sequence, &directory, index, result, &jobs).await {
            return;
        }
    }
    finalize(&sequence, shared_mapping).await;
}

fn ensure_source(source: &mut Option<SpherexSource>) -> Result<&SpherexSource, String> {
    if source.is_none() {
        *source = Some(SpherexSource::direct().map_err(|error| error.to_string())?);
    }
    source
        .as_ref()
        .ok_or_else(|| "Observation source was not initialized".to_string())
}

/// Read under the lock, then release it. `None` once the sequence is released.
fn snapshot<T>(owner: &Mutex<Sequence>, read: impl FnOnce(&Sequence) -> T) -> Option<T> {
    let sequence = owner.lock().ok()?;
    (!sequence.released).then(|| read(&sequence))
}

/// Lock briefly to apply a finished result. Returns `false`, changing nothing,
/// once the sequence was released (deleted, expired or superseded).
fn publish(owner: &Mutex<Sequence>, change: impl FnOnce(&mut Sequence)) -> bool {
    let Ok(mut sequence) = owner.lock() else {
        return false;
    };
    if sequence.released {
        return false;
    }
    change(&mut sequence);
    true
}

/// Write one built or reused sector on a blocking thread without the lock, then
/// publish its frame and info. Returns `false` when the sequence was released.
async fn store_frame(
    owner: &Mutex<Sequence>,
    directory: &Path,
    index: usize,
    result: BuildResult,
    jobs: &crate::Jobs,
) -> bool {
    let Some((info, width, height)) = snapshot(owner, |sequence| {
        (
            sequence.info.sectors[index].clone(),
            sequence.info.width,
            sequence.info.height,
        )
    }) else {
        return false;
    };
    let directory = directory.to_path_buf();
    let stored = tokio::task::spawn_blocking(move || {
        store_result(info, &directory, index, result, width, height)
    })
    .await;
    if let Ok(stored) = &stored {
        if let Some(artifact) = &stored.artifact {
            jobs.cache_artifact(artifact.clone());
        }
    }
    publish(owner, |sequence| match stored {
        Ok(stored) => {
            sequence.info.sectors[index] = stored.info;
            sequence.frames[index] = stored.frame;
        }
        Err(error) => {
            let sector = &mut sequence.info.sectors[index];
            sector.state = "failed".into();
            sector.contributions = None;
            sector.count_bytes = None;
            sector.error = Some(format!(
                "Sector '{}' frame could not be stored: {error}",
                sector.id
            ));
        }
    })
}

struct Stored {
    info: SectorFrameInfo,
    frame: Option<Arc<Frame>>,
    artifact: Option<Arc<Artifact>>,
}

/// Move a completed count sidecar next to the sequence when the numerical frame
/// is not reusable. Returns whether the sidecar survived.
fn retain_counts(directory: &Path, index: usize, counts_path: &Path) -> bool {
    let target = directory.join(format!("counts-{index}.bin"));
    std::fs::rename(counts_path, target).is_ok()
}

/// Record one finished sector. A build that produced no complete pixel is a
/// failed sector, never a playable blank frame, but its fully accumulated count
/// sidecar is retained until the sequence is released.
fn store_result(
    mut info: SectorFrameInfo,
    directory: &Path,
    index: usize,
    result: BuildResult,
    width: u32,
    height: u32,
) -> Stored {
    let mut frame = None;
    let mut artifact = None;
    match result {
        Ok(BuildOutcome::Hit(cached)) => {
            info.state = "ready".into();
            info.error = None;
            info.complete_pixels = cached.complete_pixels;
            info.target_coverage = cached.target_coverage;
            info.target_covered = cached.target_coverage >= 0.2;
            info.bytes_read = 0;
            info.requests = 0;
            info.data_tiles = cached.data_tiles.clone();
            info.contributions = Some(cached.contributions.clone());
            info.count_bytes = Some(cached.count_bytes);
            frame = Some(cached.frame.clone());
        }
        Ok(BuildOutcome::Built(build)) => {
            info.bytes_read = build.bytes;
            info.requests = build.requests;
            if build.image.complete_pixels == 0 {
                let retained = retain_counts(directory, index, &build.counts_path);
                drop(build.staging);
                info.state = "failed".into();
                info.complete_pixels = 0;
                info.target_coverage = 0.0;
                info.target_covered = false;
                info.data_tiles = Vec::new();
                info.contributions = retained.then(|| build.contributions.clone());
                info.count_bytes = retained.then_some(build.count_bytes);
                info.error = Some(format!(
                    "Sector '{}' has no pixel with every selected band; remove a band or a member before playback",
                    info.id
                ));
            } else {
                let complete_pixels = build.image.complete_pixels;
                match artifact::create(
                    build.staging,
                    info.fingerprint.clone(),
                    build.image,
                    build.contributions.clone(),
                    build.count_bytes,
                    width,
                    height,
                ) {
                    Ok(stored) => {
                        info.state = "ready".into();
                        info.complete_pixels = complete_pixels;
                        info.target_coverage = stored.target_coverage;
                        info.target_covered = stored.target_coverage >= 0.2;
                        info.data_tiles = stored.data_tiles.clone();
                        info.contributions = Some(stored.contributions.clone());
                        info.count_bytes = Some(stored.count_bytes);
                        info.error = None;
                        frame = Some(stored.frame.clone());
                        artifact = Some(stored);
                    }
                    Err(error) => {
                        info.state = "failed".into();
                        info.complete_pixels = 0;
                        info.target_coverage = 0.0;
                        info.target_covered = false;
                        info.data_tiles = Vec::new();
                        info.contributions = None;
                        info.count_bytes = None;
                        info.error = Some(format!(
                            "Sector '{}' frame could not be stored: {error}",
                            info.id
                        ));
                    }
                }
            }
        }
        Err(error) => {
            info.state = "failed".into();
            info.complete_pixels = 0;
            info.target_coverage = 0.0;
            info.target_covered = false;
            info.data_tiles = Vec::new();
            info.contributions = None;
            info.count_bytes = None;
            info.error = Some(error);
        }
    }
    Stored {
        info,
        frame,
        artifact,
    }
}

/// Validate the finished frames under the lock, compute the shared mapping on a
/// blocking thread without it, then publish. Status and release stay
/// responsive while every frame is streamed from disk. `compute` is
/// `shared_mapping` in production; tests substitute a gated computation.
async fn finalize<C>(owner: &Mutex<Sequence>, compute: C)
where
    C: FnOnce(&[Option<Arc<Frame>>]) -> Result<Option<(f32, f32)>, String> + Send + 'static,
{
    let frames = {
        let Ok(mut sequence) = owner.lock() else {
            return;
        };
        if sequence.released {
            return;
        }
        match begin_finish(&mut sequence) {
            Some(frames) => frames,
            None => return,
        }
    };
    let result = tokio::task::spawn_blocking(move || compute(&frames))
        .await
        .unwrap_or_else(|error| Err(format!("Display mapping failed: {error}")));
    publish(owner, |sequence| publish_mapping(sequence, result));
}

/// Validate the built frames before any mapping work. A failed frame or a
/// zero-complete frame is never skipped to make a shorter sequence playable: the
/// sequence fails and returns `None`. Otherwise returns the frames to map.
fn begin_finish(sequence: &mut Sequence) -> Option<Vec<Option<Arc<Frame>>>> {
    for sector in &mut sequence.info.sectors {
        if sector.state == "ready" && sector.complete_pixels == 0 {
            sector.state = "failed".into();
            sector.error = Some(format!(
                "Sector '{}' has no pixel with every selected band; remove a band or a member before playback",
                sector.id
            ));
        }
    }
    let failed: Vec<_> = sequence
        .info
        .sectors
        .iter()
        .filter(|sector| sector.state != "ready")
        .map(|sector| match &sector.error {
            Some(error) => format!("{}: {error}", sector.id),
            None => sector.id.clone(),
        })
        .collect();
    if !failed.is_empty() {
        sequence.info.state = "failed".into();
        sequence.info.error = Some(format!(
            "Sector(s) {} failed to build; fix or remove them before playback",
            failed.join("; ")
        ));
        return None;
    }
    Some(sequence.frames.clone())
}

/// Freeze the shared mapping computed by `shared_mapping`.
fn publish_mapping(sequence: &mut Sequence, result: Result<Option<(f32, f32)>, String>) {
    match result {
        Ok(Some((low, high))) => {
            sequence.info.stretch_low = Some(low);
            sequence.info.stretch_high = Some(high);
            sequence.info.display_revision = Some(1);
            sequence.info.state = "ready".into();
        }
        Ok(None) => {
            sequence.info.state = "failed".into();
            sequence.info.error =
                Some("No pixel in any sector has every selected band; nothing to display".into());
        }
        Err(error) => {
            sequence.info.state = "failed".into();
            sequence.info.error = Some(error);
        }
    }
}

/// Synchronous form of `finalize` for unit tests.
#[cfg(test)]
pub fn finish(sequence: &mut Sequence) {
    if let Some(frames) = begin_finish(sequence) {
        let result = shared_mapping(&frames);
        publish_mapping(sequence, result);
    }
}

/// Everything one display tile needs, captured under the lock. `encode` reads
/// the frame and encodes the PNG later, without the lock.
pub struct TileSource {
    frame: Arc<Frame>,
    width: u32,
    height: u32,
    low: f32,
    high: f32,
}

pub fn tile_source(
    sequence: &Sequence,
    sector_id: &str,
    display_revision: u64,
) -> Result<TileSource, (bool, String)> {
    let (Some(revision), Some(low), Some(high)) = (
        sequence.info.display_revision,
        sequence.info.stretch_low,
        sequence.info.stretch_high,
    ) else {
        return Err((true, "The shared display mapping is not ready".into()));
    };
    if revision != display_revision {
        return Err((
            true,
            format!("Display revision {display_revision} is stale; current is {revision}"),
        ));
    }
    Ok(TileSource {
        frame: find_frame(sequence, sector_id)?,
        width: sequence.info.width,
        height: sequence.info.height,
        low,
        high,
    })
}

impl TileSource {
    pub fn encode(&self, x: u32, y: u32) -> Result<Vec<u8>, (bool, String)> {
        let tile = self.frame.read_tile(x, y).map_err(|error| (false, error))?;
        encode_tile(
            self.width,
            self.height,
            x,
            y,
            self.low,
            self.high,
            |px, py| {
                tile.as_ref()
                    .map(|tile| tile.value[DataTile::index(px, py)])
                    .unwrap_or(f32::NAN)
            },
        )
        .map_err(|error| (false, error))
    }
}

#[cfg(test)]
pub fn tile(
    sequence: &Sequence,
    sector_id: &str,
    x: u32,
    y: u32,
    display_revision: u64,
) -> Result<Vec<u8>, (bool, String)> {
    tile_source(sequence, sector_id, display_revision)?.encode(x, y)
}

fn find_frame(sequence: &Sequence, sector_id: &str) -> Result<Arc<Frame>, (bool, String)> {
    let index = sequence
        .info
        .sectors
        .iter()
        .position(|sector| sector.id == sector_id)
        .ok_or((false, format!("No sector '{sector_id}' in this sequence")))?;
    sequence.frames[index]
        .clone()
        .ok_or((true, "Frame is not built".to_string()))
}

/// The frame behind a variance tile, captured under the lock.
pub fn variance_source(sequence: &Sequence, sector_id: &str) -> Result<Arc<Frame>, (bool, String)> {
    if sequence.info.state != "ready" || sequence.info.display_revision.is_none() {
        return Err((true, "The shared display mapping is not ready".into()));
    }
    find_frame(sequence, sector_id)
}

/// Raw little-endian `f32` variance tile (256×256). No PNG encoding and no
/// stretching: this is the approved per-pixel `SectorVariance(p)`. Reads the
/// frame without the sequence lock.
pub fn encode_variance(
    frame: &Frame,
    sector_id: &str,
    x: u32,
    y: u32,
) -> Result<Vec<u8>, (bool, String)> {
    let tile = frame
        .read_tile(x, y)
        .map_err(|error| (false, error))?
        .ok_or((
            false,
            format!("Sector '{sector_id}' has no data tile ({x}, {y})"),
        ))?;
    let mut bytes = Vec::with_capacity(tile.variance.len() * 4);
    for variance in &tile.variance {
        bytes.extend_from_slice(&variance.to_le_bytes());
    }
    Ok(bytes)
}

/// Raw little-endian `f32` calibrated value tile (256×256), NaN for no data.
/// A tile with no data at all is all NaN rather than an error, so a frame
/// assembles from every tile of the grid.
pub fn encode_values(
    frame: &Frame,
    _sector_id: &str,
    x: u32,
    y: u32,
) -> Result<Vec<u8>, (bool, String)> {
    let tile = frame.read_tile(x, y).map_err(|error| (false, error))?;
    let Some(tile) = tile else {
        return Ok(std::iter::repeat(f32::NAN.to_le_bytes()).take(256 * 256).flatten().collect());
    };
    let mut bytes = Vec::with_capacity(tile.value.len() * 4);
    for value in &tile.value {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
pub fn variance_tile(
    sequence: &Sequence,
    sector_id: &str,
    x: u32,
    y: u32,
) -> Result<Vec<u8>, (bool, String)> {
    let frame = variance_source(sequence, sector_id)?;
    encode_variance(&frame, sector_id, x, y)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::browse::Support;
    use crate::preview::TILE_SIZE;
    use crate::types::SnippetRow;
    use spearv_core::{
        FlagBits, PixelWindow, Plane, Poly, Sip, WavelengthMap, Wcs, PIXEL_SCALE_DEG,
    };
    use spearv_index::{ExposureRow, Quality, Survey};

    fn wcs() -> Wcs {
        Wcs {
            crpix: [3.0, 3.0],
            crval: [10.0, 20.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: Sip {
                a: Poly::new(0),
                b: Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [5, 5],
        }
    }
    pub(crate) fn region() -> ObservationRegion {
        ObservationRegion {
            ra: 10.0,
            dec: 20.0,
            fov: 5.0 * PIXEL_SCALE_DEG,
            fov_l: None,
            fov_h: None,
        }
    }
    pub(crate) fn plan(
        id: &str,
        band: u16,
        mjd: f64,
        region: ObservationRegion,
    ) -> Arc<SnippetPlan> {
        let row = SnippetRow {
            id: id.into(),
            snippet_ref: id.into(),
            band: band + 1,
            mjd,
            utc: String::new(),
            release: "qr2".into(),
            detector: 1,
            obsid: id.into(),
            version: "v".into(),
            geometric_samples: 1,
            wl_min_um: 0.75,
            wl_max_um: 0.75,
            target_covered: true,
            quality: serde_json::Value::Null,
        };
        let file = ExposureRow {
            key: format!("key-{id}"),
            url: String::new(),
            etag: Some("\"e\"".into()),
            size: None,
            release: "qr2".into(),
            survey: Survey::AllSky,
            sps_flags: None,
            period: String::new(),
            version: "v".into(),
            obsid: id.into(),
            detector: 1,
            mjd_beg: None,
            mjd_avg: mjd,
            em_min_um: 0.7,
            em_max_um: 1.1,
            wcs: wcs(),
            quality: Quality::default(),
            sc_xyz: None,
        };
        let support = Support {
            grid_bounds: [0, 4, 0, 4],
            band,
            count: 1,
            window: PixelWindow {
                x0: 0,
                y0: 0,
                width: 5,
                height: 5,
                clipped: false,
            },
            wl_min: 0.75,
            wl_max: 0.75,
        };
        Arc::new(SnippetPlan {
            region,
            file,
            support,
            row,
        })
    }
    fn sector(id: &str, snippets: &[&str]) -> SequenceSector {
        SequenceSector {
            id: id.into(),
            revision: 1,
            snippets: snippets.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn lookup(plans: Vec<Arc<SnippetPlan>>) -> impl Fn(&str) -> Option<Arc<SnippetPlan>> {
        move |reference| plans.iter().find(|plan| plan.row.id == reference).cloned()
    }

    #[test]
    fn admission_keeps_order_and_refuses_shared_empty_unknown_or_foreign_snippets() {
        let other = ObservationRegion {
            fov: 1.0,
            ..region()
        };
        let plans = vec![
            plan("a", 0, 2.0, region()),
            plan("b", 1, 1.0, region()),
            plan("c", 0, 3.0, region()),
            plan("z", 0, 1.0, other),
        ];
        let order = admit(
            &region(),
            vec![sector("s3", &["c"]), sector("s1", &["a", "b"])],
            lookup(plans.clone()),
        )
        .unwrap();
        assert_eq!(
            order
                .iter()
                .map(|item| item.sector.id.as_str())
                .collect::<Vec<_>>(),
            ["s3", "s1"]
        );
        let info = frame_info(&order[1]).unwrap();
        assert_eq!(info.bands, vec![1, 2]);
        assert_eq!(
            info.members
                .iter()
                .map(|member| member.snippet.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"],
            "public member order is submitted order"
        );
        assert_eq!(
            info.members[0].s3_key, order[1].members[0].file.key,
            "exact S3 key is retained"
        );
        assert_eq!(
            info.members[0].etag, order[1].members[0].file.etag,
            "exact ETag is retained"
        );
        assert_eq!(
            info.fingerprint,
            provenance::fingerprint(&region(), &order[1].members)
        );
        for (sectors, expected) in [
            (
                vec![sector("s1", &["a"]), sector("s2", &["a"])],
                "exactly one sector",
            ),
            (vec![sector("s1", &["a", "a"])], "exactly one sector"),
            (vec![sector("s1", &[])], "is empty"),
            (vec![sector("s1", &["missing"])], "Unknown snippet"),
            (vec![sector("s1", &["z"])], "different region"),
            (vec![sector("s1", &["a"]), sector("s1", &["b"])], "repeated"),
            (vec![], "at least one"),
        ] {
            let refusal = admit(&region(), sectors, lookup(plans.clone()))
                .err()
                .unwrap();
            assert!(refusal.message().contains(expected), "{refusal:?}");
            assert_eq!(
                matches!(refusal, Refusal::UnknownSnippet(_)),
                expected == "Unknown snippet",
                "only an unknown ref is a not-found refusal: {refusal:?}"
            );
        }
    }

    fn cutout(value: f32, variance: f32, wavelength: f32, flags: Vec<u32>) -> Cutout {
        let plane = |v| Plane {
            width: 5,
            height: 5,
            data: vec![v; 25],
        };
        Cutout {
            image: plane(value + 1.0),
            variance: plane(variance),
            zodi: Some(plane(1.0)),
            flags: Plane {
                width: 5,
                height: 5,
                data: flags,
            },
            flags_valid: None,
            wcs: wcs(),
            wavelength: WavelengthMap {
                x: vec![1.0, 5.0],
                y: vec![1.0, 5.0],
                wavelength: vec![wavelength; 4],
                bandwidth: vec![0.02; 4],
                nx: 2,
                ny: 2,
            },
            origin_fits: (1, 1),
            clipped: false,
            flag_bits: FlagBits {
                transient: 0,
                overflow: 1,
                sur_error: 2,
                nonfunc: 6,
                fullsample: 12,
                persist: 17,
                outlier: 19,
                source: 21,
            },
            bytes_read: 0,
            s3_requests: 0,
        }
    }

    fn at(image: &SectorImage, width: u32, pixel: usize) -> (f32, f32) {
        let (x, y) = crate::reduction::grid_coordinates(pixel, width).unwrap();
        image
            .tiles
            .get(&(x / TILE_SIZE, y / TILE_SIZE))
            .map(|tile| {
                let index = DataTile::index(x, y);
                (tile.value[index], tile.variance[index])
            })
            .unwrap_or((f32::NAN, f32::NAN))
    }

    #[test]
    fn grid_sampling_feeds_the_approved_reducer_in_png_order() {
        let grid = region().grid();
        let pixels = (grid.width * grid.width) as usize;
        // Band 0 at 0.753 µm twice (depth), band 2 at 0.79 µm once (bandwise).
        let mut flags = vec![0; 25];
        flags[0] = 1 << 6; // FITS (1,1) is the bottom-left: PNG bottom row.
        let mut band0 = BandAccumulator::new(0, grid.width);
        sample_member(&cutout(2.0, 1.0, 0.753, flags), &grid, &mut band0, None).unwrap();
        sample_member(
            &cutout(10.0, 4.0, 0.753, vec![0; 25]),
            &grid,
            &mut band0,
            None,
        )
        .unwrap();
        let mut band2 = BandAccumulator::new(2, grid.width);
        sample_member(
            &cutout(12.0, 9.0, 0.79, vec![0; 25]),
            &grid,
            &mut band2,
            None,
        )
        .unwrap();
        let mut wrong = BandAccumulator::new(2, grid.width);
        sample_member(
            &cutout(12.0, 9.0, 0.753, vec![0; 25]),
            &grid,
            &mut wrong,
            None,
        )
        .unwrap();
        assert!(
            (0..pixels).all(|pixel| wrong.depth(pixel).samples == 0),
            "other band never contributes"
        );
        let mut builder = SectorBuilder::new(vec![0, 2], grid.width).unwrap();
        builder.add_band(&band0).unwrap();
        builder.add_band(&band2).unwrap();
        let image = builder.finish().unwrap();
        assert_eq!(image.complete_pixels, pixels as u64);
        let bottom_left = ((grid.width - 1) * grid.width) as usize;
        // Masked first exposure leaves only 10.0 in band 0 there: (10 + 12) / 2.
        assert!((at(&image, grid.width, bottom_left).0 - 11.0).abs() < 1e-5);
        // Elsewhere: band 0 = (2/1 + 10/4)/(1 + 1/4) = 3.6, sector = (3.6 + 12)/2.
        assert!((at(&image, grid.width, 0).0 - 7.8).abs() < 1e-5);
        assert!((at(&image, grid.width, 0).1 - 2.45).abs() < 1e-5);
    }

    #[test]
    fn a_small_footprint_allocates_only_its_tile_on_a_large_grid() {
        let cutout = cutout(2.0, 1.0, 0.753, vec![0; 25]);
        let grid = TanGrid::new(10.0, 20.0, 300.0 * PIXEL_SCALE_DEG);
        let geometry = spearv_core::ExposureGeometry {
            wcs: cutout.wcs.clone(),
            wavelength: cutout.wavelength.clone(),
            header_bytes: 0,
        };
        let support = crate::browse::supports(&geometry, &grid, &[0]).remove(0);
        let mut band = BandAccumulator::new(0, grid.width);
        sample_member(&cutout, &grid, &mut band, Some(support.grid_bounds)).unwrap();
        assert_eq!(band.tiles().len(), 1, "the 5x5 support is one data tile");
        let mut builder = SectorBuilder::new(vec![0], grid.width).unwrap();
        builder.add_band(&band).unwrap();
        let image = builder.finish().unwrap();
        assert_eq!(
            image.tiles.len(),
            1,
            "a 300x300 grid has four tiles if dense"
        );
        assert_eq!(image.data_tiles(), vec![[0, 0]]);
        assert_eq!(
            image.tiles[&(0, 0)].value.len(),
            (TILE_SIZE * TILE_SIZE) as usize
        );
        assert!(!image.tiles.contains_key(&(1, 1)));
    }

    #[test]
    fn exact_discovery_bounds_match_full_grid_sampling_at_every_pixel() {
        let mut cutout = cutout(2.0, 1.0, 0.753, vec![0; 25]);
        cutout.wavelength.wavelength = vec![0.753, 0.753, 0.79, 0.79];
        let grid = TanGrid::new(10.0, 20.0, 9.0 * PIXEL_SCALE_DEG);
        let geometry = spearv_core::ExposureGeometry {
            wcs: cutout.wcs.clone(),
            wavelength: cutout.wavelength.clone(),
            header_bytes: 0,
        };
        let pixels = (grid.width * grid.width) as usize;
        let found = crate::browse::supports(&geometry, &grid, &[0, 2]);
        assert_eq!(found.len(), 2);
        for support in found {
            let mut full = BandAccumulator::new(support.band, grid.width);
            let mut bounded = BandAccumulator::new(support.band, grid.width);
            sample_member(&cutout, &grid, &mut full, None).unwrap();
            sample_member(&cutout, &grid, &mut bounded, Some(support.grid_bounds)).unwrap();
            let [left, right, bottom, top] = support.grid_bounds;
            assert!((right - left + 1) * (top - bottom + 1) < grid.width * grid.width);
            for pixel in 0..pixels {
                let expected = full.depth(pixel);
                let actual = bounded.depth(pixel);
                assert_eq!(actual.samples, expected.samples);
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.variance, expected.variance);
            }
        }
    }

    fn data_tile(values: &[f32]) -> DataTile {
        let mut tile = DataTile {
            value: vec![f32::NAN; TILE_PIXELS],
            variance: vec![f32::NAN; TILE_PIXELS],
        };
        for (index, value) in values.iter().enumerate() {
            tile.value[index] = *value;
            tile.variance[index] = 0.25;
        }
        tile
    }

    fn frame(values: &[f32]) -> Option<Frame> {
        Some(Frame::Memory(DataTiles::from([(
            (0, 0),
            data_tile(values),
        )])))
    }

    fn memory_tiles(frame: Option<Arc<Frame>>) -> DataTiles {
        match Arc::into_inner(frame.unwrap()) {
            Some(Frame::Memory(tiles)) => tiles,
            _ => unreachable!(),
        }
    }

    fn disk_frame(directory: &Path, name: &str, tiles: DataTiles) -> Frame {
        Frame::Disk(FrameFile::create(&directory.join(name), tiles).unwrap())
    }

    fn temp_root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "spearv-artifact-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn sector_build(
        root: &Path,
        fingerprint: &str,
        image: SectorImage,
        bands: &[u16],
    ) -> SectorBuild {
        let staging = artifact::Staging::create(root, fingerprint).unwrap();
        let counts_path = staging.path().join("counts.bin");
        let mut writer = CountWriter::create(&counts_path, bands, 2).unwrap();
        for band in bands {
            writer.write_band(&BandAccumulator::new(*band, 2)).unwrap();
        }
        let counts = writer.finish().unwrap();
        SectorBuild {
            image,
            contributions: counts.contributions,
            count_bytes: counts.bytes,
            counts_path,
            staging,
            bytes: 12,
            requests: 3,
        }
    }

    fn artifact_for(
        root: &Path,
        fingerprint: &str,
        tiles: DataTiles,
        complete_pixels: u64,
    ) -> Arc<Artifact> {
        let staging = artifact::Staging::create(root, fingerprint).unwrap();
        std::fs::write(staging.path().join("counts.bin"), b"counts").unwrap();
        artifact::create(
            staging,
            fingerprint.into(),
            SectorImage {
                tiles,
                complete_pixels,
            },
            vec![],
            6,
            2,
            1,
        )
        .unwrap()
    }

    fn sequence(frames: Vec<Option<Frame>>, states: &[&str]) -> Sequence {
        let sectors = states
            .iter()
            .enumerate()
            .map(|(index, state)| {
                let (complete_pixels, data_tiles) = match &frames[index] {
                    Some(Frame::Memory(tiles)) => {
                        let complete_pixels = tiles
                            .values()
                            .flat_map(|tile| tile.value.iter())
                            .filter(|value| value.is_finite())
                            .count() as u64;
                        let mut data_tiles: Vec<_> = tiles
                            .keys()
                            .map(|(x, y)| TileCoordinate { x: *x, y: *y })
                            .collect();
                        data_tiles.sort_by_key(|tile| (tile.y, tile.x));
                        (complete_pixels, data_tiles)
                    }
                    _ => (0, Vec::new()),
                };
                SectorFrameInfo {
                    id: format!("s{index}"),
                    revision: 1,
                    state: state.to_string(),
                    error: None,
                    snippets: 1,
                    members: Vec::new(),
                    fingerprint: format!("fingerprint-{index}"),
                    bands: vec![1],
                    releases: vec![],
                    mjd_start: 0.0,
                    mjd_end: 0.0,
                    utc_start: String::new(),
                    utc_end: String::new(),
                    complete_pixels,
                    target_coverage: 0.0,
                    target_covered: false,
                    contributions: None,
                    count_bytes: None,
                    bytes_read: 0,
                    requests: 0,
                    data_tiles,
                }
            })
            .collect();
        Sequence::new(
            SequenceInfo {
                sequence_id: "q".into(),
                width: 2,
                height: 1,
                tile_size: TILE_SIZE,
                state: "building".into(),
                display_revision: None,
                stretch_low: None,
                stretch_high: None,
                sectors,
                error: None,
                reduction: REDUCTION.into(),
            },
            frames,
        )
        .unwrap()
    }

    #[test]
    fn one_frozen_mapping_encodes_equal_calibrated_values_identically_across_frames() {
        let mut built = sequence(
            vec![
                frame(&[1.0, 5.0]),
                frame(&[5.0, f32::NAN]),
                frame(&[9.0, 1.0]),
            ],
            &["ready"; 3],
        );
        assert!(
            tile(&built, "s0", 0, 0, 1).unwrap_err().0,
            "no tiles before the mapping"
        );
        finish(&mut built);
        assert_eq!(built.info.state, "ready");
        assert_eq!(
            (built.info.stretch_low, built.info.stretch_high),
            (Some(1.0), Some(9.0))
        );
        let decode = |id: &str| {
            image::load_from_memory(&tile(&built, id, 0, 0, 1).unwrap())
                .unwrap()
                .to_rgba8()
        };
        let (a, b, c) = (decode("s0"), decode("s1"), decode("s2"));
        assert_eq!(
            a.get_pixel(1, 0),
            b.get_pixel(0, 0),
            "5.0 is the same grey in two frames"
        );
        assert_eq!(
            a.get_pixel(0, 0),
            c.get_pixel(1, 0),
            "1.0 is the same grey in two frames"
        );
        assert_eq!(
            b.get_pixel(1, 0).0,
            [24, 28, 36, 255],
            "no-data is not black"
        );
        assert_eq!(a.get_pixel(0, 0).0, [0, 0, 0, 255]);
        assert!(
            tile(&built, "s0", 0, 0, 2).unwrap_err().0,
            "stale display revision refused"
        );
        assert!(!tile(&built, "nope", 0, 0, 1).unwrap_err().0);
    }

    #[test]
    fn a_failed_frame_blocks_the_mapping_instead_of_shortening_the_sequence() {
        let mut built = sequence(vec![frame(&[1.0, 2.0]), None], &["ready", "failed"]);
        finish(&mut built);
        assert_eq!(built.info.state, "failed");
        assert_eq!(built.info.display_revision, None);
        assert!(built.info.error.as_deref().unwrap().contains("s1"));
        let mut empty = sequence(vec![frame(&[f32::NAN, f32::NAN])], &["ready"]);
        finish(&mut empty);
        assert_eq!(empty.info.state, "failed");
    }

    #[test]
    fn a_zero_complete_frame_blocks_a_populated_sequence_with_its_own_message() {
        let mut built = sequence(
            vec![frame(&[1.0, 2.0]), Some(Frame::Memory(DataTiles::new()))],
            &["ready", "ready"],
        );
        finish(&mut built);
        assert_eq!(built.info.state, "failed");
        assert_eq!(built.info.display_revision, None);
        assert_eq!(built.info.sectors[0].state, "ready");
        assert_eq!(built.info.sectors[1].state, "failed");
        let error = built.info.error.as_deref().unwrap();
        assert!(error.contains("s1"), "{error}");
        assert!(
            error.contains("no pixel with every selected band"),
            "{error}"
        );
    }

    #[test]
    fn a_zero_complete_build_result_is_recorded_as_failed() {
        let info = SectorFrameInfo {
            id: "solo".into(),
            revision: 1,
            state: "building".into(),
            error: None,
            snippets: 1,
            members: Vec::new(),
            fingerprint: "solo-fingerprint".into(),
            bands: vec![1],
            releases: vec![],
            mjd_start: 0.0,
            mjd_end: 0.0,
            utc_start: String::new(),
            utc_end: String::new(),
            complete_pixels: 0,
            target_coverage: 0.0,
            target_covered: false,
            contributions: None,
            count_bytes: None,
            bytes_read: 0,
            requests: 0,
            data_tiles: vec![],
        };
        let holder = sequence(vec![None], &["failed"]);
        let root = temp_root();
        let build = sector_build(
            &root,
            "solo-fingerprint",
            SectorImage {
                tiles: DataTiles::new(),
                complete_pixels: 0,
            },
            &[0],
        );
        let staging = build.staging.path().to_path_buf();
        let stored = store_result(
            info,
            holder.directory(),
            0,
            Ok(BuildOutcome::Built(build)),
            2,
            1,
        );
        assert!(stored.frame.is_none());
        assert!(stored.artifact.is_none(), "zero-complete is never cached");
        let info = stored.info;
        assert_eq!(info.state, "failed");
        assert_eq!(info.bytes_read, 12);
        assert!(info.contributions.is_some(), "all bands were accumulated");
        assert!(info.count_bytes.is_some());
        assert!(!staging.exists());
        assert!(
            holder.directory().join("counts-0.bin").exists(),
            "the completed count sidecar is retained until sequence release"
        );
        let error = info.error.unwrap();
        assert!(error.contains("solo"), "{error}");
        assert!(
            error.contains("no pixel with every selected band"),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn large_mappings_stream_through_the_bounded_histogram() {
        let len = (TILE_SIZE * TILE_SIZE) as usize;
        let plane = |offset: usize| DataTile {
            value: (0..len).map(|index| (offset + index) as f32).collect(),
            variance: vec![f32::NAN; len],
        };
        let frames = vec![Some(Frame::Memory(DataTiles::from([
            ((0, 0), plane(0)),
            ((1, 0), plane(len)),
        ])))];
        let (low, high) = shared_mapping(&frames).unwrap().expect("finite values");
        let last = (2 * len - 1) as f64;
        let expected_low = (last * 0.02).round() as f32;
        let expected_high = (last * 0.995).round() as f32;
        assert!((low - expected_low).abs() <= 2.0, "{low} vs {expected_low}");
        assert!(
            (high - expected_high).abs() <= 2.0,
            "{high} vs {expected_high}"
        );
        assert!(low <= high);
    }

    #[test]
    fn variance_tiles_are_raw_little_endian_f32_and_missing_tiles_are_not_ready() {
        let mut built = sequence(vec![frame(&[1.0, 5.0])], &["ready"]);
        finish(&mut built);
        let bytes = variance_tile(&built, "s0", 0, 0).unwrap();
        assert_eq!(bytes.len(), (TILE_SIZE * TILE_SIZE * 4) as usize);
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0.25);
        assert!(!variance_tile(&built, "s0", 1, 0).unwrap_err().0);
        let building = sequence(vec![frame(&[1.0])], &["ready"]);
        assert!(variance_tile(&building, "s0", 0, 0).unwrap_err().0);
    }

    #[test]
    fn disk_frames_round_trip_tiles_and_variance_bytes() {
        let mut memory = sequence(vec![frame(&[1.0, 5.0])], &["ready"]);
        finish(&mut memory);

        let mut disk = sequence(vec![frame(&[1.0, 5.0])], &["ready"]);
        let tiles = memory_tiles(disk.frames[0].take());
        let root = temp_root();
        let build = sector_build(
            &root,
            "disk-fingerprint",
            SectorImage {
                tiles,
                complete_pixels: 2,
            },
            &[0],
        );
        let stored = store_result(
            disk.info.sectors[0].clone(),
            disk.directory(),
            0,
            Ok(BuildOutcome::Built(build)),
            2,
            1,
        );
        let Stored {
            info,
            frame,
            artifact,
        } = stored;
        assert!(artifact.is_some());
        assert!(frame.is_some());
        disk.info.sectors[0] = info;
        disk.frames[0] = frame;
        assert!(matches!(disk.frames[0].as_deref(), Some(Frame::Disk(_))));
        assert_eq!(
            disk.info.sectors[0].data_tiles,
            vec![TileCoordinate { x: 0, y: 0 }]
        );
        assert!(disk.info.sectors[0].contributions.is_some());
        assert!(disk.info.sectors[0].count_bytes.is_some());
        finish(&mut disk);
        assert_eq!(disk.info.state, "ready");
        assert_eq!(
            (disk.info.stretch_low, disk.info.stretch_high),
            (memory.info.stretch_low, memory.info.stretch_high)
        );

        assert_eq!(
            tile(&memory, "s0", 0, 0, 1).unwrap(),
            tile(&disk, "s0", 0, 0, 1).unwrap(),
            "a disk frame encodes the same PNG bytes as the same in-memory frame"
        );
        assert_eq!(
            variance_tile(&memory, "s0", 0, 0).unwrap(),
            variance_tile(&disk, "s0", 0, 0).unwrap(),
            "the raw variance bytes are unchanged by the disk frame"
        );

        {
            let Frame::Disk(file) = disk.frames[0].as_deref().unwrap() else {
                unreachable!()
            };
            let read = file.read_tile(0, 0).unwrap().unwrap();
            assert_eq!(&read.value[..2], &[1.0, 5.0]);
            assert_eq!(&read.variance[..2], &[0.25, 0.25]);
            assert!(file.read_tile(1, 0).unwrap().is_none());
        }
        drop(disk);
        drop(artifact);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn canonical_execution_makes_reversed_members_byte_identical_and_same_fingerprint() {
        let region = region();
        let values = [("a", 1e16_f32), ("b", -1e16_f32), ("c", 1.0_f32)];
        let plans: Vec<_> = values
            .iter()
            .map(|(id, _)| plan(id, 0, 1.0, region.clone()))
            .collect();
        let find = lookup(plans.clone());
        let admitted_order = admit(&region, vec![sector("s1", &["a", "b", "c"])], find).unwrap();
        let find = lookup(plans.clone());
        let admitted_reverse = admit(&region, vec![sector("s1", &["c", "b", "a"])], find).unwrap();
        let forward_info = frame_info(&admitted_order[0]).unwrap();
        let reverse_info = frame_info(&admitted_reverse[0]).unwrap();
        assert_eq!(
            forward_info.fingerprint, reverse_info.fingerprint,
            "submitted order must not change the fingerprint"
        );
        assert_eq!(
            forward_info
                .members
                .iter()
                .map(|member| member.snippet.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"],
            "public member order stays submitted"
        );

        let root = temp_root();
        let values: BTreeMap<String, f32> = values
            .iter()
            .map(|(id, value)| ((*id).to_string(), *value))
            .collect();
        let (forward, forward_staging, forward_order) = ordered_build(
            &region,
            &values,
            &admitted_order[0].members,
            &root,
            "forward",
        )
        .await;
        let (reverse, reverse_staging, reverse_order) = ordered_build(
            &region,
            &values,
            &admitted_reverse[0].members,
            &root,
            "reverse",
        )
        .await;
        assert_eq!(
            forward_order, reverse_order,
            "members are fetched in the same canonical order"
        );
        let first_path = forward_staging.join("frame.bin");
        FrameFile::create(&first_path, forward.image.tiles).unwrap();
        let first_bytes = std::fs::read(&first_path).unwrap();
        let second_path = reverse_staging.join("frame.bin");
        FrameFile::create(&second_path, reverse.image.tiles).unwrap();
        let second_bytes = std::fs::read(&second_path).unwrap();
        assert_eq!(first_bytes, second_bytes, "frame bytes are byte-identical");
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn ordered_build(
        region: &ObservationRegion,
        values: &BTreeMap<String, f32>,
        members: &[Arc<SnippetPlan>],
        root: &Path,
        name: &str,
    ) -> (SectorBuild, PathBuf, Vec<String>) {
        let values = values.clone();
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_for_fetch = order.clone();
        let fetch = move |plan: &Arc<SnippetPlan>| {
            let id = plan.row.id.clone();
            let value = values[&id];
            let order = order_for_fetch.clone();
            async move {
                order.lock().unwrap().push(id);
                Ok(cutout(value, 1.0, 0.753, vec![0; 25]))
            }
        };
        let staging = pattern_staging(root, name);
        let grid = region.grid();
        let built = build_sector(&fetch, &grid, members, staging).await.unwrap();
        let staging = built.staging.path().to_path_buf();
        let order = order.lock().unwrap().clone();
        (built, staging, order)
    }

    fn pattern_staging(root: &Path, name: &str) -> artifact::Staging {
        artifact::Staging::create(root, name).unwrap()
    }

    #[tokio::test]
    async fn a_failed_band_removes_the_partial_count_sidecar() {
        let region = region();
        let plans = vec![
            plan("a", 0, 1.0, region.clone()),
            plan("b", 1, 2.0, region.clone()),
        ];
        let admitted = admit(&region, vec![sector("s1", &["a", "b"])], lookup(plans)).unwrap();
        let root = temp_root();
        let staging = pattern_staging(&root, "partial");
        let counts_path = staging.path().join("counts.bin");
        let path = counts_path.clone();
        let observed = Arc::new(Mutex::new(false));
        let observed_in_fetch = observed.clone();
        let fetch = move |plan: &Arc<SnippetPlan>| {
            let path = path.clone();
            let observed = observed_in_fetch.clone();
            let fail = plan.support.band == 1;
            let wavelength = if fail { 0.771 } else { 0.753 };
            async move {
                if fail {
                    *observed.lock().unwrap() = path.exists();
                    return Err("synthetic sampling failure".to_string());
                }
                Ok(cutout(2.0, 1.0, wavelength, vec![0; 25]))
            }
        };
        let grid = region.grid();
        let result = build_sector(&fetch, &grid, &admitted[0].members, staging).await;
        assert!(result.is_err());
        assert!(
            *observed.lock().unwrap(),
            "the first band had already written a partial sidecar"
        );
        assert!(
            !counts_path.exists(),
            "the partial count sidecar is removed on early failure"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_cache_hit_reports_zero_source_reads_and_survives_sequence_delete() {
        let jobs = Arc::new(crate::Jobs::default());
        let region = region();
        let plans = vec![plan("a", 0, 1.0, region.clone())];
        let admitted = admit(&region, vec![sector("s1", &["a"])], lookup(plans.clone())).unwrap();
        let sector_info = frame_info(&admitted[0]).unwrap();
        let fingerprint = sector_info.fingerprint.clone();
        let root = temp_root();
        let tiles = DataTiles::from([((0, 0), data_tile(&[1.0, 5.0]))]);
        let artifact = artifact_for(&root, &fingerprint, tiles.clone(), 2);
        jobs.cache_artifact(artifact);
        let information = |sector: &SectorFrameInfo| SequenceInfo {
            sequence_id: format!("cache-{}", sector.id),
            width: region.grid().width,
            height: region.grid().width,
            tile_size: TILE_SIZE,
            state: "building".into(),
            display_revision: None,
            stretch_low: None,
            stretch_high: None,
            sectors: vec![sector.clone()],
            error: None,
            reduction: REDUCTION.into(),
        };
        let first = Arc::new(Mutex::new(
            Sequence::new(information(&sector_info), vec![None]).unwrap(),
        ));
        run(
            first.clone(),
            region.grid(),
            admitted,
            jobs.builds.clone(),
            jobs.clone(),
        )
        .await;
        {
            let sequence = first.lock().unwrap();
            assert_eq!(sequence.info.state, "ready");
            assert_eq!(sequence.info.sectors[0].bytes_read, 0);
            assert_eq!(sequence.info.sectors[0].requests, 0);
            assert_eq!(sequence.info.sectors[0].complete_pixels, 2);
            assert!(sequence.info.sectors[0].contributions.is_some());
            assert_eq!(sequence.info.sectors[0].count_bytes, Some(6));
        }
        let admitted = admit(&region, vec![sector("s1", &["a"])], lookup(plans)).unwrap();
        let second = Arc::new(Mutex::new(
            Sequence::new(information(&sector_info), vec![None]).unwrap(),
        ));
        run(
            second.clone(),
            region.grid(),
            admitted,
            jobs.builds.clone(),
            jobs.clone(),
        )
        .await;
        let expected = tile(&second.lock().unwrap(), "s1", 0, 0, 1).unwrap();
        assert_eq!(second.lock().unwrap().info.sectors[0].bytes_read, 0);
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "both sequences used one numerical artifact"
        );
        drop(first);
        assert!(jobs.cached_artifact(&fingerprint).is_some());
        assert_eq!(
            tile(&second.lock().unwrap(), "s1", 0, 0, 1).unwrap(),
            expected,
            "deleting one sequence does not break another sequence using the artifact"
        );
        drop(second);
        jobs.expire(std::time::Instant::now() + crate::resources::IDLE);
        assert!(jobs.cached_artifact(&fingerprint).is_none());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "idle eviction removes the unreferenced artifact directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn aborting_an_artifact_build_removes_its_staging_directory() {
        let root = temp_root();
        let staging = pattern_staging(&root, "abort");
        let directory = staging.path().to_path_buf();
        let region = region();
        let plans = vec![plan("a", 0, 1.0, region.clone())];
        let task = tokio::spawn(async move {
            let fetch = |_plan: &Arc<SnippetPlan>| async {
                std::future::pending::<Result<Cutout, String>>().await
            };
            build_sector(&fetch, &region.grid(), &plans, staging).await
        });
        tokio::task::yield_now().await;
        task.abort();
        let error = match task.await {
            Ok(_) => panic!("the aborted build must not complete"),
            Err(error) => error,
        };
        assert!(error.is_cancelled());
        assert!(
            !directory.exists(),
            "an aborted build leaves no partial artifact directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disk_frame_lists_data_tiles_in_row_then_column_order() {
        let holder = sequence(vec![None], &["failed"]);
        let empty = || DataTile {
            value: vec![f32::NAN; TILE_PIXELS],
            variance: vec![f32::NAN; TILE_PIXELS],
        };
        let mut marker = empty();
        marker.value[0] = 7.0;
        marker.variance[0] = 0.5;
        let tiles = DataTiles::from([
            ((1, 2), marker),
            ((0, 2), empty()),
            ((1, 0), empty()),
            ((0, 0), empty()),
        ]);
        let Frame::Disk(file) = disk_frame(holder.directory(), "order.bin", tiles) else {
            unreachable!()
        };
        assert_eq!(
            file.data_tiles,
            vec![
                TileCoordinate { x: 0, y: 0 },
                TileCoordinate { x: 1, y: 0 },
                TileCoordinate { x: 0, y: 2 },
                TileCoordinate { x: 1, y: 2 },
            ]
        );
        assert_eq!(file.offsets.len(), 4);
        let read = file.read_tile(1, 2).unwrap().unwrap();
        assert_eq!(read.value[0], 7.0);
        assert_eq!(read.variance[0], 0.5);
    }

    #[test]
    fn shared_mapping_streams_disk_frames_exactly_for_small_samples() {
        let holder = sequence(vec![None], &["failed"]);
        let memory_tiles = |values: &[f32]| match frame(values) {
            Some(Frame::Memory(tiles)) => tiles,
            _ => unreachable!(),
        };
        let frames = vec![
            Some(disk_frame(
                holder.directory(),
                "a.bin",
                memory_tiles(&[1.0, 5.0]),
            )),
            Some(disk_frame(
                holder.directory(),
                "b.bin",
                memory_tiles(&[9.0, 1.0]),
            )),
        ];
        assert_eq!(shared_mapping(&frames).unwrap(), Some((1.0, 9.0)));
        assert_eq!(
            shared_mapping(&[frame(&[1.0, 5.0]), frame(&[9.0, 1.0])]).unwrap(),
            shared_mapping(&frames).unwrap(),
            "disk and memory frames share the same mapping"
        );
    }

    #[test]
    fn large_mappings_stream_disk_frames_through_the_bounded_histogram() {
        let holder = sequence(vec![None], &["failed"]);
        let plane = |offset: usize| DataTile {
            value: (0..TILE_PIXELS)
                .map(|index| (offset + index) as f32)
                .collect(),
            variance: vec![f32::NAN; TILE_PIXELS],
        };
        let tiles = DataTiles::from([((0, 0), plane(0)), ((1, 0), plane(TILE_PIXELS))]);
        let frames = vec![Some(disk_frame(
            holder.directory(),
            "large.bin",
            tiles.clone(),
        ))];
        let expected = shared_mapping(&[Some(Frame::Memory(tiles))]).unwrap();
        assert_eq!(shared_mapping(&frames).unwrap(), expected);
    }

    #[test]
    fn zero_complete_builds_are_never_stored_as_a_disk_frame() {
        let holder = sequence(vec![None], &["failed"]);
        let info = SectorFrameInfo {
            id: "solo".into(),
            revision: 1,
            state: "building".into(),
            error: None,
            snippets: 1,
            members: Vec::new(),
            fingerprint: "zero-fingerprint".into(),
            bands: vec![1],
            releases: vec![],
            mjd_start: 0.0,
            mjd_end: 0.0,
            utc_start: String::new(),
            utc_end: String::new(),
            complete_pixels: 0,
            target_coverage: 0.0,
            target_covered: false,
            contributions: None,
            count_bytes: None,
            bytes_read: 0,
            requests: 0,
            data_tiles: vec![],
        };
        let root = temp_root();
        let build = sector_build(
            &root,
            "zero-fingerprint",
            SectorImage {
                tiles: DataTiles::new(),
                complete_pixels: 0,
            },
            &[0],
        );
        let stored = store_result(
            info,
            holder.directory(),
            0,
            Ok(BuildOutcome::Built(build)),
            2,
            1,
        );
        assert!(stored.frame.is_none());
        assert!(stored.artifact.is_none());
        assert_eq!(stored.info.state, "failed");
        assert!(!holder.directory().join("frame-0.bin").exists());
        assert!(holder.directory().join("counts-0.bin").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn aborting_a_running_sequence_task_still_removes_its_directory() {
        let built = sequence(vec![None], &["failed"]);
        let directory = built.directory().to_path_buf();
        let owner = Arc::new(Mutex::new(built));
        let held = owner.clone();
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(owner);
        assert!(!directory.exists(), "the aborted task's Drop removes it");
    }

    #[test]
    fn dropping_a_sequence_removes_its_temporary_directory() {
        if std::env::var_os("SPEARV_SEQUENCE_DIR").is_none() {
            assert_eq!(
                sequence_root(),
                std::env::temp_dir().join("spearv-sequences"),
                "the default sequence root must stay under the system temp directory"
            );
        }
        let mut built = sequence(vec![frame(&[1.0])], &["ready"]);
        let directory = built.directory().to_path_buf();
        assert!(directory.starts_with(sequence_root()));
        let tiles = memory_tiles(built.frames[0].take());
        built.frames[0] = Some(Arc::new(disk_frame(&directory, "frame-0.bin", tiles)));
        let frame_file = directory.join("frame-0.bin");
        assert!(frame_file.exists());
        assert!(directory.exists());
        drop(built);
        assert!(!directory.exists(), "the sequence directory is removed");
    }

    #[cfg(unix)]
    #[test]
    fn startup_sweep_removes_only_this_process_directories() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "spearv-sweep-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let pid = std::process::id();
        let other = std::os::unix::process::parent_id();
        assert_ne!(other, pid, "the parent process has a different PID");
        let own_sequence = root.join(format!("{pid}-0-sequence-9"));
        let own_artifact = root.join(format!("{pid}-1-artifact-fingerprint"));
        let other_sequence = root.join(format!("{other}-0-sequence-10"));
        let other_artifact = root.join(format!("{other}-1-artifact-fingerprint"));
        let unrelated = root.join("keep-me");
        let own_file = root.join(format!("{pid}-2-sequence-11"));
        std::fs::create_dir_all(&own_sequence).unwrap();
        std::fs::create_dir_all(&own_artifact).unwrap();
        std::fs::create_dir_all(&other_sequence).unwrap();
        std::fs::create_dir_all(&other_artifact).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(&own_file, b"not a directory").unwrap();
        sweep_stale(&root);
        assert!(
            !own_sequence.exists(),
            "this process's sequence directory is removed"
        );
        assert!(
            !own_artifact.exists(),
            "this process's artifact staging is removed"
        );
        assert!(
            other_sequence.exists(),
            "a live other PID's sequence directory survives"
        );
        assert!(
            other_artifact.exists(),
            "a live other PID's artifact staging survives"
        );
        assert!(unrelated.exists(), "unrelated names survive");
        assert!(own_file.exists(), "files are never removed");
        assert!(root.is_dir(), "the configured root survives");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Runs `finalize` with a mapping computation that blocks until released,
    /// and returns once that computation has started.
    async fn gated_finalize(
        owner: Arc<Mutex<Sequence>>,
    ) -> (tokio::task::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let task = tokio::spawn(async move {
            finalize(&owner, move |frames| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                shared_mapping(frames)
            })
            .await
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        (task, release_tx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_is_readable_while_the_shared_mapping_is_computed() {
        let owner = Arc::new(Mutex::new(sequence(
            vec![frame(&[1.0, 5.0]), frame(&[9.0, 1.0])],
            &["ready"; 2],
        )));
        let (task, release) = gated_finalize(owner.clone()).await;
        {
            // The computation is in progress and blocked: the lock must be free.
            let sequence = owner
                .try_lock()
                .expect("the sequence lock is free while the mapping is computed");
            assert_eq!(sequence.info.state, "building");
            assert_eq!(sequence.info.display_revision, None);
        }
        release.send(()).unwrap();
        task.await.unwrap();
        let sequence = owner.lock().unwrap();
        assert_eq!(sequence.info.state, "ready");
        assert_eq!(
            (sequence.info.stretch_low, sequence.info.stretch_high),
            (Some(1.0), Some(9.0))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_mapping_finished_after_release_is_not_published() {
        let owner = Arc::new(Mutex::new(sequence(vec![frame(&[1.0, 5.0])], &["ready"])));
        let (task, release) = gated_finalize(owner.clone()).await;
        owner.lock().unwrap().released = true;
        release.send(()).unwrap();
        task.await.unwrap();
        let sequence = owner.lock().unwrap();
        assert_eq!(sequence.info.state, "building");
        assert_eq!(sequence.info.display_revision, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_frame_is_written_without_the_lock_and_not_published_after_release() {
        let tiles = || DataTiles::from([((0, 0), data_tile(&[1.0, 5.0]))]);
        let jobs = crate::Jobs::default();
        let root = temp_root();
        let owner = Arc::new(Mutex::new(sequence(vec![None], &["building"])));
        let directory = owner.lock().unwrap().directory().to_path_buf();
        let artifact = artifact_for(&root, "fingerprint-0", tiles(), 2);
        assert!(
            store_frame(
                &owner,
                &directory,
                0,
                Ok(BuildOutcome::Hit(artifact)),
                &jobs
            )
            .await
        );
        {
            let sequence = owner.lock().unwrap();
            assert_eq!(sequence.info.sectors[0].state, "ready");
            assert_eq!(sequence.info.sectors[0].bytes_read, 0);
            assert!(matches!(
                sequence.frames[0].as_deref(),
                Some(Frame::Disk(_))
            ));
        }
        let released = Arc::new(Mutex::new(sequence(vec![None], &["building"])));
        let directory = released.lock().unwrap().directory().to_path_buf();
        released.lock().unwrap().released = true;
        let artifact = artifact_for(&root, "fingerprint-0", tiles(), 2);
        assert!(
            !store_frame(
                &released,
                &directory,
                0,
                Ok(BuildOutcome::Hit(artifact)),
                &jobs
            )
            .await
        );
        let sequence = released.lock().unwrap();
        assert!(sequence.frames[0].is_none());
        assert_eq!(sequence.info.sectors[0].state, "building");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tiles_encode_from_the_captured_frame_while_another_holder_has_the_lock() {
        let mut built = sequence(vec![frame(&[1.0, 5.0])], &["ready"]);
        finish(&mut built);
        let expected_png = tile(&built, "s0", 0, 0, 1).unwrap();
        let expected_variance = variance_tile(&built, "s0", 0, 0).unwrap();
        let owner = Mutex::new(built);
        let (source, frame) = {
            let sequence = owner.lock().unwrap();
            (
                tile_source(&sequence, "s0", 1).unwrap(),
                variance_source(&sequence, "s0").unwrap(),
            )
        };
        let _held = owner.lock().unwrap();
        assert_eq!(source.encode(0, 0).unwrap(), expected_png);
        assert_eq!(
            encode_variance(&frame, "s0", 0, 0).unwrap(),
            expected_variance
        );
    }
}
