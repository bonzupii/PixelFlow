//! PixelFlow core: frame representation, pixel formats, and the
//! processing pipeline interface. No platform code and no I/O live
//! here; every other crate depends on this one.

pub mod format;
pub mod frame;
pub mod pipeline;

pub use format::PixelFormat;
pub use frame::{Frame, FrameError};
pub use pipeline::{ProcessError, Processor};
