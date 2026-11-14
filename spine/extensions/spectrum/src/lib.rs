use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use spearv_core::{
    flag_names,
    math::{spawn_spectrum, ImageMath, MathControl, SpectrumInput, SpectrumPlanes},
    png_pixel_to_sky_rect, query_pairs, sample_at, ApiError, Context, Error, Extension, Filter, Input, ReleaseId,
    SkyRegion, SourceId, ViewQuery, WorkBudget, BANDS,
};
use tokio::sync::Semaphore;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

const RENDER_SECONDS: u64 = 45;
/// Cutout reads in flight for one click. Each read is a 0.02 degree window, a
/// few small HTTP ranges, so the wall time is request latency, not bytes. At 8,
/// a 130-exposure point waits through about 17 serial rounds against the 45s
/// deadline; 32 keeps it to about 5 while each in-flight read stays small.
const READ_CONCURRENCY: usize = 32;
const MEANING: &str = "Each point is the nearest detector pixel, not an aperture sum.";

/// Samples one request may hand to the library. The runtime keeps its own
/// bound; this stops the caller's copy from being unbounded.
const MAX_SPECTRUM_SAMPLES: usize = 65_536;

pub struct Spectrum;

impl Extension for Spectrum {
    fn id(&self) -> &'static str {
        "spectrum"
    }

    fn routes(&self) -> OpenApiRouter<Arc<Context>> {
        OpenApiRouter::new().routes(routes!(spectrum))
    }
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SpectrumQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov_l: f64,
    pub fov_h: f64,
    /// PNG column, origin at the left of the rendered image.
    pub x: u32,
    /// PNG row, origin at the top of the rendered image.
    pub y: u32,
    pub dr: Option<String>,
    pub mask: Option<String>,
    #[serde(default)]
    pub pin: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SpectrumBody {
    pub ra: f64,
    pub dec: f64,
    pub x: u32,
    pub y: u32,
    pub release: String,
    pub meaning: String,
    pub points: Vec<Point>,
    pub pins: Vec<PinMark>,
}

#[derive(Clone, Debug, Serialize, ToSchema, PartialEq)]
pub struct Point {
    pub wavelength_um: f64,
    pub band: Option<u16>,
    pub flux: Option<f64>,
    pub error: Option<f64>,
    pub samples: u32,
    /// Ramp flags and other defects. Not a measurement, even on a SOURCE pixel.
    pub invalid: bool,
    pub flags: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct PinMark {
    pub center: u16,
    pub wavelength_um: f64,
}

#[derive(Clone, Debug)]
pub struct RawSample {
    pub wavelength_um: f64,
    pub flux: f64,
    pub variance: f64,
    pub band: Option<u16>,
    pub invalid: bool,
    pub flags: Vec<String>,
}

/// The library's per-band planes. Valid samples in one band became one
/// noise-weighted point; invalid samples are kept beside it and are not part
/// of the mean.
fn reduce(samples: &[RawSample], planes: &SpectrumPlanes) -> Vec<Point> {
    let mut by_band: Vec<Vec<usize>> = vec![Vec::new(); 102];
    let mut loose = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        match sample.band {
            Some(band) => by_band[band as usize].push(index),
            None => loose.push(point_from(sample)),
        }
    }
    let mut points = Vec::new();
    for (band, members) in by_band.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let valid = planes.counts[band];
        if valid > 0 {
            let last = planes.last_valid[band] as usize;
            points.push(Point {
                wavelength_um: samples[last].wavelength_um,
                band: Some(band as u16),
                flux: Some(planes.value[band]),
                error: Some(planes.error[band]),
                samples: valid as u32,
                invalid: false,
                flags: Vec::new(),
            });
        }
        let invalid = planes.invalid_counts[band];
        if invalid > 0 {
            let first = planes.first_invalid[band] as usize;
            let mut flags = Vec::new();
            let mut flux = None;
            for &index in members {
                if planes.accepted[index] {
                    continue;
                }
                for flag in &samples[index].flags {
                    if !flags.contains(flag) {
                        flags.push(flag.clone());
                    }
                }
                if flux.is_none() && samples[index].flux.is_finite() {
                    flux = Some(samples[index].flux);
                }
            }
            points.push(Point {
                wavelength_um: samples[first].wavelength_um,
                band: Some(band as u16),
                flux,
                error: None,
                samples: invalid as u32,
                invalid: true,
                flags,
            });
        }
    }
    points.extend(loose);
    points.sort_by(|a, b| a.wavelength_um.partial_cmp(&b.wavelength_um).unwrap_or(std::cmp::Ordering::Equal));
    points
}

/// The inverse-variance band mean belongs to the library. This hands it the
/// samples and reads back its planes.
async fn reduce_with_library(
    math: &Arc<dyn ImageMath>,
    budget: &WorkBudget,
    samples: &[RawSample],
) -> Result<Vec<Point>, Error> {
    let flux = samples.iter().map(|sample| sample.flux).collect::<Vec<f64>>();
    let variance = samples.iter().map(|sample| sample.variance).collect::<Vec<f64>>();
    // A sample with no band is not a band sample; the library skips it.
    let band = samples
        .iter()
        .map(|sample| sample.band.unwrap_or(u16::MAX))
        .collect::<Vec<u16>>();
    let rejected = samples.iter().map(|sample| sample.invalid).collect::<Vec<bool>>();
    let input = SpectrumInput::copy_from(budget, MAX_SPECTRUM_SAMPLES, &flux, &variance, &band, &rejected)?;
    let planes = spawn_spectrum(Arc::clone(math), control(), input)?.wait().await?;
    Ok(reduce(samples, planes.get()))
}

/// One control per request. Nothing bumps the generation here, so a session is
/// never stale; the control carries cancellation into the blocking worker.
fn control() -> MathControl {
    MathControl::new(Arc::new(AtomicU64::new(1)))
}

fn point_from(sample: &RawSample) -> Point {
    Point {
        wavelength_um: sample.wavelength_um,
        band: None,
        flux: if sample.flux.is_finite() { Some(sample.flux) } else { None },
        error: if sample.variance.is_finite() && sample.variance > 0.0 {
            Some(sample.variance.sqrt())
        } else {
            None
        },
        samples: 1,
        invalid: sample.invalid,
        flags: sample.flags.clone(),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/spectrum",
    params(SpectrumQuery),
    responses(
        (status = 200, body = SpectrumBody),
        (status = 400, body = ApiError),
        (status = 504, description = "The read exceeded 45s", body = ApiError)
    )
)]
async fn spectrum(
    State(ctx): State<Arc<Context>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<SpectrumBody>, Error> {
    // The library owns the band reduction, so the backend is required at the
    // request boundary: without one this is a server fault, not an empty
    // spectrum.
    let math = ctx.math()?;
    let raw = raw.unwrap_or_default();
    let query = parse_spectrum(&raw);
    let view = ViewQuery {
        ra: query.ra,
        dec: query.dec,
        fov_l: query.fov_l,
        fov_h: query.fov_h,
        mode: Some("slider".into()),
        dr: query.dr.clone(),
        pins: query.pin.clone(),
        mask: query.mask.clone(),
    }
    .parse(10.0)?;
    let (sky_ra, sky_dec) = png_pixel_to_sky_rect(query.x, query.y, view.region.ra, view.region.dec, view.region.fov_l_deg, view.region.fov_h_deg)
        .ok_or_else(|| Error::Message("click is outside the rendered image".into()))?;
    let source = ctx
        .images
        .get(&SourceId::new("spherex"))
        .cloned()
        .ok_or_else(|| Error::Message("SPHEREx source is not registered".into()))?;
    let region = SkyRegion::square(sky_ra, sky_dec, 0.02);
    let exposures = source
        .coverage(
            &region,
            &Filter {
                release: Some(ReleaseId(view.release.clone())),
                wavelength_um: None,
            },
        )
        .await?;
    if exposures.is_empty() {
        return Err(Error::OutsideExposure);
    }
    let started = std::time::Instant::now();
    let mut reads = tokio::task::JoinSet::new();
    let permits = Arc::new(Semaphore::new(READ_CONCURRENCY));
    for exposure in exposures {
        let permits = Arc::clone(&permits);
        let source = Arc::clone(&source);
        let region = region.clone();
        reads.spawn(async move {
            let _permit = permits.acquire().await;
            source.cutout(&exposure, &region).await
        });
    }
    let gathered = tokio::time::timeout(Duration::from_secs(RENDER_SECONDS), async {
        let mut cutouts = Vec::new();
        while let Some(joined) = reads.join_next().await {
            if let Ok(Ok(cutout)) = joined {
                cutouts.push(cutout);
            }
        }
        cutouts
    })
    .await;
    reads.abort_all();
    let cutouts = gathered.map_err(|_| Error::Deadline { seconds: RENDER_SECONDS })?;
    let mut samples = Vec::new();
    for cutout in &cutouts {
        let Some(zodi) = cutout.zodi.as_ref() else { continue };
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
            mask: view.mask,
            ignore_outlier: false,
        };
        if let Some(hit) = sample_at(&input, sky_ra, sky_dec, true) {
            samples.push(RawSample {
                wavelength_um: hit.wavelength_um,
                flux: hit.flux,
                variance: hit.variance,
                band: hit.band,
                invalid: hit.rejected || !hit.flux.is_finite(),
                flags: flag_names(hit.flags, &cutout.flag_bits).into_iter().map(str::to_string).collect(),
            });
        }
    }
    let _ = started;
    let pins = view
        .pins
        .iter()
        .filter_map(|pin| {
            BANDS.get(pin.center as usize).map(|band| PinMark {
                center: pin.center,
                wavelength_um: band.center_um,
            })
        })
        .collect();
    // No hit is an honest empty spectrum, not a reason to skip the reduction.
    let points = if samples.is_empty() {
        Vec::new()
    } else {
        reduce_with_library(&math, &ctx.budget, &samples).await?
    };
    Ok(Json(SpectrumBody {
        ra: sky_ra,
        dec: sky_dec,
        x: query.x,
        y: query.y,
        release: view.release,
        meaning: MEANING.into(),
        points,
        pins,
    }))
}

struct Parsed {
    ra: f64,
    dec: f64,
    fov_l: f64,
    fov_h: f64,
    x: u32,
    y: u32,
    dr: Option<String>,
    mask: Option<String>,
    pin: Vec<String>,
}

fn parse_spectrum(raw: &str) -> Parsed {
    let mut parsed = Parsed {
        ra: f64::NAN,
        dec: f64::NAN,
        fov_l: f64::NAN,
        fov_h: f64::NAN,
        x: u32::MAX,
        y: u32::MAX,
        dr: None,
        mask: None,
        pin: Vec::new(),
    };
    for (key, value) in query_pairs(raw) {
        match key.as_str() {
            "ra" => parsed.ra = value.parse().unwrap_or(f64::NAN),
            "dec" => parsed.dec = value.parse().unwrap_or(f64::NAN),
            "fov_l" => parsed.fov_l = value.parse().unwrap_or(f64::NAN),
            "fov_h" => parsed.fov_h = value.parse().unwrap_or(f64::NAN),
            "x" => parsed.x = value.parse().unwrap_or(u32::MAX),
            "y" => parsed.y = value.parse().unwrap_or(u32::MAX),
            "dr" => parsed.dr = Some(value),
            "mask" => parsed.mask = Some(value),
            "pin" => parsed.pin.push(value),
            _ => {}
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
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

    async fn reduce_samples(samples: &[RawSample]) -> Vec<Point> {
        let (budget, math) = runtime();
        let math: Arc<dyn ImageMath> = Arc::new(math.clone());
        reduce_with_library(&math, budget, samples)
            .await
            .expect("the library reduces the bands")
    }

    #[tokio::test]
    async fn invalid_samples_stay_out_of_the_mean() {
        let points = reduce_samples(&[
            RawSample {
                wavelength_um: 1.50,
                flux: 4.0,
                variance: 1.0,
                band: Some(30),
                invalid: false,
                flags: vec![],
            },
            RawSample {
                wavelength_um: 1.51,
                flux: 8.0,
                variance: 1.0,
                band: Some(30),
                invalid: false,
                flags: vec![],
            },
            RawSample {
                wavelength_um: 1.52,
                flux: 1000.0,
                variance: 1.0,
                band: Some(30),
                invalid: true,
                flags: vec!["OVERFLOW".into()],
            },
        ])
        .await;
        let valid = points.iter().find(|point| !point.invalid).unwrap();
        assert!((valid.flux.unwrap() - 6.0).abs() < 1e-9);
        assert!((valid.error.unwrap() - (0.5_f64).sqrt()).abs() < 1e-9);
        assert_eq!(valid.samples, 2);
        // The wavelength of a band point is its last valid sample.
        assert!((valid.wavelength_um - 1.51).abs() < 1e-12);
        let invalid = points.iter().find(|point| point.invalid).unwrap();
        assert!(invalid.flags.contains(&"OVERFLOW".to_string()));
        assert_eq!(invalid.samples, 1);
        // The invalid companion keeps the first invalid sample's wavelength.
        assert!((invalid.wavelength_um - 1.52).abs() < 1e-12);
    }

    #[tokio::test]
    async fn a_loose_point_stays_a_point_of_its_own() {
        let points = reduce_samples(&[
            RawSample {
                wavelength_um: 1.50,
                flux: 4.0,
                variance: 1.0,
                band: None,
                invalid: false,
                flags: vec![],
            },
        ])
        .await;
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].band, None);
        assert_eq!(points[0].samples, 1);
        assert_eq!(points[0].flux, Some(4.0));
        assert_eq!(points[0].error, Some(1.0));
    }

    #[tokio::test]
    async fn a_band_with_no_valid_sample_is_only_its_invalid_companion() {
        let points = reduce_samples(&[RawSample {
            wavelength_um: 1.50,
            flux: f64::NAN,
            variance: 1.0,
            band: Some(30),
            invalid: true,
            flags: vec!["TRANSIENT".into()],
        }])
        .await;
        assert_eq!(points.len(), 1);
        assert!(points[0].invalid);
        assert_eq!(points[0].flux, None);
        assert_eq!(points[0].samples, 1);
        assert!(points[0].flags.contains(&"TRANSIENT".to_string()));
    }
}
