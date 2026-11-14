//! Build the Gaia SQLite store from the ESA CDN. Offline: the server never runs this.
//!
//!     spearv-gaia-build [--out data/gaia.sqlite] [--radius 3.0] [--all-sky --max-mag 17.0] [--jobs 8] [--no-vacuum] [--plan]
//!
//! Each file is streamed through gzip, filtered, and dropped. Nothing is written
//! to disk but the store. A rerun skips files already in the store, so it can resume.

use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::write::MultiGzDecoder;
use md5::{Digest, Md5};
use rusqlite::{params, Connection, OptionalExtension};
use spearv_gaia::motion::{separation_deg, GAIA_DR3_REF_EPOCH};
use spearv_gaia::plan::{
    cell_of_sky, cell_of_source, cone_cells, files_for_cells, parse_manifest, CdnFile, Columns, Field, LineSplitter,
    Star, CELL_MARGIN_DEG, FIELDS,
};
use spearv_gaia::{record_max_pm, SCHEMA};

const CDN: &str = "https://cdn.gea.esac.esa.int/Gaia/gdr3/gaia_source/";
const ATTEMPTS: u32 = 4;
const DEFAULT_JOBS: usize = 8;

type Outcome<T> = Result<T, Box<dyn Error>>;

struct Options {
    out: std::path::PathBuf,
    radius: f64,
    all_sky: bool,
    max_mag: Option<f64>,
    jobs: usize,
    plan_only: bool,
    vacuum: bool,
}

enum Filter {
    AllSky {
        max_mag: Option<f64>,
    },
    Fields {
        cells: BTreeSet<u32>,
        radius: f64,
        max_mag: Option<f64>,
    },
}

struct ParsedFile {
    file: CdnFile,
    bytes: u64,
    seen: u64,
    kept: u64,
    /// Kept rows whose published position lands in another cell than source_id says.
    cell_mismatch: u64,
    stars: Vec<Star>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Outcome<()> {
    let options = parse_args()?;
    // The CDN rotates edge hosts and some refuse connections, so fail fast and retry.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(options.jobs)
        .build()?;
    let manifest = fetch_manifest(&client).await?;
    let files = parse_manifest(&manifest);
    println!("manifest: {} data files", files.len());

    let (wanted, filter): (Vec<CdnFile>, Filter) = if options.all_sky {
        println!(
            "all-sky mode: {} files, max_mag={:?}, jobs={}, vacuum={}",
            files.len(),
            options.max_mag,
            options.jobs,
            options.vacuum,
        );
        (files, Filter::AllSky { max_mag: options.max_mag })
    } else {
        let plans: Vec<(Field, BTreeSet<u32>)> = FIELDS
            .iter()
            .map(|field| (*field, cone_cells(field.ra, field.dec, options.radius + CELL_MARGIN_DEG)))
            .collect();
        let cells: BTreeSet<u32> = plans.iter().flat_map(|plan| plan.1.iter().copied()).collect();
        for (field, field_cells) in &plans {
            let names: Vec<&str> = files_for_cells(&files, field_cells).iter().map(|file| file.name.as_str()).collect();
            println!("{}: {} cells, {} files: {}", field.name, field_cells.len(), names.len(), names.join(" "));
        }
        let wanted: Vec<CdnFile> = files_for_cells(&files, &cells).into_iter().cloned().collect();
        println!("total: {} distinct files for radius {} degrees", wanted.len(), options.radius);
        (
            wanted,
            Filter::Fields { cells, radius: options.radius, max_mag: options.max_mag },
        )
    };

    if options.plan_only {
        return Ok(());
    }

    if let Some(parent) = options.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut conn = Connection::open(&options.out)?;
    conn.execute_batch(SCHEMA)?;
    require_same_config(&conn, &options)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "cache_size", -524_288_i64)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    let done_files = load_done_files(&conn)?;
    let total_wanted = wanted.len();
    let already_done = wanted.iter().filter(|file| done_files.contains(&file.name)).count();
    let pending: Vec<CdnFile> = wanted
        .into_iter()
        .filter(|file| !done_files.contains(&file.name))
        .collect();
    println!("resuming: {already_done}/{total_wanted} files already in store, {} remaining", pending.len());

    if !pending.is_empty() {
        ingest_all(&client, &mut conn, pending, Arc::new(filter), options.jobs, already_done, total_wanted).await?;
    }

    record_max_pm(&conn)?;
    if !options.all_sky {
        // A field goes in only now, so the store never claims a cone it only partly holds.
        for field in FIELDS {
            conn.execute(
                "INSERT OR REPLACE INTO field VALUES (?1, ?2, ?3, ?4)",
                params![field.name, field.ra, field.dec, options.radius],
            )?;
        }
    }
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    if options.vacuum {
        conn.execute_batch("VACUUM")?;
    }
    report(&conn, &options)
}

async fn ingest_all(
    client: &reqwest::Client,
    conn: &mut Connection,
    pending: Vec<CdnFile>,
    filter: Arc<Filter>,
    jobs: usize,
    already_done: usize,
    total_wanted: usize,
) -> Outcome<()> {
    let pending = Arc::new(pending);
    let cursor = Arc::new(AtomicUsize::new(0));
    let worker_count = jobs.min(pending.len());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<ParsedFile, String>>(4);
    let mut handles = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let client = client.clone();
        let pending = Arc::clone(&pending);
        let cursor = Arc::clone(&cursor);
        let filter = Arc::clone(&filter);
        let tx = tx.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let index = cursor.fetch_add(1, Ordering::Relaxed);
                let Some(file) = pending.get(index) else {
                    break;
                };
                let result = download_and_parse_with_retries(&client, file, &filter).await;
                let failed = result.is_err();
                if tx.send(result).await.is_err() || failed {
                    break;
                }
            }
        }));
    }
    drop(tx);

    let started = Instant::now();
    let total_pending = pending.len();
    let write_outcome: Outcome<()> = tokio::task::block_in_place(|| {
        let mut completed = 0usize;
        let mut total_kept = 0u64;
        while let Some(item) = rx.blocking_recv() {
            let parsed = item.map_err(|msg| -> Box<dyn Error> { msg.into() })?;
            write_parsed_file(conn, &parsed)?;
            completed += 1;
            total_kept += parsed.kept;
            let elapsed = started.elapsed().as_secs_f64();
            let rate = completed as f64 / elapsed.max(1e-6);
            let eta = (total_pending - completed) as f64 / rate.max(1e-6);
            println!(
                "[{}/{}] {} {:.1} MB, {} seen, {} kept ({} total new), {} cell mismatches, {:.0}s elapsed, ETA {:.0}s",
                already_done + completed,
                total_wanted,
                parsed.file.name,
                parsed.bytes as f64 / 1e6,
                parsed.seen,
                parsed.kept,
                total_kept,
                parsed.cell_mismatch,
                elapsed,
                eta,
            );
        }
        Ok(())
    });

    if write_outcome.is_err() {
        for handle in &handles {
            handle.abort();
        }
    }
    for handle in handles {
        let _ = handle.await;
    }
    write_outcome
}

fn load_done_files(conn: &Connection) -> Outcome<HashSet<String>> {
    let mut statement = conn.prepare("SELECT file FROM ingested")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut done = HashSet::new();
    for row in rows {
        done.insert(row?);
    }
    Ok(done)
}

/// A resumed build skips finished files, so require the exact same selection parameters.
fn require_same_config(conn: &Connection, options: &Options) -> Outcome<()> {
    let all_sky_val = if options.all_sky { 1.0 } else { 0.0 };
    conn.execute("INSERT OR IGNORE INTO meta VALUES ('all_sky', ?1)", [all_sky_val])?;
    let stored_all_sky: f64 = conn.query_row("SELECT value FROM meta WHERE key = 'all_sky'", [], |row| row.get(0))?;
    if (stored_all_sky != 0.0) != options.all_sky {
        return Err(format!(
            "{} was built with all_sky={stored_all_sky}. Delete it to switch mode",
            options.out.display()
        )
        .into());
    }

    if !options.all_sky {
        conn.execute("INSERT OR IGNORE INTO meta VALUES ('radius_deg', ?1)", [options.radius])?;
        let stored_radius: f64 =
            conn.query_row("SELECT value FROM meta WHERE key = 'radius_deg'", [], |row| row.get(0))?;
        if stored_radius != options.radius {
            return Err(format!(
                "{} was built at radius {stored_radius}. Delete it to use {}",
                options.out.display(),
                options.radius
            )
            .into());
        }
    }

    let stored_max_mag: Option<f64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'max_mag'", [], |row| row.get(0))
        .optional()?;
    match (stored_max_mag, options.max_mag) {
        (None, Some(mag)) => {
            let existing_rows: i64 = conn.query_row("SELECT COUNT(*) FROM ingested", [], |row| row.get(0))?;
            if existing_rows > 0 {
                return Err(format!(
                    "{} was built without max_mag. Delete it to use --max-mag {mag}",
                    options.out.display()
                )
                .into());
            }
            conn.execute("INSERT INTO meta VALUES ('max_mag', ?1)", [mag])?;
        }
        (Some(stored), Some(mag)) if stored != mag => {
            return Err(format!(
                "{} was built at max_mag {stored}. Delete it to use {mag}",
                options.out.display()
            )
            .into());
        }
        (Some(stored), None) => {
            return Err(format!(
                "{} was built at max_mag {stored}. Pass --max-mag {stored} to resume or delete it",
                options.out.display()
            )
            .into());
        }
        _ => {}
    }

    Ok(())
}

fn parse_args() -> Outcome<Options> {
    let mut out = "data/gaia.sqlite".into();
    let mut radius = 3.0;
    let mut radius_set = false;
    let mut all_sky = false;
    let mut max_mag = None;
    let mut jobs = DEFAULT_JOBS;
    let mut plan_only = false;
    let mut vacuum_override = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().ok_or("--out needs a path")?.into(),
            "--radius" => {
                radius = args.next().ok_or("--radius needs degrees")?.parse()?;
                radius_set = true;
            }
            "--all-sky" => all_sky = true,
            "--max-mag" => max_mag = Some(args.next().ok_or("--max-mag needs a magnitude")?.parse()?),
            "--jobs" => jobs = args.next().ok_or("--jobs needs a count")?.parse()?,
            "--plan" => plan_only = true,
            "--no-vacuum" => vacuum_override = Some(false),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }

    if all_sky && radius_set {
        return Err("--radius does not apply with --all-sky".into());
    }
    if all_sky && max_mag.is_none() {
        return Err("--all-sky requires --max-mag <G> (for example --max-mag 17.0)".into());
    }
    if jobs == 0 || jobs > 32 {
        return Err("--jobs must be between 1 and 32".into());
    }

    Ok(Options {
        out,
        radius,
        all_sky,
        max_mag,
        jobs,
        plan_only,
        vacuum: vacuum_override.unwrap_or(!all_sky),
    })
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

async fn download_and_parse_with_retries(
    client: &reqwest::Client,
    file: &CdnFile,
    filter: &Filter,
) -> Result<ParsedFile, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match download_and_parse(client, file, filter).await {
            Ok(parsed) => return Ok(parsed),
            Err(err) => {
                eprintln!("{} attempt {attempt}: {err}", file.name);
                last = err.to_string();
            }
        }
    }
    Err(format!("{} failed after {ATTEMPTS} attempts: {last}", file.name))
}

async fn download_and_parse(client: &reqwest::Client, file: &CdnFile, filter: &Filter) -> Outcome<ParsedFile> {
    let mut columns: Option<Columns> = None;
    let mut seen = 0_u64;
    let mut cell_mismatch = 0_u64;
    let mut stars = Vec::new();
    let mut bytes = 0_u64;

    let mut decoder = MultiGzDecoder::new(LineSplitter::new(|line: &str| {
        if line.starts_with('#') || line.is_empty() {
            return Ok(());
        }
        let Some(columns) = columns else {
            columns = Some(Columns::from_header(line).map_err(invalid)?);
            return Ok(());
        };
        seen += 1;

        let star = match filter {
            Filter::AllSky { max_mag } => {
                let Some(star) = columns
                    .star_filtered(line, *max_mag)
                    .map_err(|reason| invalid(format!("bad row ({reason}): {line:.80}")))?
                else {
                    return Ok(());
                };
                star
            }
            Filter::Fields { cells, radius, max_mag } => {
                let id = columns
                    .source_id(line)
                    .ok_or_else(|| invalid(format!("bad row: {line:.80}")))?;
                if !cells.contains(&cell_of_source(id)) {
                    return Ok(());
                }
                let Some(star) = columns
                    .star_filtered(line, *max_mag)
                    .map_err(|reason| invalid(format!("bad row ({reason}): {line:.80}")))?
                else {
                    return Ok(());
                };
                if !FIELDS.iter().any(|field| separation_deg(field.ra, field.dec, star.ra, star.dec) <= *radius) {
                    return Ok(());
                }
                star
            }
        };

        // The reader's proper-motion reach assumes one reference epoch.
        if star.ref_epoch != GAIA_DR3_REF_EPOCH {
            return Err(invalid(format!("source {} has ref_epoch {}", star.source_id, star.ref_epoch)));
        }
        cell_mismatch += u64::from(cell_of_sky(star.ra, star.dec) != cell_of_source(star.source_id));
        stars.push(star);
        Ok(())
    }));

    let mut response = client.get(format!("{CDN}{}", file.name)).send().await?.error_for_status()?;
    let mut hash = Md5::new();
    while let Some(chunk) = response.chunk().await? {
        hash.update(&chunk);
        bytes += chunk.len() as u64;
        decoder.write_all(&chunk)?;
    }
    decoder.try_finish()?;
    drop(decoder);
    let got = hex::encode(hash.finalize());
    if got != file.md5 {
        return Err(format!("md5 {got}, manifest says {}", file.md5).into());
    }
    let kept = stars.len() as u64;
    Ok(ParsedFile {
        file: file.clone(),
        bytes,
        seen,
        kept,
        cell_mismatch,
        stars,
    })
}

/// One transaction per file, so a broken stream or crash leaves no partial file.
fn write_parsed_file(conn: &mut Connection, parsed: &ParsedFile) -> Outcome<()> {
    let tx = conn.transaction()?;
    {
        let mut insert_star = tx.prepare_cached("INSERT INTO star VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
        let mut insert_sky = tx.prepare_cached("INSERT INTO star_sky VALUES (?1, ?2, ?2, ?3, ?3)")?;
        for star in &parsed.stars {
            insert_star.execute(params![
                star.source_id,
                star.ra,
                star.dec,
                star.pmra,
                star.pmdec,
                star.g,
                star.ref_epoch,
            ])?;
            insert_sky.execute(params![star.source_id, star.ra, star.dec])?;
        }
    }
    tx.execute(
        "INSERT INTO ingested VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            parsed.file.name,
            parsed.file.md5,
            parsed.bytes as i64,
            parsed.seen as i64,
            parsed.kept as i64,
        ],
    )?;
    tx.commit()?;
    Ok(())
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
    let bytes_per_row = if kept > 0 { size as f64 / kept as f64 } else { 0.0 };
    println!(
        "store {}: {} files, {:.2} GB streamed, {} rows seen, {} rows kept, {:.1} MB on disk, {:.1} bytes per row",
        options.out.display(),
        files,
        bytes as f64 / 1e9,
        seen,
        kept,
        size as f64 / 1e6,
        bytes_per_row,
    );
    Ok(())
}
