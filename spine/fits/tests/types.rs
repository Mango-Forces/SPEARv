mod common;
use spearv_fits::{
    run, Budget, Cancellation, ColumnType, Error, FloatMode, HduKind, Input, Window,
};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_native_types_nulls_shapes_and_retained_output_pins() {
    let budget = Budget::new(32 * 1024 * 1024);
    let token = Cancellation::default();
    let done = run(
        Input::Local(common::fixture()),
        budget.clone(),
        common::limits(),
        token.clone(),
        |fits| {
            assert!(fits.is_reentrant());
            assert_eq!(fits.move_to(1)?, HduKind::Image);
            assert_eq!(fits.image_info()?.axes(), &[]);
            fits.move_named("SIGNED", 0)?;
            assert_eq!(
                fits.read_flags(common::window())?.values(),
                &[u32::MAX, 1 << 31, 0, 1, i32::MAX as u32, u32::MAX - 1]
            );
            fits.move_named("UNSIGNED", 0)?;
            assert_eq!(
                fits.read_flags(common::window())?.values(),
                &[0, 1, 1 << 31, u32::MAX, 123, i32::MAX as u32]
            );
            fits.move_named("RICEBLOCK", 0)?;
            assert_eq!(fits.image_info()?.tile_axes(), Some(&[2u64, 2][..]));
            // Multi-row tiles without BLANK crash CFITSIO's cache: refused.
            assert!(matches!(
                fits.read_flags(common::window()),
                Err(Error::Unsupported(_))
            ));
            fits.move_named("OFFSET64", 0)?;
            assert!(matches!(
                fits.read_column_i64(1, 1, 1, 1),
                Err(Error::Unsupported(_))
            ));
            fits.move_named("RICEFLAGS", 0)?;
            assert!(fits.image_info()?.compressed);
            assert_eq!(
                fits.read_flags(common::window())?.values(),
                &[u32::MAX, 1 << 31, 0, 1, i32::MAX as u32, u32::MAX - 1]
            );
            fits.move_named("SCALED", 0)?;
            let scaled = fits.read_f32(common::window(), FloatMode::Automatic)?;
            assert_eq!(scaled.valid(), &[1, 1, 0, 1, 1, 1]);
            assert!(scaled.values()[2].is_nan());
            assert_eq!(
                [
                    scaled.values()[0],
                    scaled.values()[1],
                    scaled.values()[3],
                    scaled.values()[4],
                    scaled.values()[5]
                ],
                [12., 14., 4., 18., 20.]
            );
            assert!(matches!(
                fits.read_f32(common::window(), FloatMode::PreserveRaw),
                Err(Error::Unsupported(_))
            ));
            assert!(matches!(
                fits.read_flags(common::window()),
                Err(Error::Unsupported(_))
            ));
            fits.move_named("FLOAT", 0)?;
            assert_eq!(&*fits.key_string("BUNIT")?.expect("unit"), "MJy / sr");
            assert_eq!(
                fits.key_string("LONGTEXT")?
                    .expect("long native-decoded text")
                    .len(),
                180
            );
            assert_eq!(fits.key_string("MISSING")?.map(|s| s.to_string()), None);
            assert!(fits
                .header()?
                .cards()
                .iter()
                .any(|c| c.name == "BUNIT" && c.raw.contains("MJy / sr")));
            let raw = fits.read_f32(common::window(), FloatMode::Automatic)?;
            assert_eq!(
                raw.values().iter().map(|f| f.to_bits()).collect::<Vec<_>>(),
                vec![
                    1,
                    0x7fc00000,
                    0x7f800000,
                    0xff800000,
                    0x80000000,
                    2f32.to_bits()
                ]
            );
            assert_eq!(raw.valid(), &[1, 0, 0, 0, 1, 1]);
            let physical = fits.read_f32(common::window(), FloatMode::Physical)?;
            for (value, valid) in physical.values().iter().zip(physical.valid()) {
                if *valid == 0 {
                    assert!(value.is_nan());
                }
            }
            fits.move_named("DOUBLE", 0)?;
            assert_eq!(
                fits.read_f64(common::window(), FloatMode::Automatic)?
                    .values()
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                vec![
                    1,
                    0x7ff8000000000000,
                    0x7ff0000000000000,
                    0xfff0000000000000,
                    0x8000000000000000,
                    2f64.to_bits()
                ]
            );
            for name in ["LOSSLESS", "CWAVE", "CBAND"] {
                fits.move_named(name, 0)?;
                let info = fits.image_info()?;
                assert!(!info.quantized);
                assert_eq!(
                    fits.read_f32(common::window(), FloatMode::Automatic)?
                        .values()
                        .iter()
                        .map(|v| v.to_bits())
                        .collect::<Vec<_>>(),
                    vec![
                        1,
                        0x7fc00000,
                        0x7f800000,
                        0xff800000,
                        0x80000000,
                        2f32.to_bits()
                    ]
                );
                if name != "LOSSLESS" {
                    assert_eq!(
                        &*fits.key_string("BUNIT")?.expect("literal unit"),
                        "fixture-unit"
                    );
                }
            }
            fits.move_named("LOSSLESS64", 0)?;
            assert_eq!(
                fits.read_f64(common::window(), FloatMode::Automatic)?
                    .values()
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                vec![
                    1,
                    0x7ff8000000000000,
                    0x7ff0000000000000,
                    0xfff0000000000000,
                    0x8000000000000000,
                    2f64.to_bits()
                ]
            );
            fits.move_named("ONE", 0)?;
            assert_eq!(
                fits.read_f32(
                    Window {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 3
                    },
                    FloatMode::Automatic
                )?
                .values(),
                &[1., 2., 3.]
            );
            assert!(matches!(
                fits.read_f32(common::window(), FloatMode::Automatic),
                Err(Error::Bounds(_))
            ));
            fits.move_named("TABLE", 0)?;
            assert_eq!(fits.table_shape()?, (1, 1));
            let shape = fits.column_info(1)?;
            assert_eq!(shape.kind, ColumnType::I32);
            assert_eq!(shape.dimensions(), [2, 3]);
            assert_eq!(shape.repeat, 6);
            assert_eq!(&*fits.key_string("TUNIT1")?.expect("unit"), "retained-unit");
            let table = fits.read_column_f64(1, 1, 1, 6)?;
            assert_eq!(table.valid(), &[1, 1, 0, 1, 1, 1]);
            assert_eq!(
                [
                    table.values()[0],
                    table.values()[1],
                    table.values()[3],
                    table.values()[4],
                    table.values()[5]
                ],
                [12., 14., 18., 20., 22.]
            );
            assert!(table.values()[2].is_nan());
            assert_eq!(
                fits.read_column_i64(1, 1, 1, 6)?.values(),
                &[12, 14, 0, 18, 20, 22]
            );
            assert!(matches!(
                fits.read_column_f64(1, 2, 1, 1),
                Err(Error::Bounds(_))
            ));
            assert!(matches!(
                fits.read_column_f64(1, 1, 6, 2),
                Err(Error::Bounds(_))
            ));
            fits.move_named("VARIABLE", 0)?;
            assert!(fits.column_info(1)?.variable);
            assert_eq!(
                fits.read_column_i64(1, 1, 1, 6)?.values(),
                &[1, 2, -999, 4, 5, 6]
            );
            assert!(matches!(
                fits.read_column_i64(1, 1, 1, 7),
                Err(Error::Bounds(_))
            ));
            fits.move_named("REALCOL", 0)?;
            assert_eq!(fits.read_column_f64(1, 1, 1, 2)?.values(), &[1.7, -2.5]);
            assert!(matches!(
                fits.read_column_i64(1, 1, 1, 2),
                Err(Error::Unsupported(_))
            ));
            fits.move_named("HALFSCALE", 0)?;
            assert!(matches!(
                fits.read_column_i64(1, 1, 1, 6),
                Err(Error::Unsupported(_))
            ));
            fits.move_named("BADTDIM", 0)?;
            assert!(matches!(
                fits.column_info(1),
                Err(Error::Native { status: 263, .. }) | Err(Error::Bounds(_))
            ));
            fits.move_named("QUANTIZED", 0)?;
            assert!(fits.image_info()?.quantized);
            assert!(matches!(
                fits.read_f32(common::window(), FloatMode::PreserveRaw),
                Err(Error::Unsupported(_))
            ));
            let quantized = fits.read_f32(common::window(), FloatMode::Automatic)?;
            assert_eq!(quantized.valid(), &[1, 0, 1, 1, 1, 1]);
            assert!(quantized.values()[1].is_nan());
            Ok(raw)
        },
    )
    .await
    .expect("current native types");
    token.wait_idle().await;
    assert_eq!(token.active_workers(), 0);
    let pinned = Arc::new(done.value);
    assert_eq!(budget.used_bytes() as usize, pinned.charged_bytes());
    let reader = pinned.clone();
    drop(pinned);
    assert_eq!(
        budget.used_bytes() as usize,
        reader.charged_bytes(),
        "reader pin cannot free output charge"
    );
    assert_eq!(
        reader.values()[0].to_bits(),
        1,
        "raw owned output outlives native close"
    );
    drop(reader);
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_memory_prefix_short_eof_and_preallocation_bounds() {
    let path = common::fixture();
    let bytes = std::fs::read(&path).expect("small library fixture");
    let budget = Budget::new(32 * 1024 * 1024);
    let result = run(
        Input::Memory(bytes.clone()),
        budget.clone(),
        common::limits(),
        Cancellation::default(),
        |fits| {
            fits.move_named("SIGNED", 0)?;
            fits.read_flags(common::window())
        },
    )
    .await
    .expect("owned read-only memory");
    assert_eq!(result.value.values()[0], u32::MAX);
    drop(result);
    assert_eq!(budget.used_bytes(), 0);
    for len in [0, 80, 2880, 5760] {
        let prefix = bytes[..len].to_vec();
        let result = run(
            Input::Prefix(prefix),
            budget.clone(),
            common::limits(),
            Cancellation::default(),
            |fits| {
                fits.move_named("FLOAT", 0)?;
                fits.header()
            },
        )
        .await;
        assert!(
            result.is_err(),
            "short prefix {len} is actual EOF, not zero-filled success"
        );
        assert_eq!(budget.used_bytes(), 0);
    }
    let result = run(
        Input::Prefix(bytes[..bytes.len().min(65_536)].to_vec()),
        budget.clone(),
        common::limits(),
        Cancellation::default(),
        |fits| {
            fits.move_named("FLOAT", 0)?;
            assert!(matches!(
                fits.read_f32(common::window(), FloatMode::Automatic),
                Err(Error::Unsupported(_))
            ));
            fits.header()
        },
    )
    .await
    .expect("available prefix only");
    assert!(!result.value.cards().is_empty());
    drop(result);
    let mut limits = common::limits();
    limits.max_elements = 2;
    let result = run(
        Input::Local(path.clone()),
        budget.clone(),
        limits,
        Cancellation::default(),
        |fits| {
            fits.move_named("FLOAT", 0)?;
            fits.read_f32(common::window(), FloatMode::Automatic)
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Bounds(_))));
    let mut limits = common::limits();
    limits.max_cards = 1;
    let result = run(
        Input::Local(path),
        budget.clone(),
        limits,
        Cancellation::default(),
        |fits| fits.header(),
    )
    .await;
    assert!(matches!(result, Err(Error::Bounds(_))));
    assert_eq!(budget.used_bytes(), 0);
    let too_small = Budget::new(1);
    assert!(matches!(
        run(
            Input::Memory(bytes),
            too_small.clone(),
            common::limits(),
            Cancellation::default(),
            |_| Ok(())
        )
        .await,
        Err(Error::Budget { .. })
    ));
    assert_eq!(too_small.used_bytes(), 0);
    let mut limits = common::limits();
    limits.native_workspace_bytes = 40 * 2880;
    let result = run(
        Input::Memory(std::fs::read(common::fixture()).expect("small fixture")),
        budget.clone(),
        limits,
        Cancellation::default(),
        |fits| {
            fits.move_named("RICEFLAGS", 0)?;
            fits.read_flags(common::window())
        },
    )
    .await;
    assert!(
        matches!(result, Err(Error::Budget { .. })),
        "declared native tile scratch is refused before output allocation"
    );
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_cached_output_blocks_new_peak_until_last_reader_pin_is_evicted() {
    let limits = common::limits();
    let reservation = limits.native_workspace_bytes
        + limits.cache_bytes
        + 2 * limits.page_bytes
        + (limits.cache_bytes / limits.page_bytes + 1) * 64;
    // One two-axis metadata allocation (96 charged bytes) + six f32/valid (30).
    let budget = Budget::new((reservation + 96 + 30) as u64);
    let path = common::fixture();
    let first = run(
        Input::Local(path.clone()),
        budget.clone(),
        limits.clone(),
        Cancellation::default(),
        |fits| {
            fits.move_named("FLOAT", 0)?;
            fits.read_f32(common::window(), FloatMode::Automatic)
        },
    )
    .await
    .expect("first admitted scope");
    let cache = Arc::new(first.value);
    let reader = cache.clone();
    drop(cache);
    assert_eq!(
        budget.used_bytes(),
        30,
        "eviction does not free an immutable reader pin"
    );
    let rejected = run(
        Input::Local(path.clone()),
        budget.clone(),
        limits.clone(),
        Cancellation::default(),
        |fits| {
            fits.move_named("FLOAT", 0)?;
            fits.read_f32(common::window(), FloatMode::Automatic)
        },
    )
    .await;
    assert!(
        matches!(rejected, Err(Error::Budget { .. })),
        "new peak cannot ignore finished cached output"
    );
    assert_eq!(budget.used_bytes(), 30);
    drop(reader);
    assert_eq!(budget.used_bytes(), 0);
    let admitted = run(
        Input::Local(path),
        budget.clone(),
        limits,
        Cancellation::default(),
        |fits| {
            fits.move_named("FLOAT", 0)?;
            fits.read_f32(common::window(), FloatMode::Automatic)
        },
    )
    .await
    .expect("last reader drop makes room");
    drop(admitted);
    assert_eq!(budget.used_bytes(), 0);
}
