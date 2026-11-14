//! Group a field's exposures into frames of a time series.
//!
//! Measured on the QR2 + QR3 index (2026-11-08): away from the ecliptic poles a
//! field is observed in passes about 45 days long, with gaps of up to 25 days
//! inside a pass and 100-180 days between passes (Barnard's Star, WISE 0855).
//! Near the poles coverage is continuous, with gaps under 25 days for 528 days
//! (north ecliptic pole, R Dra). So a gap longer than `PASS_GAP_DAYS` starts a
//! new pass, and a pass longer than `LONG_PASS_DAYS` is split into 7-day bins
//! aligned to MJD 0, which keep their ids however much data arrives.

use std::collections::BTreeMap;

use crate::api::Pass;

pub const PASS_GAP_DAYS: f64 = 30.0;
pub const LONG_PASS_DAYS: f64 = 60.0;
pub const BIN_DAYS: f64 = 7.0;

/// `samples` are `(mjd, detector)` pairs in any order.
pub fn passes(samples: &[(f64, u8)]) -> Vec<Pass> {
    let mut sorted: Vec<(f64, u8)> = samples.iter().copied().filter(|s| s.0.is_finite()).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut clusters: Vec<Vec<(f64, u8)>> = Vec::new();
    for sample in sorted {
        match clusters.last_mut() {
            Some(cluster) if sample.0 - cluster.last().map_or(sample.0, |s| s.0) <= PASS_GAP_DAYS => {
                cluster.push(sample)
            }
            _ => clusters.push(vec![sample]),
        }
    }
    clusters
        .into_iter()
        .flat_map(|cluster| {
            let span = cluster.last().map_or(0.0, |s| s.0) - cluster.first().map_or(0.0, |s| s.0);
            if span <= LONG_PASS_DAYS {
                let id = format!("p{}", cluster[0].0.floor() as i64);
                return vec![pass(id, &cluster)];
            }
            let mut bins: BTreeMap<i64, Vec<(f64, u8)>> = BTreeMap::new();
            for sample in cluster {
                let start = (sample.0 / BIN_DAYS).floor() as i64 * BIN_DAYS as i64;
                bins.entry(start).or_default().push(sample);
            }
            bins.into_iter().map(|(start, bin)| pass(format!("w{start}"), &bin)).collect()
        })
        .collect()
}

fn pass(id: String, samples: &[(f64, u8)]) -> Pass {
    let mut detectors: Vec<u8> = samples.iter().map(|s| s.1).collect();
    detectors.sort_unstable();
    detectors.dedup();
    Pass {
        id,
        mjd_start: samples.first().map_or(0.0, |s| s.0),
        mjd_end: samples.last().map_or(0.0, |s| s.0),
        exposures: samples.len(),
        detectors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_month_passes_stay_whole_even_with_a_gap_inside() {
        // Two 45-day passes with a 25-day hole each, 160 days apart.
        let samples = [
            (60800.2, 1), (60804.0, 2), (60820.0, 3), (60845.1, 1),
            (61005.0, 4), (61010.0, 5), (61035.5, 6), (61049.9, 1),
        ];
        let out = passes(&samples);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].id, "p60800");
        assert_eq!(out[0].exposures, 4);
        assert_eq!(out[0].detectors, vec![1, 2, 3]);
        assert_eq!(out[1].id, "p61005");
        assert_eq!(out[1].mjd_end, 61049.9);
    }

    #[test]
    fn continuous_coverage_falls_into_weekly_bins_with_stable_ids() {
        let samples: Vec<(f64, u8)> = (0..100).map(|day| (61000.0 + day as f64, 1)).collect();
        let out = passes(&samples);
        assert!(out.iter().all(|p| p.id.starts_with('w')), "{out:?}");
        assert_eq!(out.iter().map(|p| p.exposures).sum::<usize>(), 100);
        // 61000 / 7 = 8714.28…, so the first bin starts at 60998.
        assert_eq!(out[0].id, "w60998");
        // More data later does not move earlier bins.
        let mut more = samples.clone();
        more.extend((100..140).map(|day| (61000.0 + day as f64, 2)));
        let later = passes(&more);
        assert_eq!(later[..out.len() - 1], out[..out.len() - 1]);
    }

    #[test]
    fn order_does_not_matter_and_nan_is_ignored() {
        let out = passes(&[(61010.0, 2), (f64::NAN, 1), (61000.0, 1)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mjd_start, 61000.0);
        assert_eq!(out[0].exposures, 2);
        assert!(passes(&[]).is_empty());
    }
}
