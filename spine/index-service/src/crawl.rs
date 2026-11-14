//! The librarian: keeps the index equal to the bucket.
//!
//! An empty index gets a full crawl, otherwise a diff crawl; the two are the
//! same loop, a full crawl just has nothing at home. Each crawl:
//!
//! 1. Lists the bucket by prefix: releases, then periods, then versions, then
//!    every key with its ETag. A listing that fails marks only its own prefix
//!    incomplete; listing the bucket root failing stops the crawl.
//! 2. Per version folder: keys not at home are read (one 64 KiB header each)
//!    and inserted; keys whose ETag changed are read again and replaced.
//! 3. Rows at home but missing from a *complete* listing are deletion
//!    candidates. Each candidate is decided on its own: HEAD 404 deletes it,
//!    200 keeps it, anything else keeps it until the next crawl. Rows under a
//!    prefix whose listing failed are never candidates.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::stream::{self, StreamExt};
use spearv_index::store::{HomeKey, Store};
use spearv_index::{CrawlKind, CrawlRecord, ExposureRow};
use tokio::sync::Semaphore;

use crate::bucket::{list_children, list_objects, Bucket, Head, Object};
use crate::row::{parse_key, release_name, row_from_header, HEADER_BYTES};

const WRITE_BATCH: usize = 500;
const VERSION_CONCURRENCY: usize = 8;
const LIST_CONCURRENCY: usize = 16;
const HEAD_CONCURRENCY: usize = 32;
/// A crawl that stopped early (the bucket root could not be listed) is retried this soon.
const RETRY_AFTER: Duration = Duration::from_secs(300);

pub struct Crawler {
    bucket: Arc<dyn Bucket>,
    store: Arc<Store>,
    /// Caps concurrent header reads across every version folder.
    headers: Arc<Semaphore>,
    header_concurrency: usize,
    current: Mutex<Option<Arc<Progress>>>,
}

/// Running counts of the crawl in progress, readable by `/health`.
struct Progress {
    base: CrawlRecord,
    listed: AtomicI64,
    added: AtomicI64,
    changed: AtomicI64,
    removed: AtomicI64,
    kept_after_head: AtomicI64,
    head_errors: AtomicI64,
    header_failures: AtomicI64,
    prefixes_failed: AtomicI64,
}

impl Progress {
    fn new(base: CrawlRecord) -> Self {
        Self {
            base,
            listed: AtomicI64::new(0),
            added: AtomicI64::new(0),
            changed: AtomicI64::new(0),
            removed: AtomicI64::new(0),
            kept_after_head: AtomicI64::new(0),
            head_errors: AtomicI64::new(0),
            header_failures: AtomicI64::new(0),
            prefixes_failed: AtomicI64::new(0),
        }
    }

    fn snapshot(&self) -> CrawlRecord {
        let read = |counter: &AtomicI64| counter.load(Ordering::Relaxed);
        CrawlRecord {
            listed: read(&self.listed),
            added: read(&self.added),
            changed: read(&self.changed),
            removed: read(&self.removed),
            kept_after_head: read(&self.kept_after_head),
            head_errors: read(&self.head_errors),
            header_failures: read(&self.header_failures),
            prefixes_failed: read(&self.prefixes_failed),
            ..self.base.clone()
        }
    }

    fn add(counter: &AtomicI64, n: usize) {
        counter.fetch_add(n as i64, Ordering::Relaxed);
    }
}

/// Upstream as listed in this crawl. `None` means that listing failed, so
/// nothing under it is known.
type Tree = BTreeMap<String, Option<BTreeMap<String, Option<BTreeSet<String>>>>>;

impl Crawler {
    pub fn new(bucket: Arc<dyn Bucket>, store: Arc<Store>, header_concurrency: usize) -> Self {
        let header_concurrency = header_concurrency.max(1);
        Self {
            bucket,
            store,
            headers: Arc::new(Semaphore::new(header_concurrency)),
            header_concurrency,
            current: Mutex::new(None),
        }
    }

    /// The crawl in progress, if any.
    pub fn current(&self) -> Option<CrawlRecord> {
        self.current.lock().ok()?.as_ref().map(|progress| progress.snapshot())
    }

    /// Crawl at once, then again `interval` after each crawl started.
    pub async fn run_forever(self: Arc<Self>, interval: Duration) {
        loop {
            let started = Instant::now();
            let record = self.crawl().await;
            eprintln!("[crawl] {}", summary(&record));
            let wait = if record.error.is_some() {
                RETRY_AFTER.min(interval)
            } else {
                interval.saturating_sub(started.elapsed()).max(Duration::from_secs(60))
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One crawl, start to finish. The record is saved whatever happens.
    pub async fn crawl(&self) -> CrawlRecord {
        let kind = match blocking({
            let store = Arc::clone(&self.store);
            move || store.is_empty()
        })
        .await
        {
            Ok(true) => CrawlKind::Full,
            Ok(false) => CrawlKind::Diff,
            Err(err) => return failed_record(format!("index unreadable: {err}")),
        };
        let started = match blocking({
            let store = Arc::clone(&self.store);
            move || store.crawl_start(kind, now_ms())
        })
        .await
        {
            Ok(record) => record,
            Err(err) => return failed_record(format!("index unwritable: {err}")),
        };
        eprintln!("[crawl] {} crawl {} started", kind.as_str(), started.id);
        let progress = Arc::new(Progress::new(started));
        if let Ok(mut current) = self.current.lock() {
            *current = Some(Arc::clone(&progress));
        }
        let outcome = self.run(&progress).await;
        let mut record = progress.snapshot();
        record.finished_ms = Some(now_ms());
        record.error = outcome.err();
        let saved = record.clone();
        let store = Arc::clone(&self.store);
        if let Err(err) = blocking(move || store.crawl_save(&saved)).await {
            eprintln!("[crawl] could not save crawl {}: {err}", record.id);
        }
        if let Ok(mut current) = self.current.lock() {
            *current = None;
        }
        record
    }

    async fn run(&self, progress: &Arc<Progress>) -> Result<(), String> {
        let tree = self.list_tree(progress).await?;

        let now = now_ms();
        for (release, periods) in &tree {
            if let Some(periods) = periods {
                let (store, release, newest) = (Arc::clone(&self.store), release.clone(), periods.keys().max().cloned());
                blocking(move || store.record_listing(&release, newest.as_deref(), now)).await?;
            }
        }

        let versions: Vec<(String, String, String)> = tree
            .iter()
            .filter_map(|(release, periods)| Some((release, periods.as_ref()?)))
            .flat_map(|(release, periods)| {
                periods.iter().filter_map(move |(period, versions)| Some((release, period, versions.as_ref()?)))
            })
            .flat_map(|(release, period, versions)| {
                versions.iter().map(move |version| (release.clone(), period.clone(), version.clone()))
            })
            .collect();
        let mut gone: Vec<String> = stream::iter(versions)
            .map(|(release, period, version)| {
                let progress = Arc::clone(progress);
                async move { self.crawl_version(&format!("{release}/level2/{period}/{version}/"), &progress).await }
            })
            .buffer_unordered(VERSION_CONCURRENCY)
            .collect::<Vec<Vec<String>>>()
            .await
            .into_iter()
            .flatten()
            .collect();

        // Folders that have rows at home but are absent from a complete listing.
        let store = Arc::clone(&self.store);
        for (release, period, version) in blocking(move || store.scopes()).await? {
            if folder_gone(&tree, &release, &period, &version) {
                let (store, prefix) = (Arc::clone(&self.store), format!("{release}/level2/{period}/{version}/"));
                gone.extend(blocking(move || store.keys_under(&prefix)).await?.into_keys());
            }
        }
        self.verify_and_delete(gone, progress).await
    }

    async fn list_tree(&self, progress: &Progress) -> Result<Tree, String> {
        let bucket = self.bucket.as_ref();
        let root = list_children(bucket, "").await.map_err(|err| format!("bucket root: {err}"))?;
        let mut tree = Tree::new();
        for release in root.iter().filter(|name| release_name(name).is_some()) {
            let periods = match list_children(bucket, &format!("{release}/level2/")).await {
                Ok(periods) => Some(periods.into_iter().map(|period| (period, None)).collect()),
                Err(err) => {
                    eprintln!("[crawl] listing failed, nothing under it is touched: {err}");
                    Progress::add(&progress.prefixes_failed, 1);
                    None
                }
            };
            tree.insert(release.clone(), periods);
        }
        let folders: Vec<(String, String)> = tree
            .iter()
            .filter_map(|(release, periods)| Some((release, periods.as_ref()?)))
            .flat_map(|(release, periods)| periods.keys().map(move |period| (release.clone(), period.clone())))
            .collect();
        let listed: Vec<(String, String, Result<Vec<String>, String>)> = stream::iter(folders)
            .map(|(release, period)| async move {
                let versions = list_children(bucket, &format!("{release}/level2/{period}/")).await;
                (release, period, versions)
            })
            .buffer_unordered(LIST_CONCURRENCY)
            .collect()
            .await;
        for (release, period, versions) in listed {
            let slot = tree.get_mut(&release).and_then(|p| p.as_mut()).and_then(|p| p.get_mut(&period));
            match (slot, versions) {
                (Some(slot), Ok(versions)) => *slot = Some(versions.into_iter().collect()),
                (_, Err(err)) => {
                    eprintln!("[crawl] listing failed, nothing under it is touched: {err}");
                    Progress::add(&progress.prefixes_failed, 1);
                }
                (None, Ok(_)) => {}
            }
        }
        Ok(tree)
    }

    /// Diff one version folder against home. Returns its deletion candidates.
    async fn crawl_version(&self, prefix: &str, progress: &Arc<Progress>) -> Vec<String> {
        let objects = match list_objects(self.bucket.as_ref(), prefix).await {
            Ok(objects) => objects,
            Err(err) => {
                eprintln!("[crawl] listing failed, nothing under it is touched: {err}");
                Progress::add(&progress.prefixes_failed, 1);
                return vec![];
            }
        };
        let store = Arc::clone(&self.store);
        let owned = prefix.to_string();
        let mut home: HashMap<String, HomeKey> = match blocking(move || store.keys_under(&owned)).await {
            Ok(home) => home,
            Err(err) => {
                eprintln!("[crawl] {prefix}: index unreadable, folder skipped: {err}");
                Progress::add(&progress.prefixes_failed, 1);
                return vec![];
            }
        };
        let mut work: Vec<(Object, bool)> = Vec::new();
        let mut listed = 0;
        for object in objects {
            if parse_key(&object.key).is_none() {
                continue;
            }
            listed += 1;
            match home.remove(&object.key) {
                None => work.push((object, false)),
                Some(known) if object.etag.is_some() && known.etag != object.etag => work.push((object, true)),
                Some(_) => {}
            }
        }
        Progress::add(&progress.listed, listed);
        let gone: Vec<String> = home.into_keys().collect();
        if work.is_empty() {
            return gone;
        }

        let (mut added, mut changed, mut failed) = (0, 0, 0);
        let mut batch: Vec<(ExposureRow, bool)> = Vec::with_capacity(WRITE_BATCH);
        let mut reads = stream::iter(work)
            .map(|(object, replaces)| {
                let (bucket, headers) = (Arc::clone(&self.bucket), Arc::clone(&self.headers));
                async move {
                    let _permit = headers.acquire_owned().await;
                    let row = async {
                        let bytes = bucket.read_prefix(&object.key, HEADER_BYTES).await?;
                        row_from_header(&object, &bytes).await
                    }
                    .await;
                    (row, replaces)
                }
            })
            .buffer_unordered(self.header_concurrency);
        while let Some((row, replaces)) = reads.next().await {
            match row {
                Ok(row) => batch.push((row, replaces)),
                Err(err) => {
                    failed += 1;
                    if failed <= 3 {
                        eprintln!("[crawl] header skipped, retried next crawl: {err}");
                    }
                }
            }
            if batch.len() >= WRITE_BATCH {
                self.flush(&mut batch, progress, &mut added, &mut changed, &mut failed).await;
            }
        }
        self.flush(&mut batch, progress, &mut added, &mut changed, &mut failed).await;
        Progress::add(&progress.header_failures, failed);
        eprintln!("[crawl] {prefix}: listed {listed}, added {added}, changed {changed}, failed {failed}");
        gone
    }

    async fn flush(
        &self,
        batch: &mut Vec<(ExposureRow, bool)>,
        progress: &Progress,
        added: &mut usize,
        changed: &mut usize,
        failed: &mut usize,
    ) {
        if batch.is_empty() {
            return;
        }
        let rows = std::mem::take(batch);
        let replaced = rows.iter().filter(|row| row.1).count();
        let count = rows.len();
        let store = Arc::clone(&self.store);
        match blocking(move || store.upsert(&rows.into_iter().map(|row| row.0).collect::<Vec<_>>())).await {
            Ok(_) => {
                *added += count - replaced;
                *changed += replaced;
                Progress::add(&progress.added, count - replaced);
                Progress::add(&progress.changed, replaced);
            }
            Err(err) => {
                eprintln!("[crawl] write of {count} rows failed, retried next crawl: {err}");
                *failed += count;
            }
        }
    }

    /// Decide each candidate on its own HEAD.
    async fn verify_and_delete(&self, candidates: Vec<String>, progress: &Progress) -> Result<(), String> {
        if candidates.is_empty() {
            return Ok(());
        }
        eprintln!("[crawl] {} rows missing from complete listings; checking each", candidates.len());
        let mut checks = stream::iter(candidates)
            .map(|key| async move {
                let head = self.bucket.head(&key).await;
                (key, head)
            })
            .buffer_unordered(HEAD_CONCURRENCY);
        let mut missing = Vec::new();
        while let Some((key, head)) = checks.next().await {
            match head {
                Head::Missing => missing.push(key),
                Head::Found => Progress::add(&progress.kept_after_head, 1),
                Head::Unknown(err) => {
                    if progress.head_errors.fetch_add(1, Ordering::Relaxed) < 3 {
                        eprintln!("[crawl] HEAD {key} proved nothing, kept: {err}");
                    }
                }
            }
            if missing.len() >= WRITE_BATCH {
                self.delete(std::mem::take(&mut missing), progress).await?;
            }
        }
        self.delete(missing, progress).await
    }

    async fn delete(&self, keys: Vec<String>, progress: &Progress) -> Result<(), String> {
        let store = Arc::clone(&self.store);
        let removed = blocking(move || store.delete_keys(&keys)).await?;
        Progress::add(&progress.removed, removed);
        Ok(())
    }
}

fn folder_gone(tree: &Tree, release: &str, period: &str, version: &str) -> bool {
    match tree.get(release) {
        None => true,
        Some(None) => false,
        Some(Some(periods)) => match periods.get(period) {
            None => true,
            Some(None) => false,
            Some(Some(versions)) => !versions.contains(version),
        },
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|err| err.to_string())?
}

fn failed_record(error: String) -> CrawlRecord {
    let now = now_ms();
    CrawlRecord {
        id: 0,
        kind: CrawlKind::Diff,
        started_ms: now,
        finished_ms: Some(now),
        listed: 0,
        added: 0,
        changed: 0,
        removed: 0,
        kept_after_head: 0,
        head_errors: 0,
        header_failures: 0,
        prefixes_failed: 0,
        error: Some(error),
    }
}

fn summary(record: &CrawlRecord) -> String {
    format!(
        "{} crawl {} {} in {}s: listed {}, added {}, changed {}, removed {}, kept after HEAD {}, HEAD errors {}, header failures {}, failed listings {}",
        record.kind.as_str(),
        record.id,
        record.error.as_deref().map_or("finished".to_string(), |err| format!("stopped ({err})")),
        (record.finished_ms.unwrap_or(record.started_ms) - record.started_ms) / 1000,
        record.listed,
        record.added,
        record.changed,
        record.removed,
        record.kept_after_head,
        record.head_errors,
        record.header_failures,
        record.prefixes_failed,
    )
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use async_trait::async_trait;
    use spearv_index::{CoverageQuery, Survey};

    use super::*;
    use crate::bucket::ListPage;
    use crate::testing::{fits_prefix, key, TempDb};

    /// An in-memory bucket. Pages hold two entries, so every listing goes
    /// through continuation tokens.
    #[derive(Default)]
    struct FakeBucket {
        /// key -> (etag, sky position)
        objects: Mutex<BTreeMap<String, (String, f64, f64)>>,
        /// Prefixes whose listing fails.
        broken_lists: Mutex<HashSet<String>>,
        /// Keys that exist but a listing leaves out.
        unlisted: Mutex<HashSet<String>>,
        /// Keys whose HEAD fails with a 503.
        broken_heads: Mutex<HashSet<String>>,
        /// Keys whose header cannot be read.
        broken_reads: Mutex<HashSet<String>>,
        heads: Mutex<Vec<String>>,
        reads: AtomicI64,
    }

    impl FakeBucket {
        fn put(&self, key: &str, etag: &str, ra: f64, dec: f64) {
            self.objects.lock().unwrap().insert(key.into(), (etag.into(), ra, dec));
        }

        fn remove(&self, key: &str) {
            self.objects.lock().unwrap().remove(key);
        }

        fn heads(&self) -> Vec<String> {
            std::mem::take(&mut self.heads.lock().unwrap())
        }
    }

    #[async_trait]
    impl Bucket for FakeBucket {
        async fn list(&self, prefix: &str, delimiter: Option<&str>, continuation: Option<&str>) -> Result<ListPage, String> {
            if self.broken_lists.lock().unwrap().contains(prefix) {
                return Err(format!("list {prefix}: HTTP 503"));
            }
            let unlisted = self.unlisted.lock().unwrap().clone();
            let objects = self.objects.lock().unwrap().clone();
            let visible = objects.iter().filter(|(k, _)| k.starts_with(prefix) && !unlisted.contains(*k));
            let entries: Vec<(String, Option<Object>)> = if delimiter == Some("/") {
                let children: BTreeSet<String> = visible
                    .filter_map(|(k, _)| k[prefix.len()..].split_once('/').map(|(child, _)| format!("{prefix}{child}/")))
                    .collect();
                children.into_iter().map(|child| (child, None)).collect()
            } else {
                visible
                    .map(|(k, v)| (k.clone(), Some(Object { key: k.clone(), etag: Some(v.0.clone()), size: Some(65_000_000) })))
                    .collect()
            };
            let rest: Vec<_> = entries.into_iter().filter(|(name, _)| continuation.is_none_or(|t| name.as_str() > t)).collect();
            let page: Vec<_> = rest.iter().take(2).cloned().collect();
            let next = (rest.len() > 2).then(|| page[1].0.clone());
            Ok(ListPage {
                prefixes: page.iter().filter(|e| e.1.is_none()).map(|e| e.0.clone()).collect(),
                objects: page.into_iter().filter_map(|e| e.1).collect(),
                next,
            })
        }

        async fn read_prefix(&self, key: &str, _bytes: u64) -> Result<Vec<u8>, String> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.broken_reads.lock().unwrap().contains(key) {
                return Err(format!("read {key}: connection reset"));
            }
            let (_, ra, dec) = self.objects.lock().unwrap().get(key).cloned().ok_or(format!("read {key}: HTTP 404"))?;
            let detector = crate::row::parse_key(key).unwrap().detector;
            Ok(fits_prefix(detector, ra, dec, &[]))
        }

        async fn head(&self, key: &str) -> Head {
            self.heads.lock().unwrap().push(key.into());
            if self.broken_heads.lock().unwrap().contains(key) {
                return Head::Unknown("HTTP 503".into());
            }
            if self.objects.lock().unwrap().contains_key(key) {
                Head::Found
            } else {
                Head::Missing
            }
        }
    }

    struct Fixture {
        bucket: Arc<FakeBucket>,
        store: Arc<Store>,
        crawler: Crawler,
        _db: TempDb,
    }

    const V2: &str = "l2b-v20-2025-267";
    const V3: &str = "l2b-v27-2026-285";
    const RETRY: &str = "l2b_retry-v27-2026-286";

    /// qr2 with one period and qr3 with two, one of them with a retry folder.
    fn fixture() -> Fixture {
        let bucket = Arc::new(FakeBucket::default());
        for n in 1..=3 {
            bucket.put(&key("qr2", "2025W34_2B", V2, 2, n), "\"a\"", 56.8, 24.0);
        }
        for n in 1..=3 {
            bucket.put(&key("qr3", "2026W40_2A", V3, 1, n), "\"a\"", 56.8, 24.0);
        }
        bucket.put(&key("qr3", "2026W40_2A", RETRY, 1, 9), "\"a\"", 56.8, 24.0);
        bucket.put(&key("qr3", "2026W41_1A", V3, 4, 1), "\"a\"", 120.0, -30.0);
        // Not exposures: ignored, never indexed, never deleted.
        bucket.put("qr3/level2/2026W40_2A/l2b-v27-2026-285/1/readme.txt", "\"a\"", 0.0, 0.0);
        bucket.put("qr3/dark/cal-v1/1/dark_D1.fits", "\"a\"", 0.0, 0.0);
        let (db, store) = TempDb::open("crawl");
        let crawler = Crawler::new(bucket.clone(), Arc::clone(&store), 4);
        Fixture { bucket, store, crawler, _db: db }
    }

    fn keys(store: &Store) -> BTreeSet<String> {
        ["qr2/level2/2025W34_2B/", "qr3/level2/2026W40_2A/", "qr3/level2/2026W41_1A/"]
            .iter()
            .flat_map(|prefix| store.keys_under(prefix).unwrap().into_keys())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_index_gets_a_full_crawl_and_a_rerun_reads_nothing() {
        let f = fixture();
        let first = f.crawler.crawl().await;
        assert_eq!(first.kind, CrawlKind::Full);
        assert_eq!((first.listed, first.added, first.error.as_deref()), (8, 8, None), "{first:?}");
        assert_eq!(keys(&f.store).len(), 8);
        assert!(f.crawler.current().is_none());
        assert_eq!(f.store.last_crawl().unwrap().unwrap(), first);
        let releases = f.store.releases().unwrap();
        assert_eq!(releases.iter().map(|r| (r.release.as_str(), r.exposures)).collect::<Vec<_>>(), [("qr2", 3), ("qr3", 5)]);
        assert_eq!(releases[1].newest_period.as_deref(), Some("2026W41_1A"));
        assert!(releases[1].last_listed_ms.is_some());
        // The retry folder is a newer version of nothing here, so it is its own row.
        let hits = f.store.coverage(&CoverageQuery::view(56.8, 24.0, 0.1)).unwrap();
        assert_eq!(hits.len(), 7);
        assert!(hits.iter().all(|row| row.survey == Survey::AllSky));

        let reads = f.bucket.reads.load(Ordering::Relaxed);
        let generation = f.store.generation().unwrap();
        let second = f.crawler.crawl().await;
        assert_eq!(second.kind, CrawlKind::Diff);
        assert_eq!((second.listed, second.added, second.changed, second.removed), (8, 0, 0, 0), "{second:?}");
        assert_eq!(f.bucket.reads.load(Ordering::Relaxed), reads, "a diff crawl reads no unchanged header");
        assert!(f.bucket.heads().is_empty());
        assert_eq!(f.store.generation().unwrap(), generation, "nothing changed, so the generation stays");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_keys_are_added_and_a_changed_etag_is_read_again() {
        let f = fixture();
        f.crawler.crawl().await;
        let fresh = key("qr4", "2027W01_1A", "l2b-v30-2027-001", 3, 1);
        f.bucket.put(&fresh, "\"a\"", 10.0, 10.0);
        let moved = key("qr2", "2025W34_2B", V2, 2, 1);
        f.bucket.put(&moved, "\"b\"", 200.0, 45.0);
        let record = f.crawler.crawl().await;
        assert_eq!((record.added, record.changed, record.removed), (1, 1, 0), "{record:?}");
        assert_eq!(f.store.coverage(&CoverageQuery::view(10.0, 10.0, 0.1)).unwrap()[0].release, "qr4");
        let row = &f.store.coverage(&CoverageQuery::view(200.0, 45.0, 0.1)).unwrap()[0];
        assert_eq!((row.key.as_str(), row.etag.as_deref()), (moved.as_str(), Some("\"b\"")));
        assert!(f.store.coverage(&CoverageQuery::view(56.8, 24.0, 0.1)).unwrap().iter().all(|r| r.key != moved));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_missing_row_is_decided_by_its_own_head() {
        let f = fixture();
        f.crawler.crawl().await;
        let deleted = key("qr3", "2026W40_2A", V3, 1, 1);
        let flaky = key("qr3", "2026W40_2A", V3, 1, 2);
        let unsure = key("qr3", "2026W40_2A", V3, 1, 3);
        f.bucket.remove(&deleted);
        f.bucket.unlisted.lock().unwrap().insert(flaky.clone());
        f.bucket.remove(&unsure);
        f.bucket.broken_heads.lock().unwrap().insert(unsure.clone());
        let generation = f.store.generation().unwrap();
        let record = f.crawler.crawl().await;
        assert_eq!((record.removed, record.kept_after_head, record.head_errors), (1, 1, 1), "{record:?}");
        let mut heads = f.bucket.heads();
        heads.sort();
        assert_eq!(heads, [deleted.clone(), flaky.clone(), unsure.clone()]);
        let left = keys(&f.store);
        assert!(!left.contains(&deleted));
        assert!(left.contains(&flaky) && left.contains(&unsure));
        assert!(f.store.generation().unwrap() > generation);

        // Next crawl: the HEAD now answers 404, so the unsure row goes too.
        f.bucket.broken_heads.lock().unwrap().clear();
        let next = f.crawler.crawl().await;
        assert_eq!((next.removed, next.kept_after_head), (1, 1), "{next:?}");
        assert!(!keys(&f.store).contains(&unsure));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rows_under_a_failed_listing_are_not_candidates() {
        let f = fixture();
        f.crawler.crawl().await;
        let before = keys(&f.store);
        // Each level fails in turn; the files are gone upstream, but nothing
        // under a failed listing may be HEADed or deleted.
        // One file stays, so the folder itself is still listed: an emptied
        // folder vanishes from its parent listing and is a candidate there.
        let gone = [key("qr3", "2026W40_2A", V3, 1, 1), key("qr3", "2026W40_2A", V3, 1, 2)];
        for key in &gone {
            f.bucket.remove(key);
        }
        for broken in ["qr3/level2/2026W40_2A/l2b-v27-2026-285/", "qr3/level2/2026W40_2A/", "qr3/level2/"] {
            f.bucket.broken_lists.lock().unwrap().insert(broken.to_string());
            let record = f.crawler.crawl().await;
            assert_eq!((record.removed, record.prefixes_failed), (0, 1), "{broken}: {record:?}");
            assert!(record.error.is_none());
            assert!(f.bucket.heads().is_empty(), "{broken}: no HEAD under a failed listing");
            assert_eq!(keys(&f.store), before, "{broken}");
            f.bucket.broken_lists.lock().unwrap().clear();
        }
        // The bucket root failing stops the crawl outright.
        f.bucket.broken_lists.lock().unwrap().insert(String::new());
        let record = f.crawler.crawl().await;
        assert!(record.error.as_deref().is_some_and(|e| e.contains("bucket root")), "{record:?}");
        assert_eq!(keys(&f.store), before);
        assert!(f.bucket.heads().is_empty());

        // Once listings work again, the two missing files are decided by HEAD.
        f.bucket.broken_lists.lock().unwrap().clear();
        let healed = f.crawler.crawl().await;
        assert_eq!(healed.removed, 2, "{healed:?}");
        let mut heads = f.bucket.heads();
        heads.sort();
        assert_eq!(heads, gone);
        assert_eq!(keys(&f.store).len(), before.len() - 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_that_vanished_from_a_complete_listing_is_checked_row_by_row() {
        let f = fixture();
        f.crawler.crawl().await;
        // The whole 2026W41_1A period disappears; so does the retry folder.
        f.bucket.objects.lock().unwrap().retain(|k, _| !k.contains("2026W41_1A") && !k.contains(RETRY));
        let record = f.crawler.crawl().await;
        assert_eq!(record.removed, 2, "{record:?}");
        assert_eq!(f.bucket.heads().len(), 2);
        assert_eq!(keys(&f.store).len(), 6);
        // A release that vanishes entirely is the same, one HEAD per row.
        f.bucket.objects.lock().unwrap().retain(|k, _| !k.starts_with("qr2/"));
        let record = f.crawler.crawl().await;
        assert_eq!(record.removed, 3, "{record:?}");
        assert!(f.store.releases().unwrap().iter().all(|r| r.release != "qr2" || r.exposures == 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_header_that_cannot_be_read_is_retried_next_crawl() {
        let f = fixture();
        let broken = key("qr2", "2025W34_2B", V2, 2, 2);
        f.bucket.broken_reads.lock().unwrap().insert(broken.clone());
        let first = f.crawler.crawl().await;
        assert_eq!((first.added, first.header_failures), (7, 1), "{first:?}");
        assert!(!keys(&f.store).contains(&broken));
        f.bucket.broken_reads.lock().unwrap().clear();
        let second = f.crawler.crawl().await;
        assert_eq!((second.kind, second.added, second.header_failures), (CrawlKind::Diff, 1, 0), "{second:?}");
        assert!(keys(&f.store).contains(&broken));
    }
}
