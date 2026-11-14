//! Color-only immutable display revisions below (never inside) science identity.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use axum::body::Body;
use axum::extract::{rejection::JsonRejection, Path};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Serialize;
use spearv_core::ApiError;
use tokio::sync::Notify;
use tokio_util::io::ReaderStream;
use utoipa::ToSchema;

use super::color_display::{ColorDisplay, ResolvedColor};
use super::display_file::{preview_dimensions, render_preview, DisplayCache, PublishedFile};
use super::{raw_state, Display, Job, Jobs, RawChannelMetadata, Stretch};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    NotRequested,
    Queued,
    Rendering,
    Ready,
    Failed,
    Superseded,
    Evicted,
}

#[derive(Clone, Serialize, ToSchema)]
pub struct DisplayArtifact {
    pub state: ArtifactState,
    pub width: u32,
    pub height: u32,
    pub url: String,
    pub bytes: Option<u64>,
    pub error: Option<String>,
    pub completed_rows: u32,
    pub total_rows: u32,
}

#[derive(Serialize, ToSchema)]
pub struct DisplayManifest {
    pub job_id: String,
    pub revision: String,
    pub current: bool,
    pub width: u32,
    pub height: u32,
    pub display: ColorDisplay,
    pub channels: Vec<RawChannelMetadata>,
    pub preview: DisplayArtifact,
    pub full: DisplayArtifact,
}

pub(super) struct Artifacts {
    pub preview: DisplayArtifact,
    pub full: DisplayArtifact,
    pub preview_bytes: Option<Arc<[u8]>>,
    pub file: Option<Arc<PublishedFile>>,
}

pub(super) struct Revision {
    token: String,
    display: ColorDisplay,
    channels: Vec<RawChannelMetadata>,
    pub resolved: ResolvedColor,
    pub current: AtomicBool,
    pub artifacts: Mutex<Artifacts>,
}

impl Revision {
    fn new(
        job: &Job,
        token: String,
        display: ColorDisplay,
        channels: Vec<RawChannelMetadata>,
    ) -> Self {
        let resolved = display.resolve(std::array::from_fn(|i| {
            channels[i]
                .default_black
                .zip(channels[i].default_span)
                .map(|(low, span)| Stretch { low, span })
        }));
        let artifact = |kind: &str, state, width, height| DisplayArtifact {
            state,
            width,
            height,
            url: format!(
                "/api/v1/mosaic/{}/display-revisions/{token}/{kind}.png",
                job.id
            ),
            bytes: None,
            error: None,
            completed_rows: 0,
            total_rows: height,
        };
        let (width, height) = preview_dimensions(job.grid.width, job.grid.height);
        let artifacts = Artifacts {
            preview: artifact("preview", ArtifactState::Queued, width, height),
            full: artifact(
                "full",
                ArtifactState::NotRequested,
                job.grid.width,
                job.grid.height,
            ),
            preview_bytes: None,
            file: None,
        };
        Self {
            token,
            display,
            channels,
            resolved,
            current: AtomicBool::new(true),
            artifacts: Mutex::new(artifacts),
        }
    }

    fn supersede(&self) {
        self.current.store(false, Ordering::Release);
        let mut artifacts = self.artifacts.lock().expect("display artifacts");
        artifacts.preview.state = ArtifactState::Superseded;
        artifacts.full.state = ArtifactState::Superseded;
        artifacts.preview.bytes = None;
        artifacts.full.bytes = None;
        artifacts.preview_bytes = None;
        if let Some(file) = &artifacts.file {
            match file.unlink() {
                Ok(()) => artifacts.file = None,
                // Retain ownership for another eviction attempt; a cache error
                // must remain visible rather than silently leaving a PNG behind.
                Err(error) => artifacts.full.error = Some(error),
            }
        }
    }

    fn manifest(&self, job: &Job) -> DisplayManifest {
        let artifacts = self.artifacts.lock().expect("display artifacts");
        DisplayManifest {
            job_id: job.id.clone(),
            revision: self.token.clone(),
            current: self.current.load(Ordering::Acquire),
            width: job.grid.width,
            height: job.grid.height,
            display: self.display.clone(),
            channels: self.channels.clone(),
            preview: artifacts.preview.clone(),
            full: artifacts.full.clone(),
        }
    }
}

#[derive(Default)]
struct Store {
    next: u64,
    revisions: VecDeque<Arc<Revision>>,
    preview_worker: bool,
    full_worker: bool,
    cancelled: bool,
}

pub(super) struct Revisions {
    store: Mutex<Store>,
    notify: Arc<Notify>,
    pub(super) cache: Arc<DisplayCache>,
}

impl Default for Revisions {
    fn default() -> Self {
        Self {
            store: Mutex::new(Store::default()),
            notify: Arc::new(Notify::new()),
            cache: DisplayCache::shared(),
        }
    }
}

impl Revisions {
    pub(super) fn cancel(&self) {
        let mut store = self.store.lock().expect("display revisions");
        store.cancelled = true;
        for revision in &store.revisions {
            revision.supersede();
        }
        self.notify.notify_waiters();
    }

    fn find(&self, token: &str) -> Option<Arc<Revision>> {
        self.store
            .lock()
            .expect("display revisions")
            .revisions
            .iter()
            .find(|r| r.token == token)
            .cloned()
    }

    fn accept(
        &self,
        job: &Job,
        display: ColorDisplay,
    ) -> Result<(Arc<Revision>, bool), (StatusCode, &'static str)> {
        let mut store = self.store.lock().expect("display revisions");
        if store.cancelled || job.cancel.load(Ordering::Relaxed) {
            return Err((
                StatusCode::GONE,
                "mosaic was evicted; its display revisions are cancelled",
            ));
        }
        if let Some(revision) = store.revisions.back().filter(|r| r.display == display) {
            let mut artifacts = revision.artifacts.lock().expect("display artifacts");
            // Reposting identical immutable settings retries a failed preview.
            if artifacts.preview.state == ArtifactState::Failed {
                artifacts.preview.state = ArtifactState::Queued;
                artifacts.preview.error = None;
                artifacts.preview.completed_rows = 0;
            }
            return Ok((Arc::clone(revision), false));
        }
        let channels = job
            .lock()
            .raw_metadata
            .as_ref()
            .ok_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                "final Color metadata is not available",
            ))?
            .channels
            .clone();
        if let Some(previous) = store.revisions.back() {
            previous.supersede();
        }
        let token = format!("d{}", store.next);
        store.next += 1;
        let revision = Arc::new(Revision::new(job, token, display, channels));
        store.revisions.push_back(Arc::clone(&revision));
        while store.revisions.len() > 2 {
            store.revisions.pop_front();
        }
        self.notify.notify_waiters();
        Ok((revision, true))
    }
}

fn kick_preview(job: &Arc<Job>) {
    let mut store = job
        .display_revisions
        .store
        .lock()
        .expect("display revisions");
    if store.preview_worker {
        return;
    }
    store.preview_worker = true;
    tokio::spawn(preview_worker(Arc::downgrade(job)));
}

async fn preview_worker(weak: Weak<Job>) {
    loop {
        let Some(job) = weak.upgrade() else {
            return;
        };
        let revision = {
            let mut store = job
                .display_revisions
                .store
                .lock()
                .expect("display revisions");
            let revision = store.revisions.back().cloned();
            let revision = revision.filter(|r| {
                r.artifacts.lock().expect("display artifacts").preview.state
                    == ArtifactState::Queued
            });
            let Some(revision) = revision else {
                store.preview_worker = false;
                return;
            };
            revision
                .artifacts
                .lock()
                .expect("display artifacts")
                .preview
                .state = ArtifactState::Rendering;
            revision
        };
        let worker_revision = Arc::clone(&revision);
        let notify = Arc::clone(&job.display_revisions.notify);
        let result = tokio::task::spawn_blocking(move || render_preview(&job, &worker_revision))
            .await
            .map_err(|e| format!("display preview worker failed: {e}"))
            .and_then(|r| r);
        {
            let mut artifacts = revision.artifacts.lock().expect("display artifacts");
            if revision.current.load(Ordering::Acquire) {
                match result {
                    Ok(bytes) => {
                        artifacts.preview.state = ArtifactState::Ready;
                        artifacts.preview.bytes = Some(bytes.len() as u64);
                        artifacts.preview_bytes = Some(bytes);
                    }
                    Err(error) => {
                        artifacts.preview.state = ArtifactState::Failed;
                        artifacts.preview.error = Some(error);
                    }
                }
            }
        }
        notify.notify_waiters();
    }
}

fn kick_full(job: &Arc<Job>) {
    let mut store = job
        .display_revisions
        .store
        .lock()
        .expect("display revisions");
    if store.full_worker {
        return;
    }
    store.full_worker = true;
    tokio::spawn(full_worker(
        Arc::downgrade(job),
        Arc::clone(&job.display_revisions.cache),
        Arc::clone(&job.display_revisions.notify),
    ));
}

async fn full_worker(weak: Weak<Job>, cache: Arc<DisplayCache>, notify: Arc<Notify>) {
    loop {
        // Enable before checking the preview to avoid missing its completion.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let Some(job) = weak.upgrade() else {
            return;
        };
        let preview_pending = {
            let mut store = job
                .display_revisions
                .store
                .lock()
                .expect("display revisions");
            let pending = store.revisions.back().map(|r| {
                let artifacts = r.artifacts.lock().expect("display artifacts");
                (
                    artifacts.full.state == ArtifactState::Queued,
                    matches!(
                        artifacts.preview.state,
                        ArtifactState::Queued | ArtifactState::Rendering
                    ),
                )
            });
            match pending {
                Some((true, preview_pending)) => preview_pending,
                _ => {
                    store.full_worker = false;
                    return;
                }
            }
        };
        drop(job);
        if preview_pending {
            notified.await;
            continue;
        }
        // Waiting tasks hold a Weak<Job>, not the potentially huge science field.
        let slot = Arc::clone(&cache.full_slot);
        let permit = tokio::select! {
            permit = slot.acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    if let Some(job) = weak.upgrade() {
                        job.display_revisions
                            .store
                            .lock()
                            .expect("display revisions")
                            .full_worker = false;
                    }
                    return;
                }
            },
            // An evicted job or newer revision must not leave a stale semaphore
            // waiter behind while another job produces a long full image.
            _ = notified => continue,
        };
        let Some(job) = weak.upgrade() else {
            return;
        };
        let revision = {
            let mut store = job
                .display_revisions
                .store
                .lock()
                .expect("display revisions");
            let revision = store.revisions.back().cloned();
            let revision = revision.filter(|r| {
                r.artifacts.lock().expect("display artifacts").full.state == ArtifactState::Queued
            });
            let Some(revision) = revision else {
                store.full_worker = false;
                return;
            };
            let mut artifacts = revision.artifacts.lock().expect("display artifacts");
            if matches!(
                artifacts.preview.state,
                ArtifactState::Queued | ArtifactState::Rendering
            ) {
                continue;
            }
            artifacts.full.state = ArtifactState::Rendering;
            drop(artifacts);
            revision
        };
        let worker_cache = Arc::clone(&cache);
        let worker_revision = Arc::clone(&revision);
        let result =
            tokio::task::spawn_blocking(move || worker_cache.render_full(&job, &worker_revision))
                .await
                .map_err(|e| format!("full display worker failed: {e}"))
                .and_then(|r| r);
        if let Err(error) = result.and_then(|file| cache.publish(&revision, file)) {
            let mut artifacts = revision.artifacts.lock().expect("display artifacts");
            if revision.current.load(Ordering::Acquire) {
                artifacts.full.state = ArtifactState::Failed;
                artifacts.full.error = Some(error);
            }
        }
        drop(permit);
        notify.notify_waiters();
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    no_store(
        (
            status,
            Json(ApiError {
                error: message.into(),
            }),
        )
            .into_response(),
    )
}

// Route helpers return the finished response directly; boxing would only add an allocation.
#[allow(clippy::result_large_err)]
fn job_of(jobs: &Jobs, id: &str) -> Result<Arc<Job>, Response> {
    let job = jobs.find(id).ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            format!("no mosaic {id}; it was evicted or never existed"),
        )
    })?;
    job.touch_activity();
    if job.recipe.display != Display::Color {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "display revisions require a Color mosaic",
        ));
    }
    if job.cancel.load(Ordering::Relaxed) {
        return Err(error(StatusCode::GONE, "mosaic was evicted"));
    }
    raw_state(job.state()).map_err(|status| {
        error(
            status,
            if status == StatusCode::CONFLICT {
                "mosaic science is not finished"
            } else {
                "mosaic science failed or was cancelled"
            },
        )
    })?;
    Ok(job)
}

// Route helpers return the finished response directly; boxing would only add an allocation.
#[allow(clippy::result_large_err)]
fn revision_of(job: &Job, token: &str) -> Result<Arc<Revision>, Response> {
    job.display_revisions.find(token).ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "display revision was evicted or never existed",
        )
    })
}

fn artifact_error(artifact: &DisplayArtifact) -> Option<Response> {
    match artifact.state {
        ArtifactState::Ready => None,
        ArtifactState::Failed => Some(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            artifact
                .error
                .clone()
                .unwrap_or_else(|| "display producer failed".into()),
        )),
        ArtifactState::Superseded | ArtifactState::Evicted => Some(error(
            StatusCode::GONE,
            "display artifact was superseded or evicted",
        )),
        _ => Some(error(StatusCode::CONFLICT, "display artifact is not ready")),
    }
}

fn png_response(body: Body, bytes: u64) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CONTENT_LENGTH, bytes.to_string())
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(body)
        .expect("display PNG response")
}

#[utoipa::path(post, path = "/api/v1/mosaic/{id}/display-revisions",
    operation_id = "color_display_revision_create",
    params(("id" = String, Path, description = "Completed Color mosaic id")), request_body = ColorDisplay,
    responses((status = 201, body = DisplayManifest), (status = 200, body = DisplayManifest),
        (status = 400, body = ApiError), (status = 404, body = ApiError), (status = 409, body = ApiError),
        (status = 410, body = ApiError), (status = 500, body = ApiError)))]
pub(crate) async fn create(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path(id): Path<String>,
    body: Result<Json<ColorDisplay>, JsonRejection>,
) -> Response {
    let job = match job_of(&jobs, &id) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let display = match body {
        Ok(Json(display)) => display,
        Err(rejection) => return error(StatusCode::BAD_REQUEST, rejection.body_text()),
    };
    let display = match display.normalize() {
        Ok(display) => display,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let (revision, fresh) = match job.display_revisions.accept(&job, display) {
        Ok(result) => result,
        Err((status, message)) => return error(status, message),
    };
    kick_preview(&job);
    no_store(
        (
            if fresh {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(revision.manifest(&job)),
        )
            .into_response(),
    )
}

#[utoipa::path(get, path = "/api/v1/mosaic/{id}/display-revisions/{revision}/status",
    operation_id = "color_display_revision_status",
    params(("id" = String, Path), ("revision" = String, Path)),
    responses((status = 200, body = DisplayManifest), (status = 400, body = ApiError),
        (status = 404, body = ApiError), (status = 409, body = ApiError), (status = 410, body = ApiError)))]
pub(crate) async fn status(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    let job = match job_of(&jobs, &id) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let revision = match revision_of(&job, &token) {
        Ok(revision) => revision,
        Err(response) => return response,
    };
    no_store(Json(revision.manifest(&job)).into_response())
}

#[utoipa::path(get, path = "/api/v1/mosaic/{id}/display-revisions/{revision}/preview.png",
    operation_id = "color_display_revision_preview",
    params(("id" = String, Path), ("revision" = String, Path)),
    responses((status = 200, content_type = "image/png", description = "Bounded PNG using frozen full-image tone"),
        (status = 400, body = ApiError), (status = 404, body = ApiError), (status = 409, body = ApiError),
        (status = 410, body = ApiError), (status = 500, body = ApiError)))]
pub(crate) async fn preview(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    let job = match job_of(&jobs, &id) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let revision = match revision_of(&job, &token) {
        Ok(revision) => revision,
        Err(response) => return response,
    };
    let artifacts = revision.artifacts.lock().expect("display artifacts");
    if !revision.current.load(Ordering::Acquire) || job.cancel.load(Ordering::Relaxed) {
        return error(
            StatusCode::GONE,
            "display revision was superseded or its mosaic was evicted",
        );
    }
    if let Some(response) = artifact_error(&artifacts.preview) {
        return response;
    }
    let Some(bytes) = &artifacts.preview_bytes else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "ready preview is missing",
        );
    };
    png_response(Body::from(bytes.as_ref().to_owned()), bytes.len() as u64)
}

#[utoipa::path(post, path = "/api/v1/mosaic/{id}/display-revisions/{revision}/full",
    operation_id = "color_display_revision_full_request",
    params(("id" = String, Path), ("revision" = String, Path)),
    responses((status = 202, body = DisplayManifest), (status = 200, body = DisplayManifest),
        (status = 400, body = ApiError), (status = 404, body = ApiError), (status = 409, body = ApiError), (status = 410, body = ApiError)))]
pub(crate) async fn request_full(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    let job = match job_of(&jobs, &id) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let revision = match revision_of(&job, &token) {
        Ok(revision) => revision,
        Err(response) => return response,
    };
    // Serialize with new revisions so a superseded artifact cannot be requeued.
    let store = job
        .display_revisions
        .store
        .lock()
        .expect("display revisions");
    if store.cancelled || job.cancel.load(Ordering::Relaxed) {
        return error(StatusCode::GONE, "mosaic was evicted");
    }
    if !revision.current.load(Ordering::Acquire) {
        return error(
            StatusCode::CONFLICT,
            "display revision is no longer current",
        );
    }
    let mut artifacts = revision.artifacts.lock().expect("display artifacts");
    let queued = matches!(
        artifacts.full.state,
        ArtifactState::NotRequested | ArtifactState::Failed | ArtifactState::Evicted
    );
    if queued {
        artifacts.full.state = ArtifactState::Queued;
        artifacts.full.error = None;
        artifacts.full.bytes = None;
        artifacts.full.completed_rows = 0;
    }
    drop(artifacts);
    drop(store);
    kick_full(&job);
    no_store(
        (
            if queued {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            Json(revision.manifest(&job)),
        )
            .into_response(),
    )
}

#[utoipa::path(get, path = "/api/v1/mosaic/{id}/display-revisions/{revision}/full.png",
    operation_id = "color_display_revision_full_png",
    params(("id" = String, Path), ("revision" = String, Path)),
    responses((status = 200, content_type = "image/png", description = "Completed native RGB8 PNG streamed from an opened file"),
        (status = 400, body = ApiError), (status = 404, body = ApiError), (status = 409, body = ApiError),
        (status = 410, body = ApiError), (status = 500, body = ApiError)))]
pub(crate) async fn full(
    Extension(jobs): Extension<Arc<Jobs>>,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    let job = match job_of(&jobs, &id) {
        Ok(job) => job,
        Err(response) => return response,
    };
    let revision = match revision_of(&job, &token) {
        Ok(revision) => revision,
        Err(response) => return response,
    };
    let published = {
        let artifacts = revision.artifacts.lock().expect("display artifacts");
        if let Some(response) = artifact_error(&artifacts.full) {
            return response;
        }
        let Some(file) = artifacts.file.clone() else {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ready full PNG is missing",
            );
        };
        file
    };
    let file = match tokio::fs::File::open(&published.path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return missing_file(&revision, &published);
        }
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not open display PNG: {e}"),
            )
        }
    };
    if !revision.current.load(Ordering::Acquire) || job.cancel.load(Ordering::Relaxed) {
        return error(
            StatusCode::GONE,
            "display revision was superseded or its mosaic was evicted",
        );
    }
    let bytes = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read display PNG length: {e}"),
            )
        }
    };
    if bytes != published.bytes {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "published display PNG length changed",
        );
    }
    job.display_revisions.cache.touch(&published.path);
    png_response(
        Body::from_stream(ReaderStream::with_capacity(file, 64 * 1024)),
        bytes,
    )
}

fn missing_file(revision: &Revision, published: &Arc<PublishedFile>) -> Response {
    let mut artifacts = revision.artifacts.lock().expect("display artifacts");
    // A delayed GET may refer to an evicted pathname after this same immutable
    // revision has regenerated. Never invalidate the replacement file.
    if artifacts
        .file
        .as_ref()
        .is_some_and(|file| Arc::ptr_eq(file, published))
    {
        artifacts.full.state = ArtifactState::Evicted;
        artifacts.full.bytes = None;
        artifacts.full.completed_rows = 0;
        artifacts.file = None;
    }
    error(
        StatusCode::GONE,
        "display PNG was evicted; request the current full artifact again",
    )
}

#[cfg(test)]
mod tests;
