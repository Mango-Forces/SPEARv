use std::sync::Arc;
use std::time::Duration;

use index_service::bucket::HttpBucket;
use index_service::crawl::Crawler;
use index_service::http::{router, AppState};
use spearv_index::store::Store;
use spearv_index::BUCKET;

/// Every 6 hours. New data lands in weekly planning periods.
const DEFAULT_CRAWL_SECS: u64 = 6 * 60 * 60;
const DEFAULT_READS: usize = 256;

#[tokio::main]
async fn main() {
    let db = env("SPEARV_INDEX_DB", "data/index.sqlite");
    let host = env("SPEARV_INDEX_HOST", "127.0.0.1");
    let port = env("SPEARV_INDEX_PORT", "8788");
    let crawl_secs: u64 = number("SPEARV_INDEX_CRAWL_SECS", DEFAULT_CRAWL_SECS);
    let reads: usize = number("SPEARV_INDEX_READS", DEFAULT_READS);
    let bucket_url = env("SPEARV_INDEX_BUCKET", BUCKET);

    let store = Arc::new(Store::open(&db).unwrap_or_else(|err| fail(&format!("open {db}: {err}"))));
    // 0 serves the index as it is, with no crawling.
    let crawler = (crawl_secs > 0).then(|| {
        let bucket = HttpBucket::new(&bucket_url, reads).unwrap_or_else(|err| fail(&err));
        Arc::new(Crawler::new(Arc::new(bucket), Arc::clone(&store), reads))
    });
    if let Some(crawler) = &crawler {
        tokio::spawn(Arc::clone(crawler).run_forever(Duration::from_secs(crawl_secs)));
    }

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|err| fail(&format!("bind {addr}: {err}")));
    eprintln!(
        "index-service on http://{addr}, database {db}, {}",
        if crawl_secs > 0 { format!("crawling every {crawl_secs}s with {reads} header reads") } else { "not crawling".into() }
    );
    axum::serve(listener, router(AppState::new(store, crawler)))
        .with_graceful_shutdown(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await
        .unwrap_or_else(|err| fail(&err.to_string()));
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(text) => text.parse().unwrap_or_else(|_| fail(&format!("{name}={text} is not a number"))),
        Err(_) => default,
    }
}

fn fail(message: &str) -> ! {
    eprintln!("index-service: {message}");
    std::process::exit(1)
}
