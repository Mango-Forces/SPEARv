/// Wavelength lookup from a SPHEREx `WCS-WAVE` table.
///
/// `x` and `y` are 1-based FITS pixel coordinates of the control points, matching
/// the table and `CRPIX1W = 1`. `wavelength` and `bandwidth` are microns, stored
/// row-major as `iy * nx + ix`. The FITS `TDIM` is `(2, nx, ny)` in Fortran order,
/// so the first index is the component and the last is Y.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WavelengthMap {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    pub wavelength: Vec<f32>,
    pub bandwidth: Vec<f32>,
    pub nx: usize,
    pub ny: usize,
}

impl WavelengthMap {
    /// Sample at a 1-based FITS pixel center. Outside the grid, the edge cell is used.
    pub fn sample(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.nx < 2 || self.ny < 2 {
            return None;
        }
        let ix = cell_index(&self.x, x);
        let iy = cell_index(&self.y, y);
        let tx = (x - self.x[ix]) / (self.x[ix + 1] - self.x[ix]);
        let ty = (y - self.y[iy]) / (self.y[iy + 1] - self.y[iy]);
        let at = |ix: usize, iy: usize, plane: &[f32]| plane[iy * self.nx + ix] as f64;
        let blend = |plane: &[f32]| {
            let v00 = at(ix, iy, plane);
            let v10 = at(ix + 1, iy, plane);
            let v01 = at(ix, iy + 1, plane);
            let v11 = at(ix + 1, iy + 1, plane);
            (1.0 - ty) * ((1.0 - tx) * v00 + tx * v10) + ty * ((1.0 - tx) * v01 + tx * v11)
        };
        Some((blend(&self.wavelength), blend(&self.bandwidth)))
    }

    /// 1-based inclusive detector rows that can hold a wavelength in `wl_min..=wl_max`.
    ///
    /// `sample` is bilinear inside a cell, so a cell never leaves the range of its
    /// corners. Each row of nodes is scanned across every X, because the band
    /// curves along X, and the detector edges join the nodes, because `sample`
    /// extrapolates past the grid. The result then grows by one grid cell each
    /// way. `None` means no pixel of this detector is in the interval.
    pub fn rows_for_wavelength(&self, wl_min: f64, wl_max: f64, size: [i32; 2]) -> Option<(i32, i32)> {
        if self.nx < 2 || self.ny < 2 {
            return None;
        }
        let stations = |axis: &[f64], size: i32| {
            let mut stops: Vec<f64> = axis.iter().copied().filter(|v| *v >= 1.0 && *v <= size as f64).collect();
            stops.extend([1.0, size as f64]);
            stops.sort_by(f64::total_cmp);
            stops.dedup();
            stops
        };
        let xs = stations(&self.x, size[0]);
        let ys = stations(&self.y, size[1]);
        let spans: Vec<(f64, f64)> = ys
            .iter()
            .map(|y| {
                xs.iter()
                    .filter_map(|x| self.sample(*x, *y))
                    .fold((f64::MAX, f64::MIN), |(lo, hi), (wave, _)| (lo.min(wave), hi.max(wave)))
            })
            .collect();
        let hit = |cell: usize| {
            let (a, b) = (spans[cell], spans[cell + 1]);
            a.0.min(b.0) <= wl_max && a.1.max(b.1) >= wl_min
        };
        let cells = 0..ys.len().saturating_sub(1);
        let first = cells.clone().find(|cell| hit(*cell))?;
        let last = cells.rev().find(|cell| hit(*cell))?;
        let low = ys[first.saturating_sub(1)];
        let high = ys[(last + 2).min(ys.len() - 1)];
        Some((low.floor() as i32, high.ceil() as i32))
    }
}

fn cell_index(axis: &[f64], value: f64) -> usize {
    let mut i = axis.partition_point(|v| *v <= value);
    if i == 0 {
        return 0;
    }
    i -= 1;
    i.min(axis.len() - 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wavelength falls with Y and bows along X, like a SPHEREx detector.
    fn curved() -> WavelengthMap {
        let x: Vec<f64> = (0..9).map(|i| 1.0 + i as f64 * 254.875).collect();
        let y: Vec<f64> = (0..14).map(|i| 1.0 + i as f64 * 2039.0 / 13.0).collect();
        let mut wavelength = Vec::new();
        for yy in &y {
            for xx in &x {
                let bow = ((xx - 1020.5) / 1020.0).powi(2) * 0.02;
                wavelength.push((1.1 - 0.4 * (yy - 1.0) / 2039.0 + bow) as f32);
            }
        }
        WavelengthMap {
            bandwidth: vec![0.01; wavelength.len()],
            nx: x.len(),
            ny: y.len(),
            x,
            y,
            wavelength,
        }
    }

    fn assert_holds_every_pixel(map: &WavelengthMap, low: f64, high: f64) -> (i32, i32) {
        let rows = map.rows_for_wavelength(low, high, [2040, 2040]).expect("in range");
        for y in 1..=2040 {
            for x in (1..=2040).step_by(17) {
                let (wave, _) = map.sample(x as f64, y as f64).unwrap();
                if wave >= low && wave <= high {
                    assert!(y >= rows.0 && y <= rows.1, "pixel {x},{y} at {wave} is outside {rows:?}");
                }
            }
        }
        rows
    }

    #[test]
    fn rows_hold_every_in_band_pixel_when_the_band_curves() {
        let map = curved();
        let rows = assert_holds_every_pixel(&map, 0.90, 0.92);
        // The bow moves this band by about 100 rows between the middle and the edge.
        assert!(rows.1 - rows.0 < 800, "still a strip: {rows:?}");
        assert!(rows.0 > 1 && rows.1 < 2040, "{rows:?}");
        assert_holds_every_pixel(&map, 0.7001, 0.71);
        assert_holds_every_pixel(&map, 1.10, 1.13);
    }

    #[test]
    fn rows_have_a_margin_of_one_cell_past_the_band() {
        let map = curved();
        let cell = 2039.0 / 13.0;
        let rows = map.rows_for_wavelength(0.9, 0.9, [2040, 2040]).unwrap();
        let mut in_band = (i32::MAX, i32::MIN);
        for y in 1..=2040 {
            let (lo, hi) = (1..=2040).fold((f64::MAX, f64::MIN), |(lo, hi), x| {
                let (wave, _) = map.sample(x as f64, y as f64).unwrap();
                (lo.min(wave), hi.max(wave))
            });
            if lo <= 0.9 && hi >= 0.9 {
                in_band = (in_band.0.min(y), in_band.1.max(y));
            }
        }
        assert!(f64::from(in_band.0 - rows.0) >= cell - 1.0, "{in_band:?} {rows:?}");
        assert!(f64::from(rows.1 - in_band.1) >= cell - 1.0, "{in_band:?} {rows:?}");
    }

    #[test]
    fn a_wavelength_off_the_detector_has_no_rows() {
        let map = curved();
        assert_eq!(map.rows_for_wavelength(2.0, 2.1, [2040, 2040]), None);
        assert_eq!(map.rows_for_wavelength(0.1, 0.2, [2040, 2040]), None);
    }

    #[test]
    fn rows_reach_the_detector_edge_when_the_grid_stops_short() {
        let map = WavelengthMap {
            x: vec![100.0, 1900.0],
            y: vec![100.0, 1900.0],
            wavelength: vec![1.0, 1.0, 0.8, 0.8],
            bandwidth: vec![0.01; 4],
            nx: 2,
            ny: 2,
        };
        // `sample` extrapolates to about 1.01 at row 1, past every node.
        let rows = map.rows_for_wavelength(1.005, 1.02, [2040, 2040]).unwrap();
        assert_eq!(rows.0, 1);
    }
}
