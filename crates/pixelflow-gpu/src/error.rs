//! What can go wrong while running GPU work, and the error a
//! GPU node reports through the graph.

use pixelflow_core::ProcessError;

/// What can go wrong while selecting an adapter, allocating a
/// GPU image, or running a GPU compute node. Every variant
/// names the failure; a GPU node converts this into
/// [`ProcessError::Internal`] and never falls back to CPU, so
/// a GPU failure is loud at the call site instead of a silent
/// change of execution backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuError {
    /// No adapter was enumerated on this machine: no discrete
    /// GPU, no integrated graphics, and no CPU fallback
    /// adapter answered.
    NoAdapter,
    /// The logical device could not be created on the
    /// selected adapter; the message is the wgpu error.
    DeviceRequest(String),
    /// The compute shader or its pipeline failed wgpu
    /// validation; the message is the validation error.
    Shader(String),
    /// The CPU frame could not be uploaded to a texture; the
    /// message says why.
    Upload(String),
    /// The GPU image could not be downloaded to a CPU frame;
    /// the message says why.
    Download(String),
    /// A compute dispatch failed; the message says why.
    Compute(String),
    /// The staging buffer mapping never completed: the
    /// mapping callback was not delivered.
    MapFailed,
    /// Waiting for the device to finish submitted work
    /// failed; the message is the wgpu poll error.
    PollFailed(String),
    /// A size computation overflowed: the requested image is
    /// too large to size, so no allocation is attempted. The
    /// string names the quantity that overflowed.
    SizeOverflow(&'static str),
    /// The requested scale factor has no GPU implementation.
    UnsupportedScale {
        /// The scale factor that was requested.
        scale: u32,
    },
    /// The input frame is not in a format this GPU node
    /// accepts; the string is the frame format.
    UnsupportedFormat {
        /// The frame format that was supplied, as a string.
        format: String,
    },
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::NoAdapter => write!(f, "no GPU adapter found on this machine"),
            GpuError::DeviceRequest(message) => {
                write!(f, "could not create a GPU device: {message}")
            }
            GpuError::Shader(message) => write!(f, "GPU shader failed validation: {message}"),
            GpuError::Upload(message) => write!(f, "GPU upload failed: {message}"),
            GpuError::Download(message) => write!(f, "GPU download failed: {message}"),
            GpuError::Compute(message) => write!(f, "GPU compute failed: {message}"),
            GpuError::MapFailed => write!(f, "GPU staging buffer mapping never completed"),
            GpuError::PollFailed(message) => write!(f, "waiting for the GPU failed: {message}"),
            GpuError::SizeOverflow(what) => write!(f, "GPU size overflow while sizing {what}"),
            GpuError::UnsupportedScale { scale } => {
                write!(f, "no GPU implementation for scale {scale}")
            }
            GpuError::UnsupportedFormat { format } => {
                write!(f, "GPU node does not accept {format} frames")
            }
        }
    }
}

impl std::error::Error for GpuError {}

impl From<GpuError> for ProcessError {
    /// A GPU failure becomes an internal processing error
    /// carrying the GPU reason. The conversion never
    /// substitutes CPU execution: the caller sees the failure.
    fn from(error: GpuError) -> Self {
        ProcessError::Internal(error.to_string())
    }
}
