//! PixelFlow command-line interface.
//!
//! Commands: `upscale` (PNG in, PNG out), `run` (launch an
//! application, capture its window, upscale each frame, present it),
//! `backends`, and `info`.

mod launcher;
mod png_io;
mod run;

use clap::{Parser, Subcommand};
use pixelflow_capture::SourceKind;
use pixelflow_core::{Node, ProcessError, Processor};
use pixelflow_processing::{Contour, Nearest};
use std::process::ExitCode;

/// PixelFlow: cross-platform pixel-art upscaling wrapper.
#[derive(Parser)]
#[command(name = "pixelflow", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Upscale a PNG image with a processing algorithm.
    Upscale {
        /// Input PNG file.
        input: String,
        /// Output PNG file.
        output: String,
        /// Integer scale factor (2 or 4).
        #[arg(long, default_value_t = 4)]
        scale: u32,
        /// Processing algorithm.
        #[arg(long, default_value = "contour")]
        algorithm: String,
    },
    /// Launch an application, capture its window, upscale each frame,
    /// and present the result.
    Run {
        /// Program to launch.
        program: String,
        /// Arguments for the program.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// What to capture: window or monitor.
        #[arg(long, value_parser = ["window", "monitor"], default_value = "window")]
        capture: String,
        /// Integer scale factor (2 or 4).
        #[arg(long, default_value_t = 4)]
        scale: u32,
        /// Windowed preview instead of fullscreen.
        #[arg(long, conflicts_with = "fullscreen")]
        windowed: bool,
        /// Fullscreen presentation.
        #[arg(long)]
        fullscreen: bool,
    },
    /// List available capture and presentation backends.
    Backends,
    /// Report hardware, display, and backend capabilities.
    Info,
}

fn algorithm(name: &str) -> Result<Box<dyn Processor>, String> {
    match name {
        "contour" => Ok(contour_from_env()),
        "nearest" => Ok(Box::new(Nearest)),
        other => Err(format!(
            "unknown algorithm {other:?}; available: contour, nearest"
        )),
    }
}

/// The `PIXELFLOW_MEMORY_BUDGET` value in bytes, when it is set
/// and parses as a plain integer. An unset or invalid value
/// means no budget: any size the machine can allocate is
/// attempted. The budget is a caller resource limit, not a
/// product ceiling.
fn memory_budget_from_env() -> Option<u64> {
    std::env::var("PIXELFLOW_MEMORY_BUDGET")
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
}

/// Build the Contour processor, honouring `PIXELFLOW_MEMORY_BUDGET`
/// when it is set.
fn contour_from_env() -> Box<dyn Processor> {
    match memory_budget_from_env() {
        Some(bytes) => Box::new(Contour::with_memory_budget(bytes)),
        None => Box::new(Contour),
    }
}

/// Build the Contour graph node for the live `run` path,
/// honouring `PIXELFLOW_MEMORY_BUDGET` the same way
/// `contour_from_env` does. The node declares the Global
/// spatial dependency `Contour::node` states, so the graph
/// runs it whole-frame.
fn contour_node_from_env(scale: u32) -> Box<dyn Node> {
    match memory_budget_from_env() {
        Some(bytes) => Box::new(Contour::with_memory_budget(bytes).node(scale)),
        None => Box::new(Contour::node(scale)),
    }
}

fn run_upscale(input: &str, output: &str, scale: u32, algorithm_name: &str) -> Result<(), String> {
    let proc = algorithm(algorithm_name)?;
    if !proc.supported_scales().contains(&scale) {
        return Err(ProcessError::UnsupportedScale {
            algorithm: proc.name(),
            scale,
        }
        .to_string());
    }
    let frame = png_io::load_png(input)?;
    eprintln!(
        "input {}x{} {} scale {}x algorithm {}",
        frame.width(),
        frame.height(),
        frame.format(),
        scale,
        proc.name()
    );
    let t0 = std::time::Instant::now();
    let out = proc.process(&frame, scale).map_err(|e| e.to_string())?;
    eprintln!(
        "output {}x{} in {:.1}s",
        out.width(),
        out.height(),
        t0.elapsed().as_secs_f32()
    );
    png_io::save_png(output, &out)?;
    eprintln!("wrote {output}");
    Ok(())
}

fn run_backends() {
    println!("capture backends:");
    println!("  wayland  XDG Desktop Portal ScreenCast + PipeWire (window or monitor)");
    println!("presentation backends:");
    println!("  wgpu     fullscreen borderless window, aspect-ratio preserving, vsync");
    println!("processing algorithms:");
    println!("  contour  2x, 4x  (algorithm 20: gated merge, adaptive labelset, edge follower)");
    println!("  nearest    2x, 4x  (nearest-neighbour baseline)");
}

fn run_info() {
    println!("pixelflow {}", env!("CARGO_PKG_VERSION"));
    println!(
        "os: {}  arch: {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match std::thread::available_parallelism() {
        Ok(n) => println!("cpus: {n}"),
        Err(_) => println!("cpus: unknown"),
    }
    println!("display: probed when the presentation window opens");
    println!("capture: wayland (XDG Desktop Portal ScreenCast + PipeWire)");
    println!("processing algorithms: contour, nearest");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Upscale {
            input,
            output,
            scale,
            algorithm,
        } => run_upscale(input, output, *scale, algorithm),
        Command::Run {
            program,
            args,
            capture,
            scale,
            windowed,
            ..
        } => {
            let source = match capture.as_str() {
                "monitor" => SourceKind::Monitor,
                _ => SourceKind::Window,
            };
            run::run(run::RunOptions {
                program: program.clone(),
                args: args.clone(),
                source,
                scale: *scale,
                windowed: *windowed,
            })
        }
        Command::Backends => {
            run_backends();
            Ok(())
        }
        Command::Info => {
            run_info();
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pixelflow: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run command's fields, or empty values when the parsed
    /// command is not `run`, so a wrong variant fails the asserts.
    fn run_fields(cli: Cli) -> (String, Vec<String>, String, u32, bool, bool) {
        match cli.command {
            Command::Run {
                program,
                args,
                capture,
                scale,
                windowed,
                fullscreen,
            } => (program, args, capture, scale, windowed, fullscreen),
            _ => (String::new(), vec![], String::new(), 0, false, false),
        }
    }

    #[test]
    fn run_parses_program_and_flags() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "pixelflow",
            "run",
            "gtk-slashem",
            "--capture",
            "monitor",
            "--scale",
            "2",
            "--windowed",
        ])?;
        let (program, args, capture, scale, windowed, fullscreen) = run_fields(cli);
        assert_eq!(program, "gtk-slashem");
        assert!(args.is_empty());
        assert_eq!(capture, "monitor");
        assert_eq!(scale, 2);
        assert!(windowed);
        assert!(!fullscreen);
        Ok(())
    }

    #[test]
    fn run_passes_trailing_args_to_the_target() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["pixelflow", "run", "zsnes", "--", "-s", "game.smc"])?;
        let (program, args, _, _, _, _) = run_fields(cli);
        assert_eq!(program, "zsnes");
        assert_eq!(args, vec!["-s".to_string(), "game.smc".to_string()]);
        Ok(())
    }

    #[test]
    fn windowed_and_fullscreen_conflict() {
        let result = Cli::try_parse_from(["pixelflow", "run", "x", "--windowed", "--fullscreen"]);
        assert!(result.is_err(), "expected a conflict error");
    }

    #[test]
    fn run_defaults_to_window_capture_at_4x() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["pixelflow", "run", "x"])?;
        let (_, _, capture, scale, windowed, _) = run_fields(cli);
        assert_eq!(capture, "window");
        assert_eq!(scale, 4);
        assert!(!windowed);
        Ok(())
    }
}
