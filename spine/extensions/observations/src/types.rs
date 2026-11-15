use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Every preview, sector build and sequence data tile is 256×256 pixels.
pub const TILE_SIZE: u32 = 256;

/// Widest region the observation extension accepts, in degrees. A 180° field
/// is the largest representable square view (a full hemisphere); it is not a
/// survey or instrument limit.
pub const MAX_FOV_DEG: f64 = 180.0;

/// Grid PNG tile coordinates. A client can request a tile only at these
/// coordinates; any other coordinate is no-data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TileCoordinate {
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ObservationRegion {
    pub ra: f64,
    pub dec: f64,
    /// Square field in degrees. Legacy clients send only this; when `fov_l`
    /// and `fov_h` are present they are the authoritative axes.
    pub fov: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov_l: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov_h: Option<f64>,
}

impl ObservationRegion {
    /// Independent horizontal and vertical axes, falling back to the square
    /// `fov` for callers that predate the rectangular pair.
    pub fn axes(&self) -> (f64, f64) {
        (
            self.fov_l.unwrap_or(self.fov),
            self.fov_h.unwrap_or(self.fov),
        )
    }

    pub fn same_grid(&self, other: &Self) -> bool {
        let (left_l, left_h) = self.axes();
        let (right_l, right_h) = other.axes();
        [self.ra, self.dec, left_l, left_h].map(f64::to_bits)
            == [other.ra, other.dec, right_l, right_h].map(f64::to_bits)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.ra.is_finite()
            || !(0.0..360.0).contains(&self.ra)
            || !self.dec.is_finite()
            || !(-90.0..=90.0).contains(&self.dec)
        {
            return Err("RA must be in [0, 360), Dec in [-90, 90]".into());
        }
        let (fov_l, fov_h) = self.axes();
        for (name, value) in [("FOV-L", fov_l), ("FOV-H", fov_h)] {
            if !value.is_finite() || value <= 0.0 || value > MAX_FOV_DEG {
                return Err(format!(
                    "{name} must be in (0, {MAX_FOV_DEG}] degrees, got {value}"
                ));
            }
        }
        Ok(())
    }
    pub fn grid(&self) -> spearv_core::TanGrid {
        let (fov_l, fov_h) = self.axes();
        spearv_core::TanGrid::new_rect(self.ra, self.dec, fov_l, fov_h)
    }
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct ObservationQuery {
    pub region: ObservationRegion,
    pub from_utc: Option<String>,
    pub to_utc: Option<String>,
    pub releases: Vec<String>,
    /// Semicolon-separated UI bands/ranges, 1–102 inclusive.
    pub bands: String,
    /// Quick-video sampling: resolve at most this many candidate exposures per
    /// visit instead of every exposure. Manual listing leaves it unset.
    #[serde(default)]
    pub per_visit: Option<usize>,
    /// Gap that starts a new visit for `per_visit` sampling. Defaults to 10 days.
    #[serde(default)]
    pub visit_gap_days: Option<f64>,
}

pub fn parse_bands(text: &str) -> Result<Vec<u16>, String> {
    let mut bands = std::collections::BTreeSet::new();
    for part in text.split(';') {
        let pieces: Vec<_> = part.split('-').map(str::trim).collect();
        if pieces.len() > 2
            || pieces
                .iter()
                .any(|piece| piece.is_empty() || !piece.bytes().all(|ch| ch.is_ascii_digit()))
        {
            return Err("Use band integers/ranges separated by semicolons".into());
        }
        let first: u16 = pieces[0].parse().map_err(|_| "Band out of range")?;
        let last: u16 = pieces[pieces.len() - 1]
            .parse()
            .map_err(|_| "Band out of range")?;
        if first < 1 || last > 102 || first > last {
            return Err("Bands must be ascending integers in 1–102".into());
        }
        bands.extend(first..=last);
    }
    Ok(bands.into_iter().map(|band| band - 1).collect())
}

pub fn utc_mjd(text: Option<&str>) -> Result<Option<f64>, String> {
    let Some(text) = text else { return Ok(None) };
    let date = DateTime::parse_from_rfc3339(text).map_err(|_| "Invalid absolute UTC timestamp")?;
    if date.offset().local_minus_utc() != 0 {
        return Err("Timestamp must be UTC".into());
    }
    let seconds = date.timestamp() as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9;
    Ok(Some(40587.0 + seconds / 86400.0))
}

/// Inclusive query bounds use the same millisecond precision as the UTC labels
/// returned to the table. Expanding each edge by half a displayed millisecond
/// makes selecting a row's shown timestamp include that row even when its native
/// MJD falls between milliseconds.
pub fn utc_mjd_range(
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(Option<f64>, Option<f64>), String> {
    const HALF_MILLISECOND_DAYS: f64 = 0.5 / 86_400_000.0;
    let raw_from = utc_mjd(from)?;
    let raw_to = utc_mjd(to)?;
    if raw_from.zip(raw_to).is_some_and(|(start, end)| start > end) {
        return Err("UTC start is after finish".into());
    }
    Ok((
        raw_from.map(|value| value - HALF_MILLISECOND_DAYS),
        raw_to.map(|value| value + HALF_MILLISECOND_DAYS),
    ))
}

pub fn mjd_utc(mjd: f64) -> Result<String, String> {
    DateTime::<Utc>::from_timestamp_millis(((mjd - 40587.0) * 86400000.0).round() as i64)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| "Exposure timestamp is not representable".into())
}

#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct SnippetRow {
    pub id: String,
    pub snippet_ref: String,
    pub band: u16,
    pub mjd: f64,
    pub utc: String,
    pub release: String,
    pub detector: u8,
    pub obsid: String,
    pub version: String,
    pub geometric_samples: u64,
    pub wl_min_um: f64,
    pub wl_max_um: f64,
    /// Discovery-time hint: this detector's footprint reaches the field centre.
    /// The built frame's `target_coverage` is the authoritative check.
    #[serde(default)]
    pub target_covered: bool,
    /// Raw header quality, including unknown/null fields; never inferred clean.
    #[schema(value_type = Object)]
    pub quality: serde_json::Value,
}

#[derive(Clone, Serialize, ToSchema)]
pub struct GeometryFailure {
    pub key: String,
    pub error: String,
}

#[derive(Clone, Serialize, ToSchema)]
pub struct ObservationStatus {
    pub dataset_id: String,
    pub generation: Option<i64>,
    pub state: String,
    pub revision: u64,
    pub candidate_files: usize,
    /// Coverage candidates before quick-video sampling. Equals
    /// `candidate_files` when `sampled` is false.
    pub sampled_from: usize,
    pub sampled: bool,
    pub resolved_files: usize,
    pub failed_files: usize,
    pub geometry_bytes: u64,
    pub geometry_requests: u64,
    pub failures: Vec<GeometryFailure>,
    pub error: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ObservationPageQuery {
    #[serde(default)]
    pub search: String,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "page_limit")]
    pub limit: usize,
    /// `pass` keeps only rows whose header quality raises no flag.
    #[serde(default)]
    pub quality: Option<String>,
}
fn page_limit() -> usize {
    50
}

#[derive(Serialize, ToSchema)]
pub struct ObservationPage {
    pub revision: u64,
    pub state: String,
    pub matches_so_far: usize,
    pub pending_files: usize,
    pub failed_files: usize,
    /// True when this list resolved a quick-video sample, not every coverage file.
    pub sampled: bool,
    pub sampled_from: usize,
    pub rows: Vec<SnippetRow>,
    pub error: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct SnippetPreviewRequest {
    pub region: ObservationRegion,
    pub snippet_ref: String,
    pub mask: bool,
}

#[derive(Clone, Serialize, ToSchema)]
pub struct SnippetPreviewInfo {
    pub preview_id: String,
    pub snippet: SnippetRow,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    pub state: String,
    pub valid_samples: u64,
    pub bytes_read: u64,
    pub requests: u64,
    pub stretch_low: f32,
    pub stretch_high: f32,
    /// Tiles that hold finite preview data, ordered by row then column. Other tiles are no-data.
    pub data_tiles: Vec<TileCoordinate>,
}

#[derive(Serialize, ToSchema)]
pub struct ObservationApiError {
    pub error: String,
}

/// Every requested ref is accounted for: `renewed` when its idle lease was
/// renewed, `missing` when it no longer exists and must be rediscovered.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct RetainResult {
    pub renewed: Vec<String>,
    pub missing: Vec<String>,
}

/// Exact provenance for one admitted member. Array order is submitted sector
/// order, never the canonical execution order.
#[derive(Clone, Serialize, ToSchema)]
pub struct MemberRef {
    pub snippet: SnippetRow,
    pub s3_key: String,
    pub etag: Option<String>,
    pub region: ObservationRegion,
}

/// Aggregate summary for one declared band. This is status convenience only;
/// the authoritative per-pixel accepted counts are the sparse little-endian
/// `u64` cells in the sector's count sidecar.
#[derive(Clone, Serialize, ToSchema)]
pub struct BandContribution {
    /// UI band number, 1–102.
    pub band: u16,
    /// Total accepted measurements in this band, including repeated depth.
    pub samples: u64,
    /// Pixels with at least one accepted measurement.
    pub pixels: u64,
    /// Non-zero tiles stored in the count sidecar for this band.
    pub tiles: u32,
    /// Cropped little-endian `u64` count-cell bytes stored for this band.
    pub bytes: u64,
}

/// One sector as submitted. Membership is the exact snippet refs, not table filters.
#[derive(Clone, Deserialize, ToSchema)]
pub struct SequenceSector {
    pub id: String,
    pub revision: u64,
    pub snippets: Vec<String>,
}

/// A sector build owns one sector recipe. The server validates that every
/// snippet belongs to exactly this sector before reading any data.
#[derive(Deserialize, ToSchema)]
pub struct SectorBuildRequest {
    pub region: ObservationRegion,
    pub sector: SequenceSector,
}

/// Per-sector calibrated result metadata. The frame itself is a per-boot
/// numerical artifact keyed by `fingerprint`, never a PNG or a display mapping.
#[derive(Clone, Serialize, ToSchema)]
pub struct SectorFrameInfo {
    pub id: String,
    pub revision: u64,
    /// queued, building, ready or failed.
    pub state: String,
    pub error: Option<String>,
    pub snippets: usize,
    /// Exact admitted members in submitted order.
    pub members: Vec<MemberRef>,
    /// SHA-256 over the canonical recipe and admitted member identities. Sector
    /// name/ID, editor revision and submitted order are excluded.
    pub fingerprint: String,
    /// UI band numbers, 1–102.
    pub bands: Vec<u16>,
    pub releases: Vec<String>,
    pub mjd_start: f64,
    pub mjd_end: f64,
    pub utc_start: String,
    pub utc_end: String,
    /// Pixels where every selected band has a valid measurement.
    pub complete_pixels: u64,
    /// Fraction of the 7-native-pixel target aperture with a complete pixel.
    pub target_coverage: f64,
    /// `target_coverage` is at least 0.2; the field centre is not a data hole.
    pub target_covered: bool,
    /// Aggregate per-band summary only; the sparse per-pixel counts live in the
    /// sidecar. `None` means the build failed before every band was accumulated.
    pub contributions: Option<Vec<BandContribution>>,
    /// Sparse little-endian `u64` count-sidecar bytes retained for this sector;
    /// `None` when no sidecar is retained.
    pub count_bytes: Option<u64>,
    pub bytes_read: u64,
    pub requests: u64,
    /// Tiles that hold data in this frame, ordered by row then column. Other tiles are no-data.
    pub data_tiles: Vec<TileCoordinate>,
}

/// One sector build handle and its current metadata.
#[derive(Clone, Serialize, ToSchema)]
pub struct SectorBuildInfo {
    pub build_id: String,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    /// queued, building, ready or failed.
    pub state: String,
    pub error: Option<String>,
    pub sector: SectorFrameInfo,
    pub reduction: String,
}

/// Array order is playback order; the server never re-sorts it.
#[derive(Deserialize, ToSchema)]
pub struct SequenceRequest {
    pub region: ObservationRegion,
    pub sectors: Vec<SequenceSector>,
}

#[derive(Clone, Serialize, ToSchema)]
pub struct SequenceInfo {
    pub sequence_id: String,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    /// building, ready or failed. Tiles exist only when ready.
    pub state: String,
    /// Set once one shared mapping is frozen over every frame.
    pub display_revision: Option<u64>,
    pub stretch_low: Option<f32>,
    pub stretch_high: Option<f32>,
    pub sectors: Vec<SectorFrameInfo>,
    pub error: Option<String>,
    pub reduction: String,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SequenceTileQuery {
    pub display_revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bands_are_strict_disjoint_and_one_based() {
        assert_eq!(
            parse_bands("1-3;102; 2 ;66").unwrap(),
            vec![0, 1, 2, 65, 101]
        );
        for text in [
            "", "0", "103", "2-1", "1.2", "1e2", "1;;2", "1-2-3", "1,2", "-1",
        ] {
            assert!(parse_bands(text).is_err(), "{text}");
        }
    }
    #[test]
    fn rectangular_region_keeps_both_axes_and_identity() {
        let rect = ObservationRegion {
            ra: 133.795129,
            dec: -7.245147,
            fov: 0.2,
            fov_l: Some(0.2),
            fov_h: Some(0.1),
        };
        let grid = rect.grid();
        assert_eq!((grid.width, grid.height), (117, 59));
        let other = ObservationRegion {
            fov_l: Some(0.1),
            fov_h: Some(0.2),
            ..rect.clone()
        };
        assert!(!rect.same_grid(&other));
        assert!(rect.same_grid(&rect.clone()));
        let square_axis = ObservationRegion {
            fov_l: Some(0.2),
            fov_h: None,
            ..rect.clone()
        }
        .grid();
        assert_eq!((square_axis.width, square_axis.height), (117, 117));
    }

    #[test]
    fn region_fov_is_bounded_by_a_full_hemisphere() {
        let region = |fov| ObservationRegion {
            ra: 56.75,
            dec: 24.12,
            fov,
            fov_l: None,
            fov_h: None,
        };
        assert!(region(31.0).validate().is_ok());
        assert!(region(MAX_FOV_DEG).validate().is_ok());
        assert!(region(180.1).validate().is_err());
        assert!(region(0.0).validate().is_err());
        assert!(region(f64::NAN).validate().is_err());
    }
    #[test]
    fn utc_is_absolute_and_calendar_validated() {
        assert_eq!(
            utc_mjd(Some("1970-01-01T00:00:00Z")).unwrap(),
            Some(40587.0)
        );
        assert_eq!(mjd_utc(40587.0).unwrap(), "1970-01-01T00:00:00.000Z");
        assert!(utc_mjd(Some("2023-02-29T00:00:00Z")).is_err());
        assert!(utc_mjd(Some("2025-01-01T00:00:00+01:00")).is_err());
    }

    #[test]
    fn displayed_millisecond_is_an_inclusive_filter_bucket() {
        let mjd = 60909.021002613;
        let utc = mjd_utc(mjd).unwrap();
        let (from, to) = utc_mjd_range(Some(&utc), Some(&utc)).unwrap();
        assert!(from.unwrap() <= mjd && mjd <= to.unwrap(), "{utc}");
    }

    #[test]
    fn reversed_raw_utc_bounds_are_rejected_before_expansion() {
        for (from, to) in [
            ("2024-01-01T00:00:00.001Z", "2024-01-01T00:00:00.000Z"),
            ("2024-01-01T00:00:00.002Z", "2024-01-01T00:00:00.000Z"),
        ] {
            let error = utc_mjd_range(Some(from), Some(to)).unwrap_err();
            assert!(error.contains("UTC start is after finish"), "{error}");
        }
    }

    #[test]
    fn equal_and_forward_raw_utc_bounds_remain_inclusive() {
        let (equal_from, equal_to) = utc_mjd_range(
            Some("2024-01-01T00:00:00.000Z"),
            Some("2024-01-01T00:00:00.000Z"),
        )
        .unwrap();
        assert!(equal_from.unwrap() <= equal_to.unwrap());
        let (forward_from, forward_to) = utc_mjd_range(
            Some("2024-01-01T00:00:00.000Z"),
            Some("2024-01-01T00:00:00.001Z"),
        )
        .unwrap();
        assert!(forward_from.unwrap() < forward_to.unwrap());
        let mjd = 60909.021002613;
        let utc = mjd_utc(mjd).unwrap();
        let (from, to) = utc_mjd_range(Some(&utc), Some(&utc)).unwrap();
        assert!(from.unwrap() <= mjd && mjd <= to.unwrap(), "{utc}");
    }

    #[test]
    fn utc_precision_is_preserved_and_bounds_may_be_open_ended() {
        let precise = utc_mjd(Some("2024-01-01T00:00:00.123456Z"))
            .unwrap()
            .unwrap();
        let millisecond = utc_mjd(Some("2024-01-01T00:00:00.123000Z"))
            .unwrap()
            .unwrap();
        let difference_ms = (precise - millisecond) * 86_400_000.0;
        assert!((difference_ms - 0.456).abs() < 1e-3, "{difference_ms}");
        assert_eq!(utc_mjd_range(None, None).unwrap(), (None, None));
        let (from, to) = utc_mjd_range(Some("2024-01-01T00:00:00.000Z"), None).unwrap();
        assert!(from.is_some() && to.is_none());
        let (from, to) = utc_mjd_range(None, Some("2024-01-01T00:00:00.000Z")).unwrap();
        assert!(from.is_none() && to.is_some());
    }
}
