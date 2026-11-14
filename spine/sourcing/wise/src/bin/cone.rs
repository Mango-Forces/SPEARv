//! Reads one cone from the real bucket and reports its cost.
//!
//! `spearv-wise-cone RA DEC FOV_DEG [output.csv]`

use std::fmt::Write;
use std::time::Instant;

use spearv_core::SkyRegion;
use spearv_wise::AllWise;

#[tokio::main]
async fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 3 {
        eprintln!("usage: spearv-wise-cone <ra> <dec> <fov_deg> [output.csv]");
        std::process::exit(2);
    }
    let number = |index: usize| args[index].parse::<f64>().expect("a number");
    let region = SkyRegion::square(number(0), number(1), number(2));
    let source = AllWise::from_env().expect("open");
    let started = Instant::now();
    let result = source.cone_rows(&region).await.expect("cone");
    println!(
        "rows={} ms={} {:?}",
        result.rows.len(),
        started.elapsed().as_millis(),
        result.stats
    );
    let Some(path) = args.get(3) else {
        return;
    };
    let number = |value: Option<f64>| value.map(|value| value.to_string()).unwrap_or_default();
    let mut csv = String::from("designation,ra,dec,w1mpro,w2mpro,cc_flags\n");
    for row in &result.rows {
        writeln!(
            csv,
            "{},{},{},{},{},{},{},{}",
            row.designation,
            row.ra,
            row.dec,
            number(row.pmra),
            number(row.pmdec),
            number(row.w1mpro),
            number(row.w2mpro),
            row.cc_flags.as_deref().unwrap_or("")
        )
        .expect("format");
    }
    std::fs::write(path, csv).expect("write csv");
}
