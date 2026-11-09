//! The `run` command: launch the target, capture its window, run
//! each frame through the processing pipeline, and present the result
//! in a fullscreen window.
//!
//! The work splits across two threads. The processing thread owns the
//! capture backend and the child process: it pulls frames, processes
//! them, and sends the newest to the presentation window, dropping
//! frames when presentation falls behind so latency never
//! accumulates. The main thread runs the presentation window, because
//! the windowing system requires the event loop there. Either side
//! ending sets the shared exit flag, and the other side winds down:
//! the processing thread stops capture and kills the child, and the
//! presentation window closes.
//!
//! The target is launched first, then PixelFlow waits briefly for its
//! window to appear before opening the portal session. The compositor
//! dialog is where the user picks the window, so the wait only covers
//! application startup; there is no window list to poll.

use crate::launcher::Launcher;
use pixelflow_capture::{CaptureBackend, CaptureConfig, SourceKind};
use pixelflow_core::Processor;
use pixelflow_processing::Contour;
use pixelflow_wayland::WaylandCapture;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// How long to wait for the target's window to appear before opening
/// the portal session.
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
    let processor = Contour;
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
    let (frame_tx, frame_rx) = mpsc::sync_channel(1);
    let exit = Arc::new(AtomicBool::new(false));
    let processing_exit = exit.clone();
    let scale = opts.scale;
    let program = opts.program.clone();
    let worker = std::thread::spawn(move || -> Result<(), String> {
        run_processing(&mut backend, &mut launcher, scale, frame_tx, processing_exit)
    });
    let present_result = pixelflow_present::run(opts.windowed, frame_rx, exit.clone());
    exit.store(true, Ordering::Relaxed);
    let worker_result = match worker.join() {
        Ok(result) => result,
        Err(_) => Err(format!("{program}: processing thread ended unexpectedly")),
    };
    let present_error = present_result.err().map(|e| format!("presentation failed: {e}"));
    match (present_error, worker_result) {
        (None, Ok(())) => Ok(()),
        (Some(e), Ok(())) => Err(e),
        (None, Err(e)) => Err(e),
        (Some(p), Err(w)) => Err(format!("{p}; {w}")),
    }
}

/// The processing thread: pull frames, process them, send the newest
/// to presentation. Ends when the exit flag is set, the target exits,
/// or capture fails.
fn run_processing(
    backend: &mut WaylandCapture,
    launcher: &mut Launcher,
    scale: u32,
    frame_tx: mpsc::SyncSender<pixelflow_core::Frame>,
    exit: Arc<AtomicBool>,
) -> Result<(), String> {
    let processor = Contour;
    let mut dropped: u64 = 0;
    let mut result: Result<(), String> = Ok(());
    while !exit.load(Ordering::Relaxed) {
        match backend.capture_frame() {
            Ok(Some(frame)) => match processor.process(&frame, scale) {
                Ok(processed) => {
                    if frame_tx.try_send(processed).is_err() {
                        dropped += 1;
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
