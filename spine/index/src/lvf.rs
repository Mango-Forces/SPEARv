//! Which wavelengths an exposure can put on a view, from its stored WCS and
//! wavelength range alone.

use spearv_core::{SkyRegion, Wcs};

/// Fraction of the detector wavelength span from the long-wavelength edge.
///
/// SPHEREx puts the long wavelength near detector row 1 and the short wavelength
/// near row 2040. These knots are the same on QR2 and QR3 detector 2, and they
/// match detector 3 to about 0.01 µm. X changes the wavelength by about 0.01 µm,
/// which the caller covers with a margin.
const ROW_FRACTION: [(f64, f64); 5] = [(100.0, 0.034), (500.0, 0.295), (1000.0, 0.562), (1500.0, 0.800), (1900.0, 0.975)];

/// Wavelength in µm at a 1-based detector row, from the file's own wavelength range.
fn detector_wavelength(em_min_um: f64, em_max_um: f64, y: f64) -> f64 {
    em_max_um + (em_min_um - em_max_um) * row_fraction(y)
}

fn row_fraction(y: f64) -> f64 {
    if y <= ROW_FRACTION[0].0 {
        return ROW_FRACTION[0].1;
    }
    ROW_FRACTION
        .windows(2)
        .find(|pair| y <= pair[1].0)
        .map(|pair| pair[0].1 + (y - pair[0].0) / (pair[1].0 - pair[0].0) * (pair[1].1 - pair[0].1))
        .unwrap_or(ROW_FRACTION[ROW_FRACTION.len() - 1].1)
}

/// True when this exposure can put part of `wavelength` onto the view.
/// A file whose band strip misses the view is false.
pub(crate) fn exposure_covers_wavelength(
    wcs: &Wcs,
    em_min_um: f64,
    em_max_um: f64,
    region: &SkyRegion,
    wavelength: (f64, f64),
) -> bool {
    let (low, high) = wavelength;
    if em_max_um < low - 0.05 || em_min_um > high + 0.05 {
        return false;
    }
    // Model error is about 0.01 µm. The view corners already cover the field.
    let margin = 0.02;
    samples(region).into_iter().any(|(ra, dec)| {
        let (_, y) = wcs.sky_to_pixel(ra, dec);
        if !y.is_finite() || y < 0.5 || y > wcs.naxis[1] as f64 + 0.5 {
            return false;
        }
        let wave = detector_wavelength(em_min_um, em_max_um, y);
        wave >= low - margin && wave <= high + margin
    })
}

/// The view's centre, corners and edge midpoints.
fn samples(region: &SkyRegion) -> [(f64, f64); 9] {
    let half_l = region.fov_l_deg / 2.0;
    let half_h = region.fov_h_deg / 2.0;
    let ra_half = half_l / region.dec.to_radians().cos().abs().max(1e-3);
    let (ra, dec) = (region.ra, region.dec);
    [
        (ra, dec),
        (ra - ra_half, dec - half_h),
        (ra + ra_half, dec - half_h),
        (ra - ra_half, dec + half_h),
        (ra + ra_half, dec + half_h),
        (ra, dec - half_h),
        (ra, dec + half_h),
        (ra - ra_half, dec),
        (ra + ra_half, dec),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_band_on_the_other_side_of_the_detector_is_not_read() {
        let region = SkyRegion::square(270.146, 66.523, 0.1);
        let at_row = |y: f64| Wcs { crpix: [1020.5, y], ..crate::testing::detector(270.146, 66.523) };
        // Detector 2 range. Row 1900 is the short-wavelength end, about 1.12 µm.
        assert!(
            !exposure_covers_wavelength(&at_row(1900.0), 1.1033, 1.6499, &region, (1.329, 1.363)),
            "band 25 is not at the short-wavelength edge"
        );
        // Row 1000 is about 1.35 µm, which is band 25.
        assert!(exposure_covers_wavelength(&at_row(1000.0), 1.1033, 1.6499, &region, (1.329, 1.363)));
    }

    #[test]
    fn row_fraction_interpolates_between_knots_and_clamps_outside() {
        assert_eq!(row_fraction(1.0), 0.034);
        assert_eq!(row_fraction(2040.0), 0.975);
        assert!((row_fraction(750.0) - 0.4285).abs() < 1e-12);
        assert!((detector_wavelength(1.0, 2.0, 1000.0) - 1.438).abs() < 1e-12);
    }
}
