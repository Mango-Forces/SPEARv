//! Sparse per-band accepted-measurement counts.
//!
//! The count sidecar is written one selected band at a time while the sector is
//! accumulated, so no dense band cube is ever materialized. Each declared band
//! stores only the tiles that contain an accepted measurement; a missing tile
//! means zero accepted counts for that declared band. A band absent from the
//! file is unknown, never zero. Edge tiles are cropped to the actual grid.
use crate::reduction::{BandAccumulator, DataTile};
use crate::types::{BandContribution, TILE_SIZE};
#[cfg(test)]
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SPKVCNT1";
const VERSION: u32 = 1;

/// One completed sidecar: the aggregate summary plus the file byte size.
pub struct Counts {
    pub contributions: Vec<BandContribution>,
    pub bytes: u64,
}

/// Streams one declared band into a sparse little-endian `u64` sidecar.
pub struct CountWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    width: u32,
    height: u32,
    bands: Vec<u16>,
    contributions: Vec<BandContribution>,
}

impl CountWriter {
    /// Square-grid convenience constructor for the count fixtures.
    #[cfg(test)]
    pub fn create(path: &Path, bands: &[u16], width: u32) -> Result<Self, String> {
        Self::create_rect(path, bands, width, width)
    }

    /// `bands` must be the builder's declared internal bands in ascending order.
    pub fn create_rect(
        path: &Path,
        bands: &[u16],
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let file = File::create(path)
            .map_err(|error| format!("Could not create {}: {error}", path.display()))?;
        let mut writer = BufWriter::new(file);
        let mut header = || -> Result<(), String> {
            writer
                .write_all(MAGIC)
                .and_then(|_| writer.write_all(&VERSION.to_le_bytes()))
                .and_then(|_| writer.write_all(&(bands.len() as u32).to_le_bytes()))
                .and_then(|_| writer.write_all(&width.to_le_bytes()))
                .and_then(|_| writer.write_all(&height.to_le_bytes()))
                .map_err(|error| format!("Could not write {}: {error}", path.display()))
        };
        header()?;
        Ok(Self {
            path: path.to_path_buf(),
            writer,
            width,
            height,
            bands: bands.to_vec(),
            contributions: Vec::with_capacity(bands.len()),
        })
    }

    /// Write one finished band. Tiles are emitted in row-major order and cropped
    /// to the grid; a tile with no accepted count is omitted.
    pub fn write_band(
        &mut self,
        accumulator: &BandAccumulator,
    ) -> Result<BandContribution, String> {
        if accumulator.width() != self.width || accumulator.height() != self.height {
            return Err("Band plane does not match the count sidecar grid".into());
        }
        if self.bands.get(self.contributions.len()).copied() != Some(accumulator.band()) {
            return Err("Bands must be written in declared order".into());
        }
        let mut tiles: Vec<_> = accumulator.tiles().iter().collect();
        tiles.sort_by_key(|((x, y), _)| (*y, *x));
        let mut kept = Vec::with_capacity(tiles.len());
        let mut samples = 0_u64;
        let mut pixels = 0_u64;
        let mut bytes = 0_u64;
        for ((x, y), sums) in tiles {
            let x0 = x.saturating_mul(TILE_SIZE);
            let y0 = y.saturating_mul(TILE_SIZE);
            if x0 >= self.width || y0 >= self.height {
                continue;
            }
            let width = (self.width - x0).min(TILE_SIZE);
            let height = (self.height - y0).min(TILE_SIZE);
            let mut tile_samples = 0_u64;
            let mut tile_pixels = 0_u64;
            for py in 0..height {
                for px in 0..width {
                    let count = sums[DataTile::index(x0 + px, y0 + py)].2;
                    if count > 0 {
                        tile_samples += count;
                        tile_pixels += 1;
                    }
                }
            }
            if tile_samples == 0 {
                continue;
            }
            samples += tile_samples;
            pixels += tile_pixels;
            bytes += u64::from(width) * u64::from(height) * 8;
            kept.push((*x, *y, width, height, sums));
        }

        let tiles_written = kept.len() as u32;
        let mut write = |slice: &[u8]| {
            self.writer
                .write_all(slice)
                .map_err(|error| format!("Could not write {}: {error}", self.path.display()))
        };
        write(&(accumulator.band() as u32).to_le_bytes())?;
        write(&tiles_written.to_le_bytes())?;
        write(&bytes.to_le_bytes())?;
        for (x, y, width, height, sums) in &kept {
            write(&x.to_le_bytes())?;
            write(&y.to_le_bytes())?;
            write(&width.to_le_bytes())?;
            write(&height.to_le_bytes())?;
            let x0 = x * TILE_SIZE;
            let y0 = y * TILE_SIZE;
            for py in 0..*height {
                for px in 0..*width {
                    let count = sums[DataTile::index(x0 + px, y0 + py)].2;
                    write(&count.to_le_bytes())?;
                }
            }
        }
        let contribution = BandContribution {
            band: accumulator.band() + 1,
            samples,
            pixels,
            tiles: tiles_written,
            bytes,
        };
        self.contributions.push(contribution.clone());
        Ok(contribution)
    }

    pub fn finish(mut self) -> Result<Counts, String> {
        if self.contributions.len() != self.bands.len() {
            return Err("Not every declared band was written to the count sidecar".into());
        }
        self.writer
            .flush()
            .map_err(|error| format!("Could not write {}: {error}", self.path.display()))?;
        let bytes = self
            .writer
            .get_ref()
            .metadata()
            .map_err(|error| format!("Could not stat {}: {error}", self.path.display()))?
            .len();
        Ok(Counts {
            contributions: self.contributions,
            bytes,
        })
    }
}

#[cfg(test)]
pub(crate) struct ReadCounts {
    pub width: u32,
    pub height: u32,
    pub bands: BTreeMap<u16, ReadBand>,
}

#[cfg(test)]
pub(crate) struct ReadBand {
    pub bytes: u64,
    pub tiles: BTreeMap<(u32, u32), ReadTile>,
}

#[cfg(test)]
pub(crate) struct ReadTile {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<u64>,
}

#[cfg(test)]
fn read_u32(reader: &mut impl std::io::Read) -> Result<u32, String> {
    let mut bytes = [0_u8; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
fn read_u64(reader: &mut impl std::io::Read) -> Result<u64, String> {
    let mut bytes = [0_u8; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
pub(crate) fn read(path: &Path) -> Result<ReadCounts, String> {
    use std::io::Read;
    let mut reader = File::open(path).map_err(|error| error.to_string())?;
    let mut magic = [0_u8; 8];
    reader
        .read_exact(&mut magic)
        .map_err(|error| error.to_string())?;
    if &magic != MAGIC {
        return Err("Not a count sidecar".into());
    }
    let version = read_u32(&mut reader)?;
    if version != VERSION {
        return Err(format!("Unknown count sidecar version {version}"));
    }
    let band_count = read_u32(&mut reader)? as usize;
    let width = read_u32(&mut reader)?;
    let height = read_u32(&mut reader)?;
    let mut bands = BTreeMap::new();
    for _ in 0..band_count {
        let band = read_u32(&mut reader)? as u16;
        let tile_count = read_u32(&mut reader)?;
        let bytes = read_u64(&mut reader)?;
        let mut tiles = BTreeMap::new();
        let mut stored = 0_u64;
        for _ in 0..tile_count {
            let x = read_u32(&mut reader)?;
            let y = read_u32(&mut reader)?;
            let tile_width = read_u32(&mut reader)?;
            let tile_height = read_u32(&mut reader)?;
            let mut cells = Vec::with_capacity((tile_width * tile_height) as usize);
            for _ in 0..tile_width * tile_height {
                cells.push(read_u64(&mut reader)?);
            }
            stored += u64::from(tile_width) * u64::from(tile_height) * 8;
            tiles.insert(
                (x, y),
                ReadTile {
                    width: tile_width,
                    height: tile_height,
                    cells,
                },
            );
        }
        if stored != bytes {
            return Err(format!("Band {band} count bytes do not match its tiles"));
        }
        bands.insert(band, ReadBand { bytes, tiles });
    }
    Ok(ReadCounts {
        width,
        height,
        bands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reduction::{grid_pixel, Measurement, SectorBuilder};
    use spearv_core::FlagBits;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    fn sample(band: u16, value: f32) -> Measurement {
        Measurement {
            band,
            value,
            variance: 1.0,
            flags: 0,
            flag_bits: bits(),
        }
    }

    fn temp_directory() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "spearv-counts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[derive(serde::Deserialize)]
    struct Fixture {
        width: u32,
        height: u32,
        snippets: Vec<FixtureSnippet>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureSnippet {
        snippet: crate::types::SnippetRow,
        common_grid: Vec<Option<FixtureSample>>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureSample {
        png_x: u32,
        png_y: u32,
        image_minus_zodi: Option<f32>,
        variance: Option<f32>,
        finite_positive_variance: bool,
        spectrum_mask_rejects: bool,
    }

    /// Audited fixture oracle: band 34 = 105 accepted counts, band 4 = 143,
    /// band 5 = 144, bandwise complete pixels = 62. The 44 support value is only
    /// the four-snippet intersection and is not the band count.
    #[test]
    fn fixture_counts_match_the_audited_oracle() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../web/e2e/fixtures/merge-review.json");
        let fixture: Fixture = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((fixture.width, fixture.height), (12, 12));
        let mut accumulators: BTreeMap<u16, BandAccumulator> = BTreeMap::new();
        for snippet in &fixture.snippets {
            let band = snippet.snippet.band - 1;
            let accumulator = accumulators
                .entry(band)
                .or_insert_with(|| BandAccumulator::new(band, fixture.width));
            for point in snippet.common_grid.iter().flatten() {
                if point.spectrum_mask_rejects || !point.finite_positive_variance {
                    continue;
                }
                let (Some(value), Some(variance)) = (point.image_minus_zodi, point.variance) else {
                    continue;
                };
                if !variance.is_finite() || variance <= 0.0 {
                    continue;
                }
                let pixel = grid_pixel(point.png_x, point.png_y, fixture.width).unwrap();
                accumulator.add(
                    pixel,
                    &Measurement {
                        band,
                        value,
                        variance,
                        flags: 0,
                        flag_bits: bits(),
                    },
                );
            }
        }
        let directory = temp_directory();
        let path = directory.join("counts.bin");
        let bands: Vec<_> = accumulators.keys().copied().collect();
        let mut writer = CountWriter::create(&path, &bands, fixture.width).unwrap();
        let mut contributions = Vec::new();
        for accumulator in accumulators.values() {
            contributions.push(writer.write_band(accumulator).unwrap());
        }
        let counts = writer.finish().unwrap();
        assert_eq!(
            counts.bytes,
            24 + 3 * 16 + 3 * (16 + 12 * 12 * 8),
            "fixture sidecar is three cropped 12×12 bands plus headers"
        );
        let by_band: BTreeMap<u16, _> =
            contributions.iter().map(|band| (band.band, band)).collect();
        for (band, samples, pixels, bytes) in [
            (34, 105, 62, 12 * 12 * 8),
            (4, 143, 143, 12 * 12 * 8),
            (5, 144, 144, 12 * 12 * 8),
        ] {
            let contribution = by_band[&band];
            assert_eq!(contribution.samples, samples, "band {band}");
            assert_eq!(contribution.pixels, pixels, "band {band}");
            assert_eq!(contribution.tiles, 1, "band {band}");
            assert_eq!(contribution.bytes, bytes, "band {band}");
        }
        let mut builder = SectorBuilder::new(bands.clone(), fixture.width).unwrap();
        for accumulator in accumulators.values() {
            builder.add_band(accumulator).unwrap();
        }
        let image = builder.finish().unwrap();
        assert_eq!(image.complete_pixels, 62);

        let stored = read(&path).unwrap();
        assert_eq!((stored.width, stored.height), (12, 12));
        assert_eq!(stored.bands.len(), 3);
        for band in &bands {
            let stored_band = &stored.bands[band];
            assert_eq!(stored_band.bytes, 12 * 12 * 8);
            let tile = &stored_band.tiles[&(0, 0)];
            assert_eq!((tile.width, tile.height), (12, 12));
            assert_eq!(tile.cells.iter().sum::<u64>(), by_band[&(band + 1)].samples);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn edge_tiles_are_cropped_and_zero_only_or_undeclared_bands_are_not_invented() {
        let width = 300;
        let directory = temp_directory();
        let path = directory.join("counts.bin");
        let mut first = BandAccumulator::new(0, width);
        first.add(grid_pixel(299, 299, width).unwrap(), &sample(0, 2.0));
        let second = BandAccumulator::new(1, width);
        let mut writer = CountWriter::create(&path, &[0, 1], width).unwrap();
        let first_contribution = writer.write_band(&first).unwrap();
        let second_contribution = writer.write_band(&second).unwrap();
        writer.finish().unwrap();
        assert_eq!(first_contribution.tiles, 1);
        assert_eq!(first_contribution.bytes, 44 * 44 * 8);
        assert_eq!(second_contribution.samples, 0);
        assert_eq!(second_contribution.tiles, 0);
        let stored = read(&path).unwrap();
        let tile = &stored.bands[&0].tiles[&(1, 1)];
        assert_eq!((tile.width, tile.height), (44, 44));
        assert_eq!(tile.cells[43 * 44 + 43], 1);
        assert!(stored.bands[&1].tiles.is_empty(), "declared band is zero");
        assert!(!stored.bands.contains_key(&2), "undeclared band is unknown");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
