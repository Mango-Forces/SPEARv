//! Catalog rows as markers. The server places each row on the PNG grid, so the
//! browser draws what it receives and does no sky math.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use spearv_core::{sky_to_png_pixel_rect, resolve_fov, ApiError, CatalogRow, Context, Error, Extension, SkyRegion, SourceId};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// Same limit as the render routes, so a marker view is always a renderable view.
/// A catalog source may impose a lower limit of its own (WISE stops at 4°).
const MAX_FOV_DEG: f64 = 10.0;
/// Enough markers to fill a 512 pixel view. The brightest are the ones kept.
const MAX_ROWS: usize = 5000;

pub struct Overlay;

impl Extension for Overlay {
    fn id(&self) -> &'static str {
        "overlay"
    }

    fn routes(&self) -> OpenApiRouter<Arc<Context>> {
        OpenApiRouter::new().routes(routes!(catalog))
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CatalogQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov_l: f64,
    pub fov_h: f64,
    /// Moves sources by proper motion when the catalog has it. Without it, rows
    /// are at the catalog's own epoch.
    pub epoch_mjd: Option<f64>,
}

#[derive(Serialize, ToSchema)]
pub struct CatalogBody {
    pub source: String,
    pub epoch_mjd: Option<f64>,
    /// True when more rows fall in the view than were returned. The brightest are kept.
    pub truncated: bool,
    pub rows: Vec<MarkerRow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct MarkerRow {
    pub id: String,
    pub ra: f64,
    pub dec: f64,
    /// The catalog's own band: G for Gaia, W1 for WISE. Do not compare across catalogs.
    pub mag: Option<f64>,
    /// PNG column on the render grid, origin at the left.
    pub x: f64,
    /// PNG row on the render grid, origin at the top.
    pub y: f64,
}

#[utoipa::path(
    get,
    path = "/api/v1/catalog/{source}",
    params(("source" = String, Path, description = "gaia or wise"), CatalogQuery),
    responses(
        (status = 200, body = CatalogBody),
        (status = 400, body = ApiError),
        (status = 404, description = "No such catalog, or it holds no data for this view", body = ApiError),
        (status = 502, description = "The catalog read failed", body = ApiError),
        (status = 504, description = "The catalog read exceeded its deadline", body = ApiError)
    )
)]
async fn catalog(
    State(ctx): State<Arc<Context>>,
    Path(source): Path<String>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<CatalogBody>, Error> {
    validate(&query)?;
    let catalog = ctx
        .catalogs
        .get(&SourceId(source.clone()))
        .ok_or_else(|| Error::NotFound(format!("catalog {source} is not available")))?;
    let region = SkyRegion::rect(query.ra, query.dec, query.fov_l, query.fov_h);
    let found = catalog.cone(&region, query.epoch_mjd).await.map_err(|err| match err {
        Error::OutsideExposure => Error::NotFound(format!("catalog {source} holds no data for this view")),
        other => other,
    })?;
    let (rows, truncated) = markers(found, &region, MAX_ROWS);
    Ok(Json(CatalogBody { source, epoch_mjd: query.epoch_mjd, truncated, rows }))
}

fn validate(query: &CatalogQuery) -> Result<(), Error> {
    if !(0.0..=360.0).contains(&query.ra) || !(-90.0..=90.0).contains(&query.dec) {
        return Err(Error::InvalidSky);
    }
    resolve_fov(query.fov_l, query.fov_h, MAX_FOV_DEG)?;
    if query.epoch_mjd.is_some_and(|mjd| !mjd.is_finite()) {
        return Err(Error::Message("epoch_mjd must be a finite number".into()));
    }
    Ok(())
}

/// Sources may return more than the view, such as the circle through its corners,
/// so this clip applies to every source. Rows on the render grid, brightest first, cut to `limit`. A row with no
/// magnitude sorts last. Ties break on id, so the same view gives the same rows.
fn markers(rows: Vec<CatalogRow>, region: &SkyRegion, limit: usize) -> (Vec<MarkerRow>, bool) {
    let mut placed: Vec<MarkerRow> = rows
        .into_iter()
        .filter_map(|row| {
            let (x, y) = sky_to_png_pixel_rect(row.ra, row.dec, region.ra, region.dec, region.fov_l_deg, region.fov_h_deg)?;
            Some(MarkerRow { id: row.id, ra: row.ra, dec: row.dec, mag: row.mag, x, y })
        })
        .collect();
    placed.sort_by(|a, b| {
        a.mag
            .unwrap_or(f64::INFINITY)
            .total_cmp(&b.mag.unwrap_or(f64::INFINITY))
            .then_with(|| a.id.cmp(&b.id))
    });
    let truncated = placed.len() > limit;
    placed.truncate(limit);
    (placed, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use async_trait::async_trait;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use spearv_core::{png_pixel_to_sky_rect, Cache, CatalogSource, Result};
    use tower::ServiceExt;

    const REGION: SkyRegion = SkyRegion::square(58.2783, 26.1398, 0.1);

    fn row(id: &str, ra: f64, dec: f64, mag: Option<f64>) -> CatalogRow {
        CatalogRow { id: id.into(), ra, dec, mag }
    }

    #[test]
    fn rectangular_markers_clip_the_square_envelope() {
        let region = SkyRegion::rect(REGION.ra, REGION.dec, 0.2, 0.1);
        let inside = png_pixel_to_sky_rect(100, 40, region.ra, region.dec, 0.2, 0.1).unwrap();
        let outside = png_pixel_to_sky_rect(58, 0, region.ra, region.dec, 0.2, 0.2).unwrap();
        let (rows, truncated) = markers(vec![row("in", inside.0, inside.1, Some(10.0)), row("out", outside.0, outside.1, Some(1.0))], &region, 1);
        assert!(!truncated);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "in");
        assert!((rows[0].x - 100.5).abs() < 1e-6 && (rows[0].y - 40.5).abs() < 1e-6);
    }

    #[test]
    fn a_row_lands_on_the_png_pixel_of_its_sky_position() {
        let (ra, dec) = png_pixel_to_sky_rect(12, 40, REGION.ra, REGION.dec, REGION.fov_l_deg, REGION.fov_h_deg).unwrap();
        let (rows, truncated) = markers(vec![row("a", ra, dec, Some(10.0))], &REGION, 10);
        assert!(!truncated);
        // Image space puts the centre of pixel index 12 at 12.5.
        assert!((rows[0].x - 12.5).abs() < 1e-6 && (rows[0].y - 40.5).abs() < 1e-6, "{rows:?}");
    }

    #[test]
    fn rows_outside_the_grid_are_omitted_and_do_not_count_as_truncation() {
        let rows = vec![row("in", REGION.ra, REGION.dec, Some(10.0)), row("out", 60.0, 26.1, Some(1.0))];
        let (rows, truncated) = markers(rows, &REGION, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "in");
        assert!(!truncated);
    }

    #[test]
    fn the_limit_keeps_the_brightest_and_says_so() {
        let rows = vec![
            row("no-mag", REGION.ra, REGION.dec, None),
            row("faint", REGION.ra, REGION.dec, Some(19.0)),
            row("bright", REGION.ra, REGION.dec, Some(8.0)),
        ];
        let (rows, truncated) = markers(rows, &REGION, 2);
        assert!(truncated);
        assert_eq!(rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["bright", "faint"]);
    }

    /// Stands in for any source. Only the error's kind matters.
    struct Fixed(Result<Vec<CatalogRow>>);

    #[async_trait]
    impl CatalogSource for Fixed {
        fn id(&self) -> SourceId {
            SourceId::new("gaia")
        }

        async fn cone(&self, _: &SkyRegion, _: Option<f64>) -> Result<Vec<CatalogRow>> {
            match &self.0 {
                Ok(rows) => Ok(rows.clone()),
                Err(Error::Upstream(text)) => Err(Error::Upstream(text.clone())),
                Err(_) => Err(Error::OutsideExposure),
            }
        }
    }

    async fn get(source: Fixed, uri: &str) -> (StatusCode, String) {
        let mut catalogs: HashMap<_, Arc<dyn CatalogSource>> = HashMap::new();
        let source: Arc<dyn CatalogSource> = Arc::new(source);
        catalogs.insert(SourceId::new("gaia"), source.clone());
        catalogs.insert(SourceId::new("wise"), source);
        let mut ctx = Context::new(Arc::new(Cache::new()));
        ctx.catalogs = catalogs;
        let ctx = Arc::new(ctx);
        let (router, _) = Overlay.routes().split_for_parts();
        let response = router
            .with_state(ctx)
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_route_returns_the_shared_contract() {
        let source = Fixed(Ok(vec![row("7", REGION.ra, REGION.dec, Some(12.3))]));
        let (status, body) = get(source, "/api/v1/catalog/gaia?ra=58.2783&dec=26.1398&fov_l=0.1&fov_h=0.1&epoch_mjd=61000").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.starts_with(r#"{"source":"gaia","epoch_mjd":61000.0,"truncated":false,"rows":[{"id":"7","#), "{body}");
        assert!(body.contains(r#""mag":12.3,"x":29.5,"y":29.5"#), "{body}");
    }

    #[tokio::test]
    async fn an_unregistered_catalog_and_an_uncovered_view_are_404() {
        let (status, body) = get(Fixed(Ok(vec![])), "/api/v1/catalog/ukidss?ra=1&dec=1&fov_l=0.1&fov_h=0.1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("catalog ukidss is not available"), "{body}");
        let (status, body) = get(Fixed(Err(Error::OutsideExposure)), "/api/v1/catalog/gaia?ra=1&dec=1&fov_l=0.1&fov_h=0.1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("holds no data"), "{body}");
    }

    #[tokio::test]
    async fn a_source_that_returns_its_corner_circle_is_clipped_to_the_square() {
        // On the circle through the corners, due east: inside the circle, off the grid.
        let east = REGION.ra + 0.0707 / REGION.dec.to_radians().cos();
        let rows = vec![row("edge", east, REGION.dec, Some(5.0)), row("w1", REGION.ra, REGION.dec, Some(9.5))];
        let (status, body) = get(Fixed(Ok(rows)), "/api/v1/catalog/wise?ra=58.2783&dec=26.1398&fov_l=0.1&fov_h=0.1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.starts_with(r#"{"source":"wise","epoch_mjd":null,"truncated":false,"rows":[{"id":"w1","#), "{body}");
        assert!(!body.contains("edge"), "{body}");
    }

    #[tokio::test]
    async fn a_failed_read_is_a_502() {
        let source = Fixed(Err(Error::Upstream("read exceeded".into())));
        let (status, body) = get(source, "/api/v1/catalog/wise?ra=1&dec=1&fov_l=0.1&fov_h=0.1").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    }

    #[tokio::test]
    async fn a_ten_degree_view_is_accepted() {
        let (status, body) = get(Fixed(Ok(vec![])), "/api/v1/catalog/gaia?ra=58.2783&dec=26.1398&fov_l=10&fov_h=10").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn bad_views_are_400() {
        for query in ["ra=400&dec=0&fov_l=0.1&fov_h=0.1", "ra=1&dec=95&fov_l=0.1&fov_h=0.1", "ra=1&dec=1&fov_l=0&fov_h=0", "ra=1&dec=1&fov_l=10.5&fov_h=10.5", "ra=NaN&dec=1&fov_l=0.1&fov_h=0.1", "ra=1&dec=1&fov_l=0.1&fov_h=0.1&epoch_mjd=inf"] {
            let (status, body) = get(Fixed(Ok(vec![])), &format!("/api/v1/catalog/gaia?{query}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        }
    }
}
