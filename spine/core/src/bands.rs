/// Standard SPHEREx bands from spectral_channels cal-sch-v2-2026-229.
/// Index is `(detector-1)*17 + (subchan-1)`, so 0..101.
pub struct Band {
    pub detector: u8,
    pub subchan: u8,
    pub wl_min_um: f64,
    pub center_um: f64,
    pub wl_max_um: f64,
}

pub const BANDS: [Band; 102] = [
    Band { detector: 1, subchan: 1, wl_min_um: 0.743000, center_um: 0.753000, wl_max_um: 0.762000 },
    Band { detector: 1, subchan: 2, wl_min_um: 0.761000, center_um: 0.771000, wl_max_um: 0.781000 },
    Band { detector: 1, subchan: 3, wl_min_um: 0.780000, center_um: 0.790000, wl_max_um: 0.800000 },
    Band { detector: 1, subchan: 4, wl_min_um: 0.798000, center_um: 0.809000, wl_max_um: 0.819000 },
    Band { detector: 1, subchan: 5, wl_min_um: 0.817000, center_um: 0.828000, wl_max_um: 0.839000 },
    Band { detector: 1, subchan: 6, wl_min_um: 0.837000, center_um: 0.848000, wl_max_um: 0.859000 },
    Band { detector: 1, subchan: 7, wl_min_um: 0.857000, center_um: 0.868000, wl_max_um: 0.879000 },
    Band { detector: 1, subchan: 8, wl_min_um: 0.877000, center_um: 0.889000, wl_max_um: 0.900000 },
    Band { detector: 1, subchan: 9, wl_min_um: 0.898000, center_um: 0.910000, wl_max_um: 0.922000 },
    Band { detector: 1, subchan: 10, wl_min_um: 0.920000, center_um: 0.932000, wl_max_um: 0.944000 },
    Band { detector: 1, subchan: 11, wl_min_um: 0.942000, center_um: 0.955000, wl_max_um: 0.967000 },
    Band { detector: 1, subchan: 12, wl_min_um: 0.964000, center_um: 0.978000, wl_max_um: 0.990000 },
    Band { detector: 1, subchan: 13, wl_min_um: 0.988000, center_um: 1.001000, wl_max_um: 1.014000 },
    Band { detector: 1, subchan: 14, wl_min_um: 1.012000, center_um: 1.025000, wl_max_um: 1.039000 },
    Band { detector: 1, subchan: 15, wl_min_um: 1.036000, center_um: 1.050000, wl_max_um: 1.064000 },
    Band { detector: 1, subchan: 16, wl_min_um: 1.061000, center_um: 1.075000, wl_max_um: 1.090000 },
    Band { detector: 1, subchan: 17, wl_min_um: 1.086000, center_um: 1.102000, wl_max_um: 1.116000 },
    Band { detector: 2, subchan: 1, wl_min_um: 1.099000, center_um: 1.113000, wl_max_um: 1.126000 },
    Band { detector: 2, subchan: 2, wl_min_um: 1.125000, center_um: 1.139000, wl_max_um: 1.153000 },
    Band { detector: 2, subchan: 3, wl_min_um: 1.152000, center_um: 1.167000, wl_max_um: 1.182000 },
    Band { detector: 2, subchan: 4, wl_min_um: 1.180000, center_um: 1.195000, wl_max_um: 1.210000 },
    Band { detector: 2, subchan: 5, wl_min_um: 1.209000, center_um: 1.224000, wl_max_um: 1.240000 },
    Band { detector: 2, subchan: 6, wl_min_um: 1.238000, center_um: 1.254000, wl_max_um: 1.269000 },
    Band { detector: 2, subchan: 7, wl_min_um: 1.267000, center_um: 1.284000, wl_max_um: 1.300000 },
    Band { detector: 2, subchan: 8, wl_min_um: 1.298000, center_um: 1.315000, wl_max_um: 1.331000 },
    Band { detector: 2, subchan: 9, wl_min_um: 1.329000, center_um: 1.346000, wl_max_um: 1.363000 },
    Band { detector: 2, subchan: 10, wl_min_um: 1.360000, center_um: 1.378000, wl_max_um: 1.396000 },
    Band { detector: 2, subchan: 11, wl_min_um: 1.393000, center_um: 1.412000, wl_max_um: 1.430000 },
    Band { detector: 2, subchan: 12, wl_min_um: 1.426000, center_um: 1.446000, wl_max_um: 1.464000 },
    Band { detector: 2, subchan: 13, wl_min_um: 1.461000, center_um: 1.481000, wl_max_um: 1.500000 },
    Band { detector: 2, subchan: 14, wl_min_um: 1.496000, center_um: 1.517000, wl_max_um: 1.537000 },
    Band { detector: 2, subchan: 15, wl_min_um: 1.533000, center_um: 1.554000, wl_max_um: 1.575000 },
    Band { detector: 2, subchan: 16, wl_min_um: 1.570000, center_um: 1.592000, wl_max_um: 1.613000 },
    Band { detector: 2, subchan: 17, wl_min_um: 1.608000, center_um: 1.631000, wl_max_um: 1.651000 },
    Band { detector: 3, subchan: 1, wl_min_um: 1.636000, center_um: 1.656000, wl_max_um: 1.676000 },
    Band { detector: 3, subchan: 2, wl_min_um: 1.675000, center_um: 1.695000, wl_max_um: 1.715000 },
    Band { detector: 3, subchan: 3, wl_min_um: 1.714000, center_um: 1.735000, wl_max_um: 1.756000 },
    Band { detector: 3, subchan: 4, wl_min_um: 1.754000, center_um: 1.775000, wl_max_um: 1.797000 },
    Band { detector: 3, subchan: 5, wl_min_um: 1.795000, center_um: 1.818000, wl_max_um: 1.840000 },
    Band { detector: 3, subchan: 6, wl_min_um: 1.838000, center_um: 1.861000, wl_max_um: 1.884000 },
    Band { detector: 3, subchan: 7, wl_min_um: 1.881000, center_um: 1.905000, wl_max_um: 1.928000 },
    Band { detector: 3, subchan: 8, wl_min_um: 1.925000, center_um: 1.949000, wl_max_um: 1.973000 },
    Band { detector: 3, subchan: 9, wl_min_um: 1.970000, center_um: 1.995000, wl_max_um: 2.019000 },
    Band { detector: 3, subchan: 10, wl_min_um: 2.015000, center_um: 2.041000, wl_max_um: 2.067000 },
    Band { detector: 3, subchan: 11, wl_min_um: 2.062000, center_um: 2.089000, wl_max_um: 2.115000 },
    Band { detector: 3, subchan: 12, wl_min_um: 2.111000, center_um: 2.139000, wl_max_um: 2.166000 },
    Band { detector: 3, subchan: 13, wl_min_um: 2.160000, center_um: 2.189000, wl_max_um: 2.216000 },
    Band { detector: 3, subchan: 14, wl_min_um: 2.211000, center_um: 2.240000, wl_max_um: 2.268000 },
    Band { detector: 3, subchan: 15, wl_min_um: 2.262000, center_um: 2.292000, wl_max_um: 2.321000 },
    Band { detector: 3, subchan: 16, wl_min_um: 2.314000, center_um: 2.345000, wl_max_um: 2.375000 },
    Band { detector: 3, subchan: 17, wl_min_um: 2.367000, center_um: 2.399000, wl_max_um: 2.440000 },
    Band { detector: 4, subchan: 1, wl_min_um: 2.422000, center_um: 2.445000, wl_max_um: 2.474000 },
    Band { detector: 4, subchan: 2, wl_min_um: 2.470000, center_um: 2.506000, wl_max_um: 2.542000 },
    Band { detector: 4, subchan: 3, wl_min_um: 2.538000, center_um: 2.575000, wl_max_um: 2.612000 },
    Band { detector: 4, subchan: 4, wl_min_um: 2.607000, center_um: 2.646000, wl_max_um: 2.684000 },
    Band { detector: 4, subchan: 5, wl_min_um: 2.678000, center_um: 2.719000, wl_max_um: 2.758000 },
    Band { detector: 4, subchan: 6, wl_min_um: 2.752000, center_um: 2.794000, wl_max_um: 2.835000 },
    Band { detector: 4, subchan: 7, wl_min_um: 2.828000, center_um: 2.871000, wl_max_um: 2.914000 },
    Band { detector: 4, subchan: 8, wl_min_um: 2.906000, center_um: 2.951000, wl_max_um: 2.995000 },
    Band { detector: 4, subchan: 9, wl_min_um: 2.985000, center_um: 3.033000, wl_max_um: 3.079000 },
    Band { detector: 4, subchan: 10, wl_min_um: 3.069000, center_um: 3.118000, wl_max_um: 3.164000 },
    Band { detector: 4, subchan: 11, wl_min_um: 3.155000, center_um: 3.204000, wl_max_um: 3.252000 },
    Band { detector: 4, subchan: 12, wl_min_um: 3.242000, center_um: 3.293000, wl_max_um: 3.342000 },
    Band { detector: 4, subchan: 13, wl_min_um: 3.330000, center_um: 3.383000, wl_max_um: 3.434000 },
    Band { detector: 4, subchan: 14, wl_min_um: 3.422000, center_um: 3.476000, wl_max_um: 3.528000 },
    Band { detector: 4, subchan: 15, wl_min_um: 3.514000, center_um: 3.570000, wl_max_um: 3.624000 },
    Band { detector: 4, subchan: 16, wl_min_um: 3.610000, center_um: 3.667000, wl_max_um: 3.723000 },
    Band { detector: 4, subchan: 17, wl_min_um: 3.707000, center_um: 3.768000, wl_max_um: 3.827000 },
    Band { detector: 5, subchan: 1, wl_min_um: 3.814400, center_um: 3.833000, wl_max_um: 3.851400 },
    Band { detector: 5, subchan: 2, wl_min_um: 3.849400, center_um: 3.867000, wl_max_um: 3.885400 },
    Band { detector: 5, subchan: 3, wl_min_um: 3.882400, center_um: 3.901000, wl_max_um: 3.919400 },
    Band { detector: 5, subchan: 4, wl_min_um: 3.916400, center_um: 3.935000, wl_max_um: 3.953400 },
    Band { detector: 5, subchan: 5, wl_min_um: 3.950400, center_um: 3.970000, wl_max_um: 3.988400 },
    Band { detector: 5, subchan: 6, wl_min_um: 3.985400, center_um: 4.005000, wl_max_um: 4.024400 },
    Band { detector: 5, subchan: 7, wl_min_um: 4.020400, center_um: 4.040000, wl_max_um: 4.059400 },
    Band { detector: 5, subchan: 8, wl_min_um: 4.055400, center_um: 4.076000, wl_max_um: 4.095400 },
    Band { detector: 5, subchan: 9, wl_min_um: 4.091400, center_um: 4.112000, wl_max_um: 4.131400 },
    Band { detector: 5, subchan: 10, wl_min_um: 4.126400, center_um: 4.148000, wl_max_um: 4.167400 },
    Band { detector: 5, subchan: 11, wl_min_um: 4.162400, center_um: 4.184000, wl_max_um: 4.203400 },
    Band { detector: 5, subchan: 12, wl_min_um: 4.199400, center_um: 4.220000, wl_max_um: 4.240400 },
    Band { detector: 5, subchan: 13, wl_min_um: 4.235400, center_um: 4.257000, wl_max_um: 4.277400 },
    Band { detector: 5, subchan: 14, wl_min_um: 4.271400, center_um: 4.294000, wl_max_um: 4.313400 },
    Band { detector: 5, subchan: 15, wl_min_um: 4.308400, center_um: 4.331000, wl_max_um: 4.351400 },
    Band { detector: 5, subchan: 16, wl_min_um: 4.345400, center_um: 4.368000, wl_max_um: 4.388400 },
    Band { detector: 5, subchan: 17, wl_min_um: 4.382400, center_um: 4.405000, wl_max_um: 4.426400 },
    Band { detector: 6, subchan: 1, wl_min_um: 4.418400, center_um: 4.435000, wl_max_um: 4.452400 },
    Band { detector: 6, subchan: 2, wl_min_um: 4.450400, center_um: 4.467000, wl_max_um: 4.484400 },
    Band { detector: 6, subchan: 3, wl_min_um: 4.482400, center_um: 4.500000, wl_max_um: 4.517400 },
    Band { detector: 6, subchan: 4, wl_min_um: 4.515400, center_um: 4.533000, wl_max_um: 4.550400 },
    Band { detector: 6, subchan: 5, wl_min_um: 4.548400, center_um: 4.566000, wl_max_um: 4.584400 },
    Band { detector: 6, subchan: 6, wl_min_um: 4.582400, center_um: 4.600000, wl_max_um: 4.618400 },
    Band { detector: 6, subchan: 7, wl_min_um: 4.615400, center_um: 4.635000, wl_max_um: 4.653400 },
    Band { detector: 6, subchan: 8, wl_min_um: 4.649400, center_um: 4.669000, wl_max_um: 4.688400 },
    Band { detector: 6, subchan: 9, wl_min_um: 4.684400, center_um: 4.704000, wl_max_um: 4.723400 },
    Band { detector: 6, subchan: 10, wl_min_um: 4.719400, center_um: 4.739000, wl_max_um: 4.758400 },
    Band { detector: 6, subchan: 11, wl_min_um: 4.753400, center_um: 4.773000, wl_max_um: 4.792400 },
    Band { detector: 6, subchan: 12, wl_min_um: 4.788400, center_um: 4.808000, wl_max_um: 4.827400 },
    Band { detector: 6, subchan: 13, wl_min_um: 4.822400, center_um: 4.843000, wl_max_um: 4.862400 },
    Band { detector: 6, subchan: 14, wl_min_um: 4.857400, center_um: 4.879000, wl_max_um: 4.899400 },
    Band { detector: 6, subchan: 15, wl_min_um: 4.893400, center_um: 4.915000, wl_max_um: 4.936400 },
    Band { detector: 6, subchan: 16, wl_min_um: 4.930400, center_um: 4.951000, wl_max_um: 4.972400 },
    Band { detector: 6, subchan: 17, wl_min_um: 4.966400, center_um: 4.988000, wl_max_um: 5.009400 },
];

/// Band containing this wavelength, if it falls in a standard interval.
/// Overlapping intervals are broken by the nearest band center.
pub fn band_of(wavelength_um: f64) -> Option<u16> {
    if !wavelength_um.is_finite() {
        return None;
    }
    let start = BANDS.partition_point(|band| band.wl_max_um < wavelength_um);
    let mut best: Option<(u16, f64)> = None;
    for (offset, band) in BANDS[start..].iter().enumerate() {
        if wavelength_um < band.wl_min_um {
            break;
        }
        let distance = (wavelength_um - band.center_um).abs();
        if best.is_none_or(|(_, d)| distance < d) {
            best = Some(((start + offset) as u16, distance));
        }
    }
    best.map(|(i, _)| i)
}

pub fn pin_covers(center: u16, width: u16, band: u16) -> bool {
    let width = width.max(1) as i32;
    let start = center as i32 - (width - 1) / 2;
    let end = start + width - 1;
    let band = band as i32;
    band >= start && band <= end && (0..102).contains(&band)
}
