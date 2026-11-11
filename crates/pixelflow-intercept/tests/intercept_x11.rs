//! Interception against real software-rendered programs.
//!
//! These tests launch programs that present through X11 image puts
//! instead of GL swap calls: `sdl_soft_target`, a real SDL 1.2
//! program drawing into a 16-bit software surface and presenting
//! with SDL_Flip, and `x11_target`, a real X11 program presenting
//! with XShmPutImage or XPutImage. They are the zsnes case: a
//! software-rendered program the swap-only shim never saw, so it
//! captured zero frames.
//!
//! The `no_frames_without_interception` test is the before state:
//! the same SDL target run without the shim presents to its own
//! window and `PixelFlow` sees nothing, which is exactly the
//! failure the X11 interception path fixes. The 32-bit test runs
//! `x11_target` built with `-m32` under qemu-i386, the other half
//! of the zsnes failure: a 64-bit shim cannot load into a 32-bit
//! process.

use pixelflow_capture::CaptureBackend;
use pixelflow_core::Frame;
use pixelflow_intercept::{InterceptCapture, shim32_path};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Display numbers for the virtual X servers, allocated per test so
/// parallel tests never share one. Displays 101..108, clear of the
/// 91..98 range the GLX tests use.
static NEXT_DISPLAY: AtomicU64 = AtomicU64::new(0);

/// Scratch-directory ids, allocated per collection so parallel
/// tests never compile the target to the same path.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

/// A running Xvfb, killed when dropped.
struct Xvfb {
    /// The display number, e.g. 101 for `:101`.
    display: u64,
    /// The server process.
    child: Child,
}

impl Xvfb {
    /// Start Xvfb on a fresh display and wait for its socket.
    fn start() -> Result<Xvfb, String> {
        let display = 101 + NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed) % 8;
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

/// Compile `source` to `out` with the system C compiler, plus the
/// extra compiler arguments (libraries, `-m32`, SDL flags).
fn compile_target(source: &str, out: &PathBuf, extra: &[String]) -> Result<(), String> {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(source);
    let mut command = Command::new("cc");
    command.args(["-O2"]).arg(&source).arg("-o").arg(out);
    for arg in extra {
        command.arg(arg);
    }
    let status = command
        .status()
        .map_err(|e| format!("the C compiler must run for the interception tests: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} must compile", source.display()))
    }
}

/// The `sdl-config --cflags --libs` arguments, split on
/// whitespace, for compiling against SDL 1.2.
fn sdl_flags() -> Result<Vec<String>, String> {
    let output = Command::new("sdl-config")
        .args(["--cflags", "--libs"])
        .output()
        .map_err(|e| format!("sdl-config must run for the interception tests: {e}"))?;
    if !output.status.success() {
        return Err("sdl-config must succeed for the interception tests".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(String::from)
        .collect())
}

/// The pixel at (`x`, `y`) of a top-first RGB8 frame.
fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
    let row = frame.row(y);
    let offset = x as usize * 3;
    [row[offset], row[offset + 1], row[offset + 2]]
}

/// True when `a` and `b` agree within `tolerance` levels.
fn near(a: u8, b: u8, tolerance: u8) -> bool {
    a.abs_diff(b) <= tolerance
}

/// How the target is intercepted, if at all.
enum Intercept {
    /// No shim: the target presents to its own window.
    Off,
    /// The 64-bit shim, for a 64-bit target.
    On,
    /// The shim selected by the target's ELF class, for a target
    /// run under qemu-i386: the 32-bit shim for the 32-bit guest.
    ForProgram,
}

/// Run `program` with `args` under `display` and collect every
/// frame the backend delivers until the target exits and the stream
/// goes quiet, or `wait` elapses. Returns the frames in arrival
/// order.
fn collect_frames(
    program: &str,
    args: &[String],
    intercept: Intercept,
    display: &str,
    wait: Duration,
) -> Result<Vec<Frame>, String> {
    let mut backend =
        InterceptCapture::new().map_err(|e| format!("the interception backend must bind: {e}"))?;
    let mut command = Command::new(program);
    command.args(args);
    command.env("DISPLAY", display);
    match intercept {
        Intercept::Off => {}
        Intercept::On => {
            for (key, value) in backend.environment() {
                command.env(key, value);
            }
        }
        Intercept::ForProgram => {
            let env = backend
                .environment_for(program, args)
                .map_err(|e| format!("the interception shim must be selected: {e}"))?;
            for (key, value) in env {
                command.env(key, value);
            }
        }
    }
    command.stdin(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|e| format!("the target must start: {e}"))?;
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
    backend
        .shutdown()
        .map_err(|e| format!("the interception backend must shut down: {e}"))?;
    Ok(frames)
}

/// A fresh scratch directory for one test's compiled target.
fn scratch_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "pixelflow-intercept-x11-test-{}-{}",
        std::process::id(),
        NEXT_RUN.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// The distinct background reds across `frames`, read at (10, 10).
/// Successive renders differ, which proves the frames are the
/// target's successive frames, not one frame repeated.
fn distinct_reds(frames: &[Frame]) -> Vec<u8> {
    let mut reds: Vec<u8> = frames.iter().map(|frame| pixel(frame, 10, 10)[0]).collect();
    reds.dedup();
    reds
}

/// The left edge of the yellow rectangle in each frame, read by
/// scanning row 60 for a yellow pixel. The rectangle slides right
/// 3 pixels per frame, so distinct edges prove the frames are
/// successive. Yellow is matched within one level, because SDL's
/// 565-to-888 expansion lands the green on 254 instead of 255.
fn distinct_rect_edges(frames: &[Frame]) -> Vec<u32> {
    let mut edges: Vec<u32> = frames
        .iter()
        .filter_map(|frame| {
            (0..frame.width()).find(|x| {
                let px = pixel(frame, *x, 60);
                near(px[0], 255, 1) && near(px[1], 255, 1) && near(px[2], 0, 1)
            })
        })
        .collect();
    edges.dedup();
    edges
}

/// The shared assertions for a 10-frame run of an X11 target:
/// at least 8 frames arrived, at the target's size, with
/// successive reds, a moving yellow rectangle, and the expected
/// background on the first frame. `rect_tolerance` allows for the
/// 565-to-888 expansion SDL applies, which lands a level off the
/// exact color.
fn assert_x11_frames(
    frames: &[Frame],
    size: (u32, u32),
    red_tolerance: u8,
    blue: u8,
    rect_tolerance: u8,
) {
    assert!(
        frames.len() >= 8,
        "expected at least 8 intercepted frames, got {}",
        frames.len()
    );
    for frame in frames {
        assert_eq!((frame.width(), frame.height()), size);
    }
    let reds = distinct_reds(frames);
    assert!(
        reds.len() >= 5,
        "expected at least 5 distinct frame reds, got {reds:?}"
    );
    let edges = distinct_rect_edges(frames);
    assert!(
        edges.len() >= 3,
        "expected the rectangle to move across frames, got edges {edges:?}"
    );
    let first = &frames[0];
    assert_eq!(pixel(first, 10, 10)[1], 0, "the background green is 0");
    assert!(
        near(pixel(first, 10, 10)[2], blue, red_tolerance),
        "the background blue is {blue}: {:?}",
        pixel(first, 10, 10)
    );
    let rect = pixel(first, 40, 60);
    assert!(
        near(rect[0], 255, rect_tolerance)
            && near(rect[1], 255, rect_tolerance)
            && near(rect[2], 0, rect_tolerance),
        "the rectangle pixel is yellow: {rect:?}"
    );
}

/// Drop leading frames that are entirely black. SDL presents one
/// blank clear before the first drawn frame, so the first frame
/// the shim sends for an SDL program is black, not a drawn frame.
fn drop_leading_blank(frames: Vec<Frame>) -> Vec<Frame> {
    let mut frames = frames;
    while let Some(first) = frames.first() {
        let blank = [(10, 10), (160, 120), (300, 200)]
            .iter()
            .all(|(x, y)| pixel(first, *x, *y) == [0, 0, 0]);
        if blank {
            frames.remove(0);
        } else {
            break;
        }
    }
    frames
}

#[test]
fn intercepted_frames_arrive_from_a_real_sdl_soft_program() -> Result<(), String> {
    let xvfb = Xvfb::start()?;
    let display = xvfb.display_value();
    let dir = scratch_dir()?;
    let target = dir.join("sdl_soft_target");
    compile_target("sdl_soft_target.c", &target, &sdl_flags()?)?;
    let frames = collect_frames(
        target.to_str().unwrap(),
        &["10".to_string()],
        Intercept::On,
        &display,
        Duration::from_secs(15),
    )?;
    // The target draws into a 16-bit surface, so the 8-bit values
    // come back quantized through RGB565: blue 200 reads back as
    // 205, reds step by 8 or 9 instead of 7, and the yellow
    // rectangle's green lands on 254 instead of 255. SDL pads its
    // display image height to 256 for a 240-row window, so the
    // intercepted frame is 320x256 with 16 black rows at the
    // bottom, and its first put is a blank clear the test drops.
    let frames = drop_leading_blank(frames);
    assert_x11_frames(&frames, (320, 256), 2, 205, 1);
    Ok(())
}

#[test]
fn intercepted_frames_arrive_from_a_real_x11_program() -> Result<(), String> {
    let xvfb = Xvfb::start()?;
    let display = xvfb.display_value();
    let dir = scratch_dir()?;
    let target = dir.join("x11_target");
    compile_target(
        "x11_target.c",
        &target,
        &["-lX11".to_string(), "-lXext".to_string()],
    )?;
    // "plain" forces XPutImage instead of XShmPutImage, so this
    // test exercises the plain put path.
    let frames = collect_frames(
        target.to_str().unwrap(),
        &["10".to_string(), "plain".to_string()],
        Intercept::On,
        &display,
        Duration::from_secs(15),
    )?;
    // 24-bit image, no quantization: blue is exactly 200, the
    // rectangle exactly yellow, and the frame exactly 320x240.
    assert_x11_frames(&frames, (320, 240), 0, 200, 0);
    Ok(())
}

#[test]
fn intercepted_frames_arrive_from_a_32_bit_x11_program() -> Result<(), String> {
    if shim32_path().is_empty() {
        eprintln!("skipped: no 32-bit shim was built (no 32-bit C toolchain)");
        return Ok(());
    }
    let qemu = Command::new("qemu-i386").arg("--version").output();
    if qemu.is_err() {
        eprintln!("skipped: qemu-i386 is not installed");
        return Ok(());
    }
    let xvfb = Xvfb::start()?;
    let display = xvfb.display_value();
    let dir = scratch_dir()?;
    let target = dir.join("x11_target32");
    compile_target(
        "x11_target.c",
        &target,
        &[
            "-m32".to_string(),
            "-lX11".to_string(),
            "-lXext".to_string(),
        ],
    )?;
    assert!(target.is_file(), "the 32-bit target must be built");
    let frames = collect_frames(
        "qemu-i386",
        &[target.to_string_lossy().into_owned(), "10".to_string()],
        Intercept::ForProgram,
        &display,
        Duration::from_secs(20),
    )?;
    // The 32-bit target presents the same 24-bit image as the
    // 64-bit one; the only difference is the shim build. Note the
    // ld.so "wrong ELF class: ELFCLASS32" line qemu prints while
    // starting is expected: the 64-bit qemu process itself cannot
    // preload the 32-bit shim, and the loader ignores it; the
    // 32-bit guest loads it fine, which is what these frames
    // prove.
    assert_x11_frames(&frames, (320, 240), 0, 200, 0);
    Ok(())
}

#[test]
fn no_frames_without_interception() -> Result<(), String> {
    let xvfb = Xvfb::start()?;
    let display = xvfb.display_value();
    let dir = scratch_dir()?;
    let target = dir.join("sdl_soft_target");
    compile_target("sdl_soft_target.c", &target, &sdl_flags()?)?;
    let frames = collect_frames(
        target.to_str().unwrap(),
        &["10".to_string()],
        Intercept::Off,
        &display,
        Duration::from_secs(3),
    )?;
    assert_eq!(
        frames.len(),
        0,
        "without the shim the target presents to its own window and PixelFlow sees nothing"
    );
    Ok(())
}
