//! index-service: the library (an SQLite index of every SPHEREx Level 2 file)
//! and its librarian (a crawler that keeps it equal to the public bucket),
//! behind a small HTTP API. Callers ask here and never open the database.

pub mod bucket;
pub mod crawl;
mod fits;
pub mod http;
mod row;

#[cfg(test)]
mod testing {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use spearv_index::store::Store;

    /// A fresh index database in the temp directory. Dropping it removes the
    /// file and its WAL companions.
    pub(crate) struct TempDb(PathBuf);

    impl TempDb {
        pub(crate) fn open(name: &str) -> (Self, Arc<Store>) {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let db = Self(std::env::temp_dir().join(format!("spearv-{name}-{}-{id}.sqlite", std::process::id())));
            db.remove();
            let store = Arc::new(Store::open(&db.0).unwrap());
            (db, store)
        }

        fn remove(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            self.remove();
        }
    }

    /// A Level 2 exposure key.
    pub(crate) fn key(release: &str, period: &str, version: &str, detector: u8, number: u32) -> String {
        format!("{release}/level2/{period}/{version}/{detector}/level2_{period}_{number:04}_1D{detector}_spx_{version}.fits")
    }

    /// One 80-character header card.
    pub(crate) fn card(line: &str) -> String {
        format!("{line:<80}")[..80].to_string()
    }

    fn block(cards: &[String]) -> String {
        let mut text: String = cards.concat();
        while !text.len().is_multiple_of(2880) {
            text.push(' ');
        }
        text
    }

    /// A primary HDU without data followed by an IMAGE header made of `image`
    /// and an `END` card.
    pub(crate) fn header_prefix(image: &[String]) -> Vec<u8> {
        let primary = ["SIMPLE  =                    T", "BITPIX  =                    8", "NAXIS   =                    0", "EXTEND  =                    T", "END"];
        let mut cards = image.to_vec();
        cards.push(card("END"));
        format!("{}{}", block(&primary.map(card)), block(&cards)).into_bytes()
    }

    /// A Level 2 header prefix with a north-up TAN WCS centred on `(ra, dec)`
    /// and `extra` cards added.
    pub(crate) fn fits_prefix(detector: u8, ra: f64, dec: f64, extra: &[&str]) -> Vec<u8> {
        let cards: Vec<String> = [
            "XTENSION= 'IMAGE   '".to_string(),
            "BITPIX  =                  -32".into(),
            "NAXIS   =                    2".into(),
            "NAXIS1  =                 2040".into(),
            "NAXIS2  =                 2040".into(),
            "EXTNAME = 'IMAGE   '".into(),
            "CTYPE1  = 'RA---TAN'".into(),
            "CTYPE2  = 'DEC--TAN'".into(),
            "CRPIX1  =               1020.5".into(),
            "CRPIX2  =               1020.5".into(),
            format!("CRVAL1  = {ra:>20}"),
            format!("CRVAL2  = {dec:>20}"),
            "CD1_1   =         -0.001708333".into(),
            "CD1_2   =                  0.0".into(),
            "CD2_1   =                  0.0".into(),
            "CD2_2   =          0.001708333".into(),
            "A_ORDER =                    0".into(),
            "B_ORDER =                    0".into(),
            format!("DETECTOR= {detector:>20}"),
            "MJD-BEG =      61314.506582095".into(),
            "MJD-AVG =      61314.507248284".into(),
        ]
        .iter()
        .map(|line| card(line))
        .chain(extra.iter().map(|line| card(line)))
        .collect();
        header_prefix(&cards)
    }
}
