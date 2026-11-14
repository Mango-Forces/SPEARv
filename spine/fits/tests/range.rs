mod common;
use async_trait::async_trait;
use spearv_fits::{
    run, Budget, Cancellation, Error, FloatMode, Input, ReadAt, ReadTrace, Result, Window,
};
use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream as AsyncStream,
    sync::Notify,
};

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Full,
    Changed,
    Precondition,
    Interval,
    Total,
    ShortLength,
    LongLength,
    ShortBody,
    OverBody,
    ChunkedOver,
    RetryOnce,
    Deadline,
    Held,
    Panic,
    ShortVector,
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Mutex<bool>,
    event: Condvar,
}
impl Gate {
    fn hold(&self) {
        self.entered.notify_one();
        let mut release = self.release.lock().expect("gate lock");
        while !*release {
            release = self.event.wait(release).expect("gate event");
        }
    }
    fn release(&self) {
        *self.release.lock().expect("gate lock") = true;
        self.event.notify_all();
    }
}
struct Server {
    address: String,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    requests: Arc<AtomicU64>,
    gate: Arc<Gate>,
}
impl Server {
    fn start(path: PathBuf, fault: Fault) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("address").to_string();
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(Gate::default());
        let (stop, count, held) = (stopped.clone(), requests.clone(), gate.clone());
        let handle = thread::spawn(move || {
            let file = Arc::new(File::open(path).expect("retained local fixture"));
            let size = file.metadata().expect("size").len();
            let mut clients = Vec::new();
            for stream in listener.incoming() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let mut stream = stream.expect("loopback accept");
                let attempt = count.fetch_add(1, Ordering::Relaxed);
                assert!(attempt < 1024, "bounded test server request ceiling");
                let (file, held) = (file.clone(), held.clone());
                clients.push(thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
                    let mut head = Vec::new();
                    let mut byte = [0u8;1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).is_err() { return; }
                        head.push(byte[0]); assert!(head.len() <= 8192);
                    }
                    let text = String::from_utf8(head).expect("HTTP request");
                    assert!(text.lines().any(|line| line.eq_ignore_ascii_case("if-match: \"fixture-v1\"")));
                    let range = text.lines().find_map(|line| line.strip_prefix("range: bytes=")).expect("exact range");
                    let (start, end) = range.split_once('-').expect("range");
                    let (start, end) = (start.parse::<u64>().expect("start"), end.parse::<u64>().expect("end"));
                    assert!(end >= start && end < size && end - start < 65_536);
                    if matches!(fault, Fault::Held | Fault::Deadline) { held.hold(); }
                    if matches!(fault, Fault::Full | Fault::Precondition) || matches!(fault, Fault::RetryOnce) && attempt == 0 {
                        let status = if matches!(fault, Fault::Full) { "200 OK" } else if matches!(fault, Fault::Precondition) { "412 Precondition Failed" } else { "503 Service Unavailable" };
                        let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    }
                    let len = (end - start + 1) as usize;
                    let mut body = vec![0u8; len];
                    file.read_exact_at(&mut body, start).expect("local slice");
                    let returned_start = if matches!(fault, Fault::Interval) { start + 1 } else { start };
                    let returned_total = if matches!(fault, Fault::Total) { size + 1 } else { size };
                    let etag = if matches!(fault, Fault::Changed) { "\"changed\"" } else { "\"fixture-v1\"" };
                    if matches!(fault, Fault::ChunkedOver) {
                        if write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {returned_start}-{end}/{returned_total}\r\nETag: {etag}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n", len + 1).is_err() { return; }
                        body.push(1);
                        let _ = stream.write_all(&body);
                        let _ = stream.write_all(b"\r\n0\r\n\r\n");
                        return;
                    }
                    let declared = if matches!(fault, Fault::ShortLength) { len - 1 } else if matches!(fault, Fault::LongLength | Fault::OverBody) { len + 1 } else { len };
                    if write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {returned_start}-{end}/{returned_total}\r\nETag: {etag}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n").is_err() { return; }
                    if matches!(fault, Fault::ShortBody) { body.pop(); }
                    if matches!(fault, Fault::OverBody) { body.push(1); }
                    let _ = stream.write_all(&body);
                }));
            }
            for client in clients {
                client.join().expect("test HTTP handler");
            }
        });
        Self {
            address,
            stopped,
            thread: Some(handle),
            requests,
            gate,
        }
    }
    fn reader(&self, size: u64, fault: Fault) -> Arc<HttpReader> {
        Arc::new(HttpReader {
            address: self.address.clone(),
            size,
            fault,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.gate.release();
        self.stopped.store(true, Ordering::Release);
        let _ = TcpStream::connect(&self.address);
        self.thread
            .take()
            .expect("server handle")
            .join()
            .expect("loopback server");
    }
}
// Deliberately tiny fixture HTTP/1.1 client over real asynchronous Tokio TCP.
// Not a product transport implementation or replacement for Source HttpFile.
struct HttpReader {
    address: String,
    size: u64,
    fault: Fault,
}
#[async_trait]
impl ReadAt for HttpReader {
    async fn read_at(
        &self,
        start: u64,
        len: usize,
        cancel: &Cancellation,
        trace: &ReadTrace,
    ) -> Result<Vec<u8>> {
        if matches!(self.fault, Fault::Panic) {
            panic!("intentional callback panic");
        }
        if matches!(self.fault, Fault::ShortVector) {
            return Ok(vec![1]);
        }
        for attempt in 1..=3 {
            cancel.check()?;
            trace.attempt();
            let timeout = Duration::from_millis(if matches!(self.fault, Fault::Deadline) {
                50
            } else {
                5000
            });
            let (mut stream, header) = tokio::time::timeout(timeout, async {
                let mut stream = AsyncStream::connect(&self.address).await.map_err(|_| Error::Transient("loopback connect failed".into()))?;
                stream.write_all(format!("GET /fixture HTTP/1.1\r\nhost: {}\r\nrange: bytes={start}-{}\r\nif-match: \"fixture-v1\"\r\nconnection: close\r\n\r\n",
                    self.address, start + len as u64 - 1).as_bytes()).await.map_err(|_| Error::Transient("loopback send failed".into()))?;
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.map_err(|_| Error::ShortRead)?);
                    if header.len() > 8192 { return Err(Error::MalformedRange("fixture header limit".into())); }
                }
                Ok::<_,Error>((stream, String::from_utf8(header).map_err(|_| Error::MalformedRange("fixture header encoding".into()))?))
            }).await.map_err(|_| Error::Deadline)??;
            let status = header
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            if status == "503" && attempt < 3 {
                continue;
            }
            if status == "412" {
                return Err(Error::ContentChanged);
            }
            if status != "206" {
                return Err(Error::MalformedRange(
                    "full response refused before body".into(),
                ));
            }
            let field = |name: &str| {
                header.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name).then_some(value.trim())
                })
            };
            let expected = format!("bytes {start}-{}/{}", start + len as u64 - 1, self.size);
            if field("Content-Range") != Some(expected.as_str()) {
                return Err(Error::MalformedRange(
                    "interval or logical total changed".into(),
                ));
            }
            if field("ETag") != Some("\"fixture-v1\"") {
                return Err(Error::ContentChanged);
            }
            if field("Content-Length")
                .is_some_and(|declared| declared.parse::<usize>().ok() != Some(len))
            {
                return Err(Error::MalformedRange("declared body length changed".into()));
            }
            let mut bytes = Vec::with_capacity(len);
            if field("Transfer-Encoding") == Some("chunked") {
                let mut chunk_header = Vec::new();
                while !chunk_header.ends_with(b"\r\n") {
                    chunk_header.push(stream.read_u8().await.map_err(|_| Error::ShortRead)?);
                    if chunk_header.len() > 32 {
                        return Err(Error::MalformedRange("fixture chunk size".into()));
                    }
                }
                let chunk_len = usize::from_str_radix(
                    std::str::from_utf8(&chunk_header)
                        .map_err(|_| Error::ShortRead)?
                        .trim(),
                    16,
                )
                .map_err(|_| Error::ShortRead)?;
                if chunk_len > len {
                    return Err(Error::MalformedRange(
                        "streamed chunk exceeds exact range".into(),
                    ));
                }
            }
            let mut scratch = [0u8; 8192];
            loop {
                let n = tokio::time::timeout(timeout, stream.read(&mut scratch))
                    .await
                    .map_err(|_| Error::Deadline)?
                    .map_err(|_| Error::ShortRead)?;
                if n == 0 {
                    break;
                }
                trace.received(n);
                cancel.check()?;
                if bytes.len() + n > len {
                    return Err(Error::MalformedRange(
                        "streamed body exceeds exact range".into(),
                    ));
                }
                bytes.extend_from_slice(&scratch[..n]);
            }
            if bytes.len() != len {
                return Err(Error::ShortRead);
            }
            return Ok(bytes);
        }
        Err(Error::Transient("three bounded attempts exhausted".into()))
    }
}

fn retained() -> Vec<PathBuf> {
    let Some(root) = std::env::var_os("SPEARV_FITS_RETAINED_DIR") else {
        return Vec::new();
    };
    let root = PathBuf::from(root);
    [
        "level2_2025W34_2B_0501_1D2_spx_l2b-v20-2025-267.fits",
        "level2_2025W34_2B_0001_1D4_spx_l2b-v27-2026-229.fits",
        "level2_2026W21_1A_0001_1D1_spx_l2b-v27-2026-268.fits",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qr2_qr3_dr1_actual_range_local_prefix_and_two_independent_handles() {
    let mut paths = retained();
    if paths.is_empty() {
        paths.push(common::fixture());
    }
    for path in paths {
        let size = std::fs::metadata(&path)
            .expect("retained input is required when directory supplied")
            .len();
        let real = path.file_name() != Some(&OsString::from("types.fits")) && size > 1_000_000;
        let server = Server::start(path.clone(), Fault::None);
        let budget = Budget::new(64 * 1024 * 1024);
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let reader = server.reader(size, Fault::None);
            let budget = budget.clone();
            let local = path.clone();
            jobs.push(tokio::spawn(async move {
                let range = run(
                    Input::Random { size, reader },
                    budget.clone(),
                    common::limits(),
                    Cancellation::default(),
                    move |fits| {
                        let mut results = Vec::new();
                        if !real {
                            fits.move_named("SIGNED", 0)?;
                            let values = fits.read_flags(common::window())?;
                            return Ok(vec![values
                                .values()
                                .iter()
                                .map(|v| *v as u64)
                                .collect::<Vec<_>>()]);
                        }
                        for window in [
                            Window {
                                x: 988,
                                y: 988,
                                width: 37,
                                height: 19,
                            },
                            Window {
                                x: 0,
                                y: 0,
                                width: 1,
                                height: 3,
                            },
                            Window {
                                x: 2038,
                                y: 2039,
                                width: 2,
                                height: 1,
                            },
                        ] {
                            for extension in ["IMAGE", "VARIANCE", "ZODI"] {
                                fits.move_named(extension, 0)?;
                                results.push(
                                    fits.read_f32(window, FloatMode::Automatic)?
                                        .values()
                                        .iter()
                                        .map(|v| v.to_bits() as u64)
                                        .collect(),
                                );
                            }
                            fits.move_named("FLAGS", 0)?;
                            results.push(
                                fits.read_flags(window)?
                                    .values()
                                    .iter()
                                    .map(|v| *v as u64)
                                    .collect(),
                            );
                        }
                        fits.move_named("WCS-WAVE", 0)?;
                        for col in 1..=3 {
                            let shape = fits.column_info(col)?;
                            results.push(
                                fits.read_column_f64(col, 1, 1, shape.repeat as usize)?
                                    .values()
                                    .iter()
                                    .map(|v| v.to_bits())
                                    .collect(),
                            );
                        }
                        Ok(results)
                    },
                )
                .await
                .expect("current native sparse range");
                assert!(range.io.peak_resident_bytes <= 262_144);
                assert_eq!(range.io.closes, 1);
                assert!(
                    range.io.transferred_bytes < size,
                    "no complete object for real/fixture operation"
                );
                let oracle = run(
                    Input::Local(local),
                    budget,
                    common::limits(),
                    Cancellation::default(),
                    move |fits| {
                        let mut results = Vec::new();
                        if !real {
                            fits.move_named("SIGNED", 0)?;
                            return Ok(vec![fits
                                .read_flags(common::window())?
                                .values()
                                .iter()
                                .map(|v| *v as u64)
                                .collect::<Vec<_>>()]);
                        }
                        for window in [
                            Window {
                                x: 988,
                                y: 988,
                                width: 37,
                                height: 19,
                            },
                            Window {
                                x: 0,
                                y: 0,
                                width: 1,
                                height: 3,
                            },
                            Window {
                                x: 2038,
                                y: 2039,
                                width: 2,
                                height: 1,
                            },
                        ] {
                            for extension in ["IMAGE", "VARIANCE", "ZODI"] {
                                fits.move_named(extension, 0)?;
                                results.push(
                                    fits.read_f32(window, FloatMode::Automatic)?
                                        .values()
                                        .iter()
                                        .map(|v| v.to_bits() as u64)
                                        .collect(),
                                );
                            }
                            fits.move_named("FLAGS", 0)?;
                            results.push(
                                fits.read_flags(window)?
                                    .values()
                                    .iter()
                                    .map(|v| *v as u64)
                                    .collect(),
                            );
                        }
                        fits.move_named("WCS-WAVE", 0)?;
                        for col in 1..=3 {
                            let shape = fits.column_info(col)?;
                            results.push(
                                fits.read_column_f64(col, 1, 1, shape.repeat as usize)?
                                    .values()
                                    .iter()
                                    .map(|v| v.to_bits())
                                    .collect(),
                            );
                        }
                        Ok(results)
                    },
                )
                .await
                .expect("separate local control");
                assert_eq!(range.value, oracle.value);
                println!(
                    "range logical_bytes={size} actual_bytes={} attempts={} cache_peak={}",
                    range.io.transferred_bytes, range.io.attempts, range.io.peak_resident_bytes
                );
            }));
        }
        for job in jobs {
            job.await.expect("two concurrent independent handles");
        }
        assert_eq!(budget.used_bytes(), 0);
        if real {
            let mut bytes = vec![0u8; 65_536];
            File::open(path)
                .expect("input")
                .read_exact(&mut bytes)
                .expect("exact prefix");
            let prefix = run(
                Input::Prefix(bytes),
                budget.clone(),
                common::limits(),
                Cancellation::default(),
                |fits| {
                    fits.move_named("IMAGE", 0)?;
                    assert_eq!(fits.image_info()?.axes(), [2040, 2040]);
                    assert_eq!(&*fits.key_string("BUNIT")?.expect("unit"), "MJy / sr");
                    fits.header()
                },
            )
            .await
            .expect("65,536 real prefix");
            assert!(prefix
                .value
                .cards()
                .iter()
                .any(|card| card.name == "CTYPE1"));
            drop(prefix);
            assert_eq!(budget.used_bytes(), 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_faults_callback_panic_retries_and_close_cleanup() {
    let path = common::fixture();
    let size = std::fs::metadata(&path).expect("fixture size").len();
    let budget = Budget::new(32 * 1024 * 1024);
    for fault in [
        Fault::Full,
        Fault::Changed,
        Fault::Precondition,
        Fault::Interval,
        Fault::Total,
        Fault::ShortLength,
        Fault::LongLength,
        Fault::ShortBody,
        Fault::OverBody,
        Fault::ChunkedOver,
        Fault::Panic,
        Fault::ShortVector,
        Fault::Deadline,
    ] {
        let server = Server::start(path.clone(), fault);
        let token = Cancellation::default();
        let result = run(
            Input::Random {
                size,
                reader: server.reader(size, fault),
            },
            budget.clone(),
            common::limits(),
            token.clone(),
            |fits| {
                fits.move_named("FLOAT", 0)?;
                fits.read_f32(common::window(), FloatMode::Automatic)
            },
        )
        .await;
        match fault {
            Fault::Changed | Fault::Precondition => {
                assert!(matches!(result, Err(Error::ContentChanged)))
            }
            Fault::ShortBody | Fault::ShortVector => {
                assert!(matches!(result, Err(Error::ShortRead)))
            }
            Fault::Panic => assert!(matches!(result, Err(Error::CallbackPanic))),
            Fault::Deadline => assert!(matches!(result, Err(Error::Deadline))),
            _ => assert!(
                matches!(result, Err(Error::MalformedRange(_))),
                "{fault:?}: {result:?}"
            ),
        }
        token.wait_idle().await;
        assert_eq!(budget.used_bytes(), 0, "{fault:?} cleanup");
        assert_eq!(token.io_stats().closes, 1, "{fault:?} actual native close");
        if !matches!(fault, Fault::Panic | Fault::ShortVector) {
            assert_eq!(
                token.io_stats().attempts,
                1,
                "{fault:?} failed attempt retained"
            );
        }
        assert!(
            server.requests.load(Ordering::Relaxed) <= 1,
            "malformed/cancel/content faults are nonretryable"
        );
    }
    let server = Server::start(path, Fault::RetryOnce);
    let done = run(
        Input::Random {
            size,
            reader: server.reader(size, Fault::RetryOnce),
        },
        budget.clone(),
        common::limits(),
        Cancellation::default(),
        |fits| {
            fits.move_named("SIGNED", 0)?;
            fits.read_flags(common::window())
        },
    )
    .await
    .expect("transient once then exact retry");
    assert_eq!(done.io.attempts, done.io.backing_reads + 1);
    drop(done);
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_http_request_and_started_native_scope_keep_leases_until_actual_exit() {
    let path = common::fixture();
    let size = std::fs::metadata(&path).expect("size").len();
    let server = Server::start(path.clone(), Fault::Held);
    let budget = Budget::new(32 * 1024 * 1024);
    let token = Cancellation::default();
    let (reader, worker_budget, worker_token) = (
        server.reader(size, Fault::Held),
        budget.clone(),
        token.clone(),
    );
    let job = tokio::spawn(async move {
        run(
            Input::Random { size, reader },
            worker_budget,
            common::limits(),
            worker_token,
            |_| Ok(()),
        )
        .await
    });
    server.gate.entered.notified().await;
    assert_eq!(token.active_workers(), 1);
    assert!(budget.used_bytes() > 0);
    job.abort();
    assert!(job.await.expect_err("outer request aborted").is_cancelled());
    token.wait_idle().await;
    assert_eq!(
        budget.used_bytes(),
        0,
        "native open exits and closes after pending HTTP future cancellation"
    );
    assert_eq!(token.io_stats().closes, 1);
    assert_eq!(
        token.io_stats().attempts,
        1,
        "cancelled attempt is still attributed"
    );
    server.gate.release();
    drop(server);
    let server = Server::start(path, Fault::None);
    let token = Cancellation::default();
    let gate = Arc::new(Gate::default());
    let published = Arc::new(AtomicBool::new(false));
    let (reader, worker_budget, worker_token, held, worker_published) = (
        server.reader(size, Fault::None),
        budget.clone(),
        token.clone(),
        gate.clone(),
        published.clone(),
    );
    let job = tokio::spawn(async move {
        let result = run(
            Input::Random { size, reader },
            worker_budget,
            common::limits(),
            worker_token,
            move |fits| {
                fits.move_named("SIGNED", 0)?;
                let output = fits.read_flags(common::window())?;
                held.hold(); // deterministic live native-handle scope, not hard-kill proof
                Ok(output)
            },
        )
        .await?;
        worker_published.store(true, Ordering::Release);
        Ok::<_, Error>(result)
    });
    gate.entered.notified().await;
    let reads = server.requests.load(Ordering::Relaxed);
    job.abort();
    assert!(job.await.expect_err("outer request aborted").is_cancelled());
    assert_eq!(token.active_workers(), 1);
    assert!(
        budget.used_bytes() > 0,
        "started native scope still owns backing/output/reservation"
    );
    gate.release();
    token.wait_idle().await;
    assert_eq!(budget.used_bytes(), 0);
    assert_eq!(token.io_stats().closes, 1);
    assert_eq!(
        server.requests.load(Ordering::Relaxed),
        reads,
        "cancelled scope starts no further reads"
    );
    assert!(
        !published.load(Ordering::Acquire),
        "no cancelled/stale publication"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_panic_after_native_open_closes_backing_and_releases_charges() {
    let path = common::fixture();
    let size = std::fs::metadata(&path).expect("size").len();
    let server = Server::start(path, Fault::None);
    let token = Cancellation::default();
    let budget = Budget::new(32 * 1024 * 1024);
    let result = run(
        Input::Random {
            size,
            reader: server.reader(size, Fault::None),
        },
        budget.clone(),
        common::limits(),
        token.clone(),
        |fits| {
            fits.move_named("SIGNED", 0)?;
            let _output = fits.read_flags(common::window())?;
            panic!("intentional operation panic after native decode");
            #[allow(unreachable_code)] // The test deliberately exercises worker unwind cleanup.
            Ok::<_, Error>(())
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Worker(_))));
    token.wait_idle().await;
    assert_eq!(token.io_stats().closes, 1);
    assert_eq!(budget.used_bytes(), 0);
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_read_page_bytes_and_read_spans_track_actual_reads() {
    let path = common::fixture();
    let size = std::fs::metadata(&path).expect("size").len();
    let server = Server::start(path, Fault::None);
    let budget = Budget::new(32 * 1024 * 1024);
    let completed = run(
        Input::Random {
            size,
            reader: server.reader(size, Fault::None),
        },
        budget,
        common::limits(),
        Cancellation::default(),
        |fits| {
            assert!(matches!(fits.set_read_page_bytes(0), Err(Error::Bounds(_))));
            assert!(matches!(
                fits.set_read_page_bytes(common::limits().page_bytes + 1),
                Err(Error::Bounds(_))
            ));
            fits.set_read_page_bytes(2880)?;
            fits.move_named("SIGNED", 0)?;
            let header_spans = fits.read_spans();
            assert!(!header_spans.is_empty());
            assert!(header_spans.iter().all(|(_, len)| *len > 0));
            fits.set_read_page_bytes(common::limits().page_bytes)?;
            let values = fits.read_flags(common::window())?;
            Ok((values.values().to_vec(), fits.read_spans()))
        },
    )
    .await
    .expect("range read");
    assert!(!completed.value.0.is_empty());
    assert!(!completed.value.1.is_empty());
}
