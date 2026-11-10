//! The `run` command: launch the target, capture its window, run
//! each frame through the processing graph, and present the result
//! in a fullscreen window.
//!
//! The work splits across two threads. The processing thread owns the
//! capture backend, the child process, and the processing graph: it
//! pulls frames, region-analyzes each one restricted to the tiles whose pixels changed, hands the
//! routes to the graph, runs each through the graph's algorithm node,
//! and publishes the newest to a replaceable frame slot. A
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
use pixelflow_core::{
    AlgorithmConfig, DirtySet, ExecutionBackend, Frame, FrameSlot, Graph, GraphBuilder, GraphError,
    Source, TileGrid, TileRouting,
};
use pixelflow_gpu::{GpuCapability, GpuContext, backend_for, gpu_capability};
use pixelflow_present::{PresentLoop, PresentUserEvent};
use pixelflow_processing::region_analysis::{RegionAnalyzer, RegionMap, Route, RoutingProfile};
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
    /// Processing algorithm id, from the registry.
    pub algorithm: String,
    /// Windowed preview instead of fullscreen.
    pub windowed: bool,
}

/// Open capture for the requested source, falling back to monitor
/// when window capture fails. Window is the default; monitor is
/// the fallback when window capture is unavailable, per the
/// `CaptureConfig` contract. Any window initialize failure triggers
/// the one monitor attempt: the failure modes that matter here
/// (picker dismissed, backend cancelled, source unavailable) are
/// all reported by the backend as an initialize error, and the
/// fallback cannot tell a transient failure from a permanent one
/// without retrying the dialog, so it attempts the fallback once
/// and reports both failures when that also fails. The monitor
/// attempt opens a fresh portal session on the same backend: a
/// failed initialize leaves the backend uninitialized, so the
/// second call starts a new session instead of reusing the failed
/// one. Exactly one additional attempt, no retry loop.
///
/// # Errors
///
/// Returns the combined failure when the requested source and, for
/// a window request, the monitor fallback both fail to initialize.
fn initialize_capture(
    backend: &mut impl CaptureBackend,
    requested: SourceKind,
) -> Result<SourceKind, String> {
    match backend.initialize(&CaptureConfig::new(requested)) {
        Ok(()) => Ok(requested),
        Err(error) => {
            if requested != SourceKind::Window {
                return Err(format!("capture failed to start: {error}"));
            }
            eprintln!(
                "pixelflow: window capture failed to start: {error}; attempting monitor capture as fallback"
            );
            match backend.initialize(&CaptureConfig::new(SourceKind::Monitor)) {
                Ok(()) => {
                    eprintln!("pixelflow: monitor capture started after window capture failed");
                    Ok(SourceKind::Monitor)
                }
                Err(monitor_error) => Err(format!(
                    "capture failed to start: window capture failed: {error}; monitor fallback also failed: {monitor_error}"
                )),
            }
        }
    }
}

/// Resolve the requested algorithm id to the id the live
/// graph builds, given whether a GPU adapter was found.
/// Nearest is the only algorithm with a GPU implementation,
/// so the GPU backend for it is the "nearest-gpu" registry
/// id; every other algorithm keeps its own id. An explicit
/// "nearest-gpu" request with no adapter is an error, not a
/// silent CPU run: the user asked for the GPU node, and the
/// CPU fallback is the "nearest" id, chosen explicitly.
///
/// # Errors
///
/// Returns a message when "nearest-gpu" was requested and no
/// adapter exists.
fn resolve_algorithm(requested: &str, adapter_present: bool) -> Result<&str, String> {
    if requested == "nearest-gpu" && !adapter_present {
        return Err(
            "nearest-gpu requires a GPU adapter; no adapter was found on this machine — use nearest for CPU execution"
                .to_string(),
        );
    }
    if backend_for(requested, adapter_present) == ExecutionBackend::Gpu {
        Ok("nearest-gpu")
    } else {
        Ok(requested)
    }
}

/// Whether a GPU adapter exists on this machine, from the
/// adapter probe: enumeration only, no device is created.
/// Only called for an algorithm with a GPU implementation;
/// CPU-only algorithms never probe.
fn adapter_present() -> bool {
    let adapters = GpuContext::probe_adapters();
    if adapters.is_empty() {
        eprintln!("pixelflow: no GPU adapter found on this machine");
        false
    } else {
        true
    }
}

/// Launch the target, capture its window, process frames, and present
/// them until the window closes or the target exits.
///
/// # Errors
///
/// Returns a message when the algorithm is unknown, the scale is
/// not supported, the target cannot launch, the portal session
/// cannot open, capture fails, or the presentation window cannot
/// start.
pub fn run(opts: RunOptions) -> Result<(), String> {
    let registry = crate::registry();
    let descriptor = registry
        .descriptor(&opts.algorithm)
        .map_err(|e| e.to_string())?;
    if !descriptor.supports_scale(opts.scale) {
        return Err(format!(
            "scale {} is not supported by {}; supported scales: {:?}",
            opts.scale, opts.algorithm, descriptor.supported_scales
        ));
    }
    let has_adapter = (gpu_capability(&opts.algorithm) == GpuCapability::Compute
        || opts.algorithm == "nearest-gpu")
        && adapter_present();
    let algorithm = resolve_algorithm(&opts.algorithm, has_adapter)?.to_string();
    let descriptor = registry.descriptor(&algorithm).map_err(|e| e.to_string())?;
    let backend = descriptor
        .backends
        .first()
        .copied()
        .unwrap_or(ExecutionBackend::Cpu);
    eprintln!("pixelflow: algorithm {algorithm} on {backend} backend");
    let mut launcher = Launcher::spawn(&opts.program, &opts.args)?;
    std::thread::sleep(WINDOW_GRACE);
    if launcher.exited() {
        return Err(format!("{} exited before capture started", opts.program));
    }
    let mut backend = WaylandCapture::new();
    let effective_source = match initialize_capture(&mut backend, opts.source) {
        Ok(source) => source,
        Err(e) => {
            // The child is already gone or cannot be killed; the capture
            // error below is the one to report.
            match launcher.kill() {
                Ok(()) => {}
                Err(_) => {}
            }
            return Err(e);
        }
    };
    eprintln!("pixelflow: capturing {effective_source}");
    let slot = Arc::new(FrameSlot::new());
    let exit = Arc::new(AtomicBool::new(false));
    let processing_exit = exit.clone();
    let scale = opts.scale;
    let program = opts.program.clone();
    // `algorithm` is the effective id from the backend
    // resolution above: the GPU node id when an adapter is
    // present, the requested id otherwise. The worker builds
    // its graph from this id.
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
            &algorithm,
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

/// Build the live processing graph: the registry algorithm's
/// nodes fed by the graph input, exposing the last stage's
/// output. The nodes come from the registry, so the live path
/// runs whichever algorithm the user selected — Contour's node
/// declares the Global spatial dependency, so the graph runs
/// it whole-frame; there is no tile-level incremental claim
/// for it. The text and sprite branches and the general path
/// all route to the first stage: no per-branch processors exist
/// yet, so routing changes no pixels. Built fresh in the
/// processing thread, which owns the graph exactly as it owns
/// the capture backend and the child process.
///
/// # Errors
///
/// Returns [`GraphError`] when the wiring fails to validate,
/// which means a programming error in this function or an
/// algorithm whose nodes do not wire linearly.
fn build_algorithm_graph(
    algorithm: &str,
    config: &AlgorithmConfig,
    scale: u32,
) -> Result<Graph, GraphError> {
    // The full registry, including the GPU factory: a
    // "nearest-gpu" graph builds its node on the GPU context
    // the factory creates, and fails loudly when no adapter
    // exists, instead of substituting the CPU node.
    let registry = crate::registry();
    let descriptor = registry
        .descriptor(algorithm)
        .map_err(|error| GraphError::Internal(error.to_string()))?;
    let nodes = registry
        .create_nodes(algorithm, config, scale)
        .map_err(|error| GraphError::Internal(error.to_string()))?;
    if nodes.len() != descriptor.stages.len() || nodes.is_empty() {
        return Err(GraphError::Internal(format!(
            "{algorithm} declares {} stages but built {} nodes",
            descriptor.stages.len(),
            nodes.len()
        )));
    }
    let input_format = descriptor
        .input_formats
        .first()
        .copied()
        .ok_or_else(|| GraphError::Internal(format!("{algorithm} declares no input format")))?;
    let first = descriptor
        .stages
        .first()
        .copied()
        .ok_or_else(|| GraphError::Internal(format!("{algorithm} declares no stages")))?;
    let last = descriptor
        .stages
        .last()
        .copied()
        .ok_or_else(|| GraphError::Internal(format!("{algorithm} declares no stages")))?;
    let mut builder = GraphBuilder::new();
    // `build` takes the builder by value, so the chained
    // `&mut Self` is moved out with `take`, not discarded:
    // the same shape as the graph tests.
    let mut current = builder.add_input("in", input_format);
    for (stage, node) in descriptor.stages.iter().zip(nodes) {
        current = current.add_node(stage, node);
    }
    current = current.connect(Source::input("in"), first, "in");
    for pair in descriptor.stages.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        current = current.connect(Source::node(from, "out"), to, "in");
    }
    current = current
        .route_branch("text", first)
        .route_branch("sprite", first)
        .route_general(first)
        .add_output("out", last, "out");
    std::mem::take(current).build()
}

/// Per-frame region state for the live path: the analyzer, the
/// routing profile, and the previous frame and map. One value
/// lives in the processing thread for the whole run; it is not
/// shared with the presentation thread.
#[derive(Default)]
struct FrameRouting {
    /// The analyzer that classifies tiles.
    analyzer: RegionAnalyzer,
    /// Which branch each class routes to.
    profile: RoutingProfile,
    /// The previous frame, for the dirty-set diff.
    previous_frame: Option<Frame>,
    /// The previous frame's region map, for dirty-restricted
    /// analysis and class-change halos.
    previous_map: Option<RegionMap>,
}

impl FrameRouting {
    /// Analyze `frame` into a region map and hand its routes
    /// and halo invalidation to `graph`, then retain the frame
    /// and map for the next frame.
    ///
    /// Analysis is dirty-restricted: only tiles whose pixels
    /// changed since the previous frame are re-analyzed, and
    /// clean tiles carry their previous classification
    /// forward. The first frame has no previous frame, so
    /// every tile is dirty and analyzed. The invalidation set
    /// is the pixel dirty set plus the halo around tiles that
    /// changed class into a routed branch, so neighbors a
    /// branch reads across are re-processed with the tile.
    /// The frame is cloned once to retain it for the next
    /// frame's diff: the graph still receives the original by
    /// value, and the clone is the retention cost of
    /// dirty-restricted analysis.
    fn route_frame(&mut self, graph: &mut Graph, frame: &Frame) {
        let grid = TileGrid::from_dims(frame.width(), frame.height());
        let dirty = match &self.previous_frame {
            Some(previous) => DirtySet::diff(previous, frame),
            None => DirtySet::whole(grid),
        };
        let map = self
            .analyzer
            .analyze_dirty(frame, &dirty, &[], self.previous_map.as_ref());
        let invalidated = match &self.previous_map {
            Some(previous) => dirty.union(&map.class_change_halos(previous, &self.profile)),
            None => dirty,
        };
        let routes = map.routes(&self.profile);
        let mut routing = TileRouting::empty(grid);
        let mut index = 0_usize;
        for ty in 0..grid.rows() {
            for tx in 0..grid.cols() {
                if let Some(route) = routes.get(index) {
                    let branch = match route.route {
                        Route::Branch(name) => Some(name),
                        Route::General => None,
                    };
                    routing.set_branch(tx, ty, branch);
                }
                index += 1;
            }
        }
        graph.set_routing(routing, invalidated);
        self.previous_map = Some(map);
        self.previous_frame = Some(frame.clone());
    }
}

/// Run one captured frame through `graph` and return the
/// processed frame. Routing runs first: `routing` analyzes
/// the frame dirty-restricted and hands its routes and halo
/// invalidation to the graph, so the graph knows the routing
/// of the frame it is about to run. Ownership of the frame
/// then moves into [`Graph::run`], which takes it by value,
/// drops it once the node has consumed it, and returns the
/// output frame by value. The routing keeps one retained copy
/// of the frame for the next frame's diff; that clone is the
/// retention cost of dirty-restricted analysis. The
/// incremental executor is deliberately not used here: with a
/// single Global node it would diff the whole frame, retain
/// the previous frame and a cached output, and clone the
/// output, for output identical to this whole-frame run.
///
/// # Errors
///
/// Returns [`GraphError`] when the frame is not RGB8 or the
/// algorithm node fails.
fn process_frame(
    graph: &mut Graph,
    frame: Frame,
    routing: &mut FrameRouting,
) -> Result<Frame, GraphError> {
    routing.route_frame(graph, &frame);
    let mut outputs = graph.run(vec![("in", frame)])?;
    match outputs.pop() {
        Some((_, frame)) => Ok(frame),
        None => Err(GraphError::Internal(
            "algorithm graph produced no output".to_string(),
        )),
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
    algorithm: &str,
    scale: u32,
    slot: Arc<FrameSlot>,
    wake: impl Fn() -> bool,
    exit: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut result: Result<(), String> = Ok(());
    let registry = crate::registry();
    let config = crate::algorithm_config(&registry, algorithm);
    match build_algorithm_graph(algorithm, &config, scale) {
        Ok(mut graph) => {
            let mut routing = FrameRouting::default();
            while !exit.load(Ordering::Relaxed) {
                match backend.capture_frame() {
                    Ok(Some(frame)) => match process_frame(&mut graph, frame, &mut routing) {
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
        }
        Err(e) => {
            result = Err(format!("processing graph failed to build: {e}"));
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
    use pixelflow_capture::CaptureError;
    use pixelflow_core::{PixelFormat, Processor};
    use pixelflow_processing::Contour;
    use pixelflow_processing::region_analysis::ContentClass;

    /// Build an RGB8 frame from a per-pixel function.
    fn pixel_frame(
        width: u32,
        height: u32,
        pixel: impl Fn(u32, u32) -> [u8; 3],
    ) -> Result<Frame, String> {
        let mut data = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&pixel(x, y));
            }
        }
        Frame::new(width, height, PixelFormat::Rgb8, data).map_err(|e| e.to_string())
    }

    /// True when the pixel is ink in the text tile's bars, in
    /// tile-local coordinates.
    fn text_tile_ink(x: u32, y: u32) -> bool {
        (x == 1 && (2..11).contains(&y))
            || (x == 5 && (2..8).contains(&y))
            || (x == 9 && (5..14).contains(&y))
            || (x == 13 && (4..15).contains(&y))
    }

    /// A 32x16 frame: a text tile on the left, a flat tile on
    /// the right.
    fn text_and_flat_frame() -> Result<Frame, String> {
        pixel_frame(32, 16, |x, y| {
            if x < 16 {
                if text_tile_ink(x, y) {
                    [20, 20, 20]
                } else {
                    [240, 240, 240]
                }
            } else {
                [200, 30, 90]
            }
        })
    }

    /// A 48x16 frame: three flat tiles.
    fn three_flat_frame() -> Result<Frame, String> {
        pixel_frame(48, 16, |_, _| [200, 30, 90])
    }

    /// The same frame with the middle tile replaced by the
    /// text tile's bars.
    fn flat_text_flat_frame() -> Result<Frame, String> {
        pixel_frame(48, 16, |x, y| {
            if (16..32).contains(&x) {
                if text_tile_ink(x - 16, y) {
                    [20, 20, 20]
                } else {
                    [240, 240, 240]
                }
            } else {
                [200, 30, 90]
            }
        })
    }

    #[test]
    fn unsupported_scale_fails_before_launching() {
        let result = run(RunOptions {
            program: "pixelflow-definitely-not-a-program".to_string(),
            args: vec![],
            source: SourceKind::Window,
            scale: 3,
            algorithm: "contour".to_string(),
            windowed: true,
        });
        assert!(result.is_err(), "expected a scale error");
        assert!(
            matches!(&result, Err(e) if e.contains("scale 3 is not supported")),
            "the error names the scale, not the missing program"
        );
    }

    #[test]
    fn unknown_algorithm_fails_before_launching() {
        let result = run(RunOptions {
            program: "pixelflow-definitely-not-a-program".to_string(),
            args: vec![],
            source: SourceKind::Window,
            scale: 4,
            algorithm: "nope".to_string(),
            windowed: true,
        });
        assert!(result.is_err(), "expected an unknown-algorithm error");
        assert!(
            matches!(&result, Err(e) if e.contains("unknown algorithm") && e.contains("contour")),
            "the error lists the registered algorithms, got {result:?}"
        );
    }

    #[test]
    fn target_that_exits_at_once_is_an_error() {
        let result = run(RunOptions {
            program: "/bin/true".to_string(),
            args: vec![],
            source: SourceKind::Window,
            scale: 4,
            algorithm: "contour".to_string(),
            windowed: true,
        });
        assert!(result.is_err(), "expected an error for an exited target");
        assert!(
            matches!(&result, Err(e) if e.contains("exited before capture started")),
            "the error names the early exit"
        );
    }

    /// A capture backend that succeeds only for the listed sources,
    /// recording every initialize call, for the fallback tests.
    struct StubBackend {
        succeeds: Vec<SourceKind>,
        attempts: Vec<SourceKind>,
    }

    impl StubBackend {
        fn new(succeeds: Vec<SourceKind>) -> StubBackend {
            StubBackend {
                succeeds,
                attempts: Vec::new(),
            }
        }
    }

    impl CaptureBackend for StubBackend {
        fn initialize(&mut self, config: &CaptureConfig) -> Result<(), CaptureError> {
            self.attempts.push(config.source);
            if self.succeeds.contains(&config.source) {
                Ok(())
            } else {
                Err(CaptureError::SessionFailed(format!(
                    "{} capture unavailable in the stub",
                    config.source
                )))
            }
        }

        fn capture_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
            Ok(None)
        }

        fn shutdown(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    #[test]
    fn window_success_needs_no_fallback() {
        let mut backend = StubBackend::new(vec![SourceKind::Window]);
        let result = initialize_capture(&mut backend, SourceKind::Window);
        assert_eq!(result, Ok(SourceKind::Window));
        assert_eq!(backend.attempts, vec![SourceKind::Window]);
    }

    #[test]
    fn window_failure_falls_back_to_monitor() {
        let mut backend = StubBackend::new(vec![SourceKind::Monitor]);
        let result = initialize_capture(&mut backend, SourceKind::Window);
        assert_eq!(result, Ok(SourceKind::Monitor));
        assert_eq!(
            backend.attempts,
            vec![SourceKind::Window, SourceKind::Monitor],
            "the fallback attempts monitor on the same backend after window fails"
        );
    }

    #[test]
    fn both_failures_are_named_in_the_error() {
        let mut backend = StubBackend::new(vec![]);
        let result = initialize_capture(&mut backend, SourceKind::Window);
        assert!(result.is_err(), "expected both attempts to fail");
        if let Err(error) = result {
            assert!(
                error.contains("window capture failed"),
                "the error names the window failure: {error}"
            );
            assert!(
                error.contains("monitor fallback also failed"),
                "the error names the monitor failure: {error}"
            );
            assert!(
                error.contains("window capture unavailable"),
                "the error carries the window failure itself: {error}"
            );
            assert!(
                error.contains("monitor capture unavailable"),
                "the error carries the monitor failure itself: {error}"
            );
        }
        assert_eq!(
            backend.attempts,
            vec![SourceKind::Window, SourceKind::Monitor],
            "exactly one additional attempt, no retry loop"
        );
    }

    #[test]
    fn monitor_request_does_not_fall_back() {
        let mut backend = StubBackend::new(vec![]);
        let result = initialize_capture(&mut backend, SourceKind::Monitor);
        assert!(result.is_err(), "expected the monitor attempt to fail");
        if let Err(error) = result {
            assert!(
                !error.contains("fallback"),
                "a monitor request has no fallback to attempt: {error}"
            );
        }
        assert_eq!(backend.attempts, vec![SourceKind::Monitor]);
    }

    #[test]
    fn gpu_selection_uses_the_gpu_node_with_an_adapter() {
        assert_eq!(resolve_algorithm("nearest", true), Ok("nearest-gpu"));
        assert_eq!(resolve_algorithm("nearest", false), Ok("nearest"));
        assert_eq!(resolve_algorithm("contour", true), Ok("contour"));
        assert_eq!(resolve_algorithm("bilinear", true), Ok("bilinear"));
        assert_eq!(resolve_algorithm("nearest-gpu", true), Ok("nearest-gpu"));
    }

    #[test]
    fn explicit_gpu_algorithm_without_an_adapter_is_an_error() {
        let result = resolve_algorithm("nearest-gpu", false);
        assert!(result.is_err(), "expected an adapter error");
        assert!(
            matches!(&result, Err(e) if e.contains("requires a GPU adapter") && e.contains("nearest")),
            "the error names the missing adapter and the CPU fallback, got {result:?}"
        );
    }

    #[test]
    fn nearest_cpu_graph_matches_direct_processor_output() -> Result<(), String> {
        use pixelflow_processing::Nearest;
        let mut data = Vec::with_capacity(4 * 4 * 3);
        for y in 0..4 {
            for x in 0..4 {
                let v: u8 = if x + y < 3 {
                    20
                } else if x + y < 5 {
                    128
                } else {
                    240
                };
                data.extend_from_slice(&[v, v, v]);
            }
        }
        let frame = Frame::new(4, 4, PixelFormat::Rgb8, data).map_err(|e| e.to_string())?;
        let direct = Nearest.process(&frame, 2).map_err(|e| e.to_string())?;
        let mut graph = build_algorithm_graph("nearest", &AlgorithmConfig::new(), 2)
            .map_err(|e| e.to_string())?;
        let processed = process_frame(&mut graph, frame, &mut FrameRouting::default())
            .map_err(|e| e.to_string())?;
        assert_eq!((processed.width(), processed.height()), (8, 8));
        assert_eq!(processed.data(), direct.data());
        Ok(())
    }

    #[test]
    fn nearest_gpu_graph_build_fails_loudly_without_an_adapter() {
        // The probe and the graph build enumerate the same
        // adapters, so they agree: no adapter fails the
        // build with the reason, and an adapter builds it.
        if GpuContext::probe_adapters().is_empty() {
            match build_algorithm_graph("nearest-gpu", &AlgorithmConfig::new(), 2) {
                Err(error) => {
                    assert!(
                        error.to_string().contains("no GPU adapter"),
                        "the error names the missing adapter, got {error}"
                    );
                }
                Ok(_) => panic!("expected a no-adapter error"),
            }
        } else {
            let graph = build_algorithm_graph("nearest-gpu", &AlgorithmConfig::new(), 2);
            assert!(graph.is_ok(), "an adapter builds the GPU graph");
        }
    }

    #[test]
    fn contour_graph_matches_direct_processor_output() -> Result<(), String> {
        let mut data = Vec::with_capacity(4 * 4 * 3);
        for y in 0..4 {
            for x in 0..4 {
                let v: u8 = if x + y < 3 {
                    20
                } else if x + y < 5 {
                    128
                } else {
                    240
                };
                data.extend_from_slice(&[v, v, v]);
            }
        }
        let frame = Frame::new(4, 4, PixelFormat::Rgb8, data).map_err(|e| e.to_string())?;
        let direct = Contour.process(&frame, 2).map_err(|e| e.to_string())?;
        let mut graph = build_algorithm_graph("contour", &AlgorithmConfig::new(), 2)
            .map_err(|e| e.to_string())?;
        let processed = process_frame(&mut graph, frame, &mut FrameRouting::default())
            .map_err(|e| e.to_string())?;
        assert_eq!((processed.width(), processed.height()), (8, 8));
        assert_eq!(processed.format(), PixelFormat::Rgb8);
        assert_eq!(processed.data(), direct.data());
        Ok(())
    }

    #[test]
    fn bilinear_graph_matches_direct_processor_output() -> Result<(), String> {
        use pixelflow_processing::Bilinear;
        let mut data = Vec::with_capacity(4 * 4 * 3);
        for y in 0..4 {
            for x in 0..4 {
                let v: u8 = if x + y < 3 {
                    20
                } else if x + y < 5 {
                    128
                } else {
                    240
                };
                data.extend_from_slice(&[v, v, v]);
            }
        }
        let frame = Frame::new(4, 4, PixelFormat::Rgb8, data).map_err(|e| e.to_string())?;
        let direct = Bilinear.process(&frame, 2).map_err(|e| e.to_string())?;
        let mut graph = build_algorithm_graph("bilinear", &AlgorithmConfig::new(), 2)
            .map_err(|e| e.to_string())?;
        let processed = process_frame(&mut graph, frame, &mut FrameRouting::default())
            .map_err(|e| e.to_string())?;
        assert_eq!((processed.width(), processed.height()), (8, 8));
        assert_eq!(processed.data(), direct.data());
        Ok(())
    }

    #[test]
    fn routed_frame_resolves_all_routes_to_contour() -> Result<(), String> {
        let frame = text_and_flat_frame()?;
        let direct = Contour.process(&frame, 2).map_err(|e| e.to_string())?;
        let mut graph = build_algorithm_graph("contour", &AlgorithmConfig::new(), 2)
            .map_err(|e| e.to_string())?;
        let mut routing = FrameRouting::default();
        let processed =
            process_frame(&mut graph, frame, &mut routing).map_err(|e| e.to_string())?;
        // Routing changes no pixels: every route resolves to
        // the contour node.
        assert_eq!(processed.data(), direct.data());
        let table = graph
            .routing()
            .ok_or_else(|| "graph has no routing after a routed frame".to_string())?;
        assert_eq!(table.branch_for(0, 0), Some("text"));
        assert_eq!(table.branch_for(1, 0), None);
        assert_eq!(table.routed_count(), 1);
        assert_eq!(graph.node_for_tile(0, 0), Some("contour"));
        assert_eq!(graph.node_for_tile(1, 0), Some("contour"));
        // First frame: every tile is dirty.
        let invalidated = graph
            .routing_invalidated()
            .ok_or_else(|| "graph has no invalidation after a routed frame".to_string())?;
        assert_eq!(invalidated.count(), 2);
        Ok(())
    }

    #[test]
    fn second_frame_reuses_prior_classes_and_halo_invalidates() -> Result<(), String> {
        let mut graph = build_algorithm_graph("contour", &AlgorithmConfig::new(), 2)
            .map_err(|e| e.to_string())?;
        let mut routing = FrameRouting::default();
        let first = three_flat_frame()?;
        process_frame(&mut graph, first, &mut routing).map_err(|e| e.to_string())?;
        let first_map = routing
            .previous_map
            .clone()
            .ok_or_else(|| "no region map after the first frame".to_string())?;
        assert_eq!(
            first_map.tile(1, 0).map(|tile| tile.class),
            Some(ContentClass::Flat)
        );
        // Only the middle tile changes, into a text tile.
        let second = flat_text_flat_frame()?;
        let second_again = second.clone();
        let direct = Contour.process(&second, 2).map_err(|e| e.to_string())?;
        let processed =
            process_frame(&mut graph, second, &mut routing).map_err(|e| e.to_string())?;
        assert_eq!(processed.data(), direct.data());
        let second_map = routing
            .previous_map
            .as_ref()
            .ok_or_else(|| "no region map after the second frame".to_string())?;
        // The clean tiles are carried forward exactly; the
        // dirty tile is reclassified as text.
        assert_eq!(second_map.tile(0, 0), first_map.tile(0, 0));
        assert_eq!(second_map.tile(2, 0), first_map.tile(2, 0));
        assert_eq!(
            second_map.tile(1, 0).map(|tile| tile.class),
            Some(ContentClass::Text)
        );
        assert_eq!(second_map.class_changes_since(&first_map), 1);
        // The class change invalidates the tile and its halo:
        // the pixel dirty set holds the middle tile, the halo
        // adds both neighbors.
        let invalidated = graph
            .routing_invalidated()
            .ok_or_else(|| "graph has no invalidation after the second frame".to_string())?;
        assert_eq!(invalidated.count(), 3);
        assert!(invalidated.is_dirty(0, 0));
        assert!(invalidated.is_dirty(1, 0));
        assert!(invalidated.is_dirty(2, 0));
        // An identical third frame invalidates nothing.
        let direct = Contour
            .process(&second_again, 2)
            .map_err(|e| e.to_string())?;
        let processed =
            process_frame(&mut graph, second_again, &mut routing).map_err(|e| e.to_string())?;
        assert_eq!(processed.data(), direct.data());
        let invalidated = graph
            .routing_invalidated()
            .ok_or_else(|| "graph has no invalidation after the third frame".to_string())?;
        assert_eq!(invalidated.count(), 0);
        Ok(())
    }
}
