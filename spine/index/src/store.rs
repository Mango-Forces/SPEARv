//! The SQLite database behind the index-service. Only the service opens it.
//!
//! One writer connection, guarded by a mutex, so the crawler's writes queue
//! instead of failing with "database is locked". Readers open their own
//! connections; WAL keeps them from blocking on the writer.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::api::*;
use crate::{footprint_boxes, overlaps, passes, version_order, view_ra_ranges, wcs_blob};

pub type Result<T> = std::result::Result<T, String>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS exposure (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    etag TEXT,
    size INTEGER,
    release TEXT NOT NULL,
    survey TEXT NOT NULL,
    sps_flags TEXT,
    period TEXT NOT NULL,
    version TEXT NOT NULL,
    obsid TEXT NOT NULL,
    detector INTEGER NOT NULL,
    mjd_beg REAL,
    mjd_avg REAL NOT NULL,
    em_min_um REAL NOT NULL,
    em_max_um REAL NOT NULL,
    wcs BLOB NOT NULL,
    dqa_l1 TEXT,
    dqa_l2 TEXT,
    psf_fwhm REAL,
    finast INTEGER,
    aurora INTEGER,
    airglow INTEGER,
    shuttle_glow INTEGER,
    n_streaks INTEGER,
    src_frac REAL,
    sc_x REAL,
    sc_y REAL,
    sc_z REAL
);
-- The crawler's home-side scopes and the release summary read these instead of the table.
CREATE INDEX IF NOT EXISTS exposure_scope ON exposure(release, period, version);
CREATE INDEX IF NOT EXISTS exposure_summary ON exposure(release, survey, mjd_avg);
-- R*Tree ids are exposure.id * 2 + box, so a footprint is found and deleted
-- without a join table. A footprint is at most two boxes (split at RA 0).
CREATE VIRTUAL TABLE IF NOT EXISTS footprint USING rtree(id, ra_min, ra_max, dec_min, dec_max);
CREATE TABLE IF NOT EXISTS release_listing (
    release TEXT PRIMARY KEY,
    newest_period TEXT,
    last_listed_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS crawl (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    listed INTEGER NOT NULL DEFAULT 0,
    added INTEGER NOT NULL DEFAULT 0,
    changed INTEGER NOT NULL DEFAULT 0,
    removed INTEGER NOT NULL DEFAULT 0,
    kept_after_head INTEGER NOT NULL DEFAULT 0,
    head_errors INTEGER NOT NULL DEFAULT 0,
    header_failures INTEGER NOT NULL DEFAULT 0,
    prefixes_failed INTEGER NOT NULL DEFAULT 0,
    error TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT OR IGNORE INTO meta (name, value) VALUES ('generation', 0);
";

/// The R*Tree leads. The `+` keeps SQLite off the exposure indexes, which would
/// walk a whole release instead of the few boxes near the view.
const COVERAGE_SQL: &str = "
SELECT e.id, e.key, e.etag, e.size, e.release, e.survey, e.sps_flags, e.period, e.version,
       e.obsid, e.detector, e.mjd_beg, e.mjd_avg, e.em_min_um, e.em_max_um, e.wcs,
       e.dqa_l1, e.dqa_l2, e.psf_fwhm, e.finast, e.aurora, e.airglow, e.shuttle_glow,
       e.n_streaks, e.src_frac, e.sc_x, e.sc_y, e.sc_z
FROM footprint f JOIN exposure e ON e.id = f.id / 2
WHERE f.ra_max >= ?1 AND f.ra_min <= ?2 AND f.dec_max >= ?3 AND f.dec_min <= ?4
  AND (?5 IS NULL OR +e.release = ?5)
  AND (?6 IS NULL OR +e.survey = ?6)
  AND (?7 IS NULL OR +e.mjd_avg >= ?7)
  AND (?8 IS NULL OR +e.mjd_avg <= ?8)";

pub struct Store {
    path: PathBuf,
    writer: Mutex<Connection>,
}

/// What the crawler needs to know about one indexed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeKey {
    pub etag: Option<String>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
        }
        let conn = Connection::open(&path).map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(60)).map_err(sql)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").map_err(sql)?;
        conn.execute_batch(SCHEMA).map_err(sql)?;
        Ok(Self { path, writer: Mutex::new(conn) })
    }

    fn reader(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(sql)?;
        Ok(conn)
    }

    fn write<T>(&self, work: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut conn = self.writer.lock().map_err(|_| "index writer lock poisoned".to_string())?;
        work(&mut conn).map_err(sql)
    }

    pub fn is_empty(&self) -> Result<bool> {
        let conn = self.reader()?;
        let any: Option<i64> = conn.query_row("SELECT id FROM exposure LIMIT 1", [], |r| r.get(0)).optional().map_err(sql)?;
        Ok(any.is_none())
    }

    pub fn generation(&self) -> Result<i64> {
        self.reader()?
            .query_row("SELECT value FROM meta WHERE name = 'generation'", [], |r| r.get(0))
            .map_err(sql)
    }

    /// Insert or replace rows by key, with their footprints, in one transaction.
    /// Bumps the generation when anything was written.
    pub fn upsert(&self, rows: &[ExposureRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.write(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            {
                let mut upsert = tx.prepare_cached(
                    "INSERT INTO exposure (key, etag, size, release, survey, sps_flags, period, version, obsid,
                        detector, mjd_beg, mjd_avg, em_min_um, em_max_um, wcs, dqa_l1, dqa_l2, psf_fwhm, finast,
                        aurora, airglow, shuttle_glow, n_streaks, src_frac, sc_x, sc_y, sc_z)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
                        ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)
                     ON CONFLICT(key) DO UPDATE SET
                        etag=excluded.etag, size=excluded.size, release=excluded.release, survey=excluded.survey,
                        sps_flags=excluded.sps_flags, period=excluded.period, version=excluded.version,
                        obsid=excluded.obsid, detector=excluded.detector, mjd_beg=excluded.mjd_beg,
                        mjd_avg=excluded.mjd_avg, em_min_um=excluded.em_min_um, em_max_um=excluded.em_max_um,
                        wcs=excluded.wcs, dqa_l1=excluded.dqa_l1, dqa_l2=excluded.dqa_l2,
                        psf_fwhm=excluded.psf_fwhm, finast=excluded.finast, aurora=excluded.aurora,
                        airglow=excluded.airglow, shuttle_glow=excluded.shuttle_glow,
                        n_streaks=excluded.n_streaks, src_frac=excluded.src_frac,
                        sc_x=excluded.sc_x, sc_y=excluded.sc_y, sc_z=excluded.sc_z
                     RETURNING id",
                )?;
                let mut clear = tx.prepare_cached("DELETE FROM footprint WHERE id IN (?1 * 2, ?1 * 2 + 1)")?;
                let mut boxed = tx.prepare_cached(
                    "INSERT INTO footprint (id, ra_min, ra_max, dec_min, dec_max) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for row in rows {
                    let q = &row.quality;
                    let sc = row.sc_xyz;
                    let id: i64 = upsert.query_row(
                        params![
                            row.key, row.etag, row.size, row.release, row.survey.as_str(), row.sps_flags,
                            row.period, row.version, row.obsid, row.detector, row.mjd_beg, row.mjd_avg,
                            row.em_min_um, row.em_max_um, wcs_blob::encode(&row.wcs), q.dqa_l1, q.dqa_l2,
                            q.psf_fwhm, q.finast, q.aurora, q.airglow, q.shuttle_glow, q.n_streaks, q.src_frac,
                            sc.map(|s| s[0]), sc.map(|s| s[1]), sc.map(|s| s[2]),
                        ],
                        |r| r.get(0),
                    )?;
                    clear.execute(params![id])?;
                    for (slot, (ra_min, ra_max, dec_min, dec_max)) in footprint_boxes(&row.wcs).into_iter().take(2).enumerate() {
                        boxed.execute(params![id * 2 + slot as i64, ra_min, ra_max, dec_min, dec_max])?;
                    }
                }
            }
            bump(&tx)?;
            tx.commit()?;
            Ok(rows.len())
        })
    }

    /// Delete rows and their footprints by key. Bumps the generation when any went.
    pub fn delete_keys(&self, keys: &[String]) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        self.write(|conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut removed = 0;
            {
                let mut find = tx.prepare_cached("SELECT id FROM exposure WHERE key = ?1")?;
                let mut clear = tx.prepare_cached("DELETE FROM footprint WHERE id IN (?1 * 2, ?1 * 2 + 1)")?;
                let mut drop = tx.prepare_cached("DELETE FROM exposure WHERE id = ?1")?;
                for key in keys {
                    let Some(id) = find.query_row(params![key], |r| r.get::<_, i64>(0)).optional()? else {
                        continue;
                    };
                    clear.execute(params![id])?;
                    removed += drop.execute(params![id])?;
                }
            }
            if removed > 0 {
                bump(&tx)?;
            }
            tx.commit()?;
            Ok(removed)
        })
    }

    /// Every indexed key under a folder prefix such as `qr3/level2/2026W40_2A/l2b-v27-2026-285/`.
    pub fn keys_under(&self, prefix: &str) -> Result<HashMap<String, HomeKey>> {
        let upper = prefix_upper_bound(prefix)?;
        let conn = self.reader()?;
        let mut stmt = conn.prepare("SELECT key, etag FROM exposure WHERE key >= ?1 AND key < ?2").map_err(sql)?;
        let rows = stmt
            .query_map(params![prefix, upper], |r| Ok((r.get::<_, String>(0)?, HomeKey { etag: r.get(1)? })))
            .map_err(sql)?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>().map_err(sql)
    }

    /// Every `(release, period, version)` folder that has rows.
    pub fn scopes(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare("SELECT DISTINCT release, period, version FROM exposure").map_err(sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(sql)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)
    }

    pub fn record_listing(&self, release: &str, newest_period: Option<&str>, now_ms: i64) -> Result<()> {
        self.write(|conn| {
            conn.execute(
                "INSERT INTO release_listing (release, newest_period, last_listed_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(release) DO UPDATE SET newest_period = excluded.newest_period,
                    last_listed_ms = excluded.last_listed_ms",
                params![release, newest_period, now_ms],
            )
            .map(|_| ())
        })
    }

    /// Exposures whose footprint overlaps the view, newest processing version
    /// only, after the query's filters.
    pub fn coverage(&self, query: &CoverageQuery) -> Result<Vec<ExposureRow>> {
        query.validate()?;
        let half = query.fov / 2.0;
        let (dec_lo, dec_hi) = (query.dec - half, query.dec + half);
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(COVERAGE_SQL).map_err(sql)?;
        let mut found: BTreeMap<i64, ExposureRow> = BTreeMap::new();
        for (ra_lo, ra_hi) in view_ra_ranges(query.ra, query.dec, query.fov) {
            let rows = stmt
                .query_map(
                    params![
                        ra_lo, ra_hi, dec_lo, dec_hi, query.release, query.survey.map(Survey::as_str),
                        query.from, query.to
                    ],
                    |r| Ok((r.get::<_, i64>(0)?, row_from_sql(r))),
                )
                .map_err(sql)?;
            for row in rows {
                let (id, row) = row.map_err(sql)?;
                if let std::collections::btree_map::Entry::Vacant(slot) = found.entry(id) {
                    slot.insert(row?);
                }
            }
        }
        // The newest processing version supersedes the rest, whatever its
        // quality, so pick it before the per-row filters.
        let mut newest: HashMap<(String, u8), ExposureRow> = HashMap::new();
        for row in found.into_values() {
            let slot = (row.obsid.clone(), row.detector);
            match newest.get(&slot) {
                Some(kept) if version_order(&kept.version) >= version_order(&row.version) => {}
                _ => {
                    newest.insert(slot, row);
                }
            }
        }
        let wavelength = query.wavelength();
        let region = spearv_core::SkyRegion::square(query.ra, query.dec, query.fov);
        let mut out: Vec<ExposureRow> = newest
            .into_values()
            .filter(|row| overlaps(&row.wcs, query.ra, query.dec, query.fov))
            .filter(|row| query.quality.is_none() || row.quality.passed())
            .filter(|row| {
                wavelength.is_none_or(|wl| {
                    crate::lvf::exposure_covers_wavelength(&row.wcs, row.em_min_um, row.em_max_um, &region, wl)
                })
            })
            .collect();
        out.sort_by(|a, b| a.mjd_avg.total_cmp(&b.mjd_avg).then_with(|| a.key.cmp(&b.key)));
        Ok(out)
    }

    pub fn passes(&self, query: &CoverageQuery) -> Result<Vec<Pass>> {
        let rows = self.coverage(query)?;
        Ok(passes::passes(&rows.iter().map(|row| (row.mjd_avg, row.detector)).collect::<Vec<_>>()))
    }

    /// Releases with rows, plus any release the crawler has listed but not yet
    /// indexed. Reads the summary index, not the table.
    pub fn releases(&self) -> Result<Vec<ReleaseSummary>> {
        let conn = self.reader()?;
        let mut by_release: BTreeMap<String, ReleaseSummary> = BTreeMap::new();
        let mut stmt = conn
            .prepare("SELECT release, survey, count(*), min(mjd_avg), max(mjd_avg) FROM exposure GROUP BY release, survey")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, Option<f64>>(4)?,
                ))
            })
            .map_err(sql)?;
        for row in rows {
            let (release, survey, count, oldest, newest) = row.map_err(sql)?;
            let entry = by_release.entry(release.clone()).or_insert_with(|| empty_summary(release));
            entry.exposures += count;
            match Survey::parse(&survey) {
                Some(Survey::Deep) => entry.deep += count,
                _ => entry.all_sky += count,
            }
            entry.oldest_mjd = min_opt(entry.oldest_mjd, oldest);
            entry.newest_mjd = max_opt(entry.newest_mjd, newest);
        }
        let mut stmt = conn.prepare("SELECT release, newest_period, last_listed_ms FROM release_listing").map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?)))
            .map_err(sql)?;
        for row in rows {
            let (release, period, listed) = row.map_err(sql)?;
            let entry = by_release.entry(release.clone()).or_insert_with(|| empty_summary(release));
            entry.newest_period = period;
            entry.last_listed_ms = Some(listed);
        }
        Ok(by_release.into_values().collect())
    }

    pub fn crawl_start(&self, kind: CrawlKind, now_ms: i64) -> Result<CrawlRecord> {
        let id = self.write(|conn| {
            conn.execute("INSERT INTO crawl (kind, started_ms) VALUES (?1, ?2)", params![kind.as_str(), now_ms])?;
            Ok(conn.last_insert_rowid())
        })?;
        Ok(CrawlRecord {
            id,
            kind,
            started_ms: now_ms,
            finished_ms: None,
            listed: 0,
            added: 0,
            changed: 0,
            removed: 0,
            kept_after_head: 0,
            head_errors: 0,
            header_failures: 0,
            prefixes_failed: 0,
            error: None,
        })
    }

    pub fn crawl_save(&self, record: &CrawlRecord) -> Result<()> {
        self.write(|conn| {
            conn.execute(
                "UPDATE crawl SET finished_ms = ?2, listed = ?3, added = ?4, changed = ?5, removed = ?6,
                    kept_after_head = ?7, head_errors = ?8, header_failures = ?9, prefixes_failed = ?10, error = ?11
                 WHERE id = ?1",
                params![
                    record.id, record.finished_ms, record.listed, record.added, record.changed, record.removed,
                    record.kept_after_head, record.head_errors, record.header_failures, record.prefixes_failed,
                    record.error
                ],
            )
            .map(|_| ())
        })
    }

    /// The newest finished crawl.
    pub fn last_crawl(&self) -> Result<Option<CrawlRecord>> {
        self.reader()?
            .query_row(
                "SELECT id, kind, started_ms, finished_ms, listed, added, changed, removed, kept_after_head,
                    head_errors, header_failures, prefixes_failed, error
                 FROM crawl WHERE finished_ms IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok(CrawlRecord {
                        id: r.get(0)?,
                        kind: if r.get::<_, String>(1)? == "full" { CrawlKind::Full } else { CrawlKind::Diff },
                        started_ms: r.get(2)?,
                        finished_ms: r.get(3)?,
                        listed: r.get(4)?,
                        added: r.get(5)?,
                        changed: r.get(6)?,
                        removed: r.get(7)?,
                        kept_after_head: r.get(8)?,
                        head_errors: r.get(9)?,
                        header_failures: r.get(10)?,
                        prefixes_failed: r.get(11)?,
                        error: r.get(12)?,
                    })
                },
            )
            .optional()
            .map_err(sql)
    }

    #[cfg(test)]
    pub(crate) fn explain_coverage(&self) -> Result<String> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {COVERAGE_SQL}")).map_err(sql)?;
        let rows = stmt
            .query_map(params![0.0, 1.0, 0.0, 1.0, "qr3", "deep", 0.0, 1.0], |r| r.get::<_, String>(3))
            .map_err(sql)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)?.join(" | "))
    }
}

fn bump(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE meta SET value = value + 1 WHERE name = 'generation'", []).map(|_| ())
}

/// The smallest string greater than every key under a `/`-terminated prefix.
fn prefix_upper_bound(prefix: &str) -> Result<String> {
    let stem = prefix.strip_suffix('/').ok_or_else(|| format!("prefix {prefix:?} must end with /"))?;
    // '0' is the character after '/'.
    Ok(format!("{stem}0"))
}

fn row_from_sql(r: &rusqlite::Row<'_>) -> Result<ExposureRow> {
    let get = |e: rusqlite::Error| e.to_string();
    let survey: String = r.get(5).map_err(get)?;
    let blob: Vec<u8> = r.get(15).map_err(get)?;
    let key: String = r.get(1).map_err(get)?;
    let sc: (Option<f64>, Option<f64>, Option<f64>) = (r.get(25).map_err(get)?, r.get(26).map_err(get)?, r.get(27).map_err(get)?);
    Ok(ExposureRow {
        url: format!("{BUCKET}/{key}"),
        etag: r.get(2).map_err(get)?,
        size: r.get(3).map_err(get)?,
        release: r.get(4).map_err(get)?,
        survey: Survey::parse(&survey).ok_or_else(|| format!("{key}: unknown survey {survey}"))?,
        sps_flags: r.get(6).map_err(get)?,
        period: r.get(7).map_err(get)?,
        version: r.get(8).map_err(get)?,
        obsid: r.get(9).map_err(get)?,
        detector: r.get(10).map_err(get)?,
        mjd_beg: r.get(11).map_err(get)?,
        mjd_avg: r.get(12).map_err(get)?,
        em_min_um: r.get(13).map_err(get)?,
        em_max_um: r.get(14).map_err(get)?,
        wcs: wcs_blob::decode(&blob).map_err(|err| format!("{key}: {err}"))?,
        quality: Quality {
            dqa_l1: r.get(16).map_err(get)?,
            dqa_l2: r.get(17).map_err(get)?,
            psf_fwhm: r.get(18).map_err(get)?,
            finast: r.get(19).map_err(get)?,
            aurora: r.get(20).map_err(get)?,
            airglow: r.get(21).map_err(get)?,
            shuttle_glow: r.get(22).map_err(get)?,
            n_streaks: r.get(23).map_err(get)?,
            src_frac: r.get(24).map_err(get)?,
        },
        sc_xyz: match sc {
            (Some(x), Some(y), Some(z)) => Some([x, y, z]),
            _ => None,
        },
        key,
    })
}

fn empty_summary(release: String) -> ReleaseSummary {
    ReleaseSummary {
        release,
        exposures: 0,
        all_sky: 0,
        deep: 0,
        newest_period: None,
        oldest_mjd: None,
        newest_mjd: None,
        last_listed_ms: None,
    }
}

fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn sql(err: rusqlite::Error) -> String {
    err.to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testing::detector;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct TempStore {
        pub store: Store,
        path: PathBuf,
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
        }
    }

    pub fn temp_store() -> TempStore {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("spearv-index-store-{}-{id}.sqlite", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        TempStore { store: Store::open(&path).unwrap(), path }
    }

    /// Detector 1 is 0.74-1.12 µm and each later detector sits redward of it.
    fn detector_range(detector: u8) -> (f64, f64) {
        let start = 0.74 * 1.5_f64.powi(i32::from(detector) - 1);
        (start, start * 1.5)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn row(release: &str, survey: Survey, obsid: &str, detector_no: u8, version: &str, ra: f64, dec: f64, mjd: f64) -> ExposureRow {
        let period = &obsid[..10];
        let key = format!("{release}/level2/{period}/{version}/{detector_no}/level2_{obsid}D{detector_no}_spx_{version}.fits");
        let (em_min_um, em_max_um) = detector_range(detector_no);
        ExposureRow {
            url: format!("{BUCKET}/{key}"),
            key,
            etag: Some("\"e1\"".into()),
            size: Some(65_000_000),
            release: release.into(),
            survey,
            sps_flags: Some(if survey == Survey::Deep { "deep_north" } else { "all_sky" }.into()),
            period: period.into(),
            version: version.into(),
            obsid: obsid.into(),
            detector: detector_no,
            mjd_beg: Some(mjd - 0.001),
            mjd_avg: mjd,
            em_min_um,
            em_max_um,
            wcs: detector(ra, dec),
            quality: Quality { dqa_l1: Some("Pass".into()), dqa_l2: Some("Pass".into()), ..Quality::default() },
            sc_xyz: Some([1.0, 2.0, 3.0]),
        }
    }

    #[test]
    fn coverage_searches_the_sky_first() {
        let t = temp_store();
        let plan = t.store.explain_coverage().unwrap();
        assert!(plan.starts_with("SCAN f VIRTUAL TABLE"), "{plan}");
        assert!(plan.contains("SEARCH e USING INTEGER PRIMARY KEY"), "{plan}");
    }

    #[test]
    fn a_row_round_trips_with_every_field() {
        let t = temp_store();
        let mut stored = row("qr3", Survey::Deep, "2026W40_2A_0001_1", 4, "l2b-v27-2026-285", 270.0, 66.5, 61314.5);
        stored.quality = Quality {
            dqa_l1: Some("Pass".into()),
            dqa_l2: Some("Pass".into()),
            psf_fwhm: Some(7.53),
            finast: Some(0),
            aurora: Some(true),
            airglow: Some(false),
            shuttle_glow: None,
            n_streaks: Some(2),
            src_frac: Some(77.93),
        };
        assert_eq!(t.store.upsert(std::slice::from_ref(&stored)).unwrap(), 1);
        let found = t.store.coverage(&CoverageQuery::view(270.0, 66.5, 0.2)).unwrap();
        assert_eq!(found, vec![stored]);
        assert!(!t.store.is_empty().unwrap());
    }

    #[test]
    fn deep_and_all_sky_are_one_release_with_a_survey_filter() {
        let t = temp_store();
        t.store
            .upsert(&[
                row("qr2", Survey::AllSky, "2025W34_2B_0501_1", 1, "l2b-v20-2025-267", 270.0, 66.5, 60911.0),
                row("qr2", Survey::Deep, "2025W35_1A_0001_1", 1, "l2b-v20-2025-270", 270.1, 66.5, 60915.0),
                row("qr3", Survey::Deep, "2026W40_2A_0001_1", 1, "l2b-v27-2026-285", 270.0, 66.6, 61314.0),
            ])
            .unwrap();
        let all = t.store.coverage(&CoverageQuery::view(270.0, 66.5, 0.2)).unwrap();
        assert_eq!(all.len(), 3);
        let qr2 = CoverageQuery { release: Some("qr2".into()), ..CoverageQuery::view(270.0, 66.5, 0.2) };
        assert_eq!(t.store.coverage(&qr2).unwrap().len(), 2);
        let deep = CoverageQuery { survey: Some(Survey::Deep), ..qr2.clone() };
        let found = t.store.coverage(&deep).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].obsid, "2025W35_1A_0001_1");
        let window = CoverageQuery { from: Some(60912.0), to: Some(61000.0), ..CoverageQuery::view(270.0, 66.5, 0.2) };
        assert_eq!(t.store.coverage(&window).unwrap().len(), 1);
        let releases = t.store.releases().unwrap();
        assert_eq!(releases.iter().map(|r| r.release.as_str()).collect::<Vec<_>>(), ["qr2", "qr3"]);
        assert_eq!((releases[0].exposures, releases[0].all_sky, releases[0].deep), (2, 1, 1));
        assert_eq!(releases[0].oldest_mjd, Some(60911.0));
        assert_eq!(releases[1].newest_mjd, Some(61314.0));
    }

    #[test]
    fn the_newest_version_wins_even_when_it_fails_quality() {
        let t = temp_store();
        let old = row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b-v27-2026-222", 56.8, 24.0, 61241.0);
        let retry = row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b_retry-v27-2026-230", 56.8, 24.0, 61241.0);
        let mut failed = retry.clone();
        failed.quality.dqa_l2 = Some("Fail".into());
        t.store.upsert(&[old.clone(), failed.clone()]).unwrap();
        let view = CoverageQuery::view(56.8, 24.0, 0.1);
        let found = t.store.coverage(&view).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].version, "l2b_retry-v27-2026-230");
        let strict = CoverageQuery { quality: Some(QualityFilter::Pass), ..view };
        assert!(t.store.coverage(&strict).unwrap().is_empty());
    }

    #[test]
    fn upsert_replaces_a_row_and_its_footprint_by_key() {
        let t = temp_store();
        let first = row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b-v27-2026-222", 56.8, 24.0, 61241.0);
        t.store.upsert(std::slice::from_ref(&first)).unwrap();
        let generation = t.store.generation().unwrap();
        let mut moved = first.clone();
        moved.wcs = detector(120.0, -30.0);
        moved.etag = Some("\"e2\"".into());
        t.store.upsert(std::slice::from_ref(&moved)).unwrap();
        assert!(t.store.generation().unwrap() > generation);
        assert!(t.store.coverage(&CoverageQuery::view(56.8, 24.0, 0.1)).unwrap().is_empty());
        assert_eq!(t.store.coverage(&CoverageQuery::view(120.0, -30.0, 0.1)).unwrap(), vec![moved]);
        let home = t.store.keys_under("qr3/level2/2026W30_1B/l2b-v27-2026-222/").unwrap();
        assert_eq!(home.len(), 1);
        assert_eq!(home[&first.key].etag.as_deref(), Some("\"e2\""));
    }

    #[test]
    fn delete_removes_the_row_and_its_footprint() {
        let t = temp_store();
        let a = row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b-v27-2026-222", 0.2, 10.0, 61241.0);
        let b = row("qr3", Survey::AllSky, "2026W30_1B_0002_1", 2, "l2b-v27-2026-222", 0.2, 10.0, 61241.1);
        t.store.upsert(&[a.clone(), b.clone()]).unwrap();
        // Straddles RA 0, so two boxes; both must go.
        let generation = t.store.generation().unwrap();
        assert_eq!(t.store.delete_keys(&[a.key.clone(), "missing".into()]).unwrap(), 1);
        assert!(t.store.generation().unwrap() > generation);
        let left = t.store.coverage(&CoverageQuery::view(359.9, 10.0, 0.3)).unwrap();
        assert_eq!(left.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(), [b.key.as_str()]);
        let generation = t.store.generation().unwrap();
        assert_eq!(t.store.delete_keys(&["missing".into()]).unwrap(), 0);
        assert_eq!(t.store.generation().unwrap(), generation);
    }

    #[test]
    fn keys_under_a_prefix_stop_at_the_folder() {
        let t = temp_store();
        let inside = row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b-v27-2026-222", 10.0, 0.0, 61241.0);
        let sibling = row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b-v27-2026-2220", 10.0, 0.0, 61241.0);
        t.store.upsert(&[inside.clone(), sibling]).unwrap();
        let home = t.store.keys_under("qr3/level2/2026W30_1B/l2b-v27-2026-222/").unwrap();
        assert_eq!(home.keys().collect::<Vec<_>>(), [&inside.key]);
        assert!(t.store.keys_under("no-slash").is_err());
        let scopes = t.store.scopes().unwrap();
        assert_eq!(scopes.len(), 2);
    }

    #[test]
    fn a_listed_release_without_rows_still_shows() {
        let t = temp_store();
        t.store.record_listing("qr4", Some("2027W01_1A"), 1234).unwrap();
        let releases = t.store.releases().unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].release, "qr4");
        assert_eq!(releases[0].exposures, 0);
        assert_eq!(releases[0].newest_period.as_deref(), Some("2027W01_1A"));
        assert_eq!(releases[0].last_listed_ms, Some(1234));
    }

    #[test]
    fn crawl_records_round_trip() {
        let t = temp_store();
        let mut record = t.store.crawl_start(CrawlKind::Full, 10).unwrap();
        assert!(t.store.last_crawl().unwrap().is_none(), "an unfinished crawl is not the last crawl");
        record.finished_ms = Some(20);
        record.added = 5;
        record.error = Some("boom".into());
        t.store.crawl_save(&record).unwrap();
        assert_eq!(t.store.last_crawl().unwrap(), Some(record));
    }

    #[test]
    fn passes_come_from_the_same_coverage() {
        let t = temp_store();
        t.store
            .upsert(&[
                row("qr2", Survey::AllSky, "2025W34_2B_0501_1", 1, "l2b-v20-2025-267", 56.8, 24.0, 60911.0),
                row("qr2", Survey::AllSky, "2025W35_2B_0501_1", 4, "l2b-v20-2025-267", 56.8, 24.0, 60915.0),
                row("qr3", Survey::AllSky, "2026W30_1B_0001_1", 2, "l2b-v27-2026-222", 56.8, 24.0, 61241.0),
            ])
            .unwrap();
        let passes = t.store.passes(&CoverageQuery::view(56.8, 24.0, 0.1)).unwrap();
        assert_eq!(passes.len(), 2, "{passes:?}");
        assert_eq!(passes[0].detectors, vec![1, 4]);
        assert_eq!(passes[1].id, "p61241");
    }
}
