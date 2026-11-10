//! Benchmark Contour processing at realistic source sizes.
//!
//! Two modes. With no arguments, the orchestrator runs one case per
//! subprocess (one size, one scale) so each case reports its own
//! peak RSS from `/proc/self/status` (`VmHWM`), and prints a table.
//! With `--one <width> <height> <scale>`, it runs one case: one
//! warmup run, then five timed runs, and prints the median wall
//! time per frame, frames per second, and peak RSS in MiB.
//!
//! The input frames are synthetic, generated in code: a flat base
//! split by a hard vertical edge, a 2:1 diagonal band (so the edge
//! follower has long runs to follow), a checkerboard block, and a
//! smooth gradient strip. No fixtures, no copyrighted images.
//!
//! The follower obeys `CONTOUR_NO_FOLLOW=1` from the environment,
//! inherited by the subprocesses, so the same binary reports the
//! follower-on and follower-off numbers in separate runs.

use pixelflow_core::{Frame, PixelFormat, Processor};
use pixelflow_processing::Contour;
use std::env;
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::process::{Command, ExitStatus};
use std::time::Instant;

const SIZES: [(u32, u32); 4] = [(256, 224), (854, 480), (1280, 720), (1920, 1080)];
const SCALES: [u32; 2] = [2, 4];
const TIMED_REPS: usize = 5;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--one") => {
            let width = parse_arg(&args, 2, "width")?;
            let height = parse_arg(&args, 3, "height")?;
            let scale = parse_arg(&args, 4, "scale")?;
            run_one(width, height, scale)
        }
        Some("--help") | Some("-h") => {
            println!("usage: pixelflow-bench [--one <width> <height> <scale>]");
            println!("With no arguments, runs all sizes and scales and prints a table.");
            println!("Set CONTOUR_NO_FOLLOW=1 to benchmark without the edge follower.");
            Ok(())
        }
        Some(other) => Err(format!("unknown argument: {other}").into()),
        None => run_all(),
    }
}

fn parse_arg(args: &[String], index: usize, name: &str) -> Result<u32, Box<dyn Error>> {
    let text = args
        .get(index)
        .ok_or_else(|| format!("missing {name} argument"))?;
    text.parse::<u32>()
        .map_err(|e| format!("invalid {name} '{text}': {e}").into())
}

/// One pixel of the synthetic benchmark frame: flat base, hard
/// vertical edge at mid-width, a 2:1 diagonal band, a checkerboard
/// block in the top-left quarter, and a gradient strip along the
/// bottom quarter.
fn pixel_color(x: u32, y: u32, width: u32, height: u32) -> [u8; 3] {
    let mut color = if x < width / 2 {
        [160, 90, 60]
    } else {
        [60, 90, 160]
    };
    let diagonal = i64::from(x) - 2 * i64::from(y);
    if diagonal.abs() < 2 {
        color = [240, 240, 240];
    }
    if x < width / 4 && y < height / 4 {
        let cell = (x / 2 + y / 2) % 2;
        color = if cell == 0 {
            [0, 0, 0]
        } else {
            [255, 255, 255]
        };
    }
    if y >= height * 3 / 4 {
        let gradient = u8::try_from(x.saturating_mul(255) / width).unwrap_or(255);
        color = [gradient, 128, 64];
    }
    color
}

fn generate_frame(width: u32, height: u32) -> Result<Frame, Box<dyn Error>> {
    let len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or("benchmark frame size overflows")?;
    let mut data = Vec::with_capacity(
        usize::try_from(len).map_err(|_| "benchmark frame size does not fit usize")?,
    );
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&pixel_color(x, y, width, height));
        }
    }
    Ok(Frame::new(width, height, PixelFormat::Rgb8, data)?)
}

/// Peak resident set size of this process in KiB, from VmHWM.
fn peak_rss_kib() -> Result<u64, Box<dyn Error>> {
    let status = fs::read_to_string("/proc/self/status")?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmHWM:") {
            let kib = value
                .split_whitespace()
                .next()
                .ok_or("VmHWM line has no value")?;
            return kib.parse::<u64>().map_err(|e| e.into());
        }
    }
    Err("VmHWM not found in /proc/self/status".into())
}

/// Run one case: one warmup, then `TIMED_REPS` timed runs. Prints
/// one machine-readable line with the median time, fps, and peak
/// RSS. A case the memory budget refuses prints a refusal line
/// instead: the budget is product behavior, and the benchmark
/// reports it rather than working around it.
fn run_one(width: u32, height: u32, scale: u32) -> Result<(), Box<dyn Error>> {
    let out_width = width
        .checked_mul(scale)
        .ok_or("benchmark output width overflows")?;
    let out_height = height
        .checked_mul(scale)
        .ok_or("benchmark output height overflows")?;
    if let Err(e) = Contour::check_memory_budget(out_width, out_height) {
        println!("case {width}x{height} scale={scale} refused: {e}");
        return Ok(());
    }
    let frame = generate_frame(width, height)?;
    let contour = Contour;
    let warmup = contour.process(&frame, scale)?;
    black_box(warmup);
    let mut times: Vec<f64> = Vec::with_capacity(TIMED_REPS);
    for _ in 0..TIMED_REPS {
        let start = Instant::now();
        let out = contour.process(&frame, scale)?;
        black_box(out);
        times.push(start.elapsed().as_secs_f64());
    }
    times.sort_by(f64::total_cmp);
    let median = times[TIMED_REPS / 2];
    let peak_mib = peak_rss_kib()? / 1024;
    println!(
        "case {width}x{height} scale={scale} reps={TIMED_REPS} median_ms={:.1} fps={:.2} peak_rss_mib={peak_mib}",
        median * 1000.0,
        1.0 / median
    );
    Ok(())
}

/// Run every size and scale in its own subprocess (so each case
/// reports its own peak RSS) and print the table.
fn run_all() -> Result<(), Box<dyn Error>> {
    let follower = if env::var("CONTOUR_NO_FOLLOW").is_ok() {
        "off"
    } else {
        "on"
    };
    println!("contour benchmark, edge follower: {follower}");
    println!(
        "sizes 256x224, 854x480, 1280x720, 1920x1080; scales 2, 4; 1 warmup + {TIMED_REPS} timed runs per case; median reported"
    );
    println!("size scale median_ms fps peak_rss_mib");
    let exe = env::current_exe()?;
    for (width, height) in SIZES {
        for scale in SCALES {
            let output = Command::new(&exe)
                .args([
                    "--one",
                    &width.to_string(),
                    &height.to_string(),
                    &scale.to_string(),
                ])
                .output()?;
            if output.status.success() {
                print!("{}", String::from_utf8_lossy(&output.stdout));
            } else {
                report_failed(width, height, scale, &output.status, &output.stderr);
            }
        }
    }
    Ok(())
}

fn report_failed(width: u32, height: u32, scale: u32, status: &ExitStatus, stderr: &[u8]) {
    use std::os::unix::process::ExitStatusExt;
    let cause = match status.signal() {
        Some(signal) => format!("killed by signal {signal}"),
        None => format!("exited with status {status}"),
    };
    println!("case {width}x{height} scale={scale} FAILED ({cause})");
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        eprintln!("{width}x{height} scale={scale} stderr: {trimmed}");
    }
}
