//! PixelFlow command-line interface.
//!
//! Milestone 1 commands only: `upscale` (PNG in, PNG out),
//! `backends`, and `info`. There is no `run` command yet; launching
//! and capturing a live application is Milestone 2, and a stub that
//! pretended to do it would be worse than no command at all.

mod png_io;

use clap::{Parser, Subcommand};
use pixelflow_core::{ProcessError, Processor};
use pixelflow_processing::{Nearest, Upscale20};
use std::process::ExitCode;

/// PixelFlow: cross-platform pixel-art upscaling wrapper (Milestone 1).
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
        #[arg(long, default_value = "upscale20")]
        algorithm: String,
    },
    /// List available capture and presentation backends.
    Backends,
    /// Report hardware, display, and backend capabilities.
    Info,
}

fn algorithm(name: &str) -> Result<Box<dyn Processor>, String> {
    match name {
        "upscale20" => Ok(Box::new(Upscale20)),
        "nearest" => Ok(Box::new(Nearest)),
        other => Err(format!("unknown algorithm {other:?}; available: upscale20, nearest")),
    }
}

fn run_upscale(input: &str, output: &str, scale: u32, algorithm_name: &str) -> Result<(), String> {
    let proc = algorithm(algorithm_name)?;
    if !proc.supported_scales().contains(&scale) {
        return Err(ProcessError::UnsupportedScale { algorithm: proc.name(), scale }.to_string());
    }
    let frame = png_io::load_png(input)?;
    eprintln!("input {}x{} {} scale {}x algorithm {}", frame.width, frame.height, frame.format, scale, proc.name());
    let t0 = std::time::Instant::now();
    let out = proc.process(&frame, scale).map_err(|e| e.to_string())?;
    eprintln!("output {}x{} in {:.1}s", out.width, out.height, t0.elapsed().as_secs_f32());
    png_io::save_png(output, &out)?;
    eprintln!("wrote {output}");
    Ok(())
}

fn run_backends() {
    println!("capture backends:");
    println!("  (none implemented yet — Milestone 2)");
    println!("presentation backends:");
    println!("  (none implemented yet — Milestone 3)");
    println!("processing algorithms:");
    println!("  upscale20  2x, 4x  (algorithm 20: gated merge, adaptive labelset, edge follower)");
    println!("  nearest    2x, 4x  (nearest-neighbour baseline)");
}

fn run_info() {
    println!("pixelflow {}", env!("CARGO_PKG_VERSION"));
    println!("os: {}  arch: {}", std::env::consts::OS, std::env::consts::ARCH);
    match std::thread::available_parallelism() {
        Ok(n) => println!("cpus: {n}"),
        Err(_) => println!("cpus: unknown"),
    }
    println!("display: not probed yet (presentation is Milestone 3)");
    println!("capture: not implemented yet (Milestone 2)");
    println!("processing algorithms: upscale20, nearest");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Upscale { input, output, scale, algorithm } => {
            run_upscale(input, output, *scale, algorithm)
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
