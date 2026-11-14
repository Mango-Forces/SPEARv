use std::{
    cell::Cell,
    ffi::{CStr, CString},
    marker::PhantomData,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, OnceLock, PoisonError,
    },
    time::Duration,
};

use ndarray::ArrayView1;
use numpy::{Element, IntoPyArray, PyArray1, PyArrayMethods, PyUntypedArrayMethods};
use pyo3::{
    ffi,
    prelude::*,
    types::{PyCapsule, PyDict, PyModule, PyTuple},
};
use spearv_core::{
    math::{
        Counts, Geometry, ImageMath, MaskPolicy, MathControl, MathError, MathLease, MathResult,
        MathSession, NativeInput, NativeWindow, OutputPlanes, Pinned, Product, RasterOrder,
        SpectrumInput, SpectrumPlanes, TargetGrid, Tile, TileOutput, Values,
    },
    FlagBits, Poly, Wcs, WorkBudget,
};

/// Trusted deployment configuration. The host constructor is qualification
/// only; it must not be used as a production distribution/ABI decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub executable: PathBuf,
    pub stdlib: PathBuf,
    pub dynload: PathBuf,
    pub packages: PathBuf,
    /// Process-lifetime imports, native libraries and allocator-retention charge.
    pub resident_bytes: u64,
    pub max_workers: usize,
    pub max_native_pixels: usize,
    pub max_spectrum_samples: usize,
}

impl RuntimeConfig {
    /// Load the private Python runtime configuration from `SPEARV_PYTHON_*`
    /// overrides or the default Bookworm container prefix `/opt/spearv/python`.
    pub fn from_env() -> Self {
        let prefix = std::env::var("SPEARV_PYTHON_PREFIX")
            .unwrap_or_else(|_| "/opt/spearv/python".into());
        let path_var = |key: &str, default: String| {
            std::env::var(key).ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(default))
        };
        Self {
            executable: path_var("SPEARV_PYTHON_EXECUTABLE", format!("{prefix}/bin/python3.14")),
            stdlib: path_var("SPEARV_PYTHON_STDLIB", format!("{prefix}/lib/python3.14")),
            dynload: path_var(
                "SPEARV_PYTHON_DYNLOAD",
                format!("{prefix}/lib/python3.14/lib-dynload"),
            ),
            packages: path_var(
                "SPEARV_PYTHON_PACKAGES",
                format!("{prefix}/lib/python3.14/site-packages"),
            ),
            resident_bytes: spearv_core::env::number::<u64>("SPEARV_PYTHON_RESIDENT_MIB", 128)
                .saturating_mul(1024 * 1024),
            max_workers: spearv_core::env::number("SPEARV_PYTHON_WORKERS", 4),
            max_native_pixels: 2040 * 2040,
            max_spectrum_samples: 65_536,
        }
    }

    pub fn host_qualification() -> Self {
        Self {
            executable: "/home/ubuntu/mv-flux-data/venv/bin/python".into(),
            stdlib: "/usr/lib/python3.14".into(),
            dynload: "/usr/lib/python3.14/lib-dynload".into(),
            packages: "/home/ubuntu/mv-flux-data/venv/lib/python3.14/site-packages".into(),
            resident_bytes: 128 * 1024 * 1024,
            max_workers: 4,
            max_native_pixels: 2040 * 2040,
            max_spectrum_samples: 65_536,
        }
    }

    fn validate(&self) -> MathResult<()> {
        if !self.executable.is_file()
            || !self.stdlib.is_dir()
            || !self.dynload.is_dir()
            || !self.packages.is_dir()
        {
            return Err(MathError::Runtime(
                "explicit interpreter/module path is missing".into(),
            ));
        }
        if self.resident_bytes < 128 * 1024 * 1024
            || self.max_workers == 0
            || self.max_workers > 32
            || self.max_native_pixels == 0
            || self.max_spectrum_samples == 0
        {
            return Err(MathError::Input(
                "runtime charge or configured bounded worker/input limits".into(),
            ));
        }
        Ok(())
    }
}

/// How often a call waiting for a worker slot rechecks its own cancellation.
const WORKER_POLL: Duration = Duration::from_millis(50);

/// Bounds concurrent library calls. A call waits for a slot: a full pool is
/// never an error, and only the caller's own cancellation ends the wait.
struct Workers {
    free: Mutex<usize>,
    released: Condvar,
}

struct WorkerSlot(Arc<Workers>);

impl Workers {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            free: Mutex::new(limit),
            released: Condvar::new(),
        })
    }

    /// Blocks the calling thread, so it must be a blocking math worker.
    fn acquire(self: &Arc<Self>, control: &MathControl) -> MathResult<WorkerSlot> {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            control.check()?;
            if *free > 0 {
                *free -= 1;
                return Ok(WorkerSlot(Arc::clone(self)));
            }
            free = self
                .released
                .wait_timeout(free, WORKER_POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.0.released.notify_one();
    }
}

struct Interpreter {
    config: RuntimeConfig,
    module: Py<PyModule>,
    workers: Arc<Workers>,
    budget: Arc<WorkBudget>,
    description: String,
    // CPython is intentionally never finalized by this in-process backend.
    // Its imports and allocator charge therefore also remain process-lifetime.
    _resident: MathLease,
}

static INTERPRETER: OnceLock<Arc<Interpreter>> = OnceLock::new();
static INITIALIZATION_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct PythonMath {
    interpreter: Arc<Interpreter>,
}

impl PythonMath {
    /// Call at process startup, before other Python users. No initialization
    /// mutex spans attachment. A concurrent/retried failed init is an error,
    /// not implicit adoption of a foreign or partly initialized interpreter.
    pub fn initialize(config: RuntimeConfig, budget: Arc<WorkBudget>) -> MathResult<Self> {
        config.validate()?;
        if let Some(interpreter) = INTERPRETER.get() {
            if interpreter.config != config || !Arc::ptr_eq(&interpreter.budget, &budget) {
                return Err(MathError::Runtime(
                    "interpreter already uses another configuration/budget".into(),
                ));
            }
            return Ok(Self {
                interpreter: Arc::clone(interpreter),
            });
        }
        let resident = MathLease::reserve(&budget, config.resident_bytes)?;
        if INITIALIZATION_STARTED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(MathError::Runtime(
                "initialization is already running or previously failed".into(),
            ));
        }
        // If initialization has begun, its process-wide resources cannot safely
        // be rolled back. Keep the charge even when preflight subsequently fails.
        static FAILED_RESIDENT: OnceLock<MathLease> = OnceLock::new();
        if let Err(error) = initialize_cpython(&config) {
            let _ = FAILED_RESIDENT.set(resident);
            return Err(error);
        }
        let loaded = Python::attach(|py| -> MathResult<_> {
            let code =
                CString::new(include_str!("../python/spearv_math.py")).map_err(runtime_error)?;
            let module = PyModule::from_code(py, &code, c"spearv_math.py", c"_spearv_math")
                .map_err(runtime_error)?;
            let description = module
                .getattr("preflight")
                .map_err(runtime_error)?
                .call1((config.packages.to_string_lossy().as_ref(),))
                .map_err(runtime_error)?
                .extract::<String>()
                .map_err(runtime_error)?;
            Ok((module.unbind(), description))
        });
        let (module, description) = match loaded {
            Ok(result) => result,
            Err(error) => {
                let _ = FAILED_RESIDENT.set(resident);
                return Err(error);
            }
        };
        let interpreter = Arc::new(Interpreter {
            workers: Workers::new(config.max_workers),
            config,
            module,
            budget,
            description,
            _resident: resident,
        });
        INTERPRETER
            .set(Arc::clone(&interpreter))
            .map_err(|_| MathError::Runtime("interpreter initialization raced".into()))?;
        Ok(Self { interpreter })
    }

    pub fn description(&self) -> &str {
        &self.interpreter.description
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.interpreter.config
    }

    pub fn budget(&self) -> &Arc<WorkBudget> {
        &self.interpreter.budget
    }
}

fn runtime_error(error: impl std::fmt::Display) -> MathError {
    MathError::Runtime(error.to_string())
}

/// Client input is validated at the Rust boundary. Anything the library itself
/// raises is a server-side failure, except count overflow.
fn library_error(error: PyErr) -> MathError {
    Python::attach(|py| {
        if error.is_instance_of::<pyo3::exceptions::PyOverflowError>(py) {
            return MathError::Overflow;
        }
        MathError::Internal(error.to_string())
    })
}

fn initialize_cpython(config: &RuntimeConfig) -> MathResult<()> {
    // Matching PyO3 CPython ABI only. No raw array pointers or Arc exporters.
    // PyConfig is cleared on every path. Never unwind or finalize Python here.
    unsafe {
        if ffi::Py_IsInitialized() != 0 {
            return Err(MathError::Runtime(
                "foreign interpreter was initialized before math".into(),
            ));
        }
        let mut pyconfig: ffi::PyConfig = std::mem::zeroed();
        ffi::PyConfig_InitIsolatedConfig(&mut pyconfig);
        pyconfig.install_signal_handlers = 0;
        pyconfig.write_bytecode = 0;
        pyconfig.parse_argv = 0;
        pyconfig.site_import = 0;
        pyconfig.module_search_paths_set = 1;
        let configured = (|| {
            let executable = CString::new(config.executable.to_string_lossy().as_bytes())
                .map_err(runtime_error)?;
            py_status(ffi::PyConfig_SetBytesString(
                &mut pyconfig,
                &mut pyconfig.executable,
                executable.as_ptr(),
            ))?;
            for path in [&config.stdlib, &config.dynload, &config.packages] {
                let text =
                    CString::new(path.to_string_lossy().as_bytes()).map_err(runtime_error)?;
                let mut decoded = std::ptr::null_mut();
                py_status(ffi::PyConfig_SetBytesString(
                    &mut pyconfig,
                    &mut decoded,
                    text.as_ptr(),
                ))?;
                let appended =
                    ffi::PyWideStringList_Append(&mut pyconfig.module_search_paths, decoded);
                ffi::PyMem_RawFree(decoded.cast());
                py_status(appended)?;
            }
            py_status(ffi::Py_InitializeFromConfig(&pyconfig))
        })();
        ffi::PyConfig_Clear(&mut pyconfig);
        configured?;
        ffi::PyEval_SaveThread();
        Ok(())
    }
}

unsafe fn py_status(status: ffi::PyStatus) -> MathResult<()> {
    if ffi::PyStatus_Exception(status) == 0 {
        return Ok(());
    }
    let reason = if status.err_msg.is_null() {
        "CPython requested initialization exit".to_owned()
    } else {
        CStr::from_ptr(status.err_msg)
            .to_string_lossy()
            .into_owned()
    };
    Err(MathError::Runtime(reason))
}

fn readonly<T: Element>(py: Python<'_>, data: Vec<T>) -> MathResult<Py<PyArray1<T>>> {
    let array = data.into_pyarray(py);
    array
        .call_method1("setflags", (false,))
        .map_err(library_error)?;
    Ok(array.unbind())
}

/// A read-only NumPy view of an immutable pinned buffer, without copying it.
/// The array owns `pin`, so the buffer outlives any reference Python keeps.
fn pinned_view<'py, T: Element, P: Send + 'static>(
    py: Python<'py>,
    data: &[T],
    pin: P,
) -> MathResult<Bound<'py, PyArray1<T>>> {
    let owner = PyCapsule::new(py, pin, None).map_err(library_error)?;
    // SAFETY: `data` lives in the buffer `pin` keeps alive and never mutates,
    // and the array is made read-only before Python can see it.
    let array = unsafe { PyArray1::borrow_from_array(&ArrayView1::from(data), owner.into_any()) };
    array
        .call_method1("setflags", (false,))
        .map_err(library_error)?;
    Ok(array)
}

fn copy_array<T: Element + Copy>(array: &Bound<'_, PyAny>, n: usize) -> MathResult<Vec<T>> {
    let array = array
        .cast::<PyArray1<T>>()
        .map_err(|_| MathError::Internal("library output dtype/rank".into()))?;
    if array.shape() != [n] {
        return Err(MathError::Internal("library output length".into()));
    }
    // A strided result is fine: this is a checked, safe Rust-owned copy.
    Ok(array.readonly().as_array().iter().copied().collect())
}

fn product_spec(product: &Product) -> MathResult<(&'static str, Vec<u16>, u64, u64)> {
    match product {
        Product::Values => Ok(("values", vec![], 16, 4)),
        Product::Science => Ok(("science", vec![], 18, 10)),
        Product::Depth { band } if *band < 102 => Ok(("depth", vec![*band], 24, 24)),
        Product::CompleteBands { bands } if !bands.is_empty() && bands.iter().all(|b| *b < 102) => {
            let mut selected = bands.clone();
            selected.sort_unstable();
            selected.dedup();
            Ok(("complete", selected, 18, 8))
        }
        _ => Err(MathError::Input(
            "nonempty internal band selection in 0..102 is required".into(),
        )),
    }
}

/// Persistent running sums owned and charged in Rust. No Python object or
/// worker slot outlives a single call, so many sessions can stay alive across
/// member order and creating or dropping one never takes the GIL.
struct Session {
    runtime: Arc<Interpreter>,
    control: MathControl,
    tile: Tile,
    product: Product,
    kind: &'static str,
    state_per_pixel: u64,
    bands: Vec<u16>,
    added: Vec<u16>,
    last_order: Option<u64>,
    sum_f: Vec<f64>,
    sum_w: Vec<f64>,
    count: Option<Counts>,
    state: MathLease,
    _exclusive: PhantomData<Cell<()>>,
}

impl ImageMath for PythonMath {
    fn session(
        &self,
        control: MathControl,
        tile: Tile,
        product: Product,
    ) -> MathResult<Box<dyn MathSession>> {
        control.check()?;
        tile.validate()?;
        let (kind, bands, state_bytes, _) = product_spec(&product)?;
        let product = match product {
            Product::CompleteBands { .. } => Product::CompleteBands {
                bands: bands.clone(),
            },
            product => product,
        };
        let n = tile.shape.len()?;
        let state = MathLease::reserve(
            &self.interpreter.budget,
            tile.shape.bytes(state_bytes)? + 256,
        )?;
        let count = match kind {
            "science" | "complete" => Some(Counts::U16(vec![0; n])),
            "depth" => Some(Counts::U64(vec![0; n])),
            _ => None,
        };
        Ok(Box::new(Session {
            runtime: Arc::clone(&self.interpreter),
            control,
            tile,
            product,
            kind,
            state_per_pixel: state_bytes,
            bands,
            added: Vec::new(),
            last_order: None,
            sum_f: vec![0.0; n],
            sum_w: vec![0.0; n],
            count,
            state,
            _exclusive: PhantomData,
        }))
    }

    fn spectrum(
        &self,
        control: &MathControl,
        input: &SpectrumInput,
    ) -> MathResult<Pinned<SpectrumPlanes>> {
        control.check()?;
        let input = input.samples();
        let n = input.flux.len();
        if n > self.config().max_spectrum_samples {
            return Err(MathError::Input("runtime spectrum sample bound".into()));
        }
        let _worker = self.interpreter.workers.acquire(control)?;
        let _scratch = MathLease::reserve(
            self.budget(),
            (n as u64).checked_mul(64).ok_or(MathError::Overflow)? + 1024 * 1024,
        )?;
        let output_lease = MathLease::reserve(self.budget(), 102 * 48 + n as u64)?;
        let planes = Python::attach(|py| -> MathResult<_> {
            let flux = readonly(py, input.flux.clone())?;
            let variance = readonly(py, input.variance.clone())?;
            let band = readonly(py, input.band.clone())?;
            let rejected = readonly(py, input.rejected.clone())?;
            let output = self
                .interpreter
                .module
                .bind(py)
                .getattr("spectrum")
                .map_err(library_error)?
                .call1((
                    flux.bind(py),
                    variance.bind(py),
                    band.bind(py),
                    rejected.bind(py),
                ))
                .map_err(library_error)?;
            Ok(SpectrumPlanes {
                value: copy_array(&output.get_item(0).map_err(library_error)?, 102)?,
                error: copy_array(&output.get_item(1).map_err(library_error)?, 102)?,
                counts: copy_array(&output.get_item(2).map_err(library_error)?, 102)?,
                invalid_counts: copy_array(&output.get_item(3).map_err(library_error)?, 102)?,
                last_valid: copy_array(&output.get_item(4).map_err(library_error)?, 102)?,
                first_invalid: copy_array(&output.get_item(5).map_err(library_error)?, 102)?,
                accepted: copy_array(&output.get_item(6).map_err(library_error)?, n)?,
            })
        })?;
        control.check()?;
        Ok(Pinned::new(planes, output_lease))
    }
}

/// Target pixels between the exact transform nodes of a geometry lattice.
const LATTICE_NODE: u32 = 16;
/// How close to a rounding boundary, in native pixels, an interpolated
/// position is solved exactly instead. The lattice is only used when its
/// interpolation error is at most half of this.
const LATTICE_AMBIGUOUS: f64 = 2e-3;

/// Exact native positions of every [`LATTICE_NODE`]-th tile pixel, row-major,
/// together with the library frames used to solve them.
struct Lattice {
    x: Vec<f64>,
    y: Vec<f64>,
    rows: usize,
    cols: usize,
    frames: Py<PyAny>,
}

impl Lattice {
    /// The lattice cell and the weight of its far node for one tile pixel.
    fn cell(at: usize, nodes: usize) -> (usize, f64) {
        let position = at as f64 / f64::from(LATTICE_NODE);
        let cell = (position as usize).min(nodes - 2);
        (cell, position - cell as f64)
    }

    /// The native index of every tile pixel whose interpolated position is
    /// unambiguous, and the tile pixels that need the exact transform: those
    /// near enough to a rounding boundary for the interpolation error to pick
    /// another native pixel. One native pixel of margin around the window
    /// keeps the pixels an exact solve could bring into it.
    fn select(&self, tile: Tile, window: NativeWindow) -> (Vec<i64>, Vec<i64>, Vec<i64>) {
        let (width, height) = (tile.shape.width as usize, tile.shape.height as usize);
        let support = Support::of(window);
        let mut indices = vec![-1; width * height];
        let (mut cols, mut rows) = (Vec::new(), Vec::new());
        let across: Vec<_> = (0..width).map(|col| Self::cell(col, self.cols)).collect();
        for row in 0..height {
            let (cell, down) = Self::cell(row, self.rows);
            let (upper, lower) = (cell * self.cols, (cell + 1) * self.cols);
            for (col, &(left, right)) in across.iter().enumerate() {
                let at = |nodes: &[f64]| {
                    let top = nodes[upper + left] * (1.0 - right) + nodes[upper + left + 1] * right;
                    let bottom = nodes[lower + left] * (1.0 - right) + nodes[lower + left + 1] * right;
                    top * (1.0 - down) + bottom * down
                };
                let (px, py) = (at(&self.x), at(&self.y));
                let (ix, iy) = ((px + 0.5).floor(), (py + 0.5).floor());
                if !support.near(ix, iy) {
                    continue;
                }
                let sure = 0.5 - LATTICE_AMBIGUOUS;
                if (px - ix).abs() > sure || (py - iy).abs() > sure {
                    cols.push(col as i64);
                    rows.push(row as i64);
                } else if let Some(index) = support.index(ix, iy) {
                    indices[row * width + col] = index;
                }
            }
        }
        (indices, cols, rows)
    }
}

/// The native pixels one window can select, as FITS pixel numbers.
struct Support {
    first: [f64; 2],
    last: [f64; 2],
    width: i64,
}

impl Support {
    fn of(window: NativeWindow) -> Self {
        let first = [f64::from(window.origin[0]), f64::from(window.origin[1])];
        Self {
            first,
            last: [
                first[0] + f64::from(window.shape.width) - 1.0,
                first[1] + f64::from(window.shape.height) - 1.0,
            ],
            width: i64::from(window.shape.width),
        }
    }

    fn near(&self, ix: f64, iy: f64) -> bool {
        ix >= self.first[0] - 1.0
            && ix <= self.last[0] + 1.0
            && iy >= self.first[1] - 1.0
            && iy <= self.last[1] + 1.0
    }

    /// The row-major window index of native pixel `(ix, iy)`, if it has one.
    fn index(&self, ix: f64, iy: f64) -> Option<i64> {
        (ix >= self.first[0] && ix <= self.last[0] && iy >= self.first[1] && iy <= self.last[1])
            .then(|| (iy - self.first[1]) as i64 * self.width + (ix - self.first[0]) as i64)
    }
}

impl Session {
    /// The native WCS, target grid and tile as the library's geometry calls
    /// take them.
    fn frames<'py>(
        &self,
        py: Python<'py>,
        native: &Wcs,
        target: TargetGrid,
    ) -> MathResult<(Bound<'py, PyDict>, Bound<'py, PyDict>, Bound<'py, PyDict>)> {
        let grid = PyDict::new(py);
        for (key, value) in [
            ("ra", target.ra),
            ("dec", target.dec),
            ("scale", target.scale_deg),
        ] {
            grid.set_item(key, value).map_err(library_error)?;
        }
        grid.set_item("width", target.shape.width)
            .map_err(library_error)?;
        grid.set_item("height", target.shape.height)
            .map_err(library_error)?;
        let tile = PyDict::new(py);
        tile.set_item("x", self.tile.x).map_err(library_error)?;
        tile.set_item("y", self.tile.y).map_err(library_error)?;
        tile.set_item("width", self.tile.shape.width)
            .map_err(library_error)?;
        tile.set_item("height", self.tile.shape.height)
            .map_err(library_error)?;
        tile.set_item("png", self.tile.order == RasterOrder::Png)
            .map_err(library_error)?;
        Ok((wcs_dict(py, native)?, grid, tile))
    }

    /// The native index each tile pixel selects, -1 outside the window. The
    /// library solves the exact transform on a lattice; positions between its
    /// nodes are interpolated here, off the GIL, and every pixel the
    /// interpolation error could move to another native pixel goes back to
    /// the library, so the result is the exact transform's. A tile the
    /// lattice cannot serve is solved exactly pixel by pixel.
    fn indices(
        &self,
        native: &Wcs,
        window: NativeWindow,
        target: TargetGrid,
    ) -> MathResult<Vec<i64>> {
        let transform = |error: PyErr| MathError::Transform(error.to_string());
        let module = &self.runtime.module;
        let lattice = Python::attach(|py| -> MathResult<_> {
            let (spec, grid, tile) = self.frames(py, native, target)?;
            let result = module
                .bind(py)
                .getattr("geometry_lattice")
                .map_err(library_error)?
                .call1((spec, grid, tile, LATTICE_NODE, LATTICE_AMBIGUOUS / 2.0))
                .map_err(transform)?;
            if result.is_none() {
                return Ok(None);
            }
            let size = |item: usize| -> MathResult<usize> {
                result
                    .get_item(item)
                    .and_then(|value| value.extract())
                    .map_err(library_error)
            };
            let (rows, cols) = (size(2)?, size(3)?);
            if rows < 2 || cols < 2 {
                return Err(MathError::Internal("library lattice shape".into()));
            }
            Ok(Some(Lattice {
                x: copy_array(&result.get_item(0).map_err(library_error)?, rows * cols)?,
                y: copy_array(&result.get_item(1).map_err(library_error)?, rows * cols)?,
                rows,
                cols,
                frames: result.get_item(4).map_err(library_error)?.unbind(),
            }))
        })?;
        let Some(lattice) = lattice else {
            return Python::attach(|py| {
                let (spec, grid, tile) = self.frames(py, native, target)?;
                let native_window = PyDict::new(py);
                for (key, value) in [
                    ("x", i64::from(window.origin[0])),
                    ("y", i64::from(window.origin[1])),
                    ("width", i64::from(window.shape.width)),
                    ("height", i64::from(window.shape.height)),
                ] {
                    native_window.set_item(key, value).map_err(library_error)?;
                }
                let result = module
                    .bind(py)
                    .getattr("geometry")
                    .map_err(library_error)?
                    .call1((spec, native_window, grid, tile))
                    .map_err(transform)?;
                copy_array(&result, self.tile.shape.len()?)
            });
        };
        self.control.check()?;
        let (mut indices, cols, rows) = lattice.select(self.tile, window);
        if cols.is_empty() {
            return Ok(indices);
        }
        let width = self.tile.shape.width as usize;
        let pixels: Vec<_> = cols
            .iter()
            .zip(&rows)
            .map(|(col, row)| *row as usize * width + *col as usize)
            .collect();
        let frames = lattice.frames;
        let (ix, iy) = Python::attach(|py| -> MathResult<(Vec<f64>, Vec<f64>)> {
            let result = module
                .bind(py)
                .getattr("geometry_exact")
                .map_err(library_error)?
                .call1((frames.bind(py), cols.into_pyarray(py), rows.into_pyarray(py)))
                .map_err(transform)?;
            Ok((
                copy_array(&result.get_item(0).map_err(library_error)?, pixels.len())?,
                copy_array(&result.get_item(1).map_err(library_error)?, pixels.len())?,
            ))
        })?;
        let support = Support::of(window);
        for ((pixel, ix), iy) in pixels.into_iter().zip(ix).zip(iy) {
            indices[pixel] = support.index(ix, iy).unwrap_or(-1);
        }
        Ok(indices)
    }

    /// A worker slot and scratch charge for one library call only. Waits for
    /// the slot, so the scratch is charged only while the call can run.
    fn admit(&self, scratch: u64) -> MathResult<(WorkerSlot, MathLease)> {
        let worker = self.runtime.workers.acquire(&self.control)?;
        Ok((worker, MathLease::reserve(&self.runtime.budget, scratch)?))
    }

    /// Private copies of the running state plus masks/coordinates, gathers,
    /// f64 temporaries and WCS objects; no full-grid bridge allocation.
    fn call_scratch(&self) -> MathResult<u64> {
        Ok(self.tile.shape.bytes(2 * self.state_per_pixel + 512)? + 1024 * 1024)
    }

    /// Writable NumPy views of the running state for one call. The library
    /// validates every input and count bound before mutating them.
    fn state_arrays<'py>(&mut self, py: Python<'py>) -> Bound<'py, PyTuple> {
        // SAFETY: `&mut self` is held for the synchronous library call, and
        // the library never retains the state tuple after returning.
        unsafe fn view<'py, T: Element>(py: Python<'py>, data: &mut [T]) -> Bound<'py, PyAny> {
            let owner = PyTuple::empty(py).into_any();
            unsafe { PyArray1::borrow_from_array(&ArrayView1::from(&*data), owner) }.into_any()
        }
        let sum_f = unsafe { view(py, &mut self.sum_f) };
        let sum_w = unsafe { view(py, &mut self.sum_w) };
        let count = match &mut self.count {
            None => py.None().into_bound(py),
            Some(Counts::U16(v)) => unsafe { view(py, v) },
            Some(Counts::U64(v)) => unsafe { view(py, v) },
        };
        PyTuple::new(py, [sum_f, sum_w, count]).expect("three-element state tuple")
    }

    fn output(&mut self, final_output: bool) -> MathResult<TileOutput> {
        self.control.check()?;
        if final_output && self.kind == "complete" && self.added.len() != self.bands.len() {
            return Err(MathError::Internal(
                "finished before every selected band was added".into(),
            ));
        }
        let (_, _, _, bytes) = product_spec(&self.product)?;
        let (_worker, _scratch) = self.admit(self.call_scratch()?)?;
        let lease = MathLease::reserve(&self.runtime.budget, self.tile.shape.bytes(bytes)?)?;
        let module = Arc::clone(&self.runtime);
        let nbands = self.bands.len();
        let kind = self.kind;
        let len = self.tile.shape.len()?;
        let product = self.product.clone();
        let planes = Python::attach(|py| -> MathResult<_> {
            let output = module
                .module
                .bind(py)
                .getattr("output")
                .map_err(library_error)?
                .call1((kind, self.state_arrays(py), nbands))
                .map_err(library_error)?;
            output_planes(&output, len, &product)
        })?;
        self.control.check()?;
        TileOutput::from_library(self.tile, self.product.clone(), planes, lease)
    }
}

impl MathSession for Session {
    fn control(&self) -> &MathControl {
        &self.control
    }

    fn state_bytes(&self) -> u64 {
        self.state.bytes()
    }

    fn geometry(
        &mut self,
        native: &Wcs,
        window: NativeWindow,
        target: TargetGrid,
    ) -> MathResult<Geometry> {
        self.control.check()?;
        let n = window.shape.len()?;
        if n > self.runtime.config.max_native_pixels
            || !target.ra.is_finite()
            || !(0.0..=360.0).contains(&target.ra)
            || !target.dec.is_finite()
            || !(-90.0..=90.0).contains(&target.dec)
            || !target.scale_deg.is_finite()
            || target.scale_deg <= 0.0
            || self
                .tile
                .x
                .checked_add(self.tile.shape.width)
                .is_none_or(|end| end > target.shape.width)
            || self
                .tile
                .y
                .checked_add(self.tile.shape.height)
                .is_none_or(|end| end > target.shape.height)
            || native.naxis.iter().any(|axis| *axis < 1)
            || window.origin.iter().any(|value| *value < 1)
            || i64::from(window.origin[0]) + i64::from(window.shape.width) - 1
                > i64::from(native.naxis[0])
            || i64::from(window.origin[1]) + i64::from(window.shape.height) - 1
                > i64::from(native.naxis[1])
            || native
                .crpix
                .iter()
                .chain(native.crval.iter())
                .chain(native.cd.iter().flatten())
                .any(|v| !v.is_finite())
        {
            return Err(MathError::Input(
                "WCS, target tile or native window bounds".into(),
            ));
        }
        target.shape.len()?;
        let (_worker, _scratch) = self.admit(self.call_scratch()?)?;
        let lease = MathLease::reserve(
            &self.runtime.budget,
            Geometry::required_bytes(self.tile, native)?,
        )?;
        let indices = self.indices(native, window, target)?;
        self.control.check()?;
        Geometry::from_library(self.tile, window, native.clone(), target, indices, lease)
    }

    fn add(
        &mut self,
        order: u64,
        input: &NativeInput,
        geometry: &Geometry,
        bits: FlagBits,
        mask: MaskPolicy,
        bands: &[u16],
    ) -> MathResult<()> {
        self.control.check()?;
        if self.last_order.is_some_and(|last| order <= last)
            || input.planes().window != geometry.window()
            || input.planes().wcs != *geometry.native_wcs()
            || geometry.tile() != self.tile
            || bands.iter().any(|b| *b >= 102)
            || input.planes().window.shape.len()? > self.runtime.config.max_native_pixels
        {
            return Err(MathError::Input(
                "exposure order, geometry/input identity, selection or bound".into(),
            ));
        }
        let selected = match self.product {
            Product::Depth { band } => vec![band],
            Product::CompleteBands { .. } => {
                return Err(MathError::Input("finished depth bands required".into()))
            }
            _ => bands.to_vec(),
        };
        if geometry.indices().iter().all(|index| *index < 0) {
            self.last_order = Some(order);
            return self.control.check();
        }
        let policy = match (&self.product, mask) {
            (Product::Depth { .. }, _) | (_, MaskPolicy::Spectrum) => "spectrum",
            (_, MaskPolicy::None) => "none",
            (_, MaskPolicy::Composite) => "composite",
            (_, MaskPolicy::Delta) => "delta",
        };
        // Python sees the pinned native planes and geometry in place; only the
        // tile-sized gathers it makes from them are scratch.
        let (_worker, _scratch) = self.admit(self.call_scratch()?)?;
        let module = Arc::clone(&self.runtime);
        Python::attach(|py| -> MathResult<()> {
            let planes = input.planes();
            let image = pinned_view(py, &planes.image, input.clone())?;
            let variance = pinned_view(py, &planes.variance, input.clone())?;
            let zodi = pinned_view(py, &planes.zodi, input.clone())?;
            let flags = pinned_view(py, &planes.flags, input.clone())?;
            let native_bands = pinned_view(py, &planes.bands, input.clone())?;
            let valid = pinned_view(py, &planes.valid, input.clone())?;
            let indices = pinned_view(py, geometry.indices(), geometry.clone())?;
            let flags_spec = PyDict::new(py);
            for (name, value) in [
                ("source", bits.source),
                ("fullsample", bits.fullsample),
                ("transient", bits.transient),
                ("overflow", bits.overflow),
                ("sur_error", bits.sur_error),
                ("outlier", bits.outlier),
            ] {
                flags_spec.set_item(name, value).map_err(library_error)?;
            }
            let state = self.state_arrays(py);
            module
                .module
                .bind(py)
                .getattr("accumulate")
                .map_err(library_error)?
                .call1((
                    self.kind,
                    &state,
                    image,
                    variance,
                    zodi,
                    flags,
                    native_bands,
                    valid,
                    indices,
                    flags_spec,
                    policy,
                    selected,
                ))
                .map_err(library_error)?;
            Ok(())
        })?;
        self.last_order = Some(order);
        self.control.check()
    }

    fn add_band(&mut self, depth: &TileOutput) -> MathResult<()> {
        self.control.check()?;
        let Product::Depth { band } = depth.product() else {
            return Err(MathError::Input("Specific needs a depth output".into()));
        };
        if depth.tile() != self.tile {
            return Err(MathError::Input("depth/sector tile mismatch".into()));
        }
        if self.kind != "complete"
            || self.bands.iter().find(|b| !self.added.contains(b)) != Some(band)
        {
            return Err(MathError::Input(
                "depth bands must be unadded selected bands in canonical order".into(),
            ));
        }
        let band = *band;
        let (_worker, _scratch) = self.admit(self.call_scratch()? + self.tile.shape.bytes(24)?)?;
        let module = Arc::clone(&self.runtime);
        Python::attach(|py| -> MathResult<()> {
            let Values::F64(value) = &depth.planes().value else {
                return Err(MathError::Input("depth f64".into()));
            };
            let Some(Values::F64(variance)) = &depth.planes().variance else {
                return Err(MathError::Input("depth variance".into()));
            };
            let Some(Counts::U64(counts)) = &depth.planes().counts else {
                return Err(MathError::Input("depth counts".into()));
            };
            let value = readonly(py, value.clone())?;
            let variance = readonly(py, variance.clone())?;
            let counts = readonly(py, counts.clone())?;
            let state = self.state_arrays(py);
            module
                .module
                .bind(py)
                .getattr("add_band")
                .map_err(library_error)?
                .call1((&state, value.bind(py), variance.bind(py), counts.bind(py)))
                .map_err(library_error)?;
            Ok(())
        })?;
        self.added.push(band);
        self.control.check()
    }

    fn snapshot(&mut self) -> MathResult<TileOutput> {
        self.output(false)
    }

    fn finish(mut self: Box<Self>) -> MathResult<TileOutput> {
        // Output is reserved before the obsolete sums and their charge drop.
        self.output(true)
    }

    fn difference(&mut self, plus: &TileOutput, minus: &TileOutput) -> MathResult<TileOutput> {
        self.control.check()?;
        if plus.tile() != self.tile
            || minus.tile() != self.tile
            || !matches!(
                (plus.product(), minus.product(), &self.product),
                (Product::Values, Product::Values, Product::Values)
                    | (Product::Science, Product::Science, Product::Science)
            )
        {
            return Err(MathError::Input(
                "difference needs matching values/science tiles".into(),
            ));
        }
        let (_, _, _, bytes) = product_spec(&self.product)?;
        let (_worker, _bridge) = self.admit(self.tile.shape.bytes(bytes * 2)? + 1024 * 1024)?;
        let lease = MathLease::reserve(&self.runtime.budget, self.tile.shape.bytes(bytes)?)?;
        let planes = Python::attach(|py| -> MathResult<_> {
            let Values::F32(p) = &plus.planes().value else {
                return Err(MathError::Input("difference f32".into()));
            };
            let Values::F32(m) = &minus.planes().value else {
                return Err(MathError::Input("difference f32".into()));
            };
            let p = readonly(py, p.clone())?;
            let m = readonly(py, m.clone())?;
            let output = if matches!(self.product, Product::Values) {
                self.runtime
                    .module
                    .bind(py)
                    .getattr("difference")
                    .map_err(library_error)?
                    .call1((
                        p.bind(py),
                        m.bind(py),
                        py.None(),
                        py.None(),
                        py.None(),
                        py.None(),
                    ))
                    .map_err(library_error)?
            } else {
                let Some(Values::F32(pv)) = &plus.planes().variance else {
                    return Err(MathError::Input("plus variance".into()));
                };
                let Some(Values::F32(mv)) = &minus.planes().variance else {
                    return Err(MathError::Input("minus variance".into()));
                };
                let Some(Counts::U16(pc)) = &plus.planes().counts else {
                    return Err(MathError::Input("plus count".into()));
                };
                let Some(Counts::U16(mc)) = &minus.planes().counts else {
                    return Err(MathError::Input("minus count".into()));
                };
                let pv = readonly(py, pv.clone())?;
                let mv = readonly(py, mv.clone())?;
                let pc = readonly(py, pc.clone())?;
                let mc = readonly(py, mc.clone())?;
                self.runtime
                    .module
                    .bind(py)
                    .getattr("difference")
                    .map_err(library_error)?
                    .call1((
                        p.bind(py),
                        m.bind(py),
                        pv.bind(py),
                        mv.bind(py),
                        pc.bind(py),
                        mc.bind(py),
                    ))
                    .map_err(library_error)?
            };
            output_planes(&output, self.tile.shape.len()?, &self.product)
        })?;
        self.control.check()?;
        TileOutput::from_library(self.tile, self.product.clone(), planes, lease)
    }
}

fn output_planes(
    output: &Bound<'_, PyAny>,
    n: usize,
    product: &Product,
) -> MathResult<OutputPlanes> {
    let value = output.get_item(0).map_err(library_error)?;
    let variance = output.get_item(1).map_err(library_error)?;
    let counts = output.get_item(2).map_err(library_error)?;
    let depth = matches!(product, Product::Depth { .. });
    Ok(OutputPlanes {
        value: if depth {
            Values::F64(copy_array(&value, n)?)
        } else {
            Values::F32(copy_array(&value, n)?)
        },
        variance: if variance.is_none() {
            None
        } else if depth {
            Some(Values::F64(copy_array(&variance, n)?))
        } else {
            Some(Values::F32(copy_array(&variance, n)?))
        },
        counts: match product {
            Product::Values | Product::CompleteBands { .. } => {
                if !counts.is_none() {
                    return Err(MathError::Internal("unexpected library count plane".into()));
                }
                None
            }
            Product::Depth { .. } => Some(Counts::U64(copy_array(&counts, n)?)),
            Product::Science => Some(Counts::U16(copy_array(&counts, n)?)),
        },
    })
}

fn poly_dict<'py>(py: Python<'py>, poly: &Poly) -> MathResult<Bound<'py, PyDict>> {
    if poly.order > 9
        || poly.coefficients().len() != (poly.order + 1) * (poly.order + 1)
        || poly.coefficients().iter().any(|v| !v.is_finite())
    {
        return Err(MathError::Input(
            "SIP exceeds WCSLIB-supported order or has non-finite coefficients".into(),
        ));
    }
    let dict = PyDict::new(py);
    dict.set_item("order", poly.order).map_err(library_error)?;
    dict.set_item("coeff", poly.coefficients().to_vec())
        .map_err(library_error)?;
    Ok(dict)
}

fn wcs_dict<'py>(py: Python<'py>, native: &Wcs) -> MathResult<Bound<'py, PyDict>> {
    if native.sip.ap.is_some() != native.sip.bp.is_some() {
        return Err(MathError::Input(
            "paired AP/BP coefficients required".into(),
        ));
    }
    let spec = PyDict::new(py);
    spec.set_item("crpix", native.crpix)
        .map_err(library_error)?;
    spec.set_item("crval", native.crval)
        .map_err(library_error)?;
    spec.set_item("cd", native.cd).map_err(library_error)?;
    spec.set_item("naxis", native.naxis)
        .map_err(library_error)?;
    spec.set_item("a", poly_dict(py, &native.sip.a)?)
        .map_err(library_error)?;
    spec.set_item("b", poly_dict(py, &native.sip.b)?)
        .map_err(library_error)?;
    for (key, poly) in [("ap", &native.sip.ap), ("bp", &native.sip.bp)] {
        if let Some(poly) = poly {
            spec.set_item(key, poly_dict(py, poly)?)
                .map_err(library_error)?;
        } else {
            spec.set_item(key, py.None()).map_err(library_error)?;
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    fn runtime() -> PythonMath {
        static MATH: OnceLock<PythonMath> = OnceLock::new();
        MATH.get_or_init(|| {
            PythonMath::initialize(
                RuntimeConfig::host_qualification(),
                Arc::new(WorkBudget::new(512)),
            )
            .unwrap()
        })
        .clone()
    }

    #[test]
    fn private_numpy_slice_owns_base_and_rust_output_outlives_python() {
        let _runtime = runtime();
        let copied = Python::attach(|py| {
            let original = vec![2f32, 4., 6., 8.];
            let private = readonly(py, original.clone()).unwrap();
            drop(original); // No pointer into the original/native cache.
            assert!(private.bind(py).call_method1("setflags", (true,)).is_err());
            assert!(private.bind(py).call_method1("resize", (8,)).is_err());
            let reference = PyModule::import(py, "weakref")
                .unwrap()
                .getattr("ref")
                .unwrap()
                .call1((private.bind(py),))
                .unwrap();
            let slice = private
                .bind(py)
                .get_item(pyo3::types::PySlice::new(py, 1, 4, 2))
                .unwrap();
            drop(private);
            assert!(!reference.call0().unwrap().is_none());
            let copied = copy_array::<f32>(&slice, 2).unwrap();
            drop(slice);
            assert!(
                reference.call0().unwrap().is_none(),
                "last NumPy slice releases the safe transferred Vec base"
            );
            copied
        });
        assert_eq!(copied, [4., 8.]);
    }

    #[test]
    fn wrong_dtype_rank_length_and_python_exceptions_are_errors() {
        let runtime = runtime();
        Python::attach(|py| {
            let numpy = PyModule::import(py, "numpy").unwrap();
            let f64_output = numpy.getattr("zeros").unwrap().call1((4,)).unwrap();
            assert!(matches!(
                copy_array::<f32>(&f64_output, 4),
                Err(MathError::Internal(_))
            ));
            let kwargs = PyDict::new(py);
            kwargs.set_item("dtype", "float32").unwrap();
            let rank2 = numpy
                .getattr("zeros")
                .unwrap()
                .call(((2, 2),), Some(&kwargs))
                .unwrap();
            assert!(copy_array::<f32>(&rank2, 4).is_err());
            let short = readonly(py, vec![1f32, 2.]).unwrap();
            assert!(copy_array::<f32>(short.bind(py).as_any(), 4).is_err());
            let error = runtime
                .interpreter
                .module
                .bind(py)
                .getattr("output")
                .unwrap()
                .call1(("values", (short.bind(py), short.bind(py), py.None()), 0))
                .unwrap_err();
            assert!(matches!(library_error(error), MathError::Internal(_)));
        });
    }

    #[test]
    fn worker_slots_are_per_call_and_a_full_pool_waits() {
        let runtime = runtime();
        let tile = Tile {
            x: 0,
            y: 0,
            shape: spearv_core::math::Shape {
                width: 1,
                height: 1,
            },
            order: RasterOrder::Fits,
        };
        let control = MathControl::new(Arc::new(std::sync::atomic::AtomicU64::new(1)));
        let mut sessions: Vec<_> = (0..64)
            .map(|_| {
                runtime
                    .session(control.clone(), tile, Product::Values)
                    .unwrap()
            })
            .collect();
        let hold = || -> Vec<_> {
            (0..runtime.config().max_workers)
                .map(|_| runtime.interpreter.workers.acquire(&control).unwrap())
                .collect()
        };
        let held = hold();
        let mut waiting = sessions.pop().unwrap();
        let waiter = std::thread::spawn(move || waiting.snapshot().map(|_| ()));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!waiter.is_finished(), "a full pool is waited for, not failed");
        drop(held);
        waiter.join().unwrap().unwrap();
        for session in &mut sessions {
            session.snapshot().unwrap();
        }

        let held = hold();
        let cancelled = MathControl::new(Arc::new(std::sync::atomic::AtomicU64::new(1)));
        let mut waiting = runtime
            .session(cancelled.clone(), tile, Product::Values)
            .unwrap();
        let waiter = std::thread::spawn(move || waiting.snapshot().map(|_| ()));
        std::thread::sleep(Duration::from_millis(200));
        cancelled.cancel();
        assert!(matches!(waiter.join().unwrap(), Err(MathError::Cancelled)));
        drop(held);
    }
}
