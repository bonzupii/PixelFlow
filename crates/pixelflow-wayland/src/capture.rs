//! The Wayland capture backend: a portal session plus a `PipeWire`
//! stream, behind the pixelflow-capture trait.

use crate::pipewire::{PipeWireCapture, StreamTarget};
use crate::{PortalSession, WaylandError};
use pixelflow_capture::{CaptureBackend, CaptureConfig, CaptureError};
use pixelflow_core::Frame;

/// Wayland capture: portal session for the source, `PipeWire` stream
/// for the frames. Created empty; `initialize` opens both.
#[derive(Default)]
pub struct WaylandCapture {
    portal: Option<PortalSession>,
    pipewire: Option<PipeWireCapture>,
}

impl WaylandCapture {
    /// An uninitialized backend.
    #[must_use]
    pub fn new() -> WaylandCapture {
        WaylandCapture::default()
    }
}

/// Map a `PipeWire` start failure to the capture error the caller sees.
fn start_error(error: WaylandError) -> CaptureError {
    match error {
        WaylandError::PipeWire(msg) => CaptureError::StreamFailed(msg),
        other => CaptureError::from(other),
    }
}

impl CaptureBackend for WaylandCapture {
    /// Open the portal session and start the `PipeWire` stream. Blocks
    /// until the user answers the compositor dialog or the request
    /// fails.
    ///
    /// # Errors
    ///
    /// Returns `SessionFailed` when the portal session cannot be
    /// created or started, `UnsupportedFormat` when the negotiated
    /// format is not convertible, and `StreamFailed` when the
    /// `PipeWire` stream fails to start.
    fn initialize(&mut self, config: &CaptureConfig) -> Result<(), CaptureError> {
        if self.portal.is_some() {
            return Err(CaptureError::SessionFailed(
                "capture is already initialized".to_string(),
            ));
        }
        let mut portal = PortalSession::open(config.source).map_err(CaptureError::from)?;
        let Some(fd) = portal.take_fd() else {
            // The session owns the portal session until it is
            // closed, so a missing fd still closes it instead of
            // leaking it on the portal side.
            return Err(match portal.close() {
                Ok(()) => {
                    CaptureError::SessionFailed("portal session has no PipeWire remote".to_string())
                }
                Err(close_failure) => CaptureError::SessionFailed(format!(
                    "portal session has no PipeWire remote; portal close also failed: {close_failure}"
                )),
            });
        };
        let node_id = portal.node_id;
        let target = StreamTarget::from_portal(portal.serial, node_id);
        let pipewire = match PipeWireCapture::start(fd, target) {
            Ok(pipewire) => pipewire,
            Err(start_failure) => {
                return Err(match portal.close() {
                    Ok(()) => start_error(start_failure),
                    Err(close_failure) => CaptureError::SessionFailed(format!(
                        "pipewire failed to start: {start_failure}; portal close also failed: {close_failure}"
                    )),
                });
            }
        };
        self.portal = Some(portal);
        self.pipewire = Some(pipewire);
        Ok(())
    }

    /// Take the next frame, waiting briefly for one to arrive.
    ///
    /// # Errors
    ///
    /// Returns `SessionFailed` when capture never initialized, the
    /// recorded stream error when one exists, and `Disconnected`
    /// when the stream ended.
    fn capture_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
        match self.pipewire.as_mut() {
            Some(pipewire) => pipewire.next_frame().map_err(CaptureError::from),
            None => Err(CaptureError::SessionFailed(
                "capture is not initialized".to_string(),
            )),
        }
    }

    /// Stop the stream and close the portal session. Safe to call on
    /// a backend that never initialized. Both halves are stopped even
    /// when the first reports an error; the first error is returned.
    ///
    /// # Errors
    ///
    /// Returns the first error from either half, if any.
    fn shutdown(&mut self) -> Result<(), CaptureError> {
        let mut error: Option<CaptureError> = None;
        if let Some(mut pipewire) = self.pipewire.take()
            && let Err(e) = pipewire.stop()
        {
            error = Some(CaptureError::from(e));
        }
        if let Some(portal) = self.portal.take()
            && let Err(e) = portal.close()
            && error.is_none()
        {
            error = Some(CaptureError::from(e));
        }
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Frames delivered by the `PipeWire` stream so far.
    fn frames_seen(&self) -> u64 {
        match self.pipewire.as_ref() {
            Some(pipewire) => pipewire.frames_seen(),
            None => 0,
        }
    }

    /// Frames dropped by the `PipeWire` stream so far.
    fn frames_dropped(&self) -> u64 {
        match self.pipewire.as_ref() {
            Some(pipewire) => pipewire.frames_dropped(),
            None => 0,
        }
    }
}

impl Drop for WaylandCapture {
    fn drop(&mut self) {
        // Best effort: errors have nowhere to go during drop.
        if let Ok(()) = self.shutdown() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelflow_capture::SourceKind;

    #[test]
    fn initialize_fails_without_a_portal() {
        let mut backend = WaylandCapture::new();
        let config = CaptureConfig::new(SourceKind::Window);
        let result = backend.initialize(&config);
        assert!(
            matches!(result, Err(CaptureError::SessionFailed(_))),
            "expected SessionFailed, got {result:?}"
        );
        assert!(
            backend.shutdown().is_ok(),
            "shutdown of a failed backend succeeds"
        );
    }

    #[test]
    fn failed_initialize_leaves_the_backend_reusable() {
        // The monitor fallback calls initialize a second time on
        // the same backend after the window attempt fails. That
        // only opens a fresh session when the failed attempt left
        // the backend uninitialized: a second call must reach the
        // portal again instead of failing as already initialized.
        let mut backend = WaylandCapture::new();
        let window = CaptureConfig::new(SourceKind::Window);
        let first = backend.initialize(&window);
        assert!(first.is_err(), "expected the window attempt to fail");
        let monitor = CaptureConfig::new(SourceKind::Monitor);
        let second = backend.initialize(&monitor);
        assert!(second.is_err(), "expected the monitor attempt to fail");
        assert!(
            !matches!(&second, Err(e) if e.to_string().contains("already initialized")),
            "the second attempt must not fail as already initialized, got {second:?}"
        );
        assert!(
            backend.shutdown().is_ok(),
            "shutdown of a twice-failed backend succeeds"
        );
    }

    #[test]
    fn capture_frame_before_initialize_is_an_error() {
        let mut backend = WaylandCapture::new();
        let result = backend.capture_frame();
        assert!(matches!(result, Err(CaptureError::SessionFailed(_))));
    }
}
