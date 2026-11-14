/// SPHEREx plate scale. The render grid uses this so a pixel is one sample,
/// not an interpolation. CSS scales the PNG; the server does not invent resolution.
pub const PIXEL_SCALE_DEG: f64 = 6.15 / 3600.0;

/// A render PNG's grid stops at this many pixels. The render routes reject a
/// wider field instead of cropping it; the mosaic's [`TanGrid`] has no cap.
pub const MAX_RENDER_PIXELS: u32 = 4096;

/// The widest field a render PNG draws at the native scale before the
/// [`MAX_RENDER_PIXELS`] clamp would crop it.
pub const MAX_RENDER_FOV_DEG: f64 = MAX_RENDER_PIXELS as f64 * PIXEL_SCALE_DEG;

pub fn grid_size(fov_deg: f64) -> u32 {
    ((fov_deg / PIXEL_SCALE_DEG).round() as u32).clamp(1, MAX_RENDER_PIXELS)
}

/// The side of a render PNG's grid, or an error when [`grid_size`] would clamp
/// and the PNG would silently show only the centre of the requested field.
/// The mosaic keeps the true size on a [`TanGrid`] and does not call this.
pub fn render_grid_size(fov_deg: f64) -> crate::Result<u32> {
    let side = (fov_deg / PIXEL_SCALE_DEG).round();
    if !(0.0..=f64::from(MAX_RENDER_PIXELS)).contains(&side) {
        return Err(crate::Error::Message(format!(
            "render images stop at {MAX_RENDER_FOV_DEG:.3} degrees (4096 px at the native scale); use /api/v1/mosaic for a wider field"
        )));
    }
    Ok(side.max(1.0) as u32)
}

/// Inclusive 0-based FITS bounds of the common-grid pixels that can sample a
/// detector rectangle. `detector` is `[left, bottom, right, top]` in FITS pixel
/// coordinates. The rectangle grows by one detector pixel, then its four edges
/// are walked so SIP distortion and rotation stay inside the box. A
/// conservative grid margin covers nearest-pixel rounding. `None` when the
/// rectangle misses the grid.
pub(crate) fn footprint_bounds(wcs: &crate::Wcs, detector: [f64; 4], grid: &TanGrid) -> Option<[u32; 4]> {
    let [left, bottom, right, top] = [
        detector[0] - 1.0,
        detector[1] - 1.0,
        detector[2] + 1.0,
        detector[3] + 1.0,
    ];
    const STEPS: i32 = 128;
    let along = |low: f64, high: f64| {
        (0..=STEPS).map(move |i| low + (high - low) * f64::from(i) / f64::from(STEPS))
    };
    let edges = along(left, right)
        .flat_map(|x| [(x, bottom), (x, top)])
        .chain(along(bottom, top).flat_map(|y| [(left, y), (right, y)]));
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (x, y) in edges {
        let (ra, dec) = wcs.pixel_to_sky(x, y);
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
    let (x0, y0) = ((min_x - 3.0).floor(), (min_y - 3.0).floor());
    let (x1, y1) = ((max_x + 1.0).ceil(), (max_y + 1.0).ceil());
    if x1 < 0.0 || y1 < 0.0 || x0 > width - 1.0 || y0 > height - 1.0 {
        return None;
    }
    Some([
        x0.max(0.0) as u32,
        x1.min(width - 1.0) as u32,
        y0.max(0.0) as u32,
        y1.min(height - 1.0) as u32,
    ])
}

/// A north-up TAN grid of any size. [`grid_size`] stops at 4096 pixels, so a
/// wider field keeps its true scale here instead of being cropped.
#[derive(Clone, Debug)]
pub struct TanGrid {
    pub ra: f64,
    pub dec: f64,
    pub width: u32,
    pub height: u32,
    wcs: crate::Wcs,
}

impl TanGrid {
    pub fn new(ra: f64, dec: f64, fov_deg: f64) -> Self {
        Self::new_rect(ra, dec, fov_deg, fov_deg)
    }

    pub fn new_rect(ra: f64, dec: f64, fov_l_deg: f64, fov_h_deg: f64) -> Self {
        let width = ((fov_l_deg / PIXEL_SCALE_DEG).round() as u32).max(1);
        let height = ((fov_h_deg / PIXEL_SCALE_DEG).round() as u32).max(1);
        let wcs = crate::Wcs {
            crpix: [(f64::from(width) + 1.0) / 2.0, (f64::from(height) + 1.0) / 2.0],
            crval: [ra, dec],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: crate::Sip {
                a: crate::Poly::new(0),
                b: crate::Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [width as i32, height as i32],
        };
        Self { ra, dec, width, height, wcs }
    }

    /// Sky at a 1-based FITS pixel of this grid.
    pub fn sky(&self, x: f64, y: f64) -> (f64, f64) {
        if self.width == self.height {
            return output_sky(x, y, self.wcs.crpix[0], self.ra, self.dec, PIXEL_SCALE_DEG);
        }
        output_sky_rect(x, y, self.wcs.crpix, self.ra, self.dec, PIXEL_SCALE_DEG)
    }

    /// 1-based FITS pixel of this grid at a sky position.
    pub fn pixel(&self, ra: f64, dec: f64) -> (f64, f64) {
        self.wcs.sky_to_pixel(ra, dec)
    }

    /// Inclusive 0-based FITS bounds of this grid that a whole detector can reach.
    pub fn detector_bounds(&self, wcs: &crate::Wcs) -> Option<[u32; 4]> {
        let [nx, ny] = wcs.naxis;
        if nx < 1 || ny < 1 {
            return None;
        }
        footprint_bounds(wcs, [0.5, 0.5, f64::from(nx) + 0.5, f64::from(ny) + 0.5], self)
    }
}

/// North-up TAN. `x` and `y` are 1-based FITS pixels of the output grid.
/// RA increases to the left.
pub(crate) fn output_sky(x: f64, y: f64, crpix: f64, ra: f64, dec: f64, scale: f64) -> (f64, f64) {
    output_sky_rect(x, y, [crpix, crpix], ra, dec, scale)
}

pub(crate) fn output_sky_rect(x: f64, y: f64, crpix: [f64; 2], ra: f64, dec: f64, scale: f64) -> (f64, f64) {
    let xi = -(x - crpix[0]) * scale;
    let eta = (y - crpix[1]) * scale;
    let xi_r = xi.to_radians();
    let eta_r = eta.to_radians();
    let phi = xi_r.atan2(-eta_r);
    let theta = 1.0_f64.atan2(xi_r.hypot(eta_r));
    let ra0 = ra.to_radians();
    let dec0 = dec.to_radians();
    let (sin_t, cos_t) = (theta.sin(), theta.cos());
    let (sin_d0, cos_d0) = (dec0.sin(), dec0.cos());
    let sin_dec = sin_t * sin_d0 - cos_t * cos_d0 * phi.cos();
    let sky_dec = sin_dec.clamp(-1.0, 1.0).asin();
    let sky_ra = ra0 + (cos_t * phi.sin()).atan2(sin_t * cos_d0 + cos_t * sin_d0 * phi.cos());
    (sky_ra.to_degrees().rem_euclid(360.0), sky_dec.to_degrees())
}

/// PNG pixel, origin top-left, to ICRS. This is the click grid the server owns.
/// The browser sends the pixel; it does not compute sky coordinates.
pub fn png_pixel_to_sky(x: u32, y: u32, ra: f64, dec: f64, fov_deg: f64) -> Option<(f64, f64)> {
    png_pixel_to_sky_rect(x, y, ra, dec, fov_deg, fov_deg)
}

pub fn png_pixel_to_sky_rect(
    x: u32,
    y: u32,
    ra: f64,
    dec: f64,
    fov_l_deg: f64,
    fov_h_deg: f64,
) -> Option<(f64, f64)> {
    let width = grid_size(fov_l_deg);
    let height = grid_size(fov_h_deg);
    if x >= width || y >= height {
        return None;
    }
    let crpix = [(f64::from(width) + 1.0) / 2.0, (f64::from(height) + 1.0) / 2.0];
    let fits_x = f64::from(x) + 1.0;
    let fits_y = f64::from(height - 1 - y) + 1.0;
    Some(output_sky_rect(fits_x, fits_y, crpix, ra, dec, PIXEL_SCALE_DEG))
}

pub fn sky_to_png_pixel(
    sky_ra: f64, sky_dec: f64, ra: f64, dec: f64, fov_deg: f64,
) -> Option<(f64, f64)> {
    sky_to_png_pixel_rect(sky_ra, sky_dec, ra, dec, fov_deg, fov_deg)
}

pub fn sky_to_png_pixel_rect(
    sky_ra: f64,
    sky_dec: f64,
    ra: f64,
    dec: f64,
    fov_l_deg: f64,
    fov_h_deg: f64,
) -> Option<(f64, f64)> {
    let width = grid_size(fov_l_deg);
    let height = grid_size(fov_h_deg);
    let wcs = crate::Wcs {
        crpix: [(f64::from(width) + 1.0) / 2.0, (f64::from(height) + 1.0) / 2.0],
        crval: [ra, dec],
        cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
        sip: crate::Sip { a: crate::Poly::new(0), b: crate::Poly::new(0), ap: None, bp: None },
        naxis: [width as i32, height as i32],
    };
    let (fits_x, fits_y) = wcs.sky_to_pixel(sky_ra, sky_dec);
    let png_x = fits_x - 0.5;
    let png_y = f64::from(height) - fits_y + 0.5;
    if !(png_x.is_finite() && png_y.is_finite()) {
        return None;
    }
    if png_x < -0.5 || png_y < -0.5 || png_x > f64::from(width) || png_y > f64::from(height) {
        return None;
    }
    Some((png_x, png_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangular_grid_has_independent_axes_and_matches_the_click_grid() {
        let grid = TanGrid::new_rect(10.0, 20.0, 0.2, 0.1);
        assert_eq!((grid.width, grid.height), (117, 59));
        assert_eq!(grid.wcs.crpix, [59.0, 30.0]);
        assert_eq!(grid.wcs.naxis, [117, 59]);
        let (ra, dec) = grid.sky(59.0, 30.0);
        assert!((ra - 10.0).abs() < 1e-9 && (dec - 20.0).abs() < 1e-9);
        for (ra, dec) in [(10.0, 20.0), (359.99, 80.0)] {
            for (l, h) in [(0.2, 0.1), (0.1, 0.2)] {
                let grid = TanGrid::new_rect(ra, dec, l, h);
                for (x, y) in [(0, 0), (grid.width - 1, grid.height - 1), (grid.width / 2, grid.height / 2)] {
                    let sky = png_pixel_to_sky_rect(x, y, ra, dec, l, h).unwrap();
                    assert_eq!(sky, grid.sky(f64::from(x + 1), f64::from(grid.height - y)));
                    let pixel = sky_to_png_pixel_rect(sky.0, sky.1, ra, dec, l, h).unwrap();
                    assert!((pixel.0 - f64::from(x) - 0.5).abs() < 1e-6);
                    assert!((pixel.1 - f64::from(y) - 0.5).abs() < 1e-6);
                }
                assert!(png_pixel_to_sky_rect(grid.width, 0, ra, dec, l, h).is_none());
                assert!(png_pixel_to_sky_rect(0, grid.height, ra, dec, l, h).is_none());
            }
        }
    }

    #[test]
    fn square_wrapper_preserves_samples_and_rect_bounds_clamp_both_axes() {
        let square = TanGrid::new(180.0, 0.0, 0.2);
        let rect = TanGrid::new_rect(180.0, 0.0, 0.2, 0.2);
        for (x, y) in [(1.0, 1.0), (59.0, 59.0), (117.0, 117.0)] {
            assert_eq!(square.sky(x, y), rect.sky(x, y));
            assert_eq!(square.sky(x, y), output_sky(x, y, 59.0, 180.0, 0.0, PIXEL_SCALE_DEG));
        }
        let rect = TanGrid::new_rect(180.0, 0.0, 0.2, 0.1);
        assert_eq!(rect.detector_bounds(&detector_wcs(2040)), Some([0, 116, 0, 58]));
        let window = crate::PixelWindow::for_region(&detector_wcs(2040), &crate::SkyRegion::rect(180.0, 0.0, 0.2, 0.1)).unwrap();
        assert!(window.width > window.height);
    }

    #[test]
    fn sky_to_png_pixel_inverts_png_pixel_to_sky() {
        let (ra, dec, fov) = (58.2783, 26.1398, 0.1);
        for (x, y) in [(0, 0), (29, 29), (47, 28), (58, 58)] {
            let (sky_ra, sky_dec) = png_pixel_to_sky(x, y, ra, dec, fov).expect("inside");
            let (px, py) = sky_to_png_pixel(sky_ra, sky_dec, ra, dec, fov).expect("inside");
            // A pixel index x has its centre at x + 0.5 in image space.
            assert!(
                (px - (x as f64 + 0.5)).abs() < 1e-6 && (py - (y as f64 + 0.5)).abs() < 1e-6,
                "{x},{y} -> {px},{py}"
            );
        }
    }

    #[test]
    fn sky_to_png_pixel_rejects_a_point_off_the_grid() {
        assert!(sky_to_png_pixel(60.0, 26.1, 58.2783, 26.1398, 0.1).is_none());
    }

    #[test]
    fn center_pixel_is_the_view_center() {
        let n = grid_size(0.05);
        let (ra, dec) = png_pixel_to_sky(n / 2, n / 2, 270.146, 66.523, 0.05).unwrap();
        assert!((ra - 270.146).abs() < 1e-9, "{ra}");
        assert!((dec - 66.523).abs() < 1e-9, "{dec}");
    }

    #[test]
    fn a_tan_grid_matches_the_click_grid_and_keeps_its_scale_past_it() {
        let grid = TanGrid::new(56.75, 24.12, 5.0);
        assert_eq!(grid.width, grid_size(5.0));
        let (ra, dec) = png_pixel_to_sky(300, 41, 56.75, 24.12, 5.0).unwrap();
        let (sky_ra, sky_dec) = grid.sky(301.0, f64::from(grid.width) - 41.0);
        assert!((ra - sky_ra).abs() < 1e-12 && (dec - sky_dec).abs() < 1e-12);
        let (x, y) = grid.pixel(sky_ra, sky_dec);
        assert!(
            (x - 301.0).abs() < 1e-6 && (y - f64::from(grid.width) + 41.0).abs() < 1e-6,
            "{x},{y}"
        );
        assert_eq!(TanGrid::new(56.75, 24.12, 10.0).width, 5854);
    }

    #[test]
    fn a_tenth_of_a_degree_is_59_pixels() {
        assert_eq!(grid_size(0.1), 59);
    }

    fn detector_wcs(naxis: i32) -> crate::Wcs {
        crate::Wcs {
            crpix: [(f64::from(naxis) + 1.0) / 2.0; 2],
            crval: [180.0, 0.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: crate::Sip {
                a: crate::Poly::new(0),
                b: crate::Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [naxis, naxis],
        }
    }

    #[test]
    fn detector_bounds_keep_the_centre_and_stay_local_at_thirty_degrees() {
        let grid = TanGrid::new(180.0, 0.0, 30.0);
        let wcs = detector_wcs(2040);
        let [left, right, bottom, top] = grid.detector_bounds(&wcs).expect("on the grid");
        let (centre_x, centre_y) = grid.pixel(180.0, 0.0);
        let (centre_x, centre_y) = (centre_x as u32, centre_y as u32);
        assert!(
            left - 1 <= centre_x && centre_x <= right + 1,
            "{left}..{right} {centre_x}"
        );
        assert!(
            bottom - 1 <= centre_y && centre_y <= top + 1,
            "{bottom}..{top} {centre_y}"
        );
        let area = u64::from(right - left + 1) * u64::from(top - bottom + 1);
        let grid_area = u64::from(grid.width) * u64::from(grid.width);
        assert!(
            area < grid_area / 50,
            "a 2040-pixel detector must not bound most of the 30-degree grid: {area} of {grid_area}"
        );
        assert!(
            area < 5_000_000,
            "footprint is about the detector size, not the grid: {area}"
        );
    }

    #[test]
    fn a_detector_that_misses_the_grid_has_no_bounds() {
        let grid = TanGrid::new(10.0, 20.0, 0.1);
        assert_eq!(grid.detector_bounds(&detector_wcs(2040)), None);
    }

    #[test]
    fn a_render_rejects_the_field_the_grid_would_crop() {
        assert_eq!(render_grid_size(0.1).unwrap(), 59);
        assert!(render_grid_size(6.99).is_ok());
        let error = render_grid_size(7.0).expect_err("7 degrees cannot be drawn at 4096 px");
        assert!(error.to_string().contains("6.997"), "{error}");
        assert_eq!(TanGrid::new(56.75, 24.12, 10.0).width, 5854, "the mosaic grid keeps the true size");
    }
}
