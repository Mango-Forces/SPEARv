//! The IMAGE header of a Level 2 file and its sky WCS, read from the first
//! bytes of the file.

use spearv_core::{Poly, Sip, Wcs};

const BLOCK: usize = 2880;
const CARD: usize = 80;

/// Header cards in file order. A repeated key reads as its last value.
#[derive(Clone, Debug, Default)]
pub(crate) struct CardMap {
    pairs: Vec<(String, String)>,
}

impl CardMap {
    /// The value with quotes and trailing comment removed.
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }

    pub(crate) fn number(&self, key: &str) -> Option<f64> {
        self.get(key)?.replace(['D', 'd'], "E").parse().ok()
    }

    pub(crate) fn integer(&self, key: &str) -> Option<i64> {
        self.number(key).map(|value| value as i64)
    }
}

/// The cards and sky WCS of the IMAGE extension that follows the primary HDU.
/// `prefix` must reach the end of that header; 64 KiB does for every Level 2 file.
pub(crate) fn image_header(prefix: &[u8]) -> Result<(CardMap, Wcs), String> {
    let (primary, primary_len) = header_at(prefix, 0)?;
    let (cards, _) = header_at(prefix, primary_len + data_len(&primary)?)?;
    match cards.get("EXTNAME") {
        Some("IMAGE") => {}
        other => return Err(format!("expected IMAGE after the primary header, found {}", other.unwrap_or("no EXTNAME"))),
    }
    let wcs = sky_wcs(&cards)?;
    Ok((cards, wcs))
}

/// The cards of the header that starts at `start`, and its length padded to whole blocks.
fn header_at(bytes: &[u8], start: usize) -> Result<(CardMap, usize), String> {
    let header = bytes.get(start..).filter(|rest| !rest.is_empty()).ok_or("header prefix is short")?;
    let end = header
        .as_chunks::<CARD>()
        .0
        .iter()
        .position(|card| card.starts_with(b"END") && card[3..8].iter().all(|b| *b == b' '))
        .map(|at| (at + 1) * CARD)
        .ok_or("header prefix has no END")?;
    let padded = end.div_ceil(BLOCK) * BLOCK;
    if header.len() < padded {
        return Err("header prefix ends inside the header block".into());
    }
    let pairs = header[..end].as_chunks::<CARD>().0.iter().filter_map(|card| parse_card(card)).collect();
    Ok((CardMap { pairs }, padded))
}

/// Bytes of the HDU's data, padded to whole blocks.
fn data_len(cards: &CardMap) -> Result<usize, String> {
    let axes = cards.integer("NAXIS").unwrap_or(0);
    if axes == 0 {
        return Ok(0);
    }
    let bitpix = cards.integer("BITPIX").ok_or("BITPIX missing")?;
    let pixels = (1..=axes)
        .map(|axis| cards.integer(&format!("NAXIS{axis}")).ok_or_else(|| format!("NAXIS{axis} missing")))
        .product::<Result<i64, String>>()?;
    let groups = cards.integer("GCOUNT").unwrap_or(1).max(1);
    let bytes = bitpix.unsigned_abs() / 8 * groups.unsigned_abs() * (cards.integer("PCOUNT").unwrap_or(0) + pixels).unsigned_abs();
    Ok((bytes as usize).div_ceil(BLOCK) * BLOCK)
}

fn parse_card(card: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(card).ok()?;
    if ["COMMENT", "HISTORY", "END"].iter().any(|skip| text.starts_with(skip)) {
        return None;
    }
    let (key, rest) = match text.strip_prefix("HIERARCH ") {
        Some(rest) => rest.split_once('=')?,
        None => (text.get(..8)?, text.get(8..)?.trim_start().strip_prefix('=')?),
    };
    let key = key.trim();
    (!key.is_empty()).then(|| (key.to_string(), parse_value(rest.trim())))
}

/// A quoted string with `''` unescaped, or the text before a `/` comment.
fn parse_value(rest: &str) -> String {
    let Some(body) = rest.strip_prefix('\'') else {
        return rest.split('/').next().unwrap_or("").trim().to_string();
    };
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' && chars.next_if_eq(&'\'').is_none() {
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// The primary sky WCS: TAN, optionally with SIP, in ICRS, with `LONPOLE` 180.
/// Accepts a `CD` matrix or `PC` with `CDELT`.
fn sky_wcs(cards: &CardMap) -> Result<Wcs, String> {
    let ctype1 = cards.get("CTYPE1").ok_or("CTYPE1 missing")?;
    let ctype2 = cards.get("CTYPE2").ok_or("CTYPE2 missing")?;
    if ctype1.trim_end_matches("-SIP") != "RA---TAN" || ctype2.trim_end_matches("-SIP") != "DEC--TAN" {
        return Err(format!("unsupported projection {ctype1}/{ctype2}; only TAN and TAN-SIP are read"));
    }
    if let Some(lonpole) = cards.number("LONPOLE").filter(|lonpole| (lonpole - 180.0).abs() > 1e-6) {
        return Err(format!("LONPOLE {lonpole} is not 180"));
    }
    if let Some(radesys) = cards.get("RADESYS").filter(|radesys| *radesys != "ICRS") {
        return Err(format!("RADESYS {radesys} is not ICRS"));
    }
    let number = |key: &str, default: f64| cards.number(key).unwrap_or(default);
    let cd = if cards.get("CD1_1").is_some() {
        [[number("CD1_1", 1.0), number("CD1_2", 0.0)], [number("CD2_1", 0.0), number("CD2_2", 1.0)]]
    } else {
        let (cdelt1, cdelt2) = (number("CDELT1", 1.0), number("CDELT2", 1.0));
        [
            [cdelt1 * number("PC1_1", 1.0), cdelt1 * number("PC1_2", 0.0)],
            [cdelt2 * number("PC2_1", 0.0), cdelt2 * number("PC2_2", 1.0)],
        ]
    };
    let a = read_poly(cards, "A")?;
    let b = read_poly(cards, "B")?;
    if ctype1.ends_with("-SIP") && a.order == 0 && b.order == 0 {
        return Err("CTYPE claims SIP but A/B orders are missing".into());
    }
    Ok(Wcs {
        crpix: [number("CRPIX1", 0.0), number("CRPIX2", 0.0)],
        crval: [cards.number("CRVAL1").ok_or("CRVAL1 missing")?, cards.number("CRVAL2").ok_or("CRVAL2 missing")?],
        cd,
        sip: Sip { a, b, ap: read_poly(cards, "AP").ok(), bp: read_poly(cards, "BP").ok() },
        naxis: [cards.integer("NAXIS1").unwrap_or(0) as i32, cards.integer("NAXIS2").unwrap_or(0) as i32],
    })
}

fn read_poly(cards: &CardMap, prefix: &str) -> Result<Poly, String> {
    let order = cards.integer(&format!("{prefix}_ORDER")).ok_or_else(|| format!("{prefix}_ORDER missing"))?;
    let order = usize::try_from(order)
        .ok()
        .filter(|order| *order <= 9)
        .ok_or_else(|| format!("{prefix}_ORDER {order} is out of range"))?;
    let mut poly = Poly::new(order);
    for i in 0..=order {
        for j in 0..=order - i {
            if let Some(value) = cards.number(&format!("{prefix}_{i}_{j}")) {
                poly.set(i, j, value);
            }
        }
    }
    Ok(poly)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{card, fits_prefix, header_prefix};

    /// The IMAGE cards of a header with `wcs` lines in place of the CD matrix and SIP orders.
    fn image(wcs: &[&str]) -> Vec<u8> {
        let base = [
            "XTENSION= 'IMAGE   '",
            "BITPIX  =                  -32",
            "NAXIS   =                    2",
            "NAXIS1  =                 2040",
            "NAXIS2  =                 2040",
            "EXTNAME = 'IMAGE   '",
            "CRPIX1  =               1020.5",
            "CRPIX2  =               1020.5",
            "CRVAL1  =         270.14600000",
            "CRVAL2  =          66.52300000",
        ];
        header_prefix(&base.iter().chain(wcs).map(|line| card(line)).collect::<Vec<_>>())
    }

    #[test]
    fn a_cd_matrix_header_reads_cards_and_wcs() {
        let (cards, wcs) = image_header(&fits_prefix(3, 232.66, -10.72, &["HIERARCH L2 SATSTRKS = 3 / streaks"])).unwrap();
        assert_eq!(cards.integer("DETECTOR"), Some(3));
        assert_eq!(cards.integer("L2 SATSTRKS"), Some(3));
        assert_eq!(wcs.crval, [232.66, -10.72]);
        assert_eq!(wcs.cd, [[-0.001708333, 0.0], [0.0, 0.001708333]]);
        assert_eq!(wcs.naxis, [2040, 2040]);
        assert!(wcs.sip.ap.is_none());
    }

    #[test]
    fn a_pc_and_cdelt_header_with_sip_reads_every_polynomial() {
        let prefix = image(&[
            "CTYPE1  = 'RA---TAN-SIP'",
            "CTYPE2  = 'DEC--TAN-SIP'",
            "RADESYS = 'ICRS    '",
            "LONPOLE =                180.0",
            "CDELT1  =         -0.001708333",
            "CDELT2  =          0.001708333",
            "PC1_1   =                  0.5",
            "PC1_2   =                 0.25",
            "PC2_1   =                -0.25",
            "PC2_2   =                  0.5",
            "A_ORDER =                    2",
            "A_0_2   =              1.5D-06 / Fortran exponent",
            "B_ORDER =                    2",
            "B_1_1   =             -2.5E-07",
            "AP_ORDER=                    1",
            "AP_1_0  =               1.0E-4",
            "BP_ORDER=                    1",
        ]);
        let (_, wcs) = image_header(&prefix).unwrap();
        assert_eq!(wcs.cd, [[-0.001708333 * 0.5, -0.001708333 * 0.25], [0.001708333 * -0.25, 0.001708333 * 0.5]]);
        assert_eq!(wcs.sip.a.order, 2);
        assert_eq!(wcs.sip.a.eval(0.0, 2.0), 1.5e-6 * 4.0);
        assert_eq!(wcs.sip.b.eval(1.0, 1.0), -2.5e-7);
        assert_eq!(wcs.sip.ap.as_ref().map(|ap| ap.eval(1.0, 0.0)), Some(1.0e-4));
        assert_eq!(wcs.sip.bp.as_ref().map(|bp| bp.order), Some(1));
    }

    #[test]
    fn a_header_outside_tan_icrs_or_lonpole_180_is_refused() {
        let orders = ["A_ORDER =                    0", "B_ORDER =                    0"];
        let refused = |lines: &[&str]| {
            let wcs: Vec<&str> = lines.iter().chain(&orders).copied().collect();
            image_header(&image(&wcs)).unwrap_err()
        };
        assert!(refused(&["CTYPE1  = 'RA---SIN'", "CTYPE2  = 'DEC--SIN'"]).contains("projection"));
        assert!(refused(&["CTYPE1  = 'RA---TAN'", "CTYPE2  = 'DEC--TAN'", "RADESYS = 'FK5     '"]).contains("ICRS"));
        assert!(refused(&["CTYPE1  = 'RA---TAN'", "CTYPE2  = 'DEC--TAN'", "LONPOLE =                  0.0"]).contains("LONPOLE"));
        assert!(refused(&["CTYPE1  = 'RA---TAN-SIP'", "CTYPE2  = 'DEC--TAN-SIP'"]).contains("SIP"));
        let no_order = image(&["CTYPE1  = 'RA---TAN'", "CTYPE2  = 'DEC--TAN'"]);
        assert!(image_header(&no_order).unwrap_err().contains("A_ORDER"));
    }

    #[test]
    fn a_short_or_foreign_prefix_is_an_error_not_a_panic() {
        let prefix = fits_prefix(1, 10.0, 10.0, &[]);
        for cut in [0, 79, 2880, 2880 + 80, prefix.len() - 1] {
            assert!(image_header(&prefix[..cut]).is_err(), "cut at {cut}");
        }
        assert!(image_header(b"not fits").is_err());
        let table = String::from_utf8(prefix).unwrap().replace("EXTNAME = 'IMAGE   '", "EXTNAME = 'FLAGS   '");
        assert!(image_header(table.as_bytes()).unwrap_err().contains("FLAGS"));
    }

    #[test]
    fn quoted_values_unescape_and_comments_are_dropped() {
        let parsed = |line: &str| parse_card(card(line).as_bytes());
        assert_eq!(parsed("OBJECT  = 'it''s here' / comment"), Some(("OBJECT".into(), "it's here".into())));
        assert_eq!(parsed("FINAST  =                    0 / good"), Some(("FINAST".into(), "0".into())));
        assert_eq!(parsed("HIERARCH SPS_FLAGS = 'deep_north' / survey"), Some(("SPS_FLAGS".into(), "deep_north".into())));
        assert_eq!(parsed("COMMENT   anything"), None);
        assert_eq!(parsed("        = 'blank key'"), None);
    }
}
