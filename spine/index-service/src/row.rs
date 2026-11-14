//! From a bucket key and the first 64 KiB of the file to one index row.

use spearv_index::{ExposureRow, Quality, Survey, BUCKET};

use crate::bucket::Object;
use crate::fits::{image_header, CardMap};

/// Enough for the primary and IMAGE headers of every Level 2 file. A QR3
/// IMAGE header ends at 28.8 KB.
pub(crate) const HEADER_BYTES: u64 = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedKey {
    pub(crate) release: String,
    pub(crate) period: String,
    pub(crate) version: String,
    pub(crate) detector: u8,
    pub(crate) obsid: String,
}

/// `qrN/level2/<period>/<version>/<detector>/level2_<obsid>D<detector>_spx_<version>.fits`.
/// Anything else in the bucket is not an exposure.
pub(crate) fn parse_key(key: &str) -> Option<ParsedKey> {
    let parts: Vec<&str> = key.split('/').collect();
    let [release, "level2", period, version, detector, file] = parts.as_slice() else {
        return None;
    };
    release_name(release)?;
    let detector: u8 = detector.parse().ok().filter(|d| (1..=6).contains(d))?;
    let stem = file.strip_prefix("level2_")?.strip_suffix(".fits")?;
    let (obsid, tail) = stem.split_once(&format!("D{detector}_spx_"))?;
    if obsid.is_empty() || tail != *version || period.is_empty() {
        return None;
    }
    Some(ParsedKey {
        release: release.to_string(),
        period: period.to_string(),
        version: version.to_string(),
        detector,
        obsid: obsid.to_string(),
    })
}

/// `qr` and a release number, e.g. `qr2` or `qr3`.
pub(crate) fn release_name(name: &str) -> Option<&str> {
    let digits = name.strip_prefix("qr")?;
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(name)
}

/// Full wavelength range in µm of one detector: the shortest and longest edges
/// of its 17 standard bands.
fn detector_range(detector: u8) -> Option<(f64, f64)> {
    match detector {
        1 => Some((0.743, 1.116)),
        2 => Some((1.099, 1.651)),
        3 => Some((1.636, 2.440)),
        4 => Some((2.422, 3.827)),
        5 => Some((3.8144, 4.4264)),
        6 => Some((4.4184, 5.0094)),
        _ => None,
    }
}

pub(crate) fn row_from_header(object: &Object, prefix: &[u8]) -> Result<ExposureRow, String> {
    let parsed = parse_key(&object.key).ok_or_else(|| format!("{} is not an exposure key", object.key))?;
    let (cards, wcs) = image_header(prefix).map_err(|err| format!("{}: {err}", object.key))?;
    if let Some(detector) = cards.integer("DETECTOR") {
        if detector != i64::from(parsed.detector) {
            return Err(format!("{}: header says detector {detector}", object.key));
        }
    }
    let mjd_avg = cards
        .number("MJD-AVG")
        .filter(|mjd| mjd.is_finite())
        .ok_or_else(|| format!("{}: no MJD-AVG", object.key))?;
    let (em_min_um, em_max_um) =
        detector_range(parsed.detector).ok_or_else(|| format!("{}: no band table", object.key))?;
    let sps_flags = text(&cards, "SPS_FLAGS");
    let sc = [cards.number("X_SC"), cards.number("Y_SC"), cards.number("Z_SC")];
    Ok(ExposureRow {
        url: format!("{BUCKET}/{}", object.key),
        key: object.key.clone(),
        etag: object.etag.clone(),
        size: object.size,
        release: parsed.release,
        survey: sps_flags.as_deref().map_or(Survey::AllSky, Survey::from_sps_flags),
        sps_flags,
        period: parsed.period,
        version: parsed.version,
        obsid: parsed.obsid,
        detector: parsed.detector,
        mjd_beg: cards.number("MJD-BEG"),
        mjd_avg,
        em_min_um,
        em_max_um,
        wcs,
        quality: Quality {
            dqa_l1: text(&cards, "L1DQAFLG"),
            dqa_l2: text(&cards, "L2DQAFLG"),
            psf_fwhm: cards.number("PSF_FWHM"),
            finast: cards.integer("FINAST"),
            aurora: flag(&cards, "AURORA_CONT"),
            airglow: flag(&cards, "HE_AIRGLOW_CONT"),
            shuttle_glow: flag(&cards, "SHUTTLE_GLOW_CONT"),
            n_streaks: cards.integer("L2 SATSTRKS"),
            src_frac: cards.number("PXSRCMSK"),
        },
        sc_xyz: match sc {
            [Some(x), Some(y), Some(z)] => Some([x, y, z]),
            _ => None,
        },
    })
}

fn text(cards: &CardMap, key: &str) -> Option<String> {
    cards.get(key).map(|value| value.trim_matches(['\'', ' ']).to_string()).filter(|value| !value.is_empty())
}

fn flag(cards: &CardMap, key: &str) -> Option<bool> {
    match text(cards, key)?.to_ascii_uppercase().as_str() {
        "T" | "TRUE" => Some(true),
        "F" | "FALSE" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fits_prefix, key};

    #[test]
    fn keys_name_release_period_version_detector_and_obsid() {
        let parsed = parse_key(&key("qr3", "2026W40_2A", "l2b_retry-v27-2026-286", 4, 1)).unwrap();
        assert_eq!(
            parsed,
            ParsedKey {
                release: "qr3".into(),
                period: "2026W40_2A".into(),
                version: "l2b_retry-v27-2026-286".into(),
                detector: 4,
                obsid: "2026W40_2A_0001_1".into(),
            }
        );
        assert!(parse_key("qr3/level2/2026W40_2A/v/1/notes.txt").is_none());
        assert!(parse_key(&key("qr3", "P", "v1", 7, 1)).is_none(), "detectors are 1-6");
        assert!(parse_key(&key("qr3", "P", "v1", 1, 1).replace("_spx_v1", "_spx_v2")).is_none(), "version must match its folder");
        assert!(parse_key(&key("dark", "P", "v1", 1, 1)).is_none());
        assert!(parse_key(&key("qr12", "P", "v1", 1, 1)).is_some());
    }

    #[test]
    fn detector_ranges_ascend_and_meet_at_the_edges() {
        let ranges: Vec<(f64, f64)> = (1..=6).filter_map(detector_range).collect();
        assert_eq!(ranges.len(), 6);
        assert!(ranges.iter().all(|(lo, hi)| lo < hi));
        assert!(ranges.windows(2).all(|pair| pair[1].0 < pair[0].1 && pair[0].1 < pair[1].1), "{ranges:?}");
        assert!(detector_range(0).is_none() && detector_range(7).is_none());
    }

    #[test]
    fn a_qr3_header_fills_quality_contamination_and_position() {
        let object = Object { key: key("qr3", "2026W40_2A", "l2b-v27-2026-285", 1, 1), etag: Some("\"x\"".into()), size: Some(65_131_200) };
        let prefix = fits_prefix(1, 232.66, -10.72, &[
            "HIERARCH SPS_FLAGS = 'deep_north' / Flags defined by Survey Planning",
            "L1DQAFLG= 'Pass    '           / L1 DQA outcome",
            "L2DQAFLG= 'Fail    '           / L2 DQA outcome",
            "PSF_FWHM=    7.530770327317724 /  [arcsec] PSF FWHM Median",
            "FINAST  =                    0 / good fine-astrometric solution",
            "HIERARCH AURORA_CONT = T / True if AURORA_CORR > 0.2",
            "HIERARCH HE_AIRGLOW_CONT = F / True for detectors 1 and 2",
            "HIERARCH L2 SATSTRKS = 3 / Number of detected satellite streaks",
            "PXSRCMSK=                77.93 / Percentage of pixels with SOURCE flag",
            "X_SC    =   1293.3687821690996 / [km] Exposure MID x position",
            "Y_SC    =  -5263.1833287560785 / [km] Exposure MID y position",
            "Z_SC    =    4461.523818027163 / [km] Exposure MID z position",
        ]);
        let row = row_from_header(&object, &prefix).unwrap();
        assert_eq!(row.release, "qr3");
        assert_eq!(row.survey, Survey::Deep);
        assert_eq!(row.sps_flags.as_deref(), Some("deep_north"));
        assert_eq!(row.obsid, "2026W40_2A_0001_1");
        assert_eq!(row.mjd_avg, 61314.507248284);
        assert_eq!(row.mjd_beg, Some(61314.506582095));
        assert_eq!((row.em_min_um, row.em_max_um), (0.743, 1.116));
        assert_eq!(row.etag.as_deref(), Some("\"x\""));
        assert_eq!(row.url, format!("{BUCKET}/{}", object.key));
        assert_eq!(row.quality.dqa_l1.as_deref(), Some("Pass"));
        assert_eq!(row.quality.dqa_l2.as_deref(), Some("Fail"));
        assert_eq!(row.quality.psf_fwhm, Some(7.530770327317724));
        assert_eq!(row.quality.finast, Some(0));
        assert_eq!(row.quality.aurora, Some(true));
        assert_eq!(row.quality.airglow, Some(false));
        assert_eq!(row.quality.shuttle_glow, None);
        assert_eq!(row.quality.n_streaks, Some(3));
        assert_eq!(row.quality.src_frac, Some(77.93));
        assert_eq!(row.sc_xyz, Some([1293.3687821690996, -5263.1833287560785, 4461.523818027163]));
        assert_eq!(row.wcs.crval, [232.66, -10.72]);
    }

    #[test]
    fn a_qr2_header_without_contamination_cards_is_all_sky_with_nones() {
        let object = Object { key: key("qr2", "2025W34_2B", "l2b-v20-2025-267", 2, 501), etag: None, size: None };
        let row = row_from_header(&object, &fits_prefix(2, 56.75, 24.12, &["HIERARCH SPS_FLAGS = 'all_sky ' / survey"])).unwrap();
        assert_eq!(row.survey, Survey::AllSky);
        assert_eq!(row.quality.aurora, None);
        assert_eq!(row.quality.n_streaks, None);
        assert_eq!(row.sc_xyz, None);
    }

    #[test]
    fn a_header_that_disagrees_with_its_key_is_refused() {
        let object = Object { key: key("qr3", "2026W40_2A", "l2b-v27-2026-285", 3, 1), etag: None, size: None };
        assert!(row_from_header(&object, &fits_prefix(1, 10.0, 10.0, &[])).unwrap_err().contains("detector 1"));
        assert!(row_from_header(&object, b"not fits").is_err());
    }
}
