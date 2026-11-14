use crate::{Error, Result, SkyRegion};

/// How the render combines bands. Delta is parsed so shared links stay stable,
/// but this build does not implement it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Composite,
    Slider,
    Range,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sign {
    Plus,
    Minus,
}

/// `center` is a 0-based band index, 0..101. `width` is the number of bands
/// centered on it: width 1 is that band, width 5 is two bands on each side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub center: u16,
    pub width: u16,
    pub visit: Option<u16>,
    pub sign: Option<Sign>,
}

#[derive(Clone, Debug)]
pub struct View {
    pub region: SkyRegion,
    pub mode: Mode,
    pub release: String,
    pub pins: Vec<Pin>,
    pub mask: bool,
}

impl View {
    /// Bands that contribute. Composite uses every standard band.
    pub fn selected_bands(&self) -> Vec<u16> {
        if self.mode == Mode::Composite {
            return (0..102).collect();
        }
        if self.pins.is_empty() {
            return vec![self.default_band()];
        }
        let mut bands = Vec::new();
        for pin in &self.pins {
            let width = match self.mode {
                Mode::Slider => 1,
                _ => pin.width.max(1),
            };
            let start = pin.center as i32 - (width as i32 - 1) / 2;
            for offset in 0..width {
                let band = start + offset as i32;
                if (0..102).contains(&band) {
                    bands.push(band as u16);
                }
            }
        }
        bands.sort_unstable();
        bands.dedup();
        bands
    }

    fn default_band(&self) -> u16 {
        42
    }

    pub fn cache_key(&self) -> String {
        let mut pins = String::new();
        for pin in &self.pins {
            pins.push_str(&format!(
                " pin={}:{}:{}:{}",
                pin.center,
                pin.width,
                pin.visit.map(|v| v.to_string()).unwrap_or_default(),
                match pin.sign {
                    Some(Sign::Plus) => "+",
                    Some(Sign::Minus) => "-",
                    None => "",
                }
            ));
        }
        format!(
            "ra={:.6} dec={:.6} fov_l={:.6} fov_h={:.6} mode={} dr={} mask={}{pins}",
            self.region.ra,
            self.region.dec,
            self.region.fov_l_deg,
            self.region.fov_h_deg,
            match self.mode {
                Mode::Composite => "composite",
                Mode::Slider => "slider",
                Mode::Range => "range",
                Mode::Delta => "delta",
            },
            self.release,
            if self.mask { "on" } else { "off" },
        )
    }
}

/// Query parameters shared by the page URL and the render route.
pub struct ViewQuery {
    pub ra: f64,
    pub dec: f64,
    pub fov_l: f64,
    pub fov_h: f64,
    pub mode: Option<String>,
    pub dr: Option<String>,
    pub pins: Vec<String>,
    pub mask: Option<String>,
}

impl ViewQuery {
    pub fn parse(self, max_fov: f64) -> Result<View> {
        if !self.ra.is_finite()
            || !self.dec.is_finite()
            || !(0.0..360.0).contains(&self.ra)
            || !(-90.0..=90.0).contains(&self.dec)
        {
            return Err(Error::InvalidSky);
        }
        // Missing/non-finite axes fall through to FovOutOfRange, not InvalidSky.
        resolve_fov(self.fov_l, self.fov_h, max_fov)?;
        let mode = match self.mode.as_deref().unwrap_or("composite") {
            "composite" => Mode::Composite,
            "slider" => Mode::Slider,
            "range" => Mode::Range,
            "delta" => Mode::Delta,
            other => return Err(Error::Message(format!("unknown mode {other}"))),
        };
        let mut pins = Vec::new();
        for raw in &self.pins {
            pins.push(parse_pin(raw)?);
        }
        let mask = match self.mask.as_deref().unwrap_or("on") {
            "on" | "1" | "true" => true,
            "off" | "0" | "false" => false,
            other => return Err(Error::Message(format!("unknown mask {other}"))),
        };
        Ok(View {
            region: SkyRegion::rect(self.ra, self.dec, self.fov_l, self.fov_h),
            mode,
            release: self.dr.unwrap_or_else(|| "qr3".into()),
            pins,
            mask,
        })
    }
}


pub fn resolve_fov(fov_l: f64, fov_h: f64, max_fov: f64) -> Result<(f64, f64)> {
    if !fov_l.is_finite() || !fov_h.is_finite() || fov_l <= 0.0 || fov_h <= 0.0 || fov_l > max_fov || fov_h > max_fov {
        return Err(Error::FovOutOfRange { max: max_fov });
    }
    Ok((fov_l, fov_h))
}

/// Split a query string the way the page and the routes share it.
/// `+` is a space, then percent-encoding is decoded, so a pin sign must be `%2B`.
pub fn query_pairs(raw: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for pair in raw.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        pairs.push((percent_decode(key), percent_decode(&value.replace('+', " "))));
    }
    pairs
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_pin(raw: &str) -> Result<Pin> {
    let mut parts = raw.split(':');
    let center: u16 = parts
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| Error::Message(format!("bad pin {raw}")))?;
    let width = parts
        .next()
        .unwrap_or("1")
        .parse()
        .map_err(|_| Error::Message(format!("bad pin {raw}")))?;
    if center >= 102 || width == 0 || width > 102 {
        return Err(Error::Message(format!("pin {raw} is outside bands 0..101")));
    }
    let visit = match parts.next() {
        None | Some("") => None,
        Some(text) => {
            let text = text.strip_prefix('v').unwrap_or(text);
            Some(
                text.parse()
                    .map_err(|_| Error::Message(format!("bad pin visit in {raw}")))?,
            )
        }
    };
    let sign = match parts.next() {
        None | Some("") => None,
        Some("+") => Some(Sign::Plus),
        Some("-") => Some(Sign::Minus),
        Some(_) => return Err(Error::Message(format!("bad pin sign in {raw}"))),
    };
    Ok(Pin {
        center,
        width,
        visit,
        sign,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangular_keys_distinguish_both_axes_and_validation_checks_each() {
        let key = |l, h| View { region: SkyRegion::rect(10.0, 20.0, l, h), mode: Mode::Composite,
            release: "qr3".into(), pins: vec![], mask: true }.cache_key();
        assert_ne!(key(0.2, 0.1), key(0.1, 0.2));
        assert_ne!(key(0.2, 0.1), key(0.2, 0.2));
        assert_eq!(resolve_fov(0.2, 0.1, 30.0).unwrap(), (0.2, 0.1));
        for (l, h) in [(0.0, 0.1), (0.1, 0.0), (30.1, 0.1), (0.1, 30.1), (f64::NAN, 0.1), (0.1, f64::NAN)] {
            assert!(resolve_fov(l, h, 30.0).is_err());
        }
    }

    #[test]
    fn pin_width_is_centered() {
        assert!(crate::pin_covers(42, 5, 40));
        assert!(crate::pin_covers(42, 5, 44));
        assert!(!crate::pin_covers(42, 5, 39));
        assert!(crate::pin_covers(42, 1, 42));
        assert!(!crate::pin_covers(42, 1, 43));
    }

    #[test]
    fn parses_repeated_pins_and_release() {
        let view = ViewQuery {
            ra: 56.75,
            dec: 24.12,
            fov_l: 0.1,
            fov_h: 0.1,
            mode: Some("range".into()),
            dr: Some("qr3".into()),
            pins: vec!["42:5".into(), "10:1:v2:+".into()],
            mask: None,
        }
        .parse(1.0)
        .unwrap();
        assert_eq!(view.mode, Mode::Range);
        assert_eq!(view.release, "qr3");
        assert_eq!(view.pins[0].center, 42);
        assert_eq!(view.pins[0].width, 5);
        assert_eq!(view.pins[1].visit, Some(2));
        assert_eq!(view.pins[1].sign, Some(Sign::Plus));
        assert!(view.selected_bands().contains(&40));
        assert!(view.selected_bands().contains(&10));
        assert!(!view.selected_bands().contains(&39));
    }

    #[test]
    fn rejects_fov_and_bad_pin() {
        let err = ViewQuery {
            ra: 1.0,
            dec: 1.0,
            fov_l: 3.0,
            fov_h: 3.0,
            mode: None,
            dr: None,
            pins: vec![],
            mask: None,
        }
        .parse(1.0)
        .unwrap_err();
        assert!(matches!(err, Error::FovOutOfRange { .. }));
        let err = ViewQuery {
            ra: 1.0,
            dec: 1.0,
            fov_l: 0.1,
            fov_h: 0.1,
            mode: None,
            dr: None,
            pins: vec!["200:1".into()],
            mask: None,
        }
        .parse(1.0)
        .unwrap_err();
        assert!(matches!(err, Error::Message(_)));
    }

    #[test]
    fn plus_sign_survives_percent_encoding() {
        let pairs = query_pairs("pin=42%3A5%3Av1%3A%2B&mode=delta");
        let pin = pairs.iter().find(|pair| pair.0 == "pin").unwrap();
        let parsed = parse_pin(&pin.1).unwrap();
        assert_eq!(parsed.sign, Some(Sign::Plus));
        assert_eq!(parsed.visit, Some(1));
        assert_eq!(parsed.width, 5);
    }
}
