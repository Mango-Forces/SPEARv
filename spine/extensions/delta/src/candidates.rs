//! Find blobs in a delta and say what they most likely are.
//!
//! Nearest-neighbor sampling leaves false `+` and `-` blobs on fixed stars, so
//! a blob is not a detection. This only labels blobs. It never changes the image.

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::Json;
use serde::Serialize;
use spearv_core::{
    flag_names, math::Product, png_pixel_to_sky_rect, sample_at, ApiError, CatalogRow, Context, Cutout, Error, Exposure,
    SkyRegion, SourceId, PIXEL_SCALE_DEG,
};
use utoipa::ToSchema;

use crate::{difference, input_of, load, DeltaQuery, Loaded, Stacked};

/// Seed threshold in sigma, after the noise scale. See `noise_scale`.
pub const SEED_SIGMA: f64 = 5.0;
/// A blob grows through neighbours of the same sign down to this.
pub const GROW_SIGMA: f64 = 3.0;
/// How far a residue peak sits from its star. Blob-to-star offsets on real deltas
/// at band 25: p95 1.35 to 1.45 px, max 1.58 px (`tools/verify/candidates.py`).
/// Each side's nearest-neighbor sample and the peak's own pixel each add up to
/// 0.71 px, so this is a sampling limit, not a seeing limit.
pub const STAR_MATCH_PX: f64 = 1.6;
/// Median peak of a G=15 star at band 25, in MJy/sr: 3.76 and 4.43 in the two
/// Geometria exposures. One band. Other bands are looser; see `COLOUR_MARGIN`.
pub const PEAK_AT_G15: f64 = 4.0;
/// How far a real residue can exceed that peak, for star colour. The worst
/// SOURCE-in-both blob at band 25 is 5.11 times it. At band 60 a few reach 22
/// times it, so this rule misses those, and SOURCE or a dipole has to catch them.
pub const COLOUR_MARGIN: f64 = 6.0;
/// Peaks of a dipole's two lobes. On real deltas they sit 1 to 2 px apart, with
/// a gap after that.
pub const DIPOLE_PX: f64 = 2.0;
/// Real dipoles measured 0.15 to 1.0. Below that the pair is a chance neighbour.
pub const DIPOLE_MIN_RATIO: f64 = 0.15;
/// Each blob costs one catalog read, so a full 1 degree delta stays bounded.
const MAX_CANDIDATES: usize = 500;
const SOURCE_SAMPLES_PER_SIDE: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub enum BlobSign {
    #[serde(rename = "+")]
    Plus,
    #[serde(rename = "-")]
    Minus,
}

/// One connected blob. `x` and `y` are its peak on the PNG grid, origin top-left.
#[derive(Clone, Debug, PartialEq)]
pub struct Blob {
    pub x: u32,
    pub y: u32,
    pub sign: BlobSign,
    pub significance: f64,
    pub delta: f64,
    pub pixels: u32,
}

pub struct Noise {
    /// Median of the delta, removed before measuring: it is a sky offset, not a source.
    pub offset: f64,
    /// Robust scatter of delta/sqrt(VARIANCE). 1 means VARIANCE describes the field.
    pub scale: f64,
}

/// Robust offset and scatter. The scale never goes below 1: VARIANCE is the
/// floor, and a quiet field must not make its blobs look stronger.
pub fn noise(diff: &Stacked) -> Option<Noise> {
    let mut values: Vec<f64> = (0..diff.value.len())
        .filter(|&i| usable(diff, i))
        .map(|i| f64::from(diff.value[i]))
        .collect();
    if values.is_empty() {
        return None;
    }
    let offset = median(&mut values);
    let mut scaled: Vec<f64> = (0..diff.value.len())
        .filter(|&i| usable(diff, i))
        .map(|i| (f64::from(diff.value[i]) - offset) / f64::from(diff.variance[i]).sqrt())
        .collect();
    let center = median(&mut scaled);
    let mut deviations: Vec<f64> = scaled.iter().map(|value| (value - center).abs()).collect();
    let scale = (1.4826 * median(&mut deviations)).max(1.0);
    Some(Noise { offset, scale })
}

fn usable(diff: &Stacked, i: usize) -> bool {
    diff.value[i].is_finite() && diff.variance[i].is_finite() && diff.variance[i] > 0.0
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        return values[middle];
    }
    (values[middle - 1] + values[middle]) / 2.0
}

/// Connected same-sign blobs with a peak at or above `SEED_SIGMA`, strongest first.
pub fn blobs(diff: &Stacked, noise: &Noise) -> Vec<Blob> {
    let width = diff.width as usize;
    let height = diff.height as usize;
    let sigma: Vec<f64> = (0..diff.value.len())
        .map(|i| {
            if !usable(diff, i) {
                return 0.0;
            }
            (f64::from(diff.value[i]) - noise.offset) / f64::from(diff.variance[i]).sqrt() / noise.scale
        })
        .collect();
    let mut seen = vec![false; sigma.len()];
    let mut found = Vec::new();
    for start in 0..sigma.len() {
        if seen[start] || sigma[start].abs() < GROW_SIGMA {
            continue;
        }
        let positive = sigma[start] > 0.0;
        let mut queue = vec![start];
        seen[start] = true;
        let mut peak = start;
        let mut pixels = 0;
        while let Some(i) = queue.pop() {
            pixels += 1;
            if sigma[i].abs() > sigma[peak].abs() {
                peak = i;
            }
            let (x, y) = ((i % width) as i64, (i / width) as i64);
            for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                let (nx, ny) = (x + dx, y + dy);
                if nx < 0 || ny < 0 || nx >= width as i64 || ny >= height as i64 {
                    continue;
                }
                let next = ny as usize * width + nx as usize;
                if !seen[next] && sigma[next].abs() >= GROW_SIGMA && (sigma[next] > 0.0) == positive {
                    seen[next] = true;
                    queue.push(next);
                }
            }
        }
        if sigma[peak].abs() < SEED_SIGMA {
            continue;
        }
        found.push(Blob {
            x: (peak % width) as u32,
            // `Stacked` rows are FITS order, bottom first. The PNG is top first.
            y: (height - 1 - peak / width) as u32,
            sign: if positive { BlobSign::Plus } else { BlobSign::Minus },
            significance: sigma[peak].abs(),
            delta: f64::from(diff.value[peak]) - noise.offset,
            pixels,
        });
    }
    found.sort_by(|a, b| b.significance.total_cmp(&a.significance));
    found
}

/// What a blob is most likely to be. `MovingCandidate` is a label for follow-up,
/// never a claim that something moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandidateClass {
    StarResidue,
    MovingCandidate,
    Unknown,
}

/// A Gaia star near a blob, at the epoch of the blob's exposure.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct NearStar {
    pub id: String,
    /// Gaia G.
    pub mag: Option<f64>,
    pub separation_px: f64,
    /// True when this star is close and bright enough to leave this blob.
    pub explains: bool,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Candidate {
    /// Peak column in image space, origin at the left, where a pixel centre is
    /// at index + 0.5. The same convention as catalog marker coordinates.
    pub x: f64,
    /// Peak row in image space, origin at the top, pixel centre at index + 0.5.
    pub y: f64,
    pub ra: f64,
    pub dec: f64,
    pub sign: BlobSign,
    /// Peak delta over sqrt(VARIANCE of both sides), divided by `noise_scale`.
    pub significance: f64,
    /// Peak delta in MJy/sr, after the field median is removed.
    pub delta_mjy_sr: f64,
    /// Pixels in the blob. A lone pixel can be a cosmic ray.
    pub pixels: u32,
    pub class: CandidateClass,
    pub reason: String,
    /// The exposure holding the excess: the `+` side for a `+` blob, the `-` side for a `-` blob.
    pub exposure: String,
    pub mjd: f64,
    /// FLAGS bit names of that exposure at the peak.
    pub flags: Vec<String>,
    /// The brightest exposure on the opposite side, and its FLAGS at the peak.
    pub other_exposure: String,
    pub other_flags: Vec<String>,
    /// Nearest Gaia star that explains the blob, or failing that the nearest one
    /// searched. Absent when none is near or Gaia does not cover this sky.
    pub gaia: Option<NearStar>,
}

#[derive(Default, Serialize, ToSchema)]
pub struct CandidateCounts {
    pub star_residue: u32,
    pub moving_candidate: u32,
    pub unknown: u32,
}

#[derive(Serialize, ToSchema)]
pub struct CandidatesBody {
    pub note: String,
    pub threshold_sigma: f64,
    /// Robust scatter of delta/sqrt(VARIANCE) in this field, never below 1.
    pub noise_scale: f64,
    /// Share of sampled delta pixels with SOURCE on both sides. Anything on
    /// those pixels is classed `star_residue`, including a real moving object.
    pub source_both_fraction: f64,
    /// True when more than the strongest 500 blobs were found.
    pub truncated: bool,
    pub counts: CandidateCounts,
    pub candidates: Vec<Candidate>,
}

const NOTE: &str = "Labels for follow-up, not detections. moving_candidate means no fixed-star evidence was found here, not that anything moved. Nothing is removed from the image.";

/// One exposure's sample at a blob peak.
struct Side {
    exposure: String,
    mjd: f64,
    flux: f64,
    flags: Vec<String>,
    source: bool,
}

/// What the catalog says about one blob. Nothing is decided here.
pub enum GaiaView {
    /// Stars near the blob at the exposure epoch. An empty list means Gaia looked and found none.
    Stars(Vec<CatalogRow>),
    /// Gaia does not hold this sky, so silence is not evidence.
    NoCoverage,
}

/// The nearest star that explains the blob, or else the nearest star.
pub fn nearest_star(blob: &Blob, ra: f64, dec: f64, rows: &[CatalogRow]) -> Option<NearStar> {
    let mut stars: Vec<NearStar> = rows
        .iter()
        .map(|row| {
            let separation_px = separation_deg(ra, dec, row.ra, row.dec) / PIXEL_SCALE_DEG;
            NearStar {
                id: row.id.clone(),
                mag: row.mag,
                separation_px,
                explains: separation_px <= STAR_MATCH_PX && bright_enough(row.mag, blob.delta.abs()),
            }
        })
        .collect();
    stars.sort_by(|a, b| b.explains.cmp(&a.explains).then(a.separation_px.total_cmp(&b.separation_px)));
    stars.into_iter().next()
}

/// A residue cannot be brighter than the star that left it. A row with no G
/// cannot rule its star out, so it counts as bright enough.
fn bright_enough(mag: Option<f64>, delta: f64) -> bool {
    mag.is_none_or(|g| COLOUR_MARGIN * PEAK_AT_G15 * 10_f64.powf(-0.4 * (g - 15.0)) >= delta)
}

/// Haversine, so it holds at arcsecond scale and across RA 0.
fn separation_deg(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let half_dec = ((dec2 - dec1) / 2.0).to_radians().sin();
    let half_ra = ((ra2 - ra1) / 2.0).to_radians().sin();
    let h = half_dec * half_dec + dec1.to_radians().cos() * dec2.to_radians().cos() * half_ra * half_ra;
    (2.0 * h.sqrt().min(1.0).asin()).to_degrees()
}

/// The opposite-sign partner this blob shares a star with, if any.
pub fn dipole_partner<'a>(blob: &Blob, all: &'a [Blob]) -> Option<&'a Blob> {
    all.iter()
        .filter(|other| other.sign != blob.sign)
        .filter(|other| {
            let (a, b) = (blob.delta.abs(), other.delta.abs());
            a.min(b) >= DIPOLE_MIN_RATIO * a.max(b)
        })
        .map(|other| {
            let distance = (f64::from(blob.x) - f64::from(other.x)).hypot(f64::from(blob.y) - f64::from(other.y));
            (other, distance)
        })
        .filter(|(_, distance)| *distance <= DIPOLE_PX)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(other, _)| other)
}

/// Any one piece of fixed-sky evidence makes a blob `star_residue`. Calling a
/// star a mover is the dangerous error, so the evidence is additive, and silence
/// from a catalog that does not cover the sky is `Unknown`, not `MovingCandidate`.
pub fn classify(
    blob: &Blob,
    partner: Option<&Blob>,
    source_both: bool,
    star: Option<&NearStar>,
    covered: bool,
) -> (CandidateClass, String) {
    let mut evidence = Vec::new();
    if let Some(star) = star.filter(|star| star.explains) {
        evidence.push(format!(
            "Gaia {} ({}) is {:.2} px away at the exposure epoch",
            star.id,
            magnitude(star.mag),
            star.separation_px
        ));
    }
    if source_both {
        evidence.push("SOURCE is set at this sky in both visits".to_string());
    }
    if let Some(other) = partner {
        evidence.push(format!("it forms a dipole with the opposite blob at {},{}", other.x, other.y));
    }
    if !evidence.is_empty() {
        return (CandidateClass::StarResidue, format!("Fixed sky: {}.", evidence.join("; ")));
    }
    let lone = if blob.pixels == 1 { " A lone pixel can be a cosmic ray." } else { "" };
    if !covered {
        return (
            CandidateClass::Unknown,
            format!("Gaia does not cover this sky, and neither SOURCE nor a dipole marks it as fixed.{lone}"),
        );
    }
    let gaia = match star {
        Some(star) if star.separation_px <= STAR_MATCH_PX => format!(
            "Gaia {} ({}) is {:.2} px away but too faint to leave {:.2} MJy/sr",
            star.id,
            magnitude(star.mag),
            star.separation_px,
            blob.delta.abs()
        ),
        Some(star) => format!(
            "the nearest Gaia star ({}) is {:.2} px away, outside {STAR_MATCH_PX} px",
            magnitude(star.mag),
            star.separation_px
        ),
        None => format!("no Gaia star within {STAR_MATCH_PX} px"),
    };
    (
        CandidateClass::MovingCandidate,
        format!("No fixed-star evidence: {gaia}, SOURCE is not set in both visits, and there is no dipole. Needs follow-up.{lone}"),
    )
}

fn magnitude(mag: Option<f64>) -> String {
    mag.map_or("no G".to_string(), |g| format!("G {g:.2}"))
}

/// Exposures that fed the stack at this sky position, brightest first.
fn sides(side: &[(Exposure, Cutout)], ra: f64, dec: f64, loaded: &Loaded) -> Vec<Side> {
    let mut hits: Vec<Side> = side
        .iter()
        .filter_map(|(exposure, cutout)| {
            let input = input_of(cutout, loaded.view.mask)?;
            let hit = sample_at(&input, ra, dec, false)?;
            if hit.rejected || !hit.flux.is_finite() || !hit.band.is_some_and(|band| loaded.bands.contains(&band)) {
                return None;
            }
            let flags = flag_names(hit.flags, &cutout.flag_bits);
            Some(Side {
                exposure: exposure.obsid.clone(),
                mjd: exposure.mjd,
                flux: hit.flux,
                source: flags.contains(&"SOURCE"),
                flags: flags.into_iter().map(String::from).collect(),
            })
        })
        .collect();
    hits.sort_by(|a, b| b.flux.total_cmp(&a.flux));
    hits
}

/// Stars around one blob at one epoch. Extensions reach Gaia only through the context.
async fn gaia_near(ctx: &Context, ra: f64, dec: f64, mjd: f64) -> Result<GaiaView, Error> {
    let Some(gaia) = ctx.catalogs.get(&SourceId::new("gaia")) else {
        return Ok(GaiaView::NoCoverage);
    };
    let region = SkyRegion::square(ra, dec, 2.0 * STAR_MATCH_PX * PIXEL_SCALE_DEG);
    match gaia.cone(&region, Some(mjd)).await {
        Ok(rows) => Ok(GaiaView::Stars(rows)),
        Err(Error::OutsideExposure) => Ok(GaiaView::NoCoverage),
        // A store that failed is not a store that has no stars here.
        Err(Error::Message(text)) => Err(Error::Upstream(text)),
        Err(other) => Err(other),
    }
}

/// How much of the delta a blob cannot be told from a star on, by SOURCE alone.
fn source_both_fraction(loaded: &Loaded, diff: &Stacked) -> f64 {
    let region = &loaded.view.region;
    let step = (diff.width / SOURCE_SAMPLES_PER_SIDE).max(1) as usize;
    let width = diff.width as usize;
    let height = diff.height as usize;
    let mut sampled = 0_u32;
    let mut both = 0_u32;
    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            if !diff.value[y * width + x].is_finite() {
                continue;
            }
            let Some((ra, dec)) = png_pixel_to_sky_rect(x as u32, (height - 1 - y) as u32, region.ra, region.dec, region.fov_l_deg, region.fov_h_deg) else {
                continue;
            };
            sampled += 1;
            let plus = sides(&loaded.plus, ra, dec, loaded);
            let minus = sides(&loaded.minus, ra, dec, loaded);
            if plus.iter().any(|side| side.source) && minus.iter().any(|side| side.source) {
                both += 1;
            }
        }
    }
    if sampled == 0 {
        return 0.0;
    }
    f64::from(both) / f64::from(sampled)
}

#[utoipa::path(
    get,
    path = "/api/v1/delta/candidates",
    params(DeltaQuery),
    responses(
        (status = 200, description = "Significant blobs in the delta for these same view parameters, each labelled for follow-up", body = CandidatesBody),
        (status = 400, description = "Pins disagree in wavelength, or a sign is missing", body = ApiError),
        (status = 502, description = "A catalog read failed", body = ApiError),
        (status = 504, description = "The read exceeded 600s", body = ApiError)
    )
)]
pub(crate) async fn candidates(
    State(ctx): State<Arc<Context>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<CandidatesBody>, Error> {
    let loaded = load(&ctx, raw.as_deref()).await?;
    // The candidates measure significance against the variance of both sides,
    // so this asks for the science product, not the picture's values.
    let diff = difference(&ctx, &loaded, Product::Science).await?;
    let region = &loaded.view.region;
    let Some(noise) = noise(&diff) else {
        return Ok(Json(CandidatesBody {
            note: NOTE.into(),
            threshold_sigma: SEED_SIGMA,
            noise_scale: 1.0,
            source_both_fraction: 0.0,
            truncated: false,
            counts: CandidateCounts::default(),
            candidates: Vec::new(),
        }));
    };
    let found = blobs(&diff, &noise);
    let mut counts = CandidateCounts::default();
    let mut out = Vec::new();
    for blob in found.iter().take(MAX_CANDIDATES) {
        let Some((ra, dec)) = png_pixel_to_sky_rect(blob.x, blob.y, region.ra, region.dec, region.fov_l_deg, region.fov_h_deg) else {
            continue;
        };
        let plus = sides(&loaded.plus, ra, dec, &loaded);
        let minus = sides(&loaded.minus, ra, dec, &loaded);
        let (own, other) = match blob.sign {
            BlobSign::Plus => (&plus, &minus),
            BlobSign::Minus => (&minus, &plus),
        };
        // A delta pixel exists only where both sides have a sample, so neither list is empty.
        let (Some(first), Some(second)) = (own.first(), other.first()) else {
            continue;
        };
        let source_both = own.iter().any(|side| side.source) && other.iter().any(|side| side.source);
        let gaia = gaia_near(&ctx, ra, dec, first.mjd).await?;
        let (star, covered) = match &gaia {
            GaiaView::Stars(rows) => (nearest_star(blob, ra, dec, rows), true),
            GaiaView::NoCoverage => (None, false),
        };
        let (class, reason) = classify(blob, dipole_partner(blob, &found), source_both, star.as_ref(), covered);
        match class {
            CandidateClass::StarResidue => counts.star_residue += 1,
            CandidateClass::MovingCandidate => counts.moving_candidate += 1,
            CandidateClass::Unknown => counts.unknown += 1,
        }
        out.push(Candidate {
            // Centres, matching `sky_to_png_pixel_rect` for catalog markers, so the
            // web layer draws every marker with the same convention.
            x: blob.x as f64 + 0.5,
            y: blob.y as f64 + 0.5,
            ra,
            dec,
            sign: blob.sign,
            significance: blob.significance,
            delta_mjy_sr: blob.delta,
            pixels: blob.pixels,
            class,
            reason,
            exposure: first.exposure.clone(),
            mjd: first.mjd,
            flags: first.flags.clone(),
            other_exposure: second.exposure.clone(),
            other_flags: second.flags.clone(),
            gaia: star,
        });
    }
    Ok(Json(CandidatesBody {
        note: NOTE.into(),
        threshold_sigma: SEED_SIGMA,
        noise_scale: noise.scale,
        source_both_fraction: source_both_fraction(&loaded, &diff),
        truncated: found.len() > MAX_CANDIDATES,
        counts,
        candidates: out,
    }))
}
