//! The HTTP API. Nobody else opens the database; they ask here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use spearv_index::store::Store;
use spearv_index::*;

use crate::crawl::Crawler;

/// The release summary scans an index of every row. During a full crawl the
/// generation moves constantly, so it is recomputed at most this often.
const SUMMARY_MAX_AGE: Duration = Duration::from_secs(30);

pub struct AppState {
    store: Arc<Store>,
    crawler: Option<Arc<Crawler>>,
    summary: Mutex<Option<(i64, Instant, Vec<ReleaseSummary>)>>,
}

impl AppState {
    pub fn new(store: Arc<Store>, crawler: Option<Arc<Crawler>>) -> Arc<Self> {
        Arc::new(Self { store, crawler, summary: Mutex::new(None) })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/coverage", get(coverage))
        .route("/passes", get(passes))
        .route("/releases", get(releases))
        .route("/health", get(health))
        .route("/generation", get(generation))
        .with_state(state)
}

enum Failure {
    BadQuery(String),
    Internal(String),
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::BadQuery(error) => (StatusCode::BAD_REQUEST, error),
            Self::Internal(error) => {
                eprintln!("[http] {error}");
                (StatusCode::INTERNAL_SERVER_ERROR, error)
            }
        };
        (status, Json(ApiError { error })).into_response()
    }
}

fn query(raw: Result<Query<CoverageQuery>, QueryRejection>) -> Result<CoverageQuery, Failure> {
    let Query(query) = raw.map_err(|err| Failure::BadQuery(err.body_text()))?;
    query.validate().map_err(Failure::BadQuery)?;
    Ok(query)
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| Failure::Internal(err.to_string()))?
        .map_err(Failure::Internal)
}

async fn coverage(
    State(state): State<Arc<AppState>>,
    raw: Result<Query<CoverageQuery>, QueryRejection>,
) -> Result<Json<CoverageResponse>, Failure> {
    let query = query(raw)?;
    let store = Arc::clone(&state.store);
    // Read the generation first: if a crawl writes in between, the answer is
    // newer than its label, never older, so a client never caches stale rows.
    blocking(move || {
        let generation = store.generation()?;
        Ok(CoverageResponse { generation, exposures: store.coverage(&query)? })
    })
    .await
    .map(Json)
}

async fn passes(
    State(state): State<Arc<AppState>>,
    raw: Result<Query<CoverageQuery>, QueryRejection>,
) -> Result<Json<PassesResponse>, Failure> {
    let query = query(raw)?;
    let store = Arc::clone(&state.store);
    blocking(move || {
        let generation = store.generation()?;
        Ok(PassesResponse { generation, passes: store.passes(&query)? })
    })
    .await
    .map(Json)
}

async fn summary(state: &Arc<AppState>) -> Result<(i64, Vec<ReleaseSummary>), Failure> {
    let store = Arc::clone(&state.store);
    let generation = blocking(move || store.generation()).await?;
    if let Ok(cached) = state.summary.lock() {
        if let Some((seen, at, releases)) = cached.as_ref() {
            if *seen == generation || at.elapsed() < SUMMARY_MAX_AGE {
                return Ok((*seen, releases.clone()));
            }
        }
    }
    let store = Arc::clone(&state.store);
    let releases = blocking(move || store.releases()).await?;
    if let Ok(mut cached) = state.summary.lock() {
        *cached = Some((generation, Instant::now(), releases.clone()));
    }
    Ok((generation, releases))
}

async fn releases(State(state): State<Arc<AppState>>) -> Result<Json<ReleasesResponse>, Failure> {
    let (generation, releases) = summary(&state).await?;
    Ok(Json(ReleasesResponse { generation, releases }))
}

async fn health(State(state): State<Arc<AppState>>) -> Result<Json<Health>, Failure> {
    let (_, releases) = summary(&state).await?;
    let store = Arc::clone(&state.store);
    let (generation, last_crawl) = blocking(move || Ok((store.generation()?, store.last_crawl()?))).await?;
    let current = state.crawler.as_ref().and_then(|crawler| crawler.current());
    let degraded = last_crawl.as_ref().is_some_and(|crawl| crawl.error.is_some() || crawl.prefixes_failed > 0);
    let status = if current.is_some() {
        "crawling"
    } else if degraded {
        "degraded"
    } else {
        "ok"
    };
    Ok(Json(Health {
        status: status.into(),
        generation,
        exposures: releases.iter().map(|release| release.exposures).sum(),
        current,
        last_crawl,
    }))
}

async fn generation(State(state): State<Arc<AppState>>) -> Result<Json<GenerationResponse>, Failure> {
    let store = Arc::clone(&state.store);
    Ok(Json(GenerationResponse { generation: blocking(move || store.generation()).await? }))
}

#[cfg(test)]
mod tests {
    use spearv_index::client::{ClientError, IndexClient};

    use super::*;
    use crate::bucket::Object;
    use crate::row::{parse_key, row_from_header};
    use crate::testing::{fits_prefix, key, TempDb};

    struct Served {
        client: IndexClient,
        store: Arc<Store>,
        _db: TempDb,
    }

    async fn serve() -> Served {
        let (db, store) = TempDb::open("http");
        let mut rows: Vec<ExposureRow> = Vec::new();
        for (key, flags, ra) in [
            (key("qr2", "2025W34_2B", "l2b-v20-2025-267", 2, 1), "all_sky", 56.8),
            (key("qr2", "2025W35_1A", "l2b-v20-2025-270", 2, 1), "deep_north", 56.8),
            (key("qr3", "2026W40_2A", "l2b-v27-2026-285", 1, 1), "all_sky", 56.8),
            (key("qr3", "2026W40_2A", "l2b-v27-2026-285", 1, 2), "all_sky", 150.0),
        ] {
            let card = format!("HIERARCH SPS_FLAGS = '{flags}' / survey");
            let prefix = fits_prefix(parse_key(&key).unwrap().detector, ra, 24.0, &[card.as_str()]);
            rows.push(row_from_header(&Object { key, etag: Some("\"e\"".into()), size: Some(1) }, &prefix).await.unwrap());
        }
        store.upsert(&rows).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(AppState::new(Arc::clone(&store), None));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Served { client: IndexClient::new(format!("http://{addr}/")).unwrap(), store, _db: db }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_client_reads_coverage_passes_releases_and_generation() {
        let s = serve().await;
        let all = s.client.coverage(&CoverageQuery::view(56.8, 24.0, 0.2)).await.unwrap();
        assert_eq!(all.exposures.len(), 3);
        assert_eq!(all.generation, s.store.generation().unwrap());
        assert!(all.exposures.windows(2).all(|w| w[0].mjd_avg <= w[1].mjd_avg));
        let deep = CoverageQuery { release: Some("qr2".into()), survey: Some(Survey::Deep), ..CoverageQuery::view(56.8, 24.0, 0.2) };
        let found = s.client.coverage(&deep).await.unwrap().exposures;
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].release.as_str(), found[0].survey), ("qr2", Survey::Deep));
        assert_eq!(found[0].wcs.crval, [56.8, 24.0]);
        // Detector 1 is 0.75-1.1 µm, detector 2 is 1.1-1.65 µm.
        let band = CoverageQuery { wl_min: Some(0.8), wl_max: Some(0.9), ..CoverageQuery::view(56.8, 24.0, 0.2) };
        let found = s.client.coverage(&band).await.unwrap().exposures;
        assert!(!found.is_empty() && found.iter().all(|row| row.detector == 1), "{found:?}");

        let passes = s.client.passes(&CoverageQuery::view(56.8, 24.0, 0.2)).await.unwrap().passes;
        assert_eq!(passes.iter().map(|p| p.exposures).sum::<usize>(), 3);

        let releases = s.client.releases().await.unwrap().releases;
        assert_eq!(releases.iter().map(|r| (r.release.as_str(), r.exposures, r.deep)).collect::<Vec<_>>(), [("qr2", 2, 1), ("qr3", 2, 0)]);
        assert_eq!(s.client.generation_id().await.unwrap(), s.store.generation().unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bad_query_is_a_400_with_a_reason() {
        let s = serve().await;
        let out_of_sky = s.client.coverage(&CoverageQuery::view(10.0, 95.0, 1.0)).await.unwrap_err();
        assert!(matches!(&out_of_sky, ClientError::BadQuery(text) if text.contains("dec")), "{out_of_sky}");
        // A missing parameter is refused the same way, not with a plain-text body.
        let missing = reqwest::get(format!("{}/coverage?ra=1&dec=1", s.client.base())).await.unwrap();
        assert_eq!(missing.status(), 400);
        let body: ApiError = missing.json().await.unwrap();
        assert!(body.error.contains("fov"), "{body:?}");
        let survey = reqwest::get(format!("{}/coverage?ra=1&dec=1&fov=1&survey=shallow", s.client.base())).await.unwrap();
        assert_eq!(survey.status(), 400);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_reports_rows_and_the_last_crawl() {
        let s = serve().await;
        let health = s.client.health_report().await.unwrap();
        assert_eq!((health.status.as_str(), health.exposures), ("ok", 4));
        assert!(health.last_crawl.is_none() && health.current.is_none());
        let mut record = s.store.crawl_start(CrawlKind::Diff, 1).unwrap();
        record.finished_ms = Some(2);
        record.prefixes_failed = 1;
        s.store.crawl_save(&record).unwrap();
        let health = s.client.health_report().await.unwrap();
        assert_eq!(health.status, "degraded");
        assert_eq!(health.last_crawl, Some(record));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_client_serves_as_the_core_index_service() {
        let s = serve().await;
        let index: &dyn spearv_core::IndexService = &s.client;
        assert_eq!(index.generation().await.unwrap(), s.store.generation().unwrap().to_string());
        let health = index.health().await.unwrap();
        assert_eq!((health["status"].as_str(), health["exposures"].as_i64()), (Some("ok"), Some(4)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_service_is_unavailable_not_a_bad_query() {
        let client = IndexClient::new("http://127.0.0.1:9").unwrap();
        let err = client.generation_id().await.unwrap_err();
        assert!(matches!(err, ClientError::Unavailable(_)), "{err}");
        let err = spearv_core::IndexService::generation(&client).await.unwrap_err();
        assert!(matches!(err, spearv_core::Error::Upstream(_)), "{err}");
    }
}
