//! AllWISE Source Catalog, read straight from the public Parquet files.
//!
//! The catalog is one file per order-5 nested HEALPix pixel. A file holds one row
//! group of tens of thousands of rows and 298 columns, roughly 20-40 MB. A query
//! reads each covering file's footer and six column chunks, about 2 MB per file.

mod cache;
mod partition;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use spearv_core::{
    grid_size, CatalogRow, CatalogSource, Error, Result, SkyRegion, SourceId, PIXEL_SCALE_DEG,
};

pub use cache::DiskCache;
use partition::{http_client, read_partition, Counters, Store};

const BASE_URL: &str = "https://nasa-irsa-wise.s3.us-west-2.amazonaws.com/wise/allwise/catalogs/p3as_psd/healpix_k5/wise-allwise.parquet";
const HEALPIX_ORDER: u8 = 5;
/// About 20 partitions, or 40 MB, at worst.
pub const MAX_FOV_DEG: f64 = 4.0;
const CONCURRENT_PARTITIONS: usize = 4;
const QUERY_DEADLINE: Duration = Duration::from_secs(45);
const DEFAULT_CACHE_BYTES: u64 = 1 << 30;

/// One catalog source. `CatalogRow` carries W1 as `mag`; this keeps the rest.
#[derive(Clone, Debug, PartialEq)]
pub struct WiseRow {
    pub designation: String,
    pub ra: f64,
    pub dec: f64,
    /// Apparent RA motion in mas/yr, including the cos(dec) factor.
    pub pmra: Option<f64>,
    /// Dec motion in mas/yr.
    pub pmdec: Option<f64>,
    pub w1mpro: Option<f64>,
    pub w2mpro: Option<f64>,
    pub cc_flags: Option<String>,
}

/// AllWISE positions are reported at epoch 2010.5. Propagating a source to a
/// frame epoch is what lets a high-proper-motion target stay identifiable.
const ALLWISE_EPOCH_MJD: f64 = 55400.0;

fn propagate(
    ra: f64,
    dec: f64,
    pmra: Option<f64>,
    pmdec: Option<f64>,
    epoch_mjd: f64,
) -> (f64, f64) {
    let years = (epoch_mjd - ALLWISE_EPOCH_MJD) / 365.25;
    let cos_dec = dec.to_radians().cos().abs().max(1e-6);
    let delta_ra = pmra.unwrap_or(0.0) * years / (3.6e6 * cos_dec);
    let delta_dec = pmdec.unwrap_or(0.0) * years / 3.6e6;
    (
        (ra + delta_ra).rem_euclid(360.0),
        (dec + delta_dec).clamp(-90.0, 90.0),
    )
}

/// What one query cost. `network_bytes` is what crossed the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryStats {
    pub partitions: u64,
    pub requests: u64,
    pub network_bytes: u64,
    pub cache_hits: u64,
    pub cache_bytes: u64,
}

pub struct ConeResult {
    pub rows: Vec<WiseRow>,
    pub stats: QueryStats,
}

pub struct AllWise {
    store: Arc<Store>,
}

impl AllWise {
    pub fn open(cache: DiskCache) -> Result<Self> {
        Self::with_base_url(BASE_URL.into(), cache)
    }

    /// `SPEARV_WISE_CACHE` is the cache directory, `SPEARV_WISE_CACHE_BYTES` its limit.
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var("SPEARV_WISE_CACHE").unwrap_or_else(|_| "data/wise-cache".into());
        let max_bytes = spearv_core::env::number_opt::<u64>("SPEARV_WISE_CACHE_BYTES")
            .map_err(Error::Message)?
            .unwrap_or(DEFAULT_CACHE_BYTES);
        Self::open(DiskCache::open(dir, max_bytes)?)
    }

    fn with_base_url(base_url: String, cache: DiskCache) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Store {
                client: http_client()?,
                base_url,
                cache: Arc::new(cache),
            }),
        })
    }

    /// Every source that can land in the view, brightest W1 first, unknown W1 last.
    ///
    /// The view is a square, so this is the circle through its corners and some
    /// rows fall outside it. Clip with `sky_to_png_pixel`. Any failed partition
    /// fails the query: a partial catalog looks like empty sky.
    pub async fn cone_rows(&self, region: &SkyRegion) -> Result<ConeResult> {
        let cone = Cone::around(region)?;
        tokio::time::timeout(QUERY_DEADLINE, self.read(cone))
            .await
            .map_err(|_| {
                Error::Upstream(format!(
                    "AllWISE read exceeded {}s",
                    QUERY_DEADLINE.as_secs()
                ))
            })?
    }

    async fn read(&self, cone: Cone) -> Result<ConeResult> {
        let pixels = cone.pixels();
        let partitions = pixels.len() as u64;
        let counters = Arc::new(Counters::default());
        let parts = futures::stream::iter(pixels)
            .map(|pixel| read_partition(self.store.clone(), counters.clone(), pixel, cone))
            .buffer_unordered(CONCURRENT_PARTITIONS)
            .try_collect::<Vec<_>>()
            .await?;
        let mut rows = parts.into_iter().flatten().collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            let w1 = |row: &WiseRow| row.w1mpro.unwrap_or(f64::INFINITY);
            w1(a)
                .total_cmp(&w1(b))
                .then_with(|| a.designation.cmp(&b.designation))
        });
        Ok(ConeResult {
            rows,
            stats: QueryStats {
                partitions,
                requests: counters.requests.load(Ordering::Relaxed),
                network_bytes: counters.network_bytes.load(Ordering::Relaxed),
                cache_hits: counters.cache_hits.load(Ordering::Relaxed),
                cache_bytes: counters.cache_bytes.load(Ordering::Relaxed),
            },
        })
    }
}

#[async_trait]
impl CatalogSource for AllWise {
    fn id(&self) -> SourceId {
        SourceId::new("wise")
    }

    /// `epoch_mjd` propagates AllWISE's 2010.5 positions with its published
    /// proper motions. The same source can then be matched across a multi-year
    /// video, and a high-proper-motion target is not left at its J2000 position.
    async fn cone(&self, region: &SkyRegion, epoch_mjd: Option<f64>) -> Result<Vec<CatalogRow>> {
        let result = self.cone_rows(region).await?;
        Ok(result
            .rows
            .into_iter()
            .map(|row| {
                let (ra, dec) = epoch_mjd
                    .map(|epoch| propagate(row.ra, row.dec, row.pmra, row.pmdec, epoch))
                    .unwrap_or((row.ra, row.dec));
                CatalogRow {
                    id: row.designation,
                    ra,
                    dec,
                    mag: row.w1mpro,
                }
            })
            .collect())
    }
}

/// A circle on the sky, in degrees.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cone {
    ra: f64,
    dec: f64,
    radius: f64,
}

impl Cone {
    /// The circle through the corners of the view. The render grid rounds to
    /// whole pixels, so it can be slightly wider than `fov_deg`.
    fn around(region: &SkyRegion) -> Result<Self> {
        let sky_ok = region.ra.is_finite()
            && region.dec.is_finite()
            && (0.0..=360.0).contains(&region.ra)
            && (-90.0..=90.0).contains(&region.dec);
        if !sky_ok {
            return Err(Error::InvalidSky);
        }
        if [region.fov_l_deg, region.fov_h_deg]
            .into_iter()
            .any(|fov| !fov.is_finite() || fov <= 0.0 || fov > MAX_FOV_DEG)
        {
            return Err(Error::FovOutOfRange { max: MAX_FOV_DEG });
        }
        let side_l = region
            .fov_l_deg
            .max(f64::from(grid_size(region.fov_l_deg)) * PIXEL_SCALE_DEG);
        let side_h = region
            .fov_h_deg
            .max(f64::from(grid_size(region.fov_h_deg)) * PIXEL_SCALE_DEG);
        let radius = if region.is_square() {
            side_l * std::f64::consts::FRAC_1_SQRT_2
        } else {
            0.5 * side_l.hypot(side_h)
        };
        Ok(Self {
            ra: region.ra,
            dec: region.dec,
            radius,
        })
    }

    fn pixels(&self) -> Vec<u64> {
        cdshealpix::nested::cone_coverage_approx(
            HEALPIX_ORDER,
            self.ra.rem_euclid(360.0).to_radians(),
            self.dec.to_radians(),
            self.radius.to_radians(),
        )
        .flat_iter()
        .collect()
    }

    /// Haversine, which stays accurate for separations of arcseconds.
    pub(crate) fn contains(&self, ra: f64, dec: f64) -> bool {
        let half_dra = ((ra - self.ra) / 2.0).to_radians().sin();
        let half_ddec = ((dec - self.dec) / 2.0).to_radians().sin();
        let cosines = self.dec.to_radians().cos() * dec.to_radians().cos();
        let separation = 2.0
            * (half_ddec * half_ddec + cosines * half_dra * half_dra)
                .sqrt()
                .asin();
        separation.to_degrees() <= self.radius
    }
}

#[cfg(test)]
mod tests;
