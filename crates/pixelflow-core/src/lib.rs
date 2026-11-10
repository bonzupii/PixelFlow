//! `PixelFlow` core: frame representation, pixel formats, and the
//! processing pipeline interface. No platform code and no I/O live
//! here; every other crate depends on this one.

pub mod algorithm;
pub mod format;
pub mod frame;
pub mod frame_slot;
pub mod graph;
pub mod incremental;
pub mod pipeline;

pub use algorithm::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin,
    ConfigValue, ExecutionBackend, ParameterDescriptor, ParameterKind, validate_config,
};
pub use format::PixelFormat;
pub use frame::{Frame, FrameError, GpuResource};
pub use frame_slot::FrameSlot;
pub use graph::{
    Graph, GraphBuilder, GraphError, Node, NodeInputs, NodeOutputs, ProcessorNode, Source,
    SpatialDependency, TemporalDependency, TileRouting,
};
pub use incremental::{DirtySet, TILE_SIZE, TileGrid};
pub use pipeline::{ProcessError, Processor, StrideSupport};
