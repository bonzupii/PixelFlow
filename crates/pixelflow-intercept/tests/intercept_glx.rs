//! Interception against a real OpenGL program.
//!
//! These tests launch `glx_target`, a real GLX program that draws 10
//! frames with frame-dependent pixels, under a virtual X server, and
//! drive it through [`InterceptCapture`]. They are not mocks of the
//! implementation: the target is a separate process rendering through
//! Mesa, the shim is the shipped interception library, and the
//! assertions check the target's own pixels arriving in `PixelFlow`.
//!
//! The `no_frames_without_interception` test is the before state:
//! the same target run without the shim presents to its own window
//! and `PixelFlow` sees nothing, which is exactly the failure the
//! interception path fixes.

use pixelflow_capture::CaptureBackend;
use pixelflow_core::Frame;
use pixelflow_intercept::InterceptCapture;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Display numbers for the virtual X servers, allocated per test so
/// parallel tests never share one. Displays 91..98.
static NEXT_DISPLAY: AtomicU64 = AtomicU64::new(0);

/// Scratch-directory ids, allocated per collection so parallel
/// tests never compile the target to the same path.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

/// A running Xvfb, killed when dropped.
struct Xvfb {
    /// The display number, e.g. 91 for `:91`.
    display: u64,
    /// The server process.
    child: Child,
}

impl Xvfb {
    /// Start Xvfb on a fresh display and wait for its socket.
    fn start() -> Result<Xvfb, String> {
        let display = 91 + NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed) % 8;
        let name = format!(":{display}");
        let mut child = Command::new("Xvfb")
            .args([name.as_str(), "-screen", "0", "1024x768x24"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Xvfb must start for the interception tests: {e}"))?;
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err(format!(
                    "Xvfb on :{display} did not create its socket in time"
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Xvfb { display, child })
    }

    /// The `DISPLAY` value for children of this server.
    fn display_value(&self) -> String {
        format!(":{}", self.display)
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Compile `glx_target.c` to `out` with the system C compiler.
fn compile_target(out: &PathBuf) -> Result<(), String> {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/glx_target.c");
    let status = Command::new("cc")
        .args(["-O2"])
        .arg(&source)
        .arg("-o")
        .arg(out)
        .args(["-lX11", "-l:libGL.so.1"])
        .status()
        .map_err(|e| format!("the C compiler must run for the interception tests: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("glx_target must compile".to_string())
    }
}

/// The red channel the target draws in frame `index`.
fn frame_red(index: u8) -> u8 {
    index.saturating_mul(25)
}

/// True when `a` and `b` agree within one level, the rounding the
/// float-to-8-bit clear-color conversion may introduce.
fn near(a: u8, b: u8) -> bool {
    a.abs_diff(b) <= 1
}

/// The pixel at (`x`, `y`) of a top-first RGB8 frame.
fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
    let row = frame.row(y);
    let offset = x as usize * 3;
    [row[offset], row[offset + 1], row[offset + 2]]
}

/// Run the target under `display` and collect every frame the
/// backend delivers until the target exits and the stream goes
/// quiet, or `wait` elapses. When `intercept` is true the target
/// runs with the shim preloaded; when false it runs unmodified.
/// Returns the frames in arrival order, and the backend's seen
/// count.
fn collect_frames(
    intercept: bool,
    display: &str,
    wait: Duration,
) -> Result<(Vec<Frame>, u64), String> {
    let dir = std::env::temp_dir().join(format!(
        "pixelflow-intercept-test-{}-{}",
        std::process::id(),
        NEXT_RUN.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join("glx_target");
    compile_target(&target)?;
    let mut backend =
        InterceptCapture::new().map_err(|e| format!("the interception backend must bind: {e}"))?;
    let mut command = Command::new(&target);
    command.env("DISPLAY", display);
    if intercept {
        for (key, value) in backend.environment() {
            command.env(key, value);
        }
    }
    command.stdin(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|e| format!("glx_target must start: {e}"))?;
    backend
        .initialize(&pixelflow_capture::CaptureConfig::default())
        .map_err(|e| format!("the interception backend must start: {e}"))?;
    let mut frames = Vec::new();
    let deadline = Instant::now() + wait;
    let mut exited_at: Option<Instant> = None;
    while Instant::now() < deadline {
        match backend.capture_frame() {
            Ok(Some(frame)) => frames.push(frame),
            Ok(None) => {
                if exited_at.is_none()
                    && child
                        .try_wait()
                        .map_err(|e| format!("the target must be waitable: {e}"))?
                        .is_some()
                {
                    exited_at = Some(Instant::now());
                }
                // The reader may still be delivering the last
                // frames after the target exits: wait for 8 frames
                // or for 2 seconds of quiet after the exit.
                if let Some(exited) = exited_at
                    && (frames.len() >= 8 || exited.elapsed() > Duration::from_secs(2))
                {
                    break;
                }
            }
            Err(e) => return Err(format!("capture failed: {e}")),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let seen = backend.frames_seen();
    backend
        .shutdown()
        .map_err(|e| format!("the interception backend must shut down: {e}"))?;
    Ok((frames, seen))
}

#[test]
fn intercepted_frames_arrive_from_a_real_gl_program() -> Result<(), String> {
    let xvfb = Xvfb::start()?;
    let display = xvfb.display_value();
    let (frames, seen) = collect_frames(true, &display, Duration::from_secs(15))?;
    assert!(
        frames.len() >= 8,
        "expected at least 8 intercepted frames, got {}",
        frames.len()
    );
    assert_eq!(
        seen,
        u64::try_from(frames.len()).unwrap_or(u64::MAX),
        "seen counts delivered frames"
    );
    for frame in &frames {
        assert_eq!((frame.width(), frame.height()), (320, 240));
    }
    // The background red increases frame by frame, which proves the
    // frames are successive renders from the target, not one frame
    // repeated. The rectangle pixel is yellow in every frame.
    let reds: Vec<u8> = frames.iter().map(|frame| pixel(frame, 10, 10)[0]).collect();
    let mut distinct = reds.clone();
    distinct.dedup();
    assert!(
        distinct.len() >= 5,
        "expected at least 5 distinct frame reds, got {reds:?}"
    );
    let first = &frames[0];
    assert!(
        (0..10).any(|i| near(pixel(first, 10, 10)[0], frame_red(i))),
        "the background red is a frame the target drew: {:?}",
        pixel(first, 10, 10)
    );
    assert_eq!(pixel(first, 10, 10)[1], 0, "the background green is 0");
    assert!(
        near(pixel(first, 10, 10)[2], 200),
        "the background blue is 200: {:?}",
        pixel(first, 10, 10)
    );
    // The rectangle was drawn at GL y 30..90, so after the shim's
    // vertical flip it sits at frame y 150..210.
    assert_eq!(
        pixel(first, 80, 180),
        [255, 255, 0],
        "the rectangle pixel is yellow"
    );
    Ok(())
}

#[test]
fn no_frames_without_interception() -> Result<(), String> {
    let xvfb = Xvfb::start()?;
    let display = xvfb.display_value();
    let (frames, _) = collect_frames(false, &display, Duration::from_secs(3))?;
    assert_eq!(
        frames.len(),
        0,
        "without the shim the target presents to its own window and PixelFlow sees nothing"
    );
    Ok(())
}
