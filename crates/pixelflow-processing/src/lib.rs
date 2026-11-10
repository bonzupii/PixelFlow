//! `PixelFlow` image processing algorithms.
//!
//! Each algorithm implements [`pixelflow_core::Processor`]: one frame
//! in, one frame out, an integer scale factor. Algorithms are pure
//! functions of the input frame; they know nothing about capture,
//! presentation, or files.

pub mod algorithms;
pub mod region_analysis;
pub mod registry;

pub use algorithms::bicubic::{Bicubic, BicubicAlgorithm};
pub use algorithms::bilinear::{Bilinear, BilinearAlgorithm};
pub use algorithms::contour::{Contour, ContourAlgorithm, LimitedContour};
pub use algorithms::lanczos3::{Lanczos3, Lanczos3Algorithm};
pub use algorithms::nearest::{Nearest, NearestAlgorithm};
pub use algorithms::pixelflow::{PixelFlow, PixelFlowAlgorithm};
pub use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin,
    ConfigValue, ExecutionBackend, ParameterDescriptor, ParameterKind,
};
pub use registry::AlgorithmRegistry;
