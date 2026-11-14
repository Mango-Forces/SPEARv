//! Private fixed-width shim signatures, never a CFITSIO struct.
use std::ffi::{c_char, c_int, c_void};
unsafe extern "C" {
    pub fn sv_init() -> c_int;
    pub fn sv_reentrant() -> c_int;
    pub fn sv_status(status: c_int, out: *mut c_char);
    pub fn sv_open(
        name: *const c_char,
        bytes: *mut c_void,
        len: usize,
        mode: c_int,
        out: *mut *mut c_void,
    ) -> c_int;
    pub fn sv_close(p: *mut c_void) -> c_int;
    pub fn sv_move(p: *mut c_void, hdu: c_int, kind: *mut c_int) -> c_int;
    pub fn sv_named(p: *mut c_void, name: *const c_char, version: c_int) -> c_int;
    pub fn sv_key_string(
        p: *mut c_void,
        key: *const c_char,
        out: *mut c_char,
        capacity: usize,
    ) -> c_int;
    pub fn sv_key_double(p: *mut c_void, key: *const c_char, out: *mut f64) -> c_int;
    pub fn sv_key_int(p: *mut c_void, key: *const c_char, out: *mut i64) -> c_int;
    pub fn sv_cards_count(p: *mut c_void, count: *mut c_int) -> c_int;
    pub fn sv_card(
        p: *mut c_void,
        index: c_int,
        name: *mut c_char,
        value: *mut c_char,
        comment: *mut c_char,
        raw: *mut c_char,
    ) -> c_int;
    pub fn sv_image_rank(p: *mut c_void, rank: *mut c_int) -> c_int;
    pub fn sv_image(
        p: *mut c_void,
        rank: c_int,
        axes: *mut i64,
        bitpix: *mut c_int,
        compressed: *mut c_int,
        quantized: *mut c_int,
        scale: *mut f64,
        zero: *mut f64,
    ) -> c_int;
    pub fn sv_pixels(
        p: *mut c_void,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
        mode: c_int,
        precision: c_int,
        out: *mut c_void,
        valid: *mut u8,
    ) -> c_int;
    pub fn sv_table(p: *mut c_void, cols: *mut c_int, rows: *mut i64) -> c_int;
    pub fn sv_column(
        p: *mut c_void,
        col: c_int,
        capacity: c_int,
        kind: *mut c_int,
        repeat: *mut i64,
        width: *mut i64,
        rank: *mut c_int,
        dims: *mut i64,
    ) -> c_int;
    pub fn sv_descriptor(p: *mut c_void, col: c_int, row: i64, count: *mut i64) -> c_int;
    pub fn sv_col_read(
        p: *mut c_void,
        col: c_int,
        row: i64,
        first: i64,
        count: i64,
        integer: c_int,
        out: *mut c_void,
        valid: *mut u8,
    ) -> c_int;
}
