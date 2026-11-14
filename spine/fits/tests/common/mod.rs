use spearv_fits::Limits;
use std::{
    ffi::{c_char, CString},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
unsafe extern "C" {
    fn sv_test_fixture(path: *const c_char) -> i32;
}
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spearv-fits")
}
pub fn fixture() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = root().join("test-fixtures");
    std::fs::create_dir_all(&dir).expect("private test directory");
    let path = dir.join(format!(
        "types-{}-{}.fits",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let cpath = CString::new(path.to_str().expect("UTF8 test path")).expect("test path");
    assert_eq!(
        unsafe { sv_test_fixture(cpath.as_ptr()) },
        0,
        "current library creates small types fixture"
    );
    path
}
pub fn limits() -> Limits {
    Limits {
        max_axes: 16,
        max_cards: 4096,
        max_elements: 65_536,
        max_column_elements: 65_536,
        page_bytes: 65_536,
        cache_bytes: 262_144,
        native_workspace_bytes: 8 * 1024 * 1024,
    }
}
pub fn window() -> spearv_fits::Window {
    spearv_fits::Window {
        x: 0,
        y: 0,
        width: 3,
        height: 2,
    }
}
