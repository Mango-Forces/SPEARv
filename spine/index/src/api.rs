//! Request and response types of the index-service HTTP API. The service and
//! its client share them, so the two cannot drift apart.

use serde::{Deserialize, Serialize};
use spearv_core::Wcs;

/// The public bucket. Every row's URL is this plus its key.
pub const BUCKET: &str = "https://nasa-irsa-spherex.s3.us-east-1.amazonaws.com";

/// Widest view `/coverage` and `/passes` answer, in degrees. A 180° field is
/// the largest representable square view (a full hemisphere); it is not a
/// survey or instrument limit.
pub const MAX_FOV_DEG: f64 = 180.0;

/// Which survey programme scheduled an exposure, from its `SPS_FLAGS` card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Survey {
    AllSky,
    Deep,
}

impl Survey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllSky => "all_sky",
            Self::Deep => "deep",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all_sky" => Some(Self::AllSky),
            "deep" => Some(Self::Deep),
            _ => None,
        }
    }

    /// `SPS_FLAGS` is a comma-separated list, e.g. `deep_north,downlink`.
    /// `deep_north` and `deep_south` are the deep fields at the ecliptic
    /// poles. Everything else, `all_sky` included, is the all-sky survey.
    pub fn from_sps_flags(flags: &str) -> Self {
        if flags.split(',').any(|flag| flag.trim().to_ascii_lowercase().starts_with("deep")) {
            Self::Deep
        } else {
            Self::AllSky
        }
    }
}

/// `pass` keeps only exposures whose L1 and L2 data-quality checks both passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityFilter {
    Pass,
}

/// Per-exposure quality and contamination, straight from the header. QR2
/// headers have no contamination or satellite-streak cards, so those are
/// `None` there, not `false`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quality {
    /// `L1DQAFLG`, e.g. `Pass`.
    pub dqa_l1: Option<String>,
    /// `L2DQAFLG`, e.g. `Pass`.
    pub dqa_l2: Option<String>,
    /// `PSF_FWHM`, arcseconds.
    pub psf_fwhm: Option<f64>,
    /// `FINAST`; 0 is a good fine-astrometric solution.
    pub finast: Option<i64>,
    /// `AURORA_CONT`.
    pub aurora: Option<bool>,
    /// `HE_AIRGLOW_CONT`.
    pub airglow: Option<bool>,
    /// `SHUTTLE_GLOW_CONT`.
    pub shuttle_glow: Option<bool>,
    /// `L2 SATSTRKS`, detected satellite streaks.
    pub n_streaks: Option<i64>,
    /// `PXSRCMSK`, percent of pixels flagged SOURCE.
    pub src_frac: Option<f64>,
}

impl Quality {
    pub fn passed(&self) -> bool {
        let pass = |flag: &Option<String>| flag.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("pass"));
        pass(&self.dqa_l1) && pass(&self.dqa_l2)
    }
}

/// One file in the bucket: one detector of one observation, in one processing version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExposureRow {
    /// S3 key, unique.
    pub key: String,
    pub url: String,
    pub etag: Option<String>,
    pub size: Option<i64>,
    pub release: String,
    pub survey: Survey,
    /// The raw `SPS_FLAGS` card, e.g. `deep_north`.
    pub sps_flags: Option<String>,
    pub period: String,
    pub version: String,
    pub obsid: String,
    pub detector: u8,
    pub mjd_beg: Option<f64>,
    pub mjd_avg: f64,
    pub em_min_um: f64,
    pub em_max_um: f64,
    pub wcs: Wcs,
    pub quality: Quality,
    /// Geocentric spacecraft position at mid-exposure, km (`X_SC`, `Y_SC`, `Z_SC`).
    pub sc_xyz: Option<[f64; 3]>,
}

/// `GET /coverage`. Every filter is optional; omitted means all.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survey: Option<Survey>,
    /// Earliest `mjd_avg`, inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<f64>,
    /// Latest `mjd_avg`, inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualityFilter>,
    /// Keep only exposures whose band strip can put this wavelength interval
    /// on the view. Both ends or neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wl_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wl_max: Option<f64>,
}

impl CoverageQuery {
    pub fn view(ra: f64, dec: f64, fov: f64) -> Self {
        Self { ra, dec, fov, ..Self::default() }
    }

    /// Reject a query the service cannot answer correctly. The message is the
    /// HTTP 400 body.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.ra.is_finite() && (0.0..360.0).contains(&self.ra)) {
            return Err(format!("ra must be in [0, 360), got {}", self.ra));
        }
        if !(self.dec.is_finite() && (-90.0..=90.0).contains(&self.dec)) {
            return Err(format!("dec must be in [-90, 90], got {}", self.dec));
        }
        if !(self.fov.is_finite() && self.fov > 0.0 && self.fov <= MAX_FOV_DEG) {
            return Err(format!("fov must be in (0, {MAX_FOV_DEG}], got {}", self.fov));
        }
        match (self.wl_min, self.wl_max) {
            (Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() && lo <= hi => {}
            (None, None) => {}
            _ => return Err("wl_min and wl_max go together, with wl_min <= wl_max".into()),
        }
        for (name, value) in [("from", self.from), ("to", self.to)] {
            if value.is_some_and(|v| !v.is_finite()) {
                return Err(format!("{name} must be a finite MJD"));
            }
        }
        Ok(())
    }

    pub fn wavelength(&self) -> Option<(f64, f64)> {
        self.wl_min.zip(self.wl_max)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoverageResponse {
    pub generation: i64,
    pub exposures: Vec<ExposureRow>,
}

/// One frame of a field's time series: an observing pass, or a 7-day bin
/// where coverage is near-continuous (the deep fields at the ecliptic poles).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pass {
    /// Stable across requests: `p<MJD>` for a pass that starts on that day,
    /// `w<MJD>` for the 7-day bin that starts on that day.
    pub id: String,
    pub mjd_start: f64,
    pub mjd_end: f64,
    pub exposures: usize,
    pub detectors: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassesResponse {
    pub generation: i64,
    pub passes: Vec<Pass>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSummary {
    pub release: String,
    /// Indexed files, every processing version included.
    pub exposures: i64,
    pub all_sky: i64,
    pub deep: i64,
    /// Newest planning period in the bucket listing, which can be ahead of the rows.
    pub newest_period: Option<String>,
    pub oldest_mjd: Option<f64>,
    pub newest_mjd: Option<f64>,
    /// Unix epoch ms when the crawler last listed this release successfully.
    pub last_listed_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReleasesResponse {
    pub generation: i64,
    pub releases: Vec<ReleaseSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlKind {
    Full,
    Diff,
}

impl CrawlKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Diff => "diff",
        }
    }
}

/// What one crawl did. Written when it starts and again when it ends, so a
/// crawl that died shows `finished_ms: null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrawlRecord {
    pub id: i64,
    pub kind: CrawlKind,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    /// Exposure keys in successful listings.
    pub listed: i64,
    pub added: i64,
    /// Keys whose ETag changed and whose header was read again.
    pub changed: i64,
    /// Rows deleted after a HEAD returned 404.
    pub removed: i64,
    /// Missing from a listing, but a HEAD found them. Kept.
    pub kept_after_head: i64,
    /// A HEAD neither found nor ruled out the object. Kept; checked next crawl.
    pub head_errors: i64,
    /// Headers that could not be read or parsed. Retried next crawl.
    pub header_failures: i64,
    /// Listings that failed. Rows under them were not deletion candidates.
    pub prefixes_failed: i64,
    /// Set when the crawl stopped early, e.g. the bucket root could not be listed.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// `ok`, `crawling`, or `degraded` when the last crawl stopped with an error
    /// or failed listings.
    pub status: String,
    pub generation: i64,
    pub exposures: i64,
    /// The crawl in progress, with its running counts.
    pub current: Option<CrawlRecord>,
    pub last_crawl: Option<CrawlRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationResponse {
    pub generation: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_flags_are_the_deep_survey() {
        assert_eq!(Survey::from_sps_flags("deep_north"), Survey::Deep);
        assert_eq!(Survey::from_sps_flags(" DEEP_south "), Survey::Deep);
        assert_eq!(Survey::from_sps_flags("all_sky "), Survey::AllSky);
        assert_eq!(Survey::from_sps_flags(""), Survey::AllSky);
        // Seen in QR2 headers.
        assert_eq!(Survey::from_sps_flags("deep_south,downlink"), Survey::Deep);
        assert_eq!(Survey::from_sps_flags("downlink,deep_north"), Survey::Deep);
        assert_eq!(Survey::from_sps_flags("all_sky,downlink"), Survey::AllSky);
    }

    #[test]
    fn a_query_outside_the_sky_is_refused() {
        assert!(CoverageQuery::view(10.0, 20.0, 1.0).validate().is_ok());
        assert!(CoverageQuery::view(360.0, 0.0, 1.0).validate().is_err());
        assert!(CoverageQuery::view(10.0, 91.0, 1.0).validate().is_err());
        assert!(CoverageQuery::view(10.0, 0.0, 0.0).validate().is_err());
        assert!(CoverageQuery::view(10.0, 0.0, 31.0).validate().is_ok());
        assert!(CoverageQuery::view(10.0, 0.0, 180.0).validate().is_ok());
        assert!(CoverageQuery::view(10.0, 0.0, 180.1).validate().is_err());
        assert!(CoverageQuery::view(f64::NAN, 0.0, 1.0).validate().is_err());
        let half = CoverageQuery { wl_min: Some(1.0), ..CoverageQuery::view(1.0, 1.0, 1.0) };
        assert!(half.validate().is_err());
        let both = CoverageQuery { wl_min: Some(1.0), wl_max: Some(1.2), ..half };
        assert!(both.validate().is_ok());
    }

    #[test]
    fn wide_fields_keep_the_coverage_ranges_and_overlap_exact() {
        assert_eq!(crate::view_ra_ranges(56.75, 24.12, 31.0).len(), 1);
        assert_eq!(crate::view_ra_ranges(56.75, 24.12, 180.0), vec![(0.0, 360.0)]);
        let wcs = crate::testing::detector(56.75, 24.12);
        assert!(crate::overlaps(&wcs, 56.75, 24.12, 31.0));
        assert!(!crate::overlaps(&wcs, 150.0, 24.12, 31.0));
        // A full-hemisphere query still stops at its tangent plane.
        assert!(crate::overlaps(&wcs, 56.75, 24.12, 180.0));
        assert!(crate::overlaps(&crate::testing::detector(57.75, 24.12), 56.75, 24.12, 180.0));
        for (ra, dec) in [(206.75, 24.12), (236.75, 24.12), (146.75, 0.0)] {
            assert!(
                !crate::overlaps(&crate::testing::detector(ra, dec), 56.75, 24.12, 180.0),
                "({ra}, {dec}) must be behind the view's tangent plane"
            );
        }
        assert!(!crate::overlaps(
            &crate::testing::detector(236.75, -24.12),
            56.75,
            24.12,
            180.0
        ));
    }

    #[test]
    fn quality_passes_only_when_both_checks_pass() {
        let mut quality = Quality { dqa_l1: Some("Pass".into()), dqa_l2: Some("Pass".into()), ..Quality::default() };
        assert!(quality.passed());
        quality.dqa_l2 = Some("Fail".into());
        assert!(!quality.passed());
        quality.dqa_l2 = None;
        assert!(!quality.passed());
    }
}
