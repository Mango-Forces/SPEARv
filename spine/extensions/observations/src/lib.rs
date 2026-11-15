//! Exact observation browsing, single-snippet previews, sector builds and sequence playback.

mod artifact;
mod browse;
mod counts;
mod preview;
mod provenance;
pub mod reduction;
mod sector;
mod sequence;
mod types;

use axum::{
    extract::{Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use spearv_core::{Context, Extension, BANDS};
use spearv_index::{client::IndexClient, release_and_survey, CoverageQuery, ExposureRow};
use spearv_spherex::SpherexSource;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use types::*;
use utoipa_axum::{router::OpenApiRouter, routes};

/// Process-local idle leases, not data/depth limits. Explicit release is preferred.
mod resources {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    pub const IDLE: Duration = Duration::from_secs(30 * 60);

    pub struct Resources<T> {
        entries: HashMap<String, (T, Mutex<Instant>)>,
    }

    impl<T> Default for Resources<T> {
        fn default() -> Self {
            Self {
                entries: HashMap::new(),
            }
        }
    }

    impl<T> Resources<T> {
        pub fn insert(&mut self, id: String, value: T) {
            self.entries.insert(id, (value, Mutex::new(Instant::now())));
        }

        pub fn get(&self, id: &str) -> Option<&T> {
            let (value, touched) = self.entries.get(id)?;
            *touched.lock().ok()? = Instant::now();
            Some(value)
        }

        #[cfg(test)]
        pub fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        pub fn remove(&mut self, id: &str) -> Option<T> {
            self.entries.remove(id).map(|entry| entry.0)
        }

        pub fn expire(&mut self, now: Instant) -> Vec<T> {
            let ids: Vec<_> = self
                .entries
                .iter()
                .filter(|(_, (_, touched))| {
                    now.saturating_duration_since(*touched.lock().unwrap()) >= IDLE
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| self.remove(id)).collect()
        }
    }
}

/// Exposure discovery, snippet previews, sector builds and sequences. Discovery
/// reads the index-service through `index`; without one (the OpenAPI dump) the
/// routes still mount and discovery reports the missing client.
pub struct Observations {
    pub index: Option<Arc<IndexClient>>,
}
impl Extension for Observations {
    fn id(&self) -> &'static str {
        "observations"
    }
    fn routes(&self) -> OpenApiRouter<Arc<Context>> {
        observation_routes(Arc::new(Jobs {
            index: self.index.clone(),
            ..Jobs::default()
        }))
    }
}

/// Public releases are `qrN` or `drN`, as the index librarian names them.
fn is_release_id(release: &str) -> bool {
    let digits = release.strip_prefix("qr").or_else(|| release.strip_prefix("dr"));
    digits.is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Remove this process's own sector-build scratch directories. Directories
/// owned by other PIDs are left in place, so engines may share the configured
/// root. Call once at startup, before serving requests.
pub fn sweep_sequences() {
    sequence::sweep_sequences();
}

fn observation_routes(jobs: Arc<Jobs>) -> OpenApiRouter<Arc<Context>> {
    // Weak ownership lets the sweeper stop when the router is dropped.
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let weak = Arc::downgrade(&jobs);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let Some(jobs) = weak.upgrade() else { break };
                jobs.expire(std::time::Instant::now());
            }
        });
    }
    OpenApiRouter::new()
        .routes(routes!(query))
        .routes(routes!(page))
        .routes(routes!(status))
        .routes(routes!(release_dataset))
        .routes(routes!(retain_snippets))
        .routes(routes!(snippet_preview))
        .routes(routes!(preview_tile))
        .routes(routes!(release_preview))
        .routes(routes!(sector_build))
        .routes(routes!(sector_build_status))
        .routes(routes!(create_sequence))
        .routes(routes!(sequence_status))
        .routes(routes!(sequence_tile))
        .routes(routes!(sequence_variance_tile))
        .routes(routes!(sequence_value_tile))
        .routes(routes!(release_sequence))
        .layer(axum::Extension(jobs))
}

/// Distinguishes `Jobs` lifetimes in this process, including two lifetimes
/// created within the same system-clock nanosecond.
static BOOT_INSTANCE: AtomicU64 = AtomicU64::new(0);

fn new_incarnation() -> u128 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Pack three independent inputs so none can erase another: 64 bits of wall
    // clock, 32 bits of PID and 32 bits of in-process boot count. This is an
    // identity qualifier, not a secret or an authentication token.
    ((nanos & u128::from(u64::MAX)) << 64)
        | (u128::from(std::process::id()) << 32)
        | u128::from(BOOT_INSTANCE.fetch_add(1, Ordering::Relaxed) & u64::from(u32::MAX))
}

struct Jobs {
    /// Identity qualifier for every resource this lifetime issues. A handle from
    /// a dead lifetime therefore cannot resolve in, or release, a new lifetime.
    incarnation: u128,
    counter: AtomicU64,
    datasets: Mutex<resources::Resources<Arc<Mutex<Dataset>>>>,
    snippets: Mutex<resources::Resources<Arc<SnippetPlan>>>,
    previews: Mutex<resources::Resources<Arc<preview::Preview>>>,
    sector_builds: Mutex<resources::Resources<Arc<Mutex<sector::Build>>>>,
    sequences: Mutex<resources::Resources<Arc<Mutex<sequence::Sequence>>>>,
    /// Per-boot numerical sector artifacts keyed by the canonical fingerprint.
    /// Shared by every build and evicted by the existing idle policy.
    artifacts: Mutex<resources::Resources<Arc<artifact::Artifact>>>,
    index: Option<Arc<IndexClient>>,
    // Sector builds are serialized: one full 30-degree builder can use several
    // GiB, so a second build waits instead of racing it for RAM.
    builds: Arc<tokio::sync::Semaphore>,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            incarnation: new_incarnation(),
            counter: AtomicU64::new(0),
            datasets: Mutex::new(Default::default()),
            snippets: Mutex::new(Default::default()),
            previews: Mutex::new(Default::default()),
            sector_builds: Mutex::new(Default::default()),
            sequences: Mutex::new(Default::default()),
            artifacts: Mutex::new(Default::default()),
            index: None,
            builds: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

struct Dataset {
    status: ObservationStatus,
    rows: Vec<SnippetRow>,
    task: Option<tokio::task::AbortHandle>,
}

impl Jobs {
    fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{:032x}-{n}", self.incarnation)
    }

    fn expire(&self, now: std::time::Instant) {
        for dataset in self.datasets.lock().unwrap().expire(now) {
            if let Some(task) = dataset.lock().unwrap().task.take() {
                task.abort();
            }
        }
        for build in self.sector_builds.lock().unwrap().expire(now) {
            let mut build = build.lock().unwrap();
            build.released = true;
            if let Some(task) = build.task.take() {
                task.abort();
            }
        }
        for sequence in self.sequences.lock().unwrap().expire(now) {
            let mut sequence = sequence.lock().unwrap();
            sequence.released = true;
            if let Some(task) = sequence.task.take() {
                task.abort();
            }
        }
        self.previews.lock().unwrap().expire(now);
        self.snippets.lock().unwrap().expire(now);
        self.artifacts.lock().unwrap().expire(now);
    }

    fn cached_artifact(&self, fingerprint: &str) -> Option<Arc<artifact::Artifact>> {
        self.artifacts.lock().ok()?.get(fingerprint).cloned()
    }

    fn cache_artifact(&self, artifact: Arc<artifact::Artifact>) {
        if let Ok(mut artifacts) = self.artifacts.lock() {
            artifacts.insert(artifact.fingerprint.clone(), artifact);
        }
    }

    /// Status reads renew the artifact lease just like other resources.
    fn touch_artifact(&self, fingerprint: &str) {
        if let Ok(artifacts) = self.artifacts.lock() {
            let _ = artifacts.get(fingerprint);
        }
    }
}

struct SnippetPlan {
    region: ObservationRegion,
    file: ExposureRow,
    support: browse::Support,
    row: SnippetRow,
}

struct ApiFailure(StatusCode, String);
impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (self.0, Json(ObservationApiError { error: self.1 })).into_response()
    }
}
fn bad(error: impl ToString) -> ApiFailure {
    ApiFailure(StatusCode::BAD_REQUEST, error.to_string())
}
fn upstream(error: impl ToString) -> ApiFailure {
    ApiFailure(StatusCode::BAD_GATEWAY, error.to_string())
}
fn tile_failure((conflict, error): (bool, String)) -> ApiFailure {
    ApiFailure(
        if conflict {
            StatusCode::CONFLICT
        } else {
            StatusCode::NOT_FOUND
        },
        error,
    )
}
fn missing() -> ApiFailure {
    ApiFailure(
        StatusCode::NOT_FOUND,
        "Unknown observation, preview, or sector build reference".into(),
    )
}

#[utoipa::path(post,operation_id="observations.query",path="/api/v1/observations/query",request_body=ObservationQuery,responses((status=202,body=ObservationStatus),(status=400,body=ObservationApiError)))]
async fn query(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Json(request): Json<ObservationQuery>,
) -> Result<(StatusCode, Json<ObservationStatus>), ApiFailure> {
    request.region.validate().map_err(bad)?;
    let bands = types::parse_bands(&request.bands).map_err(bad)?;
    let (from, to) =
        utc_mjd_range(request.from_utc.as_deref(), request.to_utc.as_deref()).map_err(bad)?;
    if from.zip(to).is_some_and(|(from, to)| from > to) {
        return Err(bad("UTC start is after finish"));
    }
    if request.releases.is_empty()
        || request
            .releases
            .iter()
            .any(|release| !is_release_id(release))
    {
        return Err(bad("Select one or more releases such as DR1, QR2 or QR3"));
    }
    let id = jobs.next_id("observations");
    let initial = ObservationStatus {
        dataset_id: id.clone(),
        generation: None,
        state: "querying".into(),
        revision: 0,
        candidate_files: 0,
        sampled_from: 0,
        sampled: false,
        resolved_files: 0,
        failed_files: 0,
        geometry_bytes: 0,
        geometry_requests: 0,
        failures: Vec::new(),
        error: None,
    };
    let dataset = Arc::new(Mutex::new(Dataset {
        status: initial.clone(),
        rows: Vec::new(),
        task: None,
    }));
    jobs.datasets
        .lock()
        .map_err(upstream)?
        .insert(id, dataset.clone());
    let owner = dataset.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = discover(&jobs, &owner, request, bands, from, to).await {
            if let Ok(mut data) = owner.lock() {
                data.status.state = "failed".into();
                data.status.error = Some(error);
                data.status.revision += 1;
            }
        }
    });
    dataset.lock().map_err(upstream)?.task = Some(task.abort_handle());
    Ok((StatusCode::ACCEPTED, Json(initial)))
}

/// Keep a bounded number of exposures per time visit for the quick-video path.
/// Exact filtering still runs on each retained exposure, and manual listing is
/// never sampled.
fn sample_per_visit(
    candidates: VecDeque<ExposureRow>,
    per_visit: usize,
    gap_days: f64,
) -> VecDeque<ExposureRow> {
    let mut files: Vec<_> = candidates.into_iter().collect();
    files.sort_by(|left, right| left.mjd_avg.total_cmp(&right.mjd_avg));
    let mut sampled = VecDeque::new();
    let mut start = 0;
    while start < files.len() {
        let mut end = start + 1;
        while end < files.len() && files[end].mjd_avg - files[end - 1].mjd_avg <= gap_days {
            end += 1;
        }
        let length = end - start;
        let take = per_visit.min(length);
        for index in 0..take {
            sampled.push_back(files[start + index * length / take].clone());
        }
        start = end;
    }
    sampled
}

/// Concurrent S3 header reads. Only files whose index footprint reaches the
/// grid survive the prefilter, so this bounds network concurrency, not science.
const DISCOVERY_READS: usize = 24;

async fn discover(
    jobs: &Arc<Jobs>,
    dataset: &Arc<Mutex<Dataset>>,
    request: ObservationQuery,
    bands: Vec<u16>,
    from: Option<f64>,
    to: Option<f64>,
) -> Result<(), String> {
    // The binder hands the same index client to the SPHEREx source and here,
    // so discovery reuses its connection pool.
    let client = jobs
        .index
        .clone()
        .ok_or_else(|| "No index-service client is configured".to_string())?;
    let (fov_l, fov_h) = request.region.axes();
    let (wl_min, wl_max) = bands
        .iter()
        .fold((f64::MAX, f64::MIN), |(low, high), band| {
            let band = &BANDS[*band as usize];
            (low.min(band.wl_min_um), high.max(band.wl_max_um))
        });
    let coverage = client
        .coverage(&CoverageQuery {
            from,
            to,
            release: if request.releases.len() == 1 {
                request.releases.first().map(|release| release_and_survey(release).0)
            } else {
                None
            },
            wl_min: Some(wl_min),
            wl_max: Some(wl_max),
            ..CoverageQuery::view(request.region.ra, request.region.dec, fov_l.max(fov_h))
        })
        .await
        .map_err(|err| err.to_string())?;
    // band_of resolves overlapping detector intervals by wavelength, not detector ID.
    // Detector equality is therefore not a safe pruning rule at interval boundaries.
    let detectors: std::collections::BTreeSet<u8> = bands
        .iter()
        .map(|band| BANDS[*band as usize].detector)
        .collect();
    let mut candidates: VecDeque<_> = coverage
        .exposures
        .into_iter()
        .filter(|file| {
            request.releases.contains(&file.release) && detectors.contains(&file.detector)
        })
        .collect();
    let sampled_from = candidates.len();
    let sampled = request.per_visit.is_some();
    if let Some(per_visit) = request.per_visit {
        candidates = sample_per_visit(
            candidates,
            per_visit.clamp(1, 64),
            request.visit_gap_days.unwrap_or(10.0).max(0.0),
        );
    }
    {
        let mut data = dataset.lock().map_err(|err| err.to_string())?;
        data.status.generation = Some(coverage.generation);
        data.status.candidate_files = candidates.len();
        data.status.sampled_from = sampled_from;
        data.status.sampled = sampled;
        data.status.state = "resolving".into();
        data.status.revision += 1;
    }
    let source = Arc::new(SpherexSource::direct().map_err(|err| err.to_string())?);
    let grid = request.region.grid();
    let mut reads = tokio::task::JoinSet::new();
    loop {
        while reads.len() < DISCOVERY_READS {
            let Some(file) = candidates.pop_front() else {
                break;
            };
            let source = source.clone();
            let region = request.region.clone();
            let bands = bands.clone();
            let grid = grid.clone();
            reads.spawn(async move {
                // The index WCS is the same header WCS the exact read would
                // return. A detector whose rectangle misses this grid cannot
                // support any of its pixels, so skip the S3 header read.
                if grid.detector_bounds(&file.wcs).is_none() {
                    return (file, Ok((Vec::new(), 0)));
                }
                let result = async {
                    let geometry = source
                        .exact_geometry(&file)
                        .await
                        .map_err(|err| err.to_string())?;
                    let bytes = geometry.header_bytes;
                    let supports = tokio::task::spawn_blocking(move || {
                        browse::supports(&geometry, &region.grid(), &bands)
                    })
                    .await
                    .map_err(|err| err.to_string())?;
                    Ok::<_, String>((supports, bytes))
                }
                .await;
                (file, result)
            });
        }
        let Some(result) = reads.join_next().await else {
            break;
        };
        let (file, result) = result.map_err(|err| err.to_string())?;
        dataset
            .lock()
            .map_err(|err| err.to_string())?
            .status
            .geometry_requests = source.stats().0;
        match result {
            Ok((supports, bytes)) => {
                let plans = supports
                    .into_iter()
                    .map(|support| {
                        let row = browse::snippet_row(&request.region, &file, &support)?;
                        Ok(Arc::new(SnippetPlan {
                            region: request.region.clone(),
                            file: file.clone(),
                            support,
                            row,
                        }))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                {
                    let mut registry = jobs.snippets.lock().map_err(|err| err.to_string())?;
                    for plan in &plans {
                        registry.insert(plan.row.id.clone(), plan.clone());
                    }
                }
                let mut data = dataset.lock().map_err(|err| err.to_string())?;
                data.rows
                    .extend(plans.into_iter().map(|plan| plan.row.clone()));
                data.status.resolved_files += 1;
                data.status.geometry_bytes += bytes;
                data.status.revision += 1;
            }
            Err(error) => {
                let mut data = dataset.lock().map_err(|err| err.to_string())?;
                data.status.failed_files += 1;
                data.status.failures.push(GeometryFailure {
                    key: file.key,
                    error,
                });
                data.status.revision += 1;
            }
        }
    }
    let mut data = dataset.lock().map_err(|err| err.to_string())?;
    data.status.state = "done".into();
    data.status.revision += 1;
    Ok(())
}

#[utoipa::path(get,operation_id="observations.page",path="/api/v1/observations/{dataset_id}",params(("dataset_id"=String,Path),ObservationPageQuery),responses((status=200,body=ObservationPage),(status=404,body=ObservationApiError)))]
async fn page(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(id): Path<String>,
    Query(query): Query<ObservationPageQuery>,
) -> Result<Json<ObservationPage>, ApiFailure> {
    let dataset = jobs
        .datasets
        .lock()
        .map_err(upstream)?
        .get(&id)
        .cloned()
        .ok_or_else(missing)?;
    let data = dataset.lock().map_err(upstream)?;
    // An open table owns all its rows, including those outside its current page.
    let registry = jobs.snippets.lock().map_err(upstream)?;
    data.rows.iter().for_each(|row| {
        registry.get(&row.id);
    });
    let (count, rows) = browse::page_rows(&data.rows, &query).map_err(bad)?;
    Ok(Json(ObservationPage {
        revision: data.status.revision,
        state: data.status.state.clone(),
        matches_so_far: count,
        pending_files: data
            .status
            .candidate_files
            .saturating_sub(data.status.resolved_files + data.status.failed_files),
        failed_files: data.status.failed_files,
        sampled: data.status.sampled,
        sampled_from: data.status.sampled_from,
        rows,
        error: data.status.error.clone(),
    }))
}

#[utoipa::path(get,operation_id="observations.status",path="/api/v1/observations/{dataset_id}/status",params(("dataset_id"=String,Path)),responses((status=200,body=ObservationStatus),(status=404,body=ObservationApiError)))]
async fn status(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(id): Path<String>,
) -> Result<Json<ObservationStatus>, ApiFailure> {
    let dataset = jobs
        .datasets
        .lock()
        .map_err(upstream)?
        .get(&id)
        .cloned()
        .ok_or_else(missing)?;
    let data = dataset.lock().map_err(upstream)?;
    let registry = jobs.snippets.lock().map_err(upstream)?;
    data.rows.iter().for_each(|row| {
        registry.get(&row.id);
    });
    let result = data.status.clone();
    Ok(Json(result))
}

#[utoipa::path(delete,operation_id="observations.release",path="/api/v1/observations/{dataset_id}",params(("dataset_id"=String,Path)),responses((status=204,description="Discovery cancelled and snapshot released; snippet refs retain their idle lease")))]
async fn release_dataset(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    if let Some(dataset) = jobs.datasets.lock().map_err(upstream)?.remove(&id) {
        if let Some(task) = dataset.lock().map_err(upstream)?.task.take() {
            task.abort();
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Keep assigned, filter-hidden exact refs alive without reading image data.
/// Every ref that still exists is renewed; expired refs are reported instead of
/// aborting the request before later live refs can be renewed.
#[utoipa::path(post,operation_id="snippets.retain",path="/api/v1/snippets/retain",request_body=Vec<String>,responses((status=200,body=RetainResult,description="Every live ref was renewed; missing refs are reported"),(status=502,body=ObservationApiError)))]
async fn retain_snippets(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Json(refs): Json<Vec<String>>,
) -> Result<Json<RetainResult>, ApiFailure> {
    let registry = jobs.snippets.lock().map_err(upstream)?;
    let (renewed, missing) = refs
        .into_iter()
        .partition(|reference| registry.get(reference).is_some());
    Ok(Json(RetainResult { renewed, missing }))
}

#[utoipa::path(post,operation_id="snippets.preview",path="/api/v1/snippets/preview",request_body=SnippetPreviewRequest,responses((status=200,body=SnippetPreviewInfo),(status=400,body=ObservationApiError),(status=404,body=ObservationApiError),(status=502,body=ObservationApiError)))]
async fn snippet_preview(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Json(request): Json<SnippetPreviewRequest>,
) -> Result<Json<SnippetPreviewInfo>, ApiFailure> {
    request.region.validate().map_err(bad)?;
    let plan = jobs
        .snippets
        .lock()
        .map_err(upstream)?
        .get(&request.snippet_ref)
        .cloned()
        .ok_or_else(missing)?;
    if !request.region.same_grid(&plan.region) {
        return Err(bad("Snippet belongs to a different region"));
    }
    let source = SpherexSource::direct().map_err(upstream)?;
    let cutout = source
        .exact_window_cutout(&plan.file, &plan.support.window)
        .await
        .map_err(upstream)?;
    let id = jobs.next_id("preview");
    let grid = plan.region.grid();
    let info = SnippetPreviewInfo {
        preview_id: id.clone(),
        snippet: plan.row.clone(),
        width: grid.width,
        height: grid.height,
        tile_size: preview::TILE_SIZE,
        state: "building".into(),
        valid_samples: 0,
        bytes_read: 0,
        requests: 0,
        stretch_low: 0.0,
        stretch_high: 0.0,
        data_tiles: Vec::new(),
    };
    let bounds = plan.support.grid_bounds;
    let result = tokio::task::spawn_blocking(move || {
        preview::render(cutout, grid, plan.support.band, request.mask, bounds, info)
    })
    .await
    .map_err(upstream)?
    .map_err(upstream)?;
    let info = result.info.clone();
    jobs.previews
        .lock()
        .map_err(upstream)?
        .insert(id, Arc::new(result));
    Ok(Json(info))
}

#[utoipa::path(get,operation_id="snippets.tile",path="/api/v1/snippets/previews/{preview_id}/tiles/{x}/{y}",params(("preview_id"=String,Path),("x"=u32,Path),("y"=u32,Path)),responses((status=200,content_type="image/png"),(status=404,body=ObservationApiError)))]
async fn preview_tile(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path((id, x, y)): Path<(String, u32, u32)>,
) -> Result<Response, ApiFailure> {
    let result = jobs
        .previews
        .lock()
        .map_err(upstream)?
        .get(&id)
        .cloned()
        .ok_or_else(missing)?;
    let png = preview::tile_png(&result, x, y).map_err(bad)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

#[utoipa::path(delete,operation_id="snippets.release",path="/api/v1/snippets/previews/{preview_id}",params(("preview_id"=String,Path)),responses((status=204,description="Preview resources released")))]
async fn release_preview(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    jobs.previews.lock().map_err(upstream)?.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post,operation_id="sectors.build",path="/api/v1/sectors/build",request_body=SectorBuildRequest,responses((status=202,body=SectorBuildInfo),(status=400,body=ObservationApiError),(status=404,body=ObservationApiError)))]
async fn sector_build(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Json(request): Json<SectorBuildRequest>,
) -> Result<(StatusCode, Json<SectorBuildInfo>), ApiFailure> {
    request.region.validate().map_err(bad)?;
    let admitted = {
        let snippets = jobs.snippets.lock().map_err(upstream)?;
        sector::admit(&request.region, vec![request.sector], |reference| {
            snippets.get(reference).cloned()
        })
        .map_err(refusal)?
    };
    let admitted = admitted
        .into_iter()
        .next()
        .ok_or_else(|| bad("Add a sector before building"))?;
    let grid = request.region.grid();
    let build_id = jobs.next_id("sector");
    let info = SectorBuildInfo {
        build_id: build_id.clone(),
        width: grid.width,
        height: grid.height,
        tile_size: preview::TILE_SIZE,
        state: "queued".into(),
        error: None,
        sector: sector::frame_info(&admitted).map_err(bad)?,
        reduction: sector::REDUCTION.into(),
    };
    let owner = Arc::new(Mutex::new(sector::Build::new(info.clone())));
    jobs.sector_builds
        .lock()
        .map_err(upstream)?
        .insert(build_id, owner.clone());
    let task = tokio::spawn(sector::run(owner.clone(), grid, admitted, jobs.clone()));
    owner.lock().map_err(upstream)?.task = Some(task.abort_handle());
    Ok((StatusCode::ACCEPTED, Json(info)))
}

fn refusal(error: sector::Refusal) -> ApiFailure {
    match error {
        sector::Refusal::UnknownSnippet(message) => ApiFailure(StatusCode::NOT_FOUND, message),
        sector::Refusal::Invalid(message) => ApiFailure(StatusCode::BAD_REQUEST, message),
    }
}

#[utoipa::path(get,operation_id="sectors.status",path="/api/v1/sectors/builds/{build_id}",params(("build_id"=String,Path)),responses((status=200,body=SectorBuildInfo),(status=404,body=ObservationApiError)))]
async fn sector_build_status(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(build_id): Path<String>,
) -> Result<Json<SectorBuildInfo>, ApiFailure> {
    let owner = jobs
        .sector_builds
        .lock()
        .map_err(upstream)?
        .get(&build_id)
        .cloned()
        .ok_or_else(missing)?;
    let info = owner.lock().map_err(upstream)?.info.clone();
    jobs.touch_artifact(&info.sector.fingerprint);
    Ok(Json(info))
}

/// Builds every submitted sector, in submitted order, with the approved reduction.
/// One sequence owns all its frames; a one-sector sequence is a sector build.
#[utoipa::path(post,operation_id="sequences.create",path="/api/v1/sequences",request_body=SequenceRequest,responses((status=202,body=SequenceInfo),(status=400,body=ObservationApiError),(status=404,body=ObservationApiError)))]
async fn create_sequence(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Json(request): Json<SequenceRequest>,
) -> Result<(StatusCode, Json<SequenceInfo>), ApiFailure> {
    let admitted = {
        let snippets = jobs.snippets.lock().map_err(upstream)?;
        sequence::admit(&request.region, request.sectors, |reference| {
            snippets.get(reference).cloned()
        })
        .map_err(|refusal| match refusal {
            sequence::Refusal::UnknownSnippet(message) => {
                ApiFailure(StatusCode::NOT_FOUND, message)
            }
            sequence::Refusal::Invalid(message) => ApiFailure(StatusCode::BAD_REQUEST, message),
        })?
    };
    let sectors = admitted
        .iter()
        .map(sequence::frame_info)
        .collect::<Result<Vec<_>, _>>()
        .map_err(bad)?;
    let grid = request.region.grid();
    let id = jobs.next_id("sequence");
    let info = SequenceInfo {
        sequence_id: id.clone(),
        width: grid.width,
        height: grid.height,
        tile_size: preview::TILE_SIZE,
        state: "building".into(),
        display_revision: None,
        stretch_low: None,
        stretch_high: None,
        sectors,
        error: None,
        reduction: sequence::REDUCTION.into(),
    };
    let frames = (0..admitted.len()).map(|_| None).collect();
    let owner = Arc::new(Mutex::new(
        sequence::Sequence::new(info.clone(), frames).map_err(upstream)?,
    ));
    jobs.sequences
        .lock()
        .map_err(upstream)?
        .insert(id, owner.clone());
    let task = tokio::spawn(sequence::run(
        owner.clone(),
        grid,
        admitted,
        jobs.builds.clone(),
        jobs.clone(),
    ));
    owner.lock().map_err(upstream)?.task = Some(task.abort_handle());
    Ok((StatusCode::ACCEPTED, Json(info)))
}

fn find_sequence(jobs: &Jobs, id: &str) -> Result<Arc<Mutex<sequence::Sequence>>, ApiFailure> {
    jobs.sequences
        .lock()
        .map_err(upstream)?
        .get(id)
        .cloned()
        .ok_or_else(missing)
}

#[utoipa::path(get,operation_id="sequences.status",path="/api/v1/sequences/{sequence_id}/status",params(("sequence_id"=String,Path)),responses((status=200,body=SequenceInfo),(status=404,body=ObservationApiError)))]
async fn sequence_status(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(id): Path<String>,
) -> Result<Json<SequenceInfo>, ApiFailure> {
    let owner = find_sequence(&jobs, &id)?;
    let info = owner.lock().map_err(upstream)?.info.clone();
    Ok(Json(info))
}

/// 409 until the shared mapping is frozen, or when the caller's display revision is stale.
#[utoipa::path(get,operation_id="sequences.tile",path="/api/v1/sequences/{sequence_id}/sectors/{sector_id}/tiles/{x}/{y}",params(("sequence_id"=String,Path),("sector_id"=String,Path),("x"=u32,Path),("y"=u32,Path),SequenceTileQuery),responses((status=200,content_type="image/png"),(status=404,body=ObservationApiError),(status=409,body=ObservationApiError)))]
async fn sequence_tile(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path((id, sector_id, x, y)): Path<(String, String, u32, u32)>,
    Query(query): Query<SequenceTileQuery>,
) -> Result<Response, ApiFailure> {
    let owner = find_sequence(&jobs, &id)?;
    // Capture the frame and mapping under the lock, then read and encode the
    // tile on a blocking thread without it.
    let (source, fingerprint) = {
        let guard = owner.lock().map_err(upstream)?;
        let source = sequence::tile_source(&guard, &sector_id, query.display_revision)
            .map_err(tile_failure)?;
        let fingerprint = guard
            .info
            .sectors
            .iter()
            .find(|sector| sector.id == sector_id)
            .map(|sector| sector.fingerprint.clone());
        (source, fingerprint)
    };
    if let Some(fingerprint) = fingerprint {
        jobs.touch_artifact(&fingerprint);
    }
    let png = tokio::task::spawn_blocking(move || source.encode(x, y))
        .await
        .map_err(upstream)?
        .map_err(tile_failure)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// Raw per-pixel `SectorVariance(p)` for one 256×256 data tile, as little-endian
/// `f32` values in row-major PNG order. No PNG encoding and no display stretch.
#[utoipa::path(get,operation_id="sequences.variance",path="/api/v1/sequences/{sequence_id}/sectors/{sector_id}/variance/{x}/{y}",params(("sequence_id"=String,Path),("sector_id"=String,Path),("x"=u32,Path),("y"=u32,Path)),responses((status=200,content_type="application/octet-stream"),(status=404,body=ObservationApiError),(status=409,body=ObservationApiError)))]
async fn sequence_variance_tile(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path((id, sector_id, x, y)): Path<(String, String, u32, u32)>,
) -> Result<Response, ApiFailure> {
    let owner = find_sequence(&jobs, &id)?;
    let (frame, fingerprint) = {
        let guard = owner.lock().map_err(upstream)?;
        let frame = sequence::variance_source(&guard, &sector_id).map_err(tile_failure)?;
        let fingerprint = guard
            .info
            .sectors
            .iter()
            .find(|sector| sector.id == sector_id)
            .map(|sector| sector.fingerprint.clone());
        (frame, fingerprint)
    };
    if let Some(fingerprint) = fingerprint {
        jobs.touch_artifact(&fingerprint);
    }
    let bytes =
        tokio::task::spawn_blocking(move || sequence::encode_variance(&frame, &sector_id, x, y))
            .await
            .map_err(upstream)?
            .map_err(tile_failure)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

#[utoipa::path(get,operation_id="sequences.values",path="/api/v1/sequences/{sequence_id}/sectors/{sector_id}/values/{x}/{y}",params(("sequence_id"=String,Path),("sector_id"=String,Path),("x"=u32,Path),("y"=u32,Path)),responses((status=200,content_type="application/octet-stream"),(status=404,body=ObservationApiError),(status=409,body=ObservationApiError)))]
/// Raw little-endian `f32` calibrated values of one 256×256 tile, MJy/sr with
/// NaN for no data. The browser applies its own shared stretch and comparisons.
async fn sequence_value_tile(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path((id, sector_id, x, y)): Path<(String, String, u32, u32)>,
) -> Result<Response, ApiFailure> {
    let owner = find_sequence(&jobs, &id)?;
    let (frame, fingerprint) = {
        let guard = owner.lock().map_err(upstream)?;
        let frame = sequence::variance_source(&guard, &sector_id).map_err(tile_failure)?;
        let fingerprint = guard
            .info
            .sectors
            .iter()
            .find(|sector| sector.id == sector_id)
            .map(|sector| sector.fingerprint.clone());
        (frame, fingerprint)
    };
    if let Some(fingerprint) = fingerprint {
        jobs.touch_artifact(&fingerprint);
    }
    let bytes = tokio::task::spawn_blocking(move || sequence::encode_values(&frame, &sector_id, x, y))
        .await
        .map_err(upstream)?
        .map_err(tile_failure)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

#[utoipa::path(delete,operation_id="sequences.release",path="/api/v1/sequences/{sequence_id}",params(("sequence_id"=String,Path)),responses((status=204,description="Sequence work cancelled and frames released")))]
async fn release_sequence(
    axum::Extension(jobs): axum::Extension<Arc<Jobs>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let removed = jobs.sequences.lock().map_err(upstream)?.remove(&id);
    if let Some(owner) = removed {
        let mut sequence = owner.lock().map_err(upstream)?;
        sequence.released = true;
        if let Some(task) = sequence.task.take() {
            task.abort();
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn snapshot_release_keeps_refs_and_retain_renews_live_or_reports_missing() {
        let jobs = Arc::new(Jobs::default());
        let plan = sequence::tests::plan("owned", 0, 60000.0, sequence::tests::region());
        jobs.snippets
            .lock()
            .unwrap()
            .insert("owned".into(), plan.clone());
        let task = tokio::spawn(std::future::pending::<()>());
        jobs.datasets.lock().unwrap().insert(
            "old".into(),
            Arc::new(Mutex::new(Dataset {
                rows: vec![plan.row.clone()],
                task: Some(task.abort_handle()),
                status: ObservationStatus {
                    dataset_id: "old".into(),
                    generation: None,
                    state: "resolving".into(),
                    revision: 0,
                    candidate_files: 1,
                    sampled_from: 1,
                    sampled: false,
                    resolved_files: 0,
                    failed_files: 0,
                    geometry_bytes: 0,
                    geometry_requests: 0,
                    failures: vec![],
                    error: None,
                },
            })),
        );
        let (router, _) = observation_routes(jobs.clone()).split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/observations/old")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(jobs.datasets.lock().unwrap().is_empty());
        let retained_at = std::time::Instant::now();
        let retain = |body: &'static str| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/snippets/retain")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let response = app
            .clone()
            .oneshot(retain("[\"gone\",\"owned\"]"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            result,
            serde_json::json!({"renewed": ["owned"], "missing": ["gone"]}),
            "a missing early ref must not abort renewal of the live ref"
        );
        // The live lease was touched by this request, so it outlives the
        // instant at which its insertion lease alone would have expired.
        jobs.expire(retained_at + resources::IDLE);
        assert!(
            jobs.snippets.lock().unwrap().get("owned").is_some(),
            "the mixed retain renewed the live ref"
        );
        jobs.expire(std::time::Instant::now() + resources::IDLE);
        assert!(jobs.snippets.lock().unwrap().is_empty());
        let response = app.oneshot(retain("[\"owned\"]")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            result,
            serde_json::json!({"renewed": [], "missing": ["owned"]}),
            "an expired ref is reported missing instead of failing the request"
        );
    }

    #[tokio::test]
    async fn sequence_routes_enforce_exclusive_membership_and_shared_mapping_gate() {
        let jobs = Arc::new(Jobs::default());
        for (id, band) in [("a", 0), ("b", 2)] {
            let plan = sequence::tests::plan(id, band, 60000.0, sequence::tests::region());
            jobs.snippets.lock().unwrap().insert(id.into(), plan);
        }
        let (router, _) = observation_routes(jobs.clone()).split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let region = sequence::tests::region();
        let post = |body: serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/sequences")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let json = |response: Response| async move {
            serde_json::from_slice::<serde_json::Value>(
                &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            )
            .unwrap()
        };
        let shared = serde_json::json!({"region": {"ra": region.ra, "dec": region.dec, "fov": region.fov},
            "sectors": [{"id": "s1", "revision": 1, "snippets": ["a"]}, {"id": "s2", "revision": 1, "snippets": ["a", "b"]}]});
        let response = app.clone().oneshot(post(shared)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("exactly one sector"));
        assert!(
            jobs.sequences.lock().unwrap().is_empty(),
            "a refused composition starts no work"
        );

        let unknown = serde_json::json!({"region": {"ra": region.ra, "dec": region.dec, "fov": region.fov},
            "sectors": [{"id": "s1", "revision": 1, "snippets": ["gone"]}]});
        let response = app.clone().oneshot(post(unknown)).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "an unknown snippet ref is not a malformed composition"
        );
        assert!(json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("Unknown snippet"));
        let empty = serde_json::json!({"region": {"ra": region.ra, "dec": region.dec, "fov": region.fov},
            "sectors": [{"id": "s1", "revision": 1, "snippets": []}]});
        let response = app.clone().oneshot(post(empty)).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "an empty sector stays a 400"
        );

        let valid = serde_json::json!({"region": {"ra": region.ra, "dec": region.dec, "fov": region.fov},
            "sectors": [{"id": "s2", "revision": 3, "snippets": ["b"]}, {"id": "s1", "revision": 1, "snippets": ["a"]}]});
        let response = app.clone().oneshot(post(valid)).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let info = json(response).await;
        let order: Vec<_> = info["sectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(order, ["s2", "s1"], "submitted order is playback order");
        let id = info["sequence_id"].as_str().unwrap().to_string();
        let get = |uri: String| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let tile = app
            .clone()
            .oneshot(get(format!(
                "/api/v1/sequences/{id}/sectors/s1/tiles/0/0?display_revision=1"
            )))
            .await
            .unwrap();
        assert_eq!(
            tile.status(),
            StatusCode::CONFLICT,
            "no tile before one shared mapping exists"
        );
        let delete = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/sequences/{id}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(delete).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        let status = app
            .oneshot(get(format!("/api/v1/sequences/{id}/status")))
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn releasing_or_expiring_a_sequence_removes_its_scratch_directory() {
        let jobs = Arc::new(Jobs::default());
        let info = |id: &str| SequenceInfo {
            sequence_id: id.into(),
            width: 2,
            height: 2,
            tile_size: preview::TILE_SIZE,
            state: "building".into(),
            display_revision: None,
            stretch_low: None,
            stretch_high: None,
            sectors: Vec::new(),
            error: None,
            reduction: sequence::REDUCTION.into(),
        };
        let owner = |id: &str| {
            Arc::new(Mutex::new(
                sequence::Sequence::new(info(id), vec![None]).unwrap(),
            ))
        };
        let released = owner("released");
        let released_dir = released.lock().unwrap().directory().to_path_buf();
        jobs.sequences
            .lock()
            .unwrap()
            .insert("released".into(), released);
        let expired = owner("expired");
        let expired_dir = expired.lock().unwrap().directory().to_path_buf();
        jobs.sequences
            .lock()
            .unwrap()
            .insert("expired".into(), expired);
        assert!(released_dir.exists() && expired_dir.exists());

        let (router, _) = observation_routes(jobs.clone()).split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let delete = Request::builder()
            .method("DELETE")
            .uri("/api/v1/sequences/released")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(delete).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert!(
            !released_dir.exists(),
            "explicit release removes the directory"
        );

        jobs.expire(std::time::Instant::now() + resources::IDLE);
        assert!(jobs.sequences.lock().unwrap().is_empty());
        assert!(!expired_dir.exists(), "idle expiry removes the directory");
    }

    #[tokio::test]
    async fn variance_route_returns_raw_f32_and_gates_missing_or_unready_tiles() {
        use crate::reduction::{DataTile, DataTiles};
        let jobs = Arc::new(Jobs::default());
        let sector = |id: &str| SectorFrameInfo {
            id: id.into(),
            revision: 1,
            state: "ready".into(),
            error: None,
            snippets: 1,
            members: Vec::new(),
            fingerprint: format!("fingerprint-{id}"),
            bands: vec![1],
            releases: vec!["qr3".into()],
            mjd_start: 0.0,
            mjd_end: 0.0,
            utc_start: String::new(),
            utc_end: String::new(),
            complete_pixels: 65536,
            target_coverage: 1.0,
            target_covered: true,
            contributions: None,
            count_bytes: None,
            bytes_read: 0,
            requests: 0,
            data_tiles: vec![TileCoordinate { x: 0, y: 0 }],
        };
        let info = |state: &str, revision: Option<u64>| SequenceInfo {
            sequence_id: "v".into(),
            width: 300,
            height: 1,
            tile_size: preview::TILE_SIZE,
            state: state.into(),
            display_revision: revision,
            stretch_low: Some(1.0),
            stretch_high: Some(2.0),
            sectors: vec![sector("s0")],
            error: None,
            reduction: sequence::REDUCTION.into(),
        };
        let tile = DataTile {
            value: vec![1.0; (preview::TILE_SIZE * preview::TILE_SIZE) as usize],
            variance: vec![0.5; (preview::TILE_SIZE * preview::TILE_SIZE) as usize],
        };
        let frames = || {
            vec![Some(sequence::Frame::Memory(DataTiles::from([(
                (0, 0),
                tile.clone(),
            )])))]
        };
        let insert = |id: &str, info: SequenceInfo| {
            jobs.sequences.lock().unwrap().insert(
                id.into(),
                Arc::new(Mutex::new(sequence::Sequence::new(info, frames()).unwrap())),
            );
        };
        insert("ready", info("ready", Some(1)));
        insert("building", info("building", None));
        let (router, _) = observation_routes(jobs.clone()).split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let get = |uri: String| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let variance = |id: &str, x: u32, y: u32| {
            format!("/api/v1/sequences/{id}/sectors/s0/variance/{x}/{y}")
        };
        let response = app
            .clone()
            .oneshot(get(variance("building", 0, 0)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let response = app
            .clone()
            .oneshot(get(variance("ready", 1, 0)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app.oneshot(get(variance("ready", 0, 0))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            bytes.len(),
            (preview::TILE_SIZE * preview::TILE_SIZE * 4) as usize
        );
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0.5);
    }

    #[tokio::test]
    async fn routes_reject_unknown_refs_and_invalid_admission_without_fetching_urls() {
        let (router, _) = Observations { index: None }.routes().split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let body=serde_json::json!({"region":{"ra":10,"dec":20,"fov":0.1},"snippet_ref":"https://attacker.invalid/file","mask":true}).to_string();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/snippets/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body=serde_json::json!({"region":{"ra":10,"dec":20,"fov":0.1},"bands":"0","releases":["qr3"]}).to_string();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/observations/query")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(error["error"].as_str().unwrap().contains("1–102"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/observations/unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sector_build_rejects_unknown_empty_and_duplicate_members_without_fetching() {
        let jobs = Arc::new(Jobs::default());
        let region = sector::tests::region();
        let plan = sector::tests::plan("a", 0, 1.0, region.clone());
        jobs.snippets.lock().unwrap().insert("a".into(), plan);
        let (router, _) = observation_routes(jobs).split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let region = serde_json::to_value(region).unwrap();
        for (sector, status, expected) in [
            (
                serde_json::json!({"id":"s1","revision":1,"snippets":["missing"]}),
                StatusCode::NOT_FOUND,
                "Unknown snippet",
            ),
            (
                serde_json::json!({"id":"s1","revision":1,"snippets":[]}),
                StatusCode::BAD_REQUEST,
                "is empty",
            ),
            (
                serde_json::json!({"id":"s1","revision":1,"snippets":["a","a"]}),
                StatusCode::BAD_REQUEST,
                "exactly one sector",
            ),
        ] {
            let body = serde_json::json!({"region":region,"sector":sector}).to_string();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/sectors/build")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            let error: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert!(error["error"].as_str().unwrap().contains(expected));
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/sectors/builds/unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sector_build_accepts_exact_members_and_reports_render_metadata() {
        let jobs = Arc::new(Jobs::default());
        let region = sector::tests::region();
        let plan = sector::tests::plan("a", 0, 1.0, region.clone());
        jobs.snippets.lock().unwrap().insert("a".into(), plan);
        let (router, _) = observation_routes(jobs.clone()).split_for_parts();
        let app = router.with_state(Arc::new(Context::new(Arc::new(spearv_core::Cache::new()))));
        let body = serde_json::json!({
            "region": region,
            "sector": {"id": "Sector 1", "revision": 1, "snippets": ["a"]}
        })
        .to_string();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/sectors/build")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let info: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(info["sector"]["id"], "Sector 1");
        assert_eq!(info["sector"]["bands"], serde_json::json!([1]));
        assert_eq!(info["sector"]["snippets"], 1);
        assert_eq!(info["sector"]["members"][0]["s3_key"], "key-a");
        assert_eq!(info["sector"]["members"][0]["etag"], "\"e\"");
        assert_eq!(info["sector"]["utc_start"], info["sector"]["utc_end"]);
        assert_eq!(
            info["sector"]["fingerprint"].as_str().unwrap().len(),
            64,
            "fingerprint is a SHA-256 hex identity"
        );
        assert!(info["reduction"]
            .as_str()
            .unwrap()
            .contains("inverse-variance"));
        let build_id = info["build_id"].as_str().unwrap().to_string();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/sectors/builds/{build_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let status: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(status["sector"]["id"], "Sector 1");
        assert_eq!(
            status["sector"]["fingerprint"],
            info["sector"]["fingerprint"]
        );
        jobs.expire(std::time::Instant::now() + resources::IDLE);
    }
}
