use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, OnceLock,
};

use spearv_core::{
    math::{
        Counts, ImageMath, MaskPolicy, MathControl, MathError, MathLease, NativeInput,
        NativeWindow, OutputPlanes, Product, RasterOrder, Shape, SpectrumInput, TargetGrid, Tile,
        TileOutput, Values,
    },
    Context, FlagBits, Poly, Sip, Wcs, WorkBudget,
};
use spearv_science::{spawn_math, spawn_spectrum, PythonMath, RuntimeConfig};

fn runtime() -> &'static PythonMath {
    static RUNTIME: OnceLock<PythonMath> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        PythonMath::initialize(
            RuntimeConfig::host_qualification(),
            Arc::new(WorkBudget::new(512)),
        )
        .unwrap()
    })
}

fn control() -> MathControl {
    MathControl::new(Arc::new(AtomicU64::new(1)))
}

fn tile(width: u32, height: u32) -> Tile {
    Tile {
        x: 0,
        y: 0,
        shape: Shape { width, height },
        order: RasterOrder::Fits,
    }
}

fn target(shape: Shape) -> TargetGrid {
    TargetGrid {
        ra: 10.0,
        dec: 20.0,
        shape,
        scale_deg: spearv_core::PIXEL_SCALE_DEG,
    }
}

fn wcs(shape: Shape) -> Wcs {
    Wcs {
        crpix: [
            (f64::from(shape.width) + 1.0) / 2.0,
            (f64::from(shape.height) + 1.0) / 2.0,
        ],
        crval: [10.0, 20.0],
        cd: [
            [-spearv_core::PIXEL_SCALE_DEG, 0.0],
            [0.0, spearv_core::PIXEL_SCALE_DEG],
        ],
        sip: Sip {
            a: Poly::new(0),
            b: Poly::new(0),
            ap: None,
            bp: None,
        },
        naxis: [shape.width as i32, shape.height as i32],
    }
}

fn bits() -> FlagBits {
    FlagBits {
        transient: 0,
        overflow: 1,
        sur_error: 2,
        nonfunc: 6,
        fullsample: 12,
        persist: 17,
        outlier: 19,
        source: 21,
    }
}

fn window(shape: Shape) -> NativeWindow {
    NativeWindow {
        shape,
        origin: [1, 1],
    }
}

fn input(
    shape: Shape,
    image: &[f32],
    variance: &[f32],
    zodi: Option<&[f32]>,
    flags: &[u32],
    bands: &[u16],
) -> NativeInput {
    NativeInput::copy_from(
        runtime().budget(),
        runtime().config().max_native_pixels,
        window(shape),
        &wcs(shape),
        image,
        variance,
        zodi,
        flags,
        bands,
        &vec![true; image.len()],
    )
    .unwrap()
}

fn f32_values(output: &TileOutput) -> &[f32] {
    let Values::F32(values) = &output.planes().value else {
        panic!("f32 result");
    };
    values
}

fn f32_variance(output: &TileOutput) -> &[f32] {
    let Some(Values::F32(values)) = &output.planes().variance else {
        panic!("f32 variance");
    };
    values
}

#[test]
fn preflight_typed_context_and_rejected_shapes() {
    assert!(runtime().description().contains("isolated"));
    let context = Context::new(Arc::default());
    assert!(matches!(context.math(), Err(MathError::Runtime(_))));
    assert!(context
        .with_math(Arc::new(runtime().clone()))
        .math()
        .is_ok());
    assert!(runtime()
        .session(control(), tile(0, 1), Product::Values)
        .is_err());
    assert!(runtime()
        .session(control(), tile(257, 1), Product::Values)
        .is_err());
    assert!(runtime()
        .session(
            control(),
            tile(1, 1),
            Product::CompleteBands { bands: vec![] }
        )
        .is_err());
    assert!(NativeInput::copy_from(
        runtime().budget(),
        1,
        window(Shape {
            width: 1,
            height: 1
        }),
        &wcs(Shape {
            width: 1,
            height: 1
        }),
        &[1.],
        &[],
        None,
        &[0],
        &[0],
        &[true]
    )
    .is_err());
    let mut wrong = RuntimeConfig::host_qualification();
    wrong.packages = "/nonexistent/spearv/math".into();
    assert!(matches!(
        PythonMath::initialize(wrong, Arc::clone(runtime().budget())),
        Err(MathError::Runtime(_))
    ));
}

#[test]
fn library_geometry_identity_singletons_rectangles_orientation_and_independence() {
    for shape in [
        Shape {
            width: 1,
            height: 1,
        },
        Shape {
            width: 1,
            height: 6,
        },
        Shape {
            width: 6,
            height: 1,
        },
        Shape {
            width: 6,
            height: 3,
        },
    ] {
        let mut session = runtime()
            .session(control(), tile(shape.width, shape.height), Product::Values)
            .unwrap();
        let geometry = session
            .geometry(&wcs(shape), window(shape), target(shape))
            .unwrap();
        assert_eq!(
            geometry.indices(),
            (0..shape.len().unwrap() as i64).collect::<Vec<_>>()
        );
        let mut png_tile = tile(shape.width, shape.height);
        png_tile.order = RasterOrder::Png;
        let mut png = runtime()
            .session(control(), png_tile, Product::Values)
            .unwrap();
        let geometry = png
            .geometry(&wcs(shape), window(shape), target(shape))
            .unwrap();
        let expected: Vec<_> = (0..shape.height)
            .rev()
            .flat_map(|y| (0..shape.width).map(move |x| i64::from(y * shape.width + x)))
            .collect();
        assert_eq!(geometry.indices(), expected);
    }
    let barrier = Arc::new(std::sync::Barrier::new(4));
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|seed| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let shape = Shape {
                        width: 3,
                        height: 2,
                    };
                    let mut native = wcs(shape);
                    native.crval[0] += f64::from(seed) * 30.0;
                    let mut grid = target(shape);
                    grid.ra = native.crval[0];
                    let mut session = runtime()
                        .session(control(), tile(3, 2), Product::Values)
                        .unwrap();
                    barrier.wait();
                    assert_eq!(
                        session
                            .geometry(&native, window(shape), grid)
                            .unwrap()
                            .indices(),
                        &[0, 1, 2, 3, 4, 5]
                    );
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

#[test]
fn f32_subtraction_before_f64_ordered_sum_flags_selection_and_nodata() {
    let shape = Shape {
        width: 6,
        height: 1,
    };
    let mut session = runtime()
        .session(control(), tile(6, 1), Product::Science)
        .unwrap();
    let geometry = session
        .geometry(&wcs(shape), window(shape), target(shape))
        .unwrap();
    let exposure_values = [
        [16_777_216., 1e20, 10., f32::NAN, 5., 0.],
        [0., 1., 20., 8., 5., 2.],
        [0., -1e20, 30., 9., 5., 4.],
    ];
    let variances = [
        [1., 1., 1., 1., 0., 1.],
        [1., 1., 3., -1., f32::INFINITY, 1.],
        [1., 1., 1., f32::NAN, 1., 1.],
    ];
    for i in 0..3 {
        let flags = if i == 1 {
            [0, 0, 1, 0, 0, 1]
        } else {
            [0, 0, 0, 0, 0, 1]
        };
        let zodi = if i == 0 {
            [-1., 0., 2., 0., 0., 0.]
        } else {
            [0., 0., 2., 0., 0., 0.]
        };
        let native = input(
            shape,
            &exposure_values[i],
            &variances[i],
            Some(&zodi),
            &flags,
            &[0; 6],
        );
        session
            .add(
                i as u64,
                &native,
                &geometry,
                bits(),
                MaskPolicy::Composite,
                &[],
            )
            .unwrap();
    }
    let output = session.snapshot().unwrap();
    assert_eq!(&f32_values(&output)[..3], &[5_592_405.5, 0., 18.]);
    assert_eq!(f32_values(&output)[4], 5.);
    assert!(f32_values(&output)[3].is_nan() && f32_values(&output)[5].is_nan());
    let Some(Counts::U16(counts)) = &output.planes().counts else {
        panic!("counts");
    };
    assert_eq!(counts, &[3, 3, 2, 0, 1, 0]);
    assert_eq!(&f32_variance(&output)[..3], &[1.0 / 3.0, 1.0 / 3.0, 0.5]);
    let native = input(shape, &[1.; 6], &[1.; 6], None, &[0; 6], &[0; 6]);
    assert!(session
        .add(2, &native, &geometry, bits(), MaskPolicy::None, &[])
        .is_err());
    let mut disjoint = runtime()
        .session(control(), tile(6, 1), Product::Values)
        .unwrap();
    disjoint
        .add(0, &native, &geometry, bits(), MaskPolicy::None, &[1])
        .unwrap();
    assert!(f32_values(&disjoint.finish().unwrap())
        .iter()
        .all(|v| v.is_nan()));
}

#[test]
fn source_flag_exception_delta_outlier_strict_spectrum_and_unknown_bits() {
    let shape = Shape {
        width: 4,
        height: 1,
    };
    let native = input(
        shape,
        &[5.; 4],
        &[1.; 4],
        None,
        &[(1 << 21) | 1, 1 << 19, 1 << 31, (1 << 21) | (1 << 6)],
        &[0; 4],
    );
    for (mask, expected) in [
        (MaskPolicy::Composite, [true, false, false, false]),
        (MaskPolicy::Delta, [true, true, false, false]),
        (MaskPolicy::Spectrum, [false, false, false, false]),
    ] {
        let mut session = runtime()
            .session(control(), tile(4, 1), Product::Values)
            .unwrap();
        let geometry = session
            .geometry(&wcs(shape), window(shape), target(shape))
            .unwrap();
        session
            .add(0, &native, &geometry, bits(), mask, &[])
            .unwrap();
        let output = session.finish().unwrap();
        assert_eq!(
            f32_values(&output)
                .iter()
                .map(|v| v.is_finite())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(output.planes().variance.is_none() && output.planes().counts.is_none());
    }
}

#[test]
fn specific_depth_f64_complete_bands_duplicate_stop_and_counts() {
    let shape = Shape {
        width: 3,
        height: 1,
    };
    let mut sector = runtime()
        .session(
            control(),
            tile(3, 1),
            Product::CompleteBands {
                bands: vec![1, 0, 1],
            },
        )
        .unwrap();
    for (band, values, variances) in [
        (0, [2., 0., 1.], [4., 1., 1.]),
        (1, [10., 4., 9.], [9., 4., 0.]),
    ] {
        let mut depth = runtime()
            .session(control(), tile(3, 1), Product::Depth { band })
            .unwrap();
        let geometry = depth
            .geometry(&wcs(shape), window(shape), target(shape))
            .unwrap();
        let native = input(shape, &values, &variances, None, &[0; 3], &[band; 3]);
        depth
            .add(0, &native, &geometry, bits(), MaskPolicy::None, &[])
            .unwrap();
        let finished = depth.finish().unwrap();
        assert!(matches!(finished.planes().value, Values::F64(_)));
        assert!(matches!(finished.planes().counts, Some(Counts::U64(_))));
        sector.add_band(&finished).unwrap();
        assert!(sector.add_band(&finished).is_err());
    }
    let finished = sector.finish().unwrap();
    assert_eq!(&f32_values(&finished)[..2], &[6., 2.]);
    assert_eq!(&f32_variance(&finished)[..2], &[3.25, 1.25]);
    assert!(f32_values(&finished)[2].is_nan());
    assert!(runtime()
        .session(
            control(),
            tile(1, 1),
            Product::CompleteBands { bands: vec![0] }
        )
        .unwrap()
        .finish()
        .is_err());
}

fn fixture_output(
    product: Product,
    value: Vec<f32>,
    variance: Option<Vec<f32>>,
    counts: Option<Vec<u16>>,
) -> TileOutput {
    let tile = tile(value.len() as u32, 1);
    TileOutput::from_library(
        tile,
        product,
        OutputPlanes {
            value: Values::F32(value),
            variance: variance.map(Values::F32),
            counts: counts.map(Counts::U16),
        },
        MathLease::reserve(runtime().budget(), 1024).unwrap(),
    )
    .unwrap()
}

#[test]
fn library_delta_holes_f32_precision_variance_and_saturating_counts() {
    let plus = fixture_output(
        Product::Science,
        vec![16_777_216., f32::NAN, 3.],
        Some(vec![1., f32::NAN, 2.]),
        Some(vec![65534, 0, 1]),
    );
    let minus = fixture_output(
        Product::Science,
        vec![-1., 2., f32::NAN],
        Some(vec![4., 5., f32::NAN]),
        Some(vec![3, 1, 0]),
    );
    let mut session = runtime()
        .session(control(), tile(3, 1), Product::Science)
        .unwrap();
    let output = session.difference(&plus, &minus).unwrap();
    assert_eq!(f32_values(&output)[0], 16_777_216.);
    assert_eq!(f32_variance(&output)[0], 5.);
    assert!(f32_values(&output)[1..].iter().all(|v| v.is_nan()));
    let Some(Counts::U16(counts)) = &output.planes().counts else {
        panic!("counts");
    };
    assert_eq!(counts, &[65535, 0, 0]);
    assert!(matches!(
        session.advanced_operator(),
        Err(MathError::Unsupported(_))
    ));
}

#[test]
fn spectrum_ordered_flux_invalid_companions_and_metadata_indices() {
    let input = SpectrumInput::copy_from(
        runtime().budget(),
        1024,
        &[1e20, 1., -1e20, 9., f64::NAN, 4., 5.],
        &[1., 1., 1., 0., 1., 4., 1.],
        &[0, 0, 0, 0, 0, 1, u16::MAX],
        &[false; 7],
    )
    .unwrap();
    let result = runtime().spectrum(&control(), &input).unwrap();
    assert_eq!(result.get().value[0], 0.);
    assert_eq!(result.get().counts[..2], [3, 1]);
    assert_eq!(result.get().invalid_counts[0], 2);
    assert_eq!(result.get().last_valid[0], 2);
    assert_eq!(result.get().first_invalid[0], 3);
    assert_eq!(result.get().error[1], 2.);
    assert_eq!(
        result.get().accepted,
        [true, true, true, false, false, true, true]
    );
}

#[test]
fn input_copy_snapshot_reader_pin_eviction_and_finalization_charges() {
    let before = runtime().budget().available_bytes();
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let mut source = vec![4.];
    let native = input(shape, &source, &[1.], None, &[0], &[0]);
    source[0] = 99.;
    let native_pin = native.clone();
    drop(native);
    let mut session = runtime()
        .session(control(), tile(1, 1), Product::Values)
        .unwrap();
    let geometry = session
        .geometry(&wcs(shape), window(shape), target(shape))
        .unwrap();
    session
        .add(0, &native_pin, &geometry, bits(), MaskPolicy::None, &[])
        .unwrap();
    drop(native_pin);
    drop(geometry);
    let output = session.finish().unwrap();
    assert_eq!(f32_values(&output), &[4.]);
    // Session/sums/worker/bridge are gone. Only the immutable result is charged.
    assert_eq!(runtime().budget().available_bytes(), before - 1024);
    let reader = output.clone();
    drop(output); // Cache eviction, not reader deallocation.
    let all_free = runtime().budget().try_acquire_exact(before - 1024).unwrap();
    assert!(runtime().budget().try_acquire_exact(1).is_none());
    assert_eq!(f32_values(&reader), &[4.]);
    drop(all_free);
    drop(reader);
    assert_eq!(runtime().budget().available_bytes(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_request_retains_started_worker_and_input_until_real_exit() {
    let before = runtime().budget().available_bytes();
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let native = input(shape, &[4.], &[1.], None, &[0], &[0]);
    let session = runtime()
        .session(control(), tile(1, 1), Product::Values)
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task = spawn_math(session, move |mut session| {
        // Deterministic rendezvous after a real library transform, before the
        // next library call. No claim that an in-flight NumPy kernel is killed.
        let geometry = session.geometry(&wcs(shape), window(shape), target(shape))?;
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        let result = session.add(0, &native, &geometry, bits(), MaskPolicy::None, &[]);
        drop(geometry);
        drop(native);
        result?;
        session.snapshot()
    })
    .unwrap();
    let mut completion = task.completion();
    entered_rx.await.unwrap();
    let during = runtime().budget().available_bytes();
    drop(task);
    assert!(during < before);
    assert_eq!(
        runtime().budget().available_bytes(),
        during,
        "request drop must not release a started session/input"
    );
    release_tx.send(()).unwrap();
    completion.wait_for(|finished| *finished).await.unwrap();
    assert_eq!(runtime().budget().available_bytes(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_request_does_not_kill_an_active_python_call() {
    use pyo3::{prelude::*, types::PyModule};
    let before = runtime().budget().available_bytes();
    let (entered, release) = Python::attach(|py| {
        let threading = PyModule::import(py, "threading").unwrap();
        let event = threading.getattr("Event").unwrap();
        (
            event.call0().unwrap().unbind(),
            event.call0().unwrap().unbind(),
        )
    });
    let worker_entered = Python::attach(|py| entered.clone_ref(py));
    let worker_release = Python::attach(|py| release.clone_ref(py));
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let native = input(shape, &[4.], &[1.], None, &[0], &[0]);
    let session = runtime()
        .session(control(), tile(1, 1), Product::Values)
        .unwrap();
    let task = spawn_math(session, move |mut session| {
        Python::attach(|py| {
            worker_entered.bind(py).call_method0("set").unwrap();
            worker_release.bind(py).call_method0("wait").unwrap();
            // A real NumPy call after outer cancellation still executes. Its
            // result cannot be published as a successful science operation.
            let np = PyModule::import(py, "numpy").unwrap();
            assert_eq!(
                np.getattr("add")
                    .unwrap()
                    .call1((2., 3.))
                    .unwrap()
                    .extract::<f64>()
                    .unwrap(),
                5.
            );
        });
        drop(native);
        session.snapshot()
    })
    .unwrap();
    let mut completion = task.completion();
    tokio::task::spawn_blocking(move || {
        Python::attach(|py| {
            entered.bind(py).call_method0("wait").unwrap();
        })
    })
    .await
    .unwrap();
    let during = runtime().budget().available_bytes();
    drop(task);
    assert_eq!(runtime().budget().available_bytes(), during);
    Python::attach(|py| {
        release.bind(py).call_method0("set").unwrap();
    });
    completion.wait_for(|finished| *finished).await.unwrap();
    assert_eq!(runtime().budget().available_bytes(), before);
}

#[test]
fn cancelled_stale_output_and_worker_admission_are_explicit() {
    let generation = Arc::new(AtomicU64::new(1));
    let stale = MathControl::new(Arc::clone(&generation));
    let mut session = runtime()
        .session(stale.clone(), tile(1, 1), Product::Values)
        .unwrap();
    generation.store(2, Ordering::Release);
    assert!(matches!(session.snapshot(), Err(MathError::Stale)));
    drop(session);
    let cancelled = control();
    cancelled.cancel();
    assert!(matches!(
        runtime().session(cancelled, tile(1, 1), Product::Values),
        Err(MathError::Cancelled)
    ));
    // Sessions hold only their sums; worker slots are per call (see the
    // runtime boundary test), so more sessions than workers may stay alive.
    let live: Vec<_> = (0..runtime().config().max_workers + 4)
        .map(|_| {
            runtime()
                .session(control(), tile(1, 1), Product::Values)
                .unwrap()
        })
        .collect();
    drop(live);
}

#[test]
fn retained_qr2_qr3_library_geometry_matches_independent_header_oracle() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/retained_geometry.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let native: Wcs = serde_json::from_value(case["wcs"].clone()).unwrap();
        let shape = Shape {
            width: 6,
            height: 3,
        };
        let mut grid = target(shape);
        grid.ra = native.crval[0];
        grid.dec = native.crval[1];
        let mut session = runtime()
            .session(control(), tile(6, 3), Product::Values)
            .unwrap();
        let geometry = session
            .geometry(
                &native,
                NativeWindow {
                    shape: Shape {
                        width: 2040,
                        height: 2040,
                    },
                    origin: [1, 1],
                },
                grid,
            )
            .unwrap();
        let expected: Vec<i64> = serde_json::from_value(case["expected"].clone()).unwrap();
        assert_eq!(geometry.indices(), expected, "{}", case["release"]);
    }
}

#[test]
fn singular_transform_is_an_error_not_finite_nodata() {
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let mut native = wcs(shape);
    native.cd = [[0., 0.], [0., 0.]];
    let mut session = runtime()
        .session(control(), tile(1, 1), Product::Values)
        .unwrap();
    assert!(matches!(
        session.geometry(&native, window(shape), target(shape)),
        Err(MathError::Transform(_))
    ));
}

#[test]
fn explicit_native_validity_and_repeated_edge_tile_snapshots_remain_charged() {
    let before = runtime().budget().available_bytes();
    let shape = Shape {
        width: 256,
        height: 153,
    };
    let n = shape.len().unwrap();
    let mut validity = vec![true; n];
    validity[0] = false; // Finite IMAGE with undefined FLAGS is not a sample.
    let native = NativeInput::copy_from(
        runtime().budget(),
        runtime().config().max_native_pixels,
        window(shape),
        &wcs(shape),
        &vec![3.; n],
        &vec![1.; n],
        None,
        &vec![0; n],
        &vec![0; n],
        &validity,
    )
    .unwrap();
    let with_input = runtime().budget().available_bytes();
    let snapshot = 256 * 153 * 4; // Exact KiB multiple; no MiB rounding.
    for _ in 0..8 {
        let mut session = runtime()
            .session(control(), tile(256, 153), Product::Values)
            .unwrap();
        let geometry = session
            .geometry(&wcs(shape), window(shape), target(shape))
            .unwrap();
        session
            .add(0, &native, &geometry, bits(), MaskPolicy::None, &[])
            .unwrap();
        let cached = session.snapshot().unwrap();
        let pinned_reader = cached.clone();
        drop(cached);
        let finished = session.finish().unwrap();
        drop(geometry);
        assert!(f32_values(&finished)[0].is_nan());
        assert_eq!(f32_values(&finished)[1], 3.);
        assert_eq!(
            runtime().budget().available_bytes(),
            with_input - 2 * snapshot,
            "input and two distinct value snapshots stay charged"
        );
        drop(finished);
        assert_eq!(runtime().budget().available_bytes(), with_input - snapshot);
        drop(pinned_reader);
        assert_eq!(runtime().budget().available_bytes(), with_input);
    }
    drop(native);
    assert_eq!(runtime().budget().available_bytes(), before);
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status
            .lines()
            .filter(|line| line.starts_with("VmHWM:") || line.starts_with("VmRSS:"))
        {
            println!("bounded repeated 256x153 qualification: {line}");
        }
    }
}

#[test]
fn cached_geometry_carries_native_wcs_and_grid_identity_without_recomputing() {
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let mut session = runtime()
        .session(control(), tile(1, 1), Product::Values)
        .unwrap();
    let geometry = session
        .geometry(&wcs(shape), window(shape), target(shape))
        .unwrap();
    let cached = geometry.clone();
    assert_eq!(cached.target(), target(shape));
    assert_eq!(cached.native_wcs(), &wcs(shape));
    let native = input(shape, &[3.], &[1.], None, &[0], &[0]);
    let mut different = wcs(shape);
    different.crval[0] = 11.;
    let wrong = session
        .geometry(
            &different,
            window(shape),
            TargetGrid {
                ra: 11.,
                ..target(shape)
            },
        )
        .unwrap();
    assert!(matches!(
        session.add(0, &native, &wrong, bits(), MaskPolicy::None, &[]),
        Err(MathError::Input(_))
    ));
    session
        .add(0, &native, &cached, bits(), MaskPolicy::None, &[])
        .unwrap();
    assert_eq!(f32_values(&session.finish().unwrap()), &[3.]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_worker_finalizes_sums_and_transfers_the_output_reservation() {
    let before = runtime().budget().available_bytes();
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let native = input(shape, &[3.], &[1.], None, &[0], &[0]);
    let session = runtime()
        .session(control(), tile(1, 1), Product::Values)
        .unwrap();
    let task = spawn_math(session, move |mut session| {
        let geometry = session.geometry(&wcs(shape), window(shape), target(shape))?;
        session.add(0, &native, &geometry, bits(), MaskPolicy::None, &[])?;
        session.finish()
    })
    .unwrap();
    let mut completion = task.completion();
    let output = task.wait().await.unwrap();
    completion.wait_for(|finished| *finished).await.unwrap();
    assert_eq!(f32_values(&output), &[3.]);
    assert_eq!(runtime().budget().available_bytes(), before - 1024);
    drop(output);
    assert_eq!(runtime().budget().available_bytes(), before);
}

#[test]
fn many_live_tile_sessions_resume_across_member_order_with_exact_state_charges() {
    let before = runtime().budget().available_bytes();
    let science = runtime()
        .session(control(), tile(256, 256), Product::Science)
        .unwrap();
    let state = 256 * 256 * 18 + 256;
    assert_eq!(science.state_bytes(), state);
    assert_eq!(
        before - runtime().budget().available_bytes(),
        state.div_ceil(1024) * 1024,
        "persistent sums are charged at their real size, not per-call scratch"
    );
    drop(science);
    assert_eq!(runtime().budget().available_bytes(), before);

    let shape = Shape {
        width: 1,
        height: 1,
    };
    let first = input(shape, &[2.], &[1.], None, &[0], &[0]);
    let second = input(shape, &[4.], &[1.], None, &[0], &[0]);
    // More live sessions than worker slots, filled member-outer like the
    // progressive mosaic: every tile receives member 0, then member 1.
    let mut sessions: Vec<_> = (0..4 * runtime().config().max_workers)
        .map(|_| {
            runtime()
                .session(control(), tile(1, 1), Product::Values)
                .unwrap()
        })
        .collect();
    let geometry = sessions[0]
        .geometry(&wcs(shape), window(shape), target(shape))
        .unwrap();
    for (order, member) in [(0, &first), (1, &second)] {
        for session in &mut sessions {
            session
                .add(order, member, &geometry, bits(), MaskPolicy::None, &[])
                .unwrap();
        }
        if order == 0 {
            assert_eq!(f32_values(&sessions[3].snapshot().unwrap()), &[2.]);
        }
    }
    for session in sessions {
        assert_eq!(f32_values(&session.finish().unwrap()), &[3.]);
    }
    drop((geometry, first, second));
    assert_eq!(runtime().budget().available_bytes(), before);
}

#[test]
fn a_rejected_add_leaves_the_running_sums_unchanged() {
    let shape = Shape {
        width: 1,
        height: 1,
    };
    let mut depth = runtime()
        .session(control(), tile(1, 1), Product::Depth { band: 0 })
        .unwrap();
    let geometry = depth
        .geometry(&wcs(shape), window(shape), target(shape))
        .unwrap();
    let native = input(shape, &[2.], &[1.], None, &[0], &[0]);
    depth
        .add(0, &native, &geometry, bits(), MaskPolicy::None, &[])
        .unwrap();
    // Wrong-length indices from a mismatched geometry never reach the sums.
    let mut wide = runtime()
        .session(control(), tile(2, 1), Product::Depth { band: 0 })
        .unwrap();
    let wide_shape = Shape {
        width: 2,
        height: 1,
    };
    let wide_geometry = wide
        .geometry(&wcs(wide_shape), window(wide_shape), target(wide_shape))
        .unwrap();
    assert!(depth
        .add(1, &native, &wide_geometry, bits(), MaskPolicy::None, &[])
        .is_err());
    let finished = depth.finish().unwrap();
    let Values::F64(value) = &finished.planes().value else {
        panic!("depth is f64");
    };
    assert_eq!(value, &[2.]);
    assert!(matches!(&finished.planes().counts, Some(Counts::U64(c)) if c == &[1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spectrum_runs_in_a_blocking_worker() {
    let input = SpectrumInput::copy_from(
        runtime().budget(),
        16,
        &[1., 3.],
        &[1., 1.],
        &[0, 0],
        &[false; 2],
    )
    .unwrap();
    let math: Arc<dyn ImageMath> = Arc::new(runtime().clone());
    let result = spawn_spectrum(math, control(), input)
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.get().value[0], 2.);
}
