//! Wayland capture backend: XDG Desktop Portal `ScreenCast` for the
//! session, `PipeWire` for the video stream. Implements the
//! pixelflow-capture trait; the portal and `PipeWire` details stay in
//! this crate and never leak into core or processing.

mod capture;
mod convert;
mod error;
mod pipewire;
mod portal;

pub use capture::WaylandCapture;
pub use convert::{SUPPORTED_FORMATS, is_supported, packed_to_frame};
pub use error::WaylandError;
pub use portal::PortalSession;
