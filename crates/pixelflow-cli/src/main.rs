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
use pixelflow_core::{AlgorithmConfig, AlgorithmDescriptor, Processor};
use pixelflow_processing::{AlgorithmRegistry, ConfigValue};
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
        /// Processing algorithm, from the registry.
        #[arg(long, default_value = "contour")]
        algorithm: String,
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
    /// List registered processing algorithms and their
    /// capabilities, or show one algorithm in full.
    Algorithms {
        /// Algorithm id to inspect, or all algorithms when omitted.
        name: Option<String>,
    },
}

/// The built-in algorithm registry: every algorithm the
/// application can run, discovered from the registry rather
/// than from a per-algorithm match.
fn registry() -> AlgorithmRegistry {
    AlgorithmRegistry::builtin()
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

/// The configuration for `id`, from the environment: the
/// memory budget is set only when the algorithm declares a
/// `memory_budget_bytes` parameter, so an algorithm without
/// that parameter never sees an unknown key.
fn algorithm_config(registry: &AlgorithmRegistry, id: &str) -> AlgorithmConfig {
    let mut config = AlgorithmConfig::new();
    if let Ok(descriptor) = registry.descriptor(id)
        && descriptor
            .parameters
            .iter()
            .any(|parameter| parameter.name == "memory_budget_bytes")
        && let Some(bytes) = memory_budget_from_env()
        && let Ok(budget) = i64::try_from(bytes)
    {
        config.set("memory_budget_bytes", ConfigValue::Integer(budget));
    }
    config
}

fn run_upscale(input: &str, output: &str, scale: u32, algorithm_name: &str) -> Result<(), String> {
    let registry = registry();
    let descriptor = registry
        .descriptor(algorithm_name)
        .map_err(|e| e.to_string())?;
    if !descriptor.supports_scale(scale) {
        return Err(format!(
            "scale {scale} is not supported by {algorithm_name}; supported scales: {:?}",
            descriptor.supported_scales
        ));
    }
    let config = algorithm_config(&registry, algorithm_name);
    let proc = registry
        .create_processor(algorithm_name, &config)
        .map_err(|e| e.to_string())?;
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

/// One algorithm's scales as a display string, e.g. "2x, 4x".
fn scales_display(descriptor: &AlgorithmDescriptor) -> String {
    descriptor
        .supported_scales
        .iter()
        .map(|scale| format!("{scale}x"))
        .collect::<Vec<String>>()
        .join(", ")
}

fn run_backends() {
    println!("capture backends:");
    println!("  wayland  XDG Desktop Portal ScreenCast + PipeWire (window or monitor)");
    println!("presentation backends:");
    println!("  wgpu     fullscreen borderless window, aspect-ratio preserving, vsync");
    println!("processing algorithms:");
    for descriptor in registry().descriptors() {
        println!(
            "  {:<9} {}  ({})",
            descriptor.id,
            scales_display(&descriptor),
            descriptor.description
        );
    }
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
    println!("processing algorithms: {}", registry().ids().join(", "));
}

/// Print one algorithm's full capabilities, from its registry
/// descriptor.
fn print_algorithm(descriptor: &AlgorithmDescriptor) {
    println!("{} ({})", descriptor.display_name, descriptor.id);
    println!("  description: {}", descriptor.description);
    println!("  version: {}", descriptor.version);
    println!("  origin: {}", descriptor.origin);
    let input_formats: Vec<String> = descriptor
        .input_formats
        .iter()
        .map(|format| format.to_string())
        .collect();
    let output_formats: Vec<String> = descriptor
        .output_formats
        .iter()
        .map(|format| format.to_string())
        .collect();
    println!("  input formats: {}", input_formats.join(", "));
    println!("  output formats: {}", output_formats.join(", "));
    println!("  scales: {}", scales_display(descriptor));
    let backends: Vec<String> = descriptor
        .backends
        .iter()
        .map(|backend| backend.to_string())
        .collect();
    println!("  backends: {}", backends.join(", "));
    println!("  spatial dependency: {:?}", descriptor.spatial_dependency);
    println!(
        "  temporal dependency: {:?}",
        descriptor.temporal_dependency
    );
    println!("  stride support: {:?}", descriptor.stride_support);
    println!("  tile safe: {}", descriptor.tile_safe);
    println!("  stages: {}", descriptor.stages.join(", "));
    match descriptor.working_bytes_per_output_pixel {
        Some(bytes) => println!("  working memory: {bytes} bytes per output pixel"),
        None => println!("  working memory: stated by the algorithm's own budget check"),
    }
    if descriptor.intermediate_resources.is_empty() {
        println!("  intermediate resources: none");
    } else {
        println!(
            "  intermediate resources: {}",
            descriptor.intermediate_resources.join(", ")
        );
    }
    println!("  deterministic: {}", descriptor.deterministic);
    println!("  determinism: {}", descriptor.determinism_note);
    if descriptor.parameters.is_empty() {
        println!("  parameters: none");
    } else {
        println!("  parameters:");
        for parameter in &descriptor.parameters {
            println!(
                "    {}: {} (default {:?})",
                parameter.name, parameter.description, parameter.default
            );
        }
    }
}

fn run_algorithms(name: Option<&str>) -> Result<(), String> {
    let registry = registry();
    match name {
        Some(id) => {
            let descriptor = registry.descriptor(id).map_err(|e| e.to_string())?;
            print_algorithm(&descriptor);
            Ok(())
        }
        None => {
            for descriptor in registry.descriptors() {
                println!(
                    "  {:<9} {}  ({})",
                    descriptor.id,
                    scales_display(&descriptor),
                    descriptor.description
                );
            }
            Ok(())
        }
    }
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
            algorithm,
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
                algorithm: algorithm.clone(),
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
        Command::Algorithms { name } => run_algorithms(name.as_deref()),
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
    fn run_fields(cli: Cli) -> (String, Vec<String>, String, u32, String, bool, bool) {
        match cli.command {
            Command::Run {
                program,
                args,
                capture,
                scale,
                algorithm,
                windowed,
                fullscreen,
            } => (
                program, args, capture, scale, algorithm, windowed, fullscreen,
            ),
            _ => (
                String::new(),
                vec![],
                String::new(),
                0,
                String::new(),
                false,
                false,
            ),
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
        let (program, args, capture, scale, algorithm, windowed, fullscreen) = run_fields(cli);
        assert_eq!(program, "gtk-slashem");
        assert!(args.is_empty());
        assert_eq!(capture, "monitor");
        assert_eq!(scale, 2);
        assert_eq!(algorithm, "contour");
        assert!(windowed);
        assert!(!fullscreen);
        Ok(())
    }

    #[test]
    fn run_passes_trailing_args_to_the_target() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["pixelflow", "run", "zsnes", "--", "-s", "game.smc"])?;
        let (program, args, _, _, _, _, _) = run_fields(cli);
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
        let (_, _, capture, scale, algorithm, windowed, _) = run_fields(cli);
        assert_eq!(capture, "window");
        assert_eq!(scale, 4);
        assert_eq!(algorithm, "contour");
        assert!(!windowed);
        Ok(())
    }

    #[test]
    fn run_accepts_algorithm_flag() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["pixelflow", "run", "x", "--algorithm", "bilinear"])?;
        let (_, _, _, _, algorithm, _, _) = run_fields(cli);
        assert_eq!(algorithm, "bilinear");
        Ok(())
    }

    #[test]
    fn algorithms_lists_and_inspects() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["pixelflow", "algorithms"])?;
        assert!(matches!(cli.command, Command::Algorithms { name: None }));
        let cli = Cli::try_parse_from(["pixelflow", "algorithms", "contour"])?;
        assert!(matches!(cli.command, Command::Algorithms { name: Some(_) }));
        Ok(())
    }

    #[test]
    fn unknown_algorithm_names_the_available_list() {
        let result = run_upscale("no-such-input.png", "out.png", 4, "nope");
        assert!(result.is_err(), "expected an unknown-algorithm error");
        assert!(
            matches!(&result, Err(e) if e.contains("unknown algorithm") && e.contains("contour") && e.contains("nearest") && e.contains("bilinear")),
            "the error lists the registered algorithms, got {result:?}"
        );
    }

    #[test]
    fn unsupported_scale_names_the_supported_scales() {
        let result = run_upscale("no-such-input.png", "out.png", 3, "nearest");
        assert!(result.is_err(), "expected an unsupported-scale error");
        assert!(
            matches!(&result, Err(e) if e.contains("scale 3 is not supported by nearest")),
            "the error names the scale and algorithm, got {result:?}"
        );
    }
}
