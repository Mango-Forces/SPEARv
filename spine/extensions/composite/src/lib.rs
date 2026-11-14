mod mosaic;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use spearv_core::math::{
    spawn_math, MathControl, MathError, NativeInput, Product, Values, MaskPolicy, TargetGrid,
};
use spearv_core::{
    cluster_visits, query_pairs, render_grid_size, visit_index, ApiError, Context, Cutout, Error, Extension,
    Filter, FlagBits, ReleaseId, SourceId, TanGrid, ViewQuery, BANDS, PIXEL_SCALE_DEG,
};
use tokio::sync::Semaphore;

use crate::mosaic::{tile_of_grid, TILE};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// The image source this extension composes.
const SPHEREX: &str = "spherex";

pub const MAX_FOV_DEG: f64 = 30.0;
const RENDER_SECONDS: u64 = 600;
const READ_CONCURRENCY: usize = 8;

/// No-data pixels are nearly black, so the usable field is what reads first.
/// A bright no-data colour turned a partial footprint into the loudest thing
/// on the page.
const NODATA: [u8; 3] = [24, 28, 36];

/// Display gamma shared by `/color.png` and the mosaic colour tiles. It is the
/// only tone applied to physical values; neither path stretches encoded bytes.
const DISPLAY_GAMMA: f32 = 0.7;

/// A channel's black point and width, in physical units.
#[derive(Clone, Copy)]
pub(crate) struct Stretch {
    pub low: f32,
    pub span: f32,
}

/// The largest native window the runtime admits for one bounded call: one
/// whole 2040-pixel detector.
pub(crate) const MAX_NATIVE_PIXELS: usize = 2040 * 2040;

/// One band stacked onto a render grid. `value` is NaN where nothing landed.
pub(crate) struct Stacked {
    pub width: u32,
    pub height: u32,
    pub value: Vec<f32>,
}

pub struct Composite;

#[async_trait::async_trait]
impl Extension for Composite {
    fn id(&self) -> &'static str {
        "composite"
    }

    fn routes(&self) -> OpenApiRouter<Arc<Context>> {
        OpenApiRouter::new()
            .routes(routes!(render_png))
            .routes(routes!(color_png))
            .routes(routes!(coverage))
            .routes(routes!(releases))
            .routes(routes!(mosaic::start))
            .routes(routes!(mosaic::status))
            .routes(routes!(mosaic::raw_metadata))
            .routes(routes!(mosaic::tile))
            .routes(routes!(mosaic::display_revision::create))
            .routes(routes!(mosaic::display_revision::status))
            .routes(routes!(mosaic::display_revision::preview))
            .routes(routes!(mosaic::display_revision::request_full))
            .routes(routes!(mosaic::display_revision::full))
            .layer(axum::Extension(mosaic::shared_jobs()))
    }

    /// Stop display producers and remove only the display files this process owns.
    async fn shutdown(&self) {
        let Some(jobs) = mosaic::existing_jobs() else {
            return;
        };
        let caches = jobs.begin_display_shutdown();
        let errors = match tokio::task::spawn_blocking(move || mosaic::cleanup_display_caches(caches)).await {
            Ok(result) => result.err().unwrap_or_default(),
            Err(error) => vec![format!("display cache cleanup task failed: {error}")],
        };
        for error in errors {
            eprintln!("[display-cache] {error}");
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RenderQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov_l: f64,
    pub fov_h: f64,
    pub mode: Option<String>,
    pub dr: Option<String>,
    pub mask: Option<String>,
    #[serde(default)]
    pub pin: Vec<String>,
    /// The red channel's centre band, 0..101. `/api/v1/color.png` and the
    /// mosaic's `color=1` use it; omitted keeps 90.
    pub color_r: Option<u16>,
    /// The red channel's width in bands, 1..102, centred on `color_r`. Omitted
    /// keeps 5.
    pub color_r_width: Option<u16>,
    /// The green channel's centre band, 0..101. Omitted keeps 40.
    pub color_g: Option<u16>,
    /// The green channel's width in bands, 1..102, centred on `color_g`.
    /// Omitted keeps 5.
    pub color_g_width: Option<u16>,
    /// The blue channel's centre band, 0..101. Omitted keeps 5.
    pub color_b: Option<u16>,
    /// The blue channel's width in bands, 1..102, centred on `color_b`.
    /// Omitted keeps 5.
    pub color_b_width: Option<u16>,
}

#[derive(Serialize, ToSchema)]
pub struct ReleaseList {
    pub releases: Vec<ReleaseInfo>,
    /// The release used when `dr` is omitted. QR2 is not mixed with QR3.
    pub default_release: String,
}

#[derive(Serialize, ToSchema)]
pub struct ReleaseInfo {
    pub id: String,
    pub newest_mjd: Option<f64>,
    /// Newest planning period the watcher has seen for this release.
    pub newest_period: Option<String>,
    /// Unix epoch milliseconds when the watcher last listed the release.
    pub last_seen_ms: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct Coverage {
    pub release: String,
    pub exposures: usize,
    pub visits: Vec<Visit>,
    pub bands: Vec<BandCoverage>,
}

#[derive(Serialize, ToSchema)]
pub struct Visit {
    pub index: u16,
    /// First exposure of the visit.
    pub mjd: f64,
    /// Last exposure of the visit. `mjd..=mjd_end` is the visit's real time
    /// range; a UI selector should not pad it with a guess.
    pub mjd_end: f64,
    pub exposures: usize,
}

#[derive(Serialize, ToSchema)]
pub struct BandCoverage {
    pub index: u16,
    pub detector: u8,
    pub subchan: u8,
    pub wl_min_um: f64,
    pub center_um: f64,
    pub wl_max_um: f64,
    pub exposures: usize,
}

#[utoipa::path(
    get,
    path = "/api/v1/releases",
    responses((status = 200, body = ReleaseList))
)]
async fn releases(State(ctx): State<Arc<Context>>) -> Result<Json<ReleaseList>, Error> {
    let source = source(&ctx)?;
    let releases = source.releases().await?;
    Ok(Json(ReleaseList {
        releases: releases
            .into_iter()
            .map(|release| ReleaseInfo {
                id: release.id.0,
                newest_mjd: release.newest_mjd,
                newest_period: release.newest_period,
                last_seen_ms: release.last_seen_ms,
            })
            .collect(),
        default_release: "qr3".into(),
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/coverage",
    params(RenderQuery),
    responses(
        (status = 200, body = Coverage),
        (status = 400, body = ApiError)
    )
)]
async fn coverage(
    State(ctx): State<Arc<Context>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Coverage>, Error> {
    let view = view_of(&parse_query(raw.as_deref().unwrap_or(""))?)?;
    let exposures = source(&ctx)?
        .coverage(
            &view.region,
            &Filter {
                release: Some(ReleaseId(view.release.clone())),
                wavelength_um: None,
            },
        )
        .await?;
    let visits = cluster_visits_for(&exposures);
    let mut bands = Vec::new();
    for (index, band) in BANDS.iter().enumerate() {
        let exposures_here = exposures
            .iter()
            .filter(|exposure| {
                exposure.detector == band.detector
                    && exposure.em_max_um >= band.wl_min_um - 0.02
                    && exposure.em_min_um <= band.wl_max_um + 0.02
            })
            .count();
        bands.push(BandCoverage {
            index: index as u16,
            detector: band.detector,
            subchan: band.subchan,
            wl_min_um: band.wl_min_um,
            center_um: band.center_um,
            wl_max_um: band.wl_max_um,
            exposures: exposures_here,
        });
    }
    Ok(Json(Coverage {
        release: view.release,
        exposures: exposures.len(),
        visits,
        bands,
    }))
}

/// Noise-weighted mean on a north-up TAN grid.
///
/// Every exposure the index says overlaps the view is read, ZODI is subtracted, flagged pixels are dropped, and the rest are
/// averaged with 1/VARIANCE. A pin selects pixels by wavelength, not whole files.
/// Files whose wavelength range cannot overlap the pin are not read.
#[utoipa::path(
    get,
    path = "/api/v1/render.png",
    params(RenderQuery),
    responses(
        (status = 200, description = "Stretched composite PNG", content_type = "image/png"),
        (status = 400, description = "View parameters are invalid", body = ApiError),
        (status = 404, description = "No exposure covers this view", body = ApiError),
        (status = 502, description = "The S3 read failed", body = ApiError),
        (status = 504, description = "The render exceeded its time limit", body = ApiError)
    )
)]
async fn render_png(
    State(ctx): State<Arc<Context>>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Error> {
    let (view, grid) = render_view_of(&parse_query(raw.as_deref().unwrap_or(""))?)?;
    if view.mode == spearv_core::Mode::Delta {
        return Err(Error::Message(
            "delta is a separate image at /api/v1/delta.png, and only within one release".into(),
        ));
    }
    let result = cached_band_png(&ctx, &view).await?;
    Ok(png_response(
        result.png,
        result.hit,
        result.used,
        result.elapsed_ms,
        grid,
        result.index_hits as u32,
        result.reads,
    ))
}

struct BandPng {
    png: Vec<u8>,
    hit: bool,
    used: u32,
    elapsed_ms: u128,
    index_hits: usize,
    reads: u32,
}

async fn cached_band_png(ctx: &Arc<Context>, view: &spearv_core::View) -> Result<BandPng, Error> {
    let key = view.cache_key();
    if let Some(png) = ctx.cache.get(&key) {
        return Ok(BandPng {
            png,
            hit: true,
            used: 0,
            elapsed_ms: 0,
            index_hits: 0,
            reads: 0,
        });
    }
    let mut result = band_png(ctx, view).await?;
    ctx.cache.insert(key, result.png.clone());
    result.hit = false;
    Ok(result)
}

/// One band's cutouts, before they are stacked.
struct BandReads {
    cutouts: Vec<Cutout>,
    used: u32,
    elapsed_ms: u128,
    index_hits: usize,
    reads: u32,
}

async fn band_png(ctx: &Arc<Context>, view: &spearv_core::View) -> Result<BandPng, Error> {
    let reads = read_band(ctx, view).await?;
    let left = Duration::from_secs(RENDER_SECONDS).saturating_sub(Duration::from_millis(reads.elapsed_ms as u64));
    let view = view.clone();
    let ctx = Arc::clone(ctx);
    let png = tokio::time::timeout(left, async move {
        let stacked = stack_of(&ctx, reads.cutouts, &view).await?;
        tokio::task::spawn_blocking(move || encode(&stacked))
            .await
            .map_err(|err| Error::Message(err.to_string()))?
            .map_err(Error::Message)
    })
    .await
    .map_err(|_| Error::Deadline { seconds: RENDER_SECONDS })??;
    Ok(BandPng {
        png,
        hit: false,
        used: reads.used,
        elapsed_ms: reads.elapsed_ms,
        index_hits: reads.index_hits,
        reads: reads.reads,
    })
}

async fn read_band(ctx: &Arc<Context>, view: &spearv_core::View) -> Result<BandReads, Error> {
    let source = source(ctx)?;
    let bands = view.selected_bands();
    let (wl_min, wl_max) = band_span(&bands);
    let wavelength = if view.mode == spearv_core::Mode::Composite {
        None
    } else {
        Some((wl_min, wl_max))
    };
    let all = source
        .coverage(
            &view.region,
            &Filter {
                release: Some(ReleaseId(view.release.clone())),
                wavelength_um: None,
            },
        )
        .await?;
    let index_hits = all.len();
    let mut selected = source
        .coverage(
            &view.region,
            &Filter {
                release: Some(ReleaseId(view.release.clone())),
                wavelength_um: wavelength,
            },
        )
        .await?;
    // A visit on a pin picks one epoch. Delta filters its own exposures; this
    // is for slider and range, so a shared link can show either visit.
    if matches!(view.mode, spearv_core::Mode::Slider | spearv_core::Mode::Range) {
        if let Some(visit) = view.pins.iter().find_map(|pin| pin.visit) {
            let spans = cluster_visits(&all.iter().map(|exposure| exposure.mjd).collect::<Vec<_>>());
            selected.retain(|exposure| visit_index(exposure.mjd, &spans) == Some(visit));
        }
    }
    let reads_planned = selected.len() as u32;
    if selected.is_empty() {
        return Err(Error::OutsideExposure);
    }
    let started = std::time::Instant::now();
    let region = view.region.clone();
    let mut reads = tokio::task::JoinSet::new();
    let permits = Arc::new(Semaphore::new(READ_CONCURRENCY));
    for exposure in selected {
        let permits = Arc::clone(&permits);
        let source = Arc::clone(&source);
        let region = region.clone();
        reads.spawn(async move {
            let _permit = permits.acquire().await;
            // A pinned view can only use the rows that hold its bands, and the
            // stack rejects every other pixel anyway, so they are not read.
            match wavelength {
                Some(wavelength) => source.band_cutout(&exposure, &region, wavelength).await,
                None => source.cutout(&exposure, &region).await.map(Some),
            }
        });
    }
    let gathered = tokio::time::timeout(Duration::from_secs(RENDER_SECONDS), async {
        let mut cutouts = Vec::new();
        let mut skipped = 0;
        while let Some(joined) = reads.join_next().await {
            match joined {
                Ok(Ok(Some(cutout))) => cutouts.push(cutout),
                Ok(Ok(None)) => skipped += 1,
                _ => {}
            }
        }
        (cutouts, skipped)
    })
    .await;
    reads.abort_all();
    let (cutouts, skipped) = match gathered {
        Ok(gathered) => gathered,
        Err(_) => return Err(Error::Deadline { seconds: RENDER_SECONDS }),
    };
    if started.elapsed() >= Duration::from_secs(RENDER_SECONDS) {
        return Err(Error::Deadline { seconds: RENDER_SECONDS });
    }
    // A strip that misses the view is not a failure. It renders as no data,
    // the same as reading those rows and rejecting every pixel.
    if cutouts.is_empty() && skipped == 0 {
        return Err(Error::Upstream("every exposure read failed".into()));
    }
    Ok(BandReads {
        used: cutouts.len() as u32,
        cutouts,
        elapsed_ms: started.elapsed().as_millis(),
        index_hits,
        reads: reads_planned,
    })
}

/// Stack the cutouts a view selected through the library's forward geometry.
/// Each 256-pixel output tile owns one session; only inputs whose footprint
/// reaches that tile call `geometry`, so far-away tiles never touch Astropy.
async fn stack_of(ctx: &Arc<Context>, cutouts: Vec<Cutout>, view: &spearv_core::View) -> Result<Stacked, Error> {
    let width = render_grid_size(view.region.fov_l_deg)?;
    let height = render_grid_size(view.region.fov_h_deg)?;
    let grid = TanGrid::new_rect(view.region.ra, view.region.dec, view.region.fov_l_deg, view.region.fov_h_deg);
    let target = TargetGrid {
        ra: grid.ra,
        dec: grid.dec,
        shape: spearv_core::math::Shape { width, height },
        scale_deg: PIXEL_SCALE_DEG,
    };
    let bands = if view.mode == spearv_core::Mode::Composite { Vec::new() } else { view.selected_bands() };
    let mask = if view.mask { MaskPolicy::Composite } else { MaskPolicy::None };
    let math = ctx.math()?;
    let budget = Arc::clone(&ctx.budget);
    let prep_grid = grid.clone();
    let inputs = tokio::task::spawn_blocking(move || {
        let mut inputs = Vec::new();
        for cutout in cutouts {
            let Some(bounds) = render_bounds(&cutout, &prep_grid) else {
                continue;
            };
            let native = match NativeInput::from_cutout(&budget, MAX_NATIVE_PIXELS, &cutout, true) {
                Ok(native) => native,
                // A cutout without ZODI (or with malformed planes) has no
                // admissible physical value, so it adds nothing.
                Err(MathError::Input(_)) => continue,
                Err(error) => return Err(Error::Math(error)),
            };
            inputs.push(RenderInput { native, bits: cutout.flag_bits, bounds });
        }
        Ok::<_, Error>(inputs)
    })
    .await
    .map_err(|error| Error::Message(format!("render input copy failed: {error}")))??;
    let cols = width.div_ceil(TILE);
    let rows = height.div_ceil(TILE);
    let control = MathControl::new(Arc::new(AtomicU64::new(0)));
    let mut value = vec![f32::NAN; (width * height) as usize];
    for png_y in 0..rows {
        for x in 0..cols {
            let tile = tile_of_grid(x, png_y, width, height);
            let chosen: Vec<(NativeInput, FlagBits)> = inputs
                .iter()
                .filter(|input| tile_reaches(input.bounds, cols, height, x, png_y))
                .map(|input| (input.native.clone(), input.bits))
                .collect();
            if chosen.is_empty() {
                continue;
            }
            let session = math.session(control.clone(), tile, Product::Values)?;
            let bands = bands.clone();
            let output = spawn_math(session, move |mut session| {
                for (order, (input, bits)) in chosen.iter().enumerate() {
                    let geometry = session.geometry(&input.planes().wcs, input.planes().window, target)?;
                    session.add(order as u64, input, &geometry, *bits, mask, &bands)?;
                }
                session.finish()
            })?
            .wait()
            .await?;
            let Values::F32(plane) = &output.planes().value else {
                return Err(Error::Message("render math returned a non-f32 tile".into()));
            };
            let tile_width = tile.shape.width as usize;
            for row in 0..tile.shape.height {
                let at = ((tile.y + row) * width + tile.x) as usize;
                let from = row as usize * tile_width;
                value[at..at + tile_width].copy_from_slice(&plane[from..from + tile_width]);
            }
        }
    }
    Ok(Stacked { width, height, value })
}

struct RenderInput {
    native: NativeInput,
    bits: FlagBits,
    bounds: [u32; 4],
}

/// Inclusive 0-based FITS bounds `[x0, x1, y0, y1]` of the render grid a
/// cutout can reach: 33 points along each edge, widened by the resampling
/// margin. This is the order `TanGrid::detector_bounds` uses and
/// [`tile_reaches`] reads. `None` when the cutout misses the render grid.
fn render_bounds(cutout: &Cutout, grid: &TanGrid) -> Option<[u32; 4]> {
    let left = f64::from(cutout.origin_fits.0) - 0.5;
    let bottom = f64::from(cutout.origin_fits.1) - 0.5;
    let right = left + f64::from(cutout.image.width);
    let top = bottom + f64::from(cutout.image.height);
    let along = |low: f64, high: f64| (0..=32).map(move |i| low + (high - low) * f64::from(i) / 32.0);
    let edges = along(left, right)
        .flat_map(|x| [(x, bottom), (x, top)])
        .chain(along(bottom, top).flat_map(|y| [(left, y), (right, y)]));
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in edges {
        let (ra, dec) = cutout.wcs.pixel_to_sky(x, y);
        let (out_x, out_y) = grid.pixel(ra, dec);
        if !out_x.is_finite() || !out_y.is_finite() {
            continue;
        }
        min_x = min_x.min(out_x);
        min_y = min_y.min(out_y);
        max_x = max_x.max(out_x);
        max_y = max_y.max(out_y);
    }
    if !(min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite()) {
        return None;
    }
    let width = f64::from(grid.width);
    let height = f64::from(grid.height);
    let (x0, y0) = ((min_x - 3.0).floor().max(0.0), (min_y - 3.0).floor().max(0.0));
    let (x1, y1) = ((max_x + 1.0).ceil().min(width - 1.0), (max_y + 1.0).ceil().min(height - 1.0));
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some([x0 as u32, x1 as u32, y0 as u32, y1 as u32])
}

/// Whether bounds from [`render_bounds`] reach the PNG tile at column `x`,
/// row `png_y`. PNG rows run down from the top; FITS rows run up.
fn tile_reaches(bounds: [u32; 4], cols: u32, grid_height: u32, x: u32, png_y: u32) -> bool {
    let first_x = bounds[0] / TILE;
    let last_x = bounds[1] / TILE;
    let first_y = (grid_height - 1 - bounds[3]) / TILE;
    let last_y = (grid_height - 1 - bounds[2]) / TILE;
    (first_x..=last_x).contains(&x) && (first_y..=last_y).contains(&png_y) && x < cols
}

const COLOR_BLUE: u16 = 5;
const COLOR_GREEN: u16 = 40;
const COLOR_RED: u16 = 90;
const COLOR_WIDTH: u16 = 5;
/// Blue, green, red.
pub(crate) const COLOR_BANDS: [u16; 3] = [COLOR_BLUE, COLOR_GREEN, COLOR_RED];

/// One false-colour channel: a centre band and the window of bands around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColorBand {
    pub center: u16,
    pub width: u16,
}

/// The three bands mapped to blue, green and red.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColorBands {
    pub blue: ColorBand,
    pub green: ColorBand,
    pub red: ColorBand,
}

impl ColorBands {
    /// The default mapping. A request without colour parameters selects
    /// exactly these windows.
    pub(crate) const DEFAULT: Self = Self {
        blue: ColorBand {
            center: COLOR_BANDS[0],
            width: COLOR_WIDTH,
        },
        green: ColorBand {
            center: COLOR_BANDS[1],
            width: COLOR_WIDTH,
        },
        red: ColorBand {
            center: COLOR_BANDS[2],
            width: COLOR_WIDTH,
        },
    };

    /// In channel order, blue first.
    pub(crate) fn channels(self) -> [ColorBand; 3] {
        [self.blue, self.green, self.red]
    }

    /// Stable text for a cache or job key, so two mappings cannot share one.
    pub(crate) fn key(self) -> String {
        format!(
            "b{}:{} g{}:{} r{}:{}",
            self.blue.center, self.blue.width, self.green.center, self.green.width, self.red.center, self.red.width
        )
    }
}

/// Read the optional colour-band query parameters. A missing centre or width
/// keeps the fixed default. The window is clipped to the band space by
/// `View::selected_bands`, exactly as a range pin is.
pub(crate) fn color_bands_of(query: &RenderQuery) -> Result<ColorBands, Error> {
    let default = ColorBands::DEFAULT;
    let band = |name: &str, center: Option<u16>, width: Option<u16>, default: ColorBand| {
        let center = center.unwrap_or(default.center);
        let width = width.unwrap_or(default.width);
        if center >= BANDS.len() as u16 || width == 0 || width > BANDS.len() as u16 {
            return Err(Error::Message(format!(
                "{name} is outside bands 0..{} with width 1..{}",
                BANDS.len() - 1,
                BANDS.len()
            )));
        }
        Ok(ColorBand { center, width })
    };
    Ok(ColorBands {
        blue: band("color_b", query.color_b, query.color_b_width, default.blue)?,
        green: band("color_g", query.color_g, query.color_g_width, default.green)?,
        red: band("color_r", query.color_r, query.color_r_width, default.red)?,
    })
}

/// The pinned view behind one colour channel.
fn color_view(base: &spearv_core::View, band: ColorBand) -> spearv_core::View {
    let mut view = base.clone();
    view.mode = spearv_core::Mode::Range;
    view.pins = vec![spearv_core::Pin {
        center: band.center,
        width: band.width,
        visit: None,
        sign: None,
    }];
    view
}

/// Three bands mapped to blue, green and red. The exact bands are a display
/// choice; this is false colour, and the route says so in the response header.
#[utoipa::path(
    get,
    path = "/api/v1/color.png",
    params(RenderQuery),
    responses(
        (status = 200, description = "False-colour PNG from three bands", content_type = "image/png"),
        (status = 400, description = "View parameters are invalid", body = ApiError),
        (status = 404, description = "No exposure covers this view", body = ApiError),
        (status = 502, description = "The S3 read failed", body = ApiError),
        (status = 504, description = "The render exceeded its time limit", body = ApiError)
    )
)]
async fn color_png(
    State(ctx): State<Arc<Context>>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Error> {
    let query = parse_query(raw.as_deref().unwrap_or(""))?;
    let (base, grid) = render_view_of(&query)?;
    if base.mode == spearv_core::Mode::Delta {
        return Err(Error::Message("colour is not a delta view".into()));
    }
    let bands = color_bands_of(&query)?;
    let key = format!(
        "color:{:.6}:{:.6}:{:.6}:{:.6}:{}:{}:{}",
        base.region.ra,
        base.region.dec,
        base.region.fov_l_deg,
        base.region.fov_h_deg,
        base.release,
        base.mask,
        bands.key()
    );
    if let Some(png) = ctx.cache.get(&key) {
        return Ok(png_response(png, true, 0, 0, grid, 0, 0));
    }
    let mut views = Vec::with_capacity(3);
    let mut reads = Vec::with_capacity(3);
    let mut elapsed = 0;
    for band in bands.channels() {
        let view = color_view(&base, band);
        let read = read_band(&ctx, &view).await?;
        elapsed += read.elapsed_ms;
        views.push(view);
        reads.push(read);
    }
    let left = Duration::from_secs(RENDER_SECONDS).saturating_sub(Duration::from_millis(elapsed as u64));
    let render_ctx = Arc::clone(&ctx);
    let png = tokio::time::timeout(left, async move {
        let mut stacks = Vec::with_capacity(3);
        for (read, view) in reads.into_iter().zip(&views) {
            stacks.push(stack_of(&render_ctx, read.cutouts, view).await?);
        }
        combine_color(&stacks)
    })
    .await
    .map_err(|_| Error::Deadline { seconds: RENDER_SECONDS })??;
    ctx.cache.insert(key, png.clone());
    Ok(png_response(png, false, 0, elapsed, grid, 0, 0))
}

/// Combine three stacks at one stretch and one tone. This is the same pixel
/// rule the mosaic's colour tiles use: `color_pixel`, so a channel without a
/// sample is black, and only a pixel without any sample is no-data.
fn combine_color(stacks: &[Stacked]) -> Result<Vec<u8>, Error> {
    if stacks.len() != 3 {
        return Err(Error::Message("colour needs three bands".into()));
    }
    let (width, height) = (stacks[0].width, stacks[0].height);
    if stacks.iter().any(|stack| stack.width != width || stack.height != height) {
        return Err(Error::Message("colour bands have different grids".into()));
    }
    let stretches: Vec<Stretch> = stacks
        .iter()
        .map(|stack| {
            let mut values: Vec<f32> = stack.value.iter().copied().filter(|value| value.is_finite()).collect();
            values.sort_unstable_by(f32::total_cmp);
            let low = percentile(&values, 0.05);
            let high = percentile(&values, 0.995);
            Stretch {
                low,
                span: if high > low { high - low } else { 1.0 },
            }
        })
        .collect();
    let mut rgb = vec![0_u8; (width * height * 3) as usize];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let out = (((height - 1 - y) * width + x) * 3) as usize;
            let channel = |index: usize| stacks[index].value[i].is_finite().then_some(stacks[index].value[i]);
            let pixel = color_pixel(
                channel(0),
                channel(1),
                channel(2),
                [Some(stretches[0]), Some(stretches[1]), Some(stretches[2])],
            );
            rgb[out..out + 3].copy_from_slice(&pixel);
        }
    }
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    use image::ImageEncoder;
    encoder
        .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|err| Error::Message(err.to_string()))?;
    Ok(png)
}

fn parse_query(raw: &str) -> Result<RenderQuery, Error> {
    let mut query = RenderQuery {
        ra: f64::NAN,
        dec: f64::NAN,
        fov_l: f64::NAN,
        fov_h: f64::NAN,
        mode: None,
        dr: None,
        mask: None,
        pin: Vec::new(),
        color_r: None,
        color_r_width: None,
        color_g: None,
        color_g_width: None,
        color_b: None,
        color_b_width: None,
    };
    for (key, value) in query_pairs(raw) {
        match key.as_str() {
            "ra" => query.ra = value.parse().unwrap_or(f64::NAN),
            "dec" => query.dec = value.parse().unwrap_or(f64::NAN),
            "fov_l" => query.fov_l = value.parse().unwrap_or(f64::NAN),
            "fov_h" => query.fov_h = value.parse().unwrap_or(f64::NAN),
            "mode" => query.mode = Some(value),
            "dr" => query.dr = Some(value),
            "mask" => query.mask = Some(value),
            "pin" => query.pin.push(value),
            "color_r" => query.color_r = Some(band_param(&key, &value)?),
            "color_r_width" => query.color_r_width = Some(band_param(&key, &value)?),
            "color_g" => query.color_g = Some(band_param(&key, &value)?),
            "color_g_width" => query.color_g_width = Some(band_param(&key, &value)?),
            "color_b" => query.color_b = Some(band_param(&key, &value)?),
            "color_b_width" => query.color_b_width = Some(band_param(&key, &value)?),
            _ => {}
        }
    }
    Ok(query)
}

fn band_param(name: &str, value: &str) -> Result<u16, Error> {
    value
        .parse()
        .map_err(|_| Error::Message(format!("{name} must be an integer")))
}

fn view_of(query: &RenderQuery) -> Result<spearv_core::View, Error> {
    ViewQuery {
        ra: query.ra,
        dec: query.dec,
        fov_l: query.fov_l,
        fov_h: query.fov_h,
        mode: query.mode.clone(),
        dr: query.dr.clone(),
        pins: query.pin.clone(),
        mask: query.mask.clone(),
    }
    .parse(MAX_FOV_DEG)
}

/// A view for a PNG route, with the grid side that route may actually draw.
/// Wider than the 4096 px render grid is rejected here with a message naming
/// the limit; the mosaic keeps the [`MAX_FOV_DEG`] cap and a true-size `TanGrid`.
fn render_view_of(query: &RenderQuery) -> Result<(spearv_core::View, (u32, u32)), Error> {
    let view = view_of(query)?;
    let grid = (render_grid_size(view.region.fov_l_deg)?, render_grid_size(view.region.fov_h_deg)?);
    Ok((view, grid))
}

fn source(ctx: &Context) -> Result<Arc<dyn spearv_core::ImageSource>, Error> {
    ctx.images
        .get(&SourceId::new(SPHEREX))
        .cloned()
        .ok_or_else(|| Error::Message("SPHEREx source is not registered".into()))
}

fn band_span(bands: &[u16]) -> (f64, f64) {
    if bands.is_empty() {
        return (0.0, 10.0);
    }
    let min = bands
        .iter()
        .map(|band| BANDS[*band as usize].wl_min_um)
        .fold(f64::MAX, f64::min);
    let max = bands
        .iter()
        .map(|band| BANDS[*band as usize].wl_max_um)
        .fold(f64::MIN, f64::max);
    (min, max)
}

fn cluster_visits_for(exposures: &[spearv_core::Exposure]) -> Vec<Visit> {
    let mjds: Vec<f64> = exposures.iter().map(|exposure| exposure.mjd).collect();
    let spans = cluster_visits(&mjds);
    spans
        .iter()
        .map(|span| {
            let mut count = 0;
            let mut end = span.mjd;
            for exposure in exposures {
                if visit_index(exposure.mjd, &spans) == Some(span.index) {
                    count += 1;
                    end = end.max(exposure.mjd);
                }
            }
            Visit {
                index: span.index,
                mjd: span.mjd,
                mjd_end: end,
                exposures: count,
            }
        })
        .collect()
}

fn encode(stacked: &Stacked) -> Result<Vec<u8>, String> {
    let mut valid = Vec::new();
    for value in &stacked.value {
        if value.is_finite() {
            valid.push(*value);
        }
    }
    valid.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let (lo, hi) = if valid.is_empty() {
        (0.0, 1.0)
    } else {
        (percentile(&valid, 0.02), percentile(&valid, 0.995))
    };
    let span = if hi > lo { hi - lo } else { 1.0 };
    let width = stacked.width as usize;
    let height = stacked.height as usize;
    let mut rgb = vec![0_u8; width * height * 3];
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let out = ((height - 1 - y) * width + x) * 3;
            let sample = stacked.value[i];
            if !sample.is_finite() {
                rgb[out..out + 3].copy_from_slice(&NODATA);
                continue;
            }
            let grey = grey_level(sample, lo, span);
            rgb[out] = grey;
            rgb[out + 1] = grey;
            rgb[out + 2] = grey;
        }
    }
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    use image::ImageEncoder;
    encoder
        .write_image(
            &rgb,
            stacked.width,
            stacked.height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|err| err.to_string())?;
    Ok(png)
}

/// A display gamma only. The stored value is untouched; it lifts the faint end so
/// a star field reads as a field, not a few bright dots on black.
fn grey_level(sample: f32, low: f32, span: f32) -> u8 {
    let t = ((((sample - low) / span).clamp(0.0, 1.0)).powf(0.7) * 1.15).clamp(0.0, 1.0);
    (t * 255.0).round() as u8
}

/// One channel through the display tone, from a physical value. `/color.png`
/// and the mosaic colour tiles both call this, so the tone has one owner.
pub(crate) fn tone(value: f32, stretch: Stretch) -> u8 {
    (((value - stretch.low) / stretch.span).clamp(0.0, 1.0).powf(DISPLAY_GAMMA) * 255.0).round() as u8
}

/// One channel of a false-colour pixel. A channel with no sample is black, so
/// the channels that do have data still show; a pixel with no sample in any
/// channel is no-data.
pub(crate) fn shade(value: Option<f32>, stretch: Option<Stretch>) -> u8 {
    match (value, stretch) {
        (Some(value), Some(stretch)) => tone(value, stretch),
        _ => 0,
    }
}

/// One false-colour pixel. `/color.png` and the mosaic colour tiles both call
/// this, so the tone and the missing-channel rule cannot drift apart.
pub(crate) fn color_pixel(
    blue: Option<f32>,
    green: Option<f32>,
    red: Option<f32>,
    stretches: [Option<Stretch>; 3],
) -> [u8; 3] {
    if blue.is_none() && green.is_none() && red.is_none() {
        return NODATA;
    }
    [shade(red, stretches[2]), shade(green, stretches[1]), shade(blue, stretches[0])]
}

fn percentile(sorted: &[f32], p: f64) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn png_response(
    png: Vec<u8>,
    hit: bool,
    used: u32,
    elapsed_ms: u128,
    grid: (u32, u32),
    index_hits: u32,
    reads: u32,
) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header("x-cache", if hit { "HIT" } else { "MISS" })
        .header("x-exposures", used.to_string())
        .header("x-render-ms", elapsed_ms.to_string())
        .header("x-grid-width", grid.0.to_string())
        .header("x-grid-height", grid.1.to_string())
        .header("x-index-hits", index_hits.to_string())
        .header("x-reads", reads.to_string())
        .body(axum::body::Body::from(png))
        .expect("png response")
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, OnceLock};

    use spearv_core::WorkBudget;
    use spearv_science::{PythonMath, RuntimeConfig};

    /// One real interpreter and budget for every composite test.
    /// `PythonMath::initialize` is process-global and requires the same budget
    /// on later calls; run these tests with `--test-threads=1`.
    pub(crate) fn math() -> (Arc<WorkBudget>, Arc<PythonMath>) {
        static MATH: OnceLock<(Arc<WorkBudget>, Arc<PythonMath>)> = OnceLock::new();
        let (budget, math) = MATH.get_or_init(|| {
            let budget = Arc::new(WorkBudget::default());
            let math = PythonMath::initialize(RuntimeConfig::host_qualification(), Arc::clone(&budget))
                .expect("composite test math runtime");
            (budget, Arc::new(math))
        });
        (Arc::clone(budget), Arc::clone(math))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spearv_core::{Exposure, ExposureId, TanGrid};

    fn query(fov: f64) -> RenderQuery {
        RenderQuery {
            ra: 56.75,
            dec: 24.12,
            fov_l: fov,
            fov_h: fov,
            mode: None,
            dr: None,
            mask: None,
            pin: Vec::new(),
            color_r: None,
            color_r_width: None,
            color_g: None,
            color_g_width: None,
            color_b: None,
            color_b_width: None,
        }
    }

    #[test]
    fn routes_require_the_pair_and_preserve_rectangular_render_dimensions() {
        assert!(view_of(&parse_query("ra=10&dec=20&fov=0.2").unwrap()).is_err());
        let (view, dims) = render_view_of(&parse_query("ra=10&dec=20&fov_l=0.2&fov_h=0.1").unwrap()).unwrap();
        assert_eq!(dims, (117, 59));
        assert_eq!((view.region.fov_l_deg, view.region.fov_h_deg), (0.2, 0.1));
        assert!(render_view_of(&parse_query("ra=10&dec=20&fov_l=0.1&fov_h=7").unwrap()).is_err());
        assert!(render_view_of(&parse_query("ra=10&dec=20&fov_l=7&fov_h=0.1").unwrap()).is_err());
    }

    #[test]
    fn a_render_view_rejects_a_field_the_grid_would_crop() {
        assert!(render_view_of(&query(6.99)).is_ok());
        let error = render_view_of(&query(7.0)).expect_err("7 degrees cannot be drawn at 4096 px");
        assert!(error.to_string().contains("6.997"), "{error}");
    }

    #[test]
    fn the_mosaic_accepts_thirty_degrees() {
        let view = view_of(&query(MAX_FOV_DEG)).unwrap();
        assert_eq!(view.region.envelope_deg(), 30.0);
        assert_eq!(TanGrid::new(view.region.ra, view.region.dec, view.region.envelope_deg()).width, 17561);
    }

    fn color_bands(raw: &str) -> Result<ColorBands, Error> {
        color_bands_of(&parse_query(raw)?)
    }

    #[test]
    fn the_default_colour_bands_keep_the_fixed_windows() {
        let query = parse_query("ra=56.75&dec=24.12&fov_l=0.1&fov_h=0.1&color=1").unwrap();
        assert_eq!(color_bands_of(&query).unwrap(), ColorBands::DEFAULT);
        let view = view_of(&query).unwrap();
        let selected: Vec<Vec<u16>> = ColorBands::DEFAULT
            .channels()
            .iter()
            .map(|band| color_view(&view, *band).selected_bands())
            .collect();
        assert_eq!(
            selected,
            [
                vec![3, 4, 5, 6, 7],
                vec![38, 39, 40, 41, 42],
                vec![88, 89, 90, 91, 92]
            ]
        );
    }

    #[test]
    fn custom_colour_bands_replace_each_channel_window() {
        let query = parse_query(
            "ra=56.75&dec=24.12&fov_l=0.1&fov_h=0.1&color=1&color_r=100&color_r_width=3\
             &color_g=1&color_g_width=7&color_b=50&color_b_width=1",
        )
        .unwrap();
        let bands = color_bands_of(&query).unwrap();
        let view = view_of(&query).unwrap();
        assert_eq!(color_view(&view, bands.red).selected_bands(), [99, 100, 101]);
        assert_eq!(color_view(&view, bands.green).selected_bands(), [0, 1, 2, 3, 4], "clipped at band 0");
        assert_eq!(color_view(&view, bands.blue).selected_bands(), [50]);
    }

    #[test]
    fn invalid_colour_bands_are_bad_requests() {
        use axum::response::IntoResponse;
        let status = |raw: &str| {
            color_bands(raw)
                .expect_err("invalid colour bands")
                .into_response()
                .status()
        };
        assert_eq!(status("color_r=102"), StatusCode::BAD_REQUEST);
        assert_eq!(status("color_r_width=0"), StatusCode::BAD_REQUEST);
        assert_eq!(status("color_g_width=103"), StatusCode::BAD_REQUEST);
        assert_eq!(status("color_b=abc"), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_colour_key_carries_the_band_mapping() {
        let changed = ColorBands {
            red: ColorBand {
                center: 91,
                ..ColorBands::DEFAULT.red
            },
            ..ColorBands::DEFAULT
        };
        assert_ne!(ColorBands::DEFAULT.key(), changed.key());
        assert!(ColorBands::DEFAULT.key().contains("r90:5"));
    }

    fn exposure(mjd: f64) -> Exposure {
        Exposure {
            id: ExposureId(format!("e{mjd}")),
            source: SourceId::new(SPHEREX),
            detector: 0,
            mjd,
            release: ReleaseId("qr3".into()),
            version: "v".into(),
            obsid: format!("o{mjd}"),
            url: format!("u{mjd}"),
            etag: None,
            size: None,
            em_min_um: 0.0,
            em_max_um: 10.0,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_render_stack_uses_the_library_session_path() {
        let (budget, math) = crate::testing::math();
        let ctx = Arc::new(
            Context::new(Arc::new(spearv_core::Cache::new()))
                .with_budget(budget)
                .with_math(math),
        );
        let view = view_of(&parse_query("ra=10&dec=20&fov_l=0.01&fov_h=0.01&mode=range&pin=40:1").unwrap()).unwrap();
        let dims = render_grid_size(view.region.fov_l_deg).unwrap();
        let cutout = flat_cutout(view.region.ra, view.region.dec, (dims, dims), (1, 1), (dims, dims));
        let stacked = stack_of(&ctx, vec![cutout], &view).await.unwrap();
        assert_eq!((stacked.width, stacked.height), (dims, dims));
        assert!(stacked.value.iter().all(|value| (value - 1.5).abs() < 1e-6), "IMAGE - ZODI through the library");
    }

    /// A flat `size` window at `origin` of a detector whose WCS is the north-up
    /// `naxis` grid centred on `(ra, dec)`: IMAGE 2, VARIANCE 1, ZODI 0.5, band 40.
    fn flat_cutout(ra: f64, dec: f64, naxis: (u32, u32), origin: (i32, i32), size: (u32, u32)) -> Cutout {
        let (width, height) = size;
        let wcs = spearv_core::Wcs {
            crpix: [(f64::from(naxis.0) + 1.0) / 2.0, (f64::from(naxis.1) + 1.0) / 2.0],
            crval: [ra, dec],
            cd: [[-spearv_core::PIXEL_SCALE_DEG, 0.0], [0.0, spearv_core::PIXEL_SCALE_DEG]],
            sip: spearv_core::Sip {
                a: spearv_core::Poly::new(0),
                b: spearv_core::Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [naxis.0 as i32, naxis.1 as i32],
        };
        let plane = |value: f32| spearv_core::Plane { width, height, data: vec![value; (width * height) as usize] };
        Cutout {
            image: plane(2.0),
            variance: plane(1.0),
            flags: spearv_core::Plane {
                width,
                height,
                data: vec![0; (width * height) as usize],
            },
            zodi: Some(plane(0.5)),
            wcs,
            wavelength: spearv_core::WavelengthMap {
                x: vec![1.0, f64::from(naxis.0)],
                y: vec![1.0, f64::from(naxis.1)],
                wavelength: vec![BANDS[40].center_um as f32; 4],
                bandwidth: vec![0.01; 4],
                nx: 2,
                ny: 2,
            },
            origin_fits: origin,
            flags_valid: None,
            clipped: false,
            flag_bits: spearv_core::FlagBits {
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

    /// Wider than tall, so bounds read in the wrong order would select the
    /// wrong tiles or index PNG rows by a column past the grid's height.
    #[test]
    fn render_bounds_select_exactly_the_tiles_a_cutout_reaches() {
        let grid = TanGrid::new_rect(10.0, 20.0, 600.0 * PIXEL_SCALE_DEG, 300.0 * PIXEL_SCALE_DEG);
        assert_eq!((grid.width, grid.height), (600, 300));
        let (cols, rows) = (600_u32.div_ceil(TILE), 300_u32.div_ceil(TILE));

        let whole = render_bounds(&flat_cutout(10.0, 20.0, (600, 300), (1, 1), (600, 300)), &grid).expect("on the grid");
        assert_eq!(whole, [0, 599, 0, 299]);
        for png_y in 0..rows {
            for x in 0..cols {
                assert!(tile_reaches(whole, cols, 300, x, png_y), "tile {x},{png_y} is covered");
            }
        }

        // FITS row 1 is the bottom of the picture: the last PNG tile row.
        let corner = render_bounds(&flat_cutout(10.0, 20.0, (600, 300), (1, 1), (10, 10)), &grid).expect("on the grid");
        let reached = (0..rows)
            .flat_map(|png_y| (0..cols).map(move |x| (x, png_y)))
            .filter(|&(x, png_y)| tile_reaches(corner, cols, 300, x, png_y))
            .collect::<Vec<_>>();
        assert_eq!(reached, [(0, rows - 1)]);

        assert!(render_bounds(&flat_cutout(10.0, 20.0, (600, 300), (2000, 1), (10, 10)), &grid).is_none());
    }

    #[test]
    fn a_visit_carries_its_last_exposure() {
        let exposures = [61000.0, 61005.0, 61030.0, 61031.0].map(exposure);
        let visits = cluster_visits_for(&exposures);
        assert_eq!(visits.len(), 2);
        assert_eq!((visits[0].mjd, visits[0].mjd_end, visits[0].exposures), (61000.0, 61005.0, 2));
        assert_eq!((visits[1].mjd, visits[1].mjd_end, visits[1].exposures), (61030.0, 61031.0, 2));
    }
}
