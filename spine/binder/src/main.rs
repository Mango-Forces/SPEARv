use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Json;
use serde_json::{json, Value};
use spearv_composite::Composite;
use spearv_core::{env, Cache, Context, Extension, IndexService, WorkBudget};
use spearv_delta::Delta;
use spearv_observations::Observations;
use spearv_embed::Embed;
use spearv_index::client::IndexClient;
use spearv_overlay::Overlay;
use spearv_science::{PythonMath, RuntimeConfig};
use spearv_spherex::SpherexSource;
use spearv_spectrum::Spectrum;
use utoipa::openapi::OpenApi;
use utoipa_axum::router::OpenApiRouter;

/// The extensions the binder mounts, one line each. `/health` reports the same
/// list, so a route cannot appear without its id there.
fn extensions(index: Option<Arc<IndexClient>>) -> Vec<Box<dyn Extension>> {
    vec![
        Box::new(Composite),
        Box::new(Spectrum),
        Box::new(Delta),
        Box::new(Overlay),
        Box::new(Embed),
        Box::new(Observations { index }),
    ]
}

#[tokio::main]
async fn main() {
    match parse(std::env::args().skip(1)) {
        Ok(Command::OpenApi(path)) => {
            let extensions = extensions(None);
            let refs = extensions.iter().map(|extension| extension.as_ref()).collect::<Vec<_>>();
            let json = openapi(&refs).to_pretty_json().expect("openapi json");
            match path {
                Some(path) => std::fs::write(path, json).expect("write spec"),
                None => println!("{json}"),
            }
        }
        Ok(Command::Serve) => {
            let index = Arc::new(IndexClient::new(index_url()).expect("index-service client"));
            let ctx = context(Arc::clone(&index));
            spearv_observations::sweep_sequences();
            let extensions = extensions(Some(index));
            let refs = extensions.iter().map(|extension| extension.as_ref()).collect::<Vec<_>>();
            let port = env::number("SPEARV_PORT", 8080_u16);
            // Loopback by default. A container needs 0.0.0.0, or the port is unreachable.
            let host = std::env::var("SPEARV_HOST").unwrap_or_else(|_| "127.0.0.1".into());
            let addr = format!("{host}:{port}");
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .unwrap_or_else(|err| panic!("bind {addr}: {err}"));
            println!("spearv listening on http://{addr}");
            serve(&refs, ctx, listener, shutdown_signal()).await.expect("server");
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

/// The served context: the SPHEREx source, the process budget, the image math
/// backend, and the index-service at `index_url()`.
fn context(index: Arc<IndexClient>) -> Arc<Context> {
    let budget = Arc::new(WorkBudget::from_env());
    // The library owns the image math, so the process cannot serve without it.
    // A missing or foreign runtime is a deployment fault and stops the server
    // here instead of failing every mosaic later.
    let config = RuntimeConfig::from_env();
    let math = PythonMath::initialize(config.clone(), Arc::clone(&budget)).unwrap_or_else(|error| {
        panic!(
            "image math runtime at {} is not usable: {error}; set SPEARV_PYTHON_PREFIX to the private runtime",
            config.executable.display()
        )
    });
    let mut ctx = context_with(index, budget, Arc::new(math));
    register_catalogs(&mut ctx);
    Arc::new(ctx)
}

/// Register the catalogs the overlay reads. A catalog that is not provisioned
/// is reported and skipped; the rest of the server still starts.
fn register_catalogs(ctx: &mut Context) {
    spearv_gaia::GaiaStore::register(&mut ctx.catalogs);
    match spearv_wise::AllWise::from_env() {
        Ok(source) => ctx.register_catalog(Arc::new(source)),
        Err(error) => eprintln!("no AllWISE catalog: {error}"),
    }
}

/// The context once every dependency is in hand, so tests can substitute them.
fn context_with(
    index: Arc<IndexClient>,
    budget: Arc<WorkBudget>,
    math: Arc<dyn spearv_core::math::ImageMath>,
) -> Context {
    let source = SpherexSource::with_index(Arc::clone(&index)).expect("SPHEREx source");
    let mut ctx = Context::new(Arc::new(Cache::new()))
        .with_budget(budget)
        .with_math(math)
        .with_index(index);
    ctx.register_image(Arc::new(source));
    ctx
}

/// Where the index-service answers, from `SPEARV_INDEX_URL`.
fn index_url() -> String {
    std::env::var("SPEARV_INDEX_URL").unwrap_or_else(|_| "http://127.0.0.1:8788".into())
}

#[derive(Debug, PartialEq)]
enum Command {
    Serve,
    OpenApi(Option<String>),
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None => Ok(Command::Serve),
        Some("--openapi") => Ok(Command::OpenApi(args.next())),
        Some(flag) => Err(format!("unknown argument {flag}; use --openapi [path]")),
    }
}

/// Every extension's routes and the spec they declare, built once.
fn split(extensions: &[&dyn Extension]) -> (axum::Router<Arc<Context>>, OpenApi) {
    let (api, mut spec) = extensions
        .iter()
        .fold(OpenApiRouter::new(), |router, extension| router.merge(extension.routes()))
        .split_for_parts();
    spec.info.title = "SPEARv".into();
    spec.info.description = Some("SPHEREx sky images. Pixel work stays on the server.".into());
    spec.info.version = "0.1.0".into();
    spec.info.contact = None;
    spec.info.license = None;
    (api, spec)
}

fn openapi(extensions: &[&dyn Extension]) -> OpenApi {
    split(extensions).1
}

/// Start every extension, serve until `signal`, then shut every extension down once.
///
/// Extensions stop as soon as `signal` fires, before open connections drain, so
/// a slow client streaming a large PNG cannot hold display producers and their
/// owned cache files past a supervisor's kill timeout. Unlinked files stay
/// readable through descriptors that are already open.
async fn serve(
    extensions: &[&dyn Extension],
    ctx: Arc<Context>,
    listener: tokio::net::TcpListener,
    signal: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let (api, spec) = split(extensions);
    let spec = Spec(Arc::from(spec.to_pretty_json().expect("openapi json")));
    let mounted = Mounted(extensions.iter().map(|extension| extension.id()).collect());
    // The state-providing routes come before `with_state`; a route added after
    // it would have to extract from the remaining `()` state instead.
    let app = api
        .route("/api/v1/openapi.json", get(openapi_json))
        .route("/health", get(health))
        .with_state(Arc::clone(&ctx))
        .layer(axum::Extension(spec))
        .layer(axum::Extension(mounted));
    for extension in extensions {
        extension.start(&ctx);
    }
    // Dropping `stop_watch` ends the watcher, whichever way the server returns.
    let (stop_watch, watch_stopped) = tokio::sync::oneshot::channel::<()>();
    let watcher = ctx.index().map(|index| {
        tokio::spawn(watch_generation(
            Arc::clone(index),
            Arc::clone(&ctx.cache),
            GENERATION_POLL,
            async {
                let _ = watch_stopped.await;
            },
        ))
    });
    let (drain, drained) = tokio::sync::oneshot::channel::<()>();
    let server = std::future::IntoFuture::into_future(axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = drained.await;
    }));
    tokio::pin!(server);
    tokio::pin!(signal);
    let mut stopped = false;
    let result = tokio::select! {
        result = &mut server => result,
        () = &mut signal => {
            shut_down(extensions).await;
            stopped = true;
            let _ = drain.send(());
            (&mut server).await
        }
    };
    drop(stop_watch);
    if let Some(watcher) = watcher {
        let _ = watcher.await;
    }
    if !stopped {
        shut_down(extensions).await;
    }
    result
}

async fn shut_down(extensions: &[&dyn Extension]) {
    for extension in extensions {
        extension.shutdown().await;
    }
}

/// How often the binder asks the index whether its exposures changed.
const GENERATION_POLL: Duration = Duration::from_secs(30);

/// Poll the index generation every `period` and clear `cache` when it changes, until `stop` resolves.
async fn watch_generation(
    index: Arc<dyn IndexService>,
    cache: Arc<Cache>,
    period: Duration,
    stop: impl Future<Output = ()>,
) {
    let mut ticks = tokio::time::interval(period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seen = None;
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = ticks.tick() => refresh_generation(index.as_ref(), &cache, &mut seen).await,
        }
    }
}

/// Read the generation once. The first read only records it; a later, different one clears `cache`.
/// A failed or slow read keeps the last generation, so the next good read still sees a change.
async fn refresh_generation(index: &dyn IndexService, cache: &Cache, seen: &mut Option<String>) {
    match tokio::time::timeout(Duration::from_secs(5), index.generation()).await {
        Ok(Ok(generation)) => {
            if seen.as_ref().is_some_and(|previous| *previous != generation) {
                cache.clear();
            }
            *seen = Some(generation);
        }
        Ok(Err(err)) => eprintln!("index generation check failed: {err}"),
        Err(_) => eprintln!("index generation check timed out"),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.expect("install Ctrl-C handler"),
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.expect("install Ctrl-C handler");
}

#[derive(Clone)]
struct Spec(Arc<str>);

#[derive(Clone)]
struct Mounted(Arc<[&'static str]>);

async fn openapi_json(axum::Extension(spec): axum::Extension<Spec>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], spec.0.to_string())
}

/// Which extensions and sources are mounted and, when an index is configured,
/// whether it answers. A failed or slow index is 503 so the container
/// healthcheck turns red; the body still names the reason.
async fn health(
    State(ctx): State<Arc<Context>>,
    axum::Extension(mounted): axum::Extension<Mounted>,
) -> (StatusCode, Json<Value>) {
    let mut image_sources = ctx.images.keys().map(|id| id.0.as_str()).collect::<Vec<_>>();
    let mut catalog_sources = ctx.catalogs.keys().map(|id| id.0.as_str()).collect::<Vec<_>>();
    image_sources.sort_unstable();
    catalog_sources.sort_unstable();
    let (status, index) = match ctx.index() {
        None => (StatusCode::OK, Value::Null),
        Some(index) => match tokio::time::timeout(Duration::from_secs(5), index.health()).await {
            Ok(Ok(found)) => (StatusCode::OK, json!({ "reachable": true, "health": found })),
            Ok(Err(err)) => (StatusCode::SERVICE_UNAVAILABLE, json!({ "reachable": false, "error": err.to_string() })),
            Err(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "reachable": false, "error": "health check timed out" }),
            ),
        },
    };
    let body = json!({
        "ok": status == StatusCode::OK,
        "extensions": &*mounted.0,
        "image_sources": image_sources,
        "catalog_sources": catalog_sources,
        "index": index,
    });
    (status, Json(body))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use spearv_core::{CatalogRow, CatalogSource, SkyRegion, SourceId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use utoipa_axum::routes;

    use super::*;

    struct Probe {
        started: AtomicUsize,
        stopped: AtomicUsize,
    }

    #[utoipa::path(get, path = "/api/v1/probe", responses((status = 200, body = String)))]
    async fn probe() -> &'static str {
        "probe"
    }

    #[async_trait]
    impl Extension for Probe {
        fn id(&self) -> &'static str {
            "probe"
        }

        fn routes(&self) -> OpenApiRouter<Arc<Context>> {
            OpenApiRouter::new().routes(routes!(probe))
        }

        fn start(&self, _ctx: &Arc<Context>) {
            self.started.fetch_add(1, Ordering::SeqCst);
        }

        async fn shutdown(&self) {
            self.stopped.fetch_add(1, Ordering::SeqCst);
        }
    }

    static PROBE: Probe = Probe { started: AtomicUsize::new(0), stopped: AtomicUsize::new(0) };

    /// A math backend that fails every call. Context wiring only needs the
    /// backend to be present, not to do any work.
    struct NoMath;

    impl spearv_core::math::ImageMath for NoMath {
        fn session(
            &self,
            _control: spearv_core::math::MathControl,
            _tile: spearv_core::math::Tile,
            _product: spearv_core::math::Product,
        ) -> spearv_core::math::MathResult<Box<dyn spearv_core::math::MathSession>> {
            Err(spearv_core::math::MathError::Runtime("test stub".into()))
        }

        fn spectrum(
            &self,
            _control: &spearv_core::math::MathControl,
            _input: &spearv_core::math::SpectrumInput,
        ) -> spearv_core::math::MathResult<spearv_core::math::Pinned<spearv_core::math::SpectrumPlanes>> {
            Err(spearv_core::math::MathError::Runtime("test stub".into()))
        }
    }

    struct Stars;

    #[async_trait]
    impl CatalogSource for Stars {
        fn id(&self) -> SourceId {
            SourceId::new("stars")
        }

        async fn cone(&self, _region: &SkyRegion, _epoch_mjd: Option<f64>) -> spearv_core::Result<Vec<CatalogRow>> {
            Ok(Vec::new())
        }
    }

    struct Index(bool);

    #[async_trait]
    impl IndexService for Index {
        async fn health(&self) -> spearv_core::Result<Value> {
            if self.0 {
                return Ok(json!({ "generation": "7" }));
            }
            Err(spearv_core::Error::Upstream("index is down".into()))
        }

        async fn generation(&self) -> spearv_core::Result<String> {
            Ok("7".into())
        }
    }

    /// An index whose generation the test moves, counting every read.
    struct Moving {
        generation: std::sync::Mutex<String>,
        reads: AtomicUsize,
    }

    impl Moving {
        fn new(generation: &str) -> Arc<Self> {
            Arc::new(Self { generation: std::sync::Mutex::new(generation.into()), reads: AtomicUsize::new(0) })
        }

        fn set(&self, generation: &str) {
            *self.generation.lock().unwrap() = generation.into();
        }
    }

    #[async_trait]
    impl IndexService for Moving {
        async fn health(&self) -> spearv_core::Result<Value> {
            Ok(json!({}))
        }

        async fn generation(&self) -> spearv_core::Result<String> {
            let generation = self.generation.lock().unwrap().clone();
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(generation)
        }
    }

    fn context(index: Option<bool>) -> Arc<Context> {
        let mut ctx = Context::new(Arc::new(Cache::new()));
        ctx.register_catalog(Arc::new(Stars));
        Arc::new(match index {
            Some(up) => ctx.with_index(Arc::new(Index(up))),
            None => ctx,
        })
    }

    async fn health_of(ctx: Arc<Context>) -> (StatusCode, Value) {
        let (status, Json(body)) = health(State(ctx), axum::Extension(Mounted(Arc::from(["probe"])))).await;
        (status, body)
    }

    async fn fetch(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: spearv\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("response");
        let status = response[9..12].parse().expect("status");
        let body = response.split_once("\r\n\r\n").expect("body").1.to_string();
        (status, body)
    }

    #[test]
    fn the_command_line_serves_or_writes_the_spec() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(parse(args(&[])), Ok(Command::Serve));
        assert_eq!(parse(args(&["--openapi"])), Ok(Command::OpenApi(None)));
        assert_eq!(parse(args(&["--openapi", "spec.json"])), Ok(Command::OpenApi(Some("spec.json".into()))));
        assert!(parse(args(&["--serve"])).unwrap_err().contains("--openapi"));
    }

    #[test]
    fn the_served_context_registers_the_source_the_math_and_the_index() {
        let index = Arc::new(IndexClient::new("http://127.0.0.1:9").expect("client"));
        let budget = Arc::new(WorkBudget::default());
        let ctx = context_with(index, budget, Arc::new(NoMath));
        assert!(ctx.index().is_some());
        assert!(ctx.math().is_ok());
        assert_eq!(ctx.images.keys().map(|id| id.0.as_str()).collect::<Vec<_>>(), ["spherex"]);
    }

    #[test]
    fn the_served_context_registers_the_wise_catalog() {
        let dir = std::env::temp_dir().join(format!("spearv-wise-{}", std::process::id()));
        std::env::set_var("SPEARV_WISE_CACHE", &dir);
        std::env::set_var("SPEARV_GAIA", dir.join("missing.sqlite"));
        let mut ctx = Context::new(Arc::new(Cache::new()));
        register_catalogs(&mut ctx);
        assert!(ctx.catalogs.contains_key(&spearv_core::SourceId::new("wise")));
        std::env::remove_var("SPEARV_WISE_CACHE");
        std::env::remove_var("SPEARV_GAIA");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_spec_lists_every_mounted_extension() {
        let extensions = extensions(None);
        let ids = extensions.iter().map(|extension| extension.id()).collect::<Vec<_>>();
        assert_eq!(ids, ["composite", "spectrum", "delta", "overlay", "embed", "observations"]);
        let refs = extensions.iter().map(|extension| extension.as_ref()).collect::<Vec<_>>();
        let spec = openapi(&refs);
        assert_eq!(spec.info.title, "SPEARv");
        let paths = spec.paths.paths.keys().map(String::as_str).collect::<Vec<_>>();
        for path in [
            "/api/v1/coverage",
            "/api/v1/releases",
            "/api/v1/render.png",
            "/api/v1/color.png",
            "/api/v1/mosaic",
            "/api/v1/spectrum",
            "/api/v1/delta.png",
            "/api/v1/delta/candidates",
            "/api/v1/catalog/{source}",
            "/embed",
        ] {
            assert!(paths.contains(&path), "missing {path}; have {paths:?}");
        }
        let spec = openapi(&[&PROBE as &dyn Extension]);
        let paths = spec.paths.paths.keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(paths, ["/api/v1/probe"]);
    }

    #[tokio::test]
    async fn health_reports_mounts_and_the_index() {
        let (status, body) = health_of(context(None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["extensions"], json!(["probe"]));
        assert_eq!(body["image_sources"], json!([]));
        assert_eq!(body["catalog_sources"], json!(["stars"]));
        assert_eq!(body["index"], Value::Null);

        let (status, body) = health_of(context(Some(true))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["index"]["health"]["generation"], "7");

        let (status, body) = health_of(context(Some(false))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ok"], false);
        assert_eq!(body["index"]["reachable"], false);
        assert!(body["index"]["error"].as_str().unwrap().contains("index is down"));
    }

    #[tokio::test]
    async fn a_generation_change_clears_the_cache() {
        let index = Moving::new("1");
        let ctx = Context::new(Arc::new(Cache::new())).with_index(index.clone());
        let installed = ctx.index().expect("index");
        let mut seen = None;
        ctx.cache.insert("view".into(), vec![1]);
        refresh_generation(installed.as_ref(), &ctx.cache, &mut seen).await;
        assert_eq!(ctx.cache.get("view"), Some(vec![1]), "the first read only records the generation");
        refresh_generation(installed.as_ref(), &ctx.cache, &mut seen).await;
        assert_eq!(ctx.cache.get("view"), Some(vec![1]), "an unchanged generation keeps the cache");
        let epoch = ctx.cache.epoch();
        index.set("2");
        refresh_generation(installed.as_ref(), &ctx.cache, &mut seen).await;
        assert!(ctx.cache.get("view").is_none());
        assert_eq!(ctx.cache.epoch(), epoch + 1);
    }

    #[tokio::test]
    async fn the_generation_watcher_polls_until_stopped() {
        let index = Moving::new("1");
        let cache = Arc::new(Cache::new());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let watcher = tokio::spawn(watch_generation(
            index.clone(),
            Arc::clone(&cache),
            Duration::from_millis(5),
            async {
                let _ = stopped.await;
            },
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while index.reads.load(Ordering::SeqCst) == 0 {
            assert!(tokio::time::Instant::now() < deadline, "the watcher never read the generation");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        cache.insert("view".into(), vec![1]);
        index.set("2");
        while cache.get("view").is_some() {
            assert!(tokio::time::Instant::now() < deadline, "the watcher never cleared the cache");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(stop);
        tokio::time::timeout(Duration::from_secs(5), watcher).await.expect("watcher stops").expect("join");
    }

    #[tokio::test]
    async fn a_mounted_extension_serves_and_runs_its_lifecycle_once() {
        static MOUNTED: [&dyn Extension; 1] = [&PROBE];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let addr = listener.local_addr().expect("address");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(&MOUNTED, context(None), listener, async {
            let _ = stopped.await;
        }));

        assert_eq!(fetch(addr, "/api/v1/probe").await, (200, "probe".into()));
        let (status, body) = fetch(addr, "/health").await;
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["extensions"], json!(["probe"]));
        let (status, body) = fetch(addr, "/api/v1/openapi.json").await;
        assert_eq!(status, 200);
        assert!(serde_json::from_str::<Value>(&body).unwrap()["paths"]["/api/v1/probe"].is_object());
        assert_eq!(PROBE.started.load(Ordering::SeqCst), 1);
        assert_eq!(PROBE.stopped.load(Ordering::SeqCst), 0);

        stop.send(()).expect("stop");
        server.await.expect("join").expect("serve");
        assert_eq!(PROBE.started.load(Ordering::SeqCst), 1);
        assert_eq!(PROBE.stopped.load(Ordering::SeqCst), 1);
    }
}
