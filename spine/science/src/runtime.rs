use std::{
    cell::Cell,
    ffi::{CStr, CString},
    marker::PhantomData,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};

use numpy::{Element, IntoPyArray, PyArray1, PyArrayMethods, PyUntypedArrayMethods};
use pyo3::{
    ffi,
    prelude::*,
    types::{PyDict, PyModule, PyTuple},
};
use spearv_core::{
    math::{
        Counts, Geometry, ImageMath, MaskPolicy, MathControl, MathError, MathLease, MathResult,
        MathSession, NativeInput, NativeWindow, OutputPlanes, Pinned, Product, RasterOrder,
        SpectrumInput, SpectrumPlanes, TargetGrid, Tile, TileOutput, Values,
    },
    FlagBits, Poly, Wcs, WorkBudget,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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

struct Interpreter {
    config: RuntimeConfig,
    module: Py<PyModule>,
    workers: Arc<Semaphore>,
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
            workers: Arc::new(Semaphore::new(config.max_workers)),
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
        let _worker = Arc::clone(&self.interpreter.workers)
            .try_acquire_owned()
            .map_err(|_| MathError::Workers {
                limit: self.interpreter.config.max_workers,
            })?;
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

impl Session {
    /// A worker slot and scratch charge for one library call only.
    fn admit(&self, scratch: u64) -> MathResult<(OwnedSemaphorePermit, MathLease)> {
        let worker = Arc::clone(&self.runtime.workers)
            .try_acquire_owned()
            .map_err(|_| MathError::Workers {
                limit: self.runtime.config.max_workers,
            })?;
        Ok((worker, MathLease::reserve(&self.runtime.budget, scratch)?))
    }

    /// Private copies of the running state plus masks/coordinates, gathers,
    /// f64 temporaries and WCS objects; no full-grid bridge allocation.
    fn call_scratch(&self) -> MathResult<u64> {
        Ok(self.tile.shape.bytes(2 * self.state_per_pixel + 512)? + 1024 * 1024)
    }

    /// Private NumPy copies of the running state for one call.
    fn state_arrays<'py>(&self, py: Python<'py>) -> Bound<'py, PyTuple> {
        let count = match &self.count {
            None => py.None().into_bound(py),
            Some(Counts::U16(v)) => PyArray1::from_slice(py, v).into_any(),
            Some(Counts::U64(v)) => PyArray1::from_slice(py, v).into_any(),
        };
        PyTuple::new(
            py,
            [
                PyArray1::from_slice(py, &self.sum_f).into_any(),
                PyArray1::from_slice(py, &self.sum_w).into_any(),
                count,
            ],
        )
        .expect("three-element state tuple")
    }

    /// Keep the call's state only after the whole library call succeeded.
    fn keep_state(&mut self, state: &Bound<'_, PyTuple>) -> MathResult<()> {
        let n = self.sum_f.len();
        let sum_f = copy_array::<f64>(&state.get_item(0).map_err(library_error)?, n)?;
        let sum_w = copy_array::<f64>(&state.get_item(1).map_err(library_error)?, n)?;
        let count = state.get_item(2).map_err(library_error)?;
        let count = match &self.count {
            None => None,
            Some(Counts::U16(_)) => Some(Counts::U16(copy_array(&count, n)?)),
            Some(Counts::U64(_)) => Some(Counts::U64(copy_array(&count, n)?)),
        };
        self.sum_f = sum_f;
        self.sum_w = sum_w;
        self.count = count;
        Ok(())
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
        let planes = Python::attach(|py| -> MathResult<_> {
            let output = self
                .runtime
                .module
                .bind(py)
                .getattr("output")
                .map_err(library_error)?
                .call1((self.kind, self.state_arrays(py), self.bands.len()))
                .map_err(library_error)?;
            output_planes(&output, self.tile.shape.len()?, &self.product)
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
        let indices = Python::attach(|py| -> MathResult<_> {
            let spec = wcs_dict(py, native)?;
            let window_dict = PyDict::new(py);
            window_dict
                .set_item("x", window.origin[0])
                .map_err(library_error)?;
            window_dict
                .set_item("y", window.origin[1])
                .map_err(library_error)?;
            window_dict
                .set_item("width", window.shape.width)
                .map_err(library_error)?;
            window_dict
                .set_item("height", window.shape.height)
                .map_err(library_error)?;
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
            let result = self
                .runtime
                .module
                .bind(py)
                .getattr("geometry")
                .map_err(library_error)?
                .call1((spec, window_dict, grid, tile))
                .map_err(|e| MathError::Transform(e.to_string()))?;
            copy_array(&result, self.tile.shape.len()?)
        })?;
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
        let policy = match (&self.product, mask) {
            (Product::Depth { .. }, _) | (_, MaskPolicy::Spectrum) => "spectrum",
            (_, MaskPolicy::None) => "none",
            (_, MaskPolicy::Composite) => "composite",
            (_, MaskPolicy::Delta) => "delta",
        };
        // Copies of native inputs and geometry used by Python are separately
        // charged while the Rust cache/input pins remain live.
        let (_worker, _scratch) = self.admit(
            self.call_scratch()?
                + input.planes().window.shape.bytes(19)?
                + self.tile.shape.bytes(8)?,
        )?;
        Python::attach(|py| -> MathResult<()> {
            let planes = input.planes();
            let image = readonly(py, planes.image.clone())?;
            let variance = readonly(py, planes.variance.clone())?;
            let zodi = readonly(py, planes.zodi.clone())?;
            let flags = readonly(py, planes.flags.clone())?;
            let native_bands = readonly(py, planes.bands.clone())?;
            let valid = readonly(py, planes.valid.clone())?;
            let indices = readonly(py, geometry.indices().to_vec())?;
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
            self.runtime
                .module
                .bind(py)
                .getattr("accumulate")
                .map_err(library_error)?
                .call1((
                    self.kind,
                    &state,
                    image.bind(py),
                    variance.bind(py),
                    zodi.bind(py),
                    flags.bind(py),
                    native_bands.bind(py),
                    valid.bind(py),
                    indices.bind(py),
                    flags_spec,
                    policy,
                    selected,
                ))
                .map_err(library_error)?;
            self.keep_state(&state)
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
            self.runtime
                .module
                .bind(py)
                .getattr("add_band")
                .map_err(library_error)?
                .call1((&state, value.bind(py), variance.bind(py), counts.bind(py)))
                .map_err(library_error)?;
            self.keep_state(&state)
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
    fn worker_slots_are_taken_per_call_not_per_session() {
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
        let limit = runtime.config().max_workers;
        let held = Arc::clone(&runtime.interpreter.workers)
            .try_acquire_many_owned(limit as u32)
            .unwrap();
        assert!(matches!(
            sessions[0].snapshot(),
            Err(MathError::Workers { limit: l }) if l == limit
        ));
        drop(held);
        for session in &mut sessions {
            session.snapshot().unwrap();
        }
    }
}
