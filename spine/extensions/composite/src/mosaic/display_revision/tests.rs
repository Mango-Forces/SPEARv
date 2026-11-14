use super::super::{color_display::defaults, recipe, MosaicState};
use super::*;
use crate::{parse_query, view_of, ColorBands};
use axum::extract::FromRequest;
use axum::http::Request;
use spearv_core::{TanGrid, PIXEL_SCALE_DEG};

fn math() -> (Arc<spearv_core::WorkBudget>, Arc<dyn spearv_core::math::ImageMath>) {
    let (budget, math) = crate::testing::math();
    (budget, math)
}

fn fixture(width: u32, height: u32) -> (Arc<Jobs>, Arc<Job>, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let jobs = Arc::new(Jobs::default());
    let view = view_of(&parse_query("ra=10&dec=20&fov_l=0.01&fov_h=0.01").unwrap()).unwrap();
    let grid = TanGrid::new_rect(
        10.0,
        20.0,
        width as f64 * PIXEL_SCALE_DEG,
        height as f64 * PIXEL_SCALE_DEG,
    );
    let (budget, math) = math();
    let mut job = Job::new(
        "m0".into(),
        "science-key".into(),
        grid,
        17,
        (recipe(&view, true, ColorBands::DEFAULT).unwrap(), true),
        math,
        budget,
    );
    job.display_revisions.cache = DisplayCache::fixture(root.path(), 1024 * 1024, 2);
    let job = Arc::new(job);
    job.prepare();
    for channel in 0..3 {
        job.publish_test_values(channel, |x, y| {
            let pixel = y * width + x;
            if pixel == 0 || (pixel == 1 && channel == 1) {
                return f32::NAN;
            }
            (pixel + channel as u32 * 3) as f32
        });
    }
    job.finish();
    jobs.list.lock().unwrap().push(Arc::clone(&job));
    (jobs, job, root)
}

fn cached_job(source: &Job, cache: &Arc<DisplayCache>, id: &str) -> Arc<Job> {
    let (budget, math) = math();
    let mut job = Job::new(
        id.into(),
        id.into(),
        source.grid.clone(),
        0,
        (source.recipe.clone(), true),
        math,
        budget,
    );
    job.display_revisions.cache = Arc::clone(cache);
    let job = Arc::new(job);
    job.prepare();
    job.finish();
    job
}

async fn post(jobs: &Arc<Jobs>, id: &str, json: &str) -> Response {
    let request = Request::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json.to_owned()))
        .unwrap();
    create(
        Extension(Arc::clone(jobs)),
        Path(id.into()),
        Json::<ColorDisplay>::from_request(request, &()).await,
    )
    .await
}

async fn json(response: Response) -> serde_json::Value {
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn wait(job: &Job, token: &str, full: bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let notify = job.display_revisions.notify.notified();
            tokio::pin!(notify);
            notify.as_mut().enable();
            let revision = job.display_revisions.find(token).unwrap();
            let done = {
                let artifacts = revision.artifacts.lock().unwrap();
                let artifact = if full {
                    &artifacts.full
                } else {
                    &artifacts.preview
                };
                !matches!(
                    artifact.state,
                    ArtifactState::Queued | ArtifactState::Rendering
                )
            };
            if done {
                return;
            }
            notify.await;
        }
    })
    .await
    .unwrap();
}

fn path(job: &Job, token: &str) -> Path<(String, String)> {
    Path((job.id.clone(), token.into()))
}

async fn image(response: Response) -> (u32, u32, Vec<u8>) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let length: usize = response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(bytes.len(), length);
    assert_eq!(&bytes[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    assert_eq!(&bytes[8..16], &[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
    assert_eq!(
        &bytes[bytes.len() - 12..],
        &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 174, 66, 96, 130]
    );
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut rgb).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    rgb.truncate(info.buffer_size());
    (info.width, info.height, rgb)
}

#[tokio::test]
async fn native_rectangular_default_layer_composition_and_no_science_changes() {
    let (jobs, job, _root) = fixture(3, 2);
    let metadata = job.raw_metadata().unwrap();
    let before = job.status(0);
    let key = job.key.clone();
    let response = post(&jobs, &job.id, &serde_json::to_string(&defaults()).unwrap()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let manifest = json(response).await;
    let token = manifest["revision"].as_str().unwrap();
    assert_eq!(manifest["job_id"], job.id);
    assert_eq!(manifest["current"], true);
    assert_eq!(
        (manifest["width"].as_u64(), manifest["height"].as_u64()),
        (Some(3), Some(2))
    );
    assert_eq!(
        manifest["display"],
        serde_json::to_value(defaults()).unwrap()
    );
    assert_eq!(manifest["full"]["state"], "not_requested");
    assert_eq!(manifest["full"]["bytes"], serde_json::Value::Null);
    assert_eq!(manifest["full"]["error"], serde_json::Value::Null);
    assert_eq!(manifest["full"]["completed_rows"], 0);
    assert_eq!(manifest["full"]["total_rows"], 2);
    assert_eq!(
        manifest["channels"],
        serde_json::to_value(&metadata.channels).unwrap()
    );
    wait(&job, token, false).await;
    let preview_pixels =
        image(preview(Extension(Arc::clone(&jobs)), path(&job, token)).await).await;
    assert_eq!(
        full(Extension(Arc::clone(&jobs)), path(&job, token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&job, token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&job, token))
            .await
            .status(),
        StatusCode::OK
    );
    wait(&job, token, true).await;
    let full_pixels = image(full(Extension(Arc::clone(&jobs)), path(&job, token)).await).await;
    assert_eq!(full_pixels, preview_pixels);
    let mut old_composition = vec![0_u8; 3 * 2 * 3];
    for channel in 0..3 {
        let (_, _, layer) = job.tile_layer(channel, 0, 0).unwrap();
        for (pixel, layer) in old_composition.iter_mut().zip(layer) {
            *pixel = pixel.saturating_add(layer);
        }
    }
    assert_eq!(full_pixels, (3, 2, old_composition));
    assert_eq!(
        &full_pixels.2[9..12],
        &[0; 3],
        "bottom-left all-missing is BLACK, not NODATA"
    );
    let manifest = json(status(Extension(Arc::clone(&jobs)), path(&job, token)).await).await;
    assert_eq!(manifest["full"]["completed_rows"], 2);
    assert_eq!(manifest["full"]["total_rows"], 2);
    assert_eq!(manifest["preview"]["completed_rows"], 2);
    assert_eq!(job.key, key);
    assert_eq!(job.raw_metadata().unwrap(), metadata);
    let after = job.status(0);
    assert_eq!(
        (
            after.seq,
            after.exposures_done,
            after.exposures_total,
            after.bytes_read,
            after.reads_failed
        ),
        (
            before.seq,
            before.exposures_done,
            before.exposures_total,
            before.bytes_read,
            before.reads_failed
        )
    );
    let directory = job.display_revisions.cache.owned_path();
    assert_eq!(
        std::fs::read_dir(directory).unwrap().count(),
        1,
        "only published PNG remains"
    );
}

#[tokio::test]
async fn malformed_inputs_and_route_state_matrix_touch_activity() {
    let (jobs, job, _root) = fixture(3, 2);
    let body = serde_json::to_string(&defaults()).unwrap();
    for bad in ["{", "{}", "{\"r\":{}}", "null", "{\"unexpected\":true}"] {
        let response = post(&jobs, &job.id, bad).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(json(response).await["error"].is_string());
    }
    assert_eq!(
        post(&jobs, "absent", &body).await.status(),
        StatusCode::NOT_FOUND
    );
    let response = post(&jobs, &job.id, &body).await;
    let manifest = json(response).await;
    let token = manifest["revision"].as_str().unwrap();
    wait(&job, token, false).await;
    for (state, expected) in [
        (MosaicState::Planning, StatusCode::CONFLICT),
        (MosaicState::Reading, StatusCode::CONFLICT),
        (MosaicState::Failed, StatusCode::GONE),
        (MosaicState::Cancelled, StatusCode::GONE),
    ] {
        job.lock().state = state;
        assert_eq!(post(&jobs, &job.id, &body).await.status(), expected);
        assert_eq!(
            status(Extension(Arc::clone(&jobs)), path(&job, token))
                .await
                .status(),
            expected
        );
        assert_eq!(
            preview(Extension(Arc::clone(&jobs)), path(&job, token))
                .await
                .status(),
            expected
        );
        assert_eq!(
            request_full(Extension(Arc::clone(&jobs)), path(&job, token))
                .await
                .status(),
            expected
        );
        assert_eq!(
            full(Extension(Arc::clone(&jobs)), path(&job, token))
                .await
                .status(),
            expected
        );
    }
    job.lock().state = MosaicState::Done;
    for route in 0..3 {
        let old = std::time::Instant::now() - std::time::Duration::from_secs(1);
        *job.last_activity.lock().unwrap() = old;
        let response = match route {
            0 => status(Extension(Arc::clone(&jobs)), path(&job, "absent")).await,
            1 => preview(Extension(Arc::clone(&jobs)), path(&job, "absent")).await,
            _ => full(Extension(Arc::clone(&jobs)), path(&job, "absent")).await,
        };
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(*job.last_activity.lock().unwrap() > old);
        assert!(json(response).await["error"].is_string());
    }
    let (budget, math) = math();
    let mut non_color = Job::new(
        "grey".into(),
        "grey".into(),
        job.grid.clone(),
        0,
        (
            recipe(
                &view_of(&parse_query("ra=10&dec=20&fov_l=0.01&fov_h=0.01").unwrap()).unwrap(),
                false,
                ColorBands::DEFAULT,
            )
            .unwrap(),
            true,
        ),
        math,
        budget,
    );
    non_color.display_revisions.cache = Arc::clone(&job.display_revisions.cache);
    jobs.list.lock().unwrap().push(Arc::new(non_color));
    assert_eq!(
        post(&jobs, "grey", &body).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn coalesced_latest_work_and_open_stream_survives_supersede() {
    let (jobs, job, _root) = fixture(3, 2);
    let body = serde_json::to_string(&defaults()).unwrap();
    let manifest = json(post(&jobs, &job.id, &body).await).await;
    let token = manifest["revision"].as_str().unwrap().to_owned();
    wait(&job, &token, false).await;
    let duplicate = post(&jobs, &job.id, &body).await;
    assert_eq!(duplicate.status(), StatusCode::OK);
    assert_eq!(json(duplicate).await["revision"], token);
    let permit = Arc::clone(&job.display_revisions.cache.full_slot)
        .acquire_owned()
        .await
        .unwrap();
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&job, &token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        full(Extension(Arc::clone(&jobs)), path(&job, &token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let mut settings = defaults();
    settings.r.gain = 0.0;
    let new_manifest =
        json(post(&jobs, &job.id, &serde_json::to_string(&settings).unwrap()).await).await;
    let new_token = new_manifest["revision"].as_str().unwrap().to_owned();
    wait(&job, &new_token, false).await;
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&job, &token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        preview(Extension(Arc::clone(&jobs)), path(&job, &token))
            .await
            .status(),
        StatusCode::GONE
    );
    assert_eq!(
        full(Extension(Arc::clone(&jobs)), path(&job, &token))
            .await
            .status(),
        StatusCode::GONE
    );
    assert_eq!(
        json(status(Extension(Arc::clone(&jobs)), path(&job, &token)).await).await["full"]["state"],
        "superseded"
    );
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&job, &new_token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    drop(permit);
    wait(&job, &new_token, true).await;
    let response = full(Extension(Arc::clone(&jobs)), path(&job, &new_token)).await;
    let old_path = job
        .display_revisions
        .find(&new_token)
        .unwrap()
        .artifacts
        .lock()
        .unwrap()
        .file
        .as_ref()
        .unwrap()
        .path
        .clone();
    settings.g.gain = 0.0;
    json(post(&jobs, &job.id, &serde_json::to_string(&settings).unwrap()).await).await;
    assert!(!old_path.exists());
    let pixels = image(response).await;
    assert!(
        pixels.2.as_chunks::<3>().0.iter().all(|p| p[0] == 0),
        "edited red setting is frozen in the opened stream"
    );
    assert!(job.display_revisions.store.lock().unwrap().revisions.len() <= 2);
    assert!(job.display_revisions.find(&token).is_none());
}

#[tokio::test]
async fn preview_queue_is_latest_only_and_canonical_requests_deduplicate() {
    let (_jobs, job, _root) = fixture(3, 2);
    let (first, fresh) = job.display_revisions.accept(&job, defaults()).unwrap();
    assert!(fresh);
    let mut settings = defaults();
    settings.r.tint = "#ABCDEF".into();
    settings.r.gain = 999.0;
    let normalized = settings.normalize().unwrap();
    let (latest, fresh) = job
        .display_revisions
        .accept(&job, normalized.clone())
        .unwrap();
    assert!(fresh);
    let (duplicate, fresh) = job.display_revisions.accept(&job, normalized).unwrap();
    assert!(!fresh);
    assert!(Arc::ptr_eq(&duplicate, &latest));
    kick_preview(&job);
    wait(&job, &latest.token, false).await;
    assert_eq!(
        first.artifacts.lock().unwrap().preview.state,
        ArtifactState::Superseded
    );
    assert!(first.artifacts.lock().unwrap().preview_bytes.is_none());
    assert_eq!(
        latest.artifacts.lock().unwrap().preview.state,
        ArtifactState::Ready
    );
    let manifest = latest.manifest(&job);
    assert_eq!(manifest.display.r.tint, "#abcdef");
    assert_eq!(manifest.display.r.gain, 2.0);
}

#[tokio::test]
async fn cache_failure_is_visible_no_partial_serving_and_current_retry_works() {
    let (jobs, job, root) = fixture(3, 2);
    let blocker = root.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    // A recoverable filesystem failure, not a sticky failed cache initializer.
    let (budget, math) = math();
    let mut isolated = Job::new(
        "retry".into(),
        "retry".into(),
        job.grid.clone(),
        0,
        (job.recipe.clone(), true),
        math,
        budget,
    );
    isolated.display_revisions.cache = DisplayCache::fixture(&blocker, 1024 * 1024, 2);
    let isolated = Arc::new(isolated);
    isolated.prepare();
    isolated.finish();
    jobs.list.lock().unwrap().push(Arc::clone(&isolated));
    let manifest = json(
        post(
            &jobs,
            &isolated.id,
            &serde_json::to_string(&defaults()).unwrap(),
        )
        .await,
    )
    .await;
    let token = manifest["revision"].as_str().unwrap();
    wait(&isolated, token, false).await;
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&isolated, token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    wait(&isolated, token, true).await;
    assert_eq!(
        isolated
            .display_revisions
            .find(token)
            .unwrap()
            .artifacts
            .lock()
            .unwrap()
            .full
            .state,
        ArtifactState::Failed
    );
    let response = full(Extension(Arc::clone(&jobs)), path(&isolated, token)).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(json(response).await["error"].is_string());
    std::fs::remove_file(&blocker).unwrap();
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&isolated, token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    wait(&isolated, token, true).await;
    image(full(Extension(Arc::clone(&jobs)), path(&isolated, token)).await).await;
    assert!(
        std::fs::read_dir(isolated.display_revisions.cache.owned_path())
            .unwrap()
            .all(|e| e.unwrap().path().extension().unwrap() == "png")
    );
    // A byte-cap failure leaves no unpublished PNG/part file.
    let tiny = DisplayCache::fixture(root.path(), 1, 2);
    let revision = job.display_revisions.accept(&job, defaults()).unwrap().0;
    assert!(tiny.render_full(&job, &revision).is_err());
    assert_eq!(std::fs::read_dir(tiny.owned_path()).unwrap().count(), 0);
    assert!(revision.artifacts.lock().unwrap().file.is_none());
}

#[tokio::test]
async fn cache_file_and_byte_limits_evict_only_owned_files_and_open_stream_stays_complete() {
    let (jobs, first, root) = fixture(3, 2);
    let cache = DisplayCache::fixture(root.path(), 1024 * 1024, 2);
    std::fs::write(root.path().join("unrelated.png"), b"user file").unwrap();
    let mut revisions = Vec::new();
    for i in 0..3 {
        let (budget, math) = math();
        let mut job = Job::new(
            format!("cache{i}"),
            format!("key{i}"),
            first.grid.clone(),
            0,
            (first.recipe.clone(), true),
            math,
            budget,
        );
        job.display_revisions.cache = Arc::clone(&cache);
        let job = Arc::new(job);
        job.prepare();
        job.finish();
        let revision = job.display_revisions.accept(&job, defaults()).unwrap().0;
        let file = cache.render_full(&job, &revision).unwrap();
        cache.publish(&revision, file).unwrap();
        jobs.list.lock().unwrap().push(Arc::clone(&job));
        revisions.push((job, revision));
    }
    assert_eq!(
        revisions[0].1.artifacts.lock().unwrap().full.state,
        ArtifactState::Evicted
    );
    assert_eq!(
        full(
            Extension(Arc::clone(&jobs)),
            path(&revisions[0].0, &revisions[0].1.token)
        )
        .await
        .status(),
        StatusCode::GONE
    );
    assert_eq!(std::fs::read_dir(cache.owned_path()).unwrap().count(), 2);
    assert_eq!(
        std::fs::read(root.path().join("unrelated.png")).unwrap(),
        b"user file"
    );
    let opened = full(
        Extension(Arc::clone(&jobs)),
        path(&revisions[1].0, &revisions[1].1.token),
    )
    .await;
    let old_path = revisions[1]
        .1
        .artifacts
        .lock()
        .unwrap()
        .file
        .as_ref()
        .unwrap()
        .path
        .clone();
    // Touch the other entry so the already-opened response is the next victim.
    cache.touch(
        &revisions[2]
            .1
            .artifacts
            .lock()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .path,
    );
    let file = cache.render_full(&revisions[0].0, &revisions[0].1).unwrap();
    cache.publish(&revisions[0].1, file).unwrap();
    assert!(!old_path.exists());
    image(opened).await;
    let file = cache
        .render_full(
            &first,
            &first
                .display_revisions
                .accept(&first, defaults())
                .unwrap()
                .0,
        )
        .unwrap();
    let byte_cache = DisplayCache::fixture(root.path(), file.bytes + 1, 2);
    let revision = first.display_revisions.find("d0").unwrap();
    let file = byte_cache.render_full(&first, &revision).unwrap();
    byte_cache.publish(&revision, file).unwrap();
    let other = &revisions[0];
    let file = byte_cache.render_full(&other.0, &other.1).unwrap();
    byte_cache.publish(&other.1, file).unwrap();
    assert_eq!(
        revision.artifacts.lock().unwrap().full.state,
        ArtifactState::Evicted
    );
    assert_eq!(
        std::fs::read_dir(byte_cache.owned_path()).unwrap().count(),
        1
    );
}

#[test]
fn preview_is_bounded_aspect_preserving_and_uses_sampled_frozen_tone() {
    assert_eq!(preview_dimensions(17561, 8779), (2048, 1024));
    assert_eq!(preview_dimensions(1, 9000), (1, 2048));
    assert_eq!(preview_dimensions(3, 2), (3, 2));
    let (_jobs, job, _root) = fixture(4097, 3);
    let revision = job.display_revisions.accept(&job, defaults()).unwrap().0;
    let bytes = render_preview(&job, &revision).unwrap();
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes.as_ref()))
        .read_info()
        .unwrap();
    let mut rgb = vec![0; decoder.output_buffer_size().unwrap()];
    let info = decoder.next_frame(&mut rgb).unwrap();
    assert_eq!((info.width, info.height), (2048, 1));
    for x in 0..info.width {
        let native_x = ((u64::from(x) * 2 + 1) * 4097 / (2048 * 2)) as u32;
        assert_eq!(
            &rgb[x as usize * 3..x as usize * 3 + 3],
            &revision
                .resolved
                .pixel([0, 1, 2].map(|channel| job.published_value(channel, native_x, 1)))
        );
    }
}

#[test]
fn stale_full_stops_at_row_boundary_and_all_default_jobs_share_one_full_slot() {
    assert!(Arc::ptr_eq(
        &DisplayCache::shared().full_slot,
        &DisplayCache::shared().full_slot
    ));
    let (_jobs, job, _root) = fixture(513, 257);
    let mut seed = 71_u32;
    for channel in 0..3 {
        let values: Vec<f32> = (0..(513 * 257))
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed % 10_000) as f32
            })
            .collect();
        job.publish_test_values(channel, |x, y| values[(y * 513 + x) as usize]);
    }
    job.finish();
    let revision = job.display_revisions.accept(&job, defaults()).unwrap().0;
    struct SupersedingWriter<'a>(&'a Revision);
    impl std::io::Write for SupersedingWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.artifacts.lock().unwrap().full.completed_rows > 0 {
                self.0.current.store(false, Ordering::Release);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let old = std::time::Instant::now() - std::time::Duration::from_secs(1000);
    *job.last_activity.lock().unwrap() = old;
    assert!(super::super::display_file::encode(
        &job,
        &revision,
        513,
        257,
        SupersedingWriter(&revision),
        false
    )
    .is_err());
    let rows = revision.artifacts.lock().unwrap().full.completed_rows;
    assert!(
        rows > 0 && rows < 257,
        "stopped after {rows} rows, before a full raster"
    );
    assert!(*job.last_activity.lock().unwrap() > old);
    assert!(job
        .display_revisions
        .cache
        .render_full(&job, &revision)
        .is_err());
    assert_eq!(
        std::fs::read_dir(job.display_revisions.cache.owned_path())
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn held_file_metadata_cannot_postpone_cache_or_revision_unlink() {
    let (jobs, source, root) = fixture(3, 2);
    let cache = DisplayCache::fixture(root.path(), 1024 * 1024, 1);
    let first = cached_job(&source, &cache, "held");
    let second = cached_job(&source, &cache, "next");
    jobs.list.lock().unwrap().push(Arc::clone(&first));
    let revision = first
        .display_revisions
        .accept(&first, defaults())
        .unwrap()
        .0;
    let held = cache.render_full(&first, &revision).unwrap();
    cache.publish(&revision, Arc::clone(&held)).unwrap();
    let opened = full(Extension(Arc::clone(&jobs)), path(&first, &revision.token)).await;
    let other = second
        .display_revisions
        .accept(&second, defaults())
        .unwrap()
        .0;
    cache
        .publish(&other, cache.render_full(&second, &other).unwrap())
        .unwrap();
    assert!(
        !held.path.exists(),
        "a GET's metadata Arc is not a cache lease"
    );
    assert_eq!(std::fs::read_dir(cache.owned_path()).unwrap().count(), 1);
    image(opened).await;
    let held = cache.render_full(&first, &revision).unwrap();
    cache.publish(&revision, Arc::clone(&held)).unwrap();
    let mut settings = defaults();
    settings.r.gain = 0.0;
    first.display_revisions.accept(&first, settings).unwrap();
    assert!(
        !held.path.exists(),
        "superseding also unlinks while metadata is borrowed"
    );
    assert_eq!(
        revision.artifacts.lock().unwrap().full.state,
        ArtifactState::Superseded
    );
}

#[tokio::test]
async fn delayed_missing_file_read_does_not_invalidate_a_regenerated_revision() {
    let (jobs, source, root) = fixture(3, 2);
    let cache = DisplayCache::fixture(root.path(), 1024 * 1024, 1);
    let first = cached_job(&source, &cache, "regenerated");
    let second = cached_job(&source, &cache, "evictor");
    jobs.list.lock().unwrap().push(Arc::clone(&first));
    let revision = first
        .display_revisions
        .accept(&first, defaults())
        .unwrap()
        .0;
    let captured = cache.render_full(&first, &revision).unwrap();
    cache.publish(&revision, Arc::clone(&captured)).unwrap();
    let other = second
        .display_revisions
        .accept(&second, defaults())
        .unwrap()
        .0;
    cache
        .publish(&other, cache.render_full(&second, &other).unwrap())
        .unwrap();
    assert!(!captured.path.exists());
    assert_eq!(
        revision.artifacts.lock().unwrap().full.state,
        ArtifactState::Evicted
    );
    let replacement = cache.render_full(&first, &revision).unwrap();
    cache.publish(&revision, Arc::clone(&replacement)).unwrap();
    let response = missing_file(&revision, &captured);
    assert_eq!(response.status(), StatusCode::GONE);
    assert!(json(response).await["error"].is_string());
    {
        let artifacts = revision.artifacts.lock().unwrap();
        assert_eq!(artifacts.full.state, ArtifactState::Ready);
        assert!(Arc::ptr_eq(artifacts.file.as_ref().unwrap(), &replacement));
        assert_eq!(artifacts.full.bytes, Some(replacement.bytes));
    }
    image(full(Extension(Arc::clone(&jobs)), path(&first, &revision.token)).await).await;
    std::fs::remove_file(&replacement.path).unwrap();
    assert_eq!(
        full(Extension(Arc::clone(&jobs)), path(&first, &revision.token))
            .await
            .status(),
        StatusCode::GONE
    );
    assert_eq!(
        revision.artifacts.lock().unwrap().full.state,
        ArtifactState::Evicted
    );
}

#[tokio::test]
async fn failed_cache_unlink_does_not_publish_beyond_the_ready_file_limit() {
    let (jobs, source, root) = fixture(3, 2);
    let cache = DisplayCache::fixture(root.path(), 1024 * 1024, 1);
    let first = cached_job(&source, &cache, "old");
    let second = cached_job(&source, &cache, "new");
    let revision = first
        .display_revisions
        .accept(&first, defaults())
        .unwrap()
        .0;
    let held = cache.render_full(&first, &revision).unwrap();
    cache.publish(&revision, Arc::clone(&held)).unwrap();
    let backup = cache.owned_path().join("backup.png");
    std::fs::rename(&held.path, &backup).unwrap();
    // Deterministic filesystem failure without root/permission assumptions.
    std::fs::create_dir(&held.path).unwrap();
    let other = second
        .display_revisions
        .accept(&second, defaults())
        .unwrap()
        .0;
    let new_file = cache.render_full(&second, &other).unwrap();
    let unpublished_path = new_file.path.clone();
    assert!(cache.publish(&other, new_file).is_err());
    assert!(!unpublished_path.exists());
    assert!(other.artifacts.lock().unwrap().file.is_none());
    assert_ne!(
        other.artifacts.lock().unwrap().full.state,
        ArtifactState::Ready
    );
    jobs.list.lock().unwrap().push(Arc::clone(&second));
    kick_preview(&second);
    wait(&second, &other.token, false).await;
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&second, &other.token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    wait(&second, &other.token, true).await;
    assert_eq!(
        other.artifacts.lock().unwrap().full.state,
        ArtifactState::Failed
    );
    let failed = full(Extension(Arc::clone(&jobs)), path(&second, &other.token)).await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(json(failed).await["error"].is_string());
    std::fs::remove_dir(&held.path).unwrap();
    std::fs::rename(backup, &held.path).unwrap();
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&second, &other.token))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    wait(&second, &other.token, true).await;
    image(full(Extension(Arc::clone(&jobs)), path(&second, &other.token)).await).await;
    assert_eq!(
        other.artifacts.lock().unwrap().full.state,
        ArtifactState::Ready
    );
    assert!(!held.path.exists());
    assert_eq!(std::fs::read_dir(cache.owned_path()).unwrap().count(), 1);
}

#[tokio::test]
async fn cancelled_job_cannot_revive_revisions_or_leave_a_queued_full_waiter() {
    use std::future::Future;
    use std::task::Poll;

    let (jobs, job, _root) = fixture(3, 2);
    let body = serde_json::to_string(&defaults()).unwrap();
    let manifest = json(post(&jobs, &job.id, &body).await).await;
    let token = manifest["revision"].as_str().unwrap();
    wait(&job, token, false).await;
    let revision = job.display_revisions.find(token).unwrap();
    let permit = Arc::clone(&job.display_revisions.cache.full_slot)
        .acquire_owned()
        .await
        .unwrap();
    revision.artifacts.lock().unwrap().full.state = ArtifactState::Queued;
    job.display_revisions.store.lock().unwrap().full_worker = true;
    let mut worker = Box::pin(full_worker(
        Arc::downgrade(&job),
        Arc::clone(&job.display_revisions.cache),
        Arc::clone(&job.display_revisions.notify),
    ));
    let pending = std::future::poll_fn(|cx| Poll::Ready(worker.as_mut().poll(cx))).await;
    assert!(pending.is_pending());
    let caches = jobs.begin_display_shutdown();
    assert_eq!(caches.len(), 1);
    assert!(Arc::ptr_eq(&caches[0], &job.display_revisions.cache));
    let completed = std::future::poll_fn(|cx| Poll::Ready(worker.as_mut().poll(cx))).await;
    assert!(
        completed.is_ready(),
        "cancellation does not wait for a different full producer"
    );
    assert!(!job.display_revisions.store.lock().unwrap().full_worker);
    assert_eq!(revision.artifacts.lock().unwrap().full.completed_rows, 0);
    assert!(matches!(
        job.display_revisions.accept(&job, defaults()),
        Err((StatusCode::GONE, _))
    ));
    let mut settings = defaults();
    settings.r.gain = 0.0;
    assert!(matches!(
        job.display_revisions.accept(&job, settings),
        Err((StatusCode::GONE, _))
    ));
    assert_eq!(post(&jobs, &job.id, &body).await.status(), StatusCode::GONE);
    assert_eq!(
        request_full(Extension(Arc::clone(&jobs)), path(&job, token))
            .await
            .status(),
        StatusCode::GONE
    );
    drop(permit);
}

#[tokio::test]
async fn preview_checks_revision_currency_before_supersede_updates_artifact_state() {
    let (jobs, job, _root) = fixture(3, 2);
    let body = serde_json::to_string(&defaults()).unwrap();
    let manifest = json(post(&jobs, &job.id, &body).await).await;
    let token = manifest["revision"].as_str().unwrap();
    wait(&job, token, false).await;
    let revision = job.display_revisions.find(token).unwrap();
    assert_eq!(
        revision.artifacts.lock().unwrap().preview.state,
        ArtifactState::Ready
    );
    // supersede() flips currency before it can acquire the artifact mutex.
    revision.current.store(false, Ordering::Release);
    let response = preview(Extension(Arc::clone(&jobs)), path(&job, token)).await;
    assert_eq!(response.status(), StatusCode::GONE);
    assert!(json(response).await["error"].is_string());
}

#[test]
fn shutdown_cleanup_unlinks_only_owned_files_and_open_stream_survives() {
    use std::io::Read;

    let (_jobs, job, _root) = fixture(3, 2);
    let cache = Arc::clone(&job.display_revisions.cache);
    let revision = job.display_revisions.accept(&job, defaults()).unwrap().0;
    let published = cache.render_full(&job, &revision).unwrap();
    cache.publish(&revision, Arc::clone(&published)).unwrap();
    let directory = cache.owned_path();
    let unexpected = directory.join("unexpected");
    std::fs::write(&unexpected, b"not owned by the display cache").unwrap();
    let mut opened = std::fs::File::open(&published.path).unwrap();
    let late = cache.render_full(&job, &revision).unwrap();
    let late_path = late.path.clone();

    cache.close();
    assert!(cache.publish(&revision, late).is_err());
    job.display_revisions.cancel();
    let errors = cache.cleanup().unwrap_err();

    assert!(!published.path.exists());
    assert!(!late_path.exists());
    assert!(unexpected.exists(), "unexpected files are not swept");
    assert!(directory.exists(), "a non-empty run directory is retained");
    assert!(errors
        .iter()
        .any(|error| error.contains("owned display directory")));
    assert!(cache.render_full(&job, &revision).is_err());
    let mut png = Vec::new();
    opened.read_to_end(&mut png).unwrap();
    assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);

    std::fs::remove_file(unexpected).unwrap();
    cache.cleanup().unwrap();
    assert!(!directory.exists());
}
