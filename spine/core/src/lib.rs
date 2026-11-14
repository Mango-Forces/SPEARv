mod budget;
pub mod env;
mod error;
mod grid;
mod wcs;

pub use budget::{WorkBudget, WorkPermit};
pub use error::{ApiError, Error, Result};
pub use grid::{grid_size, png_pixel_to_sky, png_pixel_to_sky_rect, sky_to_png_pixel, sky_to_png_pixel_rect, TanGrid, PIXEL_SCALE_DEG};
pub use wcs::{PixelWindow, Poly, Sip, Wcs};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use utoipa_axum::router::OpenApiRouter;

/// A rectangular patch of sky in ICRS degrees. FOV-L is horizontal and FOV-H vertical.
#[derive(Clone, Debug)]
pub struct SkyRegion {
    pub ra: f64,
    pub dec: f64,
    pub fov_l_deg: f64,
    pub fov_h_deg: f64,
}

impl SkyRegion {
    pub const fn square(ra: f64, dec: f64, fov_deg: f64) -> Self {
        Self { ra, dec, fov_l_deg: fov_deg, fov_h_deg: fov_deg }
    }

    pub const fn rect(ra: f64, dec: f64, fov_l_deg: f64, fov_h_deg: f64) -> Self {
        Self { ra, dec, fov_l_deg, fov_h_deg }
    }

    pub fn envelope_deg(&self) -> f64 {
        self.fov_l_deg.max(self.fov_h_deg)
    }

    pub fn is_square(&self) -> bool {
        self.fov_l_deg == self.fov_h_deg
    }
}

/// Names one image or catalog source. Each source plugin owns its identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub String);

impl SourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExposureId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseId(pub String);

#[derive(Clone, Debug)]
pub struct Release {
    pub id: ReleaseId,
    pub newest_mjd: Option<f64>,
    /// Newest planning period the watcher has seen for this release.
    pub newest_period: Option<String>,
    /// Unix epoch milliseconds when the bucket watcher last listed the release.
    pub last_seen_ms: Option<i64>,
}

/// Narrows a coverage query.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub release: Option<ReleaseId>,
}

/// One image of the sky at one time.
#[derive(Clone, Debug)]
pub struct Exposure {
    pub id: ExposureId,
    pub source: SourceId,
    pub detector: u8,
    pub mjd: f64,
    pub release: ReleaseId,
    /// Processing version, so a reprocessed file does not collide with an earlier one.
    pub version: String,
    pub obsid: String,
    /// Where the bytes live, in the public S3 bucket.
    pub url: String,
    /// Object ETag the index recorded. A source conditions its reads on it, so
    /// a replaced object fails the read instead of mixing two versions.
    pub etag: Option<String>,
    /// Object size in bytes, when the index recorded it.
    pub size: Option<u64>,
}

/// Row-major pixels. Index 0 is the lower-left corner in FITS order.
#[derive(Clone, Debug)]
pub struct Plane<T> {
    pub width: u32,
    pub height: u32,
    pub data: Vec<T>,
}

impl<T> Plane<T> {
    pub fn get(&self, x: usize, y: usize) -> &T {
        &self.data[y * self.width as usize + x]
    }
}

/// Pixels with everything needed to use them correctly.
///
/// `origin_fits` is the 1-based FITS coordinate of plane pixel (0, 0).
#[derive(Clone, Debug)]
pub struct Cutout {
    pub image: Plane<f32>,
    pub variance: Plane<f32>,
    pub flags: Plane<u32>,
    pub zodi: Option<Plane<f32>>,
    pub wcs: Wcs,
    pub origin_fits: (i32, i32),
    pub clipped: bool,
    pub bytes_read: u64,
    pub s3_requests: u64,
}

/// A catalog hit.
#[derive(Clone, Debug)]
pub struct CatalogRow {
    pub id: String,
    pub ra: f64,
    pub dec: f64,
    pub mag: Option<f64>,
}

/// What an exposure's header says about where its pixels fall, without its pixels.
#[derive(Clone, Debug)]
pub struct ExposureGeometry {
    pub wcs: Wcs,
    /// Bytes spent reading the header now. Zero when it came from the layout cache.
    pub header_bytes: u64,
}

#[async_trait]
pub trait ImageSource: Send + Sync {
    fn id(&self) -> SourceId;
    async fn releases(&self) -> Result<Vec<Release>>;
    async fn coverage(&self, region: &SkyRegion, filter: &Filter) -> Result<Vec<Exposure>>;
    async fn cutout(&self, exposure: &Exposure, region: &SkyRegion) -> Result<Cutout>;
    /// Reads are conditioned on [`Exposure::etag`] when it is set.
    async fn geometry(&self, exposure: &Exposure) -> Result<ExposureGeometry>;
    /// Read exactly this detector window, conditioned on [`Exposure::etag`] when it is set.
    async fn window_cutout(&self, exposure: &Exposure, window: &PixelWindow) -> Result<Cutout>;
}

#[async_trait]
pub trait CatalogSource: Send + Sync {
    fn id(&self) -> SourceId;
    /// `epoch_mjd` moves sources by proper motion when the catalog has it.
    async fn cone(&self, region: &SkyRegion, epoch_mjd: Option<f64>) -> Result<Vec<CatalogRow>>;
}

/// The exposure index the process reads coverage from.
#[async_trait]
pub trait IndexService: Send + Sync {
    /// The service's own health report.
    async fn health(&self) -> Result<serde_json::Value>;
    /// Changes whenever the indexed exposures change.
    async fn generation(&self) -> Result<String>;
}

/// Most entries a [`Cache`] holds before it evicts the oldest.
const CACHE_ENTRIES: usize = 32;

/// In-memory response cache keyed by a string the extension chooses. When it
/// is full, inserting a new key evicts the oldest one.
#[derive(Default)]
pub struct Cache {
    entries: Mutex<CacheEntries>,
    /// Moves on every `clear`, so caches outside this map can tell that what
    /// they hold predates the change.
    epoch: AtomicU64,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.entries.lock().ok()?.values.get(key).cloned()
    }

    /// Store `value` under `key`. Replacing a key keeps its place in the eviction order.
    pub fn insert(&self, key: String, value: Vec<u8>) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if let Some(stored) = entries.values.get_mut(&key) {
            *stored = value;
            return;
        }
        if entries.values.len() >= CACHE_ENTRIES {
            if let Some(oldest) = entries.order.pop_front() {
                entries.values.remove(&oldest);
            }
        }
        entries.order.push_back(key.clone());
        entries.values.insert(key, value);
    }

    /// Drop every entry, for example when the exposure index changes.
    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut entries) = self.entries.lock() {
            entries.values.clear();
            entries.order.clear();
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }
}

/// The cached values and their keys in insertion order, oldest first.
#[derive(Default)]
struct CacheEntries {
    values: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
}

/// What an extension can reach.
pub struct Context {
    pub images: HashMap<SourceId, Arc<dyn ImageSource>>,
    pub catalogs: HashMap<SourceId, Arc<dyn CatalogSource>>,
    pub cache: Arc<Cache>,
    /// One admission budget for every memory-heavy job in the process. See
    /// [`WorkBudget`].
    pub budget: Arc<WorkBudget>,
    index: Option<Arc<dyn IndexService>>,
}

impl Context {
    pub fn new(cache: Arc<Cache>) -> Self {
        Self {
            images: HashMap::new(),
            catalogs: HashMap::new(),
            cache,
            budget: Arc::new(WorkBudget::default()),
            index: None,
        }
    }

    /// Register an image source under its own id. A later source with the same id replaces it.
    pub fn register_image(&mut self, source: Arc<dyn ImageSource>) {
        self.images.insert(source.id(), source);
    }

    /// Register a catalog source under its own id. A later source with the same id replaces it.
    pub fn register_catalog(&mut self, source: Arc<dyn CatalogSource>) {
        self.catalogs.insert(source.id(), source);
    }

    /// Replace the default admission budget, for a deployment that sizes it
    /// differently or a test that exhausts it deliberately.
    pub fn with_budget(mut self, budget: Arc<WorkBudget>) -> Self {
        self.budget = budget;
        self
    }

    /// Install the process's one exposure index.
    pub fn with_index(mut self, index: Arc<dyn IndexService>) -> Self {
        self.index = Some(index);
        self
    }

    /// The installed exposure index, or `None` when the process runs without one.
    pub fn index(&self) -> Option<&Arc<dyn IndexService>> {
        self.index.as_ref()
    }
}

/// A plugin that mounts routes. Routes are declared without state; the binder
/// calls `with_state` once, because `OpenApiRouter::with_state` cannot be merged.
/// `routes` must not have side effects: background work belongs in `start`.
#[async_trait]
pub trait Extension: Send + Sync {
    fn id(&self) -> &'static str;
    fn routes(&self) -> OpenApiRouter<Arc<Context>>;
    /// Called once at startup, inside the runtime, before the server accepts requests.
    fn start(&self, _ctx: &Arc<Context>) {}
    /// Called once during graceful shutdown.
    async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_the_cache_empties_it_and_moves_the_epoch() {
        let cache = Cache::new();
        cache.insert("view".into(), vec![1, 2, 3]);
        let epoch = cache.epoch();
        cache.clear();
        assert!(cache.get("view").is_none());
        assert_eq!(cache.epoch(), epoch + 1);
    }

    #[test]
    fn a_full_cache_evicts_its_oldest_entry() {
        let cache = Cache::new();
        for i in 0..CACHE_ENTRIES {
            cache.insert(format!("view-{i}"), vec![i as u8]);
        }
        cache.insert("view-0".into(), vec![99]);
        cache.insert("newest".into(), vec![1]);
        assert!(cache.get("view-0").is_none(), "replacing a key does not make it newer");
        assert_eq!(cache.get("view-1"), Some(vec![1]));
        assert_eq!(cache.get("newest"), Some(vec![1]));
        cache.insert("newer".into(), vec![2]);
        assert!(cache.get("view-1").is_none());
        assert_eq!(cache.get("view-2"), Some(vec![2]));
    }

    struct Catalog(&'static str);

    #[async_trait]
    impl CatalogSource for Catalog {
        fn id(&self) -> SourceId {
            SourceId::new(self.0)
        }

        async fn cone(&self, _region: &SkyRegion, _epoch_mjd: Option<f64>) -> Result<Vec<CatalogRow>> {
            Ok(Vec::new())
        }
    }

    struct Index;

    #[async_trait]
    impl IndexService for Index {
        async fn health(&self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({"status": "ok"}))
        }

        async fn generation(&self) -> Result<String> {
            Ok("7".into())
        }
    }

    #[tokio::test]
    async fn a_context_registers_sources_by_their_own_id_and_reads_its_index() {
        let mut ctx = Context::new(Arc::new(Cache::new()));
        assert!(ctx.index().is_none());
        ctx.register_catalog(Arc::new(Catalog("stars")));
        ctx.register_catalog(Arc::new(Catalog("stars")));
        ctx.register_catalog(Arc::new(Catalog("galaxies")));
        assert_eq!(ctx.catalogs.len(), 2);
        assert!(ctx.catalogs.contains_key(&SourceId::new("stars")));
        let ctx = ctx.with_index(Arc::new(Index));
        let index = ctx.index().expect("index");
        assert_eq!(index.generation().await.unwrap(), "7");
        assert_eq!(index.health().await.unwrap()["status"], "ok");
    }

    #[test]
    fn deadline_names_the_limit() {
        assert_eq!(Error::Deadline { seconds: 45 }.to_string(), "operation timed out after 45s");
    }
}
