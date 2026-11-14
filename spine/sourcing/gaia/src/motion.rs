//! Proper motion and small-angle sky geometry.

/// Gaia DR3 astrometry is referred to Julian year 2016.0.
pub const GAIA_DR3_REF_EPOCH: f64 = 2016.0;

const MAS_PER_RADIAN: f64 = 3_600_000.0 * 180.0 / std::f64::consts::PI;
const DAYS_PER_JULIAN_YEAR: f64 = 365.25;
const J2000_MJD: f64 = 51544.5;

pub fn julian_year_to_mjd(year: f64) -> f64 {
    J2000_MJD + (year - 2000.0) * DAYS_PER_JULIAN_YEAR
}

/// Julian years from `ref_epoch` (a Julian year) to `mjd`.
pub fn years_since(ref_epoch: f64, mjd: f64) -> f64 {
    (mjd - julian_year_to_mjd(ref_epoch)) / DAYS_PER_JULIAN_YEAR
}

/// Move a star along its proper motion. `pmra` is Gaia's `pmra`, which already
/// contains cos(dec), in mas/yr. The step is taken on the unit vector in the
/// local east and north directions, so it stays correct at the poles and across
/// RA 0, where adding `pmra / cos(dec)` to RA does not. Parallax and radial
/// velocity are not in the store. Leaving them out costs 63 mas for Barnard's
/// star at MJD 61000 (`tools/verify/gaia_motion.py`), the worst case in the sky,
/// which is 1% of a SPHEREx pixel.
pub fn propagate(ra: f64, dec: f64, pmra: f64, pmdec: f64, years: f64) -> (f64, f64) {
    let (sin_a, cos_a) = ra.to_radians().sin_cos();
    let (sin_d, cos_d) = dec.to_radians().sin_cos();
    let east = pmra * years / MAS_PER_RADIAN;
    let north = pmdec * years / MAS_PER_RADIAN;
    let x = cos_d * cos_a - sin_a * east - sin_d * cos_a * north;
    let y = cos_d * sin_a + cos_a * east - sin_d * sin_a * north;
    let z = sin_d + cos_d * north;
    (
        y.atan2(x).to_degrees().rem_euclid(360.0),
        z.atan2(x.hypot(y)).to_degrees(),
    )
}

/// Great-circle separation in degrees. Haversine, so it holds at arcsecond scale.
pub fn separation_deg(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let half_dec = ((dec2 - dec1) / 2.0).to_radians().sin();
    let half_ra = ((ra2 - ra1) / 2.0).to_radians().sin();
    let h = half_dec * half_dec + dec1.to_radians().cos() * dec2.to_radians().cos() * half_ra * half_ra;
    (2.0 * h.sqrt().min(1.0).asin()).to_degrees()
}

/// RA and Dec boxes that contain a circle. Two boxes when it crosses RA 0.
pub fn circle_boxes(ra: f64, dec: f64, radius: f64) -> Vec<(f64, f64, f64, f64)> {
    let low = (dec - radius).max(-90.0);
    let high = (dec + radius).min(90.0);
    if dec.abs() + radius >= 90.0 {
        return vec![(0.0, 360.0, low, high)];
    }
    let half = (radius.to_radians().sin() / dec.to_radians().cos()).min(1.0).asin().to_degrees();
    let west = (ra - half).rem_euclid(360.0);
    let east = (ra + half).rem_euclid(360.0);
    if west > east {
        return vec![(west, 360.0, low, high), (0.0, east, low, high)];
    }
    vec![(west, east, low, high)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAS: f64 = 1.0 / 3_600_000.0;

    fn sep_mas(a: (f64, f64), b: (f64, f64)) -> f64 {
        separation_deg(a.0, a.1, b.0, b.1) / MAS
    }

    #[test]
    fn gaia_reference_epoch_is_mjd_57388_5() {
        assert_eq!(julian_year_to_mjd(GAIA_DR3_REF_EPOCH), 57388.5);
        assert_eq!(years_since(2016.0, 57388.5 + 365.25), 1.0);
    }

    #[test]
    fn no_time_or_no_motion_leaves_the_star_alone() {
        let start = (58.2783, 26.1398);
        assert!(sep_mas(propagate(start.0, start.1, 100.0, -50.0, 0.0), start) < 1e-6);
        assert!(sep_mas(propagate(start.0, start.1, 0.0, 0.0, 9.0), start) < 1e-6);
    }

    #[test]
    fn pmra_already_contains_cos_dec() {
        // 1000 mas/yr of pmra at Dec 60 is 2000 mas of RA coordinate.
        let (ra, dec) = propagate(100.0, 60.0, 1000.0, 0.0, 1.0);
        assert!(((ra - 100.0) / MAS - 2000.0).abs() < 0.01, "{ra}");
        assert!((dec - 60.0).abs() / MAS < 0.01, "{dec}");
    }

    #[test]
    fn negative_time_runs_the_motion_backward() {
        let forward = propagate(56.75, 24.12, 20.0, -45.0, 10.0);
        let back = propagate(forward.0, forward.1, 20.0, -45.0, -10.0);
        assert!(sep_mas(back, (56.75, 24.12)) < 0.001);
    }

    #[test]
    fn motion_crosses_ra_zero() {
        let (ra, _) = propagate(359.9999, 0.0, 1000.0, 0.0, 1.0);
        assert!(ra < 1.0, "{ra}");
        let (back, _) = propagate(0.0001, 0.0, -1000.0, 0.0, 1.0);
        assert!(back > 359.0, "{back}");
    }

    #[test]
    fn motion_passes_over_the_pole_without_blowing_up() {
        // 100 arcsec north from 50 arcsec below the pole lands 50 arcsec past it.
        let start_dec = 90.0 - 50.0 / 3600.0;
        let (ra, dec) = propagate(10.0, start_dec, 0.0, 10_000.0, 10.0);
        assert!((ra - 190.0).abs() < 1e-6, "{ra}");
        assert!(((90.0 - dec) * 3600.0 - 50.0).abs() < 1e-3, "{dec}");
    }

    /// Expected values are Astropy 8.0.2 `SkyCoord.apply_space_motion` (ERFA
    /// `starpm`), from `tools/verify/gaia_motion.py`, at 1 kpc with zero radial
    /// velocity. That tool holds distance out of the problem: this function has none.
    #[test]
    fn matches_astropy_space_motion() {
        for (ra, dec, pmra, pmdec, mjd, want_ra, want_dec) in ASTROPY_CASES {
            let got = propagate(ra, dec, pmra, pmdec, years_since(GAIA_DR3_REF_EPOCH, mjd));
            let off = sep_mas(got, (want_ra, want_dec));
            assert!(off < 0.5, "{ra},{dec}: {got:?} is {off} mas from Astropy");
        }
    }

    const ASTROPY_CASES: [(f64, f64, f64, f64, f64, f64, f64); 6] = [
        (269.44850252543836, 4.739420051112412, -801.551, 10362.394, 61000.0, 269.44629334378516, 4.767881361960273),
        (269.44850252543836, 4.739420051112412, -801.551, 10362.394, 57000.0, 269.448740162964, 4.73635838098079),
        (58.2783, 26.1398, 20.0, -45.0, 60909.296714453, 58.27835965351152, 26.13967950728889),
        (270.146, 66.523, 1500.0, -2300.0, 61082.694, 270.1565756471305, 66.51653782410791),
        (359.9999, -30.0, 3000.0, 500.0, 61000.0, 0.009414358165229309, -29.998626359617624),
        (123.0, 89.99, 5000.0, 4000.0, 61000.0, 217.10827673174555, 89.98623163865913),
    ];

    #[test]
    fn separation_is_right_at_small_and_large_angles() {
        assert!((separation_deg(10.0, 0.0, 10.0, 1.0) - 1.0).abs() < 1e-12);
        assert!((separation_deg(359.5, 0.0, 0.5, 0.0) - 1.0).abs() < 1e-12);
        assert!((separation_deg(0.0, 89.0, 180.0, 89.0) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn circle_boxes_split_at_ra_zero_and_open_at_the_pole() {
        assert_eq!(circle_boxes(100.0, 0.0, 1.0).len(), 1);
        let wrapped = circle_boxes(0.2, 0.0, 1.0);
        assert_eq!(wrapped.len(), 2, "{wrapped:?}");
        assert!(wrapped.iter().any(|b| b.0 > 358.0 && b.1 == 360.0));
        assert!(wrapped.iter().any(|b| b.0 == 0.0 && b.1 > 1.0));
        assert_eq!(circle_boxes(5.0, 89.5, 1.0), vec![(0.0, 360.0, 88.5, 90.0)]);
    }

    #[test]
    fn circle_box_widens_with_declination() {
        let (west, east, ..) = circle_boxes(270.146, 66.523, 1.0)[0];
        let half = (east - west) / 2.0;
        // asin(sin 1 / cos 66.523): the tangent meridian of the circle.
        assert!((half - 2.5108).abs() < 0.001, "{half}");
    }
}
