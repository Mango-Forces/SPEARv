use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use parquet::arrow::ArrowWriter;
use spearv_core::{sky_to_png_pixel, sky_to_png_pixel_rect, CatalogSource, Error, SkyRegion};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{AllWise, Cone, DiskCache, MAX_FOV_DEG};

fn region(ra: f64, dec: f64, fov_deg: f64) -> SkyRegion {
    SkyRegion::square(ra, dec, fov_deg)
}

#[test]
fn cone_reaches_the_corners_of_the_view() {
    let (ra, dec, fov) = (56.75, 24.12, 0.1);
    let cone = Cone::around(&region(ra, dec, fov)).unwrap();
    let half = fov / 2.0 - 1e-4;
    let corner_ra = ra + half / dec.to_radians().cos();
    let corner_dec = dec + half;
    assert!(
        sky_to_png_pixel(corner_ra, corner_dec, ra, dec, fov).is_some(),
        "the corner is in the view"
    );
    assert!(cone.contains(corner_ra, corner_dec));
    assert!(
        !cone.contains(ra, dec + fov),
        "a whole view away is outside"
    );
}

#[test]
fn rectangular_cone_reaches_wide_and_tall_corners() {
    for region in [
        SkyRegion::rect(56.75, 24.12, 0.2, 0.1),
        SkyRegion::rect(56.75, 24.12, 0.1, 0.2),
    ] {
        let cone = Cone::around(&region).unwrap();
        let corner = sky_to_png_pixel_rect(
            region.ra,
            region.dec,
            region.ra,
            region.dec,
            region.fov_l_deg,
            region.fov_h_deg,
        )
        .unwrap();
        assert!(corner.0.is_finite() && corner.1.is_finite());
        let grid = spearv_core::TanGrid::new_rect(
            region.ra,
            region.dec,
            region.fov_l_deg,
            region.fov_h_deg,
        );
        let (ra, dec) = grid.sky(f64::from(grid.width), f64::from(grid.height));
        assert!(cone.contains(ra, dec));
    }
}

#[test]
fn cone_crosses_ra_zero_and_the_pole() {
    let cone = Cone::around(&region(0.01, 10.0, 0.1)).unwrap();
    assert!(cone.contains(359.99, 10.0));
    let cone = Cone::around(&region(10.0, 89.99, 0.1)).unwrap();
    assert!(
        cone.contains(190.0, 89.99),
        "across the pole is 0.02 degrees"
    );
}

#[test]
fn pixels_match_the_partition_of_a_known_file() {
    // pixel 0 runs north from the base-pixel corner at (45, 0).
    let cone = Cone::around(&region(45.0, 0.2, 0.05)).unwrap();
    assert_eq!(cone.pixels(), vec![0]);
}

#[test]
fn a_small_cone_reads_only_the_partitions_it_touches() {
    // astropy_healpix cone_search_lonlat gives the same list for this cone.
    let cone = Cone::around(&region(56.75, 24.12, 0.1)).unwrap();
    assert_eq!(cone.pixels(), vec![115, 118]);
    let cone = Cone::around(&region(56.25, 23.0, 0.1)).unwrap();
    assert_eq!(cone.pixels().len(), 1, "{:?}", cone.pixels());
}

#[test]
fn bad_regions_are_rejected_before_any_read() {
    assert!(matches!(
        Cone::around(&region(400.0, 0.0, 0.1)),
        Err(Error::InvalidSky)
    ));
    assert!(matches!(
        Cone::around(&region(0.0, 91.0, 0.1)),
        Err(Error::InvalidSky)
    ));
    assert!(matches!(
        Cone::around(&region(f64::NAN, 0.0, 0.1)),
        Err(Error::InvalidSky)
    ));
    assert!(matches!(
        Cone::around(&region(0.0, 0.0, 0.0)),
        Err(Error::FovOutOfRange { .. })
    ));
    assert!(matches!(
        Cone::around(&region(0.0, 0.0, MAX_FOV_DEG + 1.0)),
        Err(Error::FovOutOfRange { .. })
    ));
}

/// A small file in the real layout: the wanted columns are not adjacent.
fn parquet_file() -> Vec<u8> {
    let filler = (0..3).map(i64::from).collect::<Vec<_>>();
    let columns: Vec<(&str, ArrayRef)> = vec![
        (
            "designation",
            Arc::new(StringArray::from(vec!["Jfar", "Jdim", "Jbright"])),
        ),
        ("ra", Arc::new(Float64Array::from(vec![45.5, 45.0, 45.001]))),
        ("dec", Arc::new(Float64Array::from(vec![1.5, 0.2, 0.2]))),
        ("unused", Arc::new(Int64Array::from(filler))),
        (
            "pmra",
            Arc::new(Int64Array::from(vec![Some(0), Some(1000), Some(-2000)])),
        ),
        (
            "pmdec",
            Arc::new(Int64Array::from(vec![Some(0), None, Some(2000)])),
        ),
        (
            "w1mpro",
            Arc::new(Float64Array::from(vec![Some(1.0), None, Some(9.5)])),
        ),
        (
            "w2mpro",
            Arc::new(Float64Array::from(vec![Some(1.0), Some(8.0), None])),
        ),
        (
            "cc_flags",
            Arc::new(StringArray::from(vec![Some("0000"), None, Some("hhh0")])),
        ),
    ];
    let batch = RecordBatch::try_from_iter(columns).unwrap();
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    bytes
}

/// Minimal S3 stand-in. `honour_range: false` is a server that ignores `Range`.
async fn serve(file: Vec<u8>, honour_range: bool) -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicU64::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::Relaxed);
            let mut request = vec![0; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let text = String::from_utf8_lossy(&request[..read]).to_lowercase();
            let spec = text
                .lines()
                .find_map(|line| line.strip_prefix("range: bytes="))
                .unwrap()
                .trim()
                .to_string();
            let size = file.len();
            let (start, end) = match spec.split_once('-').unwrap() {
                ("", suffix) => (size.saturating_sub(suffix.parse().unwrap()), size - 1),
                (start, end) => (start.parse().unwrap(), end.parse().unwrap()),
            };
            let head = if honour_range {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    end + 1 - start
                )
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n")
            };
            socket.write_all(head.as_bytes()).await.unwrap();
            let body = if honour_range {
                &file[start..=end]
            } else {
                &file[..]
            };
            socket.write_all(body).await.unwrap();
        }
    });
    (url, hits)
}

fn cache_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("spearv-wise-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn reads_a_cone_then_serves_it_from_disk_with_no_requests() {
    let (url, hits) = serve(parquet_file(), true).await;
    let dir = cache_dir("reads");
    let open =
        || AllWise::with_base_url(url.clone(), DiskCache::open(&dir, 1 << 20).unwrap()).unwrap();

    let cold = open().cone_rows(&region(45.0, 0.2, 0.05)).await.unwrap();
    let ids = cold
        .rows
        .iter()
        .map(|row| row.designation.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["Jbright", "Jdim"],
        "sorted by W1, a null W1 last, and Jfar is outside"
    );
    assert_eq!(cold.rows[0].w2mpro, None);
    assert_eq!(cold.rows[0].pmra, Some(-2000.0));
    assert_eq!(cold.rows[1].pmdec, None);
    assert_eq!(cold.rows[1].w1mpro, None);
    assert_eq!(cold.rows[1].cc_flags, None);
    assert_eq!(cold.rows[0].cc_flags.as_deref(), Some("hhh0"));
    assert!(cold.stats.network_bytes > 0);
    assert_eq!(cold.stats.partitions, 1);
    assert_eq!(cold.stats.requests, hits.load(Ordering::Relaxed));

    let before = hits.load(Ordering::Relaxed);
    let source = open();
    let warm = source.cone_rows(&region(45.0, 0.2, 0.05)).await.unwrap();
    assert_eq!(warm.rows, cold.rows);
    assert_eq!(warm.stats.network_bytes, 0);
    assert_eq!(warm.stats.requests, 0);
    assert_eq!(hits.load(Ordering::Relaxed), before);

    let rows = source
        .cone(&region(45.0, 0.2, 0.05), Some(61000.0))
        .await
        .unwrap();
    assert_eq!(rows[0].id, "Jbright");
    assert_eq!(rows[0].mag, Some(9.5));
    assert_eq!(rows[1].mag, None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_server_that_ignores_range_is_an_error() {
    let (url, _) = serve(parquet_file(), false).await;
    let dir = cache_dir("ignores");
    let source = AllWise::with_base_url(url, DiskCache::open(&dir, 1 << 20).unwrap()).unwrap();
    let err = source
        .cone_rows(&region(45.0, 0.2, 0.05))
        .await
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("expected 206"), "{err}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
#[ignore = "waits for the real 45 s deadline"]
async fn a_server_that_never_answers_ends_at_the_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = cache_dir("silent");
    let source = AllWise::with_base_url(url, DiskCache::open(&dir, 1 << 20).unwrap()).unwrap();
    let started = std::time::Instant::now();
    let err = source
        .cone_rows(&region(45.0, 0.2, 0.05))
        .await
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("exceeded 45s"), "{err}");
    assert!(started.elapsed().as_secs() < 47, "{:?}", started.elapsed());
    drop(listener);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn proper_motion_moves_a_high_pm_source_between_epochs() {
    // Barnard's Star: about 10.3 arcsec/yr in Dec.
    let (ra, dec) = crate::propagate(269.452077, 4.693365, Some(-798.71), Some(10328.12), 51544.5);
    let years = (51544.5 - 55400.0) / 365.25;
    let expected_dec = 4.693365 + 10328.12 * years / 3.6e6;
    assert!((dec - expected_dec).abs() < 1e-9, "{dec} vs {expected_dec}");
    assert!(
        (ra - 269.452077 - (-798.71) * years / (3.6e6 * 4.693365_f64.to_radians().cos())).abs()
            < 1e-9
    );
    let (later_ra, later_dec) = crate::propagate(ra, dec, Some(-798.71), Some(10328.12), 61000.0);
    assert!(later_dec > dec + 0.02, "{later_dec} vs {dec}");
    assert!(later_ra < ra, "{later_ra} vs {ra}");
}
