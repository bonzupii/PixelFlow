//! `PixelFlow` image processing algorithms.
//!
//! Each algorithm implements [`pixelflow_core::Processor`]: one frame
//! in, one frame out, an integer scale factor. Algorithms are pure
//! functions of the input frame; they know nothing about capture,
//! presentation, or files.

pub mod algorithms;
pub mod region_analysis;

pub use algorithms::contour::{Contour, LimitedContour};
pub use algorithms::nearest::Nearest;
