//! What a delta is, for every route that draws one.

use crate::{visit_index, Error, Exposure, Sign, View, VisitSpan};

/// Pins in a delta must name a visit and a sign, and both signs must cover the same bands.
pub fn delta_bands(view: &View) -> Result<Vec<u16>, Error> {
    if view.pins.is_empty() {
        return Err(Error::Message("delta needs a + pin and a - pin".into()));
    }
    let mut plus = Vec::new();
    let mut minus = Vec::new();
    for pin in &view.pins {
        if pin.visit.is_none() || pin.sign.is_none() {
            return Err(Error::Message(format!(
                "pin {}:{} needs a visit and a sign, as 42:5:v1:+",
                pin.center, pin.width
            )));
        }
        let bands = covered(pin.center, pin.width);
        match pin.sign {
            Some(Sign::Plus) => plus.extend(bands),
            Some(Sign::Minus) => minus.extend(bands),
            None => {}
        }
    }
    plus.sort_unstable();
    plus.dedup();
    minus.sort_unstable();
    minus.dedup();
    if plus.is_empty() || minus.is_empty() {
        return Err(Error::Message("delta needs both a + pin and a - pin".into()));
    }
    if plus != minus {
        return Err(Error::Message(
            "delta pins must cover the same bands. Different wavelengths are not motion.".into(),
        ));
    }
    Ok(plus)
}

fn covered(center: u16, width: u16) -> Vec<u16> {
    let width = width.max(1) as i32;
    let start = center as i32 - (width - 1) / 2;
    (0..width)
        .filter_map(|offset| {
            let band = start + offset;
            if (0..102).contains(&band) {
                Some(band as u16)
            } else {
                None
            }
        })
        .collect()
}

pub const NO_DELTA_EXPOSURE: &str =
    "no exposure in this release matches those visits and bands. A delta needs two visits of this field in one release; pick visits it has, or another release.";

/// Plus and minus exposures, by the visits the pins name. Both must have some.
pub fn delta_sides(
    exposures: Vec<Exposure>,
    spans: &[VisitSpan],
    view: &View,
) -> Result<(Vec<Exposure>, Vec<Exposure>), Error> {
    let mut plus_side = Vec::new();
    let mut minus_side = Vec::new();
    for exposure in exposures {
        let Some(visit) = visit_index(exposure.mjd, spans) else {
            continue;
        };
        let plus = view.pins.iter().any(|pin| pin.sign == Some(Sign::Plus) && pin.visit == Some(visit));
        let minus = view.pins.iter().any(|pin| pin.sign == Some(Sign::Minus) && pin.visit == Some(visit));
        if plus {
            plus_side.push(exposure.clone());
        }
        if minus {
            minus_side.push(exposure);
        }
    }
    if plus_side.is_empty() || minus_side.is_empty() {
        return Err(Error::Message(NO_DELTA_EXPOSURE.into()));
    }
    Ok((plus_side, minus_side))
}

/// Display scale from sorted absolute differences.
pub fn delta_scale(sorted_abs: &[f32]) -> f32 {
    if sorted_abs.is_empty() {
        return 1.0;
    }
    let i = ((sorted_abs.len() - 1) as f64 * 0.99).round() as usize;
    sorted_abs[i].max(1e-6)
}

/// A diverging display palette: warm where the plus visit is brighter, cool where
/// the minus visit is. A symmetric grey map made a one-sided excess
/// indistinguishable from noise.
pub fn delta_rgb(sample: f32, scale: f32) -> [u8; 3] {
    let t = (sample / scale).clamp(-1.0, 1.0);
    let a = t.abs().powf(0.7);
    let tone = if t >= 0.0 {
        (12.0 + 240.0 * a, 14.0 + 150.0 * a, 20.0 + 20.0 * a)
    } else {
        (12.0 + 18.0 * a, 14.0 + 120.0 * a, 20.0 + 225.0 * a)
    };
    [tone.0.round() as u8, tone.1.round() as u8, tone.2.round() as u8]
}
