//! Difference of two same-release visits.
//!
//! QR2 and QR3 are not mixed. `l3_flux_corrections` exists as a per-detector
//! float image, but its units were not verified, so applying it would invent a
//! calibration. A delta stays inside the release named by `dr`.
//!
//! The library owns the sampling, the flag policy and the subtraction. This
//! owns the tile grid, the admission and the PNG.

pub mod candidates;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use spearv_core::{
    cluster_visits,
    delta_bands,
    delta_rgb,
    delta_scale,
    delta_sides,
    math::{
        spawn_math, ImageMath, MaskPolicy, MathControl, MathError, NativeInput, NativeWindow, Product,
        RasterOrder, Shape, TargetGrid, Tile, TileOutput, Values,
    },
    query_pairs,
    render_grid_size,
    ApiError, Context, Cutout, Error, Exposure, Extension, Filter, FlagBits, ImageSource, Input, ReleaseId, SourceId,
    TanGrid, View, ViewQuery, Wcs, BANDS, PIXEL_SCALE_DEG,
};
use tokio::sync::Semaphore;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

const RENDER_SECONDS: u64 = 600;
const READ_CONCURRENCY: usize = 8;
/// The library works on tiles of at most this many pixels a side.
const TILE: u32 = 256;
/// The admitted native window bound. The runtime holds the same one.
const MAX_NATIVE_PIXELS: usize = 2040 * 2040;
/// Nearly black so the no-overlap wedge does not compete with the data.
const NODATA: [u8; 3] = [24, 28, 36];
const NOTE: &str = "same release only; l3_flux_corrections are not applied, so QR2 is not mixed with QR3; OUTLIER pixels are kept";

/// The differenced grid. `value` is NaN where either side has no sample.
pub struct Stacked {
    pub width: u32,
    pub height: u32,
    pub value: Vec<f32>,
    pub variance: Vec<f32>,
}

pub struct Delta;

impl Extension for Delta {
    fn id(&self) -> &'static str {
        "delta"
    }

    fn routes(&self) -> OpenApiRouter<Arc<Context>> {
        OpenApiRouter::new().routes(routes!(delta_png)).routes(routes!(candidates::candidates))
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeltaQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov_l: f64,
    pub fov_h: f64,
    pub dr: Option<String>,
    pub mask: Option<String>,
    #[serde(default)]
    pub pin: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/delta.png",
    params(DeltaQuery),
    responses(
        (status = 200, description = "Plus minus minus, mid-grey is zero", content_type = "image/png"),
        (status = 400, description = "Pins disagree in wavelength, or a sign is missing", body = ApiError),
        (status = 504, description = "The render exceeded 600s", body = ApiError)
    )
)]
async fn delta_png(
    State(ctx): State<Arc<Context>>,
    RawQuery(raw): RawQuery,
) -> Result<Response, Error> {
    let started = Instant::now();
    let loaded = load(&ctx, raw.as_deref()).await?;
    // The picture needs the difference only, so it asks for the value product.
    let diff = difference(&ctx, &loaded, Product::Values).await?;
    let png = encode(&diff)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header("x-grid-width", diff.width.to_string())
        .header("x-grid-height", diff.height.to_string())
        .header("x-delta-note", NOTE)
        .header("x-render-ms", started.elapsed().as_millis().to_string())
        .header("x-index-hits", loaded.index_hits.to_string())
        .header("x-reads", loaded.reads.to_string())
        .body(axum::body::Body::from(png))
        .expect("png"))
}

/// The pixels behind a delta. `delta.png` and `/delta/candidates` share this,
/// so a candidate is always a blob in the image for the same URL.
pub(crate) struct Loaded {
    pub view: View,
    pub bands: Vec<u16>,
    pub plus: Vec<(Exposure, Cutout)>,
    pub minus: Vec<(Exposure, Cutout)>,
    pub index_hits: u32,
    pub reads: u32,
}

/// One cutout, admitted once and shared by every tile it reaches.
struct Admitted {
    /// Position in the side's read order. The library wants a rising order.
    order: u64,
    input: NativeInput,
    wcs: Wcs,
    window: NativeWindow,
    bits: FlagBits,
    /// The tile origins this cutout reaches.
    tiles: Vec<(u32, u32)>,
}

/// One cutout's share of a tile, moved into the blocking worker.
struct Work {
    order: u64,
    input: NativeInput,
    wcs: Wcs,
    window: NativeWindow,
    bits: FlagBits,
}

/// The per-request settings every tile of this delta shares.
struct Plan {
    math: Arc<dyn ImageMath>,
    control: MathControl,
    target: TargetGrid,
    bands: Vec<u16>,
    policy: MaskPolicy,
    product: Product,
}

/// The planes of one tile output.
struct TilePlanes<'a> {
    value: &'a [f32],
    variance: Option<&'a [f32]>,
}

/// The delta of the two visits, one tile at a time.
///
/// `product` is the plane set the caller needs: the picture asks for values
/// only, the candidate finder asks for the variance it measures significance
/// against. Both subtract with the library, so the two sides must carry the
/// same product and be the same tile of the same grid.
pub(crate) async fn difference(
    ctx: &Context,
    loaded: &Loaded,
    product: Product,
) -> Result<Stacked, Error> {
    let math = ctx.math()?;
    let grid = TanGrid::new_rect(
        loaded.view.region.ra,
        loaded.view.region.dec,
        loaded.view.region.fov_l_deg,
        loaded.view.region.fov_h_deg,
    );
    let target = TargetGrid {
        ra: grid.ra,
        dec: grid.dec,
        shape: Shape {
            width: grid.width,
            height: grid.height,
        },
        scale_deg: PIXEL_SCALE_DEG,
    };
    let width = grid.width as usize;
    let height = grid.height as usize;
    let mut value = vec![f32::NAN; width * height];
    let mut variance = vec![f32::NAN; width * height];
    let plan = Plan {
        math,
        control: control(),
        target,
        bands: loaded.bands.clone(),
        policy: if loaded.view.mask { MaskPolicy::Delta } else { MaskPolicy::None },
        product,
    };
    let plus = admit(ctx, &loaded.plus, &grid)?;
    let minus = admit(ctx, &loaded.minus, &grid)?;
    for ty in (0..grid.height).step_by(TILE as usize) {
        for tx in (0..grid.width).step_by(TILE as usize) {
            let tile = Tile {
                x: tx,
                y: ty,
                shape: Shape {
                    width: (grid.width - tx).min(TILE),
                    height: (grid.height - ty).min(TILE),
                },
                order: RasterOrder::Fits,
            };
            let plus_out = stack_tile(&plan, &plus, tile).await?;
            let minus_out = stack_tile(&plan, &minus, tile).await?;
            // The two sides must be the same tile of the same grid before they
            // are differenced; anything else subtracts unrelated pixels.
            if plus_out.tile() != tile || minus_out.tile() != tile {
                return Err(Error::Math(MathError::Internal("delta tile identity".into())));
            }
            let session = plan.math.session(plan.control.clone(), tile, plan.product.clone())?;
            let diff = spawn_math(session, move |mut session| session.difference(&plus_out, &minus_out))?
                .wait()
                .await?;
            let planes = planes(&diff)?;
            for row in 0..tile.shape.height as usize {
                let target_row = ty as usize + row;
                for column in 0..tile.shape.width as usize {
                    let index = target_row * width + tx as usize + column;
                    let at = row * tile.shape.width as usize + column;
                    value[index] = planes.value[at];
                    if let Some(plane) = planes.variance {
                        variance[index] = plane[at];
                    }
                }
            }
        }
    }
    Ok(Stacked {
        width: grid.width,
        height: grid.height,
        value,
        variance,
    })
}

/// The value, variance and count planes of one tile output.
fn planes(output: &TileOutput) -> Result<TilePlanes<'_>, Error> {
    let Values::F32(value) = &output.planes().value else {
        return Err(Error::Math(MathError::Internal("delta value plane".into())));
    };
    let variance = match &output.planes().variance {
        None => None,
        Some(Values::F32(variance)) => Some(variance.as_slice()),
        Some(Values::F64(_)) => return Err(Error::Math(MathError::Internal("delta variance plane".into()))),
    };
    Ok(TilePlanes {
        value: value.as_slice(),
        variance,
    })
}

/// One tile of one side. The library samples, masks and sums; this only says
/// which cutouts reach the tile.
async fn stack_tile(plan: &Plan, admitted: &[Admitted], tile: Tile) -> Result<TileOutput, Error> {
    let work: Vec<Work> = admitted
        .iter()
        .filter(|cutout| cutout.tiles.contains(&(tile.x, tile.y)))
        .map(|cutout| Work {
            order: cutout.order,
            input: cutout.input.clone(),
            wcs: cutout.wcs.clone(),
            window: cutout.window,
            bits: cutout.bits,
        })
        .collect();
    let bands = plan.bands.clone();
    let target = plan.target;
    let policy = plan.policy;
    let session = plan.math.session(plan.control.clone(), tile, plan.product.clone())?;
    let task = spawn_math(session, move |mut session| {
        for cutout in &work {
            // Only a cutout that reaches this tile gets a geometry.
            let geometry = session.geometry(&cutout.wcs, cutout.window, target)?;
            session.add(cutout.order, &cutout.input, &geometry, cutout.bits, policy, &bands)?;
        }
        session.finish()
    })?;
    Ok(task.wait().await?)
}

/// Admit one side's cutouts. A cutout without ZODI is skipped: the delta
/// compares sky flux, and a missing ZODI plane would compare raw counts.
fn admit(ctx: &Context, side: &[(Exposure, Cutout)], grid: &TanGrid) -> Result<Vec<Admitted>, Error> {
    let mut admitted = Vec::new();
    for (order, (_, cutout)) in side.iter().enumerate() {
        if cutout.zodi.is_none() {
            continue;
        }
        let input = NativeInput::from_cutout(&ctx.budget, MAX_NATIVE_PIXELS, cutout, true)?;
        admitted.push(Admitted {
            order: order as u64,
            input,
            wcs: cutout.wcs.clone(),
            window: NativeWindow {
                shape: Shape {
                    width: cutout.image.width,
                    height: cutout.image.height,
                },
                origin: [cutout.origin_fits.0, cutout.origin_fits.1],
            },
            bits: cutout.flag_bits,
            tiles: tiles_of(cutout, grid),
        });
    }
    Ok(admitted)
}

/// The tiles a cutout reaches.
fn tiles_of(cutout: &Cutout, grid: &TanGrid) -> Vec<(u32, u32)> {
    let Some((x0, x1, y0, y1)) = footprint(cutout, grid) else {
        return Vec::new();
    };
    let mut tiles = Vec::new();
    for ty in (y0 / TILE)..=(y1 / TILE) {
        for tx in (x0 / TILE)..=(x1 / TILE) {
            tiles.push((tx * TILE, ty * TILE));
        }
    }
    tiles
}

/// Inclusive 0-based grid bounds a cutout can reach.
///
/// The rectangle grows by a pixel, its edges are walked so SIP distortion and
/// rotation stay inside the box, and the margin covers nearest-pixel
/// rounding. `None` when the cutout misses the grid.
fn footprint(cutout: &Cutout, grid: &TanGrid) -> Option<(u32, u32, u32, u32)> {
    let left = f64::from(cutout.origin_fits.0) - 0.5;
    let bottom = f64::from(cutout.origin_fits.1) - 0.5;
    let right = left + f64::from(cutout.image.width);
    let top = bottom + f64::from(cutout.image.height);
    const STEPS: i32 = 128;
    let along = |low: f64, high: f64| {
        (0..=STEPS).map(move |i| low + (high - low) * f64::from(i) / f64::from(STEPS))
    };
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
    let (x1, y1) = (
        (max_x + 1.0).ceil().min(width - 1.0),
        (max_y + 1.0).ceil().min(height - 1.0),
    );
    if x1 < x0 || y1 < y0 {
        return None;
    }
    Some((x0 as u32, x1 as u32, y0 as u32, y1 as u32))
}

/// One control per request. Nothing bumps the generation here, so a session is
/// never stale; the control carries cancellation into the blocking worker.
fn control() -> MathControl {
    MathControl::new(Arc::new(AtomicU64::new(1)))
}

pub(crate) fn input_of(cutout: &Cutout, mask: bool) -> Option<Input<'_>> {
    Some(Input {
        wcs: &cutout.wcs,
        image: &cutout.image.data,
        variance: &cutout.variance.data,
        flags: &cutout.flags.data,
        zodi: &cutout.zodi.as_ref()?.data,
        wavelength: &cutout.wavelength,
        origin_fits: cutout.origin_fits,
        width: cutout.image.width as usize,
        flag_bits: cutout.flag_bits,
        mask,
        ignore_outlier: true,
    })
}

pub(crate) async fn load(ctx: &Context, raw: Option<&str>) -> Result<Loaded, Error> {
    let view = view_of(raw)?;
    let bands = delta_bands(&view)?;
    let (wl_min, wl_max) = band_span(&bands);
    let source = ctx
        .images
        .get(&SourceId::new("spherex"))
        .cloned()
        .ok_or_else(|| Error::Message("SPHEREx source is not registered".into()))?;
    let release = Filter {
        release: Some(ReleaseId(view.release.clone())),
        wavelength_um: None,
    };
    let all = source.coverage(&view.region, &release).await?;
    let spans = cluster_visits(&all.iter().map(|exposure| exposure.mjd).collect::<Vec<_>>());
    let band_filter = Filter {
        release: Some(ReleaseId(view.release.clone())),
        wavelength_um: Some((wl_min, wl_max)),
    };
    let matched = source.coverage(&view.region, &band_filter).await?;
    let (plus_exp, minus_exp) = delta_sides(matched, &spans, &view)?;
    let reads = (plus_exp.len() + minus_exp.len()) as u32;
    let index_hits = all.len() as u32;
    let deadline = Instant::now() + Duration::from_secs(RENDER_SECONDS);
    let plus = read_cutouts(Arc::clone(&source), plus_exp, &view, deadline).await?;
    let minus = read_cutouts(source, minus_exp, &view, deadline).await?;
    Ok(Loaded { view, bands, plus, minus, index_hits, reads })
}

fn view_of(raw: Option<&str>) -> Result<View, Error> {
    let mut query = ViewQuery {
        ra: f64::NAN,
        dec: f64::NAN,
        fov_l: f64::NAN,
        fov_h: f64::NAN,
        mode: Some("delta".into()),
        dr: None,
        pins: Vec::new(),
        mask: None,
    };
    for (key, value) in query_pairs(raw.unwrap_or("")) {
        match key.as_str() {
            "ra" => query.ra = value.parse().unwrap_or(f64::NAN),
            "dec" => query.dec = value.parse().unwrap_or(f64::NAN),
            "fov_l" => query.fov_l = value.parse().unwrap_or(f64::NAN),
            "fov_h" => query.fov_h = value.parse().unwrap_or(f64::NAN),
            "dr" => query.dr = Some(value),
            "mask" => query.mask = Some(value),
            "pin" => query.pins.push(value),
            _ => {}
        }
    }
    let view = query.parse(10.0)?;
    // `delta.png` draws one PNG on the render grid, so the same 4096 px cap
    // applies. Reject a field that would silently crop.
    render_grid_size(view.region.fov_l_deg)?;
    render_grid_size(view.region.fov_h_deg)?;
    Ok(view)
}

async fn read_cutouts(
    source: Arc<dyn ImageSource>,
    exposures: Vec<Exposure>,
    view: &View,
    deadline: Instant,
) -> Result<Vec<(Exposure, Cutout)>, Error> {
    let mut reads = tokio::task::JoinSet::new();
    let permits = Arc::new(Semaphore::new(READ_CONCURRENCY));
    for exposure in exposures {
        let permits = Arc::clone(&permits);
        let source = Arc::clone(&source);
        let region = view.region.clone();
        reads.spawn(async move {
            let _permit = permits.acquire().await;
            let cutout = source.cutout(&exposure, &region).await;
            cutout.map(|cutout| (exposure, cutout))
        });
    }
    let left = deadline.saturating_duration_since(Instant::now());
    let gathered = tokio::time::timeout(left, async {
        let mut cutouts = Vec::new();
        while let Some(joined) = reads.join_next().await {
            if let Ok(Ok(pair)) = joined {
                cutouts.push(pair);
            }
        }
        cutouts
    })
    .await;
    reads.abort_all();
    let mut cutouts = gathered.map_err(|_| Error::Deadline { seconds: RENDER_SECONDS })?;
    // Reads finish in any order. A stable order keeps the float sums, and so the image, repeatable.
    cutouts.sort_by(|a, b| a.0.id.0.cmp(&b.0.id.0));
    Ok(cutouts)
}

fn band_span(bands: &[u16]) -> (f64, f64) {
    let min = bands.iter().map(|band| BANDS[*band as usize].wl_min_um).fold(f64::MAX, f64::min);
    let max = bands.iter().map(|band| BANDS[*band as usize].wl_max_um).fold(f64::MIN, f64::max);
    (min, max)
}

fn encode(stacked: &Stacked) -> Result<Vec<u8>, Error> {
    let mut abs = Vec::new();
    for value in &stacked.value {
        if value.is_finite() {
            abs.push(value.abs());
        }
    }
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let scale = delta_scale(&abs);
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
            rgb[out..out + 3].copy_from_slice(&delta_rgb(sample, scale));
        }
    }
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    use image::ImageEncoder;
    encoder
        .write_image(&rgb, stacked.width, stacked.height, image::ExtendedColorType::Rgb8)
        .map_err(|err| Error::Message(err.to_string()))?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spearv_core::{band_of, Cache, ExposureId, Mode, Pin, Plane, Poly, Sign, SkyRegion, Sip, WavelengthMap, WorkBudget};
    use spearv_science::{PythonMath, RuntimeConfig};
    use std::sync::OnceLock;

    /// The interpreter is process-global and keeps its budget, so the tests
    /// share one pair.
    fn runtime() -> &'static (Arc<WorkBudget>, PythonMath) {
        static RUNTIME: OnceLock<(Arc<WorkBudget>, PythonMath)> = OnceLock::new();
        RUNTIME.get_or_init(|| {
            let budget = Arc::new(WorkBudget::new(512));
            let math = PythonMath::initialize(RuntimeConfig::host_qualification(), Arc::clone(&budget))
                .expect("preflight the math runtime");
            (budget, math)
        })
    }

    fn context() -> Arc<Context> {
        let (budget, math) = runtime();
        Arc::new(
            Context::new(Arc::new(Cache::new()))
                .with_budget(Arc::clone(budget))
                .with_math(Arc::new(math.clone())),
        )
    }

    fn bits() -> FlagBits {
        FlagBits {
            transient: 0,
            overflow: 1,
            sur_error: 2,
            nonfunc: 6,
            fullsample: 12,
            persist: 17,
            outlier: 19,
            source: 21,
        }
    }

    /// A four-pixel detector centred on the view, at the render scale.
    fn detector_wcs() -> Wcs {
        Wcs {
            crpix: [2.0, 2.0],
            crval: [10.0, 20.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: Sip {
                a: Poly::new(0),
                b: Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [4, 4],
        }
    }

    fn wavelength() -> WavelengthMap {
        WavelengthMap {
            x: vec![1.0, 4.0],
            y: vec![1.0, 4.0],
            wavelength: vec![0.75; 4],
            bandwidth: vec![0.02; 4],
            nx: 2,
            ny: 2,
        }
    }

    /// A detector of the given size, centred on the view, at the render scale.
    fn wide_wcs(side: u32) -> Wcs {
        Wcs {
            crpix: [(f64::from(side) + 1.0) / 2.0, (f64::from(side) + 1.0) / 2.0],
            crval: [10.0, 20.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: Sip {
                a: Poly::new(0),
                b: Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [side as i32, side as i32],
        }
    }

    fn wide_wavelength(side: u32) -> WavelengthMap {
        WavelengthMap {
            x: vec![1.0, f64::from(side)],
            y: vec![1.0, f64::from(side)],
            wavelength: vec![0.75; 4],
            bandwidth: vec![0.02; 4],
            nx: 2,
            ny: 2,
        }
    }

    fn cutout(image: f32, variance: f32, flags: u32, zodi: Option<f32>) -> Cutout {
        Cutout {
            image: Plane {
                width: 4,
                height: 4,
                data: vec![image; 16],
            },
            variance: Plane {
                width: 4,
                height: 4,
                data: vec![variance; 16],
            },
            flags: Plane {
                width: 4,
                height: 4,
                data: vec![flags; 16],
            },
            flags_valid: None,
            zodi: zodi.map(|value| Plane {
                width: 4,
                height: 4,
                data: vec![value; 16],
            }),
            wcs: detector_wcs(),
            wavelength: wavelength(),
            origin_fits: (1, 1),
            clipped: false,
            flag_bits: bits(),
            bytes_read: 0,
            s3_requests: 0,
        }
    }

    fn exposure(id: &str) -> Exposure {
        Exposure {
            id: ExposureId(id.into()),
            source: SourceId::new("spherex"),
            detector: 2,
            mjd: 0.0,
            release: ReleaseId("qr3".into()),
            version: String::new(),
            obsid: id.into(),
            url: String::new(),
            etag: None,
            size: None,
            em_min_um: 1.0,
            em_max_um: 1.7,
        }
    }

    /// The view the synthetic detector is centred on.
    fn detector_view() -> View {
        View {
            region: SkyRegion::square(10.0, 20.0, 3.0 * PIXEL_SCALE_DEG),
            mode: Mode::Delta,
            release: "qr3_deep".into(),
            pins: Vec::new(),
            mask: true,
        }
    }

    fn loaded(plus: Cutout, minus: Cutout) -> Loaded {
        Loaded {
            view: detector_view(),
            bands: vec![band_of(0.75).expect("a standard band")],
            plus: vec![(exposure("plus"), plus)],
            minus: vec![(exposure("minus"), minus)],
            index_hits: 2,
            reads: 2,
        }
    }

    #[tokio::test]
    async fn a_delta_is_the_difference_of_the_two_visits() {
        let ctx = context();
        let loaded = loaded(cutout(5.0, 1.0, 0, Some(0.0)), cutout(2.0, 1.0, 0, Some(0.0)));
        let diff = difference(&ctx, &loaded, Product::Values).await.expect("delta");
        assert_eq!((diff.width, diff.height), (3, 3));
        let centre = diff.value[4];
        assert!((centre - 3.0).abs() < 1e-5, "centre {centre}");
        assert!(
            diff.value.iter().all(|value| value.is_finite()),
            "the detector covers the grid"
        );
    }

    /// A detector larger than one tile, so the tiles are placed on the grid in
    /// the right place and the right way up.
    #[tokio::test]
    async fn a_value_lands_on_the_grid_pixel_it_came_from() {
        let ctx = context();
        let side = 258;
        let mut plus = cutout(0.0, 1.0, 0, Some(0.0));
        plus.image = Plane {
            width: side,
            height: side,
            data: (0..side * side).map(|i| i as f32).collect(),
        };
        plus.variance = Plane {
            width: side,
            height: side,
            data: vec![1.0; (side * side) as usize],
        };
        plus.flags = Plane {
            width: side,
            height: side,
            data: vec![0; (side * side) as usize],
        };
        plus.zodi = Some(Plane {
            width: side,
            height: side,
            data: vec![0.0; (side * side) as usize],
        });
        plus.wcs = wide_wcs(side);
        plus.wavelength = wide_wavelength(side);
        let mut minus = plus.clone();
        minus.image = Plane {
            width: side,
            height: side,
            data: vec![0.0; (side * side) as usize],
        };
        let mut loaded = loaded(plus, minus);
        loaded.view.region = SkyRegion::square(10.0, 20.0, side as f64 * PIXEL_SCALE_DEG);
        let diff = difference(&ctx, &loaded, Product::Values).await.expect("delta");
        assert_eq!((diff.width, diff.height), (side, side));
        for y in 0..side as usize {
            for x in 0..side as usize {
                let got = diff.value[y * side as usize + x];
                let want = (y * side as usize + x) as f32;
                assert!((got - want).abs() < 1e-3, "pixel {x},{y} is {got}, not {want}");
            }
        }
    }

    #[tokio::test]
    async fn the_candidate_product_keeps_the_variance_of_both_sides() {
        let ctx = context();
        let loaded = loaded(cutout(5.0, 1.0, 0, Some(0.0)), cutout(2.0, 3.0, 0, Some(0.0)));
        let diff = difference(&ctx, &loaded, Product::Science).await.expect("delta");
        let centre = diff.value[4];
        assert!((centre - 3.0).abs() < 1e-5, "centre {centre}");
        // Independent variances add.
        assert!((diff.variance[4] - 4.0).abs() < 1e-5, "variance {}", diff.variance[4]);
    }

    #[tokio::test]
    async fn a_delta_keeps_an_outlier_and_drops_a_defect() {
        let ctx = context();
        let plus = cutout(5.0, 1.0, 1 << 19, Some(0.0));
        let minus = cutout(2.0, 1.0, 1 << 6, Some(0.0));
        let diff = difference(&ctx, &loaded(plus, minus), Product::Values).await.expect("delta");
        // The minus side is a NONFUNC defect, so that pixel has no sample and
        // the difference keeps no value there.
        assert!(
            diff.value.iter().all(|value| !value.is_finite()),
            "a defect is not a measurement"
        );
    }

    #[tokio::test]
    async fn a_cutout_without_zodi_is_skipped() {
        let ctx = context();
        let loaded = loaded(cutout(5.0, 1.0, 0, None), cutout(2.0, 1.0, 0, Some(0.0)));
        let diff = difference(&ctx, &loaded, Product::Values).await.expect("delta");
        assert!(
            diff.value.iter().all(|value| !value.is_finite()),
            "no ZODI, no measurement"
        );
    }

    #[test]
    fn rejects_a_wavelength_mismatch() {
        let err = delta_bands(&view(vec![
            Pin {
                center: 30,
                width: 1,
                visit: Some(0),
                sign: Some(Sign::Plus),
            },
            Pin {
                center: 40,
                width: 1,
                visit: Some(1),
                sign: Some(Sign::Minus),
            },
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("same bands"), "{err}");
    }

    #[test]
    fn accepts_the_same_bands_on_two_visits() {
        let bands = delta_bands(&view(vec![
            Pin {
                center: 30,
                width: 5,
                visit: Some(0),
                sign: Some(Sign::Plus),
            },
            Pin {
                center: 30,
                width: 5,
                visit: Some(1),
                sign: Some(Sign::Minus),
            },
        ]))
        .unwrap();
        assert!(bands.contains(&28));
        assert!(bands.contains(&32));
        assert!(!bands.contains(&27));
    }

    #[test]
    fn a_delta_rejects_the_field_the_render_grid_would_crop() {
        let query = |fov: f64| Some(format!("ra=56.75&dec=24.12&fov_l={fov}&fov_h={fov}"));
        assert!(view_of(query(6.99).as_deref()).is_ok());
        let error = view_of(query(7.0).as_deref()).expect_err("7 degrees cannot be drawn at 4096 px");
        assert!(error.to_string().contains("6.997"), "{error}");
    }

    fn view(pins: Vec<Pin>) -> View {
        View {
            region: SkyRegion::square(270.0, 66.5, 0.1),
            mode: Mode::Delta,
            release: "qr3_deep".into(),
            pins,
            mask: true,
        }
    }
}
