use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arrow_array::{Array, Float64Array, Int64Array, RecordBatch, StringArray};
use bytes::Bytes;
use futures::future::{try_join_all, BoxFuture};
use futures::{FutureExt, TryStreamExt};
use parquet::arrow::arrow_reader::ArrowReaderOptions;
use parquet::arrow::async_reader::AsyncFileReader;
use parquet::arrow::{ParquetRecordBatchStreamBuilder, ProjectionMask};
use parquet::errors::ParquetError;
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use spearv_core::{Error, Result};

use crate::cache::DiskCache;
use crate::{Cone, WiseRow};

const COLUMNS: [&str; 8] = [
    "designation",
    "ra",
    "dec",
    "pmra",
    "pmdec",
    "w1mpro",
    "w2mpro",
    "cc_flags",
];
/// Real footers are about 57 KB, so one suffix read normally gets all of it.
const TAIL_BYTES: u64 = 96 * 1024;
const ATTEMPTS: u32 = 3;
/// Column chunks in these files sit about 110 bytes apart. Reading the padding
/// is cheaper than another request.
const MERGE_GAP_BYTES: u64 = 4096;

/// Counts for one query. Network bytes exclude anything served from the cache.
#[derive(Default)]
pub(crate) struct Counters {
    pub requests: AtomicU64,
    pub network_bytes: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_bytes: AtomicU64,
}

/// What a partition read needs. Shared by every partition of every query.
pub(crate) struct Store {
    pub client: Client,
    pub base_url: String,
    pub cache: Arc<DiskCache>,
}

pub(crate) fn http_client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(16)
        .user_agent("spearv/0.1 (anonymous AllWISE catalog read)")
        .build()
        .map_err(|err| Error::Upstream(err.to_string()))
}

/// Reads the six columns of one partition and keeps the rows inside `cone`.
pub(crate) async fn read_partition(
    store: Arc<Store>,
    counters: Arc<Counters>,
    pixel: u64,
    cone: Cone,
) -> Result<Vec<WiseRow>> {
    let file = PartitionFile {
        store,
        counters,
        pixel,
    };
    let builder = ParquetRecordBatchStreamBuilder::new(file)
        .await
        .map_err(upstream)?;
    let mask = ProjectionMask::columns(builder.parquet_schema(), COLUMNS);
    let batches = builder
        .with_projection(mask)
        .build()
        .map_err(upstream)?
        .try_collect::<Vec<_>>()
        .await
        .map_err(upstream)?;
    let rows = batches
        .iter()
        .map(|batch| rows_in(batch, &cone))
        .collect::<Result<Vec<_>>>()?;
    Ok(rows.into_iter().flatten().collect())
}

fn rows_in(batch: &RecordBatch, cone: &Cone) -> Result<Vec<WiseRow>> {
    let designation = column::<StringArray>(batch, "designation")?;
    let ra = column::<Float64Array>(batch, "ra")?;
    let dec = column::<Float64Array>(batch, "dec")?;
    let pmra = column::<Int64Array>(batch, "pmra")?;
    let pmdec = column::<Int64Array>(batch, "pmdec")?;
    let w1mpro = column::<Float64Array>(batch, "w1mpro")?;
    let w2mpro = column::<Float64Array>(batch, "w2mpro")?;
    let cc_flags = column::<StringArray>(batch, "cc_flags")?;
    let magnitude = |values: &Float64Array, row| values.is_valid(row).then(|| values.value(row));
    let motion = |values: &Int64Array, row| values.is_valid(row).then(|| values.value(row) as f64);
    Ok((0..batch.num_rows())
        .filter(|row| cone.contains(ra.value(*row), dec.value(*row)))
        .map(|row| WiseRow {
            designation: designation.value(row).to_string(),
            ra: ra.value(row),
            dec: dec.value(row),
            pmra: motion(pmra, row),
            pmdec: motion(pmdec, row),
            w1mpro: magnitude(w1mpro, row),
            w2mpro: magnitude(w2mpro, row),
            cc_flags: cc_flags
                .is_valid(row)
                .then(|| cc_flags.value(row).to_string()),
        })
        .collect())
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|array| array.as_any().downcast_ref::<T>())
        .ok_or_else(|| {
            let have = batch
                .schema()
                .fields()
                .iter()
                .map(|field| format!("{}:{:?}", field.name(), field.data_type()))
                .collect::<Vec<_>>()
                .join(",");
            Error::Upstream(format!(
                "AllWISE column {name} is missing or has an unexpected type; have {have}"
            ))
        })
}

fn upstream(err: ParquetError) -> Error {
    Error::Upstream(format!("AllWISE Parquet read: {err}"))
}

/// One partition seen through byte ranges, through the disk cache.
struct PartitionFile {
    store: Arc<Store>,
    counters: Arc<Counters>,
    pixel: u64,
}

impl PartitionFile {
    fn path(&self) -> String {
        format!(
            "{}/healpix_k0={}/healpix_k5={}/part0.snappy.parquet",
            self.store.base_url,
            self.pixel >> 10,
            self.pixel
        )
    }

    async fn cached(&self, key: String, expected: Option<usize>) -> Option<Bytes> {
        let cache = self.store.cache.clone();
        let bytes = tokio::task::spawn_blocking(move || cache.get(&key))
            .await
            .ok()??;
        if expected.is_some_and(|len| len != bytes.len()) {
            return None;
        }
        self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        self.counters
            .cache_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Some(Bytes::from(bytes))
    }

    async fn store(&self, key: String, bytes: Bytes) {
        let cache = self.store.cache.clone();
        let _ = tokio::task::spawn_blocking(move || cache.put(&key, &bytes)).await;
    }

    async fn span(&self, range: Range<u64>) -> Result<Bytes> {
        let key = format!("k5-{}-{}-{}", self.pixel, range.start, range.end);
        let len = (range.end - range.start) as usize;
        if let Some(bytes) = self.cached(key.clone(), Some(len)).await {
            return Ok(bytes);
        }
        let header = format!("bytes={}-{}", range.start, range.end - 1);
        let (bytes, got) = self.request(&header).await?;
        if got.start != range.start || bytes.len() != len {
            return Err(Error::Upstream(format!(
                "{}: asked for {header}, got {len_got} bytes from {start}",
                self.path(),
                len_got = bytes.len(),
                start = got.start
            )));
        }
        self.store(key, bytes.clone()).await;
        Ok(bytes)
    }

    /// The end of the file. A suffix read needs no prior size probe.
    async fn tail(&self, suffix: u64) -> Result<Bytes> {
        let key = format!("k5-{}-tail-{suffix}", self.pixel);
        if let Some(bytes) = self.cached(key.clone(), None).await {
            return Ok(bytes);
        }
        let (bytes, _) = self.request(&format!("bytes=-{suffix}")).await?;
        self.store(key, bytes.clone()).await;
        Ok(bytes)
    }

    async fn metadata(&self) -> Result<ParquetMetaData> {
        let tail = self.tail(TAIL_BYTES).await?;
        let needed = footer_size(&tail)?;
        let tail = if needed > tail.len() as u64 {
            self.tail(needed).await?
        } else {
            tail
        };
        // A footer length past the start of the file would underflow below.
        if needed > tail.len() as u64 {
            return Err(Error::Upstream(format!(
                "{}: footer claims {needed} bytes, file has {}",
                self.path(),
                tail.len()
            )));
        }
        let end = tail.len() - 8;
        let start = tail.len() - needed as usize;
        ParquetMetaDataReader::decode_metadata(&tail[start..end]).map_err(upstream)
    }

    /// Anything other than a matching 206 is a failure: a server that ignores
    /// `Range` would otherwise send a whole 30 MB object.
    async fn request(&self, range: &str) -> Result<(Bytes, ContentRange)> {
        let url = self.path();
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
            }
            self.counters.requests.fetch_add(1, Ordering::Relaxed);
            let response = match self
                .store
                .client
                .get(&url)
                .header(RANGE, range)
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    last = err.to_string();
                    continue;
                }
            };
            let status = response.status();
            if status.is_server_error() {
                last = format!("HTTP {status}");
                continue;
            }
            if status != StatusCode::PARTIAL_CONTENT {
                return Err(Error::Upstream(format!(
                    "{url}: expected 206 for {range}, got {status}"
                )));
            }
            let span = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(ContentRange::parse)
                .ok_or_else(|| {
                    Error::Upstream(format!("{url}: 206 without a usable Content-Range"))
                })?;
            match response.bytes().await {
                Ok(bytes) => {
                    self.counters
                        .network_bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    return Ok((bytes, span));
                }
                Err(err) => last = err.to_string(),
            }
        }
        Err(Error::Upstream(format!(
            "{url}: {range} failed after {ATTEMPTS} attempts: {last}"
        )))
    }
}

impl AsyncFileReader for PartitionFile {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, parquet::errors::Result<Bytes>> {
        async move { self.span(range).await.map_err(external) }.boxed()
    }

    /// Near ranges become one request. The rest run at the same time, which
    /// costs one round trip where the default would pay one per column.
    fn get_byte_ranges(
        &mut self,
        ranges: Vec<Range<u64>>,
    ) -> BoxFuture<'_, parquet::errors::Result<Vec<Bytes>>> {
        async move {
            let spans = merged(&ranges);
            let this = &*self;
            let bodies = try_join_all(spans.iter().map(|span| this.span(span.clone())))
                .await
                .map_err(external)?;
            ranges
                .iter()
                .map(|range| {
                    let (span, body) = spans
                        .iter()
                        .zip(&bodies)
                        .find(|(span, _)| span.start <= range.start && range.end <= span.end)
                        .ok_or_else(|| {
                            ParquetError::General("range outside merged spans".into())
                        })?;
                    Ok(body.slice(
                        (range.start - span.start) as usize..(range.end - span.start) as usize,
                    ))
                })
                .collect()
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        _options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, parquet::errors::Result<Arc<ParquetMetaData>>> {
        async move { self.metadata().await.map(Arc::new).map_err(external) }.boxed()
    }
}

fn external(err: Error) -> ParquetError {
    ParquetError::External(Box::new(err))
}

fn merged(ranges: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|range| range.start);
    sorted
        .into_iter()
        .fold(Vec::new(), |mut spans: Vec<Range<u64>>, range| {
            match spans.last_mut() {
                Some(last) if range.start <= last.end + MERGE_GAP_BYTES => {
                    last.end = last.end.max(range.end)
                }
                _ => spans.push(range),
            }
            spans
        })
}

/// Bytes from the start of the Thrift footer to the end of the file.
fn footer_size(tail: &[u8]) -> Result<u64> {
    let bad = || Error::Upstream("AllWISE file does not end in a Parquet footer".into());
    let trailer = tail
        .len()
        .checked_sub(8)
        .map(|start| &tail[start..])
        .ok_or_else(bad)?;
    if &trailer[4..] != b"PAR1" {
        return Err(bad());
    }
    let len = u32::from_le_bytes(trailer[..4].try_into().map_err(|_| bad())?);
    Ok(u64::from(len) + 8)
}

struct ContentRange {
    start: u64,
}

impl ContentRange {
    /// `bytes 100-199/1000`
    fn parse(value: &str) -> Option<Self> {
        let (start, _) = value.strip_prefix("bytes ")?.split_once('-')?;
        Some(Self {
            start: start.parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_near_ranges_merge() {
        let far = 20 + MERGE_GAP_BYTES + 1;
        assert_eq!(merged(&[10..20, 0..10, 25..30, 27..28]), vec![0..30]);
        assert_eq!(merged(&[far..far + 5, 0..20]), vec![0..20, far..far + 5]);
    }

    #[test]
    fn footer_size_reads_the_trailer() {
        let mut tail = vec![0; 20];
        tail.extend_from_slice(&12_u32.to_le_bytes());
        tail.extend_from_slice(b"PAR1");
        assert_eq!(footer_size(&tail).unwrap(), 20);
        assert!(footer_size(b"short").is_err());
        assert!(footer_size(&[0; 16]).is_err());
    }

    #[test]
    fn content_range_gives_the_start() {
        assert_eq!(
            ContentRange::parse("bytes 100-199/1000").unwrap().start,
            100
        );
        assert!(ContentRange::parse("items 1-2/3").is_none());
    }
}
