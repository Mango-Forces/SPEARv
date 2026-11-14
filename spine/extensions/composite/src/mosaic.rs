//! Progressive tiled mosaic for every render mode.
//!
//! A [`Recipe`] says what a view reads and how it is shown. Reading is
//! exposure-major. A file can only be read by whole detector rows, so per-tile
//! reads would fetch the same rows again for each tile. Every file is read
//! once; each planned tile owns a charged library session, and a settled tile
//! is published as an immutable physical snapshot.

mod color_display;
pub(super) mod display_revision;
mod display_file;

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use spearv_core::math::{
    spawn_math, ImageMath, MaskPolicy, MathControl, MathError, MathSession, NativeInput, Product, RasterOrder,
    Shape, TargetGrid, Tile, TileOutput, Values,
};
use spearv_core::{
    cluster_visits, delta_bands, delta_rgb, delta_scale, delta_sides, query_pairs, visit_index, ApiError, Context,
    Error, Exposure, ExposureGeometry, Filter, ImageSource, Mode, PixelWindow, ReleaseId, TanGrid, View, WorkBudget,
    BANDS, NO_DELTA_EXPOSURE, PIXEL_SCALE_DEG,
};
use tokio::task::JoinSet;
use utoipa::{IntoParams, ToSchema};

use super::{
    band_span, color_bands_of, color_pixel, color_view, grey_level, parse_query, percentile, shade, source, view_of,
    ColorBands, Stretch, MAX_NATIVE_PIXELS, NODATA,
};

/// Tile side on the parent grid, shared with the one-shot render routes.
pub(super) const TILE: u32 = 256;
/// Largest encoded scale position; `u16::MAX` is reserved for no-data.
const SCALE_MAX: u16 = u16::MAX - 1;
/// Files downloading at once, unless `SPEARV_MOSAIC_READS` says otherwise. Each
/// is three or four range requests, and a home link stops improving near 32
/// requests. A server beside the bucket wants far more.
const READS: usize = 8;
const PLANS: usize = 64;
const MAX_JOBS: usize = 4;
/// A job with no status/tile activity this long stops reading and may be dropped.
/// Matched to the browser keepalive (tab open) plus a refresh grace window.
const ABANDON_AFTER: Duration = Duration::from_secs(10 * 60);
/// A tile still receiving exposures is reported at most this often.
const DIRTY_EVERY: Duration = Duration::from_secs(1);
/// Detector step for a plan's footprint. See [`footprint`].
const LATTICE: i32 = 16;
/// A channel's stretch stops moving once this share of the grid has data.
const LOCK_FRACTION: f64 = 0.25;
/// Pixels sampled for a stretch estimate before the final one.
const ESTIMATE_SAMPLES: usize = 200_000;

#[derive(Serialize, ToSchema)]
pub struct MosaicInfo {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub tile: u32,
    pub cols: u32,
    pub rows: u32,
    /// Files whose wavelength range can reach a channel. Planning may drop some.
    pub exposures: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MosaicState {
    Planning,
    Reading,
    Done,
    Failed,
    Cancelled,
}

#[derive(Serialize, ToSchema)]
pub struct MosaicTile {
    pub x: u32,
    pub y: u32,
    pub version: u32,
    /// No read still in the queue can change this tile. The stretch can, until
    /// the job is done.
    #[serde(rename = "final")]
    pub settled: bool,
}

#[derive(Serialize, ToSchema)]
pub struct ActiveTile {
    pub x: u32,
    pub y: u32,
}

#[derive(Serialize, ToSchema)]
pub struct MosaicStatus {
    pub state: MosaicState,
    pub seq: u64,
    pub exposures_done: usize,
    pub exposures_total: usize,
    /// Reads that gave no pixels. Above zero the picture has holes, even when
    /// `state` is `done`.
    pub reads_failed: usize,
    pub bytes_read: u64,
    pub elapsed_ms: u64,
    /// Tiles with no pending read, out of `tiles_total`.
    pub tiles_done: usize,
    /// Tiles that any planned read reaches.
    pub tiles_total: usize,
    /// Settled reads per channel, in channel order. A failed read counts as
    /// settled, so a painter box never stalls on it.
    pub channels_done: Vec<usize>,
    /// Planned reads per channel.
    pub channels_total: Vec<usize>,
    /// Estimated download for every planned read. Zero while planning.
    pub bytes_planned: u64,
    /// Per channel, how many path tiles from the start are finished: every read
    /// of that channel that reaches the tile has settled. Its box stands on the
    /// next tile, the one being filled, and moves when that one finishes.
    pub frontiers: Vec<usize>,
    pub active: Vec<ActiveTile>,
    /// The planned tiles in snake order: top-left along the top, down the
    /// right, back along the bottom, up the left, spiralling inward. Every
    /// tile is on it, even one no exposure touches, so a painter box walking
    /// it never teleports. Empty while planning.
    pub order: Vec<ActiveTile>,
    /// Tiles changed since `since`.
    pub tiles: Vec<MosaicTile>,
    pub error: Option<String>,
}

/// The view parameters, plus the colour switch that `render.png` has no need of.
/// This documents the route. Repeated `pin` needs the shared raw-query parser.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
pub struct MosaicQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov_l: f64,
    pub fov_h: f64,
    pub mode: Option<String>,
    pub dr: Option<String>,
    pub mask: Option<String>,
    #[serde(default)]
    pub pin: Vec<String>,
    /// `1` shows the three-band false colour, whatever `mode` and `pin` say.
    pub color: Option<String>,
    /// Red channel's centre band, 0..101. Default 90.
    pub color_r: Option<u16>,
    /// Red channel's width in bands, 1..102, centred on `color_r`. Default 5.
    pub color_r_width: Option<u16>,
    /// Green channel's centre band, 0..101. Default 40.
    pub color_g: Option<u16>,
    /// Green channel's width in bands, 1..102, centred on `color_g`. Default 5.
    pub color_g_width: Option<u16>,
    /// Blue channel's centre band, 0..101. Default 5.
    pub color_b: Option<u16>,
    /// Blue channel's width in bands, 1..102, centred on `color_b`. Default 5.
    pub color_b_width: Option<u16>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatusQuery {
    /// The `seq` of the previous status. Omit it for every tile so far.
    pub since: Option<u64>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TileQuery {
    pub x: u32,
    pub y: u32,
    /// The tile version. It only makes the URL change; the current pixels are served.
    #[allow(dead_code)]
    pub v: Option<u32>,
    /// One channel's layer, in channel order (blue, green, red for colour).
    /// Omit it for the full tile.
    pub channel: Option<usize>,
    /// `f32` returns one final physical channel; `scale16` returns one
    /// normalized colour layer. Both require `channel`. Omit it, or use `png`,
    /// for the existing PNG tile.
    pub format: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct RawMetadata {
    pub format_version: u8,
    pub job_id: String,
    pub state: MosaicState,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    pub dtype: String,
    pub orientation: String,
    pub nodata: String,
    pub source_recipe: RawSourceRecipe,
    pub channels: Vec<RawChannelMetadata>,
}

#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct RawSourceRecipe {
    /// `color`, `composite`, or `range`.
    pub mode: String,
    pub mask: bool,
    /// Band numbers are the 1..=102 numbers shown in the UI.
    pub pins: Vec<RawPin>,
}

#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct RawPin {
    pub center_band: u16,
    pub width: u16,
    pub visit: Option<u16>,
    pub sign: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct RawChannelMetadata {
    pub index: usize,
    pub role: String,
    /// The 1..=102 band number shown in the UI, when the recipe has one centre.
    pub center_band: Option<u16>,
    pub width: u16,
    /// The contributing 1..=102 band numbers shown in the UI.
    pub bands: Vec<u16>,
    pub wavelength_min_um: f64,
    pub wavelength_max_um: f64,
    pub finite_count: usize,
    pub default_black: Option<f32>,
    pub default_white: Option<f32>,
    /// The exact effective span used by the server render, including the
    /// constant-channel fallback of 1.
    pub default_span: Option<f32>,
    /// Physical value represented by encoded scale position zero. Present for
    /// normalized colour layers only.
    pub scale_offset: Option<f32>,
    /// Physical width represented by encoded scale positions 0..=65534.
    pub scale_span: Option<f32>,
}

/// Jobs in age order. Only this module sees them.
#[derive(Default)]
pub struct Jobs {
    list: Mutex<Vec<Arc<Job>>>,
    next_id: AtomicU64,
}

static JOBS: OnceLock<Arc<Jobs>> = OnceLock::new();

pub(super) fn shared_jobs() -> Arc<Jobs> {
    Arc::clone(JOBS.get_or_init(|| Arc::new(Jobs::default())))
}

pub(super) fn existing_jobs() -> Option<Arc<Jobs>> {
    JOBS.get().cloned()
}

pub(super) fn cleanup_display_caches(
    caches: Vec<Arc<display_file::DisplayCache>>,
) -> Result<(), Vec<String>> {
    let errors = caches
        .into_iter()
        .flat_map(|cache| cache.cleanup().err().unwrap_or_default())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return Ok(());
    }
    Err(errors)
}

impl Jobs {
    pub(super) fn begin_display_shutdown(&self) -> Vec<Arc<display_file::DisplayCache>> {
        let jobs = self.list.lock().expect("mosaic job list").clone();
        let caches = jobs.iter().fold(Vec::new(), |mut caches, job| {
            if !caches
                .iter()
                .any(|cache| Arc::ptr_eq(cache, &job.display_revisions.cache))
            {
                caches.push(Arc::clone(&job.display_revisions.cache));
            }
            caches
        });
        for cache in &caches {
            cache.close();
        }
        for job in jobs {
            job.display_revisions.cancel();
        }
        caches
    }
    fn find(&self, id: &str) -> Option<Arc<Job>> {
        self.list.lock().ok()?.iter().find(|job| job.id == id).cloned()
    }

    /// A running or finished job for this key. One that was cancelled or failed
    /// holds nothing worth joining, so the caller starts again.
    fn join(&self, key: &str) -> Option<Arc<Job>> {
        let list = self.list.lock().ok()?;
        let job = list.iter().find(|job| job.key == key)?;
        (!matches!(job.state(), MosaicState::Cancelled | MosaicState::Failed)).then(|| Arc::clone(job))
    }

    /// Registers a job, unless a concurrent request got there first. The bool is
    /// true when the caller must run it.
    fn insert(
        &self,
        key: String,
        grid: TanGrid,
        exposures: usize,
        view: (Recipe, bool),
        math: Arc<dyn ImageMath>,
        budget: Arc<WorkBudget>,
    ) -> (Arc<Job>, bool) {
        if let Some(job) = self.join(&key) {
            return (job, false);
        }
        let id = format!("m{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let job = Arc::new(Job::new(id, key, grid, exposures, view, math, budget));
        let mut list = self.list.lock().expect("mosaic job list");
        list.retain(|old| {
            if old.key != job.key { return true; }
            old.display_revisions.cancel();
            false
        });
        while list.len() >= MAX_JOBS {
            // Prefer idle finished, then any idle (past ABANDON_AFTER). Only if every
            // slot is still warm do we drop a finished job, then the coldest runner.
            // Keepalive/status/tile traffic marks activity, so an open tab is warm.
            let victim = list
                .iter()
                .enumerate()
                .filter(|(_, old)| old.finished() && old.idle_for(ABANDON_AFTER))
                .min_by_key(|(index, old)| (old.polled_at(), *index))
                .map(|(index, _)| index)
                .or_else(|| {
                    list.iter()
                        .enumerate()
                        .filter(|(_, old)| old.idle_for(ABANDON_AFTER))
                        .min_by_key(|(index, old)| (old.polled_at(), *index))
                        .map(|(index, _)| index)
                })
                .or_else(|| {
                    list.iter()
                        .enumerate()
                        .filter(|(_, old)| old.finished())
                        .min_by_key(|(index, old)| (old.polled_at(), *index))
                        .map(|(index, _)| index)
                })
                .or_else(|| (0..list.len()).min_by_key(|index| list[*index].polled_at()))
                .expect("the list is full");
            let old = list.remove(victim);
            old.cancel.store(true, Ordering::Relaxed);
            old.display_revisions.cancel();
        }
        eprintln!("mosaic {}: started {}", job.id, job.key);
        list.push(Arc::clone(&job));
        (job, true)
    }
}

/// What one channel reads. An empty `bands` takes every pixel, as `stack` does,
/// and with no `span` a file is read across all its rows.
#[derive(Clone)]
struct ChannelSpec {
    bands: Vec<u16>,
    span: Option<(f64, f64)>,
    /// Delta keeps `OUTLIER`, because that flag is how motion gets removed.
    ignore_outlier: bool,
}

/// How channels become a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Display {
    /// Blue, green, red. Stretched like `combine_color`.
    Color,
    /// One channel. Stretched like `encode`.
    Grey,
    /// Plus minus minus. Scaled and coloured like `delta.png`.
    Delta,
}

impl Display {
    /// Black and white points as percentiles of the physical values.
    fn percentiles(self) -> (f64, f64) {
        match self {
            Display::Color => (0.05, 0.995),
            Display::Grey | Display::Delta => (0.02, 0.995),
        }
    }
}

/// What a view reads and how it is shown.
#[derive(Clone)]
struct Recipe {
    display: Display,
    channels: Vec<ChannelSpec>,
    source: RawSourceRecipe,
    metadata: Vec<ChannelMetadataSpec>,
}

#[derive(Clone)]
struct ChannelMetadataSpec {
    role: &'static str,
    center_band: Option<u16>,
    width: u16,
}

#[derive(Default)]
struct Channel {
    /// Finite output pixels published so far. The stretch locks on this count.
    valid: usize,
    stretch: Option<Stretch>,
    locked: bool,
}

fn stretch_of(display: Display, mut values: Vec<f32>) -> Option<Stretch> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f32::total_cmp);
    if display == Display::Delta {
        return Some(Stretch {
            low: 0.0,
            span: delta_scale(&values),
        });
    }
    let (low, high) = display.percentiles();
    let (low, high) = (percentile(&values, low), percentile(&values, high));
    Some(Stretch {
        low,
        span: if high > low { high - low } else { 1.0 },
    })
}

#[derive(Clone)]
struct TileState {
    pending: u32,
    active: u32,
    version: u32,
    seq: u64,
    dirty: bool,
    reported: Option<Instant>,
    planned: bool,
}

struct Progress {
    state: MosaicState,
    display: Display,
    channels: Vec<Channel>,
    /// Settled reads per channel: merged or failed. A box reaches the end of
    /// the path exactly when its channel settles.
    channel_done: Vec<usize>,
    channel_total: Vec<usize>,
    bytes_planned: u64,
    /// Reads still in flight per channel and tile. A tile a channel never
    /// touches stays zero, so empty sky never blocks a box.
    channel_pending: Vec<Vec<u32>>,
    tiles: Vec<TileState>,
    seq: u64,
    /// Reads counted by `register`, whether started early or in the main run.
    registered: usize,
    exposures_done: usize,
    exposures_total: usize,
    reads_failed: usize,
    bytes_read: u64,
    finished: Option<Duration>,
    estimated: Option<Instant>,
    error: Option<String>,
    raw_metadata: Option<RawMetadata>,
}

impl Progress {
    /// Turn dirty tiles into new versions, no more than once a second each. A
    /// tile that just settled is not held back.
    fn publish(&mut self, now: Instant) {
        for tile in &mut self.tiles {
            let due = tile.reported.is_none_or(|at| now.duration_since(at) >= DIRTY_EVERY);
            if tile.dirty && (tile.pending == 0 || due) {
                self.seq += 1;
                tile.version += 1;
                tile.seq = self.seq;
                tile.dirty = false;
                tile.reported = Some(now);
            }
        }
    }

    /// Decrement this read's counters. Returns the tiles that just settled and,
    /// for this channel, the tiles whose last read of this channel landed.
    fn settle(&mut self, channel: usize, tiles: &[u32]) -> (Vec<u32>, Vec<u32>) {
        self.exposures_done += 1;
        self.channel_done[channel] += 1;
        let mut settled = Vec::new();
        for index in tiles {
            let tile = &mut self.tiles[*index as usize];
            tile.pending = tile.pending.saturating_sub(1);
            tile.active = tile.active.saturating_sub(1);
            if tile.pending == 0 {
                settled.push(*index);
            }
        }
        let mut channel_finished = Vec::new();
        for index in tiles {
            let pending = &mut self.channel_pending[channel][*index as usize];
            *pending = pending.saturating_sub(1);
            if *pending == 0 {
                channel_finished.push(*index);
            }
        }
        (settled, channel_finished)
    }
}

pub struct Job {
    id: String,
    key: String,
    grid: TanGrid,
    cols: u32,
    rows: u32,
    exposures: usize,
    recipe: Recipe,
    mask: bool,
    started: Instant,
    last_poll: Mutex<Instant>,
    /// Raw reads keep a running job alive without changing status-poll LRU.
    last_activity: Mutex<Instant>,
    cancel: AtomicBool,
    /// Bounds downloads of this job, early and main. A slot frees when its
    /// file lands, so the next download starts while that file is stacked.
    read_slots: tokio::sync::Semaphore,
    /// Bounds stacking to the cores. Files past their download wait here.
    stack_slots: tokio::sync::Semaphore,
    /// Downloads and stacks in flight together, which bounds cutouts in memory.
    width: usize,
    /// One bounded library call at a time per tile. Persistent running sums
    /// live in these sessions and are charged by their `MathLease`; they are
    /// not covered by the job's transient permit.
    sessions: Vec<tokio::sync::Mutex<Option<Box<dyn MathSession>>>>,
    /// Final physical tiles, one per channel and tile. Charged by their own
    /// leases and retained until the job is evicted.
    outputs: Mutex<Vec<Option<TileOutput>>>,
    math: Arc<dyn ImageMath>,
    budget: Arc<WorkBudget>,
    /// Cancellation/staleness control shared by every session of this job.
    control: MathControl,
    /// Strictly increasing `add` order, assigned while a tile is locked.
    order: AtomicU64,
    /// First math failure that is not a per-exposure read failure.
    fatal: Mutex<Option<String>>,
    /// Time spent merging, lock wait included: the one serial step.
    merge_nanos: AtomicU64,
    progress: Mutex<Progress>,
    display_revisions: display_revision::Revisions,
}

impl Job {
    fn new(
        id: String,
        key: String,
        grid: TanGrid,
        exposures: usize,
        (recipe, mask): (Recipe, bool),
        math: Arc<dyn ImageMath>,
        budget: Arc<WorkBudget>,
    ) -> Self {
        let cols = grid.width.div_ceil(TILE);
        let rows = grid.height.div_ceil(TILE);
        let tiles = (cols * rows) as usize;
        let reads = spearv_core::env::number("SPEARV_MOSAIC_READS", READS).max(1);
        // Stack concurrency must not exceed math workers: each stack holds a
        // worker via try_acquire, and exhaustion fails the whole job.
        let workers = spearv_core::env::number("SPEARV_PYTHON_WORKERS", 4).clamp(1, 32);
        let stacks = std::thread::available_parallelism()
            .map_or(4, usize::from)
            .min(workers)
            .max(1);
        let tile = TileState {
            pending: 0,
            active: 0,
            version: 0,
            seq: 0,
            dirty: false,
            reported: None,
            planned: false,
        };
        Self {
            id,
            key,
            cols,
            rows,
            exposures,
            mask,
            started: Instant::now(),
            last_poll: Mutex::new(Instant::now()),
            last_activity: Mutex::new(Instant::now()),
            cancel: AtomicBool::new(false),
            read_slots: tokio::sync::Semaphore::new(reads),
            stack_slots: tokio::sync::Semaphore::new(stacks),
            width: reads + stacks,
            sessions: (0..recipe.channels.len() * tiles)
                .map(|_| tokio::sync::Mutex::new(None))
                .collect(),
            outputs: Mutex::new((0..recipe.channels.len() * tiles).map(|_| None).collect()),
            math,
            budget,
            control: MathControl::new(Arc::new(AtomicU64::new(0))),
            order: AtomicU64::new(0),
            fatal: Mutex::new(None),
            merge_nanos: AtomicU64::new(0),
            display_revisions: display_revision::Revisions::default(),
            progress: Mutex::new(Progress {
                state: MosaicState::Planning,
                display: recipe.display,
                channels: recipe.channels.iter().map(|_| Channel::default()).collect(),
                channel_done: vec![0; recipe.channels.len()],
                channel_total: vec![0; recipe.channels.len()],
                bytes_planned: 0,
                channel_pending: vec![Vec::new(); recipe.channels.len()],
                tiles: vec![tile; tiles],
                seq: 0,
                registered: 0,
                exposures_done: 0,
                exposures_total: exposures,
                reads_failed: 0,
                bytes_read: 0,
                finished: None,
                estimated: None,
                error: None,
                raw_metadata: None,
            }),
            recipe,
            grid,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress.lock().expect("mosaic progress")
    }

    fn state(&self) -> MosaicState {
        self.lock().state
    }

    fn finished(&self) -> bool {
        !matches!(self.state(), MosaicState::Planning | MosaicState::Reading)
    }

    fn polled_at(&self) -> Instant {
        *self.last_poll.lock().expect("mosaic poll time")
    }

    fn abandoned(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
            || self.idle_for(ABANDON_AFTER)
    }

    /// True when neither status polls nor tile/raw activity happened within `for_`.
    fn idle_for(&self, for_: Duration) -> bool {
        self.last_activity.lock().expect("mosaic activity time").elapsed() > for_
    }

    fn info(&self) -> MosaicInfo {
        MosaicInfo {
            id: self.id.clone(),
            width: self.grid.width,
            height: self.grid.height,
            tile: TILE,
            cols: self.cols,
            rows: self.rows,
            exposures: self.exposures,
        }
    }

    /// Allocate the per-tile counters so reads can start while planning is
    /// still resolving other exposures. Running sums belong to math sessions.
    fn prepare(&self) {
        let mut progress = self.lock();
        let tiles = progress.tiles.len();
        for channel in 0..progress.channels.len() {
            if progress.channel_pending[channel].is_empty() {
                progress.channel_pending[channel] = vec![0; tiles];
            }
        }
    }

    fn tiles_len(&self) -> usize {
        (self.cols * self.rows) as usize
    }

    /// Output tile for a progress tile index, whose `y` counts PNG rows from
    /// the top. Math operates on FITS rows, so the row is mapped by
    /// [`tile_of_grid`].
    fn tile_of(&self, index: u32) -> Tile {
        tile_of_grid(index % self.cols, index / self.cols, self.grid.width, self.grid.height)
    }

    fn target(&self) -> TargetGrid {
        TargetGrid {
            ra: self.grid.ra,
            dec: self.grid.dec,
            shape: Shape {
                width: self.grid.width,
                height: self.grid.height,
            },
            scale_deg: PIXEL_SCALE_DEG,
        }
    }

    fn stretches(&self) -> usize {
        if self.recipe.display == Display::Delta {
            1
        } else {
            self.recipe.channels.len()
        }
    }

    /// Read one published channel value. The output tile plane is row-major in
    /// FITS order; `grid_y` is a 0-based FITS row of the parent grid.
    pub(super) fn value_from(
        outputs: &[Option<TileOutput>],
        cols: u32,
        grid_height: u32,
        channel: usize,
        tiles: usize,
        grid_x: u32,
        grid_y: u32,
    ) -> Option<f32> {
        let tile_x = grid_x / TILE;
        let png_y = (grid_height - 1 - grid_y) / TILE;
        let output = outputs.get(channel * tiles + (png_y * cols + tile_x) as usize)?.as_ref()?;
        let tile = output.tile();
        let shape = tile.shape;
        if grid_x < tile.x || grid_y < tile.y {
            return None;
        }
        let local_x = grid_x - tile.x;
        let local_y = grid_y - tile.y;
        if local_x >= shape.width || local_y >= shape.height {
            return None;
        }
        let Values::F32(values) = &output.planes().value else {
            return None;
        };
        values
            .get(local_y as usize * shape.width as usize + local_x as usize)
            .copied()
            .filter(|value| value.is_finite())
    }

    fn output(&self, channel: usize, tile: u32) -> Option<TileOutput> {
        self.outputs
            .lock()
            .ok()?
            .get(channel * self.tiles_len() + tile as usize)?
            .clone()
    }

    /// Publish one immutable final tile outside the progress lock. The tile's
    /// dirty flag is set only after the output exists, so a version bump can
    /// never announce pixels the tile endpoint cannot serve.
    fn publish_output(&self, channel: usize, tile: u32, output: TileOutput) {
        let Values::F32(values) = &output.planes().value else {
            return;
        };
        let valid = values.iter().filter(|value| value.is_finite()).count();
        if let Ok(mut outputs) = self.outputs.lock() {
            outputs[channel * self.tiles_len() + tile as usize] = Some(output);
        }
        self.lock().channels[channel].valid += valid;
    }

    /// Values a stretch is taken from, every `step` samples. Delta stretches
    /// its absolute difference; the difference already lives in channel 0.
    fn stretch_values(&self, slot: usize, step: usize) -> Vec<f32> {
        let Ok(outputs) = self.outputs.lock() else {
            return Vec::new();
        };
        let channel = if self.recipe.display == Display::Delta { 0 } else { slot };
        let delta = self.recipe.display == Display::Delta;
        let tiles = self.tiles_len();
        let pixels = self.grid.width as usize * self.grid.height as usize;
        let mut values = Vec::with_capacity(tiles + pixels / step);
        for tile in 0..tiles {
            let Some(output) = outputs.get(channel * tiles + tile).and_then(Option::as_ref) else {
                continue;
            };
            let Values::F32(plane) = &output.planes().value else {
                continue;
            };
            for value in plane.iter().filter(|value| value.is_finite()) {
                if values.len() % step == 0 {
                    values.push(if delta { value.abs() } else { *value });
                }
            }
        }
        values
    }

    /// Tiles drawn with an unlocked stretch would not match their neighbours, so
    /// locking one redraws them all, once.
    fn refresh_stretch(&self, now: Instant) {
        let pixels = self.grid.width as usize * self.grid.height as usize;
        {
            let mut progress = self.lock();
            if progress.estimated.is_some_and(|at| now.duration_since(at) < DIRTY_EVERY) {
                return;
            }
            progress.estimated = Some(now);
        }
        let step = (pixels / ESTIMATE_SAMPLES).max(1) | 1;
        let mut locked_any = false;
        for slot in 0..self.stretches() {
            if self.lock().channels[slot].locked {
                continue;
            }
            if let Some(stretch) = stretch_of(self.recipe.display, self.stretch_values(slot, step)) {
                self.lock().channels[slot].stretch = Some(stretch);
            }
            if self.lock().channels[slot].valid as f64 >= pixels as f64 * LOCK_FRACTION {
                self.lock().channels[slot].locked = true;
                locked_any = true;
            }
        }
        if locked_any {
            self.lock()
                .tiles
                .iter_mut()
                .filter(|tile| tile.version > 0)
                .for_each(|tile| tile.dirty = true);
        }
    }

    /// Publish test values as charged immutable tiles, replacing any previous
    /// output. Only test builds fabricate outputs outside a math session.
    #[cfg(test)]
    fn publish_test_values(&self, channel: usize, value: impl Fn(u32, u32) -> f32) {
        let tiles = self.tiles_len();
        let mut valid = 0;
        let mut outputs = self.outputs.lock().expect("mosaic outputs");
        for index in 0..tiles as u32 {
            let tile = self.tile_of(index);
            let n = (tile.shape.width * tile.shape.height) as usize;
            let mut plane = Vec::with_capacity(n);
            for row in 0..tile.shape.height {
                for col in 0..tile.shape.width {
                    plane.push(value(tile.x + col, tile.y + row));
                }
            }
            valid += plane.iter().filter(|value| value.is_finite()).count();
            let lease = spearv_core::math::MathLease::reserve(&self.budget, n as u64 * 4)
                .expect("test output lease");
            outputs[channel * tiles + index as usize] = Some(
                TileOutput::from_library(
                    tile,
                    Product::Values,
                    spearv_core::math::OutputPlanes {
                        value: Values::F32(plane),
                        variance: None,
                        counts: None,
                    },
                    lease,
                )
                .expect("test output"),
            );
        }
        drop(outputs);
        self.lock().channels[channel].valid = valid;
    }

    #[cfg(test)]
    fn published_value(&self, channel: usize, x: u32, y: u32) -> Option<f32> {
        let outputs = self.outputs.lock().ok()?;
        Self::value_from(&outputs, self.cols, self.grid.height, channel, self.tiles_len(), x, y)
    }

    /// Record the bytes of one read, then settle its counters. Returns the
    /// tiles that settled and this channel's newly finished tiles.
    fn settle(&self, channel: usize, bytes: u64, tiles: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let mut progress = self.lock();
        progress.bytes_read += bytes;
        progress.settle(channel, tiles)
    }

    fn add_bytes(&self, bytes: u64) {
        self.lock().bytes_read += bytes;
    }

    fn mark_settled(&self, tiles: &[u32]) {
        let mut progress = self.lock();
        for tile in tiles {
            progress.tiles[*tile as usize].dirty = true;
        }
    }

    /// A taken session that fails cannot be put back safely (its state may be
    /// partial), so every math fault except cancellation fails the whole job
    /// rather than silently restarting a tile.
    fn fail_fatal(&self, error: &MathError) {
        if matches!(error, MathError::Cancelled | MathError::Stale) {
            return;
        }
        if let Ok(mut fatal) = self.fatal.lock() {
            fatal.get_or_insert_with(|| error.to_string());
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.control.cancel();
    }

    /// A fault before this read settled. The read still counts as failed so
    /// the box path does not stall. A malformed/missing-ZODI native input is
    /// decided before any session is taken and stays a read failure.
    fn fail_math(&self, error: MathError, channel: usize, tiles: &[u32]) {
        self.fail_fatal(&error);
        self.fail_read(channel, tiles);
    }

    fn cancel_or_fail(&self) {
        let fatal = self.fatal.lock().ok().and_then(|mut fatal| fatal.take());
        match fatal {
            Some(error) => self.end(MosaicState::Failed, Some(error)),
            None => self.end(MosaicState::Cancelled, None),
        }
    }

    fn session_index(&self, channel: usize, tile: u32) -> usize {
        channel * self.tiles_len() + tile as usize
    }

    /// Finish one channel's tile session and publish its physical values.
    /// `finish` consumes the session, so its running sums drop as soon as the
    /// snapshot lease has been taken.
    async fn finalize_channel(&self, channel: usize, tile: u32) -> Result<(), MathError> {
        let mut guard = self.sessions[self.session_index(channel, tile)].lock().await;
        let Some(session) = guard.take() else {
            return Ok(());
        };
        let output = spawn_math(session, |session| session.finish())?.wait().await?;
        drop(guard);
        self.publish_output(channel, tile, output);
        Ok(())
    }

    /// A delta needs both sides of one tile. The plus session stays alive for
    /// `difference`; the minus session finishes first and releases its sums.
    async fn finalize_delta(&self, tile: u32) -> Result<(), MathError> {
        let mut plus_guard = self.sessions[self.session_index(0, tile)].lock().await;
        let mut minus_guard = self.sessions[self.session_index(1, tile)].lock().await;
        let Some(plus) = plus_guard.take() else {
            minus_guard.take();
            return Ok(());
        };
        let Some(minus) = minus_guard.take() else {
            return Ok(());
        };
        drop(minus_guard);
        let minus_output = spawn_math(minus, |session| session.finish())?.wait().await?;
        let (plus, plus_output) = spawn_math(plus, |mut session| {
            let output = session.snapshot()?;
            Ok((session, output))
        })?
        .wait()
        .await?;
        let difference = spawn_math(plus, move |mut session| {
            session.difference(&plus_output, &minus_output)
        })?
        .wait()
        .await?;
        drop(plus_guard);
        self.publish_output(0, tile, difference);
        Ok(())
    }

    /// Count one planned read. Must run before the read itself, so a read
    /// that lands during planning still settles against real counters.
    fn register(&self, plan: &Planned) {
        let mut progress = self.lock();
        progress.registered += 1;
        progress.channel_total[plan.channel] += 1;
        for tile in &plan.tiles {
            let at = *tile as usize;
            progress.tiles[at].pending += 1;
            progress.tiles[at].planned = true;
            progress.channel_pending[plan.channel][at] += 1;
        }
    }

    fn begin_reading(&self, planned: &[Planned], plan_failures: usize, bytes: u64, bytes_planned: u64) {
        self.prepare();
        for plan in planned {
            self.register(plan);
        }
        let mut progress = self.lock();
        progress.exposures_total = progress.registered + plan_failures;
        progress.exposures_done += plan_failures;
        progress.reads_failed = plan_failures;
        progress.bytes_read += bytes;
        progress.bytes_planned = bytes_planned;
        progress.state = MosaicState::Reading;
    }

    fn mark_active(&self, tiles: &[u32]) {
        let mut progress = self.lock();
        for tile in tiles {
            progress.tiles[*tile as usize].active += 1;
        }
    }

    /// Settle counters without producing pixels. Only tests that check boxes
    /// and frontiers use it; the read path settles through [`Self::settle`] and
    /// finalizes its sessions.
    #[cfg(test)]
    fn settle_read(&self, channel: usize, tiles: &[u32]) {
        let _ = self.settle(channel, 0, tiles);
    }

    fn fail_read(&self, channel: usize, tiles: &[u32]) {
        let mut progress = self.lock();
        progress.reads_failed += 1;
        progress.settle(channel, tiles);
    }

    fn end(&self, state: MosaicState, error: Option<String>) {
        self.control.cancel();
        for slot in &self.sessions {
            if let Ok(mut session) = slot.try_lock() {
                session.take();
            }
        }
        let mut progress = self.lock();
        progress.state = state;
        progress.error = error;
        progress.finished = Some(self.started.elapsed());
    }

    /// The whole grid decides the stretch, then every tile is redrawn once.
    fn finish(&self) {
        // No read remains; any session that never settled (a panicked task)
        // must not keep its running sums charged on a finished job.
        for slot in &self.sessions {
            if let Ok(mut session) = slot.try_lock() {
                session.take();
            }
        }
        for index in 0..self.stretches() {
            let values = self.stretch_values(index, 1);
            let stretch = stretch_of(self.recipe.display, values);
            self.lock().channels[index].stretch = stretch;
        }
        let mut progress = self.lock();
        progress.seq += 1;
        let seq = progress.seq;
        for tile in &mut progress.tiles {
            tile.version += 1;
            tile.seq = seq;
            tile.dirty = false;
        }
        if self.recipe.display != Display::Delta {
            progress.raw_metadata = Some(self.freeze_raw_metadata(&progress));
        }
        progress.state = MosaicState::Done;
        progress.finished = Some(self.started.elapsed());
        eprintln!(
            "mosaic {}: done in {} ms, {} ms of it merging",
            self.id,
            self.started.elapsed().as_millis(),
            self.merge_nanos.load(Ordering::Relaxed) / 1_000_000,
        );
    }

    fn status(&self, since: u64) -> MosaicStatus {
        let now = Instant::now();
        if let Ok(mut last_poll) = self.last_poll.lock() {
            *last_poll = now;
        }
        if let Ok(mut last_activity) = self.last_activity.lock() {
            *last_activity = now;
        }
        let mut progress = self.lock();
        progress.publish(now);
        let place = |index: usize| (index as u32 % self.cols, index as u32 / self.cols);
        let tiles = progress
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, tile)| tile.seq > since)
            .map(|(index, tile)| {
                let (x, y) = place(index);
                MosaicTile {
                    x,
                    y,
                    version: tile.version,
                    settled: tile.pending == 0,
                }
            })
            .collect();
        let reading = progress.state == MosaicState::Reading;
        let active = progress
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, tile)| reading && tile.active > 0)
            .map(|(index, _)| {
                let (x, y) = place(index);
                ActiveTile { x, y }
            })
            .collect();
        let path = snake_order(self.cols, self.rows);
        let mut order = Vec::new();
        let mut frontiers = vec![0; progress.channels.len()];
        if progress.state != MosaicState::Planning {
            order = path
                .iter()
                .map(|index| {
                    let (x, y) = place(*index as usize);
                    ActiveTile { x, y }
                })
                .collect();
            // A tile no read of this channel can reach stays at zero pending,
            // so empty sky never blocks a box. Neither does a failed read.
            for (channel, frontier) in frontiers.iter_mut().enumerate() {
                *frontier = path
                    .iter()
                    .take_while(|tile| progress.channel_pending[channel].get(**tile as usize).copied().unwrap_or(0) == 0)
                    .count();
            }
        }
        MosaicStatus {
            state: progress.state,
            seq: progress.seq,
            exposures_done: progress.exposures_done,
            exposures_total: progress.exposures_total,
            reads_failed: progress.reads_failed,
            bytes_read: progress.bytes_read,
            elapsed_ms: progress.finished.unwrap_or_else(|| self.started.elapsed()).as_millis() as u64,
            tiles_done: progress.tiles.iter().filter(|tile| tile.planned && tile.pending == 0).count(),
            tiles_total: progress.tiles.iter().filter(|tile| tile.planned).count(),
            channels_done: progress.channel_done.clone(),
            channels_total: progress.channel_total.clone(),
            bytes_planned: progress.bytes_planned,
            frontiers,
            active,
            order,
            tiles,
            error: progress.error.clone(),
        }
    }

    fn freeze_raw_metadata(&self, progress: &Progress) -> RawMetadata {
        let channels = self
            .recipe
            .channels
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let bands: Vec<usize> = if spec.bands.is_empty() {
                    (0..BANDS.len()).collect()
                } else {
                    spec.bands.iter().map(|band| *band as usize).collect()
                };
                let finite_count = progress.channels[index].valid;
                let stretch = (finite_count > 0).then_some(progress.channels[index].stretch).flatten();
                let metadata = &self.recipe.metadata[index];
                RawChannelMetadata {
                    index,
                    role: metadata.role.into(),
                    center_band: metadata.center_band,
                    width: metadata.width,
                    bands: bands.iter().map(|band| *band as u16 + 1).collect(),
                    wavelength_min_um: bands.iter().map(|band| BANDS[*band].wl_min_um).fold(f64::MAX, f64::min),
                    wavelength_max_um: bands.iter().map(|band| BANDS[*band].wl_max_um).fold(f64::MIN, f64::max),
                    finite_count,
                    default_black: stretch.map(|stretch| stretch.low),
                    default_white: stretch.map(|stretch| stretch.low + stretch.span),
                    default_span: stretch.map(|stretch| stretch.span),
                    // Color scale codes cover the server's effective display
                    // range, so Auto keeps the exact thresholds the server used.
                    scale_offset: (self.recipe.display == Display::Color).then_some(stretch).flatten().map(|stretch| stretch.low),
                    scale_span: (self.recipe.display == Display::Color).then_some(stretch).flatten().map(|stretch| stretch.span),
                }
            })
            .collect();
        RawMetadata {
            format_version: if self.recipe.display == Display::Color { 2 } else { 1 },
            job_id: self.id.clone(),
            state: MosaicState::Done,
            width: self.grid.width,
            height: self.grid.height,
            tile_size: TILE,
            dtype: if self.recipe.display == Display::Color { "u16le" } else { "f32le" }.into(),
            orientation: "png-top-row-first".into(),
            nodata: if self.recipe.display == Display::Color { "65535" } else { "nan" }.into(),
            source_recipe: self.recipe.source.clone(),
            channels,
        }
    }

    fn touch_activity(&self) {
        if let Ok(mut last_activity) = self.last_activity.lock() {
            *last_activity = Instant::now();
        }
    }

    fn raw_metadata(&self) -> Result<RawMetadata, StatusCode> {
        self.touch_activity();
        let progress = self.lock();
        if self.recipe.display == Display::Delta {
            return Err(StatusCode::NOT_FOUND);
        }
        raw_state(progress.state)?;
        progress.raw_metadata.clone().ok_or(StatusCode::CONFLICT)
    }

    /// Output dimensions of one PNG tile. Tiles at the right and bottom edges
    /// are cut to the parent grid.
    fn tile_shape(&self, x: u32, y: u32) -> (u32, u32) {
        (
            TILE.min(self.grid.width - x * TILE),
            TILE.min(self.grid.height - y * TILE),
        )
    }

    /// One final physical channel, in PNG row order. Missing or non-finite
    /// samples are the canonical quiet NaN, never an infinity.
    fn tile_f32(&self, channel: usize, x: u32, y: u32) -> Result<Option<Vec<u8>>, StatusCode> {
        self.touch_activity();
        if matches!(self.recipe.display, Display::Color | Display::Delta)
            || channel >= self.recipe.channels.len()
            || x >= self.cols
            || y >= self.rows
        {
            return Ok(None);
        }
        raw_state(self.lock().state)?;
        let (width, height) = self.tile_shape(x, y);
        let output = self.output(channel, y * self.cols + x);
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let local_y = height - 1 - row;
            for col in 0..width {
                let bits = output
                    .as_ref()
                    .and_then(|output| tile_value(output, col, local_y))
                    .map_or(0x7fc0_0000, f32::to_bits);
                bytes.extend_from_slice(&bits.to_le_bytes());
            }
        }
        Ok(Some(bytes))
    }

    /// One final colour layer as normalized little-endian scale positions.
    /// `65535` is no-data; 0..=65534 maps linearly through the channel's
    /// `scale_offset` and `scale_span` metadata, which are the server's
    /// effective display range.
    fn tile_scale16(&self, channel: usize, x: u32, y: u32) -> Result<Option<Vec<u8>>, StatusCode> {
        self.touch_activity();
        if self.recipe.display != Display::Color
            || channel >= self.recipe.channels.len()
            || x >= self.cols
            || y >= self.rows
        {
            return Ok(None);
        }
        let (width, height) = self.tile_shape(x, y);
        let encoded_range = {
            let progress = self.lock();
            raw_state(progress.state)?;
            progress
                .raw_metadata
                .as_ref()
                .and_then(|metadata| metadata.channels.get(channel))
                .and_then(|metadata| metadata.scale_offset.zip(metadata.scale_span))
        };
        let Some((offset, span)) = encoded_range else {
            return Ok(Some(vec![0xff; (width * height * 2) as usize]));
        };
        let output = self.output(channel, y * self.cols + x);
        let mut bytes = Vec::with_capacity((width * height * 2) as usize);
        for row in 0..height {
            let local_y = height - 1 - row;
            for col in 0..width {
                let encoded = output
                    .as_ref()
                    .and_then(|output| tile_value(output, col, local_y))
                    .map(|value| (((value - offset) / span).clamp(0.0, 1.0) * f32::from(SCALE_MAX)).round() as u16)
                    .unwrap_or(u16::MAX);
                bytes.extend_from_slice(&encoded.to_le_bytes());
            }
        }
        Ok(Some(bytes))
    }

    /// One channel's share of a tile, with the same locked stretch the RGB
    /// tile uses. Channel order is blue, green, red, so the layer paints its
    /// own gun only: red paints `[shade, 0, 0]`. A pixel this channel has no
    /// data for stays black, which adds nothing under `lighter`, so the three
    /// layers add up to the RGB tile. Grey returns its one full layer; a delta
    /// cannot be layered honestly, so it returns the whole tile.
    fn tile_layer(&self, channel: usize, x: u32, y: u32) -> Option<(u32, u32, Vec<u8>)> {
        if channel >= self.recipe.channels.len() || x >= self.cols || y >= self.rows {
            return None;
        }
        if self.recipe.display != Display::Color {
            return self.tile_rgb(x, y);
        }
        let (width, height) = self.tile_shape(x, y);
        let stretch = self.lock().channels[channel].stretch;
        let output = self.output(channel, y * self.cols + x);
        let gun = 2usize.saturating_sub(channel);
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for row in 0..height {
            let local_y = height - 1 - row;
            for col in 0..width {
                let mut pixel = [0u8; 3];
                if let (Some(value), Some(stretch)) = (
                    output.as_ref().and_then(|output| tile_value(output, col, local_y)),
                    stretch,
                ) {
                    pixel[gun] = shade(Some(value), Some(stretch));
                }
                rgb.extend(pixel);
            }
        }
        Some((width, height, rgb))
    }

    /// RGB for one tile, north up with RA increasing to the left. `y` counts
    /// from the top of the picture; the grid's Y counts from the bottom.
    fn tile_rgb(&self, x: u32, y: u32) -> Option<(u32, u32, Vec<u8>)> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        let (width, height) = self.tile_shape(x, y);
        let (display, stretches) = {
            let progress = self.lock();
            (
                progress.display,
                [0, 1, 2].map(|channel| progress.channels.get(channel).and_then(|channel| channel.stretch)),
            )
        };
        let outputs = self.outputs.lock().expect("mosaic outputs");
        let tiles = self.tiles_len();
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for row in 0..height {
            let grid_y = self.grid.height - 1 - (y * TILE + row);
            for col in 0..width {
                let grid_x = x * TILE + col;
                let value = |channel: usize| {
                    Self::value_from(&outputs, self.cols, self.grid.height, channel, tiles, grid_x, grid_y)
                };
                match display {
                    Display::Color => {
                        rgb.extend(color_pixel(value(0), value(1), value(2), stretches));
                    }
                    Display::Grey => match (value(0), stretches[0]) {
                        (Some(sample), Some(stretch)) => {
                            rgb.extend([grey_level(sample, stretch.low, stretch.span); 3]);
                        }
                        _ => rgb.extend(NODATA),
                    },
                    Display::Delta => match (value(0), stretches[0]) {
                        (Some(sample), Some(stretch)) => rgb.extend(delta_rgb(sample, stretch.span)),
                        _ => rgb.extend(NODATA),
                    },
                }
            }
        }
        Some((width, height, rgb))
    }
}

/// One finite value of a published tile plane, in tile-local FITS rows.
fn tile_value(output: &TileOutput, x: u32, y: u32) -> Option<f32> {
    let shape = output.tile().shape;
    if x >= shape.width || y >= shape.height {
        return None;
    }
    let Values::F32(values) = &output.planes().value else {
        return None;
    };
    values
        .get(y as usize * shape.width as usize + x as usize)
        .copied()
        .filter(|value| value.is_finite())
}

/// The FITS-order output tile under a PNG tile coordinate. Shared with the
/// one-shot render routes so both paths tile identically.
pub(super) fn tile_of_grid(x: u32, png_y: u32, width: u32, height: u32) -> Tile {
    // PNG rows run down from the top; the grid's FITS rows run up. The short
    // tile, when the height is not a multiple of 256, is the bottom row of the
    // picture, so it sits at FITS y = 0.
    let tile_height = TILE.min(height - png_y * TILE);
    let fits_y = height - png_y * TILE - tile_height;
    Tile {
        x: x * TILE,
        y: fits_y,
        shape: Shape {
            width: TILE.min(width - x * TILE),
            height: tile_height,
        },
        order: RasterOrder::Fits,
    }
}

/// One planned read.
struct Planned {
    exposure: Exposure,
    channel: usize,
    window: PixelWindow,
    /// Row-major indices of the tiles this read can touch.
    tiles: Vec<u32>,
}

/// Detector window and tiles for one exposure's band strip, or `None` when the
/// strip misses the grid.
///
/// Detector points are projected forward, because the inverse SIP is unreliable
/// far off the detector. Any pixel is within `LATTICE` of a lattice point, so a
/// reach of 1.5 lattice steps cannot lose a pixel that lands on the grid. A tile
/// is far wider than that reach, so the four reach corners find every tile.
///
/// The band rows carry a margin, so they can reach the grid when the band itself
/// does not. Such a file adds no pixels and is dropped. A lattice point counts if
/// the band is anywhere in the square around it, which holds every nearby pixel.
fn footprint(
    geometry: &ExposureGeometry,
    span: Option<(f64, f64)>,
    grid: &TanGrid,
    cols: u32,
) -> Option<(PixelWindow, Vec<u32>)> {
    let wcs = &geometry.wcs;
    let rows = match span {
        Some((low, high)) => geometry.wavelength.rows_for_wavelength(low, high, wcs.naxis)?,
        None => (1, wcs.naxis[1]),
    };
    let stops = |first: i32, last: i32| (first..=last).step_by(LATTICE as usize).chain([last]);
    let width = f64::from(grid.width);
    let height = f64::from(grid.height);
    let reach = f64::from(LATTICE) * 1.5;
    let on_grid = |value: f64, extent: f64| value >= 0.5 - reach && value <= extent + 0.5 + reach;
    let tile_of = |value: f64, extent: f64| ((value - 0.5).clamp(0.0, extent - 1.0) as u32) / TILE;
    let step = f64::from(LATTICE);
    let band_near = |x: f64, y: f64| {
        [(-step, -step), (step, -step), (-step, step), (step, step), (0.0, 0.0)]
            .into_iter()
            .filter_map(|(dx, dy)| geometry.wavelength.sample(x + dx, y + dy))
            .fold((f64::MAX, f64::MIN), |(low, high), (wave, _)| (low.min(wave), high.max(wave)))
    };
    let mut holds_band = span.is_none();
    let mut bounds: Option<(i32, i32, i32, i32)> = None;
    let mut tiles = BTreeSet::new();
    for y in stops(rows.0, rows.1) {
        for x in stops(1, wcs.naxis[0]) {
            let (ra, dec) = wcs.pixel_to_sky(f64::from(x), f64::from(y));
            let (out_x, out_y) = grid.pixel(ra, dec);
            if !on_grid(out_x, width) || !on_grid(out_y, height) {
                continue;
            }
            if let Some(span) = span.filter(|_| !holds_band) {
                let (low, high) = band_near(f64::from(x), f64::from(y));
                holds_band = low <= span.1 && high >= span.0;
            }
            let (x0, y0, x1, y1) = bounds.unwrap_or((x, y, x, y));
            bounds = Some((x0.min(x), y0.min(y), x1.max(x), y1.max(y)));
            for (dx, dy) in [(-reach, -reach), (reach, -reach), (-reach, reach), (reach, reach)] {
                // PNG rows run down from the top of the grid.
                tiles.insert(tile_of(height + 1.0 - (out_y + dy), height) * cols + tile_of(out_x + dx, width));
            }
        }
    }
    if !holds_band {
        return None;
    }
    let (x0, y0, x1, y1) = bounds?;
    let (x0, x1) = ((x0 - LATTICE).max(1), (x1 + LATTICE).min(wcs.naxis[0]));
    let (y0, y1) = ((y0 - LATTICE).max(rows.0), (y1 + LATTICE).min(rows.1));
    let window = PixelWindow {
        x0: x0 - 1,
        y0: y0 - 1,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
        clipped: false,
    };
    Some((window, tiles.into_iter().collect()))
}

/// Row-major tile indices in snake order: left to right along the top, down
/// the right side, right to left along the bottom, up the left, spiralling
/// inward so the centre comes last.
fn snake_order(cols: u32, rows: u32) -> Vec<u32> {
    let mut order = Vec::with_capacity((cols * rows) as usize);
    let (mut left, mut right, mut top, mut bottom) = (0i64, cols as i64 - 1, 0i64, rows as i64 - 1);
    while left <= right && top <= bottom {
        for x in left..=right {
            order.push((top * cols as i64 + x) as u32);
        }
        top += 1;
        if top > bottom {
            break;
        }
        for y in top..=bottom {
            order.push((y * cols as i64 + right) as u32);
        }
        right -= 1;
        if left > right {
            break;
        }
        for x in (left..=right).rev() {
            order.push((bottom * cols as i64 + x) as u32);
        }
        bottom -= 1;
        if top > bottom {
            break;
        }
        for y in (top..=bottom).rev() {
            order.push((y * cols as i64 + left) as u32);
        }
        left += 1;
    }
    order
}
/// Reads interleaved across channels: each channel takes its reads tile by tile
/// along the snake, then the channels deal round-robin, so no channel starves the
/// others. Every file is still read exactly once. A tile finishes when its last
/// file lands, so the box path is the order the picture is finished in.
fn snake_plan(planned: Vec<Planned>, cols: u32, rows: u32) -> Vec<Planned> {
    use std::collections::BTreeMap;
    let mut by_channel: BTreeMap<usize, Vec<Planned>> = BTreeMap::new();
    for plan in planned {
        by_channel.entry(plan.channel).or_default().push(plan);
    }
    let mut queues: Vec<std::collections::VecDeque<Planned>> = by_channel
        .into_values()
        .map(|plans| snake_walk(plans, cols, rows).into())
        .collect();
    let mut ordered = Vec::new();
    while queues.iter().any(|queue| !queue.is_empty()) {
        for queue in &mut queues {
            if let Some(plan) = queue.pop_front() {
                ordered.push(plan);
            }
        }
    }
    ordered
}

/// Walk the snake and queue each tile's not-yet-queued exposures.
fn snake_walk(mut planned: Vec<Planned>, cols: u32, rows: u32) -> Vec<Planned> {
    let mut ordered = Vec::with_capacity(planned.len());
    for tile in snake_order(cols, rows) {
        if planned.is_empty() {
            break;
        }
        let (now, later): (Vec<Planned>, Vec<Planned>) =
            planned.into_iter().partition(|plan| plan.tiles.contains(&tile));
        ordered.extend(now);
        planned = later;
    }
    ordered
}

/// Run `tasks` in order, `width` at a time. False when the job was abandoned;
/// tasks still in flight are aborted, which ends their reads.
async fn pump<T, F>(job: &Job, tasks: impl Iterator<Item = F>, width: usize, mut done: impl FnMut(T)) -> bool
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let mut tasks = tasks;
    let mut running = JoinSet::new();
    loop {
        if job.abandoned() {
            running.abort_all();
            return false;
        }
        while running.len() < width {
            let Some(task) = tasks.next() else { break };
            running.spawn(task);
        }
        if running.is_empty() {
            return true;
        }
        tokio::select! {
            joined = running.join_next() => {
                if let Some(Ok(value)) = joined {
                    done(value);
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn run(
    job: Arc<Job>,
    source: Arc<dyn ImageSource>,
    wanted: Vec<(Exposure, usize)>,
    budget: spearv_core::WorkPermit,
) {
    // Held for the whole job; dropping it at any return path returns the
    // memory to the process budget.
    let _budget = budget;
    let cols = job.cols;
    let specs = job.recipe.channels.clone();
    let plans = wanted.into_iter().map(|(exposure, channel)| {
        let source = Arc::clone(&source);
        let grid = job.grid.clone();
        let span = specs[channel].span;
        async move {
            let geometry = source.geometry(&exposure).await?;
            let plan = footprint(&geometry, span, &grid, cols).map(|(window, tiles)| Planned {
                exposure,
                channel,
                window,
                tiles,
            });
            // Whole rows of image, variance, zodi and flags, four bytes a pixel each.
            let estimate = plan.as_ref().map_or(0, |plan| {
                u64::from(plan.window.height.unsigned_abs()) * geometry.wcs.naxis[0] as u64 * 16
            });
            Ok::<_, Error>((plan, geometry.header_bytes, estimate))
        }
    });
    job.prepare();
    let mut planned = Vec::new();
    let mut plan_failures = 0;
    let mut header_bytes = 0;
    let mut bytes_planned = 0;
    let kept = pump(&job, plans, PLANS, |result| match result {
        Ok((plan, bytes, estimate)) => {
            header_bytes += bytes;
            bytes_planned += estimate;
            planned.extend(plan);
        }
        Err(err) => {
            eprintln!("mosaic {}: header read failed: {err}", job.id);
            plan_failures += 1;
        }
    })
    .await;
    if !kept {
        return job.cancel_or_fail();
    }
    if planned.is_empty() {
        let error = if plan_failures > 0 {
            "every exposure header read failed"
        } else {
            "no band strip of these exposures crosses the field"
        };
        return job.end(MosaicState::Failed, Some(error.into()));
    }
    if job.recipe.display == Display::Delta && (0..2).any(|side| !planned.iter().any(|plan| plan.channel == side)) {
        return job.end(MosaicState::Failed, Some(NO_DELTA_EXPOSURE.into()));
    }
    let planned = snake_plan(planned, job.cols, job.rows);
    job.begin_reading(&planned, plan_failures, header_bytes, bytes_planned);
    let reads = planned.into_iter().map(|plan| {
        let job = Arc::clone(&job);
        let source = Arc::clone(&source);
        let spec = specs[plan.channel].clone();
        read_one(job, source, plan, spec)
    });
    if !pump(&job, reads, job.width, |()| {}).await {
        return job.cancel_or_fail();
    }
    if job.fatal.lock().ok().is_some_and(|fatal| fatal.is_some()) {
        return job.cancel_or_fail();
    }
    let unread = {
        let mut progress = job.lock();
        // A task that panicked never reported. It still left a hole.
        let lost = progress.exposures_total - progress.exposures_done;
        progress.reads_failed += lost;
        progress.exposures_done = progress.exposures_total;
        progress.reads_failed == progress.exposures_total
    };
    if unread {
        return job.end(MosaicState::Failed, Some("every exposure read failed".into()));
    }
    let finishing = Arc::clone(&job);
    if tokio::task::spawn_blocking(move || finishing.finish()).await.is_err() {
        job.end(MosaicState::Failed, Some("the final stretch did not finish".into()));
    }
}

/// Read one exposure's window and add it to every planned tile session. The
/// cutout is copied into a charged [`NativeInput`] and dropped; each bounded
/// library call runs in a blocking math worker, never on the async thread.
async fn read_one(job: Arc<Job>, source: Arc<dyn ImageSource>, plan: Planned, spec: ChannelSpec) {
    let Ok(slot) = job.read_slots.acquire().await else { return };
    job.mark_active(&plan.tiles);
    let cutout = match source.window_cutout(&plan.exposure, &plan.window).await {
        Ok(cutout) => cutout,
        Err(err) => {
            eprintln!("mosaic {}: read failed for {}: {err}", job.id, plan.exposure.url);
            return job.fail_read(plan.channel, &plan.tiles);
        }
    };
    drop(slot);
    let Ok(_stacking) = job.stack_slots.acquire().await else { return };
    let bytes = cutout.bytes_read;
    let bits = cutout.flag_bits;
    let budget = Arc::clone(&job.budget);
    let copied = tokio::task::spawn_blocking(move || {
        NativeInput::from_cutout(&budget, MAX_NATIVE_PIXELS, &cutout, true)
    })
    .await;
    let input = match copied {
        Ok(Ok(input)) => input,
        // Missing ZODI or a malformed plane makes this exposure unusable, not
        // the whole job: no session has been touched yet.
        Ok(Err(MathError::Input(_))) => {
            job.add_bytes(bytes);
            return job.fail_read(plan.channel, &plan.tiles);
        }
        Ok(Err(error)) => {
            job.add_bytes(bytes);
            return job.fail_math(error, plan.channel, &plan.tiles);
        }
        Err(_) => {
            job.add_bytes(bytes);
            return job.fail_read(plan.channel, &plan.tiles);
        }
    };
    let merging = Instant::now();
    let mask = if !job.mask {
        MaskPolicy::None
    } else if spec.ignore_outlier {
        MaskPolicy::Delta
    } else {
        MaskPolicy::Composite
    };
    let target = job.target();
    for tile in &plan.tiles {
        let mut guard = job.sessions[job.session_index(plan.channel, *tile)].lock().await;
        if guard.is_none() {
            match job.math.session(job.control.clone(), job.tile_of(*tile), Product::Values) {
                Ok(session) => *guard = Some(session),
                Err(error) => {
                    drop(guard);
                    return job.fail_math(error, plan.channel, &plan.tiles);
                }
            }
        }
        let session = guard.take().expect("a session was just ensured");
        let input = input.clone();
        let bands = spec.bands.clone();
        let ordering = Arc::clone(&job);
        let spawned = spawn_math(session, move |mut session| {
            let geometry = session.geometry(&input.planes().wcs, input.planes().window, target)?;
            let order = ordering.order.fetch_add(1, Ordering::Relaxed);
            session.add(order, &input, &geometry, bits, mask, &bands)?;
            Ok(session)
        });
        let returned = match spawned {
            Ok(task) => task.wait().await,
            Err(error) => Err(error),
        };
        match returned {
            Ok(session) => *guard = Some(session),
            Err(error) => {
                drop(guard);
                return job.fail_math(error, plan.channel, &plan.tiles);
            }
        }
    }
    drop(input);
    let (settled, finished) = job.settle(plan.channel, bytes, &plan.tiles);
    if job.recipe.display == Display::Delta {
        for tile in &settled {
            if let Err(error) = job.finalize_delta(*tile).await {
                return job.fail_fatal(&error);
            }
        }
    } else {
        for tile in &finished {
            if let Err(error) = job.finalize_channel(plan.channel, *tile).await {
                return job.fail_fatal(&error);
            }
        }
    }
    job.mark_settled(&settled);
    job.refresh_stretch(Instant::now());
    job.merge_nanos
        .fetch_add(merging.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// What this view reads. Colour ignores the view's own mode and pins.
fn recipe(view: &View, color: bool, bands: ColorBands) -> Result<Recipe, Error> {
    let channel = |view: &View| {
        let bands = view.selected_bands();
        ChannelSpec {
            span: Some(band_span(&bands)),
            bands,
            ignore_outlier: false,
        }
    };
    let source = RawSourceRecipe {
        mode: if color {
            "color"
        } else if view.mode == Mode::Composite {
            "composite"
        } else {
            "range"
        }
        .into(),
        mask: view.mask,
        pins: if color || view.mode == Mode::Composite {
            Vec::new()
        } else if view.pins.is_empty() {
            vec![RawPin { center_band: 43, width: 1, visit: None, sign: None }]
        } else {
            view.pins
                .iter()
                .map(|pin| RawPin {
                    center_band: pin.center + 1,
                    width: if view.mode == Mode::Slider { 1 } else { pin.width },
                    visit: pin.visit,
                    sign: pin.sign.map(|sign| match sign {
                        spearv_core::Sign::Plus => "+".into(),
                        spearv_core::Sign::Minus => "-".into(),
                    }),
                })
                .collect()
        },
    };
    if color {
        let selected = bands.channels();
        return Ok(Recipe {
            display: Display::Color,
            channels: selected.iter().map(|band| channel(&color_view(view, *band))).collect(),
            source,
            metadata: ["blue", "green", "red"]
                .into_iter()
                .zip(selected)
                .map(|(role, band)| ChannelMetadataSpec {
                    role,
                    center_band: Some(band.center + 1),
                    width: band.width,
                })
                .collect(),
        });
    }
    match view.mode {
        Mode::Slider | Mode::Range => {
            let channel = channel(view);
            let center_band = match view.pins.as_slice() {
                [] => Some(43),
                [pin] => Some(pin.center + 1),
                _ => None,
            };
            let width = if view.pins.len() == 1 {
                if view.mode == Mode::Slider { 1 } else { view.pins[0].width }
            } else {
                channel.bands.len() as u16
            };
            Ok(Recipe {
                display: Display::Grey,
                channels: vec![channel],
                source,
                metadata: vec![ChannelMetadataSpec { role: "grey", center_band, width }],
            })
        }
        Mode::Composite => Ok(Recipe {
            display: Display::Grey,
            channels: vec![ChannelSpec {
                bands: Vec::new(),
                span: None,
                ignore_outlier: false,
            }],
            source,
            metadata: vec![ChannelMetadataSpec {
                role: "grey",
                center_band: None,
                width: BANDS.len() as u16,
            }],
        }),
        Mode::Delta => {
            let bands = delta_bands(view)?;
            let side = ChannelSpec {
                span: Some(band_span(&bands)),
                bands,
                ignore_outlier: true,
            };
            Ok(Recipe {
                display: Display::Delta,
                channels: vec![side.clone(), side],
                source,
                metadata: Vec::new(),
            })
        }
    }
}

/// The reads this recipe wants, each with its channel.
///
/// The index's own wavelength filter tests a few points of the view, which a
/// strip on a wide field can fall between, so this only asks whether the file
/// holds the band. The plan phase decides with the file's real wavelength map.
fn select(all: Vec<Exposure>, view: &View, color: bool, recipe: &Recipe) -> Result<Vec<(Exposure, usize)>, Error> {
    let spans = cluster_visits(&all.iter().map(|exposure| exposure.mjd).collect::<Vec<_>>());
    if recipe.display == Display::Delta {
        let span = recipe.channels[0].span.unwrap_or((0.0, 10.0));
        let holding = all.into_iter().filter(|exposure| holds(exposure, span)).collect();
        let (plus, minus) = delta_sides(holding, &spans, view)?;
        let tag = |side: usize| move |exposure| (exposure, side);
        return Ok(plus.into_iter().map(tag(0)).chain(minus.into_iter().map(tag(1))).collect());
    }
    // A visit on a pin picks one epoch, for slider and range only, as in `band_png`.
    let pinned = !color && matches!(view.mode, Mode::Slider | Mode::Range);
    let visit = view.pins.iter().find_map(|pin| pin.visit).filter(|_| pinned);
    Ok(all
        .into_iter()
        .filter(|exposure| visit.is_none() || visit_index(exposure.mjd, &spans) == visit)
        .filter_map(|exposure| {
            let channel = recipe.channels.iter().position(|spec| spec.span.is_none_or(|span| holds(&exposure, span)))?;
            Some((exposure, channel))
        })
        .collect())
}

fn holds(exposure: &Exposure, span: (f64, f64)) -> bool {
    exposure.em_max_um >= span.0 - 0.02 && exposure.em_min_um <= span.1 + 0.02
}

fn missing(id: &str) -> Response {
    let error = format!("no mosaic {id}; it was evicted or never existed");
    (StatusCode::NOT_FOUND, Json(ApiError { error })).into_response()
}

fn raw_state(state: MosaicState) -> Result<(), StatusCode> {
    match state {
        MosaicState::Done => Ok(()),
        MosaicState::Planning | MosaicState::Reading => Err(StatusCode::CONFLICT),
        MosaicState::Failed | MosaicState::Cancelled => Err(StatusCode::GONE),
    }
}

fn raw_error(status: StatusCode, id: &str) -> Response {
    let error = match status {
        StatusCode::CONFLICT => format!("mosaic {id} is not finished"),
        StatusCode::GONE => format!("mosaic {id} did not finish"),
        _ => format!("raw channels are not available for mosaic {id}"),
    };
    (status, Json(ApiError { error })).into_response()
}

/// Estimated transient heap of one mosaic job, bytes.
///
/// Persistent running sums and published tiles are charged individually by
/// their own `MathLease`s, so admitting them here as well would double-charge.
/// This permit covers what has no lease of its own: the read fan's cutouts, the
/// charged native copies made while a stack slot is held, each concurrent
/// library call's private input/scratch copy, and the final exact-stretch
/// sample vector (one channel at a time). Per-channel state is leased, so the
/// channel count is not part of this estimate.
fn job_bytes(width: u32, height: u32) -> u64 {
    let pixels = u64::from(width) * u64::from(height);
    let detector = pixels.min(MAX_NATIVE_PIXELS as u64);
    let reads = spearv_core::env::number("SPEARV_MOSAIC_READS", READS as u64).max(1);
    let workers = spearv_core::env::number("SPEARV_PYTHON_WORKERS", 4_u64).clamp(1, 32);
    let stacks = std::thread::available_parallelism()
        .map_or(4, |cores| cores.get() as u64)
        .min(workers)
        .max(1);
    let cutouts = (reads + stacks) * detector * 16;
    let natives = stacks * detector * 19;
    let tile = pixels.min(u64::from(TILE) * u64::from(TILE));
    let per_call = tile * 544 + detector * 19 + tile * 8 + 1024 * 1024;
    let scratch = workers * per_call;
    let stretch = pixels * 4;
    cutouts + natives + scratch + stretch
}

/// Take the job's memory from the process budget, or fail with a 429.
fn admit(ctx: &Context, width: u32, height: u32) -> Result<spearv_core::WorkPermit, Error> {
    let bytes = job_bytes(width, height);
    ctx.budget
        .try_acquire_bytes(bytes)
        .ok_or_else(|| ctx.budget.busy(bytes))
}

/// The identity a mosaic job is shared by. The colour mapping is part of it,
/// so two different band recipes never join one job or serve its tiles.
fn job_key(view: &View, color: bool, bands: ColorBands) -> String {
    if color {
        return format!(
            "color ra={:.6} dec={:.6} fov_l={:.6} fov_h={:.6} dr={} mask={} bands={}",
            view.region.ra,
            view.region.dec,
            view.region.fov_l_deg,
            view.region.fov_h_deg,
            view.release,
            view.mask,
            bands.key()
        );
    }
    view.cache_key()
}

/// Start a mosaic, or join the one already running or finished for this view.
#[utoipa::path(
    get,
    path = "/api/v1/mosaic",
    params(MosaicQuery),
    responses(
        (status = 200, body = MosaicInfo),
        (status = 400, description = "View parameters are invalid", body = ApiError),
        (status = 404, description = "No exposure covers this view", body = ApiError),
        (status = 429, description = "The process-wide work budget has no room for another mosaic; retry when a running job finishes", body = ApiError)
    )
)]
pub(super) async fn start(
    State(ctx): State<Arc<Context>>,
    Extension(jobs): Extension<Arc<Jobs>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<MosaicInfo>, Error> {
    let raw = raw.as_deref().unwrap_or("");
    let query = parse_query(raw)?;
    let view = view_of(&query)?;
    let color = query_pairs(raw).iter().any(|(key, value)| key == "color" && value == "1");
    let bands = color_bands_of(&query)?;
    let recipe = recipe(&view, color, bands)?;
    let region = &view.region;
    // A job made before the index last changed may lack new exposures. The
    // cache epoch moves when the binder clears the cache for a new index.
    let key = format!("{} epoch={}", job_key(&view, color, bands), ctx.cache.epoch());
    if let Some(job) = jobs.join(&key) {
        return Ok(Json(job.info()));
    }
    let source = source(&ctx)?;
    let all = source
        .coverage(
            region,
            &Filter {
                release: Some(ReleaseId(view.release.clone())),
                wavelength_um: None,
            },
        )
        .await?;
    let wanted = select(all, &view, color, &recipe)?;
    if wanted.is_empty() {
        return Err(Error::OutsideExposure);
    }
    let grid = TanGrid::new_rect(region.ra, region.dec, region.fov_l_deg, region.fov_h_deg);
    let math = ctx.math()?;
    let budget = admit(&ctx, grid.width, grid.height)?;
    let (job, fresh) = jobs.insert(key, grid, wanted.len(), (recipe, view.mask), math, Arc::clone(&ctx.budget));
    if fresh {
        tokio::spawn(run(Arc::clone(&job), source, wanted, budget));
    }
    Ok(Json(job.info()))
}

/// Progress, and the tiles that changed since `since`. Polling keeps the job
/// alive: one that nobody polls/touches for ABANDON_AFTER is cancelled.
#[utoipa::path(
    get,
    path = "/api/v1/mosaic/{id}/status",
    params(("id" = String, Path, description = "Mosaic id"), StatusQuery),
    responses(
        (status = 200, body = MosaicStatus),
        (status = 404, description = "No such mosaic", body = ApiError)
    )
)]
pub(super) async fn status(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path(id): Path<String>,
    Query(query): Query<StatusQuery>,
) -> Response {
    match jobs.find(&id) {
        Some(job) => Json(job.status(query.since.unwrap_or(0))).into_response(),
        None => missing(&id),
    }
}

/// Frozen final raw-channel description. It is unavailable until the mosaic
/// finishes and never changes afterward.
#[utoipa::path(
    get,
    path = "/api/v1/mosaic/{id}/raw-metadata",
    params(("id" = String, Path, description = "Mosaic id")),
    responses(
        (status = 200, body = RawMetadata),
        (status = 404, description = "No such mosaic", body = ApiError),
        (status = 409, description = "The mosaic is still running", body = ApiError),
        (status = 410, description = "The mosaic failed or was cancelled", body = ApiError)
    )
)]
pub(super) async fn raw_metadata(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path(id): Path<String>,
) -> Response {
    let Some(job) = jobs.find(&id) else {
        return missing(&id);
    };
    match job.raw_metadata() {
        Ok(metadata) => Json(metadata).into_response(),
        Err(status) => raw_error(status, &id),
    }
}

/// One tile as it stands now, or one channel's layer of it with `channel`.
/// A layer uses the same locked stretch as the full tile, so the colour
/// layers add up to it. An unknown channel is 404.
#[utoipa::path(
    get,
    path = "/api/v1/mosaic/{id}/tile",
    params(("id" = String, Path, description = "Mosaic id"), TileQuery),
    responses(
        (status = 200, description = "Tile PNG, a final f32le physical channel, or a normalized u16le colour layer",
            content(("image/png"), ("application/octet-stream"))),
        (status = 400, description = "Unknown tile format", body = ApiError),
        (status = 404, description = "No such mosaic, tile or channel", body = ApiError),
        (status = 409, description = "Raw tile requested while the mosaic is still running", body = ApiError),
        (status = 410, description = "Raw tile requested after the mosaic failed or was cancelled", body = ApiError)
    )
)]
pub(super) async fn tile(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path(id): Path<String>,
    Query(query): Query<TileQuery>,
) -> Response {
    if query.format.as_deref().is_some_and(|format| !matches!(format, "png" | "f32" | "scale16")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "format must be png, f32, or scale16 when provided".into() }),
        )
            .into_response();
    }
    let Some(job) = jobs.find(&id) else {
        return missing(&id);
    };
    if matches!(query.format.as_deref(), Some("f32" | "scale16")) {
        let Some(channel) = query.channel else {
            return missing(&id);
        };
        let bytes = if query.format.as_deref() == Some("scale16") {
            job.tile_scale16(channel, query.x, query.y)
        } else {
            job.tile_f32(channel, query.x, query.y)
        };
        return match bytes {
            Ok(Some(bytes)) => {
                let length = bytes.len();
                let mut response = bytes.into_response();
                let headers = response.headers_mut();
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
                headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&length.to_string()).expect("tile length"));
                headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
                response
            }
            Ok(None) => missing(&id),
            Err(status) => raw_error(status, &id),
        };
    }
    if job.recipe.display == Display::Color && query.channel.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "color mosaics are delivered as separate channel layers".into() }),
        ).into_response();
    }
    let channel = query.channel;
    let encoded = tokio::task::spawn_blocking(move || {
        let (width, height, rgb) = match channel {
            Some(channel) => job.tile_layer(channel, query.x, query.y)?,
            None => job.tile_rgb(query.x, query.y)?,
        };
        let mut png = Vec::new();
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
            .ok()?;
        Some(png)
    })
    .await;
    match encoded {
        Ok(Some(png)) => (
            [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")],
            png,
        )
            .into_response(),
        _ => missing(&id),
    }
}

#[cfg(test)]
mod tests;
