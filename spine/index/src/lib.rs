//! The SPHEREx exposure index: the rows the index-service stores and the HTTP API
//! it answers.
//!
//! One table holds every release. `release` (`qr2`, `qr3`, ...) and
//! `survey` (`all_sky` or `deep`) are columns, so a new release is new rows and
//! never a schema change. Callers use the `client` feature; only the
//! index-service enables `store` and opens the database.

pub mod api;
#[cfg(feature = "store")]
mod lvf;
pub mod passes;
pub mod wcs_blob;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "store")]
pub mod store;

pub use api::*;

use spearv_core::Wcs;

/// `dr` names a release, e.g. `qr3`, which includes its deep-field exposures.
/// A `_deep` suffix, e.g. `qr3_deep`, selects the deep fields alone.
///
/// Every caller that builds a [`CoverageQuery`] from a `dr` value goes through
/// this one mapping, so the same view always returns the same rows.
pub fn release_and_survey(dr: &str) -> (String, Option<Survey>) {
    match dr.strip_suffix("_deep") {
        Some(release) => (release.to_string(), Some(Survey::Deep)),
        None => (dr.to_string(), None),
    }
}

/// Order processing versions such as `l2b-v27-2026-285` and
/// `l2b_retry-v27-2026-286`. The trailing year and day of year decide; the
/// pipeline version and then the full name break ties. Unparsable versions
/// sort first, so a well-formed version always wins.
pub fn version_order(version: &str) -> (u32, u32, u32, &str) {
    let mut parts = version.rsplit('-');
    let day = parts.next().and_then(|day| day.parse().ok());
    let year = parts.next().and_then(|year| year.parse().ok());
    let pipeline = parts.next().and_then(|v| v.strip_prefix('v')).and_then(|v| v.parse().ok());
    match (year, day) {
        (Some(year), Some(day)) => (year, day, pipeline.unwrap_or(0), version),
        _ => (0, 0, 0, version),
    }
}

/// R*Tree boxes `(ra_min, ra_max, dec_min, dec_max)` that contain a detector's
/// footprint. One box, or two when the footprint crosses RA 0.
///
/// The edges are sampled, not only the corners: a TAN image's edge bulges
/// poleward of its corners. A footprint that contains a celestial pole covers
/// every RA.
pub fn footprint_boxes(wcs: &Wcs) -> Vec<(f64, f64, f64, f64)> {
    const STEPS: usize = 16;
    let (nx, ny) = (wcs.naxis[0] as f64, wcs.naxis[1] as f64);
    let mut points = Vec::with_capacity(4 * STEPS);
    for step in 0..STEPS {
        let t = step as f64 / STEPS as f64;
        points.push(wcs.pixel_to_sky(0.5 + t * nx, 0.5));
        points.push(wcs.pixel_to_sky(0.5 + nx, 0.5 + t * ny));
        points.push(wcs.pixel_to_sky(0.5 + nx - t * nx, 0.5 + ny));
        points.push(wcs.pixel_to_sky(0.5, 0.5 + ny - t * ny));
    }
    let dec_min = points.iter().map(|p| p.1).fold(f64::MAX, f64::min);
    let dec_max = points.iter().map(|p| p.1).fold(f64::MIN, f64::max);
    let contains = |ra: f64, dec: f64| {
        let (x, y) = wcs.sky_to_pixel(ra, dec);
        x.is_finite() && y.is_finite() && x >= 0.5 && y >= 0.5 && x <= nx + 0.5 && y <= ny + 0.5
    };
    // Only a field near the pole can hold it. Far away the TAN inverse of a
    // point behind the tangent plane is meaningless, so it is not asked.
    if wcs.crval[1] > 80.0 && contains(0.0, 90.0) {
        return vec![(0.0, 360.0, dec_min, 90.0)];
    }
    if wcs.crval[1] < -80.0 && contains(0.0, -90.0) {
        return vec![(0.0, 360.0, -90.0, dec_max)];
    }
    let ras: Vec<f64> = points.iter().map(|p| p.0).collect();
    ra_boxes(&ras).into_iter().map(|(lo, hi)| (lo, hi, dec_min, dec_max)).collect()
}

/// RA intervals of a square view, split at RA 0/360. A view that reaches a
/// pole covers every RA.
pub fn view_ra_ranges(ra: f64, dec: f64, fov_deg: f64) -> Vec<(f64, f64)> {
    let half = fov_deg / 2.0;
    if dec.abs() + half >= 89.0 {
        return vec![(0.0, 360.0)];
    }
    let pad = half / (dec.abs() + half).to_radians().cos().max(1e-3);
    if pad >= 180.0 {
        return vec![(0.0, 360.0)];
    }
    let (lo, hi) = (ra - pad, ra + pad);
    if lo < 0.0 {
        return vec![(0.0, hi), (lo + 360.0, 360.0)];
    }
    if hi >= 360.0 {
        return vec![(0.0, hi - 360.0), (lo, 360.0)];
    }
    vec![(lo, hi)]
}

/// True when any of the view's sample points lands on the detector. The
/// R*Tree only knows bounding boxes; this is the exact test against the WCS.
pub fn overlaps(wcs: &Wcs, ra: f64, dec: f64, fov_deg: f64) -> bool {
    // A TAN field sees only the open hemisphere in front of its centre.
    // Reject a detector whose centre is at or behind the view's tangent plane
    // before sampling the lattice: the inverse there is undefined and the far
    // sky would fold back onto the detector. The margin absorbs the rounding
    // of a point that should sit exactly on the plane.
    let view_dec = dec.to_radians();
    let det_dec = wcs.crval[1].to_radians();
    let dra = (wcs.crval[0] - ra).to_radians();
    let cosine = view_dec.sin() * det_dec.sin() + view_dec.cos() * det_dec.cos() * dra.cos();
    if cosine.is_nan() || cosine <= 1e-12 {
        return false;
    }
    let half = fov_deg / 2.0;
    // RA converges toward the pole, so the widest RA offset needed by a view is
    // at the declination edge farthest from the equator. A view touching the
    // pole covers every RA, matching view_ra_ranges and the R*Tree prefilter.
    let ra_half = if dec.abs() + half >= 89.0 {
        180.0
    } else {
        half / (dec.abs() + half).to_radians().cos().max(1e-3)
    };
    let inside = |(r, d): (f64, f64)| {
        let d: f64 = d.clamp(-90.0, 90.0);
        let (x, y) = wcs.sky_to_pixel(r.rem_euclid(360.0), d);
        x.is_finite()
            && y.is_finite()
            && x >= 0.5
            && y >= 0.5
            && x <= wcs.naxis[0] as f64 + 0.5
            && y <= wcs.naxis[1] as f64 + 0.5
    };
    let in_view = |(r, d): (f64, f64)| {
        let dra = (r - ra + 180.0).rem_euclid(360.0) - 180.0;
        dra.abs() <= ra_half && (d - dec).abs() <= half
    };
    // Most overlaps are one of two easy cases: the view's centre is on the
    // detector, or the detector's centre is in the view. Checking those first
    // keeps a 10° query from projecting the whole lattice for every row.
    let centre = wcs.pixel_to_sky(0.5 + wcs.naxis[0] as f64 / 2.0, 0.5 + wcs.naxis[1] as f64 / 2.0);
    if inside((ra, dec)) || in_view(centre) {
        return true;
    }
    // A detector is about 3.5° wide, so on a wide field it can sit between
    // the corners. A lattice every half degree catches it; the detector's own
    // corners catch one that only just enters the field.
    let steps = ((fov_deg / 0.5).ceil() as usize).max(2);
    let along = |i: usize| 2.0 * i as f64 / steps as f64 - 1.0;
    let lattice = (0..=steps)
        .flat_map(|i| (0..=steps).map(move |j| (i, j)))
        .any(|(i, j)| inside((ra + ra_half * along(i), dec + half * along(j))));
    lattice
        || [(0, 0), (1, 0), (0, 1), (1, 1)].into_iter().any(|(cx, cy)| {
            in_view(wcs.pixel_to_sky(
                1.0 + f64::from(cx * (wcs.naxis[0] - 1)),
                1.0 + f64::from(cy * (wcs.naxis[1] - 1)),
            ))
        })
}

/// RA intervals that hold every RA in `ras`: the circle minus its widest gap.
/// One interval, or two split at RA 0 when the points straddle it.
fn ra_boxes(ras: &[f64]) -> Vec<(f64, f64)> {
    let mut values: Vec<f64> = ras.iter().map(|ra| ra.rem_euclid(360.0)).collect();
    if values.is_empty() {
        return vec![];
    }
    values.sort_by(f64::total_cmp);
    let last = values.len() - 1;
    let (mut widest, mut gap_after) = (values[0] + 360.0 - values[last], last);
    for i in 0..last {
        let gap = values[i + 1] - values[i];
        if gap > widest {
            (widest, gap_after) = (gap, i);
        }
    }
    if gap_after == last {
        return vec![(values[0], values[last])];
    }
    vec![(0.0, values[gap_after]), (values[gap_after + 1], 360.0)]
}

#[cfg(test)]
pub(crate) mod testing {
    use spearv_core::{Poly, Sip, Wcs};

    /// A north-up 2040² detector with 6.15″ pixels centred on `(ra, dec)`.
    pub fn detector(ra: f64, dec: f64) -> Wcs {
        let scale = 6.15 / 3600.0;
        Wcs {
            crpix: [1020.5, 1020.5],
            crval: [ra, dec],
            cd: [[-scale, 0.0], [0.0, scale]],
            sip: Sip { a: Poly::new(0), b: Poly::new(0), ap: None, bp: None },
            naxis: [2040, 2040],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing::detector;

    #[test]
    fn a_deep_suffix_keeps_the_release_and_selects_the_deep_fields() {
        assert_eq!(release_and_survey("qr3"), ("qr3".to_string(), None));
        assert_eq!(release_and_survey("qr2_deep"), ("qr2".to_string(), Some(Survey::Deep)));
        assert_eq!(release_and_survey("qr4"), ("qr4".to_string(), None));
    }

    #[test]
    fn versions_order_by_date_not_by_text() {
        assert!(version_order("l2b-v27-2026-045") < version_order("l2b-v27-2026-285"));
        assert!(version_order("l2b-v27-2026-285") < version_order("l2b_retry-v27-2026-286"));
        assert!(version_order("l2b-v20-2025-300") < version_order("l2b-v27-2026-001"));
        // A day that is not zero-padded still sorts by number.
        assert!(version_order("l2b-v27-2026-45") < version_order("l2b-v27-2026-285"));
        assert!(version_order("junk") < version_order("l2b-v1-2020-1"));
    }

    #[test]
    fn ra_boxes_split_at_zero() {
        let boxes = ra_boxes(&[359.0, 1.0, 0.2, 358.5]);
        assert_eq!(boxes, vec![(0.0, 1.0), (358.5, 360.0)]);
        assert_eq!(ra_boxes(&[56.0, 57.5, 56.8]), vec![(56.0, 57.5)]);
        assert!(ra_boxes(&[]).is_empty());
    }

    #[test]
    fn a_footprint_box_holds_the_bulging_edge() {
        let wcs = detector(120.0, 66.0);
        let boxes = footprint_boxes(&wcs);
        assert_eq!(boxes.len(), 1);
        // The top edge's middle sits poleward of the corners.
        let (_, top) = wcs.pixel_to_sky(1020.5, 2040.5);
        assert!(boxes[0].3 >= top - 1e-9, "{boxes:?} vs {top}");
    }

    #[test]
    fn a_footprint_on_the_pole_covers_every_ra() {
        let boxes = footprint_boxes(&detector(10.0, 89.5));
        assert_eq!(boxes, vec![(0.0, 360.0, boxes[0].2, 90.0)]);
        assert!(boxes[0].2 < 88.0);
        let south = footprint_boxes(&detector(200.0, -89.9));
        assert_eq!(south[0].0, 0.0);
        assert_eq!(south[0].2, -90.0);
    }

    #[test]
    fn a_footprint_across_ra_zero_splits_in_two() {
        let boxes = footprint_boxes(&detector(0.5, 10.0));
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        assert!(boxes.iter().any(|b| b.0 == 0.0));
        assert!(boxes.iter().any(|b| b.1 == 360.0));
    }

    #[test]
    fn view_ranges_wrap_and_reach_the_pole() {
        assert_eq!(view_ra_ranges(180.0, 0.0, 2.0).len(), 1);
        let wrap = view_ra_ranges(0.2, 0.0, 1.0);
        assert_eq!(wrap.len(), 2);
        assert!(wrap.iter().any(|r| r.0 == 0.0 && (r.1 - 0.7).abs() < 1e-3), "{wrap:?}");
        assert!(wrap.iter().any(|r| (r.0 - 359.7).abs() < 1e-3 && r.1 == 360.0), "{wrap:?}");
        assert_eq!(view_ra_ranges(10.0, 89.0, 1.0), vec![(0.0, 360.0)]);
    }

    #[test]
    fn a_detector_between_the_sample_points_of_a_wide_field_overlaps() {
        let wcs = detector(55.0, 0.0);
        assert!(overlaps(&wcs, 50.0, 0.0, 10.0));
        assert!(!overlaps(&wcs, 40.0, 0.0, 10.0));
        assert!(overlaps(&wcs, 55.0, 0.0, 0.05));
        assert!(!overlaps(&wcs, 60.0, 0.0, 0.05));
    }

    #[test]
    fn a_detector_edge_that_clips_a_small_view_overlaps() {
        // 1020 px of 6.15″ puts the west edge at RA 53.2575. A 0.3° view at
        // 53.2 has neither centre inside the other, but its east side is on
        // the detector.
        let wcs = detector(55.0, 0.0);
        assert!(overlaps(&wcs, 53.2, 0.0, 0.3));
        assert!(!overlaps(&wcs, 53.0, 0.0, 0.3));
    }

    #[test]
    fn polar_ra_convergence_does_not_hide_a_detector_inside_the_view() {
        assert!(overlaps(&detector(180.0, 88.0), 0.0, 80.0, 30.0));
        assert!(overlaps(&detector(180.0, -88.0), 0.0, -80.0, 30.0));
    }

    #[test]
    fn a_full_hemisphere_view_stops_at_its_tangent_plane() {
        let (ra, dec, fov) = (56.75, 24.12, 180.0);
        assert!(overlaps(&detector(ra, dec), ra, dec, fov));
        assert!(overlaps(&detector(ra + 1.0, dec), ra, dec, fov));

        // Centres at or beyond 90° from the view are behind its tangent
        // plane; without the guard the far sky would fold onto the detector.
        for (r, d) in [
            (ra + 150.0, dec),  // 123.7° away
            (ra + 180.0, dec),  // 131.8° away
            (ra + 90.0, 0.0),   // exactly 90° away
            (ra + 90.0, -dec),  // 99.6° away
            (ra + 150.0, -dec), // 152.6° away
            (ra + 180.0, -dec), // the antipode, 180° away
        ] {
            assert!(
                !overlaps(&detector(r, d), ra, dec, fov),
                "({r}, {d}) must be behind the view's tangent plane"
            );
        }

        // The same +90° in RA at the view's declination is 80.4° away on the
        // sky, so it is still in front of the plane and stays a valid overlap.
        assert!(overlaps(&detector(ra + 90.0, dec), ra, dec, fov));
    }
}
