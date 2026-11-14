use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use spearv_core::{Error, Result};

/// Byte ranges on disk, bounded by total size. The oldest-used entries go first.
///
/// Keys never include an object version: AllWISE is a frozen release. A failed
/// write loses a cache entry, never a query.
pub struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
    used: Mutex<u64>,
    next_tmp: AtomicU64,
}

impl DiskCache {
    pub fn open(dir: impl Into<PathBuf>, max_bytes: u64) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|err| Error::Message(format!("cache {}: {err}", dir.display())))?;
        let cache = Self {
            used: Mutex::new(0),
            next_tmp: AtomicU64::new(0),
            dir,
            max_bytes,
        };
        cache.evict();
        Ok(cache)
    }

    pub fn used_bytes(&self) -> u64 {
        *self.used.lock().expect("cache lock")
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.dir.join(key);
        let bytes = fs::read(&path).ok()?;
        // Reads refresh the age that eviction sorts by.
        if let Ok(file) = File::options().write(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    pub fn put(&self, key: &str, bytes: &[u8]) {
        let size = bytes.len() as u64;
        if size > self.max_bytes {
            return;
        }
        let id = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!("{key}.{}.{id}.tmp", std::process::id()));
        // Rename is atomic, so a reader never sees half an entry.
        let written = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, self.dir.join(key)));
        if let Err(err) = written {
            eprintln!("wise cache: cannot store {key}: {err}");
            let _ = fs::remove_file(&tmp);
            return;
        }
        let over = {
            let mut used = self.used.lock().expect("cache lock");
            *used += size;
            *used > self.max_bytes
        };
        if over {
            self.evict();
        }
    }

    /// Recounts from disk, so double-counted concurrent writes correct themselves.
    /// Stops at 90% of the limit, so the next write does not rescan the directory.
    fn evict(&self) {
        let mut entries = self.entries();
        entries.sort_by_key(|entry| entry.1);
        let mut total = entries.iter().map(|entry| entry.2).sum::<u64>();
        if total > self.max_bytes {
            let target = self.max_bytes / 10 * 9;
            for (path, _, size) in entries {
                if total <= target {
                    break;
                }
                if fs::remove_file(path).is_ok() {
                    total -= size;
                }
            }
        }
        *self.used.lock().expect("cache lock") = total;
    }

    fn entries(&self) -> Vec<(PathBuf, SystemTime, u64)> {
        let Ok(dir) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        dir.flatten()
            .filter(|entry| !is_tmp(&entry.path()))
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                Some((entry.path(), meta.modified().ok()?, meta.len()))
            })
            .collect()
    }
}

fn is_tmp(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "tmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spearv-wise-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn age(cache: &DiskCache, key: &str, seconds: u64) {
        let file = File::options().write(true).open(cache.dir.join(key)).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(seconds)).unwrap();
    }

    #[test]
    fn evicts_the_least_recently_read_entry() {
        let dir = temp("evict");
        let cache = DiskCache::open(&dir, 300).unwrap();
        cache.put("a", &[1; 100]);
        cache.put("b", &[2; 100]);
        cache.put("c", &[3; 100]);
        age(&cache, "a", 300);
        age(&cache, "b", 200);
        age(&cache, "c", 100);
        assert!(cache.get("a").is_some(), "reading a makes b the oldest");
        cache.put("d", &[4; 100]);
        assert!(cache.get("b").is_none());
        assert_eq!(cache.get("a").unwrap(), vec![1; 100]);
        assert!(cache.get("d").is_some());
        assert!(cache.used_bytes() <= 300, "{}", cache.used_bytes());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reopening_counts_and_trims_what_is_on_disk() {
        let dir = temp("reopen");
        let cache = DiskCache::open(&dir, 1000).unwrap();
        cache.put("a", &[1; 400]);
        cache.put("b", &[2; 400]);
        age(&cache, "a", 100);
        drop(cache);
        let cache = DiskCache::open(&dir, 500).unwrap();
        assert_eq!(cache.used_bytes(), 400);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_entry_larger_than_the_limit_is_not_stored() {
        let dir = temp("large");
        let cache = DiskCache::open(&dir, 10).unwrap();
        cache.put("a", &[1; 11]);
        assert!(cache.get("a").is_none());
        assert_eq!(cache.used_bytes(), 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
