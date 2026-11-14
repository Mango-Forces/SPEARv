use super::*;
use crate::combine_color;
use async_trait::async_trait;
use crate::{Stacked, SPHEREX};
use spearv_core::{
    Cutout, ExposureId, FlagBits, Plane, Poly, Release, Result, Sip, SkyRegion, SourceId, WavelengthMap, Wcs, BANDS,
    PIXEL_SCALE_DEG,
};
use std::sync::atomic::AtomicUsize;

const RA: f64 = 50.0;
const DEC: f64 = 0.0;

/// The one real interpreter and budget every composite test shares.
fn math() -> (Arc<WorkBudget>, Arc<dyn ImageMath>) {
    let (budget, math) = crate::testing::math();
    (budget, math)
}

fn test_job(id: &str, grid: TanGrid, exposures: usize, recipe: (Recipe, bool)) -> Job {
    let (budget, math) = math();
    Job::new(id.into(), "k".into(), grid, exposures, recipe, math, budget)
}

fn insert(jobs: &Jobs, key: &str, grid: TanGrid, exposures: usize, view: (Recipe, bool)) -> (Arc<Job>, bool) {
    let (budget, math) = math();
    jobs.insert(key.into(), grid, exposures, view, math, budget)
}

fn permit() -> spearv_core::WorkPermit {
    let (budget, _) = math();
    budget.try_acquire(1).expect("test permit")
}

/// Publish test values as charged immutable tiles.
fn publish_values(job: &Job, channel: usize, value: impl Fn(u32, u32) -> f32) {
    job.publish_test_values(channel, value);
}

#[test]
fn rectangular_tiles_raw_orientation_and_snake_path() {
    for (width, height) in [(513, 257), (257, 513), (300, 257)] {
        let job = test_job("rect",
            TanGrid::new_rect(RA, DEC, f64::from(width) * PIXEL_SCALE_DEG, f64::from(height) * PIXEL_SCALE_DEG),
            0, (grey_recipe(42), true));
        job.prepare();
        let info = job.info();
        assert_eq!((info.width, info.height), (width, height));
        assert_eq!(job.lock().tiles.len(), (info.cols * info.rows) as usize);
        let path = snake_order(info.cols, info.rows);
        assert_eq!(path.len(), (info.cols * info.rows) as usize);
        assert_eq!(path.iter().copied().collect::<BTreeSet<_>>().len(), path.len());
        publish_values(&job, 0, |x, y| (y * width + x) as f32);
        job.finish();
        let metadata = job.raw_metadata().unwrap();
        assert_eq!((metadata.width, metadata.height), (width, height));
        let first = job.tile_f32(0, 0, 0).unwrap().unwrap();
        assert_eq!(f32::from_le_bytes(first[..4].try_into().unwrap()), ((height - 1) * width) as f32);
        let (tx, ty) = (info.cols - 1, info.rows - 1);
        let raw = job.tile_f32(0, tx, ty).unwrap().unwrap();
        let (w, h, rgb) = job.tile_rgb(tx, ty).unwrap();
        assert_eq!((w, h), (width - tx * TILE, height - ty * TILE));
        assert_eq!(raw.len(), (w * h * 4) as usize);
        assert_eq!(rgb.len(), (w * h * 3) as usize);
        assert_eq!(f32::from_le_bytes(raw[raw.len()-4..].try_into().unwrap()), (width - 1) as f32);
    }
}

#[test]
fn a_thirty_degree_color_job_tiles_the_grid_without_full_grid_sums() {
    let job = job(17561.0);
    assert_eq!((job.grid.width, job.grid.height), (17561, 17561));
    assert_eq!((job.cols, job.rows), (69, 69));
    assert_eq!(job.tiles_len(), 69 * 69);
    assert_eq!(job.sessions.len(), 3 * 69 * 69, "one Values session per channel and tile");
    assert_eq!(job.outputs.lock().unwrap().len(), 3 * 69 * 69);
    let bottom = job.tile_of(69 * 69 - 1);
    assert_eq!((bottom.x, bottom.y), (68 * 256, 0));
    assert_eq!(bottom.shape, Shape { width: 153, height: 153 });
    let top = job.tile_of(0);
    assert_eq!((top.x, top.y), (0, 17561 - 256));
    assert_eq!(top.shape, Shape { width: 256, height: 256 });
}

#[test]
fn rectangular_keys_and_budget_do_not_use_the_envelope() {
    let key = |l, h, color| {
        let mut view = view(Mode::Composite, vec![]);
        view.region = SkyRegion::rect(RA, DEC, l, h);
        job_key(&view, color, ColorBands::DEFAULT)
    };
    for color in [false, true] {
        assert_ne!(key(0.2, 0.1, color), key(0.1, 0.2, color));
        assert_ne!(key(0.2, 0.1, color), key(0.2, 0.15, color));
    }
    assert!(job_bytes(513, 257) < job_bytes(513, 513));
    assert_eq!(job_bytes(513, 257), job_bytes(257, 513));
}

#[test]
fn rectangular_color_matches_the_render_tone() {
    let job = test_job("rect",
        TanGrid::new_rect(RA, DEC, 3.0 * PIXEL_SCALE_DEG, 2.0 * PIXEL_SCALE_DEG),
        0, (color_recipe(), true));
    let values = [vec![0.1, 0.2, f32::NAN, 0.4, 0.5, 0.6],
        vec![0.3, 0.2, f32::NAN, 0.4, 0.5, 0.6], vec![0.1, 0.1, f32::NAN, 0.4, 0.5, 0.6]];
    for (slot, values) in values.iter().enumerate() {
        publish_values(&job, slot, |x, y| values[(y * 3 + x) as usize]);
        job.lock().channels[slot].stretch = stretch_of(
            Display::Color,
            values.iter().copied().filter(|v| v.is_finite()).collect(),
        );
    }
    let stacks = values.iter().map(|v| Stacked { width: 3, height: 2, value: v.clone() }).collect::<Vec<_>>();
    let decoded = image::load_from_memory(&combine_color(&stacks).unwrap()).unwrap().to_rgb8();
    assert_eq!(decoded.dimensions(), (3, 2));
    assert_eq!(job.tile_rgb(0, 0).unwrap().2, decoded.into_raw());
}

#[test]
fn an_exhausted_budget_refuses_a_new_mosaic() {
    let ctx = Context::new(Arc::new(spearv_core::Cache::new())).with_budget(Arc::new(spearv_core::WorkBudget::new(64)));
    let held = ctx.budget.try_acquire(64).expect("the whole budget");
    let error = admit(&ctx, 64, 64).expect_err("no budget left");
    assert_eq!(error.into_response().status(), StatusCode::TOO_MANY_REQUESTS);
    drop(held);
    assert!(admit(&ctx, 64, 64).is_ok());
}

fn wcs(ra: f64, dec: f64) -> Wcs {
    Wcs {
        crpix: [64.5, 64.5],
        crval: [ra, dec],
        cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
        sip: Sip {
            a: Poly::new(0),
            b: Poly::new(0),
            ap: None,
            bp: None,
        },
        naxis: [128, 128],
    }
}

/// The whole detector sits in band `band`, so a test need not model the strip.
fn wave(band: u16) -> WavelengthMap {
    WavelengthMap {
        x: vec![1.0, 128.0],
        y: vec![1.0, 128.0],
        wavelength: vec![BANDS[band as usize].center_um as f32; 4],
        bandwidth: vec![0.01; 4],
        nx: 2,
        ny: 2,
    }
}

fn exposure(name: &str, band: u16) -> (Exposure, usize) {
    let channel = ColorBands::DEFAULT
        .channels()
        .iter()
        .position(|spec| spec.center == band)
        .unwrap();
    let center = BANDS[band as usize].center_um;
    let exposure = Exposure {
        id: ExposureId(name.into()),
        source: SourceId::new(SPHEREX),
        detector: BANDS[band as usize].detector,
        mjd: 0.0,
        release: ReleaseId("qr3".into()),
        version: "v".into(),
        obsid: name.into(),
        url: name.into(),
        etag: None,
        size: None,
        em_min_um: center - 0.05,
        em_max_um: center + 0.05,
    };
    (exposure, channel)
}

fn color_recipe() -> Recipe {
    recipe(&view(Mode::Composite, vec![]), true, ColorBands::DEFAULT).unwrap()
}

fn grey_recipe(band: u16) -> Recipe {
    recipe(&view(Mode::Range, vec![pin(band, 1, None)]), false, ColorBands::DEFAULT).unwrap()
}

fn view(mode: Mode, pins: Vec<spearv_core::Pin>) -> View {
    View {
        region: SkyRegion::square(RA, DEC, 0.1),
        mode,
        release: "qr3".into(),
        pins,
        mask: true,
    }
}

fn pin(center: u16, width: u16, visit: Option<u16>) -> spearv_core::Pin {
    spearv_core::Pin { center, width, visit, sign: None }
}

/// Serves flat cutouts and counts reads. `fail` names files that do not read;
/// `gate` holds every read until it is released.
#[derive(Default)]
struct Fake {
    reads: Mutex<Vec<String>>,
    fail: Vec<String>,
    at: Mutex<std::collections::HashMap<String, (f64, f64)>>,
    values: Mutex<std::collections::HashMap<String, f32>>,
    peak: AtomicUsize,
    live: AtomicUsize,
    gate: Option<tokio::sync::Semaphore>,
}

fn band_of_url(url: &str) -> u16 {
    url.rsplit('-').next().unwrap().parse().unwrap()
}

#[async_trait]
impl ImageSource for Fake {
    fn id(&self) -> SourceId {
        SourceId::new(SPHEREX)
    }
    async fn releases(&self) -> Result<Vec<Release>> {
        Ok(vec![])
    }
    async fn coverage(&self, _region: &SkyRegion, _filter: &Filter) -> Result<Vec<Exposure>> {
        Ok(vec![])
    }
    async fn cutout(&self, _exposure: &Exposure, _region: &SkyRegion) -> Result<Cutout> {
        Err(Error::Message("the mosaic reads windows".into()))
    }
    async fn geometry(&self, exposure: &Exposure) -> Result<ExposureGeometry> {
        let at = self.at.lock().unwrap().get(&exposure.url).copied().unwrap_or((RA, DEC));
        Ok(ExposureGeometry {
            wcs: wcs(at.0, at.1),
            wavelength: wave(band_of_url(&exposure.url)),
            header_bytes: 10,
        })
    }
    async fn window_cutout(&self, exposure: &Exposure, window: &PixelWindow) -> Result<Cutout> {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            let _permit = gate.acquire().await;
        }
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.reads.lock().unwrap().push(exposure.url.clone());
        if self.fail.contains(&exposure.url) {
            return Err(Error::Upstream("no".into()));
        }
        let geometry = self.geometry(exposure).await?;
        let plane = |value: f32| Plane {
            width: window.width as u32,
            height: window.height as u32,
            data: vec![value; (window.width * window.height) as usize],
        };
        let image = self
            .values
            .lock()
            .unwrap()
            .get(&exposure.url)
            .copied()
            .unwrap_or_else(|| f32::from(band_of_url(&exposure.url)));
        Ok(Cutout {
            image: plane(image),
            variance: plane(1.0),
            flags: Plane {
                width: window.width as u32,
                height: window.height as u32,
                data: vec![0; (window.width * window.height) as usize],
            },
            zodi: Some(plane(0.0)),
            wcs: geometry.wcs,
            wavelength: geometry.wavelength,
            origin_fits: (window.x0 + 1, window.y0 + 1),
            flags_valid: None,
            clipped: false,
            flag_bits: FlagBits {
                transient: 0,
                overflow: 1,
                sur_error: 2,
                nonfunc: 6,
                fullsample: 12,
                persist: 17,
                outlier: 19,
                source: 21,
            },
            bytes_read: 100,
            s3_requests: 1,
        })
    }
}

fn job(fov_px: f64) -> Arc<Job> {
    let grid = TanGrid::new(RA, DEC, fov_px * PIXEL_SCALE_DEG);
    Arc::new(test_job("m0", grid, 0, (color_recipe(), true)))
}

fn poll_until_finished(job: &Job) {
    for _ in 0..4000 {
        if job.finished() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the job did not finish");
}

#[test]
fn the_three_channels_never_share_a_file() {
    let detectors: BTreeSet<u8> = color_recipe()
        .channels
        .iter()
        .flat_map(|spec| spec.bands.iter().map(|band| BANDS[*band as usize].detector))
        .collect();
    assert_eq!(detectors.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_file_is_read_once_and_tiles_settle_before_the_final_stretch() {
    let job = job(300.0);
    let wanted: Vec<_> = ColorBands::DEFAULT
        .channels()
        .iter()
        .map(|band| exposure(&format!("f-{}", band.center), band.center))
        .collect();
    let fake = Arc::new(Fake::default());
    run(Arc::clone(&job), fake.clone(), wanted, permit()).await;
    poll_until_finished(&job);
    let mut reads = fake.reads.lock().unwrap().clone();
    reads.sort();
    assert_eq!(reads, ["f-40", "f-5", "f-90"]);
    let status = job.status(0);
    assert_eq!(status.state, MosaicState::Done);
    assert_eq!((status.exposures_done, status.exposures_total, status.reads_failed), (3, 3, 0));
    assert_eq!(status.bytes_read, 330);
    assert_eq!(status.tiles.len(), 4, "every tile is redrawn once at the end");
    assert!(status.tiles.iter().all(|tile| tile.settled));
    assert!(status.active.is_empty());
    let seq = status.seq;
    assert!(job.status(seq).tiles.is_empty(), "nothing changed since");
}

#[tokio::test(flavor = "multi_thread")]
async fn delta_difference_and_retained_outputs_use_library_math_and_stay_charged() {
    let grid = TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let recipe = recipe(&delta_view(), false, ColorBands::DEFAULT).unwrap();
    let job = Arc::new(test_job("d", grid, 0, (recipe, true)));
    let wanted = vec![(exposure("early-40", 40).0, 0), (exposure("late-40", 40).0, 1)];
    let mut values = std::collections::HashMap::new();
    values.insert("early-40".into(), 5.0);
    values.insert("late-40".into(), 2.0);
    let fake = Arc::new(Fake { values: Mutex::new(values), ..Fake::default() });
    let (budget, _) = math();
    let before = budget.available_bytes();
    run(Arc::clone(&job), fake, wanted, permit()).await;
    assert_eq!(job.status(0).state, MosaicState::Done);
    let output = job.output(0, 0).expect("delta output");
    let Values::F32(values) = &output.planes().value else {
        panic!("delta is f32");
    };
    assert!(values.iter().filter(|value| value.is_finite()).all(|value| (value - 3.0).abs() < 1e-6));
    drop(output);
    // The finished job still holds its published tile: the run permit is
    // gone, but the snapshot lease remains until the job is dropped.
    let held = budget.available_bytes();
    assert!(held < before);
    drop(job);
    assert!(budget.available_bytes() > held);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_read_is_counted_and_the_rest_still_land() {
    let job = job(300.0);
    let wanted: Vec<_> = ColorBands::DEFAULT
        .channels()
        .iter()
        .map(|band| exposure(&format!("f-{}", band.center), band.center))
        .collect();
    let fake = Arc::new(Fake {
        fail: vec!["f-40".into()],
        ..Fake::default()
    });
    run(Arc::clone(&job), fake, wanted, permit()).await;
    let status = job.status(0);
    assert_eq!(status.state, MosaicState::Done);
    assert_eq!(status.reads_failed, 1);
    assert_eq!(status.exposures_done, 3);
    let (_, _, rgb) = job.tile_rgb(0, 0).unwrap();
    assert!(rgb.chunks(3).any(|pixel| pixel != NODATA));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_where_every_read_fails_is_not_done() {
    let job = job(300.0);
    let wanted = vec![exposure("f-5", 5)];
    let fake = Arc::new(Fake {
        fail: vec!["f-5".into()],
        ..Fake::default()
    });
    run(Arc::clone(&job), fake, wanted, permit()).await;
    assert_eq!(job.status(0).state, MosaicState::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_nobody_polls_stops_reading() {
    let job = job(300.0);
    let wanted: Vec<_> = (0..40).map(|i| exposure(&format!("f{i}-5"), 5)).collect();
    let fake = Arc::new(Fake {
        gate: Some(tokio::sync::Semaphore::new(0)),
        ..Fake::default()
    });
    let running = tokio::spawn(run(Arc::clone(&job), fake.clone(), wanted, permit()));
    for _ in 0..2000 {
        if fake.live.load(Ordering::SeqCst) == READS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(fake.peak.load(Ordering::SeqCst), READS, "reads are bounded");
    *job.last_activity.lock().unwrap() = Instant::now() - ABANDON_AFTER - Duration::from_secs(1);
    running.await.unwrap();
    assert_eq!(job.status(0).state, MosaicState::Cancelled);
    assert!(fake.reads.lock().unwrap().is_empty(), "held reads were aborted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_slot_frees_before_its_file_is_stacked() {
    let job = job(300.0);
    job.stack_slots.forget_permits(usize::MAX);
    let wanted: Vec<_> = (0..200).map(|i| exposure(&format!("f{i}-5"), 5)).collect();
    let fake = Arc::new(Fake::default());
    let running = tokio::spawn(run(Arc::clone(&job), fake.clone(), wanted, permit()));
    for _ in 0..2000 {
        if fake.reads.lock().unwrap().len() > READS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(fake.reads.lock().unwrap().len() > READS, "nothing stacked, yet downloads go on");
    *job.last_activity.lock().unwrap() = Instant::now() - ABANDON_AFTER - Duration::from_secs(1);
    running.await.unwrap();
}

#[test]
fn a_grey_tile_uses_the_render_stretch() {
    let grid = TanGrid::new(RA, DEC, 300.0 * PIXEL_SCALE_DEG);
    let job = test_job("g", grid, 0, (grey_recipe(40), true));
    publish_values(&job, 0, |x, y| if (x, y) == (0, 299) { 1.0 } else { f32::NAN });
    job.lock().channels[0].stretch = Some(Stretch { low: 0.0, span: 2.0 });
    let (_, _, rgb) = job.tile_rgb(0, 0).unwrap();
    let expected = grey_level(1.0, 0.0, 2.0);
    assert_eq!(rgb[..3], [expected; 3]);
    assert_eq!(rgb[3..6], NODATA, "a grey hole is still no data");
    let values = (0..=1000).map(|i| i as f32).collect();
    let stretch = stretch_of(Display::Grey, values).unwrap();
    assert_eq!((stretch.low, stretch.span), (20.0, 975.0), "0.02 and 0.995, as `encode` takes them");
}

#[test]
fn a_pin_visit_keeps_only_that_visit() {
    // Two visits, 30 days apart. A pin on visit 1 keeps only the later one.
    let early = Exposure { mjd: 60000.0, ..exposure("early-40", 40).0 };
    let late = Exposure { mjd: 60030.0, ..exposure("late-40", 40).0 };
    let wrong_band = Exposure { mjd: 60030.0, ..exposure("late-90", 90).0 };
    let view = view(Mode::Range, vec![pin(40, 1, Some(1))]);
    let picked = select(vec![early.clone(), late, wrong_band], &view, false, &recipe(&view, false, ColorBands::DEFAULT).unwrap()).unwrap();
    let urls: Vec<&str> = picked.iter().map(|(exposure, _)| exposure.url.as_str()).collect();
    assert_eq!(urls, ["late-40"]);
    // Colour ignores a stray pin visit, as `color.png` does.
    let picked = select(vec![early], &view, true, &recipe(&view, true, ColorBands::DEFAULT).unwrap()).unwrap();
    assert_eq!(picked.len(), 1);
}

#[test]
fn a_slider_reads_one_band_and_a_range_reads_the_pin() {
    let slider = recipe(&view(Mode::Slider, vec![pin(40, 5, None)]), false, ColorBands::DEFAULT).unwrap();
    assert_eq!(slider.channels[0].bands, [40]);
    let range = recipe(&view(Mode::Range, vec![pin(40, 5, None)]), false, ColorBands::DEFAULT).unwrap();
    assert_eq!(range.channels[0].bands, [38, 39, 40, 41, 42]);
    assert_eq!(range.display, Display::Grey);
}

#[test]
fn a_composite_reads_every_file_across_all_its_rows_and_ignores_pin_visits() {
    let composite = recipe(&view(Mode::Composite, vec![pin(40, 5, Some(1))]), false, ColorBands::DEFAULT).unwrap();
    assert!(composite.channels[0].bands.is_empty(), "stack takes an empty list as every band");
    assert!(composite.channels[0].span.is_none());
    let early = Exposure { mjd: 60000.0, ..exposure("early-40", 40).0 };
    let late = Exposure { mjd: 60030.0, ..exposure("late-90", 90).0 };
    let view = view(Mode::Composite, vec![pin(40, 5, Some(1))]);
    let picked = select(vec![early, late], &view, false, &composite).unwrap();
    assert_eq!(picked.len(), 2, "a composite stacks every epoch and every band");
    let grid = TanGrid::new(RA, DEC, 2000.0 * PIXEL_SCALE_DEG);
    let geometry = ExposureGeometry { wcs: wcs(RA, DEC), wavelength: wave(5), header_bytes: 0 };
    let (window, _) = footprint(&geometry, None, &grid, 8).unwrap();
    assert_eq!((window.y0, window.height), (0, 128));
}

#[test]
fn a_colour_recipe_uses_the_requested_band_windows() {
    let view = view(Mode::Composite, vec![]);
    let bands = |recipe: &Recipe| -> Vec<Vec<u16>> {
        recipe.channels.iter().map(|spec| spec.bands.clone()).collect()
    };
    let default = recipe(&view, true, ColorBands::DEFAULT).unwrap();
    assert_eq!(
        bands(&default),
        [
            vec![3, 4, 5, 6, 7],
            vec![38, 39, 40, 41, 42],
            vec![88, 89, 90, 91, 92]
        ],
        "blue, green and red, the fixed 5/40/90 width-5 windows"
    );
    let custom = ColorBands {
        blue: crate::ColorBand { center: 0, width: 7 },
        green: crate::ColorBand { center: 50, width: 1 },
        red: crate::ColorBand { center: 100, width: 3 },
    };
    let custom = recipe(&view, true, custom).unwrap();
    assert_eq!(
        bands(&custom),
        [vec![0, 1, 2, 3], vec![50], vec![99, 100, 101]],
        "custom windows, clipped at band 0"
    );
}

#[test]
fn a_colour_job_key_carries_the_band_mapping() {
    let view = view(Mode::Composite, vec![]);
    assert_eq!(job_key(&view, false, ColorBands::DEFAULT), view.cache_key());
    let changed = ColorBands {
        red: crate::ColorBand {
            center: 91,
            ..ColorBands::DEFAULT.red
        },
        ..ColorBands::DEFAULT
    };
    let key = job_key(&view, true, ColorBands::DEFAULT);
    assert!(key.contains("bands=b5:5 g40:5 r90:5"), "{key}");
    assert_ne!(key, job_key(&view, true, changed));
}

fn delta_view() -> View {
    let side = |visit, sign| spearv_core::Pin { center: 40, width: 1, visit: Some(visit), sign: Some(sign) };
    view(Mode::Delta, vec![side(0, spearv_core::Sign::Plus), side(1, spearv_core::Sign::Minus)])
}

#[test]
fn a_delta_splits_by_visit_and_keeps_outliers() {
    let view = delta_view();
    let delta = recipe(&view, false, ColorBands::DEFAULT).unwrap();
    assert_eq!(delta.channels.len(), 2);
    assert!(delta.channels.iter().all(|side| side.ignore_outlier));
    let early = Exposure { mjd: 60000.0, ..exposure("early-40", 40).0 };
    let late = Exposure { mjd: 60030.0, ..exposure("late-40", 40).0 };
    let picked = select(vec![early.clone(), late], &view, false, &delta).unwrap();
    let sides: Vec<(&str, usize)> = picked.iter().map(|(e, side)| (e.url.as_str(), *side)).collect();
    assert_eq!(sides, [("early-40", 0), ("late-40", 1)]);
    assert!(select(vec![early], &view, false, &delta).is_err(), "one visit is not a delta");
}

#[test]
fn a_delta_rejects_a_wavelength_mismatch() {
    let mut view = delta_view();
    view.pins[1].center = 41;
    assert!(recipe(&view, false, ColorBands::DEFAULT).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delta_tile_is_final_only_when_both_sides_are() {
    let grid = TanGrid::new(RA, DEC, 100.0 * PIXEL_SCALE_DEG);
    let job = Arc::new(test_job("d", grid, 0, (recipe(&delta_view(), false, ColorBands::DEFAULT).unwrap(), true)));
    let plan = |side: usize| Planned {
        exposure: exposure(&format!("s{side}-40"), 40).0,
        channel: side,
        window: PixelWindow { x0: 0, y0: 0, width: 1, height: 1, clipped: false },
        tiles: vec![0],
    };
    job.begin_reading(&[plan(0), plan(1)], 0, 0, 0);
    job.settle_read(0, &[0]);
    assert!(job.status(0).tiles.iter().all(|tile| !tile.settled), "one side is not final");
    job.settle_read(1, &[0]);
    std::thread::sleep(Duration::from_millis(2));
    assert!(job.status(0).tiles.iter().all(|tile| tile.settled));
}

#[test]
fn color_scale_tiles_are_separate_top_first_clipped_and_normalized() {
    let job = job(300.0);
    for (channel, low) in [10.0, -5.0, 100.0].into_iter().enumerate() {
        publish_values(&job, channel, |x, y| match (x, y) {
            (0, 299) => low + 4.0,
            (299, 0) => low + 8.0,
            _ => low,
        });
    }
    job.finish();
    let metadata = job.raw_metadata().unwrap();
    assert_eq!(metadata.format_version, 2);
    assert_eq!(metadata.dtype, "u16le");
    assert_eq!(metadata.nodata, "65535");
    for channel in 0..3 {
        let stretch = job.lock().channels[channel].stretch.unwrap();
        assert_eq!(metadata.channels[channel].scale_offset, Some(stretch.low));
        assert_eq!(metadata.channels[channel].scale_span, Some(stretch.span));
        let code = |value: f32| {
            let stretch = job.lock().channels[channel].stretch.unwrap();
            (((value - stretch.low) / stretch.span).clamp(0.0, 1.0) * f32::from(SCALE_MAX)).round() as u16
        };
        let first = job.tile_scale16(channel, 0, 0).unwrap().unwrap();
        assert_eq!(first.len(), 256 * 256 * 2);
        assert_eq!(u16::from_le_bytes(first[..2].try_into().unwrap()), code([10.0, -5.0, 100.0][channel] + 4.0));
        assert_eq!(u16::from_le_bytes(first[2..4].try_into().unwrap()), code([10.0, -5.0, 100.0][channel]));
        let edge = job.tile_scale16(channel, 1, 1).unwrap().unwrap();
        assert_eq!(edge.len(), 44 * 44 * 2);
        assert_eq!(u16::from_le_bytes(edge[edge.len() - 2..].try_into().unwrap()), code([10.0, -5.0, 100.0][channel] + 8.0));
        assert!(job.tile_f32(channel, 0, 0).unwrap().is_none(), "Color never delivers physical scalar tiles");
    }
    assert!(job.tile_scale16(3, 0, 0).unwrap().is_none());
    assert!(job.tile_scale16(0, 2, 0).unwrap().is_none());
}

#[test]
fn color_empty_and_constant_scale_layers_have_explicit_ranges() {
    let job = job(10.0);
    job.prepare();
    publish_values(&job, 1, |_, _| 7.0);
    job.finish();
    let metadata = job.raw_metadata().unwrap();
    assert_eq!((metadata.channels[0].scale_offset, metadata.channels[0].scale_span), (None, None));
    let stretch = job.lock().channels[1].stretch.unwrap();
    assert_eq!((metadata.channels[1].scale_offset, metadata.channels[1].scale_span), (Some(stretch.low), Some(stretch.span)));
    assert!(job.tile_scale16(0, 0, 0).unwrap().unwrap().iter().all(|byte| *byte == 255));
    assert!(job.tile_scale16(1, 0, 0).unwrap().unwrap().iter().all(|byte| *byte == 0));
}

#[tokio::test]
async fn color_wire_requires_separate_layers_and_serves_compact_scale_bytes() {
    let jobs = Arc::new(Jobs::default());
    let grid = TanGrid::new(RA, DEC, 300.0 * PIXEL_SCALE_DEG);
    let (job, _) = insert(&jobs, "color", grid, 0, (color_recipe(), true));
    job.prepare();
    assert_eq!(get_tile(&jobs, &job.id, Some("scale16"), Some(0), 0, 0).await.status(), StatusCode::CONFLICT);
    job.finish();
    assert_eq!(get_tile(&jobs, &job.id, None, None, 0, 0).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get_tile(&jobs, &job.id, Some("f32"), Some(0), 0, 0).await.status(), StatusCode::NOT_FOUND);
    for channel in 0..3 {
        let response = get_tile(&jobs, &job.id, Some("scale16"), Some(channel), 1, 1).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/octet-stream");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], (44 * 44 * 2).to_string());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(bytes.iter().all(|byte| *byte == 255));
        assert_eq!(get_tile(&jobs, &job.id, Some("png"), Some(channel), 0, 0).await.status(), StatusCode::OK);
    }
}

#[test]
fn raw_routes_have_the_done_only_state_matrix() {
    assert_eq!(raw_state(MosaicState::Planning), Err(StatusCode::CONFLICT));
    assert_eq!(raw_state(MosaicState::Reading), Err(StatusCode::CONFLICT));
    assert_eq!(raw_state(MosaicState::Done), Ok(()));
    assert_eq!(raw_state(MosaicState::Failed), Err(StatusCode::GONE));
    assert_eq!(raw_state(MosaicState::Cancelled), Err(StatusCode::GONE));
}

#[test]
fn raw_metadata_is_frozen_and_raw_reads_do_not_change_png_pixels() {
    let grid = TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let job = test_job("g", grid, 0, (grey_recipe(40), true));
    job.prepare();
    publish_values(&job, 0, |_, _| 5.0);
    job.finish();
    let metadata = job.raw_metadata().unwrap();
    assert_eq!(metadata.source_recipe.mode, "range");
    assert_eq!(metadata.source_recipe.pins[0].center_band, 41);
    assert_eq!(metadata.channels[0].bands, [41]);
    let png_before = job.tile_rgb(0, 0).unwrap();
    let _ = job.tile_f32(0, 0, 0).unwrap().unwrap();
    assert_eq!(job.tile_rgb(0, 0).unwrap(), png_before, "the existing PNG pixels are untouched");
    job.lock().channels[0].stretch = Some(Stretch { low: 99.0, span: 1.0 });
    assert_eq!(job.raw_metadata().unwrap(), metadata, "metadata is a finish-time snapshot");
}

#[test]
fn empty_and_constant_channels_have_explicit_default_ranges() {
    let grid = TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let empty = test_job("empty", grid.clone(), 0, (grey_recipe(40), true));
    empty.prepare();
    empty.finish();
    let empty = empty.raw_metadata().unwrap();
    assert_eq!(empty.channels[0].finite_count, 0);
    assert_eq!((empty.channels[0].default_black, empty.channels[0].default_white, empty.channels[0].default_span), (None, None, None));

    let constant = test_job("constant", grid, 0, (grey_recipe(40), true));
    constant.prepare();
    publish_values(&constant, 0, |_, _| 7.0);
    constant.finish();
    let constant = constant.raw_metadata().unwrap();
    assert_eq!(constant.channels[0].finite_count, 100);
    assert_eq!(constant.channels[0].default_black, Some(7.0));
    assert_eq!(constant.channels[0].default_white, Some(8.0), "constant data uses span 1");
    assert_eq!(constant.channels[0].default_span, Some(1.0), "constant data uses effective span 1");
}

async fn get_tile(jobs: &Arc<Jobs>, id: &str, format: Option<&str>, channel: Option<usize>, x: u32, y: u32) -> Response {
    tile(
        Extension(Arc::clone(jobs)),
        Path(id.into()),
        Query(TileQuery { x, y, v: None, channel, format: format.map(str::to_owned) }),
    )
    .await
}

#[tokio::test]
async fn raw_handlers_report_states_and_leave_png_available_in_every_state() {
    let jobs = Arc::new(Jobs::default());
    let grid = TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let (job, _) = insert(&jobs, "g", grid, 0, (grey_recipe(40), true));
    job.prepare();
    job.finish();
    for (state, expected) in [
        (MosaicState::Planning, StatusCode::CONFLICT),
        (MosaicState::Reading, StatusCode::CONFLICT),
        (MosaicState::Done, StatusCode::OK),
        (MosaicState::Failed, StatusCode::GONE),
        (MosaicState::Cancelled, StatusCode::GONE),
    ] {
        job.lock().state = state;
        let metadata = raw_metadata(Extension(Arc::clone(&jobs)), Path(job.id.clone())).await;
        assert_eq!(metadata.status(), expected);
        assert_eq!(get_tile(&jobs, &job.id, Some("f32"), Some(0), 0, 0).await.status(), expected);
        let png = get_tile(&jobs, &job.id, None, None, 0, 0).await;
        assert_eq!(png.status(), StatusCode::OK, "PNG still serves progressive and terminal jobs");
        assert_eq!(png.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(get_tile(&jobs, &job.id, Some("f32"), Some(1), 0, 0).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get_tile(&jobs, &job.id, Some("f32"), Some(0), 1, 0).await.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(get_tile(&jobs, "absent", Some("f32"), Some(0), 0, 0).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(raw_metadata(Extension(Arc::clone(&jobs)), Path("absent".into())).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get_tile(&jobs, &job.id, Some("f32"), None, 0, 0).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get_tile(&jobs, &job.id, Some("f64"), Some(0), 0, 0).await.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn raw_response_headers_match_clipped_body_and_png_bytes_do_not_change() {
    let jobs = Arc::new(Jobs::default());
    let grid = TanGrid::new(RA, DEC, 300.0 * PIXEL_SCALE_DEG);
    let (job, _) = insert(&jobs, "g", grid, 0, (grey_recipe(40), true));
    job.prepare();
    job.finish();
    let png = get_tile(&jobs, &job.id, None, None, 1, 1).await;
    let png = axum::body::to_bytes(png.into_body(), usize::MAX).await.unwrap();
    let raw = get_tile(&jobs, &job.id, Some("f32"), Some(0), 1, 1).await;
    assert_eq!(raw.status(), StatusCode::OK);
    assert_eq!(raw.headers()[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(raw.headers()[header::CONTENT_LENGTH], (44 * 44 * 4).to_string());
    assert_eq!(raw.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(raw.headers()["x-content-type-options"], "nosniff");
    let raw = axum::body::to_bytes(raw.into_body(), usize::MAX).await.unwrap();
    assert_eq!(raw.len(), 44 * 44 * 4);
    assert!(raw.chunks(4).all(|bytes| bytes == 0x7fc0_0000u32.to_le_bytes()));
    let repeated = get_tile(&jobs, &job.id, Some("png"), None, 1, 1).await;
    let repeated = axum::body::to_bytes(repeated.into_body(), usize::MAX).await.unwrap();
    assert_eq!(repeated, png, "the existing PNG encoding is bit-for-bit unchanged");
}

#[test]
fn raw_activity_keeps_reads_alive_but_does_not_pin_lru() {
    let jobs = Jobs::default();
    let grid = || TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let (first, _) = insert(&jobs, "first", grid(), 0, (grey_recipe(40), true));
    let (_second, _) = insert(&jobs, "second", grid(), 0, (grey_recipe(40), true));
    let (_c, _) = insert(&jobs, "c", grid(), 0, (grey_recipe(40), true));
    let (_d, _) = insert(&jobs, "d", grid(), 0, (grey_recipe(40), true));
    let old = Instant::now() - ABANDON_AFTER - Duration::from_secs(1);
    *first.last_poll.lock().unwrap() = old;
    *first.last_activity.lock().unwrap() = old;
    assert!(first.abandoned());
    // raw_metadata touches activity (keeps reads alive) but not status-poll LRU.
    assert_eq!(first.raw_metadata(), Err(StatusCode::CONFLICT));
    assert!(!first.abandoned());
    assert_eq!(first.polled_at(), old, "raw activity is not an LRU status poll");
    // Capacity full of warm runners except first has the coldest poll — next insert drops it.
    let (_e, _) = insert(&jobs, "e", grid(), 0, (grey_recipe(40), true));
    assert!(jobs.find(&first.id).is_none(), "the oldest status-polled job is still evicted");
}

#[test]
fn raw_metadata_uses_ui_bands_and_wavelength_limits_including_clipped_windows() {
    let custom = ColorBands {
        blue: crate::ColorBand { center: 0, width: 7 },
        ..ColorBands::DEFAULT
    };
    let grid = TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let job = test_job("color", grid, 0, (recipe(&view(Mode::Composite, vec![]), true, custom).unwrap(), true));
    job.prepare();
    job.finish();
    let metadata = job.raw_metadata().unwrap();
    assert_eq!(metadata.channels.iter().map(|channel| channel.role.as_str()).collect::<Vec<_>>(), ["blue", "green", "red"]);
    let blue = &metadata.channels[0];
    assert_eq!(blue.center_band, Some(1));
    assert_eq!(blue.width, 7, "keep the requested width even at an edge");
    assert_eq!(blue.bands, [1, 2, 3, 4]);
    assert_eq!(blue.wavelength_min_um, BANDS[..4].iter().map(|band| band.wl_min_um).fold(f64::MAX, f64::min));
    assert_eq!(blue.wavelength_max_um, BANDS[..4].iter().map(|band| band.wl_max_um).fold(f64::MIN, f64::max));
}

#[test]
fn a_tile_row_runs_north_to_south_and_east_to_west() {
    let job = job(300.0);
    publish_values(&job, 2, |x, y| if (x, y) == (0, 299) { 1.0 } else { f32::NAN });
    publish_values(&job, 1, |x, y| if (x, y) == (299, 0) { 1.0 } else { f32::NAN });
    publish_values(&job, 0, |_, _| f32::NAN);
    for channel in &mut job.lock().channels {
        channel.stretch = Some(Stretch { low: 0.0, span: 1.0 });
    }
    let (width, height, rgb) = job.tile_rgb(0, 0).unwrap();
    assert_eq!((width, height), (256, 256));
    assert_eq!(rgb[..3], [255, 0, 0]);
    assert_eq!(rgb[3..6], NODATA);
    let (width, height, rgb) = job.tile_rgb(1, 1).unwrap();
    assert_eq!((width, height), (44, 44), "edge tiles are cut to the grid");
    assert_eq!(rgb[rgb.len() - 3..], [0, 255, 0]);
    assert!(job.tile_rgb(2, 0).is_none());
}

#[test]
fn a_tile_is_not_reported_dirty_more_than_once_a_second() {
    let job = job(300.0);
    let start = Instant::now();
    let mut progress = job.lock();
    progress.tiles[0].pending = 2;
    progress.tiles[0].dirty = true;
    progress.publish(start);
    assert_eq!(progress.tiles[0].version, 1);
    progress.tiles[0].dirty = true;
    progress.publish(start + Duration::from_millis(400));
    assert_eq!(progress.tiles[0].version, 1, "held back");
    progress.publish(start + Duration::from_millis(1100));
    assert_eq!(progress.tiles[0].version, 2);
    progress.tiles[0].pending = 0;
    progress.tiles[0].dirty = true;
    progress.publish(start + Duration::from_millis(1200));
    assert_eq!(progress.tiles[0].version, 3, "a tile that settled is not held back");
}

#[test]
fn a_locked_stretch_stops_moving_and_redraws_what_was_drawn() {
    let job = job(100.0);
    job.lock().tiles[0].version = 1;
    let start = Instant::now();
    publish_values(&job, 0, |_, y| if y < 10 { 5.0 } else { f32::NAN });
    job.refresh_stretch(start);
    assert!(!job.lock().channels[0].locked);
    publish_values(&job, 0, |_, y| match y {
        0..=9 => 5.0,
        10..=39 => 9.0,
        _ => f32::NAN,
    });
    job.refresh_stretch(start + DIRTY_EVERY);
    assert!(job.lock().channels[0].locked);
    assert!(job.lock().tiles[0].dirty, "drawn tiles are redrawn at the lock");
    let locked = job.lock().channels[0].stretch.unwrap().low;
    publish_values(&job, 0, |_, y| match y {
        0..=9 => 5.0,
        10..=39 => 9.0,
        _ => 100.0,
    });
    job.refresh_stretch(start + DIRTY_EVERY * 2);
    assert_eq!(job.lock().channels[0].stretch.unwrap().low, locked);
}

#[test]
fn the_footprint_keeps_every_tile_the_strip_reaches_and_no_more() {
    let grid = TanGrid::new(RA, DEC, 2000.0 * PIXEL_SCALE_DEG);
    let cols = grid.width.div_ceil(TILE);
    // A 128 px detector at the field centre touches only the four middle tiles.
    let geometry = ExposureGeometry {
        wcs: wcs(RA, DEC),
        wavelength: wave(5),
        header_bytes: 0,
    };
    let (window, tiles) = footprint(&geometry, Some((0.7, 0.9)), &grid, cols).unwrap();
    assert_eq!((window.x0, window.y0, window.width, window.height), (0, 0, 128, 128));
    let middle = cols / 2;
    let expected: Vec<u32> = [(middle - 1, middle - 1), (middle, middle - 1), (middle - 1, middle), (middle, middle)]
        .map(|(x, y)| y * cols + x)
        .into();
    assert_eq!(tiles, expected);
    assert!(footprint(&geometry, Some((3.0, 3.5)), &grid, cols).is_none(), "wrong band");
    let far = ExposureGeometry {
        wcs: wcs(RA + 5.0, DEC),
        wavelength: wave(5),
        header_bytes: 0,
    };
    assert!(footprint(&far, Some((0.7, 0.9)), &grid, cols).is_none(), "off the field");
}

#[test]
fn a_file_whose_band_misses_the_grid_is_dropped_even_if_its_margin_reaches() {
    // Rows up to 40 are at 0.8 um and the rest at 1.0 um. The grid sees rows 78
    // and up, so its margin rows are all it has of 0.8 um.
    let wavelength = WavelengthMap {
        x: vec![1.0, 128.0],
        y: vec![1.0, 40.0, 41.0, 128.0],
        wavelength: vec![0.8, 0.8, 0.8, 0.8, 1.0, 1.0, 1.0, 1.0],
        bandwidth: vec![0.01; 8],
        nx: 2,
        ny: 4,
    };
    let mut wcs = wcs(RA, DEC);
    wcs.crpix = [64.5, 128.0];
    let geometry = ExposureGeometry { wcs, wavelength, header_bytes: 0 };
    let grid = TanGrid::new(RA, DEC, 100.0 * PIXEL_SCALE_DEG);
    assert!(footprint(&geometry, Some((0.95, 1.05)), &grid, 1).is_some(), "1.0 um is here");
    assert!(geometry.wavelength.rows_for_wavelength(0.79, 0.81, geometry.wcs.naxis).is_some());
    assert!(footprint(&geometry, Some((0.79, 0.81)), &grid, 1).is_none(), "0.8 um is not");
}

#[test]
fn the_snake_visits_every_tile_once_starting_top_left_and_ending_centre() {
    assert_eq!(snake_order(1, 1), [0]);
    assert_eq!(snake_order(2, 2), [0, 1, 3, 2]);
    assert_eq!(snake_order(3, 3), [0, 1, 2, 5, 8, 7, 6, 3, 4]);
    // Non-square: 4 wide, 2 tall.
    assert_eq!(snake_order(4, 2), [0, 1, 2, 3, 7, 6, 5, 4]);
    // Tall: 2 wide, 4 tall.
    assert_eq!(snake_order(2, 4), [0, 1, 3, 5, 7, 6, 4, 2]);
    for (cols, rows) in [(1, 5), (5, 1), (4, 4), (5, 3), (3, 5), (12, 12)] {
        let order = snake_order(cols, rows);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..cols * rows).collect::<Vec<_>>(), "{cols}x{rows} covers every tile once");
        if cols == rows && cols % 2 == 1 {
            let centre = (rows / 2) * cols + cols / 2;
            assert_eq!(*order.last().unwrap(), centre, "odd squares end at the centre");
        }
    }
    assert_eq!(snake_order(3, 3)[0], 0, "the first tile is the top-left one");
}


#[test]
fn reads_follow_the_snake_and_skip_tiles_no_exposure_touches() {
    let plan = |name: &str, tiles: Vec<u32>| Planned {
        exposure: exposure(name, 5).0,
        channel: 0,
        window: PixelWindow { x0: 0, y0: 0, width: 1, height: 1, clipped: false },
        tiles,
    };
    // 3 by 3, nothing touches tiles 1, 2 or 5: no read waits on them. The
    // box path still walks every tile; only the reads skip.
    let ordered = snake_plan(
        vec![plan("centre-5", vec![3, 4]), plan("corner-5", vec![0]), plan("bottom-5", vec![6, 7, 8])],
        3,
        3,
    );
    let names: Vec<&str> = ordered.iter().map(|plan| plan.exposure.url.as_str()).collect();
    assert_eq!(names, ["corner-5", "bottom-5", "centre-5"]);
    // Every exposure is queued exactly once, even when it touches several tiles.
    let ordered = snake_plan(
        vec![plan("wide-5", vec![0, 8]), plan("top-5", vec![1]), plan("low-5", vec![8])],
        3,
        3,
    );
    let names: Vec<&str> = ordered.iter().map(|plan| plan.exposure.url.as_str()).collect();
    assert_eq!(names, ["wide-5", "top-5", "low-5"]);
}

#[test]
fn reads_interleave_across_channels_so_no_box_starves() {
    let plan = |name: &str, channel: usize, tiles: Vec<u32>| Planned {
        exposure: exposure(name, 5).0,
        channel,
        window: PixelWindow { x0: 0, y0: 0, width: 1, height: 1, clipped: false },
        tiles,
    };
    // Channel 0 touches the top row first, channel 1 the bottom row first.
    // Dealt round-robin, the two channels take turns from the start.
    let ordered = snake_plan(
        vec![
            plan("a0-5", 0, vec![0]),
            plan("b0-5", 0, vec![1]),
            plan("a1-5", 1, vec![8]),
            plan("b1-5", 1, vec![7]),
        ],
        3,
        3,
    );
    let names: Vec<&str> = ordered.iter().map(|plan| plan.exposure.url.as_str()).collect();
    assert_eq!(names, ["a0-5", "a1-5", "b0-5", "b1-5"]);
    // Every file is still read exactly once.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, ["a0-5", "a1-5", "b0-5", "b1-5"]);
}

#[test]
fn the_box_path_covers_every_tile_even_ones_no_read_touches() {
    // One planned read on tile 0 of a 2 by 2 grid. The path still walks all 4.
    let job = job(300.0);
    {
        let mut progress = job.lock();
        progress.channel_total = vec![1, 0, 0];
        progress.channel_pending = vec![vec![1, 0, 0, 0], vec![0; 4], vec![0; 4]];
        progress.state = MosaicState::Reading;
        progress.tiles[0].pending = 1;
        progress.tiles[0].planned = true;
    }
    let status = job.status(0);
    assert_eq!(status.order.len(), 4, "the full spiral, not just planned tiles");
    let mut sorted: Vec<u32> = status.order.iter().map(|tile| tile.y * 2 + tile.x).collect();
    sorted.sort_unstable();
    assert_eq!(sorted, [0, 1, 2, 3]);
    assert_eq!(status.channels_total, [1, 0, 0]);
    assert_eq!(status.channels_done, [0, 0, 0]);
    // Channel 0 has a read pending on tile 0, so its frontier is the start;
    // the empty channels block nothing.
    assert_eq!(status.frontiers, [0, 4, 4]);
}

fn planned(name: &str, channel: usize, tiles: Vec<u32>) -> Planned {
    Planned {
        exposure: exposure(name, 5).0,
        channel,
        window: PixelWindow { x0: 0, y0: 0, width: 1, height: 1, clipped: false },
        tiles,
    }
}

#[test]
fn a_tile_is_finished_only_when_every_read_of_it_has_settled() {
    // 2 by 2, path 0, 1, 3, 2. Channel 0 has two reads on tile 0 and one on
    // tile 3; channel 1 reads tile 3 only.
    let job = job(300.0);
    let plans = [
        planned("a-5", 0, vec![0]),
        planned("b-5", 0, vec![0]),
        planned("c-5", 0, vec![3]),
        planned("d-5", 1, vec![3]),
    ];
    job.begin_reading(&plans, 0, 0, 0);
    let frontiers = || job.status(0).frontiers;
    assert_eq!(frontiers()[..2], [0, 2], "channel 1 passes tiles it never touches");
    job.settle_read(0, &[0]);
    assert_eq!(frontiers()[0], 0, "a read still pending keeps the tile open");
    job.settle_read(0, &[0]);
    assert_eq!(frontiers()[0], 2, "the last read finishes it, and the empty tile after it");
    job.settle_read(1, &[3]);
    assert_eq!(frontiers()[1], 4, "nothing left for channel 1 anywhere");
    job.fail_read(0, &[3]);
    assert_eq!(frontiers()[0], 4, "a failed read settles, so no box stalls on it");
}

#[tokio::test(flavor = "multi_thread")]
async fn settled_reads_move_their_own_channel_only() {
    let job = job(300.0);
    let wanted: Vec<_> = ColorBands::DEFAULT
        .channels()
        .iter()
        .map(|band| exposure(&format!("f-{}", band.center), band.center))
        .collect();
    let fake = Arc::new(Fake { fail: vec!["f-5".into()], ..Fake::default() });
    run(Arc::clone(&job), fake, wanted, permit()).await;
    let status = job.status(0);
    assert_eq!(status.state, MosaicState::Done);
    // The failed blue read still settles its channel, so no box stalls.
    assert_eq!(status.channels_done, [1, 1, 1]);
    assert_eq!(status.channels_total, [1, 1, 1]);
}

#[test]
fn the_three_layers_add_up_to_the_rgb_tile() {
    let job = job(300.0);
    for (slot, value) in [1.0, 2.0, 3.0].iter().enumerate() {
        // Every pixel has data in every channel, so no NODATA is involved.
        publish_values(&job, slot, |_, _| *value);
        job.lock().channels[slot].stretch = Some(Stretch { low: 0.0, span: 4.0 });
    }
    let (_, _, rgb) = job.tile_rgb(0, 0).unwrap();
    let mut layered = vec![0u8; rgb.len()];
    for channel in 0..3 {
        let (_, _, layer) = job.tile_layer(channel, 0, 0).unwrap();
        for (out, add) in layered.iter_mut().zip(layer) {
            *out = out.saturating_add(add);
        }
    }
    assert_eq!(rgb, layered, "lighter over black is the RGB tile");
    // One channel alone paints only its own gun.
    let (_, _, red) = job.tile_layer(2, 0, 0).unwrap();
    assert_eq!(red[..3], [shade(Some(3.0), Some(Stretch { low: 0.0, span: 4.0 })), 0, 0]);
    assert!(job.tile_layer(3, 0, 0).is_none(), "no fourth channel");
}

#[test]
fn normalized_layers_reproduce_the_server_guns_within_quantization() {
    let job = job(300.0);
    for (slot, value) in [0.8f32, 1.7, 3.1].iter().enumerate() {
        publish_values(&job, slot, |_, _| *value);
    }
    job.finish();
    {
        let mut progress = job.lock();
        for channel in &mut progress.channels {
            channel.stretch = Some(Stretch { low: 0.0, span: 4.0 });
        }
    }
    let metadata = job.raw_metadata().unwrap();
    // The bottom-left grid pixel is the first pixel of the bottom tile row.
    let (_, _, server) = job.tile_rgb(0, 1).unwrap();
    let decoded = |channel: usize| {
        let channel = &metadata.channels[channel];
        let bytes = job.tile_scale16(channel.index, 0, 1).unwrap().unwrap();
        let decodes = |at: usize| {
            let code = f32::from(u16::from_le_bytes(bytes[at * 2..at * 2 + 2].try_into().unwrap()));
            let (offset, span) = (channel.scale_offset.unwrap(), channel.scale_span.unwrap());
            offset + (code / f32::from(SCALE_MAX)) * span
        };
        // Tile pixel 0 is the top-right corner of the grid; all pixels share a value.
        let gun = (((decodes(0) - 0.0) / 4.0).clamp(0.0, 1.0).powf(0.7) * 255.0).round() as u8;
        (gun, decodes(0))
    };
    for (channel, expected) in [(0usize, 0.8f32), (1, 1.7), (2, 3.1)] {
        let (gun, value) = decoded(channel);
        assert!((value - expected).abs() < 0.001, "channel {channel} decodes to {value}");
        assert!(gun.abs_diff(server[2 - channel]) <= 1, "channel {channel}: {gun} vs {}", server[2 - channel]);
    }
}

#[test]
fn jobs_evict_finished_before_running_when_full() {
    let jobs = Jobs::default();
    let grid = || TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let (first, fresh) = insert(&jobs, "a", grid(), 1, (color_recipe(), true));
    assert!(fresh);
    let (again, fresh) = insert(&jobs, "a", grid(), 1, (color_recipe(), true));
    assert!(!fresh, "a repeat joins");
    assert_eq!(first.id, again.id);
    let (second, _) = insert(&jobs, "b", grid(), 1, (color_recipe(), true));
    second.end(MosaicState::Done, None);
    let (_c, _) = insert(&jobs, "c", grid(), 1, (color_recipe(), true));
    let (_d, _) = insert(&jobs, "d", grid(), 1, (color_recipe(), true));
    assert_eq!(jobs.list.lock().unwrap().len(), MAX_JOBS);
    let (_e, _) = insert(&jobs, "e", grid(), 1, (color_recipe(), true));
    assert!(jobs.find(&second.id).is_none(), "the finished job went first");
    assert!(jobs.find(&first.id).is_some());
    assert!(!first.cancel.load(Ordering::Relaxed));
}

#[test]
fn a_job_still_being_watched_outlives_one_that_was_left() {
    let jobs = Jobs::default();
    let grid = || TanGrid::new(RA, DEC, 10.0 * PIXEL_SCALE_DEG);
    let (watched, _) = insert(&jobs, "a", grid(), 1, (color_recipe(), true));
    let (left, _) = insert(&jobs, "b", grid(), 1, (color_recipe(), true));
    let (_c, _) = insert(&jobs, "c", grid(), 1, (color_recipe(), true));
    let (_d, _) = insert(&jobs, "d", grid(), 1, (color_recipe(), true));
    *left.last_poll.lock().unwrap() = Instant::now() - ABANDON_AFTER - Duration::from_secs(1);
    *left.last_activity.lock().unwrap() = Instant::now() - ABANDON_AFTER - Duration::from_secs(1);
    let (_new, _) = insert(&jobs, "e", grid(), 1, (color_recipe(), true));
    assert!(jobs.find(&watched.id).is_some(), "the older job is the one with a reader");
    assert!(jobs.find(&left.id).is_none());
}

#[test]
fn a_cancelled_job_is_not_joined() {
    let jobs = Jobs::default();
    let (job, _) = insert(&jobs, "a", TanGrid::new(RA, DEC, 0.1), 1, (color_recipe(), true));
    job.end(MosaicState::Cancelled, None);
    let (fresh_job, fresh) = insert(&jobs, "a", TanGrid::new(RA, DEC, 0.1), 1, (color_recipe(), true));
    assert!(fresh);
    assert_ne!(job.id, fresh_job.id);
    assert!(jobs.find(&job.id).is_none());
}

#[test]
fn the_color_route_and_a_mosaic_tile_use_the_same_tone() {
    let job = job(3.0);
    let values: [Vec<f32>; 3] = [
        vec![0.1, 0.2, f32::NAN, 0.4, 0.5, 0.6, 0.7, 0.8, f32::NAN],
        vec![0.3, 0.2, f32::NAN, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9],
        vec![0.1, 0.1, f32::NAN, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9],
    ];
    for (slot, values) in values.iter().enumerate() {
        publish_values(&job, slot, |x, y| values[(y * 3 + x) as usize]);
        job.lock().channels[slot].stretch = stretch_of(
            Display::Color,
            values.iter().copied().filter(|value| value.is_finite()).collect(),
        );
    }
    let (_, _, rgb) = job.tile_rgb(0, 0).unwrap();
    let stacks: Vec<Stacked> = values
        .iter()
        .map(|values| Stacked { width: 3, height: 3, value: values.clone() })
        .collect();
    let png = combine_color(&stacks).unwrap();
    let decoded = image::load_from_memory(&png).unwrap().to_rgb8().into_raw();
    assert_eq!(rgb, decoded, "color.png and the mosaic must use one tone");
    assert!(rgb.chunks(3).any(|pixel| pixel == NODATA), "all-missing stays no-data");
}
