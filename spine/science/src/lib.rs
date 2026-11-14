//! Concrete Astropy/NumPy implementation of `spearv_core::math::ImageMath`.
//! No Rust image reducer or product FITS decoder is hidden here.
mod runtime;

pub use runtime::{PythonMath, RuntimeConfig};
pub use spearv_core::math::{spawn_math, spawn_spectrum, MathTask};
