//! Python-neutral image math contracts. A library backend implements
//! [`ImageMath`]; Rust keeps admission, leases, cancellation and workers.
//! IMAGE, ZODI and physical image/spectrum values are MJy/sr; variance is
//! (MJy/sr)^2. No Jy, clipping, tone or calibration conversion happens here.
//! Native point/scorer preparation remains a separate caller-owned observable.
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use crate::{band_of, Cutout, FlagBits, Wcs, WorkBudget, WorkPermit};

pub type MathResult<T> = std::result::Result<T, MathError>;

#[derive(Debug, thiserror::Error)]
pub enum MathError {
    #[error("invalid math input: {0}")]
    Input(String),
    #[error("math runtime is unavailable: {0}")]
    Runtime(String),
    #[error("library coordinate transform failed: {0}")]
    Transform(String),
    #[error("math operation is not qualified: {0}")]
    Unsupported(&'static str),
    #[error("math needs {bytes} reserved bytes; the work budget is full")]
    Busy { bytes: u64 },
    #[error("all {limit} math workers are busy; retry when one finishes")]
    Workers { limit: usize },
    #[error("math library rejected an internal request: {0}")]
    Internal(String),
    #[error("math request was cancelled")]
    Cancelled,
    #[error("math result belongs to an obsolete generation")]
    Stale,
    #[error("math count or allocation size overflowed")]
    Overflow,
}

/// Cloneable control, not a resource lease. A worker checks before and after
/// each bounded library call; cancellation does not kill a started C kernel.
#[derive(Clone)]
pub struct MathControl {
    cancelled: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl MathControl {
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        let expected = generation.load(Ordering::Acquire);
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            generation,
            expected,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn check(&self) -> MathResult<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(MathError::Cancelled);
        }
        if self.generation.load(Ordering::Acquire) != self.expected {
            return Err(MathError::Stale);
        }
        Ok(())
    }
}

/// An immutable representation and its reservation have one lifetime. Cloning
/// this pin cannot make its buffers free to admission. Data drops before lease.
pub struct Pinned<T>(Arc<Charged<T>>);

struct Charged<T> {
    data: T,
    _lease: MathLease,
}

impl<T> Clone for Pinned<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Pinned<T> {
    pub fn new(data: T, lease: MathLease) -> Self {
        Self(Arc::new(Charged {
            data,
            _lease: lease,
        }))
    }

    pub fn get(&self) -> &T {
        &self.0.data
    }
}

/// Not cloneable: moving a lease transfers ownership without an accounting gap.
pub struct MathLease {
    _permit: WorkPermit,
    bytes: u64,
}

impl MathLease {
    /// Charged at KiB granularity: tile-sized objects are numerous, and whole
    /// MiB rounding per object would refuse work whose real bytes fit.
    pub fn reserve(budget: &WorkBudget, bytes: u64) -> MathResult<Self> {
        if bytes.div_ceil(1024) > u32::MAX as u64 {
            return Err(MathError::Overflow);
        }
        let permit = budget
            .try_acquire_exact(bytes)
            .ok_or(MathError::Busy { bytes })?;
        Ok(Self {
            _permit: permit,
            bytes,
        })
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
}

impl Shape {
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn len(self) -> MathResult<usize> {
        if self.is_empty() {
            return Err(MathError::Input("zero-sized plane".into()));
        }
        usize::try_from(u64::from(self.width) * u64::from(self.height))
            .map_err(|_| MathError::Overflow)
    }

    pub fn bytes(self, per_pixel: u64) -> MathResult<u64> {
        (self.len()? as u64)
            .checked_mul(per_pixel)
            .ok_or(MathError::Overflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RasterOrder {
    Fits,
    Png,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub shape: Shape,
    pub order: RasterOrder,
}

impl Tile {
    pub fn validate(self) -> MathResult<()> {
        self.shape.len()?;
        if self.shape.width > 256 || self.shape.height > 256 {
            return Err(MathError::Input(
                "math operates on at most 256 by 256 tiles".into(),
            ));
        }
        Ok(())
    }
}

/// Target TAN metadata only; Astropy, not Rust Wcs methods, generates coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetGrid {
    pub ra: f64,
    pub dec: f64,
    pub shape: Shape,
    pub scale_deg: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeWindow {
    pub shape: Shape,
    /// 1-based FITS origin of this bounded native input.
    pub origin: [i32; 2],
}

#[derive(Clone)]
pub struct Geometry {
    data: Pinned<GeometryData>,
}

struct GeometryData {
    tile: Tile,
    window: NativeWindow,
    native_wcs: Wcs,
    target: TargetGrid,
    indices: Vec<i64>,
}

impl Geometry {
    pub fn required_bytes(tile: Tile, native: &Wcs) -> MathResult<u64> {
        tile.shape
            .bytes(8)?
            .checked_add(coefficient_bytes(native)?)
            .and_then(|n| n.checked_add(256))
            .ok_or(MathError::Overflow)
    }

    /// Only a library backend should construct this. -1 is outside native
    /// support, not a failed/non-converged WCS solution.
    pub fn from_library(
        tile: Tile,
        window: NativeWindow,
        native_wcs: Wcs,
        target: TargetGrid,
        indices: Vec<i64>,
        lease: MathLease,
    ) -> MathResult<Self> {
        tile.validate()?;
        let native_len = i64::try_from(window.shape.len()?).map_err(|_| MathError::Overflow)?;
        if indices.len() != tile.shape.len()?
            || indices.iter().any(|i| *i < -1 || *i >= native_len)
            || lease.bytes() < Self::required_bytes(tile, &native_wcs)?
        {
            return Err(MathError::Internal(
                "library geometry length, index or reservation".into(),
            ));
        }
        Ok(Self {
            data: Pinned::new(
                GeometryData {
                    tile,
                    window,
                    native_wcs,
                    target,
                    indices,
                },
                lease,
            ),
        })
    }

    pub fn indices(&self) -> &[i64] {
        &self.data.get().indices
    }

    pub fn tile(&self) -> Tile {
        self.data.get().tile
    }

    pub fn window(&self) -> NativeWindow {
        self.data.get().window
    }

    pub fn native_wcs(&self) -> &Wcs {
        &self.data.get().native_wcs
    }

    pub fn target(&self) -> TargetGrid {
        self.data.get().target
    }
}

/// Mission policy is explicit. Unknown flag bits remain defects.
#[derive(Clone, Copy, Debug)]
pub enum MaskPolicy {
    None,
    Composite,
    Delta,
    Spectrum,
}

#[derive(Clone, Debug)]
pub enum Product {
    /// Physical f32 values for Color/grey. No unused variance/count cube.
    Values,
    /// Inverse-variance image coadd with variance and saturating u16 counts.
    Science,
    /// Specific depth, strict flags and non-narrowed u64 count sidecars.
    Depth { band: u16 },
    /// Equal-weight finished-band means; every selected band is required.
    CompleteBands { bands: Vec<u16> },
}

pub struct NativePlanes {
    pub window: NativeWindow,
    pub wcs: Wcs,
    pub image: Vec<f32>,
    pub variance: Vec<f32>,
    pub zodi: Vec<f32>,
    pub flags: Vec<u32>,
    /// Native mission band classification; u16::MAX denotes no band.
    pub bands: Vec<u16>,
    /// Native decoder/policy validity of all required companion planes.
    /// Undefined FLAGS or band classification must not become finite success.
    pub valid: Vec<bool>,
}

#[derive(Clone)]
pub struct NativeInput {
    planes: Pinned<NativePlanes>,
}

impl NativeInput {
    /// Copy only the admitted native window. The original cache is independent:
    /// if a caller retains it, that cache must keep its own reservation.
    // Co-registered typed planes are validated and charged together at this boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn copy_from(
        budget: &WorkBudget,
        max_pixels: usize,
        window: NativeWindow,
        wcs: &Wcs,
        image: &[f32],
        variance: &[f32],
        zodi: Option<&[f32]>,
        flags: &[u32],
        bands: &[u16],
        valid: &[bool],
    ) -> MathResult<Self> {
        let n = window.shape.len()?;
        if n > max_pixels
            || image.len() != n
            || variance.len() != n
            || flags.len() != n
            || bands.len() != n
            || valid.len() != n
            || zodi.is_some_and(|v| v.len() != n)
            || bands.iter().any(|b| *b >= 102 && *b != u16::MAX)
        {
            return Err(MathError::Input(
                "native plane length, band or configured window limit".into(),
            ));
        }
        let bytes = window
            .shape
            .bytes(19)?
            .checked_add(coefficient_bytes(wcs)?)
            .and_then(|n| n.checked_add(256))
            .ok_or(MathError::Overflow)?;
        let lease = MathLease::reserve(budget, bytes)?;
        Ok(Self {
            planes: Pinned::new(
                NativePlanes {
                    window,
                    wcs: wcs.clone(),
                    image: image.to_vec(),
                    variance: variance.to_vec(),
                    zodi: zodi.map_or_else(|| vec![0.0; n], <[f32]>::to_vec),
                    flags: flags.to_vec(),
                    bands: bands.to_vec(),
                    valid: valid.to_vec(),
                },
                lease,
            ),
        })
    }

    /// Convert a decoded [`Cutout`] into an admitted [`NativeInput`].
    /// Missing ZODI is an explicit caller decision (`require_zodi`), never
    /// silently zeroed unless the caller allows it.
    pub fn from_cutout(
        budget: &WorkBudget,
        max_pixels: usize,
        cutout: &Cutout,
        require_zodi: bool,
    ) -> MathResult<Self> {
        let zodi = match (&cutout.zodi, require_zodi) {
            (Some(zodi), _) => Some(zodi.data.as_slice()),
            (None, false) => None,
            (None, true) => return Err(MathError::Input("cutout is missing ZODI plane".into())),
        };
        let width = cutout.image.width as usize;
        let height = cutout.image.height as usize;
        let n = width
            .checked_mul(height)
            .ok_or(MathError::Overflow)?;
        let mut bands = Vec::new();
        bands.try_reserve_exact(n).map_err(|_| MathError::Overflow)?;
        let (x0, y0) = cutout.origin_fits;
        for y in 0..height {
            let fy = y0 as f64 + y as f64;
            for x in 0..width {
                let fx = x0 as f64 + x as f64;
                let band = cutout
                    .wavelength
                    .sample(fx, fy)
                    .and_then(|(w, _)| band_of(w))
                    .unwrap_or(u16::MAX);
                bands.push(band);
            }
        }
        let valid_storage;
        let valid: &[bool] = match &cutout.flags_valid {
            Some(v) => v.as_slice(),
            None => {
                valid_storage = vec![true; n];
                &valid_storage
            }
        };
        Self::copy_from(
            budget,
            max_pixels,
            NativeWindow {
                shape: Shape {
                    width: cutout.image.width,
                    height: cutout.image.height,
                },
                origin: [cutout.origin_fits.0, cutout.origin_fits.1],
            },
            &cutout.wcs,
            &cutout.image.data,
            &cutout.variance.data,
            zodi,
            &cutout.flags.data,
            &bands,
            valid,
        )
    }

    pub fn planes(&self) -> &NativePlanes {
        self.planes.get()
    }
}

pub enum Counts {
    U16(Vec<u16>),
    U64(Vec<u64>),
}

pub enum Values {
    F32(Vec<f32>),
    /// Specific depth is not rounded until the completed-band image finalizes.
    F64(Vec<f64>),
}

impl Values {
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        match self {
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
    }
}

pub struct OutputPlanes {
    pub value: Values,
    pub variance: Option<Values>,
    pub counts: Option<Counts>,
}

#[derive(Clone)]
pub struct TileOutput {
    data: Pinned<OutputData>,
}

struct OutputData {
    tile: Tile,
    product: Product,
    planes: OutputPlanes,
}

impl TileOutput {
    pub fn from_library(
        tile: Tile,
        product: Product,
        planes: OutputPlanes,
        lease: MathLease,
    ) -> MathResult<Self> {
        tile.validate()?;
        let n = tile.shape.len()?;
        let (variance, counts, bytes) = match product {
            Product::Values => (false, 0, 4),
            Product::Science => (true, 16, 10),
            Product::Depth { .. } => (true, 64, 24),
            Product::CompleteBands { .. } => (true, 0, 8),
        };
        let count_matches = match &planes.counts {
            None => counts == 0,
            Some(Counts::U16(v)) => counts == 16 && v.len() == n,
            Some(Counts::U64(v)) => counts == 64 && v.len() == n,
        };
        let depth = matches!(product, Product::Depth { .. });
        let dtype_matches =
            |v: &Values| matches!((v, depth), (Values::F64(_), true) | (Values::F32(_), false));
        if planes.value.len() != n
            || !dtype_matches(&planes.value)
            || planes.variance.is_some() != variance
            || planes.variance.as_ref().is_some_and(|v| v.len() != n)
            || planes.variance.as_ref().is_some_and(|v| !dtype_matches(v))
            || !count_matches
            || lease.bytes() < tile.shape.bytes(bytes)?
        {
            return Err(MathError::Internal(
                "library output planes or reservation".into(),
            ));
        }
        Ok(Self {
            data: Pinned::new(
                OutputData {
                    tile,
                    product,
                    planes,
                },
                lease,
            ),
        })
    }

    pub fn planes(&self) -> &OutputPlanes {
        &self.data.get().planes
    }

    pub fn tile(&self) -> Tile {
        self.data.get().tile
    }

    pub fn product(&self) -> &Product {
        &self.data.get().product
    }
}

fn coefficient_bytes(wcs: &Wcs) -> MathResult<u64> {
    [&wcs.sip.a, &wcs.sip.b]
        .into_iter()
        .chain(wcs.sip.ap.iter())
        .chain(wcs.sip.bp.iter())
        .try_fold(0u64, |sum, poly| {
            sum.checked_add(poly.coefficients().len() as u64)
                .ok_or(MathError::Overflow)
        })?
        .checked_mul(8)
        .ok_or(MathError::Overflow)
}

/// One exclusive tile accumulator. Creating and dropping it never touches the
/// library runtime: it owns only its charged running sums, so progressive and
/// Specific callers may keep many sessions alive across member order. Each
/// library call takes a worker slot and scratch charge for its own duration;
/// run those calls in a blocking worker (`spawn_math`), never on an async
/// thread. Do not share a mutable session behind a cache/progress lock.
///
/// A call that observes cancellation or a stale generation after the library
/// returned reports an error even though its exposure was already added.
/// Discard a session after `Cancelled` or `Stale`; never retry into it.
pub trait MathSession: Send {
    /// Charged bytes of the persistent running state, held until drop.
    fn state_bytes(&self) -> u64;
    fn control(&self) -> &MathControl;
    fn geometry(
        &mut self,
        native: &Wcs,
        window: NativeWindow,
        target: TargetGrid,
    ) -> MathResult<Geometry>;
    fn add(
        &mut self,
        order: u64,
        input: &NativeInput,
        geometry: &Geometry,
        bits: FlagBits,
        mask: MaskPolicy,
        bands: &[u16],
    ) -> MathResult<()>;
    fn add_band(&mut self, depth: &TileOutput) -> MathResult<()>;
    fn snapshot(&mut self) -> MathResult<TileOutput>;
    fn finish(self: Box<Self>) -> MathResult<TileOutput>;
    fn difference(&mut self, plus: &TileOutput, minus: &TileOutput) -> MathResult<TileOutput>;

    /// No finite science fallback for still-undecided spatial/clipping operators.
    fn advanced_operator(&mut self) -> MathResult<()> {
        Err(MathError::Unsupported(
            "advanced sky operator and composed uncertainty",
        ))
    }
}

pub struct SpectrumSamples {
    pub flux: Vec<f64>,
    pub variance: Vec<f64>,
    pub band: Vec<u16>,
    pub rejected: Vec<bool>,
}

#[derive(Clone)]
pub struct SpectrumInput {
    samples: Pinned<SpectrumSamples>,
}

impl SpectrumInput {
    pub fn copy_from(
        budget: &WorkBudget,
        max_samples: usize,
        flux: &[f64],
        variance: &[f64],
        band: &[u16],
        rejected: &[bool],
    ) -> MathResult<Self> {
        let n = flux.len();
        if n > max_samples
            || variance.len() != n
            || band.len() != n
            || rejected.len() != n
            || band.iter().any(|b| *b >= 102 && *b != u16::MAX)
        {
            return Err(MathError::Input(
                "spectrum length, band or configured limit".into(),
            ));
        }
        let lease = MathLease::reserve(
            budget,
            (n as u64).checked_mul(19).ok_or(MathError::Overflow)?,
        )?;
        Ok(Self {
            samples: Pinned::new(
                SpectrumSamples {
                    flux: flux.to_vec(),
                    variance: variance.to_vec(),
                    band: band.to_vec(),
                    rejected: rejected.to_vec(),
                },
                lease,
            ),
        })
    }

    pub fn samples(&self) -> &SpectrumSamples {
        self.samples.get()
    }
}

/// Metadata/flag unions, loose points and invalid companion presentation stay
/// typed caller policy. Indices preserve last-valid/first-invalid wavelength.
pub struct SpectrumPlanes {
    pub value: Vec<f64>,
    pub error: Vec<f64>,
    pub counts: Vec<u64>,
    pub invalid_counts: Vec<u64>,
    pub last_valid: Vec<i64>,
    pub first_invalid: Vec<i64>,
    pub accepted: Vec<bool>,
}

pub trait ImageMath: Send + Sync {
    fn session(
        &self,
        control: MathControl,
        tile: Tile,
        product: Product,
    ) -> MathResult<Box<dyn MathSession>>;
    fn spectrum(
        &self,
        control: &MathControl,
        input: &SpectrumInput,
    ) -> MathResult<Pinned<SpectrumPlanes>>;
}

/// Dropping the requester cancels publication, not the started blocking worker.
/// The closure, not this handle, owns the session/input/resource lifetimes.
pub struct MathTask<T> {
    handle: Option<tokio::task::JoinHandle<MathResult<T>>>,
    control: MathControl,
    completed: bool,
    exit: tokio::sync::watch::Receiver<bool>,
}

impl<T> Drop for MathTask<T> {
    fn drop(&mut self) {
        if !self.completed {
            self.control.cancel();
        }
    }
}

impl<T> MathTask<T> {
    /// Independent supervisor evidence of actual worker/session exit. A finished
    /// result may still be pinned in the join handle or caller/cache afterwards.
    pub fn completion(&self) -> tokio::sync::watch::Receiver<bool> {
        self.exit.clone()
    }

    pub async fn wait(mut self) -> MathResult<T> {
        let result = self
            .handle
            .take()
            .expect("wait only consumes once")
            .await
            .map_err(|error| {
                MathError::Runtime(format!("math worker exited unexpectedly: {error}"))
            })?;
        self.control.check()?;
        self.completed = true;
        result
    }
}

/// Run library calls on a session in a blocking worker, never an async thread.
pub fn spawn_math<T: Send + 'static>(
    session: Box<dyn MathSession>,
    operation: impl FnOnce(Box<dyn MathSession>) -> MathResult<T> + Send + 'static,
) -> MathResult<MathTask<T>> {
    let control = session.control().clone();
    spawn_owned(control, session, operation)
}

/// The spectrum reducer takes the GIL for its ordered sample loop, so it also
/// runs in a blocking worker.
pub fn spawn_spectrum(
    math: Arc<dyn ImageMath>,
    control: MathControl,
    input: SpectrumInput,
) -> MathResult<MathTask<Pinned<SpectrumPlanes>>> {
    let worker_control = control.clone();
    spawn_owned(control, (math, input), move |(math, input)| {
        math.spectrum(&worker_control, &input)
    })
}

fn spawn_owned<S: Send + 'static, T: Send + 'static>(
    control: MathControl,
    owned: S,
    operation: impl FnOnce(S) -> MathResult<T> + Send + 'static,
) -> MathResult<MathTask<T>> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| MathError::Runtime("blocking math needs a captured Tokio runtime".into()))?;
    control.check()?;
    let worker_control = control.clone();
    let (exit_tx, exit) = tokio::sync::watch::channel(false);
    struct Worker<S, F> {
        owned: Option<S>,
        operation: Option<F>,
        exit: tokio::sync::watch::Sender<bool>,
    }
    impl<S, F> Drop for Worker<S, F> {
        fn drop(&mut self) {
            drop(self.operation.take());
            drop(self.owned.take());
            let _ = self.exit.send(true);
        }
    }
    let handle = runtime.spawn_blocking(move || {
        let mut worker = Worker {
            owned: Some(owned),
            operation: Some(operation),
            exit: exit_tx,
        };
        worker_control.check()?;
        let operation = worker.operation.take().expect("operation runs once");
        let output = operation(worker.owned.take().expect("owned until operation takes it"))?;
        worker_control.check()?;
        Ok(output)
    });
    Ok(MathTask {
        handle: Some(handle),
        control,
        completed: false,
        exit,
    })
}
