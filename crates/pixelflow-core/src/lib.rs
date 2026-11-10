//! `PixelFlow` core: frame representation, pixel formats, and the
//! processing pipeline interface. No platform code and no I/O live
//! here; every other crate depends on this one.

pub mod format;
pub mod frame;
pub mod frame_slot;
pub mod pipeline;

pub use format::PixelFormat;
pub use frame::{Frame, FrameError, GpuResource};
pub use frame_slot::FrameSlot;
pub use pipeline::{ProcessError, Processor, StrideSupport};
