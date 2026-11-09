//! The capture backend trait: the contract every platform backend
//! implements, and the CLI programs against.

use crate::{CaptureConfig, CaptureError};
use pixelflow_core::Frame;

/// A video source: a window or a monitor, delivered as frames.
pub trait CaptureBackend {
    /// Open the session and start the stream. This is where the
    /// compositor asks the user for permission and source selection,
    /// so it blocks until the user answers or the request fails.
    fn initialize(&mut self, config: &CaptureConfig) -> Result<(), CaptureError>;

    /// Take the next frame, waiting briefly for one to arrive.
    /// `Ok(None)` means no frame arrived in that wait, not end of
    /// stream; `Err(Disconnected)` means the stream ended.
    fn capture_frame(&mut self) -> Result<Option<Frame>, CaptureError>;

    /// Stop the stream and close the session. Safe to call on a
    /// backend that never initialized.
    fn shutdown(&mut self) -> Result<(), CaptureError>;
}
