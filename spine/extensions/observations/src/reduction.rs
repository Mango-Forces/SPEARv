//! User-approved numerical rules. Operates on calibrated measurements, never PNGs.
//!
//! Depthwise: inverse-variance mean per band. Bandwise: equal-weight mean over the
//! sector's fixed band set. A pixel missing any selected band is no-data. Mask:
//! strict Spectrum policy. No clipping, normalization or cross-release correction.
use crate::types::TILE_SIZE;
use serde::Serialize;
use spearv_core::{bad_spectrum_pixel, FlagBits};
use std::collections::BTreeMap;

pub struct Measurement {
    /// Internal band index, 0–101.
    pub band: u16,
    /// Native IMAGE − ZODI, before any stretch or per-band normalization.
    pub value: f32,
    pub variance: f32,
    pub flags: u32,
    pub flag_bits: FlagBits,
}

#[derive(Debug, Serialize)]
pub struct DepthPixel {
    pub band: u16,
    pub value: Option<f64>,
    pub variance: Option<f64>,
    pub samples: u64,
}

#[derive(Debug, Serialize)]
pub struct SectorPixel {
    pub value: Option<f64>,
    pub variance: Option<f64>,
    pub bands: Vec<DepthPixel>,
}

/// A 256×256 data tile in grid PNG order. `NaN` is no-data. The variance is
/// stored next to the value so the production sequence keeps both.
#[derive(Clone, Debug)]
pub struct DataTile {
    pub value: Vec<f32>,
    pub variance: Vec<f32>,
}

impl DataTile {
    fn empty() -> Self {
        let len = (TILE_SIZE * TILE_SIZE) as usize;
        Self {
            value: vec![f32::NAN; len],
            variance: vec![f32::NAN; len],
        }
    }

    pub fn index(x: u32, y: u32) -> usize {
        ((y % TILE_SIZE) * TILE_SIZE + x % TILE_SIZE) as usize
    }
}

/// Splits a row-major PNG-order pixel index on a `width`×`height` grid into
/// `(x, y)`. The arithmetic is 64-bit, so a grid with more than 2^32 pixels does
/// not wrap; `None` means the index is outside the grid.
pub fn grid_coordinates_rect(pixel: usize, width: u32, height: u32) -> Option<(u32, u32)> {
    let pixel = u64::try_from(pixel).ok()?;
    let width = u64::from(width);
    if pixel >= width * u64::from(height) {
        return None;
    }
    Some((
        u32::try_from(pixel % width).ok()?,
        u32::try_from(pixel / width).ok()?,
    ))
}

/// Square-grid convenience wrapper used by the single-pixel tests.
pub fn grid_coordinates(pixel: usize, width: u32) -> Option<(u32, u32)> {
    grid_coordinates_rect(pixel, width, width)
}

/// Row-major PNG-order pixel index of `(x, y)` on a `width`×`height` grid, in
/// 64-bit arithmetic. Fails when the index does not fit this platform's `usize`
/// or the position is outside the grid.
pub fn grid_pixel_rect(x: u32, y: u32, width: u32, height: u32) -> Result<usize, String> {
    if x >= width || y >= height {
        return Err(format!(
            "Pixel ({x}, {y}) is outside the {width}×{height}-pixel grid"
        ));
    }
    let index = u64::from(y) * u64::from(width) + u64::from(x);
    usize::try_from(index).map_err(|_| format!("Pixel index {index} does not fit this platform"))
}

/// Square-grid convenience wrapper used by the single-pixel tests.
pub fn grid_pixel(x: u32, y: u32, width: u32) -> Result<usize, String> {
    grid_pixel_rect(x, y, width, width)
}

/// Sparse frames: only tiles touched by member support are allocated. A missing
/// tile is no-data and is encoded on demand, never materialized as a full frame.
pub type DataTiles = BTreeMap<(u32, u32), DataTile>;

/// The one acceptance rule shared by every path: finite value, finite positive
/// variance, and not rejected by the strict Spectrum mask.
fn accepted(sample: &Measurement) -> Option<(f64, f64)> {
    (sample.value.is_finite()
        && sample.variance.is_finite()
        && sample.variance > 0.0
        && !bad_spectrum_pixel(sample.flags, &sample.flag_bits))
    .then(|| (f64::from(sample.value), 1.0 / f64::from(sample.variance)))
}

fn validated_bands(mut bands: Vec<u16>) -> Result<Vec<u16>, String> {
    if bands.is_empty() || bands.iter().any(|band| *band >= 102) {
        return Err("Sector needs a nonempty band set in internal range 0–101".into());
    }
    bands.sort_unstable();
    bands.dedup();
    Ok(bands)
}

type TileSums = Vec<(f64, f64, u64)>;
pub(crate) type BandTiles = BTreeMap<(u32, u32), TileSums>;

/// Inverse-variance accumulation of one band over the support it touches.
/// Memory is one 256×256 tile per touched region, never a full-grid plane and
/// never a band cube.
pub struct BandAccumulator {
    band: u16,
    width: u32,
    height: u32,
    tiles: BandTiles,
}

impl BandAccumulator {
    /// Square-grid convenience constructor for the single-pixel tests.
    pub fn new(band: u16, width: u32) -> Self {
        Self::new_rect(band, width, width)
    }

    pub fn new_rect(band: u16, width: u32, height: u32) -> Self {
        Self {
            band,
            width,
            height,
            tiles: BTreeMap::new(),
        }
    }

    pub fn band(&self) -> u16 {
        self.band
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn tiles(&self) -> &BandTiles {
        &self.tiles
    }

    /// Measurements of another band or out-of-grid pixels are ignored.
    pub fn add(&mut self, pixel: usize, sample: &Measurement) {
        if sample.band != self.band {
            return;
        }
        let Some((x, y)) = grid_coordinates_rect(pixel, self.width, self.height) else {
            return;
        };
        let Some((value, weight)) = accepted(sample) else {
            return;
        };
        let tile = self
            .tiles
            .entry((x / TILE_SIZE, y / TILE_SIZE))
            .or_insert_with(|| vec![(0.0, 0.0, 0); (TILE_SIZE * TILE_SIZE) as usize]);
        let sum = &mut tile[DataTile::index(x, y)];
        sum.0 += weight * value;
        sum.1 += weight;
        sum.2 += 1;
    }

    pub fn depth(&self, pixel: usize) -> DepthPixel {
        let empty = DepthPixel {
            band: self.band,
            value: None,
            variance: None,
            samples: 0,
        };
        let Some((x, y)) = grid_coordinates_rect(pixel, self.width, self.height) else {
            return empty;
        };
        let Some(tile) = self.tiles.get(&(x / TILE_SIZE, y / TILE_SIZE)) else {
            return empty;
        };
        let (sum_f, sum_w, samples) = tile[DataTile::index(x, y)];
        DepthPixel {
            band: self.band,
            value: (samples > 0).then(|| sum_f / sum_w),
            variance: (samples > 0).then(|| 1.0 / sum_w),
            samples,
        }
    }
}

struct BuilderTile {
    sum_value: Vec<f64>,
    sum_variance: Vec<f64>,
    complete: Vec<u16>,
}

/// Bandwise reduction over a common grid, fed one finished band at a time.
/// Only tiles touched by an added band are allocated.
pub struct SectorBuilder {
    bands: Vec<u16>,
    added: Vec<u16>,
    width: u32,
    height: u32,
    tiles: BTreeMap<(u32, u32), BuilderTile>,
}

pub struct SectorImage {
    /// Sparse tiles in grid PNG order. A pixel missing any selected band is NaN.
    pub tiles: DataTiles,
    pub complete_pixels: u64,
}

impl SectorImage {
    /// `[x, y]` tile coordinates that hold data, ordered by row then column.
    pub fn data_tiles(&self) -> Vec<[u32; 2]> {
        let mut tiles: Vec<_> = self.tiles.keys().map(|(x, y)| [*x, *y]).collect();
        tiles.sort_by_key(|[x, y]| (*y, *x));
        tiles
    }

    /// Fraction of the target aperture with a complete pixel. The aperture is
    /// seven native pixels around the field centre, so a masked target core
    /// does not by itself make the whole frame look uncovered.
    pub fn target_coverage(&self, width: u32, height: u32) -> f64 {
        if width == 0 || height == 0 {
            return 0.0;
        }
        let centre_x = i64::from(width / 2);
        let centre_y = i64::from(height / 2);
        let radius = 7_i64;
        let mut covered = 0_u64;
        let mut total = 0_u64;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let x = centre_x + dx;
                let y = centre_y + dy;
                if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
                    continue;
                }
                total += 1;
                let (x, y) = (x as u32, y as u32);
                let Some(tile) = self.tiles.get(&(x / TILE_SIZE, y / TILE_SIZE)) else {
                    continue;
                };
                if tile.value[DataTile::index(x, y)].is_finite() {
                    covered += 1;
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            covered as f64 / total as f64
        }
    }
}

impl SectorBuilder {
    /// Square-grid convenience constructor for the per-pixel tests.
    pub fn new(bands: Vec<u16>, width: u32) -> Result<Self, String> {
        Self::new_rect(bands, width, width)
    }

    pub fn new_rect(bands: Vec<u16>, width: u32, height: u32) -> Result<Self, String> {
        Ok(Self {
            bands: validated_bands(bands)?,
            added: Vec::new(),
            width,
            height,
            tiles: BTreeMap::new(),
        })
    }

    pub fn bands(&self) -> &[u16] {
        &self.bands
    }

    pub fn add_band(&mut self, band: &BandAccumulator) -> Result<(), String> {
        if !self.bands.contains(&band.band) || self.added.contains(&band.band) {
            return Err(format!(
                "Band {} is not an unadded member band",
                band.band + 1
            ));
        }
        if band.width != self.width || band.height != self.height {
            return Err("Band plane does not match the sector grid".into());
        }
        self.added.push(band.band);
        for (key, sums) in &band.tiles {
            let tile = self.tiles.entry(*key).or_insert_with(|| BuilderTile {
                sum_value: vec![0.0; (TILE_SIZE * TILE_SIZE) as usize],
                sum_variance: vec![0.0; (TILE_SIZE * TILE_SIZE) as usize],
                complete: vec![0; (TILE_SIZE * TILE_SIZE) as usize],
            });
            for (pixel, &(sum_f, sum_w, samples)) in sums.iter().enumerate() {
                if samples == 0 {
                    continue;
                }
                tile.sum_value[pixel] += sum_f / sum_w;
                tile.sum_variance[pixel] += 1.0 / sum_w;
                tile.complete[pixel] += 1;
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<SectorImage, String> {
        if self.added.len() != self.bands.len() {
            return Err("Not every selected band was accumulated".into());
        }
        let count = self.bands.len() as f64;
        let all = self.bands.len() as u16;
        let mut complete_pixels = 0;
        let mut tiles = DataTiles::new();
        for (key, builder) in self.tiles {
            let mut tile = DataTile::empty();
            let mut tile_complete = 0;
            for pixel in 0..builder.sum_value.len() {
                if builder.complete[pixel] != all {
                    continue;
                }
                tile_complete += 1;
                tile.value[pixel] = (builder.sum_value[pixel] / count) as f32;
                tile.variance[pixel] = (builder.sum_variance[pixel] / (count * count)) as f32;
            }
            // A tile touched only by partial bands holds no complete pixel; do
            // not retain it or report it as a data tile.
            if tile_complete > 0 {
                complete_pixels += tile_complete;
                tiles.insert(key, tile);
            }
        }
        Ok(SectorImage {
            tiles,
            complete_pixels,
        })
    }
}

/// Single-pixel form with per-band detail, used by the review tooling and tests.
pub struct SectorReducer {
    bands: Vec<u16>,
}

impl SectorReducer {
    pub fn new(bands: Vec<u16>) -> Result<Self, String> {
        Ok(Self {
            bands: validated_bands(bands)?,
        })
    }

    /// Input and band error independence is the variance-propagation assumption, not
    /// a claim that nearest-pixel resampling or calibration correlations are solved.
    pub fn reduce(&self, measurements: &[Measurement]) -> SectorPixel {
        let bands: Vec<_> = self
            .bands
            .iter()
            .map(|band| {
                let mut accumulator = BandAccumulator::new(*band, 1);
                measurements
                    .iter()
                    .for_each(|sample| accumulator.add(0, sample));
                accumulator.depth(0)
            })
            .collect();
        let complete = bands.iter().all(|band| band.samples > 0);
        let count = bands.len() as f64;
        SectorPixel {
            value: complete
                .then(|| bands.iter().map(|band| band.value.unwrap()).sum::<f64>() / count),
            variance: complete.then(|| {
                bands.iter().map(|band| band.variance.unwrap()).sum::<f64>() / (count * count)
            }),
            bands,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(band: u16, value: f32, variance: f32, flags: u32) -> Measurement {
        Measurement {
            band,
            value,
            variance,
            flags,
            flag_bits: FlagBits {
                transient: 0,
                overflow: 1,
                sur_error: 2,
                nonfunc: 6,
                fullsample: 12,
                persist: 17,
                outlier: 19,
                source: 21,
            },
        }
    }

    fn at(image: &SectorImage, width: u32, pixel: usize) -> (f32, f32) {
        let (x, y) = grid_coordinates(pixel, width).unwrap();
        image
            .tiles
            .get(&(x / TILE_SIZE, y / TILE_SIZE))
            .map(|tile| {
                let index = DataTile::index(x, y);
                (tile.value[index], tile.variance[index])
            })
            .unwrap_or((f32::NAN, f32::NAN))
    }

    #[test]
    fn target_coverage_is_an_aperture_fraction_on_a_rectangular_grid() {
        let (width, height) = (117, 59);
        let mut tile = DataTile::empty();
        let (cx, cy) = (width / 2, height / 2);
        tile.value[DataTile::index(cx, cy)] = 1.0;
        let mut image = SectorImage {
            tiles: BTreeMap::new(),
            complete_pixels: 1,
        };
        image.tiles.insert((0, 0), tile);
        let empty = SectorImage {
            tiles: BTreeMap::new(),
            complete_pixels: 0,
        };
        assert_eq!(empty.target_coverage(width, height), 0.0);
        let partial = image.target_coverage(width, height);
        assert!(partial > 0.0 && partial < 0.1, "{partial}");
        let mut full = DataTile::empty();
        for y in 0..height {
            for x in 0..width {
                full.value[DataTile::index(x, y)] = 1.0;
            }
        }
        image.tiles.insert((0, 0), full);
        assert_eq!(image.target_coverage(width, height), 1.0);
    }

    #[test]
    fn depth_is_inverse_variance_and_bandwise_weights_are_equal() {
        let reduced = SectorReducer::new(vec![0, 1]).unwrap().reduce(&[
            sample(0, 2.0, 1.0, 0),
            sample(0, 10.0, 4.0, 0),
            sample(1, 12.0, 9.0, 0),
        ]);
        assert!((reduced.bands[0].value.unwrap() - 3.6).abs() < 1e-12);
        assert!((reduced.bands[0].variance.unwrap() - 0.8).abs() < 1e-12);
        assert!((reduced.value.unwrap() - 7.8).abs() < 1e-12);
        assert!((reduced.variance.unwrap() - 2.45).abs() < 1e-12);
        assert_eq!(reduced.bands[0].samples, 2);
    }
    #[test]
    fn strict_mask_rejects_source_ramp_and_outlier_without_temporal_clipping() {
        let reduced = SectorReducer::new(vec![0]).unwrap().reduce(&[
            sample(0, 2.0, 1.0, 1 << 21),
            sample(0, 100.0, 1.0, (1 << 21) | (1 << 0)),
            sample(0, 200.0, 1.0, 1 << 19),
            sample(0, 10.0, 1.0, 0),
        ]);
        assert_eq!(reduced.value, Some(6.0)); // The unflagged high sample is not sigma-clipped.
        assert_eq!(reduced.bands[0].samples, 2);
    }
    #[test]
    fn missing_selected_band_invalidates_sector_but_retains_depth_planes() {
        let reduced = SectorReducer::new(vec![0, 1])
            .unwrap()
            .reduce(&[sample(0, 0.0, 1.0, 0)]);
        assert_eq!(reduced.value, None);
        assert_eq!(reduced.variance, None);
        assert_eq!(reduced.bands[0].value, Some(0.0));
        assert_eq!(reduced.bands[1].samples, 0);
    }
    #[test]
    fn invalid_values_variances_and_other_bands_do_not_contribute() {
        let reduced = SectorReducer::new(vec![0]).unwrap().reduce(&[
            sample(0, f32::NAN, 1.0, 0),
            sample(0, 1.0, 0.0, 0),
            sample(0, 1.0, -1.0, 0),
            sample(0, 1.0, f32::INFINITY, 0),
            sample(1, 9.0, 1.0, 0),
        ]);
        assert_eq!(reduced.value, None);
        assert_eq!(reduced.bands[0].samples, 0);
    }
    #[test]
    fn more_depth_does_not_increase_one_bands_bandwise_weight() {
        let measurements = (0..100)
            .map(|_| sample(0, 2.0, 1.0, 0))
            .chain([sample(1, 10.0, 1.0, 0)])
            .collect::<Vec<_>>();
        let reduced = SectorReducer::new(vec![0, 1, 1])
            .unwrap()
            .reduce(&measurements);
        assert_eq!(reduced.value, Some(6.0));
        assert_eq!(reduced.bands[0].samples, 100);
        assert!(SectorReducer::new(vec![]).is_err());
        assert!(SectorReducer::new(vec![102]).is_err());
    }
    #[test]
    fn streaming_grid_builder_matches_the_per_pixel_reducer() {
        // Three pixels: complete, missing band 1, fully masked.
        let per_pixel: [Vec<Measurement>; 3] = [
            vec![
                sample(0, 2.0, 1.0, 0),
                sample(0, 10.0, 4.0, 0),
                sample(1, 12.0, 9.0, 0),
            ],
            vec![sample(0, 5.0, 2.0, 0)],
            vec![sample(0, 5.0, 2.0, 1 << 6), sample(1, 5.0, 2.0, 1 << 6)],
        ];
        let mut builder = SectorBuilder::new(vec![1, 0], 3).unwrap();
        for band in [0, 1] {
            let mut accumulator = BandAccumulator::new(band, 3);
            for (pixel, samples) in per_pixel.iter().enumerate() {
                samples
                    .iter()
                    .for_each(|sample| accumulator.add(pixel, sample));
            }
            builder.add_band(&accumulator).unwrap();
        }
        let image = builder.finish().unwrap();
        let reducer = SectorReducer::new(vec![0, 1]).unwrap();
        for (pixel, samples) in per_pixel.iter().enumerate() {
            let expected = reducer.reduce(samples);
            let (value, variance) = at(&image, 3, pixel);
            match expected.value {
                Some(expected_value) => {
                    assert_eq!(value, expected_value as f32);
                    assert_eq!(variance, expected.variance.unwrap() as f32);
                }
                None => assert!(value.is_nan() && variance.is_nan()),
            }
        }
        assert_eq!(image.complete_pixels, 1);
    }
    #[test]
    fn builder_allocates_only_tiles_touched_by_a_band() {
        let width = 513;
        let mut accumulator = BandAccumulator::new(0, width);
        accumulator.add((255 * width + 255) as usize, &sample(0, 2.0, 1.0, 0));
        assert_eq!(accumulator.tiles().len(), 1);
        let mut builder = SectorBuilder::new(vec![0], width).unwrap();
        builder.add_band(&accumulator).unwrap();
        let image = builder.finish().unwrap();
        assert_eq!(image.tiles.len(), 1, "513x513 would be nine tiles if dense");
        assert_eq!(image.complete_pixels, 1);
        assert_eq!(
            image.tiles[&(0, 0)].value.len(),
            (TILE_SIZE * TILE_SIZE) as usize
        );
        assert_eq!(image.data_tiles(), vec![[0, 0]]);
    }
    #[test]
    fn pixel_indices_above_u32_do_not_wrap_on_grids_wider_than_65536() {
        // 70,000² = 4.9e9 pixels > 2^32. Only one 256×256 tile is allocated.
        let width = 70_000_u32;
        let (x, y) = (width - 1, width - 1);
        let pixel = grid_pixel(x, y, width).unwrap();
        assert_eq!(pixel as u64, u64::from(y) * u64::from(width) + u64::from(x));
        assert!(pixel as u64 > u64::from(u32::MAX));
        assert_eq!(grid_coordinates(pixel, width), Some((x, y)));
        // The old u32 arithmetic would have aliased this index to another pixel.
        let wrapped = (pixel as u64 % (1_u64 << 32)) as usize;
        assert_ne!(grid_coordinates(wrapped, width), Some((x, y)));
        assert_eq!(grid_coordinates(pixel + 1, width), None, "past the grid");
        assert!(grid_pixel(width, 0, width).is_err());
        assert_eq!(grid_coordinates(usize::MAX, u32::MAX), None);
        assert_eq!(
            grid_pixel(u32::MAX - 1, u32::MAX - 1, u32::MAX).unwrap() as u64,
            u64::from(u32::MAX - 1) * u64::from(u32::MAX) + u64::from(u32::MAX - 1)
        );

        let mut accumulator = BandAccumulator::new(0, width);
        accumulator.add(pixel, &sample(0, 2.0, 1.0, 0));
        accumulator.add(wrapped, &sample(0, 9.0, 1.0, 0));
        let far = (x / TILE_SIZE, y / TILE_SIZE);
        assert_eq!(accumulator.tiles().len(), 2);
        assert!(accumulator.tiles().contains_key(&far));
        let depth = accumulator.depth(pixel);
        assert_eq!((depth.value, depth.samples), (Some(2.0), 1));
        assert_eq!(accumulator.depth(wrapped).value, Some(9.0));
        let mut builder = SectorBuilder::new(vec![0], width).unwrap();
        builder.add_band(&accumulator).unwrap();
        let image = builder.finish().unwrap();
        assert_eq!(image.tiles.len(), 2);
        assert_eq!(at(&image, width, pixel).0, 2.0);
    }
    #[test]
    fn builder_refuses_missing_duplicate_or_foreign_bands() {
        let mut builder = SectorBuilder::new(vec![0, 1], 1).unwrap();
        builder.add_band(&BandAccumulator::new(0, 1)).unwrap();
        assert!(builder.add_band(&BandAccumulator::new(0, 1)).is_err());
        assert!(builder.add_band(&BandAccumulator::new(5, 1)).is_err());
        assert!(builder.add_band(&BandAccumulator::new(1, 2)).is_err());
        assert!(builder.finish().is_err(), "band 1 never added");
    }
}
