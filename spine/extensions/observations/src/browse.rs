use crate::types::{mjd_utc, ObservationPageQuery, ObservationRegion, SnippetRow};
use sha2::{Digest, Sha256};
use spearv_core::{geometric_sample, ExposureGeometry, PixelWindow, TanGrid};
use spearv_index::ExposureRow;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Support {
    pub band: u16,
    pub count: u64,
    pub window: PixelWindow,
    /// Inclusive zero-based common-grid x/y bounds (FITS orientation).
    pub grid_bounds: [u32; 4],
    pub wl_min: f64,
    pub wl_max: f64,
}

/// Exhaustive common-grid samples inside the detector's exact footprint, not a
/// centre/corner/coarse-lattice approximation. The footprint bound is derived
/// from the WCS/SIP detector edges, so a wide field only scans the pixels one
/// detector can actually reach. Bounded working memory: 102 support boxes, no
/// image planes and no band cube.
pub fn supports(geometry: &ExposureGeometry, grid: &TanGrid, bands: &[u16]) -> Vec<Support> {
    let Some([left, right, bottom, top]) = grid.detector_bounds(&geometry.wcs) else {
        return Vec::new();
    };
    supports_in(geometry, grid, bands, [left, right, bottom, top])
}

fn supports_in(
    geometry: &ExposureGeometry,
    grid: &TanGrid,
    bands: &[u16],
    bounds: [u32; 4],
) -> Vec<Support> {
    let [left, right, bottom, top] = bounds;
    let mut supports: BTreeMap<u16, Support> = BTreeMap::new();
    for y in bottom..=top {
        for x in left..=right {
            let (ra, dec) = grid.sky(f64::from(x) + 1.0, f64::from(y) + 1.0);
            let Some((ix, iy, wl, Some(band))) =
                geometric_sample(&geometry.wcs, &geometry.wavelength, ra, dec)
            else {
                continue;
            };
            if !bands.contains(&band) {
                continue;
            }
            let support = supports.entry(band).or_insert_with(|| Support {
                band,
                count: 0,
                grid_bounds: [x, x, y, y],
                window: PixelWindow {
                    x0: ix - 1,
                    y0: iy - 1,
                    width: 1,
                    height: 1,
                    clipped: false,
                },
                wl_min: wl,
                wl_max: wl,
            });
            let right = (support.window.x0 + support.window.width - 1).max(ix - 1);
            let top = (support.window.y0 + support.window.height - 1).max(iy - 1);
            support.window.x0 = support.window.x0.min(ix - 1);
            support.window.y0 = support.window.y0.min(iy - 1);
            support.window.width = right - support.window.x0 + 1;
            support.window.height = top - support.window.y0 + 1;
            support.count += 1;
            support.grid_bounds[0] = support.grid_bounds[0].min(x);
            support.grid_bounds[1] = support.grid_bounds[1].max(x);
            support.grid_bounds[2] = support.grid_bounds[2].min(y);
            support.grid_bounds[3] = support.grid_bounds[3].max(y);
            support.wl_min = support.wl_min.min(wl);
            support.wl_max = support.wl_max.max(wl);
        }
    }
    supports.into_values().collect()
}

pub fn snippet_id(region: &ObservationRegion, file: &ExposureRow, band: u16) -> String {
    let (fov_l, fov_h) = region.axes();
    let tuple = serde_json::to_vec(&(
        region.ra.to_bits(),
        region.dec.to_bits(),
        fov_l.to_bits(),
        fov_h.to_bits(),
        &file.key,
        &file.version,
        &file.etag,
        band,
    ))
    .expect("serializable identity");
    hex::encode(Sha256::digest(tuple))
}

pub fn snippet_row(
    region: &ObservationRegion,
    file: &ExposureRow,
    support: &Support,
) -> Result<SnippetRow, String> {
    let id = snippet_id(region, file, support.band);
    let grid = region.grid();
    let [left, right, bottom, top] = support.grid_bounds;
    let target_x = grid.width / 2;
    let target_y = grid.height / 2;
    Ok(SnippetRow {
        snippet_ref: id.clone(),
        id,
        band: support.band + 1,
        mjd: file.mjd_avg,
        utc: mjd_utc(file.mjd_avg)?,
        release: file.release.clone(),
        detector: file.detector,
        obsid: file.obsid.clone(),
        version: file.version.clone(),
        geometric_samples: support.count,
        wl_min_um: support.wl_min,
        wl_max_um: support.wl_max,
        target_covered: left <= target_x
            && target_x <= right
            && bottom <= target_y
            && target_y <= top,
        quality: serde_json::to_value(&file.quality).expect("quality"),
    })
}

pub fn page_rows(
    rows: &[SnippetRow],
    query: &ObservationPageQuery,
) -> Result<(usize, Vec<SnippetRow>), String> {
    let sort = query.sort.as_deref().unwrap_or("time");
    let direction = query.direction.as_deref().unwrap_or("asc");
    if !["time", "band", "psf"].contains(&sort) || !["asc", "desc"].contains(&direction) {
        return Err("Sort must be time/band/psf, direction asc/desc".into());
    }
    let passing_only = match query.quality.as_deref() {
        None | Some("") | Some("any") => false,
        Some("pass") => true,
        Some(_) => return Err("Quality must be any or pass".into()),
    };
    if query.limit == 0 {
        return Err("Page limit must be positive".into());
    }
    let terms: Vec<_> = query
        .search
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let mut matching: Vec<_> = rows
        .iter()
        .filter(|row| {
            let text = format!(
                "band {} {} {} d{} {} {}",
                row.band, row.utc, row.release, row.detector, row.obsid, row.version
            )
            .to_ascii_lowercase();
            terms.iter().all(|term| {
                if term.bytes().all(|ch| ch.is_ascii_digit()) {
                    return term.parse::<u16>().ok() == Some(row.band);
                }
                text.contains(term)
            })
        })
        .filter(|row| !passing_only || quality_flags(&row.quality) == 0)
        .collect();
    matching.sort_by(|a, b| {
        if sort == "psf" {
            // Missing PSF widths sort after every measured one in either direction.
            let width = |row: &SnippetRow| row.quality.get("psf_fwhm").and_then(|value| value.as_f64());
            let measured = |x: f64, y: f64| if direction == "desc" { y.total_cmp(&x) } else { x.total_cmp(&y) };
            return match (width(a), width(b)) {
                (Some(x), Some(y)) => measured(x, y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then(a.mjd.total_cmp(&b.mjd))
            .then(a.id.cmp(&b.id));
        }
        let primary = if sort == "band" {
            a.band.cmp(&b.band).then(a.mjd.total_cmp(&b.mjd))
        } else {
            a.mjd.total_cmp(&b.mjd).then(a.band.cmp(&b.band))
        };
        let order = primary.then(a.id.cmp(&b.id));
        if direction == "desc" {
            order.reverse()
        } else {
            order
        }
    });
    Ok((
        matching.len(),
        matching
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .cloned()
            .collect(),
    ))
}

/// Header flags on one row, matching the web `qualityFlags`: a non-Pass DQA,
/// aurora, airglow or shuttle glow, satellite streaks, or a nonzero FINAST.
/// Unknown fields raise nothing.
pub fn quality_flags(quality: &serde_json::Value) -> usize {
    let text = |key: &str| quality.get(key).and_then(|value| value.as_str());
    let flag = |key: &str| quality.get(key).and_then(|value| value.as_bool()) == Some(true);
    let number = |key: &str| quality.get(key).and_then(|value| value.as_i64());
    [
        text("dqa_l1").is_some_and(|value| !value.eq_ignore_ascii_case("pass")),
        text("dqa_l2").is_some_and(|value| !value.eq_ignore_ascii_case("pass")),
        flag("aurora"),
        flag("airglow"),
        flag("shuttle_glow"),
        number("n_streaks").is_some_and(|count| count > 0),
        number("finast").is_some_and(|value| value != 0),
    ]
    .into_iter()
    .filter(|raised| *raised)
    .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spearv_core::{sample_at, FlagBits, Input, Poly, Sip, WavelengthMap, Wcs, PIXEL_SCALE_DEG};
    pub fn geometry() -> ExposureGeometry {
        ExposureGeometry {
            wcs: Wcs {
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
            },
            wavelength: WavelengthMap {
                x: vec![1.0, 5.0],
                y: vec![1.0, 5.0],
                wavelength: vec![0.753, 0.753, 0.79, 0.79],
                bandwidth: vec![0.02; 4],
                nx: 2,
                ny: 2,
            },
            header_bytes: 0,
        }
    }
    #[test]
    fn edge_only_bands_and_disjoint_filters_follow_actual_samples() {
        let geometry = geometry();
        let grid = TanGrid::new(10.0, 20.0, 5.0 * PIXEL_SCALE_DEG);
        let found = supports(&geometry, &grid, &[0, 2]);
        assert_eq!(
            found.iter().map(|hit| hit.band).collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(found.iter().all(|hit| hit.count > 0));
        let flat = vec![1.0; 25];
        let flags = vec![0; 25];
        let bits = FlagBits {
            transient: 0,
            overflow: 1,
            sur_error: 2,
            nonfunc: 6,
            fullsample: 12,
            persist: 17,
            outlier: 19,
            source: 21,
        };
        let input = Input {
            wcs: &geometry.wcs,
            wavelength: &geometry.wavelength,
            image: &flat,
            variance: &flat,
            zodi: &flat,
            flags: &flags,
            origin_fits: (1, 1),
            width: 5,
            flag_bits: bits,
            mask: false,
            ignore_outlier: false,
        };
        for support in found {
            let count = (0..grid.width)
                .flat_map(|y| (0..grid.width).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    let (ra, dec) = grid.sky(f64::from(*x) + 1.0, f64::from(*y) + 1.0);
                    sample_at(&input, ra, dec, false)
                        .is_some_and(|hit| hit.band == Some(support.band))
                })
                .count();
            assert_eq!(support.count, count as u64);
        }
    }

    /// The pre-F1 full-grid scan, kept as the oracle for bounded equality.
    fn full_scan_supports(
        geometry: &ExposureGeometry,
        grid: &TanGrid,
        bands: &[u16],
    ) -> Vec<Support> {
        let mut supports: BTreeMap<u16, Support> = BTreeMap::new();
        for y in 0..grid.width {
            for x in 0..grid.width {
                let (ra, dec) = grid.sky(f64::from(x) + 1.0, f64::from(y) + 1.0);
                let Some((ix, iy, wl, Some(band))) =
                    geometric_sample(&geometry.wcs, &geometry.wavelength, ra, dec)
                else {
                    continue;
                };
                if !bands.contains(&band) {
                    continue;
                }
                let support = supports.entry(band).or_insert_with(|| Support {
                    band,
                    count: 0,
                    grid_bounds: [x, x, y, y],
                    window: PixelWindow {
                        x0: ix - 1,
                        y0: iy - 1,
                        width: 1,
                        height: 1,
                        clipped: false,
                    },
                    wl_min: wl,
                    wl_max: wl,
                });
                let right = (support.window.x0 + support.window.width - 1).max(ix - 1);
                let top = (support.window.y0 + support.window.height - 1).max(iy - 1);
                support.window.x0 = support.window.x0.min(ix - 1);
                support.window.y0 = support.window.y0.min(iy - 1);
                support.window.width = right - support.window.x0 + 1;
                support.window.height = top - support.window.y0 + 1;
                support.count += 1;
                support.grid_bounds[0] = support.grid_bounds[0].min(x);
                support.grid_bounds[1] = support.grid_bounds[1].max(x);
                support.grid_bounds[2] = support.grid_bounds[2].min(y);
                support.grid_bounds[3] = support.grid_bounds[3].max(y);
                support.wl_min = support.wl_min.min(wl);
                support.wl_max = support.wl_max.max(wl);
            }
        }
        supports.into_values().collect()
    }

    fn distorted_geometry() -> ExposureGeometry {
        let angle = 0.4_f64;
        let (sin, cos) = angle.sin_cos();
        let mut a = Poly::new(2);
        a.set(2, 0, 2e-3);
        let mut b = Poly::new(2);
        b.set(0, 2, -2e-3);
        ExposureGeometry {
            wcs: Wcs {
                crpix: [21.0, 21.0],
                crval: [10.0, 20.0],
                cd: [
                    [-PIXEL_SCALE_DEG * cos, -PIXEL_SCALE_DEG * sin],
                    [PIXEL_SCALE_DEG * sin, -PIXEL_SCALE_DEG * cos],
                ],
                sip: Sip {
                    a,
                    b,
                    ap: None,
                    bp: None,
                },
                naxis: [41, 41],
            },
            wavelength: WavelengthMap {
                x: vec![1.0, 41.0],
                y: vec![1.0, 41.0],
                wavelength: vec![0.753, 0.753, 0.79, 0.79],
                bandwidth: vec![0.02; 4],
                nx: 2,
                ny: 2,
            },
            header_bytes: 0,
        }
    }

    #[test]
    fn rectangular_supports_stay_inside_the_grid_height() {
        let geometry = geometry();
        let grid = TanGrid::new_rect(10.0, 20.0, 0.2, 0.1);
        let found = supports(&geometry, &grid, &[0, 2]);
        assert!(!found.is_empty());
        for support in found {
            assert!(support.grid_bounds[1] < grid.width);
            assert!(support.grid_bounds[3] < grid.height);
        }
    }

    #[test]
    fn bounded_detector_scan_matches_the_full_grid_for_every_support() {
        let geometry = distorted_geometry();
        let grid = TanGrid::new(10.0, 20.0, 61.0 * PIXEL_SCALE_DEG);
        for bands in [vec![0], vec![2], vec![0, 2]] {
            assert_eq!(
                supports(&geometry, &grid, &bands),
                full_scan_supports(&geometry, &grid, &bands),
                "bands {bands:?}"
            );
        }
    }

    #[test]
    fn a_thirty_degree_query_scans_only_the_detector_footprint() {
        let mut geometry = geometry();
        geometry.wcs = Wcs {
            crpix: [10.5, 10.5],
            crval: [180.0, 0.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: Sip {
                a: Poly::new(0),
                b: Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [20, 20],
        };
        geometry.wavelength = WavelengthMap {
            x: vec![1.0, 20.0],
            y: vec![1.0, 20.0],
            wavelength: vec![0.753, 0.753, 0.79, 0.79],
            bandwidth: vec![0.02; 4],
            nx: 2,
            ny: 2,
        };
        let grid = TanGrid::new(180.0, 0.0, 30.0);
        let bounds = grid.detector_bounds(&geometry.wcs).expect("on the grid");
        let area = u64::from(bounds[1] - bounds[0] + 1) * u64::from(bounds[3] - bounds[2] + 1);
        assert!(
            area < 1_000,
            "the bounded scan must cover the 20-pixel detector, not the 30-degree grid: {area}"
        );
        let found = supports(&geometry, &grid, &[0, 2]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|support| support.count > 0));
        assert!(found
            .iter()
            .all(|support| support.grid_bounds[1] - support.grid_bounds[0] < 21));
    }

    #[test]
    fn overlapping_detector_bands_use_the_shared_wavelength_classifier() {
        let mut geometry = geometry();
        geometry.wavelength.wavelength = vec![1.102; 4];
        let grid = TanGrid::new(10.0, 20.0, 5.0 * PIXEL_SCALE_DEG);
        let found = supports(&geometry, &grid, &[16, 17]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].band, 16);
        assert_eq!(found[0].count, 25);
    }
    #[test]
    fn search_and_sort_cover_all_rows_before_paging() {
        let row = |band, mjd, id: &str| SnippetRow {
            id: id.into(),
            snippet_ref: id.into(),
            band,
            mjd,
            utc: "2025-01-01T00:00:00Z".into(),
            release: "qr3".into(),
            detector: 1,
            obsid: id.into(),
            version: "v1".into(),
            geometric_samples: 1,
            wl_min_um: 0.75,
            wl_max_um: 0.75,
            target_covered: true,
            quality: serde_json::Value::Null,
        };
        let rows = vec![row(10, 2.0, "b"), row(1, 3.0, "c"), row(1, 1.0, "a")];
        let query = ObservationPageQuery {
            search: "1 QR3".into(),
            sort: Some("time".into()),
            direction: Some("asc".into()),
            offset: 1,
            limit: 1,
            quality: None,
        };
        let (count, page) = page_rows(&rows, &query).unwrap();
        assert_eq!(count, 2);
        assert_eq!(page[0].id, "c");
    }

    #[test]
    fn quality_filter_and_psf_sort_follow_the_header_flags() {
        let row = |id: &str, quality: serde_json::Value| SnippetRow {
            id: id.into(),
            snippet_ref: id.into(),
            band: 1,
            mjd: 1.0,
            utc: "2025-01-01T00:00:00Z".into(),
            release: "dr1".into(),
            detector: 1,
            obsid: id.into(),
            version: "v1".into(),
            geometric_samples: 1,
            wl_min_um: 0.75,
            wl_max_um: 0.75,
            target_covered: true,
            quality,
        };
        let rows = vec![
            row("streaked", serde_json::json!({"dqa_l1": "Pass", "n_streaks": 2, "psf_fwhm": 5.0})),
            row("clean", serde_json::json!({"dqa_l1": "Pass", "dqa_l2": "Pass", "n_streaks": 0, "finast": 0, "psf_fwhm": 9.0})),
            row("unknown", serde_json::Value::Null),
            row("failed", serde_json::json!({"dqa_l2": "Fail", "psf_fwhm": 7.0})),
        ];
        assert_eq!(quality_flags(&rows[0].quality), 1);
        assert_eq!(quality_flags(&rows[3].quality), 1);
        assert_eq!(quality_flags(&rows[2].quality), 0, "unknown fields raise nothing");
        let query = |quality: Option<&str>, direction: &str| ObservationPageQuery {
            search: String::new(),
            sort: Some("psf".into()),
            direction: Some(direction.into()),
            offset: 0,
            limit: 10,
            quality: quality.map(Into::into),
        };
        let ids = |page: Vec<SnippetRow>| page.into_iter().map(|row| row.id).collect::<Vec<_>>();
        let (count, page) = page_rows(&rows, &query(Some("pass"), "asc")).unwrap();
        assert_eq!(count, 2);
        assert_eq!(ids(page), ["clean", "unknown"]);
        let (_, page) = page_rows(&rows, &query(None, "asc")).unwrap();
        assert_eq!(ids(page), ["streaked", "failed", "clean", "unknown"]);
        let (_, page) = page_rows(&rows, &query(None, "desc")).unwrap();
        assert_eq!(ids(page), ["clean", "failed", "streaked", "unknown"]);
        assert!(page_rows(&rows, &query(Some("bogus"), "asc")).is_err());
    }

    #[test]
    fn singleton_preview_subtracts_zodi_and_rejects_other_bands_without_a_merge() {
        use spearv_core::{Cutout, Plane};
        let geometry = geometry();
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
            wcs: geometry.wcs.clone(),
            wavelength: geometry.wavelength.clone(),
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
            bytes_read: 400,
            s3_requests: 4,
        };
        let row = SnippetRow {
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
        let grid = TanGrid::new(10.0, 20.0, 5.0 * PIXEL_SCALE_DEG);
        let support = supports(&geometry, &grid, &[0]).remove(0);
        let expected = support.count;
        let info = crate::types::SnippetPreviewInfo {
            preview_id: "a".into(),
            snippet: row,
            width: 5,
            height: 5,
            tile_size: 256,
            state: "building".into(),
            valid_samples: 0,
            bytes_read: 0,
            requests: 0,
            stretch_low: 0.0,
            stretch_high: 0.0,
            data_tiles: vec![],
        };
        let preview =
            crate::preview::render(cutout, grid, 0, true, support.grid_bounds, info).unwrap();
        assert_eq!(preview.info.valid_samples, expected);
        assert_eq!(preview.info.bytes_read, 400);
        assert_eq!(preview.info.stretch_low, 8.0);
        assert!(preview
            .tiles
            .values()
            .flat_map(|tile| tile.iter())
            .filter(|value| value.is_finite())
            .all(|value| *value == 8.0));
    }
}
