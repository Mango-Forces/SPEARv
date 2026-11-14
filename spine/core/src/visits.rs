/// A visit is a run of exposures closer than 20 days. SPHEREx visits are about
/// that long; a larger gap is a later pass.
pub const VISIT_GAP_DAYS: f64 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisitSpan {
    pub index: u16,
    pub mjd: f64,
}

pub fn cluster_visits(mjds: &[f64]) -> Vec<VisitSpan> {
    let mut sorted: Vec<f64> = mjds.iter().copied().filter(|mjd| mjd.is_finite()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    let mut visits: Vec<VisitSpan> = Vec::new();
    for mjd in sorted {
        if let Some(last) = visits.last() {
            if mjd - last.mjd < VISIT_GAP_DAYS {
                continue;
            }
        }
        visits.push(VisitSpan {
            index: visits.len() as u16,
            mjd,
        });
    }
    visits
}

pub fn visit_index(mjd: f64, visits: &[VisitSpan]) -> Option<u16> {
    visits.iter().rev().find(|visit| mjd + 1e-6 >= visit.mjd && mjd - visit.mjd < VISIT_GAP_DAYS).map(|visit| visit.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_twenty_day_gap_starts_a_new_visit() {
        let visits = cluster_visits(&[61000.0, 61005.0, 61030.0, 61031.0]);
        assert_eq!(visits.len(), 2);
        assert_eq!(visit_index(61005.0, &visits), Some(0));
        assert_eq!(visit_index(61030.0, &visits), Some(1));
        assert_eq!(visit_index(60900.0, &visits), None);
    }
}
