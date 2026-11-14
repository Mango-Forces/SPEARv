//! SPHEREx Level 2 source: HTTP ranges and CFITSIO decoding.
//!
//! Transport, content identity, cache keys and mission policy stay here. Card,
//! HDU, Rice, image and table decoding is exclusively `spearv_fits` on the
//! pinned CFITSIO build. A layout stores typed metadata only, never offsets
//! into a fabricated HDU tree, so a cached layout cannot fake native state.

/// A file's IMAGE header from its first bytes, for the index-service librarian.
pub mod header {
    use spearv_core::{FlagBits, Poly, Sip, Wcs};

    /// Native-decoded IMAGE header cards, keyed by keyword.
    #[derive(Clone, Debug, Default)]
    pub struct CardMap {
        pub pairs: Vec<(String, String)>,
    }

    impl CardMap {
        pub fn get(&self, key: &str) -> Option<&str> {
            self.pairs
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        }

        pub fn number(&self, key: &str) -> Option<f64> {
            self.get(key).and_then(parse_number)
        }

        pub fn integer(&self, key: &str) -> Option<i64> {
            self.number(key).map(|value| value as i64)
        }

        pub(crate) fn from_header(header: &spearv_fits::Header) -> Self {
            Self {
                pairs: header
                    .cards()
                    .iter()
                    .map(|card| (card.name.clone(), card.value.clone()))
                    .collect(),
            }
        }
    }

    const BUDGET: u64 = 8 * 1024 * 1024;

    /// The cards and sky WCS of the IMAGE extension. The prefix must reach the
    /// end of that header; 64 KiB does for every SPHEREx Level 2 file.
    pub async fn image_header(prefix: &[u8]) -> spearv_core::Result<(CardMap, Wcs)> {
        let completed = spearv_fits::run(
            spearv_fits::Input::Prefix(prefix.to_vec()),
            spearv_fits::Budget::new(BUDGET),
            limits(),
            spearv_fits::Cancellation::default(),
            |fits| {
                fits.move_named("IMAGE", 0)?;
                let axes = fits.image_info()?.axes().to_vec();
                let cards = CardMap::from_header(&fits.header()?);
                let wcs = sky_wcs(&cards, &axes)?;
                Ok((cards, wcs))
            },
        )
        .await
        .map_err(|error| spearv_core::Error::Message(format!("FITS header: {error}")))?;
        Ok(completed.value)
    }

    fn limits() -> spearv_fits::Limits {
        spearv_fits::Limits {
            max_axes: 16,
            max_cards: 8192,
            max_elements: 1,
            max_column_elements: 1,
            page_bytes: 65_536,
            cache_bytes: 65_536,
            native_workspace_bytes: 1024 * 1024,
        }
    }

    /// Primary IMAGE sky WCS. Alternate axes (`CRPIX1A`, `CTYPE1W`, ...) are
    /// ignored. SIP keywords are not alternate-WCS keywords.
    pub(crate) fn sky_wcs(cards: &CardMap, axes: &[u64]) -> spearv_fits::Result<Wcs> {
        if axes.len() != 2 {
            return Err(spearv_fits::Error::Unsupported(
                "IMAGE is not two-dimensional".into(),
            ));
        }
        let ctype1 = cards
            .get("CTYPE1")
            .map(unquote)
            .ok_or_else(|| spearv_fits::Error::Unsupported("CTYPE1 missing".into()))?;
        let ctype2 = cards
            .get("CTYPE2")
            .map(unquote)
            .ok_or_else(|| spearv_fits::Error::Unsupported("CTYPE2 missing".into()))?;
        let proj1 = ctype1.strip_suffix("-SIP").unwrap_or(&ctype1);
        let proj2 = ctype2.strip_suffix("-SIP").unwrap_or(&ctype2);
        if proj1 != "RA---TAN" || proj2 != "DEC--TAN" {
            return Err(spearv_fits::Error::Unsupported(format!(
                "unsupported projection {ctype1}/{ctype2}; only TAN+SIP is implemented"
            )));
        }
        if let Some(lonpole) = cards.number("LONPOLE") {
            if (lonpole - 180.0).abs() > 1e-6 {
                return Err(spearv_fits::Error::Unsupported(format!(
                    "LONPOLE {lonpole} is not 180; the TAN transform is not implemented for that"
                )));
            }
        }
        if let Some(radesys) = cards.get("RADESYS").map(unquote) {
            if radesys != "ICRS" {
                return Err(spearv_fits::Error::Unsupported(format!(
                    "RADESYS {radesys} is not ICRS"
                )));
            }
        }
        let cd = if cards.get("CD1_1").is_some() {
            [
                [
                    cards.number("CD1_1").unwrap_or(1.0),
                    cards.number("CD1_2").unwrap_or(0.0),
                ],
                [
                    cards.number("CD2_1").unwrap_or(0.0),
                    cards.number("CD2_2").unwrap_or(1.0),
                ],
            ]
        } else {
            let cdelt1 = cards.number("CDELT1").unwrap_or(1.0);
            let cdelt2 = cards.number("CDELT2").unwrap_or(1.0);
            [
                [
                    cdelt1 * cards.number("PC1_1").unwrap_or(1.0),
                    cdelt1 * cards.number("PC1_2").unwrap_or(0.0),
                ],
                [
                    cdelt2 * cards.number("PC2_1").unwrap_or(0.0),
                    cdelt2 * cards.number("PC2_2").unwrap_or(1.0),
                ],
            ]
        };
        let a = read_poly(cards, "A")?;
        let b = read_poly(cards, "B")?;
        let ap = read_poly(cards, "AP").ok();
        let bp = read_poly(cards, "BP").ok();
        if ctype1.ends_with("-SIP") && (a.order == 0 && b.order == 0) {
            return Err(spearv_fits::Error::Unsupported(
                "CTYPE claims SIP but A/B orders are missing".into(),
            ));
        }
        Ok(Wcs {
            crpix: [
                cards.number("CRPIX1").unwrap_or(0.0),
                cards.number("CRPIX2").unwrap_or(0.0),
            ],
            crval: [
                cards
                    .number("CRVAL1")
                    .ok_or_else(|| spearv_fits::Error::Unsupported("CRVAL1 missing".into()))?,
                cards
                    .number("CRVAL2")
                    .ok_or_else(|| spearv_fits::Error::Unsupported("CRVAL2 missing".into()))?,
            ],
            cd,
            sip: Sip { a, b, ap, bp },
            naxis: [axes[0] as i32, axes[1] as i32],
        })
    }

    fn read_poly(cards: &CardMap, prefix: &str) -> spearv_fits::Result<Poly> {
        let order = cards
            .integer(&format!("{prefix}_ORDER"))
            .ok_or_else(|| spearv_fits::Error::Unsupported(format!("{prefix}_ORDER missing")))?;
        if !(0..=9).contains(&order) {
            return Err(spearv_fits::Error::Unsupported(format!(
                "{prefix}_ORDER {order} is too high"
            )));
        }
        let mut poly = Poly::new(order as usize);
        for i in 0..=order {
            for j in 0..=(order - i) {
                if let Some(value) = cards.number(&format!("{prefix}_{i}_{j}")) {
                    poly.set(i as usize, j as usize, value);
                }
            }
        }
        Ok(poly)
    }

    /// Bit numbers parsed from a FLAGS header. Names, not numbers, decide policy.
    pub(crate) fn flag_bits(cards: &CardMap) -> FlagBits {
        // QR2 stores MP_TRANSIENT = 0. QR3 stores MSKN0000 = 'TRANSIENT' and
        // omits MP_*. The numbers below are the QR2 dictionary, confirmed on a
        // QR3 file by MSKN cards.
        let mut named = std::collections::HashMap::new();
        for (key, value) in &cards.pairs {
            if let Some(bit) = key.strip_prefix("MP_") {
                if let Ok(number) = value.trim().parse::<u32>() {
                    named.insert(bit.to_string(), number);
                }
            }
            if let Some(rest) = key.strip_prefix("MSKN") {
                if let Ok(number) = rest.parse::<u32>() {
                    named.insert(unquote(value), number);
                }
            }
        }
        let bit = |name: &str, default: u32| named.get(name).copied().unwrap_or(default);
        FlagBits {
            transient: bit("TRANSIENT", 0),
            overflow: bit("OVERFLOW", 1),
            sur_error: bit("SUR_ERROR", 2),
            nonfunc: bit("NONFUNC", 6),
            fullsample: bit("FULLSAMPLE", 12),
            persist: bit("PERSIST", 17),
            outlier: bit("OUTLIER", 19),
            source: bit("SOURCE", 21),
        }
    }

    pub(crate) fn unquote(value: &str) -> String {
        let value = value.trim();
        let value = value
            .strip_prefix('\'')
            .and_then(|inner| inner.strip_suffix('\''))
            .unwrap_or(value);
        value.trim().to_string()
    }

    fn parse_number(value: &str) -> Option<f64> {
        let value = value.trim().replace('D', "E").replace('d', "e");
        value.parse().ok()
    }
}

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use spearv_core::{
    Cutout, Error, Exposure, ExposureGeometry, ExposureId, Filter, FlagBits, ImageSource,
    PixelWindow, Plane, Release, ReleaseId, Result, SkyRegion, SourceId, WavelengthMap, Wcs,
};
use spearv_fits::{
    Budget as FitsBudget, Cancellation, ColumnType, Fits, FloatMode, Input, Limits, ReadAt,
    ReadTrace, Window,
};
use spearv_index::client::IndexClient;
use spearv_index::{release_and_survey, CoverageQuery};

use header::CardMap;

/// The id this source registers under.
pub const ID: &str = "spherex";

/// A layout is reused only for the same object content: its URL and ETag.
type LayoutKey = (String, Option<String>);
type HeaderPages = Arc<[(u64, Arc<[u8]>)]>;

/// Maximum exposures whose 64 KiB header pages stay in RAM at once (~256 MiB).
/// Typed metadata (`wcs`, `wavelength`, `flag_bits`, `size`, ~1 KiB) stays in
/// `entries` for every seen exposure; evicted header pages reload from disk.
const MEMORY_HEADER_PAGES: usize = 1024;

#[derive(Default)]
struct LayoutCache {
    entries: HashMap<LayoutKey, Arc<Layout>>,
    paged: VecDeque<LayoutKey>,
}

impl LayoutCache {
    fn get(&self, key: &LayoutKey) -> Option<Arc<Layout>> {
        self.entries.get(key).map(Arc::clone)
    }

    fn insert(&mut self, key: LayoutKey, layout: Arc<Layout>) {
        if !layout.pages.is_empty() {
            self.paged.retain(|k| k != &key);
            if self.paged.len() >= MEMORY_HEADER_PAGES {
                if let Some(oldest) = self.paged.pop_front() {
                    if let Some(old) = self.entries.get(&oldest) {
                        self.entries.insert(oldest, Arc::new(old.without_pages()));
                    }
                }
            }
            self.paged.push_back(key.clone());
        }
        self.entries.insert(key, layout);
    }
}

pub struct SpherexSource {
    client: Client,
    index: Option<Arc<IndexClient>>,
    cache_dir: Option<PathBuf>,
    /// Request and byte counters for the current process.
    stats: Arc<Stats>,
    /// Typed layouts by URL and ETag. A replaced object has a new ETag and so
    /// a new entry; a URL without an ETag has no pinned content identity and
    /// is never written to disk.
    layouts: Mutex<LayoutCache>,
}

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
}

/// Typed metadata only. Native HDU offsets are re-resolved by CFITSIO on every
/// open, so a cached layout never reconstructs a fake `StoredHdu`.
#[derive(Clone)]
struct Layout {
    wcs: Wcs,
    wavelength: WavelengthMap,
    flag_bits: FlagBits,
    size: u64,
    pages: HeaderPages,
    header_spans: Vec<(u64, u32)>,
    header_bytes: u64,
    header_requests: u64,
}

impl Layout {
    /// A cache entry never claims read cost that a later caller did not spend.
    fn cached(&self) -> Self {
        Self {
            header_bytes: 0,
            header_requests: 0,
            ..self.clone()
        }
    }

    fn without_pages(&self) -> Self {
        Self {
            pages: Arc::from([]),
            ..self.cached()
        }
    }
}

struct LayoutMeta {
    wcs: Wcs,
    wavelength: WavelengthMap,
    flag_bits: FlagBits,
    pages: HeaderPages,
    header_spans: Vec<(u64, u32)>,
}

/// Per-probe request accounting. A range read is counted from the `ReadTrace`
/// in `IoStats`, never by subtracting a shared source counter.
#[derive(Default)]
struct ProbeIo {
    requests: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Clone)]
pub struct HttpFile {
    client: Client,
    url: String,
    stats: Arc<Stats>,
    etag: Option<String>,
    size_hint: Option<u64>,
    pages: HeaderPages,
    probe: Arc<ProbeIo>,
}

impl HttpFile {
    async fn logical_size(&self, cancel: &Cancellation) -> Result<u64> {
        if let Some(size) = self.size_hint {
            return Ok(size);
        }
        self.probe_size(cancel).await
    }

    async fn probe_size(&self, cancel: &Cancellation) -> Result<u64> {
        let mut attempt = 1;
        loop {
            cancel.check().map_err(fits_error)?;
            self.probe.requests.fetch_add(1, Ordering::Relaxed);
            let result = tokio::select! { biased;
                _ = cancel.until_cancelled() => Err(spearv_fits::Error::Cancelled),
                result = self.send(0, 1, None, cancel, None) => result,
            };
            match result {
                Ok((body, total)) => {
                    self.probe
                        .bytes
                        .fetch_add(body.len() as u64, Ordering::Relaxed);
                    return Ok(total);
                }
                Err(error) if error.is_transient() && attempt < RANGE_ATTEMPTS => {
                    wait(cancel, attempt).await.map_err(fits_error)?;
                    attempt += 1;
                }
                Err(error) => return Err(fits_error(error)),
            }
        }
    }

    fn pinned(mut self, size: u64) -> Self {
        self.size_hint = Some(size);
        self
    }

    fn pinned_with(mut self, size: u64, pages: HeaderPages) -> Self {
        self.size_hint = Some(size);
        self.pages = pages;
        self
    }

    /// One HTTP attempt. Validates status, interval, total, content identity and
    /// body bounds; never replaces a range with a full-object response.
    async fn send(
        &self,
        start: u64,
        len: usize,
        expected_total: Option<u64>,
        cancel: &Cancellation,
        trace: Option<&ReadTrace>,
    ) -> spearv_fits::Result<(Vec<u8>, u64)> {
        if len == 0 {
            return Err(spearv_fits::Error::Bounds("empty range request".into()));
        }
        let end = start
            .checked_add(len as u64 - 1)
            .ok_or_else(|| spearv_fits::Error::Bounds("range overflow".into()))?;
        let request = self
            .client
            .get(&self.url)
            .header("Range", format!("bytes={start}-{end}"))
            .timeout(range_timeout(len as u64));
        let request = match &self.etag {
            Some(etag) => request.header("If-Match", etag),
            None => request,
        };
        cancel.check()?;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let mut response = request
            .send()
            .await
            .map_err(|error| transport_error(error, cancel))?;
        let status = response.status();
        if status.as_u16() == 412 {
            return Err(spearv_fits::Error::ContentChanged);
        }
        if status.as_u16() != 206 {
            return Err(if status.as_u16() == 200 {
                spearv_fits::Error::MalformedRange(
                    "full-object 200 response refused in place of an exact range".into(),
                )
            } else if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                spearv_fits::Error::Transient(format!("exact range returned HTTP {status}"))
            } else {
                spearv_fits::Error::MalformedRange(format!("exact range returned HTTP {status}"))
            });
        }
        if let Some(expected) = &self.etag {
            let returned = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok());
            if returned != Some(expected.as_str()) {
                return Err(spearv_fits::Error::ContentChanged);
            }
        }
        let header = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                spearv_fits::Error::MalformedRange("range response had no Content-Range".into())
            })?;
        let (returned_start, returned_end, total) =
            parse_content_range(header).ok_or_else(|| {
                spearv_fits::Error::MalformedRange(format!("bad Content-Range {header}"))
            })?;
        if returned_start != start || returned_end != end {
            return Err(spearv_fits::Error::MalformedRange(format!(
                "range response interval {returned_start}-{returned_end} does not match {start}-{end}"
            )));
        }
        if let Some(expected) = expected_total {
            if total != expected {
                return Err(spearv_fits::Error::ContentChanged);
            }
        }
        if let Some(length) = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            if length != len as u64 {
                return Err(spearv_fits::Error::MalformedRange(format!(
                    "range Content-Length {length} does not match {len}"
                )));
            }
        }
        let mut body = Vec::with_capacity(len);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| transport_error(error, cancel))?
        {
            cancel.check()?;
            if body.len() + chunk.len() > len {
                return Err(spearv_fits::Error::MalformedRange(
                    "range body exceeds the requested length".into(),
                ));
            }
            body.extend_from_slice(&chunk);
            self.stats.bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            if let Some(trace) = trace {
                trace.received(chunk.len());
            }
        }
        if body.len() != len {
            return Err(spearv_fits::Error::ShortRead);
        }
        Ok((body, total))
    }
}

#[async_trait]
impl ReadAt for HttpFile {
    async fn read_at(
        &self,
        start: u64,
        len: usize,
        cancel: &Cancellation,
        trace: &ReadTrace,
    ) -> spearv_fits::Result<Vec<u8>> {
        if len == 0 {
            return Err(spearv_fits::Error::Bounds("zero-length range".into()));
        }
        let size = self.size_hint.ok_or_else(|| {
            spearv_fits::Error::Bounds("range reader has no pinned logical size".into())
        })?;
        start
            .checked_add(len as u64)
            .filter(|end| *end <= size)
            .ok_or_else(|| {
                spearv_fits::Error::Bounds("range is outside the logical object".into())
            })?;
        let mut attempt = 1;
        loop {
            cancel.check()?;
            trace.attempt();
            let result = tokio::select! { biased;
                _ = cancel.until_cancelled() => Err(spearv_fits::Error::Cancelled),
                result = self.send(start, len, Some(size), cancel, Some(trace)) => result,
            };
            match result {
                Ok((bytes, _)) => return Ok(bytes),
                Err(error) if error.is_transient() && attempt < RANGE_ATTEMPTS => {
                    wait(cancel, attempt).await?;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn cached_pages(&self) -> HeaderPages {
        Arc::clone(&self.pages)
    }
}

/// reqwest text can include the URL; transport messages must not. Keep only
/// stable, URL-free categories.
fn transport_error(error: reqwest::Error, cancel: &Cancellation) -> spearv_fits::Error {
    if cancel.check().is_err() {
        return spearv_fits::Error::Cancelled;
    }
    if error.is_timeout() {
        spearv_fits::Error::Transient("range request timed out".into())
    } else if error.is_connect() {
        spearv_fits::Error::Transient("range request could not connect".into())
    } else if error.is_body() {
        spearv_fits::Error::Transient("range response body failed".into())
    } else {
        spearv_fits::Error::Transient("range request failed".into())
    }
}

async fn wait(cancel: &Cancellation, attempt: u64) -> spearv_fits::Result<()> {
    tokio::select! { biased;
        _ = cancel.until_cancelled() => Err(spearv_fits::Error::Cancelled),
        _ = tokio::time::sleep(Duration::from_millis(500 * attempt)) => Ok(()),
    }
}

fn parse_content_range(text: &str) -> Option<(u64, u64, u64)> {
    let interval = text.trim().strip_prefix("bytes ")?;
    let (span, total) = interval.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    Some((
        start.trim().parse().ok()?,
        end.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

impl SpherexSource {
    /// Share the binder's one index client, so the image source and every
    /// extension use one connection pool and one URL.
    pub fn with_index(index: Arc<IndexClient>) -> Result<Self> {
        Ok(Self {
            client: http_client()?,
            index: Some(index),
            cache_dir: std::env::var_os("SPEARV_CACHE").map(PathBuf::from),
            stats: Arc::new(Stats::default()),
            layouts: Mutex::new(LayoutCache::default()),
        })
    }

    /// Read indexed files without querying the index from this source. The
    /// caller supplies the rows.
    pub fn direct() -> Result<Self> {
        Ok(Self {
            client: http_client()?,
            index: None,
            cache_dir: std::env::var_os("SPEARV_CACHE").map(PathBuf::from),
            stats: Arc::new(Stats::default()),
            layouts: Mutex::new(LayoutCache::default()),
        })
    }

    /// Requests sent and bytes received by this source.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.stats.requests.load(Ordering::Relaxed),
            self.stats.bytes.load(Ordering::Relaxed),
        )
    }

    fn index(&self) -> Result<&Arc<IndexClient>> {
        self.index
            .as_ref()
            .ok_or_else(|| Error::Message("this source has no index-service client".into()))
    }

    /// Ranged reads of `exposure`, conditioned on its ETag and pinned to its
    /// size when the index recorded them.
    fn http(&self, exposure: &Exposure) -> HttpFile {
        HttpFile {
            client: self.client.clone(),
            url: exposure.url.clone(),
            stats: Arc::clone(&self.stats),
            etag: exposure.etag.clone().filter(|etag| !etag.is_empty()),
            size_hint: exposure.size.filter(|size| *size > 0),
            pages: Arc::from([]),
            probe: Arc::new(ProbeIo::default()),
        }
    }

    /// An indexed row as the exposure its exact reads are conditioned on.
    fn exact_exposure(row: &spearv_index::ExposureRow) -> Result<Exposure> {
        let etag = row
            .etag
            .clone()
            .filter(|etag| !etag.is_empty())
            .ok_or_else(|| {
                Error::Message(
                    "indexed file has no content ETag; exact discovery is incomplete for this file"
                        .into(),
                )
            })?;
        Ok(Exposure {
            id: ExposureId(format!("{}D{}:{}", row.obsid, row.detector, row.version)),
            source: SourceId::new(ID),
            detector: row.detector,
            mjd: row.mjd_avg,
            release: ReleaseId(row.release.clone()),
            version: row.version.clone(),
            obsid: row.obsid.clone(),
            url: row.url.clone(),
            etag: Some(etag),
            size: row.size.and_then(|size| u64::try_from(size).ok()).filter(|size| *size > 0),
            em_min_um: row.em_min_um,
            em_max_um: row.em_max_um,
        })
    }

    /// Header geometry for an indexed row, without reading its pixels.
    pub async fn exact_geometry(
        &self,
        row: &spearv_index::ExposureRow,
    ) -> Result<ExposureGeometry> {
        let exposure = Self::exact_exposure(row)?;
        let layout = self.layout(&exposure, false).await?;
        Ok(ExposureGeometry {
            wcs: layout.wcs.clone(),
            wavelength: layout.wavelength.clone(),
            header_bytes: layout.header_bytes,
        })
    }

    /// Read a detector window from an indexed row. Every range carries If-Match
    /// for the discovered content, including FLAGS.
    pub async fn exact_window_cutout(
        &self,
        row: &spearv_index::ExposureRow,
        window: &PixelWindow,
    ) -> Result<Cutout> {
        let exposure = Self::exact_exposure(row)?;
        let layout = self.layout(&exposure, true).await?;
        self.read_window(&exposure, &layout, window).await
    }

    async fn layout(&self, exposure: &Exposure, need_pages: bool) -> Result<Arc<Layout>> {
        let file = self.http(exposure);
        let key = (file.url.clone(), file.etag.clone());
        let disk_path = match (&self.cache_dir, &file.etag, file.size_hint) {
            (Some(dir), Some(etag), Some(size)) if size > 1024 * 1024 => {
                Some(disk_layout_path(dir, &file.url, etag, size))
            }
            _ => None,
        };
        let cached = self
            .layouts
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key));
        let layout = if let Some(layout) = cached {
            layout
        } else if let Some(layout) = disk_path.as_deref().and_then(load_disk_layout) {
            let layout = Arc::new(layout);
            if let Ok(mut cache) = self.layouts.lock() {
                cache.insert(key.clone(), Arc::clone(&layout));
            }
            layout
        } else {
            let layout = self.build_layout(file.clone()).await?;
            if let Some(path) = &disk_path {
                save_disk_layout(path, &layout);
            }
            if let Ok(mut cache) = self.layouts.lock() {
                cache.insert(key.clone(), Arc::new(layout.cached()));
            }
            Arc::new(layout)
        };
        if need_pages && layout.pages.is_empty() && layout.size > 64 * 1024 {
            return self
                .populate_header_pages(&file, &key, disk_path.as_deref(), &layout)
                .await;
        }
        Ok(layout)
    }

    async fn build_layout(&self, file: HttpFile) -> Result<Layout> {
        let cancel = Cancellation::default();
        let size = file.logical_size(&cancel).await?;
        let probe_requests = file.probe.requests.load(Ordering::Relaxed);
        let probe_bytes = file.probe.bytes.load(Ordering::Relaxed);
        let reader: Arc<dyn ReadAt> = Arc::new(file.clone().pinned(size));
        let completed = spearv_fits::run(
            Input::Random { size, reader },
            FitsBudget::new(LAYOUT_BUDGET),
            layout_limits(),
            cancel,
            layout_meta,
        )
        .await
        .map_err(fits_error)?;
        let meta = completed.value;
        let cache_pages = file.etag.is_some() && size > 1024 * 1024;
        let pages = if cache_pages {
            meta.pages
        } else {
            Arc::from([])
        };
        let header_spans = if cache_pages {
            meta.header_spans
        } else {
            Vec::new()
        };
        Ok(Layout {
            wcs: meta.wcs,
            wavelength: meta.wavelength,
            flag_bits: meta.flag_bits,
            size,
            pages,
            header_spans,
            header_bytes: completed.io.transferred_bytes + probe_bytes,
            header_requests: completed.io.attempts + probe_requests,
        })
    }

    async fn populate_header_pages(
        &self,
        file: &HttpFile,
        key: &LayoutKey,
        disk_path: Option<&Path>,
        layout: &Layout,
    ) -> Result<Arc<Layout>> {
        let (pages, header_spans, fetched_bytes, fetched_requests) =
            if !layout.header_spans.is_empty() {
                let pinned = file.clone().pinned(layout.size);
                let cancel = Cancellation::default();
                let mut join = tokio::task::JoinSet::new();
                for (index, &(offset, len)) in layout.header_spans.iter().enumerate() {
                    let pinned = pinned.clone();
                    let cancel = cancel.clone();
                    join.spawn(async move {
                        let trace = ReadTrace::default();
                        let bytes = pinned
                            .read_at(offset, len as usize, &cancel, &trace)
                            .await
                            .map_err(fits_error)?;
                        Ok::<_, Error>((
                            index,
                            offset,
                            Arc::<[u8]>::from(bytes),
                            trace.bytes(),
                            trace.attempts(),
                        ))
                    });
                }
                let mut results = Vec::with_capacity(layout.header_spans.len());
                while let Some(joined) = join.join_next().await {
                    let item = match joined {
                        Ok(Ok(item)) => item,
                        Ok(Err(error)) => {
                            cancel.cancel();
                            return Err(error);
                        }
                        Err(_) => {
                            cancel.cancel();
                            return Err(Error::Message("SPHEREx header fetch task failed".into()));
                        }
                    };
                    results.push(item);
                }
                results.sort_unstable_by_key(|(index, _, _, _, _)| *index);
                let mut total_bytes = 0u64;
                let mut total_requests = 0u64;
                let pages = results
                    .into_iter()
                    .map(|(_, offset, bytes, read_bytes, attempts)| {
                        total_bytes += read_bytes;
                        total_requests += attempts;
                        (offset, bytes)
                    })
                    .collect::<Vec<_>>();
                (
                    HeaderPages::from(pages),
                    layout.header_spans.clone(),
                    total_bytes,
                    total_requests,
                )
            } else {
                let size = layout.size;
                let cancel = Cancellation::default();
                let reader: Arc<dyn ReadAt> = Arc::new(file.clone().pinned(size));
                let completed = spearv_fits::run(
                    Input::Random { size, reader },
                    FitsBudget::new(LAYOUT_BUDGET),
                    layout_limits(),
                    cancel,
                    move |fits| {
                        fits.move_named("IMAGE", 0)?;
                        fits.move_named("FLAGS", 0)?;
                        fits.move_named("VARIANCE", 0)?;
                        fits.move_named("ZODI", 0)?;
                        let pages = fits.cached_pages();
                        let spans = compact_spans(fits.read_spans(), size);
                        Ok((pages, spans))
                    },
                )
                .await
                .map_err(fits_error)?;
                let (pages, spans) = completed.value;
                (
                    pages,
                    spans,
                    completed.io.transferred_bytes,
                    completed.io.attempts,
                )
            };
        let updated = Layout {
            wcs: layout.wcs.clone(),
            wavelength: layout.wavelength.clone(),
            flag_bits: layout.flag_bits,
            size: layout.size,
            pages,
            header_spans,
            header_bytes: layout.header_bytes + fetched_bytes,
            header_requests: layout.header_requests + fetched_requests,
        };
        if file.etag.is_some() && layout.size > 1024 * 1024 {
            if layout.header_spans.is_empty() && !updated.header_spans.is_empty() {
                if let Some(path) = disk_path {
                    save_disk_layout(path, &updated);
                }
            }
            if let Ok(mut cache) = self.layouts.lock() {
                cache.insert(key.clone(), Arc::new(updated.cached()));
            }
        }
        Ok(Arc::new(updated))
    }
}

const RANGE_ATTEMPTS: u64 = 3;
const LAYOUT_BUDGET: u64 = 64 * 1024 * 1024;
const CUTOUT_BUDGET: u64 = 384 * 1024 * 1024;

/// Twelve seconds suits a header. A page is a few MiB, and with some 24
/// requests sharing about 28 MB/s it takes a few seconds, so the limit grows
/// with the range to leave room for a slower link. 256 KiB/s is the rate it
/// then allows.
fn range_timeout(bytes: u64) -> Duration {
    Duration::from_secs(12 + bytes / (256 * 1024))
}

fn http_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(256)
        .user_agent("spearv/0.1 (anonymous SPHEREx cutout)")
        .build()
        .map_err(|err| Error::Upstream(err.to_string()))
}

fn layout_limits() -> Limits {
    Limits {
        max_axes: 16,
        max_cards: 8192,
        max_elements: 1,
        max_column_elements: 8192,
        page_bytes: 64 * 1024,
        cache_bytes: 1024 * 1024,
        native_workspace_bytes: 8 * 1024 * 1024,
    }
}

fn cutout_limits(page_bytes: usize) -> Limits {
    Limits {
        max_axes: 16,
        max_cards: 8192,
        max_elements: 4_194_304,
        max_column_elements: 65_536,
        page_bytes,
        cache_bytes: page_bytes.saturating_add(1024 * 1024),
        native_workspace_bytes: 16 * 1024 * 1024,
    }
}

fn compact_spans(spans: Vec<(u64, u64)>, size: u64) -> Vec<(u64, u32)> {
    if spans.is_empty() || spans.len() > 16 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut total = 0u64;
    for (offset, len) in spans {
        let Ok(len32) = u32::try_from(len) else {
            return Vec::new();
        };
        if len32 == 0 || offset.checked_add(len).is_none_or(|end| end > size) {
            return Vec::new();
        }
        total += len;
        if total > 512 * 1024 {
            return Vec::new();
        }
        out.push((offset, len32));
    }
    out
}

fn layout_meta(fits: &mut Fits) -> spearv_fits::Result<LayoutMeta> {
    fits.move_named("IMAGE", 0)?;
    let info = fits.image_info()?;
    let axes = info.axes().to_vec();
    let cards = CardMap::from_header(&fits.header()?);
    let wcs = header::sky_wcs(&cards, &axes)?;
    fits.move_named("FLAGS", 0)?;
    let flag_bits = header::flag_bits(&CardMap::from_header(&fits.header()?));
    fits.move_named("VARIANCE", 0)?;
    fits.move_named("ZODI", 0)?;
    let pages = fits.cached_pages();
    let header_spans = compact_spans(fits.read_spans(), u64::MAX);
    let wavelength = read_wavelength(fits, &cards)?;
    Ok(LayoutMeta {
        wcs,
        wavelength,
        flag_bits,
        pages,
        header_spans,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskMeta {
    wcs: Wcs,
    wavelength: WavelengthMap,
    flag_bits: FlagBits,
    size: u64,
    #[serde(default)]
    header_spans: Vec<(u64, u32)>,
}

fn disk_layout_path(dir: &Path, url: &str, etag: &str, size: u64) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("cfitsio-v1:{url}:{etag}:{size}").as_bytes());
    dir.join("cfitsio-layouts-v2")
        .join(format!("{}.bin", hex::encode(hasher.finalize())))
}

fn load_disk_layout(path: &Path) -> Option<Layout> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 8 {
        return None;
    }
    let meta_len = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    let mut at = 4usize.checked_add(meta_len)?;
    let meta: DiskMeta = serde_json::from_slice(bytes.get(4..at)?).ok()?;
    let count = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
    at += 4;
    if count > 16 {
        return None;
    }
    let mut pages = Vec::with_capacity(count);
    for _ in 0..count {
        let offset = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
        let len = u32::from_le_bytes(bytes.get(at + 8..at + 12)?.try_into().ok()?) as usize;
        at += 12;
        let slice = bytes.get(at..at.checked_add(len)?)?;
        at += len;
        pages.push((offset, Arc::<[u8]>::from(slice)));
    }
    if at != bytes.len() {
        return None;
    }
    let header_spans = compact_spans(
        meta.header_spans
            .into_iter()
            .map(|(offset, len)| (offset, u64::from(len)))
            .collect(),
        meta.size,
    );
    Some(Layout {
        wcs: meta.wcs,
        wavelength: meta.wavelength,
        flag_bits: meta.flag_bits,
        size: meta.size,
        pages: pages.into(),
        header_spans,
        header_bytes: 0,
        header_requests: 0,
    })
}

fn save_disk_layout(path: &Path, layout: &Layout) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let meta = DiskMeta {
        wcs: layout.wcs.clone(),
        wavelength: layout.wavelength.clone(),
        flag_bits: layout.flag_bits,
        size: layout.size,
        header_spans: layout.header_spans.clone(),
    };
    let Ok(meta_bytes) = serde_json::to_vec(&meta) else { return };
    let Ok(meta_len) = u32::try_from(meta_bytes.len()) else { return };
    // Typed metadata only: header pages stay in the bounded memory cache.
    // Persisting them costs about 250 KiB per exposure, which a full-sky
    // index would turn into hundreds of gigabytes.
    let mut out = Vec::with_capacity(8 + meta_bytes.len());
    out.extend_from_slice(&meta_len.to_le_bytes());
    out.extend_from_slice(&meta_bytes);
    out.extend_from_slice(&0u32.to_le_bytes());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Decode the WCS-WAVE control-point table with native table reads. `TDIM`
/// must agree with `NAXIS`-derived counts when present, and the value axis must
/// be one or two scalar components per grid point.
fn read_wavelength(fits: &mut Fits, image_cards: &CardMap) -> spearv_fits::Result<WavelengthMap> {
    fits.move_named("WCS-WAVE", 0)?;
    let (columns, rows) = fits.table_shape()?;
    if columns < 3 {
        return Err(spearv_fits::Error::Unsupported(format!(
            "WCS-WAVE has {columns} columns; 3 are required"
        )));
    }
    if rows != 1 {
        return Err(spearv_fits::Error::Unsupported(format!(
            "WCS-WAVE has {rows} rows; exactly 1 is required"
        )));
    }
    let x = fits.column_info(1)?;
    let y = fits.column_info(2)?;
    let values = fits.column_info(3)?;
    if x.kind != ColumnType::I32 || y.kind != ColumnType::I32 {
        return Err(spearv_fits::Error::Unsupported(
            "WCS-WAVE X/Y columns must be 32-bit integers".into(),
        ));
    }
    if values.kind != ColumnType::F32 {
        return Err(spearv_fits::Error::Unsupported(
            "WCS-WAVE VALUES must be 32-bit floats".into(),
        ));
    }
    if x.variable || y.variable || values.variable {
        return Err(spearv_fits::Error::Unsupported(
            "WCS-WAVE columns must be fixed-width".into(),
        ));
    }
    let nx = usize::try_from(x.repeat)
        .map_err(|_| spearv_fits::Error::Unsupported("WCS-WAVE X count overflow".into()))?;
    let ny = usize::try_from(y.repeat)
        .map_err(|_| spearv_fits::Error::Unsupported("WCS-WAVE Y count overflow".into()))?;
    let count = usize::try_from(values.repeat)
        .map_err(|_| spearv_fits::Error::Unsupported("WCS-WAVE count overflow".into()))?;
    if nx < 2 || ny < 2 {
        return Err(spearv_fits::Error::Unsupported(
            "WCS-WAVE needs at least two control points per axis".into(),
        ));
    }
    if count != nx.saturating_mul(ny).saturating_mul(2) {
        return Err(spearv_fits::Error::Unsupported(format!(
            "WCS-WAVE VALUES count {count} does not match {nx} x {ny} x 2"
        )));
    }
    validate_tdim(&x, &[x.repeat])?;
    validate_tdim(&y, &[y.repeat])?;
    validate_tdim(&values, &[2, x.repeat, y.repeat])?;
    if let Some(unit) = fits.key_string("TUNIT3")? {
        if !micrometers(&unit) {
            return Err(spearv_fits::Error::Unsupported(format!(
                "WCS-WAVE TUNIT3 {:?} is not micrometers",
                &*unit
            )));
        }
    }
    let xs = fits.read_column_f64(1, 1, 1, nx)?;
    let ys = fits.read_column_f64(2, 1, 1, ny)?;
    let values = fits.read_column_f64(3, 1, 1, count)?;
    if xs.valid().contains(&0) || ys.valid().contains(&0) || values.valid().contains(&0) {
        return Err(spearv_fits::Error::Unsupported(
            "WCS-WAVE contains null or invalid control points".into(),
        ));
    }
    // PV indices are 1-based into the fastest TDIM axis. They live on the image WCS, not the table.
    let wave_at = image_cards.integer("PV1_3W").unwrap_or(1);
    let band_at = image_cards.integer("PV2_3W").unwrap_or(2);
    if !(1..=2).contains(&wave_at) || !(1..=2).contains(&band_at) {
        return Err(spearv_fits::Error::Unsupported(
            "PV*_3W is outside the VALUES vector".into(),
        ));
    }
    let mut wavelength = Vec::with_capacity(nx * ny);
    let mut bandwidth = Vec::with_capacity(nx * ny);
    for index in 0..nx * ny {
        let base = 2 * index;
        wavelength.push(values.values()[base + wave_at as usize - 1] as f32);
        bandwidth.push(values.values()[base + band_at as usize - 1] as f32);
    }
    Ok(WavelengthMap {
        x: xs.values().to_vec(),
        y: ys.values().to_vec(),
        wavelength,
        bandwidth,
        nx,
        ny,
    })
}

fn validate_tdim(info: &spearv_fits::ColumnInfo, expected: &[u64]) -> spearv_fits::Result<()> {
    let dimensions = info.dimensions();
    if dimensions.is_empty() {
        return Ok(());
    }
    if dimensions != expected {
        return Err(spearv_fits::Error::Unsupported(format!(
            "WCS-WAVE TDIM {dimensions:?} does not match {expected:?}"
        )));
    }
    Ok(())
}

fn micrometers(text: &str) -> bool {
    matches!(
        text.trim(),
        "" | "um" | "micron" | "microns" | "micrometer" | "micrometers" | "µm"
    )
}

fn plane<T: Copy>(window: Window, values: &[T]) -> Plane<T> {
    Plane {
        width: window.width as u32,
        height: window.height as u32,
        data: values.to_vec(),
    }
}

fn fits_error(error: spearv_fits::Error) -> Error {
    match error {
        spearv_fits::Error::ContentChanged => Error::Message(
            "SPHEREx object changed since discovery; refresh the table before previewing it".into(),
        ),
        spearv_fits::Error::Cancelled => Error::Message("SPHEREx read cancelled".into()),
        spearv_fits::Error::Transient(message) | spearv_fits::Error::MalformedRange(message) => {
            Error::Upstream(format!("FITS: {message}"))
        }
        spearv_fits::Error::ShortRead => Error::Upstream("FITS: short exact range".into()),
        spearv_fits::Error::Deadline => Error::Upstream("FITS: range deadline expired".into()),
        spearv_fits::Error::Worker(message) => Error::Upstream(format!("FITS: {message}")),
        other => Error::Message(format!("FITS: {other}")),
    }
}

#[async_trait]
impl ImageSource for SpherexSource {
    fn id(&self) -> SourceId {
        SourceId::new(ID)
    }

    async fn releases(&self) -> Result<Vec<Release>> {
        let index = self.index()?;
        Ok(index
            .releases()
            .await?
            .releases
            .into_iter()
            .map(|row| Release {
                id: ReleaseId(row.release),
                newest_mjd: row.newest_mjd,
                newest_period: row.newest_period,
                last_seen_ms: row.last_listed_ms,
            })
            .collect())
    }

    async fn coverage(&self, region: &SkyRegion, filter: &Filter) -> Result<Vec<Exposure>> {
        let index = self.index()?;
        let (release, survey) = release_and_survey(
            filter
                .release
                .as_ref()
                .map(|id| id.0.as_str())
                .unwrap_or("qr3"),
        );
        let query = CoverageQuery {
            release: Some(release),
            survey,
            wl_min: filter.wavelength_um.map(|wl| wl.0),
            wl_max: filter.wavelength_um.map(|wl| wl.1),
            ..CoverageQuery::view(
                region.ra.rem_euclid(360.0),
                region.dec,
                region.envelope_deg(),
            )
        };
        Ok(index
            .coverage(&query)
            .await?
            .exposures
            .into_iter()
            .map(|row| Exposure {
                id: ExposureId(format!("{}D{}:{}", row.obsid, row.detector, row.version)),
                source: SourceId::new(ID),
                detector: row.detector,
                mjd: row.mjd_avg,
                release: ReleaseId(row.release),
                version: row.version,
                obsid: row.obsid,
                url: row.url,
                etag: row.etag,
                size: row.size.and_then(|size| u64::try_from(size).ok()),
                em_min_um: row.em_min_um,
                em_max_um: row.em_max_um,
            })
            .collect())
    }

    async fn cutout(&self, exposure: &Exposure, region: &SkyRegion) -> Result<Cutout> {
        let layout = self.layout(exposure, true).await?;
        let window = PixelWindow::for_region(&layout.wcs, region)?;
        self.read_window(exposure, &layout, &window).await
    }

    async fn geometry(&self, exposure: &Exposure) -> Result<ExposureGeometry> {
        let layout = self.layout(exposure, false).await?;
        Ok(ExposureGeometry {
            wcs: layout.wcs.clone(),
            wavelength: layout.wavelength.clone(),
            header_bytes: layout.header_bytes,
        })
    }

    async fn window_cutout(&self, exposure: &Exposure, window: &PixelWindow) -> Result<Cutout> {
        let layout = self.layout(exposure, true).await?;
        self.read_window(exposure, &layout, window).await
    }
}

impl SpherexSource {
    async fn read_window(&self, exposure: &Exposure, layout: &Layout, window: &PixelWindow) -> Result<Cutout> {
        let file = self.http(exposure);
        if window.x0 < 0 || window.y0 < 0 || window.width <= 0 || window.height <= 0 {
            return Err(Error::Message("pixel window is outside the image".into()));
        }
        let clipped = window.clipped;
        let window = Window {
            x: window.x0 as u64,
            y: window.y0 as u64,
            width: window.width as u64,
            height: window.height as u64,
        };
        // One page should cover the rows this window touches. That keeps a
        // 256-row mosaic tile at a handful of requests instead of dozens,
        // without reading 4 MiB for a tiny preview window.
        let row_bytes = u64::from(layout.wcs.naxis[0].max(1) as u32) * 4;
        let page_bytes = row_bytes
            .saturating_mul(window.height)
            .saturating_add(5_760)
            .clamp(64 * 1024, 16 * 1024 * 1024) as usize;
        let header_page_bytes = (64 * 1024).min(page_bytes);
        let reader: Arc<dyn ReadAt> =
            Arc::new(file.pinned_with(layout.size, Arc::clone(&layout.pages)));
        let budget = FitsBudget::new(CUTOUT_BUDGET);
        let limits = cutout_limits(page_bytes);
        let size = layout.size;
        let read_float = |name: &'static str| {
            let reader = Arc::clone(&reader);
            let budget = budget.clone();
            let limits = limits.clone();
            async move {
                spearv_fits::run(
                    Input::Random { size, reader },
                    budget,
                    limits,
                    Cancellation::default(),
                    move |fits| {
                        fits.set_read_page_bytes(header_page_bytes)?;
                        fits.move_named(name, 0)?;
                        fits.set_read_page_bytes(page_bytes)?;
                        let values = fits.read_f32(window, FloatMode::Automatic)?;
                        Ok(plane(window, values.values()))
                    },
                )
                .await
                .map_err(fits_error)
            }
        };
        let read_flags = {
            let reader = Arc::clone(&reader);
            let budget = budget.clone();
            let limits = limits.clone();
            async move {
                spearv_fits::run(
                    Input::Random { size, reader },
                    budget,
                    limits,
                    Cancellation::default(),
                    move |fits| {
                        fits.set_read_page_bytes(header_page_bytes)?;
                        fits.move_named("FLAGS", 0)?;
                        fits.set_read_page_bytes(page_bytes)?;
                        let flags = fits.read_flags(window)?;
                        let valid = flags
                            .valid()
                            .contains(&0)
                            .then(|| flags.valid().iter().map(|valid| *valid != 0).collect());
                        Ok((plane(window, flags.values()), valid))
                    },
                )
                .await
                .map_err(fits_error)
            }
        };
        let (image, variance, zodi, flags) = tokio::try_join!(
            read_float("IMAGE"),
            read_float("VARIANCE"),
            read_float("ZODI"),
            read_flags,
        )?;
        let transferred_bytes = image.io.transferred_bytes
            + variance.io.transferred_bytes
            + zodi.io.transferred_bytes
            + flags.io.transferred_bytes;
        let attempts =
            image.io.attempts + variance.io.attempts + zodi.io.attempts + flags.io.attempts;
        let (flags, flags_valid) = flags.value;
        Ok(Cutout {
            image: image.value,
            variance: variance.value,
            flags,
            flags_valid,
            zodi: Some(zodi.value),
            wcs: layout.wcs.clone(),
            wavelength: layout.wavelength.clone(),
            origin_fits: (window.x as i32 + 1, window.y as i32 + 1),
            clipped,
            flag_bits: layout.flag_bits,
            bytes_read: layout.header_bytes + transferred_bytes,
            s3_requests: layout.header_requests + attempts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_range_reads_are_conditioned_and_changed_content_is_not_retried() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let app = Router::new().route(
            "/file",
            get(move |headers: HeaderMap| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push((
                        headers
                            .get("if-match")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_string(),
                        headers.get("range").unwrap().to_str().unwrap().to_string(),
                    ));
                    (StatusCode::PRECONDITION_FAILED, "changed")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let file = HttpFile {
            client: Client::new(),
            stats: Arc::new(Stats::default()),
            url: format!("http://{addr}/file"),
            etag: Some("\"stale\"".into()),
            size_hint: Some(1000),
            pages: Arc::from([]),
            probe: Arc::new(ProbeIo::default()),
        };
        let cancel = Cancellation::default();
        let trace = ReadTrace::default();
        let error = file
            .pinned(1000)
            .read_at(100, 100, &cancel, &trace)
            .await
            .unwrap_err();
        assert!(matches!(error, spearv_fits::Error::ContentChanged));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("\"stale\"".into(), "bytes=100-199".into())]
        );
        server.abort();
    }

    fn card(text: &str) -> String {
        let mut line = text.to_string();
        while line.len() < 80 {
            line.push(' ');
        }
        line.truncate(80);
        line
    }

    fn block(cards: &[String]) -> Vec<u8> {
        let mut text = cards.concat();
        while !text.len().is_multiple_of(2880) {
            text.push(' ');
        }
        text.into_bytes()
    }

    fn picture_hdu(name: &str, bitpix: i32, extra: &[String], data: Vec<u8>) -> Vec<u8> {
        let mut cards = vec![
            card("XTENSION= 'IMAGE   '"),
            card(&format!("BITPIX  = {bitpix:>20}")),
            card("NAXIS   =                    2"),
            card("NAXIS1  =                    4"),
            card("NAXIS2  =                    3"),
            card("PCOUNT  =                    0"),
            card("GCOUNT  =                    1"),
            card(&format!("EXTNAME = '{name}'")),
        ];
        cards.extend(extra.iter().cloned());
        cards.push(card("END"));
        let mut out = block(&cards);
        out.extend(data);
        while !out.len().is_multiple_of(2880) {
            out.push(0);
        }
        out
    }

    fn image_cards() -> Vec<String> {
        [
            "CTYPE1  = 'RA---TAN'".to_string(),
            "CTYPE2  = 'DEC--TAN'".to_string(),
            "CRPIX1  =                  2.0".into(),
            "CRPIX2  =                  2.0".into(),
            "CRVAL1  =                 10.0".into(),
            "CRVAL2  =                 20.0".into(),
            "CD1_1   =               -0.001".into(),
            "CD1_2   =                  0.0".into(),
            "CD2_1   =                  0.0".into(),
            "CD2_2   =                0.001".into(),
            "A_ORDER =                    0".into(),
            "B_ORDER =                    0".into(),
            "DETECTOR=                    1".into(),
            "MJD-AVG =              60000.0".into(),
            "BUNIT   = 'MJy/sr  '".into(),
            "PV1_3W  =                    1".into(),
            "PV2_3W  =                    2".into(),
        ]
        .into_iter()
        .map(|text| card(&text))
        .collect()
    }

    fn wave_table_hdu() -> Vec<u8> {
        let cards = vec![
            card("XTENSION= 'BINTABLE'"),
            card("BITPIX  =                    8"),
            card("NAXIS   =                    2"),
            card("NAXIS1  =                   96"),
            card("NAXIS2  =                    1"),
            card("PCOUNT  =                    0"),
            card("GCOUNT  =                    1"),
            card("TFIELDS =                    3"),
            card("TTYPE1  = 'X       '"),
            card("TFORM1  = '3J      '"),
            card("TDIM1   = '(3)'"),
            card("TTYPE2  = 'Y       '"),
            card("TFORM2  = '3J      '"),
            card("TDIM2   = '(3)'"),
            card("TTYPE3  = 'VALUES  '"),
            card("TFORM3  = '18E     '"),
            card("TDIM3   = '(2,3,3)'"),
            card("EXTNAME = 'WCS-WAVE'"),
            card("END"),
        ];
        let mut out = block(&cards);
        for value in [1_i32, 2, 3] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in [1_i32, 2, 3] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for iy in 0..3 {
            for ix in 0..3 {
                let wave = 1.0 + 0.1 * iy as f32 + 0.01 * ix as f32;
                out.extend_from_slice(&wave.to_be_bytes());
                out.extend_from_slice(&0.05_f32.to_be_bytes());
            }
        }
        while !out.len().is_multiple_of(2880) {
            out.push(0);
        }
        out
    }

    fn synthetic_fits() -> Vec<u8> {
        let primary = block(&[
            card("SIMPLE  =                    T"),
            card("BITPIX  =                    8"),
            card("NAXIS   =                    0"),
            card("EXTEND  =                    T"),
            card("END"),
        ]);
        let mut file = primary;
        let mut image = Vec::new();
        for value in 1..=12 {
            image.extend_from_slice(&(value as f32).to_be_bytes());
        }
        file.extend(picture_hdu("IMAGE", -32, &image_cards(), image));
        let variance = (0..12)
            .flat_map(|_| 0.5_f32.to_be_bytes())
            .collect::<Vec<_>>();
        file.extend(picture_hdu("VARIANCE", -32, &[], variance));
        let zodi = (0..12)
            .flat_map(|_| 0.1_f32.to_be_bytes())
            .collect::<Vec<_>>();
        file.extend(picture_hdu("ZODI", -32, &[], zodi));
        let mut flags = Vec::new();
        for value in [0_i32, 1, 1 << 21, 0, 0, 1 << 21, i32::MIN, 0, 0, 0, 0, 2] {
            flags.extend_from_slice(&value.to_be_bytes());
        }
        file.extend(picture_hdu(
            "FLAGS",
            32,
            &[card("BLANK   =            -2147483648")],
            flags,
        ));
        file.extend(wave_table_hdu());
        file
    }

    async fn serve(
        file: Vec<u8>,
        etag: &'static str,
    ) -> (String, Arc<Mutex<Vec<(Option<String>, String)>>>) {
        use axum::{
            http::{HeaderMap, HeaderValue, StatusCode},
            routing::get,
            Router,
        };
        let bytes = Arc::new(file);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = (bytes, seen.clone(), etag);
        let app = Router::new().route(
            "/file",
            get(move |headers: HeaderMap| {
                let (bytes, seen, etag) = state.clone();
                async move {
                    let if_match = headers
                        .get("if-match")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    let range = headers
                        .get("range")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("bytes=0-")
                        .to_string();
                    seen.lock().unwrap().push((if_match.clone(), range.clone()));
                    if let Some(if_match) = if_match {
                        if if_match != etag {
                            return (
                                StatusCode::PRECONDITION_FAILED,
                                HeaderMap::new(),
                                Vec::new(),
                            );
                        }
                    }
                    let interval = range
                        .strip_prefix("bytes=")
                        .unwrap_or("0-")
                        .split(',')
                        .next()
                        .unwrap_or("0-");
                    let (first, last) = interval.split_once('-').unwrap_or(("0", "0"));
                    let start = first.parse::<usize>().unwrap_or(0);
                    let end = last
                        .parse::<usize>()
                        .unwrap_or(bytes.len().saturating_sub(1))
                        .min(bytes.len().saturating_sub(1));
                    let body = bytes[start..=end].to_vec();
                    let mut out = HeaderMap::new();
                    out.insert(
                        "content-range",
                        HeaderValue::from_str(&format!("bytes {start}-{end}/{}", bytes.len()))
                            .unwrap(),
                    );
                    out.insert("etag", HeaderValue::from_str(etag).unwrap());
                    (StatusCode::PARTIAL_CONTENT, out, body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/file"), seen)
    }

    fn source() -> SpherexSource {
        SpherexSource::with_index(Arc::new(IndexClient::new("http://127.0.0.1:9").unwrap())).unwrap()
    }

    fn exposure(url: &str, etag: Option<&str>, size: Option<u64>) -> Exposure {
        Exposure {
            id: ExposureId("2025W34_2B_0501_1D2:v".into()),
            source: SourceId::new(ID),
            detector: 1,
            mjd: 60_000.0,
            release: ReleaseId("qr3".into()),
            version: "v".into(),
            obsid: "o".into(),
            url: url.into(),
            etag: etag.map(str::to_string),
            size,
            em_min_um: 0.75,
            em_max_um: 1.1,
        }
    }

    const WINDOW: PixelWindow = PixelWindow { x0: 1, y0: 1, width: 2, height: 2, clipped: true };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_window_decodes_image_variance_zodi_and_flags_through_cfitsio() {
        let (url, seen) = serve(synthetic_fits(), "\"v1\"").await;
        let source = source();
        let exposure = exposure(&url, None, None);
        let geometry = source.geometry(&exposure).await.unwrap();
        assert_eq!((geometry.wavelength.nx, geometry.wavelength.ny), (3, 3));
        assert_eq!(geometry.wavelength.wavelength[0], 1.0);
        assert_eq!(geometry.wavelength.wavelength[3], 1.1);
        assert!(geometry.header_bytes > 0);
        let cutout = source.window_cutout(&exposure, &WINDOW).await.unwrap();
        assert_eq!(cutout.image.data, vec![6.0, 7.0, 10.0, 11.0]);
        assert_eq!(cutout.variance.data, vec![0.5; 4]);
        let zodi = cutout.zodi.as_ref().unwrap();
        assert!(zodi.data.iter().all(|value| (*value - 0.1).abs() < 1e-7));
        assert_eq!(cutout.flags.data, vec![1 << 21, 0, 0, 0]);
        assert_eq!(cutout.flags_valid, Some(vec![true, false, true, true]));
        assert!(cutout.clipped, "the caller window clipping flag is preserved");
        assert_eq!(cutout.origin_fits, (2, 2));
        assert!(cutout.bytes_read > 0);
        assert!(cutout.s3_requests > 0);
        assert!(seen.lock().unwrap().iter().all(|(if_match, _)| if_match.is_none()), "no ETag, no If-Match");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_indexed_etag_conditions_every_range_and_keys_the_layout() {
        let file = synthetic_fits();
        let size = file.len() as u64;
        let (url, seen) = serve(file, "\"v1\"").await;
        let source = source();
        let current = exposure(&url, Some("\"v1\""), Some(size));
        let first = source.geometry(&current).await.unwrap();
        assert!(first.header_bytes > 0);
        let second = source.geometry(&current).await.unwrap();
        assert_eq!(second.header_bytes, 0, "the typed layout came from memory");
        let cutout = source.window_cutout(&current, &WINDOW).await.unwrap();
        assert_eq!(cutout.image.data, vec![6.0, 7.0, 10.0, 11.0]);
        assert_eq!(cutout.flags_valid, Some(vec![true, false, true, true]));
        {
            let seen = seen.lock().unwrap();
            assert!(
                !seen.is_empty() && seen.iter().all(|(if_match, _)| if_match.as_deref() == Some("\"v1\"")),
                "every range is conditioned: {seen:?}"
            );
        }
        let replaced = exposure(&url, Some("\"v2\""), Some(size));
        let error = source.geometry(&replaced).await.unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
    }

    #[tokio::test]
    async fn a_full_object_200_is_refused_without_retry() {
        use axum::{http::StatusCode, routing::get, Router};
        let requests = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        let app = Router::new().route(
            "/file",
            get(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::OK, "the whole object")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let file = HttpFile {
            client: Client::new(),
            stats: Arc::new(Stats::default()),
            url: format!("http://{addr}/file"),
            etag: None,
            size_hint: Some(10),
            pages: Arc::from([]),
            probe: Arc::new(ProbeIo::default()),
        };
        let error = file
            .read_at(0, 10, &Cancellation::default(), &ReadTrace::default())
            .await
            .unwrap_err();
        assert!(matches!(error, spearv_fits::Error::MalformedRange(_)));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn a_transient_range_failure_is_retried_then_succeeds() {
        use axum::{
            http::{HeaderMap, HeaderValue, StatusCode},
            routing::get,
            Router,
        };
        let requests = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        let app = Router::new().route(
            "/file",
            get(move || {
                let count = count.clone();
                async move {
                    if count.fetch_add(1, Ordering::Relaxed) == 0 {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            HeaderMap::new(),
                            Vec::new(),
                        );
                    }
                    let mut headers = HeaderMap::new();
                    headers.insert("content-range", HeaderValue::from_static("bytes 0-3/4"));
                    (StatusCode::PARTIAL_CONTENT, headers, b"abcd".to_vec())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let file = HttpFile {
            client: Client::new(),
            stats: Arc::new(Stats::default()),
            url: format!("http://{addr}/file"),
            etag: None,
            size_hint: Some(4),
            pages: Arc::from([]),
            probe: Arc::new(ProbeIo::default()),
        };
        let bytes = file
            .read_at(0, 4, &Cancellation::default(), &ReadTrace::default())
            .await
            .unwrap();
        assert_eq!(bytes, b"abcd");
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        server.abort();
    }

    #[tokio::test]
    async fn a_changed_etag_on_a_range_is_not_retried() {
        use axum::{
            http::{HeaderMap, HeaderValue, StatusCode},
            routing::get,
            Router,
        };
        let requests = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        let app = Router::new().route(
            "/file",
            get(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    let mut headers = HeaderMap::new();
                    headers.insert("content-range", HeaderValue::from_static("bytes 0-3/4"));
                    headers.insert("etag", HeaderValue::from_static("\"other\""));
                    (StatusCode::PARTIAL_CONTENT, headers, b"abcd".to_vec())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let file = HttpFile {
            client: Client::new(),
            stats: Arc::new(Stats::default()),
            url: format!("http://{addr}/file"),
            etag: Some("\"expected\"".into()),
            size_hint: Some(4),
            pages: Arc::from([]),
            probe: Arc::new(ProbeIo::default()),
        };
        let error = file
            .read_at(0, 4, &Cancellation::default(), &ReadTrace::default())
            .await
            .unwrap_err();
        assert!(matches!(error, spearv_fits::Error::ContentChanged));
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        server.abort();
    }

    #[tokio::test]
    async fn a_cancelled_range_starts_no_request() {
        let cancel = Cancellation::default();
        cancel.cancel();
        let file = HttpFile {
            client: Client::new(),
            stats: Arc::new(Stats::default()),
            url: "http://127.0.0.1:9/file".into(),
            etag: None,
            size_hint: Some(4),
            pages: Arc::from([]),
            probe: Arc::new(ProbeIo::default()),
        };
        let error = file
            .read_at(0, 4, &cancel, &ReadTrace::default())
            .await
            .unwrap_err();
        assert!(matches!(error, spearv_fits::Error::Cancelled));
    }

    fn large_picture_hdu(
        name: &str,
        bitpix: i32,
        width: usize,
        height: usize,
        extra: &[String],
        data: Vec<u8>,
    ) -> Vec<u8> {
        let mut cards = vec![
            card("XTENSION= 'IMAGE   '"),
            card(&format!("BITPIX  = {bitpix:>20}")),
            card("NAXIS   =                    2"),
            card(&format!("NAXIS1  = {width:>20}")),
            card(&format!("NAXIS2  = {height:>20}")),
            card("PCOUNT  =                    0"),
            card("GCOUNT  =                    1"),
            card(&format!("EXTNAME = '{name}'")),
        ];
        cards.extend(extra.iter().cloned());
        cards.push(card("END"));
        let mut out = block(&cards);
        out.extend(data);
        while !out.len().is_multiple_of(2880) {
            out.push(0);
        }
        out
    }

    fn large_synthetic_fits(width: usize, height: usize) -> Vec<u8> {
        let primary = block(&[
            card("SIMPLE  =                    T"),
            card("BITPIX  =                    8"),
            card("NAXIS   =                    0"),
            card("EXTEND  =                    T"),
            card("END"),
        ]);
        let pixels = width * height;
        let mut file = primary;
        let mut image = Vec::with_capacity(pixels * 4);
        for row in 0..height {
            let val = (row as f32).to_be_bytes();
            for _ in 0..width {
                image.extend_from_slice(&val);
            }
        }
        file.extend(large_picture_hdu(
            "IMAGE",
            -32,
            width,
            height,
            &image_cards(),
            image,
        ));
        let mut variance = Vec::with_capacity(pixels * 4);
        let var_bytes = 0.25_f32.to_be_bytes();
        for _ in 0..pixels {
            variance.extend_from_slice(&var_bytes);
        }
        file.extend(large_picture_hdu("VARIANCE", -32, width, height, &[], variance));
        let mut zodi = Vec::with_capacity(pixels * 4);
        let zodi_bytes = 0.5_f32.to_be_bytes();
        for _ in 0..pixels {
            zodi.extend_from_slice(&zodi_bytes);
        }
        file.extend(large_picture_hdu("ZODI", -32, width, height, &[], zodi));
        let mut flags = Vec::with_capacity(pixels * 4);
        let flag_bytes = 4_i32.to_be_bytes();
        for _ in 0..pixels {
            flags.extend_from_slice(&flag_bytes);
        }
        file.extend(large_picture_hdu(
            "FLAGS",
            32,
            width,
            height,
            &[card("BLANK   =            -2147483648")],
            flags,
        ));
        file.extend(wave_table_hdu());
        file
    }

    fn test_row(url: &str, etag: &str, size: u64, width: i32, height: i32) -> spearv_index::ExposureRow {
        spearv_index::ExposureRow {
            key: "file.fits".into(),
            url: url.into(),
            etag: Some(etag.into()),
            size: Some(size as i64),
            release: "qr3".into(),
            survey: spearv_index::Survey::AllSky,
            sps_flags: None,
            period: "2025W34".into(),
            version: "v1".into(),
            obsid: "o1".into(),
            detector: 1,
            mjd_beg: None,
            mjd_avg: 60_000.0,
            em_min_um: 0.75,
            em_max_um: 1.1,
            wcs: Wcs {
                crpix: [2.0, 2.0],
                crval: [10.0, 20.0],
                cd: [[-0.001, 0.0], [0.0, 0.001]],
                sip: spearv_core::Sip {
                    a: spearv_core::Poly::new(0),
                    b: spearv_core::Poly::new(0),
                    ap: None,
                    bp: None,
                },
                naxis: [width, height],
            },
            quality: spearv_index::Quality::default(),
            sc_xyz: None,
        }
    }

    fn source_with_cache(dir: PathBuf) -> SpherexSource {
        SpherexSource {
            client: http_client().unwrap(),
            index: None,
            cache_dir: Some(dir),
            stats: Arc::new(Stats::default()),
            layouts: Mutex::new(LayoutCache::default()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cold_disk_layouts_keep_cutout_transfer_near_planned_bytes() {
        let width = 2040usize;
        let height = 512usize;
        let file = large_synthetic_fits(width, height);
        let size = file.len() as u64;
        assert!(size > 10 * 1024 * 1024);
        let (url, seen) = serve(file, "\"v1\"").await;
        let row = test_row(&url, "\"v1\"", size, width as i32, height as i32);
        let window = PixelWindow {
            x0: 0,
            y0: 128,
            width: width as i32,
            height: 256,
            clipped: false,
        };
        let planned = (window.height as u64) * (width as u64) * 16;

        let cache_dir = std::env::temp_dir().join(format!(
            "spearv-spherex-layout-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let disk_path = disk_layout_path(&cache_dir, &url, "\"v1\"", size);

        // 1. Cold build_layout + warm memory cutout.
        let s1 = source_with_cache(cache_dir.clone());
        let geom = s1.exact_geometry(&row).await.unwrap();
        assert!(geom.header_bytes > 0);
        let disk_size = std::fs::metadata(&disk_path).unwrap().len();
        assert!(
            disk_size < 8192,
            "disk layout must stay small metadata only, got {disk_size} bytes"
        );
        let warm = s1.exact_window_cutout(&row, &window).await.unwrap();
        assert_eq!(warm.image.data[0], 128.0);
        assert_eq!(warm.image.data[warm.image.data.len() - 1], 383.0);
        assert_eq!(warm.variance.data[0], 0.25);
        assert_eq!(warm.zodi.as_ref().unwrap().data[0], 0.5);
        assert_eq!(warm.flags.data[0], 4);
        assert!(
            warm.bytes_read <= planned + 64 * 1024,
            "warm cutout read {} vs planned {}",
            warm.bytes_read,
            planned
        );
        assert_eq!(warm.s3_requests, 4);

        // 2. Cold disk layout with header_spans on a fresh SpherexSource.
        seen.lock().unwrap().clear();
        let s2 = source_with_cache(cache_dir.clone());
        let cold_spans = s2.exact_window_cutout(&row, &window).await.unwrap();
        assert_eq!(cold_spans.image.data[0], 128.0);
        assert!(
            cold_spans.bytes_read <= planned + 128 * 1024,
            "cold disk layout with header_spans read {} vs planned {}",
            cold_spans.bytes_read,
            planned
        );
        assert!(cold_spans.s3_requests <= 8);

        // Second cutout on s2 is now warm in memory.
        let warm2 = s2.exact_window_cutout(&row, &window).await.unwrap();
        assert_eq!(warm2.s3_requests, 4);
        assert!(warm2.bytes_read <= planned + 64 * 1024);

        // 3. Metadata-only disk layout without header_spans.
        let meta_only_bytes = std::fs::read(&disk_path).unwrap();
        let meta_len = u32::from_le_bytes(meta_only_bytes[0..4].try_into().unwrap()) as usize;
        let mut meta_val: serde_json::Value =
            serde_json::from_slice(&meta_only_bytes[4..4 + meta_len]).unwrap();
        meta_val.as_object_mut().unwrap().remove("header_spans");
        let meta_only_json = serde_json::to_vec(&meta_val).unwrap();
        let mut meta_only_out = Vec::new();
        meta_only_out.extend_from_slice(&(meta_only_json.len() as u32).to_le_bytes());
        meta_only_out.extend_from_slice(&meta_only_json);
        meta_only_out.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&disk_path, meta_only_out).unwrap();

        seen.lock().unwrap().clear();
        let s3 = source_with_cache(cache_dir.clone());
        let cold_meta_only = s3.exact_window_cutout(&row, &window).await.unwrap();
        assert_eq!(cold_meta_only.image.data[0], 128.0);
        assert!(
            cold_meta_only.bytes_read <= planned + 512 * 1024,
            "cold metadata-only disk layout read {} vs planned {} (overhead {})",
            cold_meta_only.bytes_read,
            planned,
            cold_meta_only.bytes_read.saturating_sub(planned)
        );
        assert!(cold_meta_only.s3_requests <= 12);

        eprintln!(
            "MEASURE planned={} warm={} ({:.4}x, req={}) cold_spans={} ({:.4}x, req={}) cold_meta_only={} ({:.4}x, req={}) disk_bytes={}",
            planned,
            warm.bytes_read,
            warm.bytes_read as f64 / planned as f64,
            warm.s3_requests,
            cold_spans.bytes_read,
            cold_spans.bytes_read as f64 / planned as f64,
            cold_spans.s3_requests,
            cold_meta_only.bytes_read,
            cold_meta_only.bytes_read as f64 / planned as f64,
            cold_meta_only.s3_requests,
            disk_size
        );

        // Metadata-only file was upgraded with header_spans on disk.
        let reloaded = load_disk_layout(&disk_path).unwrap();
        assert!(!reloaded.header_spans.is_empty());

        // 4. Stale ETag during populate_header_pages returns ContentChanged.
        let stale_row = test_row(&url, "\"v2\"", size, width as i32, height as i32);
        let stale_disk = disk_layout_path(&cache_dir, &url, "\"v2\"", size);
        let mut stale_layout = reloaded;
        stale_layout.pages = Arc::from([]);
        save_disk_layout(&stale_disk, &stale_layout);
        let s4 = source_with_cache(cache_dir.clone());
        let err = s4.exact_window_cutout(&stale_row, &window).await.unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
