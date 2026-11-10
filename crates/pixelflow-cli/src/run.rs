//! The `run` command: launch the target, capture its window, run
//! each frame through the processing pipeline, and present the result
//! in a fullscreen window.
//!
//! The work splits across two threads. The processing thread owns the
//! capture backend and the child process: it pulls frames, processes
//! them, and publishes the newest to a replaceable frame slot. A
//! newer frame replaces one still waiting in the slot, so the
//! presentation window always draws the freshest frame and latency
//! never accumulates. The processing thread wakes the presentation
//! loop through an event-loop proxy each time it publishes, so the
//! loop leaves `ControlFlow::Wait` as soon as a frame is ready. The
//! main thread runs the presentation window, because the windowing
//! system requires the event loop there. Either side ending sets the
//! shared exit flag, and the other side winds down: the processing
//! thread stops capture and kills the child, and the presentation
//! window closes.
//!
//! The target is launched first, then PixelFlow waits briefly for it
//! to start before opening the portal session. The wait is startup
//! grace, not detection: nothing verifies that the source the user
//! picks in the compositor dialog belongs to the launched process,
//! and there is no window list to poll.

use crate::launcher::Launcher;
use pixelflow_capture::{CaptureBackend, CaptureConfig, SourceKind};
use pixelflow_core::FrameSlot;
use pixelflow_present::{PresentLoop, PresentUserEvent};
use pixelflow_wayland::WaylandCapture;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long to wait for the target to start before opening the
/// portal session. Startup grace, not detection.
const WINDOW_GRACE: Duration = Duration::from_secs(2);

/// Options for one `run` invocation.
pub struct RunOptions {
    /// Program to launch.
    pub program: String,
    /// Arguments for the program.
    pub args: Vec<String>,
    /// Window or monitor capture.
    pub source: SourceKind,
    /// Integer scale factor, 2 or 4.
    pub scale: u32,
    /// Windowed preview instead of fullscreen.
    pub windowed: bool,
}

/// Launch the target, capture its window, process frames, and present
/// them until the window closes or the target exits.
///
/// # Errors
///
/// Returns a message when the target cannot launch, the scale is not
/// supported, the portal session cannot open, capture fails, or the
/// presentation window cannot start.
pub fn run(opts: RunOptions) -> Result<(), String> {
    let processor = crate::contour_from_env();
    if !processor.supported_scales().contains(&opts.scale) {
        return Err(format!(
            "scale {} is not supported by {}; supported scales: {:?}",
            opts.scale,
            processor.name(),
            processor.supported_scales()
        ));
    }
    let mut launcher = Launcher::spawn(&opts.program, &opts.args)?;
    std::thread::sleep(WINDOW_GRACE);
    if launcher.exited() {
        return Err(format!("{} exited before capture started", opts.program));
    }
    let mut backend = WaylandCapture::new();
    if let Err(e) = backend.initialize(&CaptureConfig::new(opts.source)) {
        // The child is already gone or cannot be killed; the capture
        // error below is the one to report.
        match launcher.kill() {
            Ok(()) => {}
            Err(_) => {}
        }
        return Err(format!("capture failed to start: {e}"));
    }
    let slot = Arc::new(FrameSlot::new());
    let exit = Arc::new(AtomicBool::new(false));
    let processing_exit = exit.clone();
    let scale = opts.scale;
    let program = opts.program.clone();
    // Build the presentation loop before the worker starts, so the
    // worker has the proxy it wakes the loop with. When no display
    // server exists this fails here, before any frame is produced:
    // stop capture and kill the child before reporting, the same
    // shutdown the worker would have done.
    let present = match PresentLoop::new(opts.windowed, slot.clone(), exit.clone()) {
        Ok(present) => present,
        Err(e) => {
            match backend.shutdown() {
                Ok(()) => {}
                Err(_) => {}
            }
            match launcher.kill() {
                Ok(()) => {}
                Err(_) => {}
            }
            return Err(format!("presentation failed: {e}"));
        }
    };
    let proxy = present.proxy();
    let worker = std::thread::spawn(move || {
        run_processing(
            &mut backend,
            &mut launcher,
            scale,
            slot,
            move || proxy.send_event(PresentUserEvent::FrameReady).is_ok(),
            processing_exit,
        )
    });
    let present_result = present.run();
    exit.store(true, Ordering::Relaxed);
    let worker_result = match worker.join() {
        Ok(result) => result,
        Err(_) => Err(format!("{program}: processing thread ended unexpectedly")),
    };
    let present_error = present_result
        .err()
        .map(|e| format!("presentation failed: {e}"));
    match (present_error, worker_result) {
        (None, Ok(())) => Ok(()),
        (Some(e), Ok(())) => Err(e),
        (None, Err(e)) => Err(e),
        (Some(p), Err(w)) => Err(format!("{p}; {w}")),
    }
}

/// The processing thread: pull frames, process them, publish the
/// newest to the presentation slot and wake the presentation loop.
/// `wake` reports whether the loop is still alive: it sends the
/// frame-ready user event through the loop's proxy, and returns
/// false once the loop has closed. Ends when the exit flag is set,
/// the target exits, the loop closes, or capture fails.
fn run_processing(
    backend: &mut WaylandCapture,
    launcher: &mut Launcher,
    scale: u32,
    slot: Arc<FrameSlot>,
    wake: impl Fn() -> bool,
    exit: Arc<AtomicBool>,
) -> Result<(), String> {
    let processor = crate::contour_from_env();
    let mut result: Result<(), String> = Ok(());
    while !exit.load(Ordering::Relaxed) {
        match backend.capture_frame() {
            Ok(Some(frame)) => match processor.process(&frame, scale) {
                Ok(processed) => {
                    slot.publish(processed);
                    if !wake() {
                        break;
                    }
                }
                Err(e) => {
                    result = Err(format!("processing failed: {e}"));
                    break;
                }
            },
            Ok(None) => {
                if launcher.exited() {
                    exit.store(true, Ordering::Relaxed);
                    break;
                }
            }
            Err(e) => {
                result = Err(format!("capture failed: {e}"));
                break;
            }
        }
    }
    exit.store(true, Ordering::Relaxed);
    let seen = backend.frames_seen();
    let dropped_capture = backend.frames_dropped();
    match backend.shutdown() {
        Ok(()) => {}
        Err(e) => {
            if result.is_ok() {
                result = Err(format!("capture shutdown failed: {e}"));
            }
        }
    }
    eprintln!("pixelflow: capture saw {seen} frames, dropped {dropped_capture}");
    match launcher.kill() {
        Ok(()) => {}
        Err(e) => {
            if result.is_ok() {
                result = Err(e);
            }
        }
    }
    let dropped = slot.dropped();
    if dropped > 0 {
        eprintln!("pixelflow: dropped {dropped} processed frames (presentation behind)");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_scale_fails_before_launching() {
        let result = run(RunOptions {
            program: "pixelflow-definitely-not-a-program".to_string(),
            args: vec![],
            source: SourceKind::Window,
            scale: 3,
            windowed: true,
        });
        assert!(result.is_err(), "expected a scale error");
        assert!(
            matches!(&result, Err(e) if e.contains("scale 3 is not supported")),
            "the error names the scale, not the missing program"
        );
    }

    #[test]
    fn target_that_exits_at_once_is_an_error() {
        let result = run(RunOptions {
            program: "/bin/true".to_string(),
            args: vec![],
            source: SourceKind::Window,
            scale: 4,
            windowed: true,
        });
        assert!(result.is_err(), "expected an error for an exited target");
        assert!(
            matches!(&result, Err(e) if e.contains("exited before capture started")),
            "the error names the early exit"
        );
    }
}
