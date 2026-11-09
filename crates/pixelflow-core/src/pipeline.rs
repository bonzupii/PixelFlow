//! Processing pipeline interface: the contract every upscaling
//! algorithm implements, and the error type it reports with.
//!
//! The interface is deliberately narrow: one frame in, one frame out,
//! an integer scale factor. Capture backends, presentation, and the
//! CLI all program against this trait, never against an algorithm.

use crate::frame::Frame;

/// An image processing algorithm: nearest neighbour, upscale20, and
/// whatever comes later all implement this trait.
pub trait Processor {
    /// Short stable name, e.g. "upscale20" or "nearest". Used in
    /// configuration and in CLI output.
    fn name(&self) -> &'static str;

    /// Integer scale factors this algorithm supports, e.g. [2, 4].
    fn supported_scales(&self) -> &[u32];

    /// Process one frame. The input frame's format must be RGB8; use
    /// [`Frame::to_rgb8`] first for anything else. The output frame
    /// is RGB8 at `width * scale` by `height * scale`.
    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError>;
}

/// What can go wrong while processing a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    /// The requested scale factor is not supported by this algorithm.
    UnsupportedScale { algorithm: &'static str, scale: u32 },
    /// The input frame is not in a format this algorithm accepts.
    UnsupportedFormat { algorithm: &'static str, format: String },
    /// The algorithm failed internally; the message says why.
    Internal(String),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::UnsupportedScale { algorithm, scale } => {
                write!(f, "{algorithm} does not support scale {scale}")
            }
            ProcessError::UnsupportedFormat { algorithm, format } => {
                write!(f, "{algorithm} does not accept {format} frames")
            }
            ProcessError::Internal(msg) => write!(f, "processing failed: {msg}"),
        }
    }
}

impl std::error::Error for ProcessError {}
