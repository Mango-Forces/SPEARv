//! Gaia DR3 as a catalog: a local, trimmed SQLite store with an R*Tree.
//! `spearv-gaia-build` makes the store offline. The server only reads it.

pub mod motion;
pub mod plan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use motion::{circle_boxes, propagate, separation_deg, years_since, GAIA_DR3_REF_EPOCH};
use rusqlite::{params, Connection, OpenFlags};
use spearv_core::{grid_size, CatalogRow, CatalogSource, Error, Result, SkyRegion, SourceId, PIXEL_SCALE_DEG};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS star (
    source_id INTEGER PRIMARY KEY,
    ra REAL NOT NULL,
    dec REAL NOT NULL,
    pmra REAL,
    pmdec REAL,
    phot_g_mean_mag REAL,
    ref_epoch REAL NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS star_sky USING rtree(id, ra_min, ra_max, dec_min, dec_max);
CREATE TABLE IF NOT EXISTS ingested (
    file TEXT PRIMARY KEY,
    md5 TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    rows_seen INTEGER NOT NULL,
    rows_kept INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS field (
    name TEXT PRIMARY KEY,
    ra REAL NOT NULL,
    dec REAL NOT NULL,
    radius_deg REAL NOT NULL
);
";

/// A cone the store holds completely. Written only after every file for it is in.
#[derive(Clone, Debug)]
pub struct CoveredField {
    pub name: String,
    pub ra: f64,
    pub dec: f64,
    pub radius_deg: f64,
}

pub struct GaiaStore {
    path: PathBuf,
    fields: Vec<CoveredField>,
    /// Fastest star in the store, so a query can reach stars that moved into it.
    max_pm_mas_yr: f64,
}

impl GaiaStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = read_only(&path)?;
        let mut statement = conn.prepare("SELECT name, ra, dec, radius_deg FROM field").map_err(sql)?;
        let fields = statement
            .query_map([], |row| {
                Ok(CoveredField {
                    name: row.get(0)?,
                    ra: row.get(1)?,
                    dec: row.get(2)?,
                    radius_deg: row.get(3)?,
                })
            })
            .map_err(sql)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql)?;
        let max_pm_mas_yr = conn
            .query_row("SELECT value FROM meta WHERE key = 'max_pm_mas_yr'", [], |row| row.get(0))
            .map_err(sql)?;
        Ok(Self { path, fields, max_pm_mas_yr })
    }

    /// Add the store at `SPEARV_GAIA` (default `data/gaia.sqlite`). The store is
    /// built offline and is not in git, so a server without one still starts,
    /// and the catalog route answers 404.
    pub fn register(catalogs: &mut HashMap<SourceId, Arc<dyn CatalogSource>>) {
        let path = std::env::var("SPEARV_GAIA").unwrap_or_else(|_| "data/gaia.sqlite".into());
        match Self::open(&path) {
            Ok(store) => {
                catalogs.insert(store.id(), Arc::new(store));
            }
            Err(err) => eprintln!("no Gaia catalog at {path}: {err}"),
        }
    }

    pub fn fields(&self) -> &[CoveredField] {
        &self.fields
    }

    /// How far any stored star can travel between 2016.0 and the epoch, in degrees.
    fn reach_deg(&self, epoch_mjd: Option<f64>) -> f64 {
        let years = epoch_mjd.map_or(0.0, |mjd| years_since(GAIA_DR3_REF_EPOCH, mjd).abs());
        years * self.max_pm_mas_yr / 3_600_000.0
    }

    fn covers(&self, region: &SkyRegion, radius: f64) -> bool {
        self.fields
            .iter()
            .any(|field| separation_deg(field.ra, field.dec, region.ra, region.dec) + radius <= field.radius_deg)
    }

    fn read(&self, region: &SkyRegion, radius: f64, reach: f64, epoch_mjd: Option<f64>) -> Result<Vec<CatalogRow>> {
        let conn = read_only(&self.path)?;
        let mut statement = conn
            .prepare(
                "SELECT s.source_id, s.ra, s.dec, s.pmra, s.pmdec, s.phot_g_mean_mag, s.ref_epoch
                 FROM star_sky b JOIN star s ON s.source_id = b.id
                 WHERE b.ra_max >= ?1 AND b.ra_min <= ?2 AND b.dec_max >= ?3 AND b.dec_min <= ?4",
            )
            .map_err(sql)?;
        let mut rows = Vec::new();
        for (ra_min, ra_max, dec_min, dec_max) in circle_boxes(region.ra, region.dec, radius + reach) {
            let found = statement
                .query_map(params![ra_min, ra_max, dec_min, dec_max], |row| {
                    let (ra, dec) = at_epoch(
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(6)?,
                        epoch_mjd,
                    );
                    Ok(CatalogRow {
                        id: row.get::<_, i64>(0)?.to_string(),
                        ra,
                        dec,
                        mag: row.get(5)?,
                    })
                })
                .map_err(sql)?;
            for row in found {
                let row = row.map_err(sql)?;
                if separation_deg(region.ra, region.dec, row.ra, row.dec) <= radius {
                    rows.push(row);
                }
            }
        }
        Ok(rows)
    }
}

/// Record the fastest star, so opening a store does not scan it.
pub fn record_max_pm(conn: &Connection) -> rusqlite::Result<usize> {
    let squared: f64 = conn.query_row(
        "SELECT COALESCE(MAX(pmra * pmra + pmdec * pmdec), 0.0) FROM star",
        [],
        |row| row.get(0),
    )?;
    conn.execute("INSERT OR REPLACE INTO meta VALUES ('max_pm_mas_yr', ?1)", [squared.sqrt()])
}

/// A star with no measured proper motion stays where Gaia saw it.
fn at_epoch(
    ra: f64,
    dec: f64,
    pmra: Option<f64>,
    pmdec: Option<f64>,
    ref_epoch: f64,
    epoch_mjd: Option<f64>,
) -> (f64, f64) {
    match (pmra, pmdec, epoch_mjd) {
        (Some(pmra), Some(pmdec), Some(mjd)) => propagate(ra, dec, pmra, pmdec, years_since(ref_epoch, mjd)),
        _ => (ra, dec),
    }
}

#[async_trait]
impl CatalogSource for GaiaStore {
    fn id(&self) -> SourceId {
        SourceId::new("gaia")
    }

    /// Every star inside the square's circumscribed circle at the epoch. A view
    /// the store does not fully hold is `OutsideExposure`, not a short list:
    /// an empty result would read as a patch of sky with no stars in it.
    async fn cone(&self, region: &SkyRegion, epoch_mjd: Option<f64>) -> Result<Vec<CatalogRow>> {
        // The render grid rounds to whole pixels, so it can be wider than `fov_deg`.
        let side_l = region.fov_l_deg.max(f64::from(grid_size(region.fov_l_deg)) * PIXEL_SCALE_DEG);
        let side_h = region.fov_h_deg.max(f64::from(grid_size(region.fov_h_deg)) * PIXEL_SCALE_DEG);
        let radius = if region.is_square() {
            side_l * std::f64::consts::FRAC_1_SQRT_2
        } else {
            0.5 * side_l.hypot(side_h)
        };

        let reach = self.reach_deg(epoch_mjd);
        if !self.covers(region, radius + reach) {
            return Err(Error::OutsideExposure);
        }
        tokio::task::block_in_place(|| self.read(region, radius, reach, epoch_mjd))
    }
}

fn read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(sql)
}

fn sql(err: rusqlite::Error) -> Error {
    Error::Message(format!("gaia store: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use motion::julian_year_to_mjd;
    use spearv_core::png_pixel_to_sky;
    use std::ops::Deref;

    const DAY_2026: f64 = 61041.0; // 2026.0, ten years after the reference epoch.

    /// A real store on disk, removed when the test ends.
    struct TempStore(GaiaStore);

    impl Deref for TempStore {
        type Target = GaiaStore;

        fn deref(&self) -> &GaiaStore {
            &self.0
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0.path);
        }
    }

    type TestStar = (i64, f64, f64, Option<f64>, Option<f64>, Option<f64>);

    /// The reader opens it the way the server does.
    fn store(name: &str, field: (f64, f64, f64), stars: &[TestStar]) -> TempStore {
        let path = std::env::temp_dir().join(format!("spearv-gaia-{}-{name}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO field VALUES ('test', ?1, ?2, ?3)",
            params![field.0, field.1, field.2],
        )
        .unwrap();
        for (id, ra, dec, pmra, pmdec, g) in stars {
            conn.execute(
                "INSERT INTO star VALUES (?1, ?2, ?3, ?4, ?5, ?6, 2016.0)",
                params![id, ra, dec, pmra, pmdec, g],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO star_sky VALUES (?1, ?2, ?2, ?3, ?3)",
                params![id, ra, dec],
            )
            .unwrap();
        }
        record_max_pm(&conn).unwrap();
        drop(conn);
        TempStore(GaiaStore::open(path).unwrap())
    }

    fn view(ra: f64, dec: f64, fov_deg: f64) -> SkyRegion {
        SkyRegion::square(ra, dec, fov_deg)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cone_moves_stars_to_the_view_epoch() {
        let gaia = store("epoch", (100.0, 0.0, 2.0), &[(1, 100.0, 0.0, Some(3600.0), Some(0.0), Some(9.0))]);
        let at_ref = gaia.cone(&view(100.0, 0.0, 0.2), Some(julian_year_to_mjd(2016.0))).await.unwrap();
        assert!((at_ref[0].ra - 100.0).abs() < 1e-9);
        // 3.6 arcsec a year for ten years is 0.01 degrees.
        let later = gaia.cone(&view(100.0, 0.0, 0.2), Some(DAY_2026)).await.unwrap();
        assert!((later[0].ra - 100.00999).abs() < 1e-4, "{}", later[0].ra);
        assert_eq!(later[0].id, "1");
        assert_eq!(later[0].mag, Some(9.0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_epoch_means_the_catalog_position() {
        let gaia = store("none", (100.0, 0.0, 2.0), &[(1, 100.0, 0.0, Some(3600.0), Some(0.0), None)]);
        let rows = gaia.cone(&view(100.0, 0.0, 0.2), None).await.unwrap();
        assert_eq!(rows[0].ra, 100.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_star_without_proper_motion_stays_put() {
        let gaia = store("still", (100.0, 0.0, 2.0), &[(1, 100.0, 0.0, None, None, Some(20.0))]);
        let rows = gaia.cone(&view(100.0, 0.0, 0.2), Some(DAY_2026)).await.unwrap();
        assert_eq!((rows[0].ra, rows[0].dec), (100.0, 0.0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fast_star_can_move_into_the_view() {
        // 0.3 degrees east at 2016, moving west at 108 arcsec a year: 0.3 degrees in ten years.
        let gaia = store("fast", (100.0, 0.0, 2.0), &[(1, 100.3, 0.0, Some(-108_000.0), Some(0.0), None)]);
        assert_eq!(gaia.cone(&view(100.0, 0.0, 0.1), Some(DAY_2026)).await.unwrap().len(), 1);
        assert_eq!(gaia.cone(&view(100.0, 0.0, 0.1), None).await.unwrap().len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_star_in_the_corner_pixel_of_a_rounded_grid_is_found() {
        // fov 0.1 is 59 pixels, 0.1008 degrees: its corner is past the circle of fov alone.
        let (ra, dec) = png_pixel_to_sky(0, 0, 100.0, 0.0, 0.1).unwrap();
        let (ra, dec) = (ra + 0.3 * PIXEL_SCALE_DEG, dec + 0.3 * PIXEL_SCALE_DEG);
        assert!(separation_deg(100.0, 0.0, ra, dec) > 0.1 * std::f64::consts::FRAC_1_SQRT_2);
        let gaia = store("corner", (100.0, 0.0, 2.0), &[(1, ra, dec, None, None, None)]);
        assert_eq!(gaia.cone(&view(100.0, 0.0, 0.1), None).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_across_ra_zero_finds_both_sides() {
        let gaia = store(
            "wrap",
            (0.0, 0.0, 2.0),
            &[(1, 359.95, 0.0, None, None, None), (2, 0.05, 0.0, None, None, None), (3, 180.0, 0.0, None, None, None)],
        );
        let mut ids: Vec<_> = gaia.cone(&view(0.0, 0.0, 0.3), None).await.unwrap().into_iter().map(|row| row.id).collect();
        ids.sort();
        assert_eq!(ids, ["1", "2"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_the_store_does_not_hold_is_an_error_not_an_empty_list() {
        let gaia = store("outside", (100.0, 0.0, 1.0), &[(1, 100.0, 0.0, None, None, None)]);
        assert!(matches!(gaia.cone(&view(120.0, 0.0, 0.1), None).await, Err(Error::OutsideExposure)));
        // Inside by centre, but its corners pass the edge of the field.
        assert!(matches!(gaia.cone(&view(100.9, 0.0, 0.5), None).await, Err(Error::OutsideExposure)));
        assert!(gaia.cone(&view(100.5, 0.0, 0.5), None).await.is_ok());
    }
}
