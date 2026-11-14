//! FITS decoding through a private, static CFITSIO 4.7.0.
//!
//! All decoding is CFITSIO 4.7.0. An exclusive, non-Send/non-Sync [`Fits`]
//! exists only inside an admitted blocking operation. Published buffers own
//! their byte charges independently of the worker.
mod budget;
mod ffi;
pub mod source;

pub use budget::Budget;
use budget::Charge;
pub use source::{Cancellation, IoStats, ReadAt, ReadTrace};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    marker::PhantomData,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, OnceLock},
};

pub type Result<T> = std::result::Result<T, Error>;
/// Native and transport failures remain distinct. Transport messages must not
/// contain URLs, authorization headers or credentials.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FITS operation cancelled")]
    Cancelled,
    #[error("FITS bounds: {0}")]
    Bounds(String),
    #[error("FITS representation is not supported by this operation: {0}")]
    Unsupported(String),
    #[error("native CFITSIO operation failed: {message} ({status})")]
    Native { status: i32, message: String },
    #[error("short exact FITS read")]
    ShortRead,
    #[error("FITS content identity changed")]
    ContentChanged,
    #[error("malformed range response: {0}")]
    MalformedRange(String),
    #[error("transient range failure: {0}")]
    Transient(String),
    #[error("range deadline expired")]
    Deadline,
    #[error("Rust panic caught at the native callback boundary")]
    CallbackPanic,
    #[error("FITS blocking worker failed: {0}")]
    Worker(String),
    #[error("FITS needs {requested} bytes but only {available} admitted bytes remain")]
    Budget { requested: u64, available: u64 },
    #[error("FITS allocation failed")]
    Allocation,
    #[error("FITS runtime: {0}")]
    Runtime(String),
}
impl Error {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::Deadline)
    }
}
/// Inputs never enter CFITSIO filename filters. Memory owns exactly these bytes.
pub enum Input {
    Local(PathBuf),
    Memory(Vec<u8>),
    /// Header-only operation. At most 65,536 available bytes, never a fake file.
    Prefix(Vec<u8>),
    Random {
        size: u64,
        reader: Arc<dyn ReadAt>,
    },
}
/// Caller-selected allocation bounds, not mission-specific probe restrictions.
/// Native workspace is an explicit estimate, not interception of CFITSIO malloc.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_axes: usize,
    pub max_cards: usize,
    pub max_elements: usize,
    pub max_column_elements: usize,
    pub page_bytes: usize,
    pub cache_bytes: usize,
    pub native_workspace_bytes: usize,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        if self.max_axes == 0
            || self.max_axes > 999
            || self.max_cards > i32::MAX as usize
            || self.max_elements == 0
            || self.max_column_elements == 0
            || self.page_bytes == 0
            || self.cache_bytes < self.page_bytes
            || self.native_workspace_bytes < 40 * 2880
        {
            return Err(Error::Bounds(
                "invalid adapter limits or native I/O buffer allowance".into(),
            ));
        }
        Ok(())
    }
}
/// Immutable published physical values and explicit valid/noData.
/// Invalid floats are deterministic NaN except the raw IEEE preservation mode,
/// which retains their original bits; invalid integer slots are zero + valid=0.
/// Arc pins are safe: this object, not the worker, owns its charge.
#[derive(Debug)]
pub struct Plane<T> {
    values: Vec<T>,
    valid: Vec<u8>,
    _charge: Charge,
}
impl<T> Plane<T> {
    pub fn values(&self) -> &[T] {
        &self.values
    }
    pub fn valid(&self) -> &[u8] {
        &self.valid
    }
    pub fn charged_bytes(&self) -> usize {
        self.values.capacity() * std::mem::size_of::<T>() + self.valid.capacity()
    }
}
#[derive(Debug)]
pub struct Text {
    value: String,
    _charge: Charge,
}
impl std::ops::Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        &self.value
    }
}
#[derive(Debug)]
pub struct Card {
    pub name: String,
    pub value: String,
    pub comment: String,
    pub raw: String,
}
#[derive(Debug)]
pub struct Header {
    cards: Vec<Card>,
    _charge: Charge,
}
impl Header {
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HduKind {
    Image,
    AsciiTable,
    BinaryTable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelType {
    U8,
    I16,
    I32,
    I64,
    F32,
    F64,
}
#[derive(Debug)]
pub struct ImageInfo {
    axes: Vec<u64>,
    pub pixel_type: PixelType,
    pub compressed: bool,
    pub quantized: bool,
    pub scale: f64,
    pub zero: f64,
    /// Library-decoded compressed tile dimensions; absent for ordinary images.
    tile_axes: Option<Vec<u64>>,
    _charge: Charge,
}
impl ImageInfo {
    pub fn axes(&self) -> &[u64] {
        &self.axes
    }
    pub fn tile_axes(&self) -> Option<&[u64]> {
        self.tile_axes.as_deref()
    }
}
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub x: u64,
    pub y: u64,
    pub width: u64,
    pub height: u64,
}
/// Automatic preserves unscaled/unquantized matching float precision.
/// Physical requests explicit native null/scaling conversion.
#[derive(Debug, Clone, Copy)]
pub enum FloatMode {
    Automatic,
    Physical,
    PreserveRaw,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bit,
    U8,
    I8,
    Logical,
    String,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Complex32,
    Complex64,
}
impl ColumnType {
    fn numeric(self) -> bool {
        !matches!(
            self,
            Self::Bit | Self::String | Self::Complex32 | Self::Complex64
        )
    }
}
#[derive(Debug)]
pub struct ColumnInfo {
    pub column: u32,
    pub kind: ColumnType,
    pub variable: bool,
    pub repeat: u64,
    pub width: u64,
    pub rows: u64,
    dimensions: Vec<u64>,
    _charge: Charge,
}
impl ColumnInfo {
    pub fn dimensions(&self) -> &[u64] {
        &self.dimensions
    }
}
#[derive(Debug)]
pub struct Completed<T> {
    pub value: T,
    pub io: IoStats,
}

/// Exclusive operation scope. Its destructor closes before backing/cache/
/// admission release; its native handle is deliberately neither Send nor Sync.
pub struct Fits {
    pointer: Option<NonNull<c_void>>,
    _memory: Vec<u8>,
    context: Option<Arc<source::Context>>,
    cancel: Cancellation,
    budget: Budget,
    limits: Limits,
    prefix: bool,
    _charge: Charge,
    _exclusive: PhantomData<Rc<()>>,
}
struct CancelOnDrop(Option<Cancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}
struct RegistryGuard(Option<Arc<source::Context>>);
impl Drop for RegistryGuard {
    fn drop(&mut self) {
        if let Some(context) = &self.0 {
            context.finish();
        }
    }
}

/// Capture a multithread Tokio runtime before entering blocking CFITSIO.
/// Drop of this future signals cancellation but does not release worker
/// ownership or promise to kill a native decompression already in progress.
pub async fn run<T: Send + 'static>(
    input: Input,
    budget: Budget,
    limits: Limits,
    cancel: Cancellation,
    operation: impl FnOnce(&mut Fits) -> Result<T> + Send + 'static,
) -> Result<Completed<T>> {
    limits.validate()?;
    cancel.check()?;
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| Error::Runtime("Tokio runtime required".into()))?;
    if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        return Err(Error::Runtime(
            "multithread runtime required to drive async ranges during blocking work".into(),
        ));
    }
    let backing_bytes = match &input {
        Input::Memory(bytes) => bytes.capacity(),
        Input::Prefix(bytes) => {
            if bytes.len() > 65_536 {
                return Err(Error::Bounds(
                    "header prefix exceeds 65,536 available bytes".into(),
                ));
            }
            bytes.capacity()
        }
        Input::Random { size, .. } if *size == 0 || *size > i64::MAX as u64 => {
            return Err(Error::Bounds(
                "logical object size is outside signed 64-bit CFITSIO".into(),
            ))
        }
        _ => 0,
    };
    let bytes = limits
        .native_workspace_bytes
        .checked_add(backing_bytes)
        .and_then(|n| n.checked_add(limits.cache_bytes))
        .and_then(|n| n.checked_add(limits.page_bytes.checked_mul(2)?))
        .and_then(|n| n.checked_add((limits.cache_bytes / limits.page_bytes + 1).checked_mul(64)?))
        .ok_or_else(|| Error::Bounds("admission byte overflow".into()))?;
    let charge = budget.charge(bytes)?;
    let mut guard = CancelOnDrop(Some(cancel.clone()));
    let worker_cancel = cancel.clone();
    let worker_guard = cancel.track()?;
    let result = tokio::task::spawn_blocking(move || {
        let _worker = worker_guard;
        let mut fits = Fits::open(input, budget, limits, worker_cancel, runtime, charge)?;
        let value = operation(&mut fits);
        // Close even after operation failure; close errors must not mask first cause.
        let close = fits.close();
        let io = fits
            .context
            .as_ref()
            .map_or(IoStats::default(), |ctx| ctx.stats());
        let value = value?;
        close?;
        fits.cancel.check()?;
        Ok(Completed { value, io })
    })
    .await
    .map_err(|error| {
        Error::Worker(if error.is_panic() {
            "operation panicked".into()
        } else {
            "worker was not started".into()
        })
    })?;
    cancel.check()?;
    guard.0.take();
    result
}
fn initialized() -> Result<()> {
    static INIT: OnceLock<i32> = OnceLock::new();
    let status = *INIT.get_or_init(|| unsafe { ffi::sv_init() });
    native_result(status)
}
fn native_result(status: i32) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    let mut message = [0 as c_char; 81];
    unsafe {
        ffi::sv_status(status, message.as_mut_ptr());
    }
    Err(Error::Native {
        status,
        message: c_string(&message)
            .unwrap_or_else(|_| "native status message encoding is invalid".into()),
    })
}
fn c_string(bytes: &[c_char]) -> Result<String> {
    // All buffers are zero-initialized and CFITSIO writes bounded NUL-terminated strings.
    unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| Error::Unsupported("native header text is not UTF-8".into()))
}
fn name(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| Error::Bounds("NUL in native key or HDU name".into()))
}
fn allocated<T: Clone>(len: usize, initial: T) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(len)
        .map_err(|_| Error::Allocation)?;
    values.resize(len, initial);
    Ok(values)
}
impl Fits {
    fn open(
        input: Input,
        budget: Budget,
        limits: Limits,
        cancel: Cancellation,
        runtime: tokio::runtime::Handle,
        charge: Charge,
    ) -> Result<Self> {
        initialized()?;
        cancel.check()?;
        let prefix = matches!(&input, Input::Prefix(_));
        let (path, mut memory, context, mode) = match input {
            Input::Local(path) => {
                let path = path
                    .canonicalize()
                    .map_err(|_| Error::Bounds("local FITS path is unavailable".into()))?;
                (
                    CString::new(path.as_os_str().as_bytes())
                        .map_err(|_| Error::Bounds("NUL in local path".into()))?,
                    Vec::new(),
                    None,
                    0,
                )
            }
            Input::Memory(bytes) | Input::Prefix(bytes) => (name("owned-memory")?, bytes, None, 2),
            Input::Random { size, reader } => {
                let context = source::Context::new(
                    size,
                    reader,
                    runtime,
                    cancel.clone(),
                    limits.page_bytes,
                    limits.cache_bytes,
                )?;
                (
                    name(&format!("spearvfits://{}", context.id))?,
                    Vec::new(),
                    Some(context),
                    1,
                )
            }
        };
        let mut registry = RegistryGuard(context.clone());
        let mut pointer = std::ptr::null_mut();
        let status = unsafe {
            ffi::sv_open(
                path.as_ptr(),
                memory.as_mut_ptr().cast(),
                memory.len(),
                mode,
                &mut pointer,
            )
        };
        if status != 0 {
            if let Some(cause) = context.as_ref().and_then(|ctx| ctx.take_cause()) {
                return Err(cause);
            }
            native_result(status)?;
        }
        let pointer = NonNull::new(pointer)
            .ok_or_else(|| Error::Runtime("CFITSIO returned no handle".into()))?;
        registry.0.take();
        Ok(Self {
            pointer: Some(pointer),
            _memory: memory,
            context,
            cancel,
            budget,
            limits,
            prefix,
            _charge: charge,
            _exclusive: PhantomData,
        })
    }
    fn pointer(&self) -> *mut c_void {
        self.pointer
            .expect("handle only used before close")
            .as_ptr()
    }
    fn status(&self, status: i32) -> Result<()> {
        if status != 0 {
            if let Some(cause) = self.context.as_ref().and_then(|ctx| ctx.take_cause()) {
                return Err(cause);
            }
        }
        native_result(status)?;
        self.cancel.check()
    }
    fn close(&mut self) -> Result<()> {
        let status = match self.pointer.take() {
            Some(pointer) => unsafe { ffi::sv_close(pointer.as_ptr()) },
            None => 0,
        };
        if let Some(context) = &self.context {
            context.finish();
            if let Some(cause) = context.take_cause() {
                return Err(cause);
            }
        }
        native_result(status)
    }
    pub fn move_to(&mut self, hdu: u32) -> Result<HduKind> {
        self.cancel.check()?;
        if hdu == 0 || hdu > i32::MAX as u32 {
            return Err(Error::Bounds("HDU number is one-based".into()));
        }
        let mut kind = 0;
        self.status(unsafe { ffi::sv_move(self.pointer(), hdu as i32, &mut kind) })?;
        match kind {
            0 => Ok(HduKind::Image),
            1 => Ok(HduKind::AsciiTable),
            2 => Ok(HduKind::BinaryTable),
            _ => Err(Error::Unsupported("unknown HDU type".into())),
        }
    }
    pub fn move_named(&mut self, extension: &str, version: u32) -> Result<()> {
        self.cancel.check()?;
        if version > i32::MAX as u32 {
            return Err(Error::Bounds("extension version overflow".into()));
        }
        self.status(unsafe {
            ffi::sv_named(self.pointer(), name(extension)?.as_ptr(), version as i32)
        })
    }
    pub fn key_string(&self, key: &str) -> Result<Option<Text>> {
        self.cancel.check()?;
        let mut cards = 0;
        self.status(unsafe { ffi::sv_cards_count(self.pointer(), &mut cards) })?;
        if cards < 0 || cards as usize > self.limits.max_cards {
            return Err(Error::Bounds("header card ceiling exceeded".into()));
        }
        // Every continuation card is at most 80 bytes. Charge both the native
        // long-string allocation and the bridge buffer before decoding it.
        let capacity = (cards as usize)
            .checked_mul(80)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Error::Bounds("header string capacity overflow".into()))?;
        let _temporary = self.budget.charge(
            capacity
                .checked_mul(2)
                .ok_or_else(|| Error::Bounds("header string scratch overflow".into()))?,
        )?;
        let mut out = allocated(capacity, 0 as c_char)?;
        let status = unsafe {
            ffi::sv_key_string(
                self.pointer(),
                name(key)?.as_ptr(),
                out.as_mut_ptr(),
                capacity,
            )
        };
        if status == 202 {
            return Ok(None);
        }
        self.status(status)?;
        let length = unsafe { CStr::from_ptr(out.as_ptr()) }.to_bytes().len();
        let charge = self.budget.charge(length)?;
        Ok(Some(Text {
            value: c_string(&out)?,
            _charge: charge,
        }))
    }
    pub fn key_f64(&self, key: &str) -> Result<Option<f64>> {
        self.cancel.check()?;
        let mut value = 0.;
        let status = unsafe { ffi::sv_key_double(self.pointer(), name(key)?.as_ptr(), &mut value) };
        if status == 202 {
            return Ok(None);
        }
        self.status(status)?;
        Ok(Some(value))
    }
    pub fn key_i64(&self, key: &str) -> Result<Option<i64>> {
        self.cancel.check()?;
        let mut value = 0;
        let status = unsafe { ffi::sv_key_int(self.pointer(), name(key)?.as_ptr(), &mut value) };
        if status == 202 {
            return Ok(None);
        }
        self.status(status)?;
        Ok(Some(value))
    }
    pub fn header(&self) -> Result<Header> {
        self.cancel.check()?;
        let mut count = 0;
        self.status(unsafe { ffi::sv_cards_count(self.pointer(), &mut count) })?;
        if count < 0 || count as usize > self.limits.max_cards {
            return Err(Error::Bounds("header card ceiling exceeded".into()));
        }
        let bytes = (count as usize)
            .checked_mul(std::mem::size_of::<Card>() + 4 * 81)
            .ok_or_else(|| Error::Bounds("header allocation overflow".into()))?;
        let charge = self.budget.charge(bytes)?;
        let mut cards = Vec::new();
        cards
            .try_reserve_exact(count as usize)
            .map_err(|_| Error::Allocation)?;
        for index in 1..=count {
            let (mut key, mut value, mut comment, mut raw) = (
                [0 as c_char; 81],
                [0 as c_char; 81],
                [0 as c_char; 81],
                [0 as c_char; 81],
            );
            self.status(unsafe {
                ffi::sv_card(
                    self.pointer(),
                    index,
                    key.as_mut_ptr(),
                    value.as_mut_ptr(),
                    comment.as_mut_ptr(),
                    raw.as_mut_ptr(),
                )
            })?;
            cards.push(Card {
                name: c_string(&key)?,
                value: c_string(&value)?,
                comment: c_string(&comment)?,
                raw: c_string(&raw)?,
            });
        }
        Ok(Header {
            cards,
            _charge: charge,
        })
    }
    pub fn image_info(&self) -> Result<ImageInfo> {
        self.cancel.check()?;
        let mut rank = 0;
        self.status(unsafe { ffi::sv_image_rank(self.pointer(), &mut rank) })?;
        if rank < 0 || rank as usize > self.limits.max_axes {
            return Err(Error::Bounds("image axis ceiling exceeded".into()));
        }
        let charge = self.budget.charge(rank as usize * 6 * 8)?;
        let mut axes = allocated(rank as usize, 0i64)?;
        let (mut bitpix, mut compressed, mut quantized, mut scale, mut zero) = (0, 0, 0, 1., 0.);
        self.status(unsafe {
            ffi::sv_image(
                self.pointer(),
                rank,
                axes.as_mut_ptr(),
                &mut bitpix,
                &mut compressed,
                &mut quantized,
                &mut scale,
                &mut zero,
            )
        })?;
        if axes.iter().any(|axis| *axis < 0) || !scale.is_finite() || !zero.is_finite() {
            return Err(Error::Bounds("invalid axes or image scaling".into()));
        }
        let pixel_type = match bitpix {
            8 => PixelType::U8,
            16 => PixelType::I16,
            32 => PixelType::I32,
            64 => PixelType::I64,
            -32 => PixelType::F32,
            -64 => PixelType::F64,
            _ => return Err(Error::Unsupported("unknown native image type".into())),
        };
        let tile_axes = if compressed != 0 {
            let mut tiles = Vec::new();
            tiles
                .try_reserve_exact(rank as usize)
                .map_err(|_| Error::Allocation)?;
            for index in 0..rank as usize {
                // Same optional-key defaults as pinned imcomp_get_compressed_image_par;
                // only metadata for admission, never a compressed raster decoder.
                let tile = self
                    .key_i64(&format!("ZTILE{}", index + 1))?
                    .unwrap_or(if index == 0 { axes[0] } else { 1 });
                if tile <= 0 {
                    return Err(Error::Bounds(
                        "compressed tile dimension must be positive".into(),
                    ));
                }
                tiles.push(tile as u64);
            }
            Some(tiles)
        } else {
            None
        };
        Ok(ImageInfo {
            axes: axes.into_iter().map(|n| n as u64).collect(),
            pixel_type,
            compressed: compressed != 0,
            quantized: quantized != 0,
            scale,
            zero,
            tile_axes,
            _charge: charge,
        })
    }
    fn window(&self, window: Window) -> Result<(ImageInfo, usize)> {
        self.cancel.check()?;
        if self.prefix {
            return Err(Error::Unsupported("prefix handle is header-only".into()));
        }
        let image = self.image_info()?;
        if image.axes.len() != 2
            || window.width == 0
            || window.height == 0
            || window
                .x
                .checked_add(window.width)
                .filter(|n| *n <= image.axes[0])
                .is_none()
            || window
                .y
                .checked_add(window.height)
                .filter(|n| *n <= image.axes[1])
                .is_none()
        {
            return Err(Error::Bounds(
                "rectangle is outside a two-dimensional image".into(),
            ));
        }
        let elements = window
            .width
            .checked_mul(window.height)
            .filter(|n| *n <= self.limits.max_elements as u64 && *n <= i64::MAX as u64)
            .ok_or_else(|| Error::Bounds("image element ceiling exceeded".into()))?;
        if let Some(tiles) = &image.tile_axes {
            // Declared-workspace gate, not a native malloc bound. CFITSIO keeps
            // one decoded tile (up to 8 B/px) plus its null array per tile
            // column while the file is open (imcompress.c ntilebins cache),
            // in addition to the transient scratch for the tile being decoded.
            let tile = tiles.iter().try_fold(1u64, |n, dim| n.checked_mul(*dim));
            let columns = tiles
                .first()
                .filter(|dim| **dim > 0)
                .map(|dim| image.axes[0].div_ceil(*dim));
            let scratch = tile
                .zip(columns)
                .and_then(|(tile, columns)| {
                    tile.checked_mul(9)?
                        .checked_mul(columns)?
                        .checked_add(tile.checked_mul(32)?)
                })
                .and_then(|n| n.checked_add(40 * 2880))
                .ok_or_else(|| Error::Bounds("compressed tile scratch overflow".into()))?;
            if scratch > self.limits.native_workspace_bytes as u64 {
                return Err(Error::Budget {
                    requested: scratch,
                    available: self.limits.native_workspace_bytes as u64,
                });
            }
        }
        Ok((image, elements as usize))
    }
    fn plane<T: Clone>(&self, len: usize, initial: T) -> Result<Plane<T>> {
        let bytes = len
            .checked_mul(std::mem::size_of::<T>() + 1)
            .ok_or_else(|| Error::Bounds("output byte overflow".into()))?;
        let charge = self.budget.charge(bytes)?;
        Ok(Plane {
            values: allocated(len, initial)?,
            valid: allocated(len, 0)?,
            _charge: charge,
        })
    }
    fn float_mode(&self, image: &ImageInfo, mode: FloatMode, precision: i32) -> Result<i32> {
        let raw = !image.quantized
            && image.scale == 1.
            && image.zero == 0.
            && image.pixel_type
                == if precision == 32 {
                    PixelType::F32
                } else {
                    PixelType::F64
                };
        match mode {
            FloatMode::PreserveRaw if !raw => Err(Error::Unsupported(
                "raw float requires matching, unscaled, unquantized IEEE image".into(),
            )),
            FloatMode::PreserveRaw => Ok(0),
            FloatMode::Automatic if raw => Ok(0),
            _ => Ok(1),
        }
    }
    pub fn read_f32(&self, window: Window, mode: FloatMode) -> Result<Plane<f32>> {
        let (image, len) = self.window(window)?;
        let mode = self.float_mode(&image, mode, 32)?;
        if mode != 0 {
            full_row_tiles(&image)?;
        }
        let mut output = self.plane(len, f32::NAN)?;
        self.status(unsafe {
            ffi::sv_pixels(
                self.pointer(),
                window.x as i64,
                window.y as i64,
                window.width as i64,
                window.height as i64,
                mode,
                32,
                output.values.as_mut_ptr().cast(),
                output.valid.as_mut_ptr(),
            )
        })?;
        if mode == 0 {
            for (value, valid) in output.values.iter().zip(&mut output.valid) {
                *valid = u8::from(value.is_finite());
            }
        }
        Ok(output)
    }
    pub fn read_f64(&self, window: Window, mode: FloatMode) -> Result<Plane<f64>> {
        let (image, len) = self.window(window)?;
        let mode = self.float_mode(&image, mode, 64)?;
        if mode != 0 {
            full_row_tiles(&image)?;
        }
        let mut output = self.plane(len, f64::NAN)?;
        self.status(unsafe {
            ffi::sv_pixels(
                self.pointer(),
                window.x as i64,
                window.y as i64,
                window.width as i64,
                window.height as i64,
                mode,
                64,
                output.values.as_mut_ptr().cast(),
                output.valid.as_mut_ptr(),
            )
        })?;
        if mode == 0 {
            for (value, valid) in output.values.iter().zip(&mut output.valid) {
                *valid = u8::from(value.is_finite());
            }
        }
        Ok(output)
    }
    pub fn read_flags(&self, window: Window) -> Result<Plane<u32>> {
        let (image, len) = self.window(window)?;
        if image.pixel_type != PixelType::I32
            || image.scale != 1.
            || (image.zero != 0. && image.zero != 2147483648.)
        {
            return Err(Error::Unsupported(
                "FLAGS require signed 32-bit bits or unsigned BZERO=2147483648".into(),
            ));
        }
        full_row_tiles(&image)?;
        let mut output = self.plane(len, 0u32)?;
        self.status(unsafe {
            ffi::sv_pixels(
                self.pointer(),
                window.x as i64,
                window.y as i64,
                window.width as i64,
                window.height as i64,
                if image.zero == 0. { 2 } else { 3 },
                32,
                output.values.as_mut_ptr().cast(),
                output.valid.as_mut_ptr(),
            )
        })?;
        Ok(output)
    }
    pub fn table_shape(&self) -> Result<(u32, u64)> {
        self.cancel.check()?;
        let (mut cols, mut rows) = (0, 0);
        self.status(unsafe { ffi::sv_table(self.pointer(), &mut cols, &mut rows) })?;
        if cols < 0 || rows < 0 {
            return Err(Error::Bounds("negative table shape".into()));
        }
        Ok((cols as u32, rows as u64))
    }
    pub fn column_info(&self, column: u32) -> Result<ColumnInfo> {
        let (columns, rows) = self.table_shape()?;
        if column == 0 || column > columns {
            return Err(Error::Bounds("column number is outside table".into()));
        }
        let charge = self.budget.charge(self.limits.max_axes * 3 * 8)?;
        let mut dims = allocated(self.limits.max_axes, 0i64)?;
        let (mut kind, mut repeat, mut width, mut rank) = (0, 0, 0, 0);
        self.status(unsafe {
            ffi::sv_column(
                self.pointer(),
                column as i32,
                self.limits.max_axes as i32,
                &mut kind,
                &mut repeat,
                &mut width,
                &mut rank,
                dims.as_mut_ptr(),
            )
        })?;
        if repeat < 0
            || width < 0
            || rank < 0
            || rank as usize > self.limits.max_axes
            || dims[..rank as usize].iter().any(|n| *n < 0)
        {
            return Err(Error::Bounds("invalid native column shape".into()));
        }
        dims.truncate(rank as usize);
        if kind >= 0 {
            let product = dims
                .iter()
                .try_fold(1u64, |n, dim| n.checked_mul(*dim as u64));
            if product != Some(repeat as u64) {
                return Err(Error::Bounds("TDIM does not match repeat".into()));
            }
        }
        let typed = match kind.unsigned_abs() {
            1 => ColumnType::Bit,
            11 => ColumnType::U8,
            12 => ColumnType::I8,
            14 => ColumnType::Logical,
            16 => ColumnType::String,
            20 => ColumnType::U16,
            21 => ColumnType::I16,
            30 | 40 => ColumnType::U32,
            31 | 41 => ColumnType::I32,
            80 => ColumnType::U64,
            81 => ColumnType::I64,
            42 => ColumnType::F32,
            82 => ColumnType::F64,
            83 => ColumnType::Complex32,
            163 => ColumnType::Complex64,
            _ => return Err(Error::Unsupported("unknown native column type".into())),
        };
        Ok(ColumnInfo {
            column,
            kind: typed,
            variable: kind < 0,
            repeat: repeat as u64,
            width: width as u64,
            rows,
            dimensions: dims.into_iter().map(|n| n as u64).collect(),
            _charge: charge,
        })
    }
    fn column_bounds(&self, column: u32, row: u64, first: u64, count: usize) -> Result<ColumnInfo> {
        self.cancel.check()?;
        if self.prefix {
            return Err(Error::Unsupported("prefix handle is header-only".into()));
        }
        let info = self.column_info(column)?;
        if !info.kind.numeric() {
            return Err(Error::Unsupported("column is not real numeric".into()));
        }
        if row == 0
            || row > info.rows
            || row > i64::MAX as u64
            || first == 0
            || first > i64::MAX as u64
            || count == 0
            || count > self.limits.max_column_elements
            || count > i64::MAX as usize
        {
            return Err(Error::Bounds("table row/element ceiling exceeded".into()));
        }
        let repeat = if info.variable {
            let mut repeat = 0;
            self.status(unsafe {
                ffi::sv_descriptor(self.pointer(), column as i32, row as i64, &mut repeat)
            })?;
            if repeat < 0 {
                return Err(Error::Bounds("negative variable column length".into()));
            }
            repeat as u64
        } else {
            info.repeat
        };
        if first
            .checked_sub(1)
            .and_then(|n| n.checked_add(count as u64))
            .filter(|n| *n <= repeat)
            .is_none()
        {
            return Err(Error::Bounds(
                "column read crosses row/vector bounds".into(),
            ));
        }
        Ok(info)
    }
    pub fn read_column_f64(
        &self,
        column: u32,
        row: u64,
        first: u64,
        count: usize,
    ) -> Result<Plane<f64>> {
        self.column_bounds(column, row, first, count)?;
        let mut output = self.plane(count, f64::NAN)?;
        self.status(unsafe {
            ffi::sv_col_read(
                self.pointer(),
                column as i32,
                row as i64,
                first as i64,
                count as i64,
                0,
                output.values.as_mut_ptr().cast(),
                output.valid.as_mut_ptr(),
            )
        })?;
        Ok(output)
    }
    pub fn read_column_i64(
        &self,
        column: u32,
        row: u64,
        first: u64,
        count: usize,
    ) -> Result<Plane<i64>> {
        let info = self.column_bounds(column, row, first, count)?;
        // Integer output is exact only for integer storage with an integral
        // affine scale; CFITSIO would otherwise truncate and mark it valid.
        // It scales through f64 (getcolj.c), so a scaled 64-bit column or a
        // scaled value beyond 2^53 would round silently.
        let bits = match info.kind {
            ColumnType::U8 | ColumnType::I8 => 8,
            ColumnType::U16 | ColumnType::I16 => 16,
            ColumnType::U32 | ColumnType::I32 => 32,
            ColumnType::U64 | ColumnType::I64 => 64,
            _ => return Err(Error::Unsupported("column is not integer-typed".into())),
        };
        let scale = self.key_f64(&format!("TSCAL{column}"))?.unwrap_or(1.);
        let zero = self.key_f64(&format!("TZERO{column}"))?.unwrap_or(0.);
        let exact = if bits == 64 {
            scale == 1. && zero == 0.
        } else {
            scale != 0.
                && scale.is_finite()
                && scale.fract() == 0.
                && zero.is_finite()
                && zero.fract() == 0.
                && 2f64.powi(bits) * scale.abs() + zero.abs() <= 2f64.powi(53)
        };
        if !exact {
            return Err(Error::Unsupported(
                "integer column scaling is not exact in 64-bit integers".into(),
            ));
        }
        // Fixed-width shim copies from matching native LONGLONG temporaries.
        let _temporary = self.budget.charge(
            count
                .checked_mul(8)
                .ok_or_else(|| Error::Bounds("column scratch overflow".into()))?,
        )?;
        let mut output = self.plane(count, 0i64)?;
        self.status(unsafe {
            ffi::sv_col_read(
                self.pointer(),
                column as i32,
                row as i64,
                first as i64,
                count as i64,
                1,
                output.values.as_mut_ptr().cast(),
                output.valid.as_mut_ptr(),
            )
        })?;
        Ok(output)
    }
    pub fn is_reentrant(&self) -> bool {
        unsafe { ffi::sv_reentrant() == 1 }
    }
}
impl Drop for Fits {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Null-flag reads of compressed images must use single full-row tiles. For
/// multi-row tiles the pinned CFITSIO 4.7.0 tile cache dereferences a missing
/// null array when the image has no BLANK (imcompress.c:5976) and leaks one
/// null array per decoded tile (imcompress.c:6898); full-row tiles bypass that
/// cache entirely (imcompress.c:5954, 6856).
fn full_row_tiles(image: &ImageInfo) -> Result<()> {
    let Some(tiles) = &image.tile_axes else {
        return Ok(());
    };
    if tiles.first() != image.axes.first() || tiles.iter().skip(1).any(|n| *n != 1) {
        return Err(Error::Unsupported(
            "null-flag reads of compressed images need single full-row tiles".into(),
        ));
    }
    Ok(())
}
