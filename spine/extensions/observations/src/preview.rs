use crate::types::{SnippetPreviewInfo, TileCoordinate};
use image::ImageEncoder;
use spearv_core::{sample_at, Cutout, Input, TanGrid};
use std::collections::HashMap;

pub use crate::types::TILE_SIZE;
pub struct Preview {
    pub info: SnippetPreviewInfo,
    pub tiles: HashMap<(u32, u32), Vec<f32>>,
}

/// One exact exposure/band. No multi-member merge or independently stretched video.
/// `bounds` is the inclusive 0-based FITS footprint from discovery, so a wide
/// field does not scan or retain pixels this detector cannot reach.
pub fn render(
    cutout: Cutout,
    grid: TanGrid,
    band: u16,
    mask: bool,
    bounds: [u32; 4],
    mut info: SnippetPreviewInfo,
) -> Result<Preview, String> {
    let zodi = cutout
        .zodi
        .as_ref()
        .ok_or("Exact preview requires the ZODI plane")?;
    let input = Input {
        wcs: &cutout.wcs,
        image: &cutout.image.data,
        variance: &cutout.variance.data,
        flags: &cutout.flags.data,
        zodi: &zodi.data,
        wavelength: &cutout.wavelength,
        origin_fits: cutout.origin_fits,
        width: cutout.image.width as usize,
        flag_bits: cutout.flag_bits,
        mask,
        ignore_outlier: false,
    };
    let [left, right, bottom, top] = bounds;
    let last_x = grid.width - 1;
    let last_y = grid.height - 1;
    let (left, right, bottom, top) = (
        left.min(last_x),
        right.min(last_x),
        bottom.min(last_y),
        top.min(last_y),
    );
    let mut tiles: HashMap<(u32, u32), Vec<f32>> = HashMap::new();
    let mut valid = Vec::new();
    for y in bottom..=top {
        for x in left..=right {
            let (ra, dec) = grid.sky(f64::from(x) + 1.0, f64::from(y) + 1.0);
            let Some(hit) = sample_at(&input, ra, dec, true) else {
                continue;
            };
            if hit.band != Some(band)
                || hit.rejected
                || !hit.flux.is_finite()
                || !hit.variance.is_finite()
                || hit.variance <= 0.0
            {
                continue;
            }
            let png_y = grid.height - 1 - y;
            let tile = tiles
                .entry((x / TILE_SIZE, png_y / TILE_SIZE))
                .or_insert_with(|| vec![f32::NAN; (TILE_SIZE * TILE_SIZE) as usize]);
            tile[((png_y % TILE_SIZE) * TILE_SIZE + x % TILE_SIZE) as usize] = hit.flux as f32;
            valid.push(hit.flux as f32);
        }
    }
    valid.sort_by(f32::total_cmp);
    info.valid_samples = valid.len() as u64;
    info.state = if valid.is_empty() { "empty" } else { "ready" }.into();
    info.bytes_read = cutout.bytes_read;
    info.requests = cutout.s3_requests;
    info.stretch_low = percentile(&valid, 0.02);
    info.stretch_high = percentile(&valid, 0.995);
    let mut data_tiles: Vec<_> = tiles
        .keys()
        .map(|(x, y)| TileCoordinate { x: *x, y: *y })
        .collect();
    data_tiles.sort_by_key(|tile| (tile.y, tile.x));
    info.data_tiles = data_tiles;
    Ok(Preview { info, tiles })
}

pub(crate) fn percentile(values: &[f32], fraction: f64) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values[((values.len() - 1) as f64 * fraction).round() as usize]
}

pub fn tile_png(preview: &Preview, x: u32, y: u32) -> Result<Vec<u8>, String> {
    let values = preview.tiles.get(&(x, y));
    encode_tile(
        preview.info.width,
        preview.info.height,
        x,
        y,
        preview.info.stretch_low,
        preview.info.stretch_high,
        |px, py| {
            let i = ((py % TILE_SIZE) * TILE_SIZE + px % TILE_SIZE) as usize;
            values.map(|tile| tile[i]).unwrap_or(f32::NAN)
        },
    )
}

/// Grey level for a calibrated value under a fixed mapping. Shared by previews and
/// sequence frames so one mapping means one encoding.
pub fn grey_level(value: f32, low: f32, high: f32) -> u8 {
    let span = (high - low).max(f32::EPSILON);
    (((value - low) / span).clamp(0.0, 1.0).powf(0.7) * 255.0).round() as u8
}

/// One 256×256 RGBA tile. `value(px, py)` takes grid PNG coordinates. NaN is
/// no-data and is drawn in a colour no finite value can produce; pixels past the
/// grid edge are transparent.
pub fn encode_tile(
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    low: f32,
    high: f32,
    value: impl Fn(u32, u32) -> f32,
) -> Result<Vec<u8>, String> {
    if u64::from(x) * u64::from(TILE_SIZE) >= u64::from(width)
        || u64::from(y) * u64::from(TILE_SIZE) >= u64::from(height)
    {
        return Err("Tile is outside the grid".into());
    }
    let mut rgba = vec![0_u8; (TILE_SIZE * TILE_SIZE * 4) as usize];
    for py in 0..TILE_SIZE {
        for px in 0..TILE_SIZE {
            let (gx, gy) = (x * TILE_SIZE + px, y * TILE_SIZE + py);
            if gx >= width || gy >= height {
                continue;
            }
            let i = (py * TILE_SIZE + px) as usize;
            let sample = value(gx, gy);
            let color = if sample.is_finite() {
                let grey = grey_level(sample, low, high);
                [grey, grey, grey, 255]
            } else {
                [24, 28, 36, 255]
            };
            rgba[i * 4..i * 4 + 4].copy_from_slice(&color);
        }
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&rgba, TILE_SIZE, TILE_SIZE, image::ExtendedColorType::Rgba8)
        .map_err(|err| err.to_string())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_zero_is_distinct_from_missing_data_and_tiles_are_256() {
        let row = crate::types::SnippetRow {
            id: "a".into(),
            snippet_ref: "a".into(),
            band: 1,
            mjd: 1.0,
            utc: "".into(),
            release: "qr3".into(),
            detector: 1,
            obsid: "a".into(),
            version: "1".into(),
            geometric_samples: 1,
            wl_min_um: 0.75,
            wl_max_um: 0.75,
            target_covered: true,
            quality: serde_json::Value::Null,
        };
        let info = SnippetPreviewInfo {
            preview_id: "a".into(),
            snippet: row,
            width: 2,
            height: 1,
            tile_size: TILE_SIZE,
            state: "ready".into(),
            valid_samples: 1,
            bytes_read: 0,
            requests: 0,
            stretch_low: 0.0,
            stretch_high: 1.0,
            data_tiles: vec![],
        };
        let mut tile = vec![f32::NAN; (TILE_SIZE * TILE_SIZE) as usize];
        tile[0] = 0.0;
        let preview = Preview {
            info,
            tiles: HashMap::from([((0, 0), tile)]),
        };
        let png = tile_png(&preview, 0, 0).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (256, 256));
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(1, 0).0, [24, 28, 36, 255]);
        assert_eq!(decoded.get_pixel(2, 0).0, [0, 0, 0, 0]);
        assert!(tile_png(&preview, 1, 0).is_err());
    }

    #[test]
    fn render_scans_the_support_bounds_and_lists_only_data_tiles() {
        use crate::types::SnippetRow;
        use spearv_core::{
            Cutout, FlagBits, Plane, Poly, Sip, WavelengthMap, Wcs, PIXEL_SCALE_DEG,
        };
        let wcs = Wcs {
            crpix: [3.0, 3.0],
            crval: [10.0, 20.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: Sip {
                a: Poly::new(0),
                b: Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [5, 5],
        };
        let wavelength = WavelengthMap {
            x: vec![1.0, 5.0],
            y: vec![1.0, 5.0],
            wavelength: vec![0.753; 4],
            bandwidth: vec![0.02; 4],
            nx: 2,
            ny: 2,
        };
        let grid = TanGrid::new(10.0, 20.0, 513.0 * PIXEL_SCALE_DEG);
        let geometry = spearv_core::ExposureGeometry {
            wcs: wcs.clone(),
            wavelength: wavelength.clone(),
            header_bytes: 0,
        };
        let support = crate::browse::supports(&geometry, &grid, &[0]);
        assert_eq!(support.len(), 1);
        let plane = |value| Plane {
            width: 5,
            height: 5,
            data: vec![value; 25],
        };
        let cutout = Cutout {
            image: plane(10.0),
            variance: plane(1.0),
            zodi: Some(plane(2.0)),
            flags: Plane {
                width: 5,
                height: 5,
                data: vec![0; 25],
            },
            flags_valid: None,
            wcs,
            wavelength,
            origin_fits: (1, 1),
            clipped: false,
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
            bytes_read: 0,
            s3_requests: 0,
        };
        let info = SnippetPreviewInfo {
            preview_id: "a".into(),
            snippet: SnippetRow {
                id: "a".into(),
                snippet_ref: "a".into(),
                band: 1,
                mjd: 1.0,
                utc: String::new(),
                release: "qr3".into(),
                detector: 1,
                obsid: "a".into(),
                version: "1".into(),
                geometric_samples: support[0].count,
                wl_min_um: 0.75,
                wl_max_um: 0.75,
                target_covered: true,
                quality: serde_json::Value::Null,
            },
            width: grid.width,
            height: grid.height,
            tile_size: TILE_SIZE,
            state: "building".into(),
            valid_samples: 0,
            bytes_read: 0,
            requests: 0,
            stretch_low: 0.0,
            stretch_high: 0.0,
            data_tiles: vec![],
        };
        let full_bounds = [0, grid.width - 1, 0, grid.height - 1];
        let full = render(
            cutout.clone(),
            grid.clone(),
            support[0].band,
            true,
            full_bounds,
            info.clone(),
        )
        .unwrap();
        let preview = render(
            cutout,
            grid,
            support[0].band,
            true,
            support[0].grid_bounds,
            info,
        )
        .unwrap();
        assert_eq!(preview.info.valid_samples, support[0].count);
        assert_eq!(
            preview.info.valid_samples, full.info.valid_samples,
            "bounded and full-grid previews see the same valid samples"
        );
        // NaN is not equal to itself, so compare the finite entries by index.
        let finite = |tiles: &HashMap<(u32, u32), Vec<f32>>| {
            let mut entries: Vec<_> = tiles
                .iter()
                .flat_map(|(key, tile)| {
                    tile.iter()
                        .enumerate()
                        .filter(|(_, value)| value.is_finite())
                        .map(|(index, value)| (*key, index, *value))
                })
                .collect();
            entries.sort_by_key(|(key, index, _)| (*key, *index));
            entries
        };
        assert_eq!(finite(&preview.tiles), finite(&full.tiles));
        assert_eq!(preview.info.data_tiles, full.info.data_tiles);
        assert_eq!(preview.info.data_tiles.len(), preview.tiles.len());
        let mut expected: Vec<_> = preview
            .tiles
            .keys()
            .map(|(x, y)| TileCoordinate { x: *x, y: *y })
            .collect();
        expected.sort_by_key(|tile| (tile.y, tile.x));
        assert_eq!(preview.info.data_tiles, expected);
        assert!(
            preview.info.valid_samples <= 25,
            "only the detector support is read: {}",
            preview.info.valid_samples
        );
    }
}
