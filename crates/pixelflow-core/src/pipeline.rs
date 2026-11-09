//! Processing pipeline interface: the contract every upscaling
//! algorithm implements, and the error type it reports with.
//!
//! The interface is deliberately narrow: one frame in, one frame out,
//! an integer scale factor. Capture backends, presentation, and the
//! CLI all program against this trait, never against an algorithm.

use crate::frame::Frame;

/// What row layouts an algorithm accepts. Nodes declare this;
/// the graph executor reads the declaration and packs a strided
/// frame before a tight-only node, and the tight-only node
/// itself packs at its entry point until the executor exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrideSupport {
    /// The node reads pixels through the stride-aware accessors
    /// and accepts any stride.
    Any,
    /// The node indexes a tightly packed buffer directly and
    /// needs its input packed first. See [`Frame::packed`].
    TightOnly,
}

/// An image processing algorithm: nearest neighbour, contour, and
/// whatever comes later all implement this trait.
pub trait Processor {
    /// Short stable name, e.g. "contour" or "nearest". Used in
    /// configuration and in CLI output.
    fn name(&self) -> &'static str;

    /// Integer scale factors this algorithm supports, e.g. [2, 4].
    fn supported_scales(&self) -> &[u32];

    /// What row layouts this algorithm accepts. The default is
    /// [`StrideSupport::Any`]; a node that indexes a tight
    /// buffer directly overrides this with
    /// [`StrideSupport::TightOnly`] and packs its input at its
    /// entry point.
    fn stride_support(&self) -> StrideSupport {
        StrideSupport::Any
    }

    /// Process one frame. The input frame's format must be RGB8; use
    /// [`Frame::to_rgb8`] first for anything else. The output frame
    /// is RGB8 at `width * scale` by `height * scale`.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError`] for an unsupported scale factor, an
    /// unsupported input format, or an internal algorithm failure.
    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError>;
}

/// What can go wrong while processing a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    /// The requested scale factor is not supported by this algorithm.
    UnsupportedScale {
        /// Name of the algorithm that rejected the scale factor.
        algorithm: &'static str,
        /// The scale factor that was requested.
        scale: u32,
    },
    /// The input frame is not in a format this algorithm accepts.
    UnsupportedFormat {
        /// Name of the algorithm that rejected the format.
        algorithm: &'static str,
        /// The frame format that was supplied, as a string.
        format: String,
    },
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
