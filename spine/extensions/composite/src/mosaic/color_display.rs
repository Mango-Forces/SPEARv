//! Pure display settings and tone. Nothing here reads or changes science state.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::super::Stretch;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ColorDisplay {
    pub r: Layer,
    pub g: Layer,
    pub b: Layer,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub enabled: bool,
    pub tint: String,
    pub gain: f64,
    pub black_white: BlackWhite,
    pub stretch: ColorStretch,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlackWhite {
    Auto {},
    Manual { low: f64, high: f64 },
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ColorStretch {
    Linear {},
    Asinh {},
    Gamma { gamma: f64 },
}

impl ColorDisplay {
    pub(super) fn normalize(mut self) -> Result<Self, String> {
        for layer in [&mut self.r, &mut self.g, &mut self.b] {
            if layer.tint.len() != 7
                || !layer.tint.starts_with('#')
                || !layer.tint.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
            {
                return Err("tint must be #rrggbb".into());
            }
            layer.tint.make_ascii_lowercase();
            if !layer.gain.is_finite() {
                return Err("gain must be finite".into());
            }
            layer.gain = layer.gain.clamp(0.0, 2.0) + 0.0;
            if let ColorStretch::Gamma { gamma } = &mut layer.stretch {
                if !gamma.is_finite() {
                    return Err("gamma must be finite".into());
                }
                *gamma = gamma.clamp(0.2, 3.0);
            }
            if let BlackWhite::Manual { low, high } = &mut layer.black_white {
                let span = (*high - *low) as f32;
                if !low.is_finite()
                    || !high.is_finite()
                    || *high <= *low
                    || !(*low as f32).is_finite()
                    || !(*high as f32).is_finite()
                    || !span.is_finite()
                    || span <= 0.0
                {
                    return Err("manual black/white points need a finite positive f32 span".into());
                }
                *low += 0.0;
                *high += 0.0;
            }
        }
        Ok(self)
    }

    /// Frozen default spans are used directly, never recomputed from white-black.
    pub(super) fn resolve(&self, stretches: [Option<Stretch>; 3]) -> ResolvedColor {
        ResolvedColor(std::array::from_fn(|index| {
            let layer = [&self.b, &self.g, &self.r][index];
            ResolvedLayer {
                enabled: layer.enabled,
                tint: [1, 3, 5].map(|at| {
                    u8::from_str_radix(&layer.tint[at..at + 2], 16).expect("normalized tint") as f32
                        / 255.0
                }),
                gain: layer.gain as f32,
                stretch: match layer.black_white {
                    BlackWhite::Auto {} => stretches[index],
                    BlackWhite::Manual { low, high } => Some(Stretch {
                        low: low as f32,
                        span: (high - low) as f32,
                    }),
                },
                tone: layer.stretch.clone(),
            }
        }))
    }
}

pub(super) struct ResolvedColor([ResolvedLayer; 3]);

struct ResolvedLayer {
    enabled: bool,
    tint: [f32; 3],
    gain: f32,
    stretch: Option<Stretch>,
    tone: ColorStretch,
}

impl ResolvedColor {
    pub(super) fn pixel(&self, values: [Option<f32>; 3]) -> [u8; 3] {
        let mut rgb = [0.0_f32; 3];
        for (layer, value) in self.0.iter().zip(values) {
            if !layer.enabled {
                continue;
            }
            let (Some(value), Some(stretch)) = (value.filter(|v| v.is_finite()), layer.stretch)
            else {
                continue;
            };
            let t = ((value - stretch.low) / stretch.span).clamp(0.0, 1.0);
            let intensity = match layer.tone {
                ColorStretch::Linear {} => t,
                ColorStretch::Asinh {} => (t / 0.1).asinh() / 10.0_f32.asinh(),
                ColorStretch::Gamma { gamma } => t.powf(gamma as f32),
            } * layer.gain;
            for (gun, tint) in rgb.iter_mut().zip(layer.tint) {
                *gun += intensity * tint;
            }
        }
        rgb.map(|gun| (gun.clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

#[cfg(test)]
pub(super) fn defaults() -> ColorDisplay {
    let layer = |tint: &str| Layer {
        enabled: true,
        tint: tint.into(),
        gain: 1.0,
        black_white: BlackWhite::Auto {},
        stretch: ColorStretch::Gamma { gamma: 0.7 },
    };
    ColorDisplay {
        r: layer("#ff0000"),
        g: layer("#00ff00"),
        b: layer("#0000ff"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tone;

    #[test]
    fn normalization_and_strict_wire_boundary() {
        let mut value = defaults();
        value.r.tint = "#AB00EF".into();
        value.r.gain = -0.0;
        value.g.gain = 99.0;
        value.b.stretch = ColorStretch::Gamma { gamma: -1.0 };
        let value = value.normalize().unwrap();
        assert_eq!(value.r.tint, "#ab00ef");
        assert_eq!(value.r.gain.to_bits(), 0.0_f64.to_bits());
        assert_eq!(value.g.gain, 2.0);
        assert_eq!(value.b.stretch, ColorStretch::Gamma { gamma: 0.2 });
        let json = serde_json::to_value(defaults()).unwrap();
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"r":{},"g":{},"b":{}}),
        ] {
            assert!(serde_json::from_value::<ColorDisplay>(bad).is_err());
        }
        for path in ["root", "layer", "bw", "stretch"] {
            let mut bad = json.clone();
            let object = match path {
                "root" => &mut bad,
                "layer" => &mut bad["r"],
                "bw" => &mut bad["r"]["black_white"],
                _ => &mut bad["r"]["stretch"],
            };
            object["extra"] = true.into();
            assert!(
                serde_json::from_value::<ColorDisplay>(bad).is_err(),
                "{path}"
            );
        }
        for (low, high) in [
            (1.0, 1.0),
            (2.0, 1.0),
            (0.0, f64::INFINITY),
            (0.0, 1e100),
            (0.0, 1e-100),
            (f64::NAN, 1.0),
        ] {
            let mut bad = defaults();
            bad.r.black_white = BlackWhite::Manual { low, high };
            assert!(bad.normalize().is_err());
        }
        for bad in ["ff0000", "#123", "#xx0000", "#é0000"] {
            let mut value = defaults();
            value.r.tint = bad.into();
            assert!(value.normalize().is_err());
        }
        let mut bad = defaults();
        bad.r.gain = f64::NAN;
        assert!(bad.normalize().is_err());
        let mut bad = defaults();
        bad.r.stretch = ColorStretch::Gamma {
            gamma: f64::INFINITY,
        };
        assert!(bad.normalize().is_err());
    }

    #[test]
    fn default_exact_server_tone_and_black_holes() {
        let stretch = Stretch {
            low: -3.25,
            span: 12.75,
        };
        let resolved = defaults().resolve([Some(stretch); 3]);
        for value in (0..1024).map(|i| i as f32 / 23.0 - 8.0) {
            assert_eq!(resolved.pixel([Some(value); 3]), [tone(value, stretch); 3]);
        }
        assert_eq!(resolved.pixel([None; 3]), [0; 3]);
        assert_eq!(resolved.pixel([Some(f32::NAN), None, None]), [0; 3]);
        assert_eq!(
            resolved.pixel([Some(0.0), None, None]),
            [0, 0, tone(0.0, stretch)]
        );
    }

    #[test]
    fn edited_manual_tint_gain_and_all_tones() {
        let mut settings = defaults();
        settings.b.enabled = false;
        settings.g.gain = 0.0;
        settings.r.tint = "#80ff00".into();
        settings.r.gain = 2.0;
        settings.r.black_white = BlackWhite::Manual {
            low: 10.0,
            high: 20.0,
        };
        for (tone, expected) in [
            (ColorStretch::Linear {}, [64, 128, 0]),
            (ColorStretch::Gamma { gamma: 2.0 }, [16, 32, 0]),
            (ColorStretch::Asinh {}, [141, 255, 0]),
        ] {
            settings.r.stretch = tone;
            assert_eq!(
                settings
                    .resolve([None; 3])
                    .pixel([Some(100.0), Some(100.0), Some(12.5)]),
                expected
            );
        }
        settings.r.stretch = ColorStretch::Linear {};
        assert_eq!(
            settings
                .resolve([None; 3])
                .pixel([None, None, Some(1000.0)]),
            [255, 255, 0]
        );
        assert_eq!(
            settings.resolve([None; 3]).pixel([None, None, Some(-10.0)]),
            [0; 3]
        );
    }
}
