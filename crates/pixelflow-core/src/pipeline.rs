//! Processing pipeline interface: the contract every upscaling
//! algorithm implements, and the error type it reports with.
//!
//! The interface is deliberately narrow: one frame in, one frame out,
//! an integer scale factor. Capture backends, presentation, and the
//! CLI all program against this trait, never against an algorithm.

use crate::algorithm::{
    AlgorithmConfig, AlgorithmError, ExecutionBackend, ParameterDescriptor, validate_config,
};
use crate::format::PixelFormat;
use crate::frame::Frame;
use crate::graph::{SpatialDependency, TemporalDependency};

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
///
/// The trait carries the stable algorithm contract beyond the
/// `process` call itself: the id, the description, the formats
/// and scales, the validated parameters, the execution backends,
/// the dependencies the graph executor reads, and the resource
/// and determinism notes. The defaults describe a single-stage,
/// CPU-only, deterministic RGB8 algorithm with no parameters;
/// an algorithm overrides only what differs. The registry reads
/// these methods into an [`AlgorithmDescriptor`] (see
/// `crate::algorithm`), so the description and the
/// implementation cannot drift apart.
pub trait Processor {
    /// Short stable name, e.g. "contour" or "nearest". Used in
    /// configuration and in CLI output.
    fn name(&self) -> &'static str;

    /// Stable algorithm id. The registry lookup key and the
    /// command-line name; the default is the processor name.
    fn algorithm_id(&self) -> &'static str {
        self.name()
    }

    /// What the algorithm does, in one sentence. Shown by the
    /// CLI when it lists algorithms.
    fn description(&self) -> &'static str {
        ""
    }

    /// Algorithm version.
    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// Input pixel formats the algorithm accepts. The default
    /// is RGB8 only; `process` rejects anything else.
    fn input_formats(&self) -> &[PixelFormat] {
        &[PixelFormat::Rgb8]
    }

    /// Output pixel formats the algorithm produces. The default
    /// is RGB8.
    fn output_formats(&self) -> &[PixelFormat] {
        &[PixelFormat::Rgb8]
    }

    /// Execution backends the algorithm can run on. The default
    /// is CPU only.
    fn backends(&self) -> &[ExecutionBackend] {
        &[ExecutionBackend::Cpu]
    }

    /// What input area one output pixel depends on. The default
    /// is [`SpatialDependency::Global`]: the honest declaration
    /// for an algorithm that has not stated a tighter one.
    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Global
    }

    /// What history the algorithm keeps between frames. The
    /// default is stateless.
    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    /// Validated algorithm-specific parameters. The default is
    /// no parameters: any set value fails validation.
    fn parameters(&self) -> &[ParameterDescriptor] {
        &[]
    }

    /// Working memory per output pixel, in bytes, when the
    /// working set is a flat per-pixel figure. `None` when the
    /// working set has no such figure — the algorithm's own
    /// budget check states its model instead.
    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        None
    }

    /// Intermediate resources a multi-stage algorithm holds
    /// between its stages, named so a caller can account for
    /// them. Empty for a single-stage algorithm.
    fn intermediate_resources(&self) -> &[&'static str] {
        &[]
    }

    /// True when the same input frame, scale, and configuration
    /// always produce byte-identical output.
    fn deterministic(&self) -> bool {
        true
    }

    /// What determinism means for this algorithm, or why it
    /// does not hold.
    fn determinism_note(&self) -> &'static str {
        "deterministic: the same input frame, scale, and configuration produce byte-identical output"
    }

    /// Graph node stage names, in run order, for a multi-stage
    /// algorithm: one name per node its factory builds. Empty
    /// for a single-stage algorithm, which the descriptor then
    /// names after the algorithm id.
    fn stage_names(&self) -> &[&'static str] {
        &[]
    }

    /// Validate `config` against this algorithm's parameters:
    /// every set parameter must be declared, of the declared
    /// type, and inside its range.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::InvalidConfig`] for an unknown
    /// parameter, a wrongly typed value, or a value outside its
    /// range.
    fn validate_config(&self, config: &AlgorithmConfig) -> Result<(), AlgorithmError> {
        validate_config(self.parameters(), self.algorithm_id(), config)
    }

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

    /// Whether this algorithm can run on a cropped input frame
    /// and produce exactly the corresponding crop of its
    /// whole-frame output: the output for a pixel depends only
    /// on the input pixels the spatial dependency already
    /// covers, never on the frame's size or the pixel's absolute
    /// position. The default is `false`: the executor runs the
    /// algorithm whole-frame and reads dirty tiles from that
    /// output. An algorithm overrides this only when the cropped
    /// run is exact — nearest-neighbour scaling qualifies; an
    /// algorithm with edge handling that clamps to the frame
    /// boundary does not, because a crop would clamp to the
    /// crop boundary instead.
    fn tile_safe(&self) -> bool {
        false
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
    /// The input would need more working memory than the
    /// configured budget allows. This is a property of the
    /// requested output size and the caller's budget, not an
    /// internal failure: the caller set a budget and asked for
    /// an output whose estimated working set exceeds it. The
    /// budget is explicit (see `Contour::with_memory_budget`);
    /// there is no fixed product limit. The estimate and its
    /// model are recorded in `docs/performance.md`.
    MemoryBudgetExceeded {
        /// Name of the algorithm that refused the input.
        algorithm: &'static str,
        /// Pipeline stage that ran the check (e.g. "budget").
        stage: &'static str,
        /// Output pixels requested (`width * height` at the
        /// requested scale).
        output_pixels: u64,
        /// Working set the output would need, in bytes,
        /// estimated from the working-set model.
        estimated_bytes: u64,
        /// The budget, in bytes.
        budget_bytes: u64,
    },
    /// A size computation overflowed: the requested dimensions
    /// need more bytes than the arithmetic can express, so no
    /// allocation is attempted. This is a property of the
    /// requested size, not an internal failure.
    SizeOverflow {
        /// Name of the algorithm that refused the input.
        algorithm: &'static str,
        /// Pipeline stage that ran the check (e.g. "budget").
        stage: &'static str,
        /// Output pixels requested (`width * height` at the
        /// requested scale).
        output_pixels: u64,
    },
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
            ProcessError::MemoryBudgetExceeded {
                algorithm,
                stage,
                output_pixels,
                estimated_bytes,
                budget_bytes,
            } => write!(
                f,
                "{algorithm} ({stage}) refuses {output_pixels} output pixels: estimated working set \
                 {estimated_bytes} bytes exceeds the {budget_bytes} byte budget"
            ),
            ProcessError::SizeOverflow {
                algorithm,
                stage,
                output_pixels,
            } => write!(
                f,
                "{algorithm} ({stage}) cannot size {output_pixels} output pixels: the size arithmetic overflows"
            ),
        }
    }
}

impl std::error::Error for ProcessError {}

/// A boxed processor is itself a processor: every contract
/// method delegates to the boxed value. This is what lets the
/// registry hand a type-erased processor to [`ProcessorNode`]
/// without a match arm per algorithm.
impl Processor for Box<dyn Processor> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn algorithm_id(&self) -> &'static str {
        (**self).algorithm_id()
    }

    fn description(&self) -> &'static str {
        (**self).description()
    }

    fn version(&self) -> &'static str {
        (**self).version()
    }

    fn input_formats(&self) -> &[PixelFormat] {
        (**self).input_formats()
    }

    fn output_formats(&self) -> &[PixelFormat] {
        (**self).output_formats()
    }

    fn backends(&self) -> &[ExecutionBackend] {
        (**self).backends()
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        (**self).spatial_dependency()
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        (**self).temporal_dependency()
    }

    fn parameters(&self) -> &[ParameterDescriptor] {
        (**self).parameters()
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        (**self).working_bytes_per_output_pixel()
    }

    fn intermediate_resources(&self) -> &[&'static str] {
        (**self).intermediate_resources()
    }

    fn deterministic(&self) -> bool {
        (**self).deterministic()
    }

    fn determinism_note(&self) -> &'static str {
        (**self).determinism_note()
    }

    fn stage_names(&self) -> &[&'static str] {
        (**self).stage_names()
    }

    fn validate_config(&self, config: &AlgorithmConfig) -> Result<(), AlgorithmError> {
        (**self).validate_config(config)
    }

    fn supported_scales(&self) -> &[u32] {
        (**self).supported_scales()
    }

    fn stride_support(&self) -> StrideSupport {
        (**self).stride_support()
    }

    fn tile_safe(&self) -> bool {
        (**self).tile_safe()
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        (**self).process(frame, scale)
    }
}
