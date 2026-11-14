/// TAN projection with SIP distortion.
///
/// Pixel arguments are 1-based FITS coordinates: the center of the first pixel is
/// (1, 1). That is the same frame as `CRPIX` and as the `WCS-WAVE` index columns.
/// Astropy's `pixel_to_world` / `world_to_pixel` are 0-based, so they differ by 1.
///
/// `LONPOLE` must be 180 (the TAN default away from the pole). The celestial
/// transform below was checked against `astropy.wcs` on a SPHEREx `RA---TAN-SIP`
/// header; it is not a claim that a textbook transcription of Paper II is enough
/// on its own.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Wcs {
    pub crpix: [f64; 2],
    pub crval: [f64; 2],
    pub cd: [[f64; 2]; 2],
    pub sip: Sip,
    pub naxis: [i32; 2],
}

/// SIP polynomials. `a`/`b` distort detector pixels toward the sky.
/// `ap`/`bp`, when present, are the sky-to-detector polynomials and the start
/// of the inverse. They are not an exact inverse: on the test exposure they
/// leave about 0.02 px, so [`Wcs::sky_to_pixel`] refines them with a few Newton
/// steps on `a`/`b`. If `ap`/`bp` are absent, those steps start from the linear
/// solution.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sip {
    pub a: Poly,
    pub b: Poly,
    pub ap: Option<Poly>,
    pub bp: Option<Poly>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Poly {
    pub order: usize,
    coeff: Vec<f64>,
}

impl Poly {
    pub fn new(order: usize) -> Self {
        Self {
            order,
            coeff: vec![0.0; (order + 1) * (order + 1)],
        }
    }

    /// Row-major `(order + 1)²` coefficients, as `set` stores them.
    pub fn coefficients(&self) -> &[f64] {
        &self.coeff
    }

    /// The inverse of `coefficients`. `None` when the length does not match the order.
    pub fn from_coefficients(order: usize, coeff: Vec<f64>) -> Option<Self> {
        (coeff.len() == (order + 1) * (order + 1)).then_some(Self { order, coeff })
    }

    pub fn set(&mut self, i: usize, j: usize, value: f64) {
        self.coeff[i * (self.order + 1) + j] = value;
    }

    pub fn eval(&self, u: f64, v: f64) -> f64 {
        let mut sum = 0.0;
        let mut up = 1.0;
        for i in 0..=self.order {
            let mut vp = 1.0;
            for j in 0..=(self.order - i) {
                sum += self.coeff[i * (self.order + 1) + j] * up * vp;
                vp *= v;
            }
            up *= u;
        }
        sum
    }
}

impl Wcs {
    /// 1-based FITS pixel to ICRS degrees.
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64) {
        let (u, v) = (x - self.crpix[0], y - self.crpix[1]);
        let us = u + self.sip.a.eval(u, v);
        let vs = v + self.sip.b.eval(u, v);
        let xi = self.cd[0][0] * us + self.cd[0][1] * vs;
        let eta = self.cd[1][0] * us + self.cd[1][1] * vs;
        let phi = xi.atan2(-eta);
        let theta = 1.0_f64.atan2(xi.to_radians().hypot(eta.to_radians()));
        let (ra0, dec0) = (self.crval[0].to_radians(), self.crval[1].to_radians());
        let (sin_t, cos_t) = (theta.sin(), theta.cos());
        let (sin_d0, cos_d0) = (dec0.sin(), dec0.cos());
        let sin_dec = sin_t * sin_d0 - cos_t * cos_d0 * phi.cos();
        let dec = sin_dec.clamp(-1.0, 1.0).asin();
        let ra = ra0
            + (cos_t * phi.sin()).atan2(sin_t * cos_d0 + cos_t * sin_d0 * phi.cos());
        (ra.to_degrees().rem_euclid(360.0), dec.to_degrees())
    }

    /// ICRS degrees to 1-based FITS pixel.
    pub fn sky_to_pixel(&self, ra: f64, dec: f64) -> (f64, f64) {
        let (us, vs) = self.undistorted_offset(ra, dec);
        if !us.is_finite() || !vs.is_finite() {
            return (f64::NAN, f64::NAN);
        }
        let (u0, v0) = match (&self.sip.ap, &self.sip.bp) {
            (Some(ap), Some(bp)) => (us + ap.eval(us, vs), vs + bp.eval(us, vs)),
            _ => (us, vs),
        };
        let (u, v) = refine_inverse(&self.sip, us, vs, u0, v0);
        (u + self.crpix[0], v + self.crpix[1])
    }

    fn undistorted_offset(&self, ra: f64, dec: f64) -> (f64, f64) {
        let ra = ra.to_radians();
        let dec = dec.to_radians();
        let ra0 = self.crval[0].to_radians();
        let dec0 = self.crval[1].to_radians();
        let dra = (ra - ra0 + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU)
            - std::f64::consts::PI;
        let (sin_d, cos_d) = (dec.sin(), dec.cos());
        let (sin_d0, cos_d0) = (dec0.sin(), dec0.cos());
        let sin_theta = sin_d * sin_d0 + cos_d * cos_d0 * dra.cos();
        // A TAN inverse exists only in front of the tangent plane. Here
        // `sin_theta` is cos(separation from `crval`): at 90° the point is at
        // infinity, and past 90° `cot(theta)` turns negative and folds the far
        // hemisphere back onto the detector. The margin absorbs the rounding
        // of a point that should sit exactly on the plane.
        if sin_theta <= 1e-12 {
            return (f64::NAN, f64::NAN);
        }
        let sphi = cos_d * dra.sin();
        let cphi = -sin_d * cos_d0 + cos_d * sin_d0 * dra.cos();
        let phi = sphi.atan2(cphi);
        let theta = sin_theta.atan2(sphi.hypot(cphi));
        let cot = theta.cos() / theta.sin();
        let xi = cot.to_degrees() * phi.sin();
        let eta = cot.to_degrees() * -phi.cos();
        let det = self.cd[0][0] * self.cd[1][1] - self.cd[0][1] * self.cd[1][0];
        let us = (self.cd[1][1] * xi - self.cd[0][1] * eta) / det;
        let vs = (-self.cd[1][0] * xi + self.cd[0][0] * eta) / det;
        (us, vs)
    }
}

fn refine_inverse(sip: &Sip, us: f64, vs: f64, mut u: f64, mut v: f64) -> (f64, f64) {
    let eps = 1e-6;
    for _ in 0..6 {
        let (fu, fv) = forward(sip, u, v);
        let (du, dv) = (us - fu, vs - fv);
        if du.hypot(dv) < 1e-12 {
            break;
        }
        let (fu1, fv1) = forward(sip, u + eps, v);
        let (fu2, fv2) = forward(sip, u, v + eps);
        let j11 = (fu1 - fu) / eps;
        let j21 = (fv1 - fv) / eps;
        let j12 = (fu2 - fu) / eps;
        let j22 = (fv2 - fv) / eps;
        let det = j11 * j22 - j12 * j21;
        if det.abs() < 1e-18 {
            break;
        }
        u += (j22 * du - j12 * dv) / det;
        v += (-j21 * du + j11 * dv) / det;
    }
    (u, v)
}

fn forward(sip: &Sip, u: f64, v: f64) -> (f64, f64) {
    (u + sip.a.eval(u, v), v + sip.b.eval(u, v))
}

/// Axis-aligned detector window, in 0-based pixel indices, covering a sky square.
///
/// The square is `fov_deg` on a side in declination, and the same angle in right
/// ascension after dividing by cos(dec). Footprints that cross RA 0/360 are not
/// split; that waits for the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PixelWindow {
    pub x0: i32,
    pub y0: i32,
    pub width: i32,
    pub height: i32,
    pub clipped: bool,
}

impl PixelWindow {
    /// This window limited to 1-based inclusive detector rows. `None` when they miss.
    /// `y0` moves with the cut, so `origin_fits` stays true for the shorter plane.
    pub fn restrict_rows(&self, rows: (i32, i32)) -> Option<Self> {
        let first = (self.y0 + 1).max(rows.0);
        let last = (self.y0 + self.height).min(rows.1);
        if last < first {
            return None;
        }
        Some(Self {
            y0: first - 1,
            height: last - first + 1,
            ..self.clone()
        })
    }

    pub fn for_region(wcs: &Wcs, region: &crate::SkyRegion) -> crate::Result<Self> {
        let half_l = region.fov_l_deg / 2.0;
        let half_h = region.fov_h_deg / 2.0;
        let cos_dec = region.dec.to_radians().cos().abs().max(1e-3);
        let dra = half_l / cos_dec;
        let corners = [
            (region.ra - dra, region.dec - half_h),
            (region.ra - dra, region.dec + half_h),
            (region.ra + dra, region.dec - half_h),
            (region.ra + dra, region.dec + half_h),
            (region.ra, region.dec),
        ];
        let mut min_x = f64::MAX;
        let mut min_y = f64::MAX;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;
        for (ra, dec) in corners {
            let (x, y) = wcs.sky_to_pixel(ra, dec);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        // 1-based inclusive, plus a one-pixel margin so a corner landing in a pixel is inside.
        let raw_x0 = min_x.floor() as i32 - 1;
        let raw_y0 = min_y.floor() as i32 - 1;
        let raw_x1 = max_x.ceil() as i32 + 1;
        let raw_y1 = max_y.ceil() as i32 + 1;
        let naxis1 = wcs.naxis[0];
        let naxis2 = wcs.naxis[1];
        let x0 = raw_x0.clamp(1, naxis1);
        let y0 = raw_y0.clamp(1, naxis2);
        let x1 = raw_x1.clamp(1, naxis1);
        let y1 = raw_y1.clamp(1, naxis2);
        if x1 < x0 || y1 < y0 || raw_x1 < 1 || raw_y1 < 1 || raw_x0 > naxis1 || raw_y0 > naxis2 {
            return Err(crate::Error::OutsideExposure);
        }
        Ok(Self {
            x0: x0 - 1,
            y0: y0 - 1,
            width: x1 - x0 + 1,
            height: y1 - y0 + 1,
            clipped: raw_x0 < 1 || raw_y0 < 1 || raw_x1 > naxis1 || raw_y1 > naxis2,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tan(ra: f64, dec: f64) -> Wcs {
        let scale = 6.15 / 3600.0;
        Wcs {
            crpix: [1020.5, 1020.5],
            crval: [ra, dec],
            cd: [[-scale, 0.0], [0.0, scale]],
            sip: Sip { a: Poly::new(0), b: Poly::new(0), ap: None, bp: None },
            naxis: [2040, 2040],
        }
    }

    #[test]
    fn a_tan_inverse_round_trips_in_front_of_the_tangent_plane() {
        let wcs = tan(10.0, 20.0);
        for (ra, dec) in [(10.0, 20.0), (10.0, 30.0), (20.0, 20.0), (4.0, 13.0), (9.9, 20.1)] {
            let (x, y) = wcs.sky_to_pixel(ra, dec);
            assert!(x.is_finite() && y.is_finite(), "({ra}, {dec}) -> ({x}, {y})");
            let (back_ra, back_dec) = wcs.pixel_to_sky(x, y);
            let dra = (back_ra - ra + 180.0).rem_euclid(360.0) - 180.0;
            assert!(
                dra.abs() < 1e-9 && (back_dec - dec).abs() < 1e-9,
                "({ra}, {dec}) -> ({x}, {y}) -> ({back_ra}, {back_dec})"
            );
        }
    }

    #[test]
    fn a_tan_inverse_stops_at_the_tangent_plane() {
        let wcs = tan(10.0, 20.0);
        // (100, 0) is exactly 90° from (10, 20); (190, -20) is its antipode.
        for (ra, dec) in [(100.0, 0.0), (190.0, 20.0), (190.0, -20.0)] {
            let (x, y) = wcs.sky_to_pixel(ra, dec);
            assert!(!x.is_finite() && !y.is_finite(), "({ra}, {dec}) -> ({x}, {y})");
        }
        // 89.9° away is still in front of the plane, where the inverse exists
        // (as very large finite pixels).
        let (x, y) = wcs.sky_to_pixel(99.9, 0.0);
        assert!(x.is_finite() && y.is_finite(), "({x}, {y})");
        // ΔRA = 90° at the same declination is 83.3° on the sky, not 90°.
        let (x, y) = wcs.sky_to_pixel(100.0, 20.0);
        assert!(x.is_finite() && y.is_finite(), "({x}, {y})");
    }

    #[test]
    fn restricting_rows_moves_the_origin_with_them() {
        let window = PixelWindow { x0: 10, y0: 99, width: 50, height: 200, clipped: false };
        // Rows 100..=299, 1-based.
        let cut = window.restrict_rows((150, 1000)).unwrap();
        assert_eq!((cut.x0, cut.y0, cut.width, cut.height), (10, 149, 50, 150));
        let cut = window.restrict_rows((1, 120)).unwrap();
        assert_eq!((cut.y0, cut.height), (99, 21));
        assert_eq!(window.restrict_rows((1, 2040)), Some(window.clone()));
        assert_eq!(window.restrict_rows((300, 400)), None);
        assert_eq!(window.restrict_rows((1, 99)), None);
    }

    fn coeff(order: usize, terms: &[(usize, usize, f64)]) -> Poly {
        let mut poly = Poly::new(order);
        for (i, j, value) in terms {
            poly.set(*i, *j, *value);
        }
        poly
    }

    fn test_wcs() -> Wcs {
        let a = coeff(
            3,
            &[
                (0, 0, -0.0721241937723),
                (0, 1, -0.000379038266172),
                (0, 2, -2.03669108861e-8),
                (0, 3, -1.04955206743e-11),
                (1, 0, -0.000639339130065),
                (1, 1, 3.95433782713e-6),
                (1, 2, 6.14228863495e-10),
                (2, 0, -3.45098385639e-8),
                (2, 1, 1.13129991377e-11),
                (3, 0, 5.75282683032e-10),
            ],
        );
        let b = coeff(
            3,
            &[
                (0, 0, -1.14756503744),
                (0, 1, 3.18766579643e-5),
                (0, 2, 1.94917392656e-6),
                (0, 3, 4.80950719829e-10),
                (1, 0, 0.000567532060252),
                (1, 1, 3.31305803929e-8),
                (1, 2, 1.14675677692e-11),
                (2, 0, 6.54578617659e-6),
                (2, 1, 6.16671425969e-10),
                (3, 0, 1.53658702202e-12),
            ],
        );
        let ap = coeff(
            3,
            &[
                (0, 0, 0.0726288653007),
                (0, 1, 0.000379035646435),
                (0, 2, 1.79890692796e-8),
                (0, 3, 9.91334570914e-12),
                (1, 0, 0.000634038660419),
                (1, 1, -3.94224721895e-6),
                (1, 2, -5.89333290071e-10),
                (2, 0, 3.41626234502e-8),
                (2, 1, -1.20255149328e-11),
                (3, 0, -5.48850836661e-10),
            ],
        );
        let bp = coeff(
            3,
            &[
                (0, 0, 1.1425284193),
                (0, 1, -3.74426738012e-5),
                (0, 2, -1.93712592676e-6),
                (0, 3, -4.71642484185e-10),
                (1, 0, -0.000568852499738),
                (1, 1, -3.35060811646e-8),
                (1, 2, -1.06587792721e-11),
                (2, 0, -6.53495984909e-6),
                (2, 1, -5.37999541552e-10),
                (3, 0, -1.12641026429e-12),
            ],
        );
        Wcs {
            crpix: [1020.5, 1020.5],
            crval: [56.8585235304, 24.0102167285],
            cd: [
                [-0.00166839906039, -0.000350577382662],
                [-0.000349663160438, 0.00167089670603],
            ],
            sip: Sip {
                a,
                b,
                ap: Some(ap),
                bp: Some(bp),
            },
            naxis: [2040, 2040],
        }
    }

    #[test]
    fn pixel_to_sky_matches_astropy_corners() {
        let wcs = test_wcs();
        // 0-based astropy pixels, plus the sky astropy returned.
        let cases: [(f64, f64, f64, f64); 5] = [
            (0.0, 0.0, 59.07848202, 22.65675822),
            (2039.0, 0.0, 55.41426103, 21.95771710),
            (0.0, 2039.0, 58.35958098, 26.07712116),
            (2039.0, 2039.0, 54.57086851, 25.35368743),
            (1019.5, 1019.5, 56.85909567, 24.00832448),
        ];
        for (x0, y0, ra, dec) in cases {
            let (got_ra, got_dec) = wcs.pixel_to_sky(x0 + 1.0, y0 + 1.0);
            let dra = (got_ra - ra).abs().min(360.0 - (got_ra - ra).abs());
            let cos_dec = dec.to_radians().cos();
            let arcsec = 3600.0 * (dra * cos_dec).hypot(got_dec - dec);
            assert!(
                arcsec < 1e-4,
                "pixel {x0},{y0}: got {got_ra},{got_dec} expected {ra},{dec} ({arcsec} arcsec)"
            );
        }
    }

    #[test]
    fn sky_to_pixel_matches_astropy_target() {
        let wcs = test_wcs();
        let (x, y) = wcs.sky_to_pixel(56.75, 24.12);
        // astropy world_to_pixel is 0-based: 1063.2534935304698, 1095.4542855461389
        let err = (x - 1.0 - 1063.2534935304698).hypot(y - 1.0 - 1095.4542855461389);
        assert!(err < 1e-6, "sky_to_pixel error {err} px at ({x}, {y})");
    }

    #[test]
    fn outside_region_is_an_error() {
        let wcs = test_wcs();
        let err = PixelWindow::for_region(
            &wcs,
            &crate::SkyRegion::square(0.0, 0.0, 0.1),
        );
        assert!(matches!(err, Err(crate::Error::OutsideExposure)));
    }
}
