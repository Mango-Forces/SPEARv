use crate::{bad_delta_pixel, bad_pixel, bad_spectrum_pixel, band_of, FlagBits, WavelengthMap, Wcs};

/// One exposure already cut to the view, in detector pixels.
pub struct Input<'a> {
    pub wcs: &'a Wcs,
    pub image: &'a [f32],
    pub variance: &'a [f32],
    pub flags: &'a [u32],
    pub zodi: &'a [f32],
    pub wavelength: &'a WavelengthMap,
    pub origin_fits: (i32, i32),
    pub width: usize,
    pub flag_bits: FlagBits,
    pub mask: bool,
    /// Delta keeps `OUTLIER`. Composite and spectra leave this false.
    pub ignore_outlier: bool,
}

pub struct Hit {
    pub flux: f64,
    pub variance: f64,
    pub wavelength_um: f64,
    pub band: Option<u16>,
    pub flags: u32,
    /// True when the mask policy rejects this pixel as a measurement.
    pub rejected: bool,
}

/// Nearest detector pixel at this sky position. `strict` uses the spectrum mask.
pub fn sample_at(input: &Input<'_>, ra: f64, dec: f64, strict: bool) -> Option<Hit> {
    let (ix, iy, wavelength, band) = geometric_sample(input.wcs, input.wavelength, ra, dec)?;
    let col = ix - input.origin_fits.0;
    let row = iy - input.origin_fits.1;
    if col < 0 || row < 0 {
        return None;
    }
    let height = input.image.len() / input.width;
    if col as usize >= input.width || row as usize >= height {
        return None;
    }
    let i = row as usize * input.width + col as usize;
    let rejected = if !input.mask {
        false
    } else if strict {
        bad_spectrum_pixel(input.flags[i], &input.flag_bits)
    } else if input.ignore_outlier {
        bad_delta_pixel(input.flags[i], &input.flag_bits)
    } else {
        bad_pixel(input.flags[i], &input.flag_bits)
    };
    let flux = input.image[i] - input.zodi[i];
    Some(Hit {
        flux: flux as f64,
        variance: input.variance[i] as f64,
        wavelength_um: wavelength,
        band,
        flags: input.flags[i],
        rejected,
    })
}

/// Geometry-only classification shared by exact browsing and pixel sampling.
pub fn geometric_sample(
    wcs: &Wcs,
    wave: &WavelengthMap,
    ra: f64,
    dec: f64,
) -> Option<(i32, i32, f64, Option<u16>)> {
    let (x, y) = wcs.sky_to_pixel(ra, dec);
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let (ix, iy) = (x.round() as i32, y.round() as i32);
    if ix < 1 || iy < 1 || ix > wcs.naxis[0] || iy > wcs.naxis[1] {
        return None;
    }
    let (wavelength, _) = wave.sample(f64::from(ix), f64::from(iy))?;
    Some((ix, iy, wavelength, band_of(wavelength)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Poly, Sip, PIXEL_SCALE_DEG};

    fn flat_wcs() -> Wcs {
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

    fn input<'a>(
        wcs: &'a Wcs,
        wave: &'a WavelengthMap,
        image: &'a [f32],
        variance: &'a [f32],
        flags: &'a [u32],
        zodi: &'a [f32],
    ) -> Input<'a> {
        Input {
            wcs,
            image,
            variance,
            flags,
            zodi,
            wavelength: wave,
            origin_fits: (1, 1),
            width: 4,
            flag_bits: bits(),
            mask: true,
            ignore_outlier: false,
        }
    }

    fn wave() -> WavelengthMap {
        WavelengthMap {
            x: vec![1.0, 4.0],
            y: vec![1.0, 4.0],
            wavelength: vec![0.75; 4],
            bandwidth: vec![0.02; 4],
            nx: 2,
            ny: 2,
        }
    }

    #[test]
    fn a_sample_subtracts_zodi_and_keeps_the_band() {
        let wcs = flat_wcs();
        let wave = wave();
        let image = vec![2.5; 16];
        let variance = vec![0.25; 16];
        let flags = vec![0_u32; 16];
        let zodi = vec![0.5; 16];
        let hit = sample_at(&input(&wcs, &wave, &image, &variance, &flags, &zodi), 10.0, 20.0, false)
            .expect("centre");
        assert!((hit.flux - 2.0).abs() < 1e-6);
        assert!((hit.variance - 0.25).abs() < 1e-6);
        assert!((hit.wavelength_um - 0.75).abs() < 1e-6);
        assert_eq!(hit.band, Some(0));
    }

    /// The shorter plane a band read returns has to land on the same sky.
    #[test]
    fn a_row_restricted_input_samples_the_same_pixels() {
        let wcs = flat_wcs();
        let wave = wave();
        let image: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let variance = vec![1.0; 16];
        let flags = vec![0; 16];
        let zodi = vec![0.0; 16];
        let whole = input(&wcs, &wave, &image, &variance, &flags, &zodi);
        let mut strip = input(&wcs, &wave, &image[8..12], &variance[8..12], &flags[8..12], &zodi[8..12]);
        strip.origin_fits = (1, 3);
        for row in 1..=4 {
            for col in 1..=4 {
                let (ra, dec) = wcs.pixel_to_sky(f64::from(col), f64::from(row));
                let got = sample_at(&strip, ra, dec, false).map(|hit| hit.flux);
                if row == 3 {
                    assert_eq!(got, sample_at(&whole, ra, dec, false).map(|hit| hit.flux));
                } else {
                    assert!(got.is_none(), "row {row} is not in the strip");
                }
            }
        }
    }

    #[test]
    fn delta_keeps_an_outlier_that_a_composite_drops() {
        let wcs = flat_wcs();
        let mut wave = wave();
        wave.wavelength = vec![1.35; 4];
        let image = vec![6.14; 16];
        let variance = vec![0.01; 16];
        let mut flags = vec![0_u32; 16];
        flags[5] = 1 << 19;
        let zodi = vec![0.0; 16];
        let mut kept = input(&wcs, &wave, &image, &variance, &flags, &zodi);
        kept.ignore_outlier = true;
        let hit = sample_at(&kept, 10.0, 20.0, false).unwrap();
        assert!(!hit.rejected, "delta keeps OUTLIER");
        assert!((hit.flux - 6.14).abs() < 1e-6);
        let dropped = input(&wcs, &wave, &image, &variance, &flags, &zodi);
        let hit = sample_at(&dropped, 10.0, 20.0, false).unwrap();
        assert!(hit.rejected, "composite still masks OUTLIER");
    }

    #[test]
    fn a_spectrum_mask_rejects_a_saturated_source_pixel() {
        let wcs = flat_wcs();
        let wave = wave();
        let image = vec![1.0; 16];
        let variance = vec![1.0; 16];
        let mut flags = vec![0_u32; 16];
        flags[5] = (1 << 21) | (1 << 1);
        let zodi = vec![0.0; 16];
        let hit = sample_at(&input(&wcs, &wave, &image, &variance, &flags, &zodi), 10.0, 20.0, true)
            .expect("centre");
        assert!(hit.rejected);
    }

    #[test]
    fn a_pixel_outside_the_detector_has_no_sample() {
        let wcs = flat_wcs();
        let wave = wave();
        let image = vec![1.0; 16];
        let variance = vec![1.0; 16];
        let flags = vec![0; 16];
        let zodi = vec![0.0; 16];
        assert!(sample_at(&input(&wcs, &wave, &image, &variance, &flags, &zodi), 80.0, 20.0, false).is_none());
    }
}
