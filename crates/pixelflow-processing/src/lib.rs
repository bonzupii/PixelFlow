//! `PixelFlow` image processing algorithms.
//!
//! Each algorithm implements [`pixelflow_core::Processor`]: one frame
//! in, one frame out, an integer scale factor. Algorithms are pure
//! functions of the input frame; they know nothing about capture,
//! presentation, or files.

pub mod algorithms;

pub use algorithms::nearest::Nearest;
pub use algorithms::contour::Contour;
