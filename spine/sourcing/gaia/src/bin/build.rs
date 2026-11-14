//! Build the trimmed Gaia store from the ESA CDN. Offline: the server never runs this.
//!
//!     spearv-gaia-build [--out data/gaia.sqlite] [--radius 3.0] [--plan]
//!
//! Each file is streamed through gzip, filtered to the showcase fields, and
//! dropped. Nothing is written to disk but the store. `--plan` lists the files
//! and stops. A rerun skips files already in the store, so it can resume.

use std::collections::BTreeSet;
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flate2::write::MultiGzDecoder;
use md5::{Digest, Md5};
use rusqlite::{params, Connection};
use spearv_gaia::motion::{separation_deg, GAIA_DR3_REF_EPOCH};
use spearv_gaia::plan::{
    cell_of_sky, cell_of_source, cone_cells, files_for_cells, parse_manifest, CdnFile, Columns, Field, LineSplitter,
    CELL_MARGIN_DEG, FIELDS,
};
use spearv_gaia::{record_max_pm, SCHEMA};

const CDN: &str = "https://cdn.gea.esac.esa.int/Gaia/gdr3/gaia_source/";
const ATTEMPTS: u32 = 4;

type Outcome<T> = Result<T, Box<dyn Error>>;

struct Options {
    out: PathBuf,
    radius: f64,
    plan_only: bool,
}

#[derive(Default)]
struct Tally {
    bytes: u64,
    seen: u64,
    kept: u64,
    /// Kept rows whose published position lands in another cell than source_id says.
    cell_mismatch: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Outcome<()> {
    let options = parse_args()?;
    // The CDN rotates edge hosts and some refuse connections, so fail fast and retry.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .build()?;
    let manifest = fetch_manifest(&client).await?;
    let files = parse_manifest(&manifest);
    println!("manifest: {} data files", files.len());

    let plans: Vec<(Field, BTreeSet<u32>)> = FIELDS
        .iter()
        .map(|field| (*field, cone_cells(field.ra, field.dec, options.radius + CELL_MARGIN_DEG)))
        .collect();
    let cells: BTreeSet<u32> = plans.iter().flat_map(|plan| plan.1.iter().copied()).collect();
    for (field, field_cells) in &plans {
        let names: Vec<&str> = files_for_cells(&files, field_cells).iter().map(|file| file.name.as_str()).collect();
        println!("{}: {} cells, {} files: {}", field.name, field_cells.len(), names.len(), names.join(" "));
    }
    let wanted = files_for_cells(&files, &cells);
    println!("total: {} distinct files for radius {} degrees", wanted.len(), options.radius);
    if options.plan_only {
        return Ok(());
    }

    if let Some(parent) = options.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(&options.out)?;
    conn.execute_batch(SCHEMA)?;
    require_same_radius(&conn, options.radius, &options.out)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let started = Instant::now();
    for (position, file) in wanted.iter().enumerate() {
        let done: i64 = conn.query_row("SELECT COUNT(*) FROM ingested WHERE file = ?1", [&file.name], |row| row.get(0))?;
        if done > 0 {
            println!("[{}/{}] {} already in the store", position + 1, wanted.len(), file.name);
            continue;
        }
        let tally = ingest_with_retries(&client, &mut conn, file, &cells, options.radius).await?;
        println!(
            "[{}/{}] {} {:.1} MB, {} rows seen, {} kept, {} cell mismatches, {:.0}s so far",
            position + 1,
            wanted.len(),
            file.name,
            tally.bytes as f64 / 1e6,
            tally.seen,
            tally.kept,
            tally.cell_mismatch,
            started.elapsed().as_secs_f64(),
        );
    }

    record_max_pm(&conn)?;
    // A field goes in only now, so the store never claims a cone it only partly holds.
    for field in FIELDS {
        conn.execute(
            "INSERT OR REPLACE INTO field VALUES (?1, ?2, ?3, ?4)",
            params![field.name, field.ra, field.dec, options.radius],
        )?;
    }
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.execute_batch("VACUUM")?;
    report(&conn, &options)
}

/// A resumed build skips finished files, that were already filtered at a smaller radius.
fn require_same_radius(conn: &Connection, radius: f64, out: &Path) -> Outcome<()> {
    conn.execute("INSERT OR IGNORE INTO meta VALUES ('radius_deg', ?1)", [radius])?;
    let stored: f64 = conn.query_row("SELECT value FROM meta WHERE key = 'radius_deg'", [], |row| row.get(0))?;
    if stored != radius {
        return Err(format!("{} was built at radius {stored}. Delete it to use {radius}", out.display()).into());
    }
    Ok(())
}

fn parse_args() -> Outcome<Options> {
    let mut options = Options { out: "data/gaia.sqlite".into(), radius: 3.0, plan_only: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => options.out = args.next().ok_or("--out needs a path")?.into(),
            "--radius" => options.radius = args.next().ok_or("--radius needs degrees")?.parse()?,
            "--plan" => options.plan_only = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(options)
}

async fn fetch_manifest(client: &reqwest::Client) -> Outcome<String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        let response = client.get(format!("{CDN}_MD5SUM.txt")).timeout(Duration::from_secs(60)).send().await;
        match response.and_then(|response| response.error_for_status()) {
            Ok(response) => return Ok(response.text().await?),
            Err(err) => {
                eprintln!("manifest attempt {attempt}: {err}");
                last = err.to_string();
            }
        }
    }
    Err(format!("manifest failed: {last}").into())
}

async fn ingest_with_retries(
    client: &reqwest::Client,
    conn: &mut Connection,
    file: &CdnFile,
    cells: &BTreeSet<u32>,
    radius: f64,
) -> Outcome<Tally> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match ingest(client, conn, file, cells, radius).await {
            Ok(tally) => return Ok(tally),
            Err(err) => {
                eprintln!("{} attempt {attempt}: {err}", file.name);
                last = err.to_string();
            }
        }
    }
    Err(format!("{} failed after {ATTEMPTS} attempts: {last}", file.name).into())
}

/// One transaction per file, so a broken stream or a bad checksum leaves no rows.
async fn ingest(
    client: &reqwest::Client,
    conn: &mut Connection,
    file: &CdnFile,
    cells: &BTreeSet<u32>,
    radius: f64,
) -> Outcome<Tally> {
    let tx = conn.transaction()?;
    let mut tally = Tally::default();
    {
        let mut insert_star = tx.prepare("INSERT INTO star VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
        let mut insert_sky = tx.prepare("INSERT INTO star_sky VALUES (?1, ?2, ?2, ?3, ?3)")?;
        let mut columns: Option<Columns> = None;
        let (mut seen, mut kept, mut cell_mismatch) = (0_u64, 0_u64, 0_u64);
        let mut decoder = MultiGzDecoder::new(LineSplitter::new(|line: &str| {
            if line.starts_with('#') || line.is_empty() {
                return Ok(());
            }
            let Some(columns) = columns else {
                columns = Some(Columns::from_header(line).map_err(invalid)?);
                return Ok(());
            };
            seen += 1;
            let id = columns.source_id(line).ok_or_else(|| invalid(format!("bad row: {line:.80}")))?;
            if !cells.contains(&cell_of_source(id)) {
                return Ok(());
            }
            let star = columns.star(line).ok_or_else(|| invalid(format!("bad row: {line:.80}")))?;
            if !FIELDS.iter().any(|field| separation_deg(field.ra, field.dec, star.ra, star.dec) <= radius) {
                return Ok(());
            }
            // The reader's proper-motion reach assumes one reference epoch.
            if star.ref_epoch != GAIA_DR3_REF_EPOCH {
                return Err(invalid(format!("source {id} has ref_epoch {}", star.ref_epoch)));
            }
            cell_mismatch += u64::from(cell_of_sky(star.ra, star.dec) != cell_of_source(id));
            insert_star
                .execute(params![id, star.ra, star.dec, star.pmra, star.pmdec, star.g, star.ref_epoch])
                .map_err(invalid)?;
            insert_sky.execute(params![id, star.ra, star.dec]).map_err(invalid)?;
            kept += 1;
            Ok(())
        }));

        let mut response = client.get(format!("{CDN}{}", file.name)).send().await?.error_for_status()?;
        let mut hash = Md5::new();
        while let Some(chunk) = response.chunk().await? {
            hash.update(&chunk);
            tally.bytes += chunk.len() as u64;
            decoder.write_all(&chunk)?;
        }
        decoder.try_finish()?;
        drop(decoder);
        let got = hex::encode(hash.finalize());
        if got != file.md5 {
            return Err(format!("md5 {got}, manifest says {}", file.md5).into());
        }
        tally.seen = seen;
        tally.kept = kept;
        tally.cell_mismatch = cell_mismatch;
    }
    tx.execute(
        "INSERT INTO ingested VALUES (?1, ?2, ?3, ?4, ?5)",
        params![file.name, file.md5, tally.bytes as i64, tally.seen as i64, tally.kept as i64],
    )?;
    tx.commit()?;
    Ok(tally)
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn report(conn: &Connection, options: &Options) -> Outcome<()> {
    let (files, bytes, seen, kept): (i64, i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), SUM(bytes), SUM(rows_seen), SUM(rows_kept) FROM ingested",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let size = std::fs::metadata(&options.out)?.len();
    println!(
        "store {}: {} files, {:.2} GB streamed, {} rows seen, {} rows kept, {:.1} MB on disk, {:.1} bytes per row",
        options.out.display(),
        files,
        bytes as f64 / 1e9,
        seen,
        kept,
        size as f64 / 1e6,
        size as f64 / kept as f64,
    );
    Ok(())
}
