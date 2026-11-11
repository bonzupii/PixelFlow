//! The `run` command: launch the target, intercept its presented
//! frames, run each frame through the processing graph, and present
//! the result in a fullscreen window.
//!
//! The work splits across two threads. The processing thread owns the
//! capture backend, the child process, and the processing graphs: it
//! pulls frames, region-analyzes each one restricted to the tiles whose pixels changed, hands the
//! routes to the graphs, runs each frame through only the graphs its
//! tiles need — one graph per algorithm the routing table names —
//! composites the branch outputs tile by tile, and publishes the
//! newest to a replaceable frame slot. A
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
//! The target is launched with the interception shim preloaded, so
//! its swap calls stream frames to the capture backend instead of
//! presenting to the target's own window. There is no portal session
//! and no source selection: interception happens at the program's
//! own rendering boundary, and the program's own presentation is
//! replaced, not recorded.

use crate::launcher::Launcher;
use pixelflow_capture::{CaptureBackend, CaptureConfig};
use pixelflow_core::{
    AlgorithmConfig, DirtySet, ExecutionBackend, Frame, FrameSlot, Graph, GraphBuilder, GraphError,
    Source, TileGrid, TileRouting,
};
use pixelflow_gpu::{GpuCapability, GpuContext, backend_for, gpu_capability};
use pixelflow_intercept::InterceptCapture;
use pixelflow_present::{PresentLoop, PresentUserEvent};
use pixelflow_processing::region_analysis::{
    ContentClass, RegionAnalyzer, RegionMap, Route, RoutingProfile,
    resolve_algorithm as resolve_class_algorithm,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long to wait, after launching, for the target either to
/// deliver its first intercepted frame or to exit. A target that
/// exits before delivering a frame never started; a target that is
/// still running after the wait may simply be slow to its first
/// swap, and the processing loop keeps waiting for it.
const STARTUP_WAIT: Duration = Duration::from_secs(2);

/// Options for one `run` invocation.
pub struct RunOptions {
    /// Program to launch.
    pub program: String,
    /// Arguments for the program.
    pub args: Vec<String>,
    /// Integer scale factor, 2 or 4.
    pub scale: u32,
    /// Processing algorithm id, from the registry.
    pub algorithm: String,
    /// Windowed preview instead of fullscreen.
    pub windowed: bool,
}

/// Run a best-effort shutdown step and drop its result: the
/// caller is already reporting the failure that made the step
/// necessary, and a kill of a child that already exited, or a
/// shutdown of a backend that never started, is not a second
/// failure worth reporting.
fn best_effort<T, E>(step: Result<T, E>) {
    match step {
        Ok(_) => {}
        Err(_) => {}
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

/// Launch the target, intercept its presented frames, process
/// frames, and present them until the window closes or the target
/// exits.
///
/// # Errors
///
/// Returns a message when the algorithm is unknown, the scale is
/// not supported, the target cannot launch, interception fails to
/// start, capture fails, or the presentation window cannot start.
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
    // Bind the interception listener first, then launch the target
    // with the shim preloaded: the shim connects on the target's
    // first present call, so the listener must exist before the
    // target can produce a frame. The reader starts before the
    // startup wait, so no early frame is lost while waiting. The
    // shim matches the target's ELF class: a 32-bit target with no
    // 32-bit shim built fails here, loudly, instead of running
    // with a shim the loader silently refuses and capturing zero
    // frames.
    let mut backend = InterceptCapture::new().map_err(|e| e.to_string())?;
    let env = match backend.environment_for(&opts.program, &opts.args) {
        Ok(env) => env,
        Err(e) => {
            best_effort(backend.shutdown());
            return Err(e);
        }
    };
    let mut launcher = match Launcher::spawn_with_env(&opts.program, &opts.args, &env) {
        Ok(launcher) => launcher,
        Err(e) => {
            best_effort(backend.shutdown());
            return Err(e);
        }
    };
    // Interception ignores the source kind: there is no window or
    // monitor to select. The default config only satisfies the
    // trait's signature.
    if let Err(e) = backend.initialize(&CaptureConfig::default()) {
        best_effort(launcher.kill());
        return Err(format!("capture failed to start: {e}"));
    }
    // Wait for the first intercepted frame, or for the target to
    // exit without producing one. A target that is still running
    // after the wait may be slow to its first swap; the processing
    // loop keeps waiting for it.
    let deadline = std::time::Instant::now() + STARTUP_WAIT;
    while std::time::Instant::now() < deadline {
        if backend.frames_received() > 0 {
            break;
        }
        if launcher.exited() {
            best_effort(backend.shutdown());
            return Err(format!("{} exited before capture started", opts.program));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("pixelflow: intercepting {}", opts.program);
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
            best_effort(backend.shutdown());
            best_effort(launcher.kill());
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

/// Build one algorithm's graph: the registry algorithm's
/// nodes fed by the graph input, exposing the last stage's
/// output. The nodes come from the registry, so the graph
/// runs whichever algorithm `algorithm` names — Contour's node
/// declares the Global spatial dependency, so the graph runs
/// it whole-frame; there is no tile-level incremental claim
/// for it. `branches` are the branch names this graph
/// serves, routed to the first stage; `general` routes the
/// general path to the first stage too, when this graph
/// serves it. A graph serves exactly the branches its
/// algorithm is tabled for, so a tile resolves through
/// the table first and the graph that owns the tile's
/// branch is asked. Built fresh in the processing thread, which owns
/// the graph exactly as it owns the capture backend and the
/// child process.
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
    branches: &[&'static str],
    general: bool,
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
    // The check above proves the stage list is non-empty: an
    // empty list builds zero nodes and fails there, and a
    // length mismatch fails too. Indexing is exact here.
    let first = descriptor.stages[0];
    let last = descriptor.stages[descriptor.stages.len() - 1];
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
    for branch in branches {
        current = current.route_branch(branch, first);
    }
    if general {
        current = current.route_general(first);
    }
    current = current.add_output("out", last, "out");
    std::mem::take(current).build()
}

/// One graph per algorithm in the routed set, built once
/// for the run: the general-path algorithm plus every
/// algorithm the routing table names. A frame runs only
/// through the graphs its tiles need; each graph runs
/// whole-frame and the outputs are composited tile by
/// tile, so a tile is processed by the algorithm its class
/// resolves to. The graphs are built from the registry, so
/// no algorithm is named in a match arm here.
struct RoutedGraphs<'a> {
    /// Algorithm id and its graph, in first-use class order.
    graphs: Vec<(&'a str, Graph)>,
    /// The general-path algorithm id.
    general: &'a str,
    /// Scale factor the graphs were built at.
    scale: u32,
}

impl<'a> RoutedGraphs<'a> {
    /// The algorithm id that processes a tile of `class`
    /// under this run's general path.
    #[must_use]
    pub fn algorithm_for(&self, class: ContentClass) -> &'a str {
        resolve_class_algorithm(class, self.general)
    }

    /// The algorithm ids the frame's tiles need, in
    /// first-use tile order: one entry per distinct
    /// resolved id over the map's tiles.
    #[must_use]
    pub fn needed_ids(&self, map: &RegionMap) -> Vec<&'a str> {
        let mut ids: Vec<&'a str> = Vec::new();
        for tile in map.tiles() {
            let id = self.algorithm_for(tile.class);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }
}

/// Build the routed graphs for a run: one graph per
/// algorithm [`RoutingProfile::routed_algorithms`] names
/// for `general`, each serving its branches and the general
/// path when it is the general algorithm. Each algorithm
/// gets its own configuration from the environment, so a
/// memory budget reaches Contour and no other algorithm.
///
/// # Errors
///
/// Returns a message when any algorithm in the routed set
/// is unknown, unsupported at the scale, or fails to wire.
fn build_routed_graphs<'a>(
    general: &'a str,
    scale: u32,
    profile: &RoutingProfile,
) -> Result<RoutedGraphs<'a>, String> {
    let registry = crate::registry();
    let mut graphs: Vec<(&'a str, Graph)> = Vec::new();
    for entry in profile.routed_algorithms(general) {
        let config = crate::algorithm_config(&registry, entry.id);
        let graph = build_algorithm_graph(entry.id, &config, scale, &entry.branches, entry.general)
            .map_err(|error| error.to_string())?;
        graphs.push((entry.id, graph));
    }
    Ok(RoutedGraphs {
        graphs,
        general,
        scale,
    })
}

/// Composite the branch outputs into one frame: each
/// output tile comes from the graph of the algorithm its
/// class resolves to, starting from the general graph's
/// output and overwriting the tiles other algorithms own.
/// Tile-aligned, no blending: a tile is produced entirely
/// by one branch, per section 9 of the specification. Every
/// branch output is a whole-frame run, so each tile is
/// computed with its full frame context and the branch halo
/// is not needed to avoid seams here. Where Contour is the
/// route, the tile is copied unchanged from the direct
/// Contour output, so Contour output is byte-identical to
/// the direct run.
///
/// # Errors
///
/// Returns [`GraphError::Internal`] when a tile resolves to
/// an algorithm with no output, which means a programming
/// error in the caller: every needed algorithm was run.
fn composite_routed_outputs(
    base: Frame,
    base_id: &str,
    outputs: &[(&str, Frame)],
    map: &RegionMap,
    general: &str,
    scale: u32,
) -> Result<Frame, GraphError> {
    let mut out = base;
    let grid = map.grid();
    for ty in 0..grid.rows() {
        for tx in 0..grid.cols() {
            let class = map
                .tile(tx, ty)
                .map_or(ContentClass::Unknown, |tile| tile.class);
            let id = resolve_class_algorithm(class, general);
            if id == base_id {
                continue;
            }
            let (_, source) = outputs
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .ok_or_else(|| {
                    GraphError::Internal(format!("routed run produced no output for {id}"))
                })?;
            let (x0, y0, x1, y1) = grid.tile_rect(tx, ty);
            let (ox0, oy0, ox1) = (
                x0 as usize * scale as usize,
                y0 as usize * scale as usize,
                x1 as usize * scale as usize,
            );
            for oy in oy0..(y1 as usize * scale as usize) {
                let row = source.row(oy as u32);
                let dst = out.row_mut(oy as u32);
                dst[ox0 * 3..ox1 * 3].copy_from_slice(&row[ox0 * 3..ox1 * 3]);
            }
        }
    }
    Ok(out)
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
    /// Analyze `frame` into a region map and build its tile
    /// routing and halo invalidation, then retain the frame
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
    /// frame's diff, and the map is cloned once for the same
    /// reason; the caller keeps the returned map for the
    /// frame it is about to run.
    fn analyze(&mut self, frame: &Frame) -> (RegionMap, TileRouting, DirtySet) {
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
        self.previous_map = Some(map.clone());
        self.previous_frame = Some(frame.clone());
        (map, routing, invalidated)
    }
}

/// Run one captured frame through the routed graphs and
/// return the composited frame. Routing runs first: the
/// frame is analyzed dirty-restricted and the routes are
/// handed to every graph, so each graph's routing table
/// names the branches it serves. Then only the graphs the
/// frame's tiles need run: the first takes the frame by value
/// and each later one takes a clone, and the outputs are
/// composited tile by tile by
/// [`composite_routed_outputs`]. A frame with no tile of a
/// class never runs that class's algorithm: an all-flat
/// frame never runs Contour. The incremental executor is
/// deliberately not used here: each branch graph runs
/// whole-frame, so a branch output is computed with its
/// full frame context.
///
/// # Errors
///
/// Returns [`GraphError`] when the frame is not RGB8, a
/// branch graph fails, or a tile resolves to an algorithm
/// with no output.
fn process_frame_routed(
    graphs: &mut RoutedGraphs<'_>,
    frame: Frame,
    routing: &mut FrameRouting,
) -> Result<Frame, GraphError> {
    let (map, tile_routing, invalidated) = routing.analyze(&frame);
    for (_, graph) in &mut graphs.graphs {
        graph.set_routing(tile_routing.clone(), invalidated.clone());
    }
    let needed = graphs.needed_ids(&map);
    let mut outputs: Vec<(&str, Frame)> = Vec::with_capacity(needed.len());
    let mut input = Some(frame);
    for (index, id) in needed.iter().enumerate() {
        // The last graph takes the frame by value; every
        // earlier graph takes a clone, so a frame runs
        // through N graphs with N - 1 clones.
        let frame_in = if index + 1 == needed.len() {
            input.take().ok_or_else(|| {
                GraphError::Internal("routed run lost its input frame".to_string())
            })?
        } else {
            match input.as_ref() {
                Some(frame) => frame.clone(),
                None => {
                    return Err(GraphError::Internal(
                        "routed run lost its input frame".to_string(),
                    ));
                }
            }
        };
        let (_, graph) = graphs
            .graphs
            .iter_mut()
            .find(|(candidate, _)| *candidate == *id)
            .ok_or_else(|| GraphError::Internal(format!("routed run has no graph for {id}")))?;
        let mut run_outputs = graph.run(vec![("in", frame_in)])?;
        let (_, out) = run_outputs
            .pop()
            .ok_or_else(|| GraphError::Internal(format!("{id} graph produced no output")))?;
        outputs.push((*id, out));
    }
    if outputs.is_empty() {
        return Err(GraphError::Internal(
            "routed run has no tiles to process".to_string(),
        ));
    }
    let general = graphs.general;
    let scale = graphs.scale;
    // The general graph's output is the base when it ran:
    // it already holds every tile that resolves to the
    // general path. Otherwise the first output is the base
    // and every tile is overwritten from its own graph.
    let (base_id, base) = if let Some(position) = outputs.iter().position(|(id, _)| *id == general)
    {
        let (_, base) = outputs.remove(position);
        (general, base)
    } else {
        let (id, base) = outputs.remove(0);
        (id, base)
    };
    composite_routed_outputs(base, base_id, &outputs, &map, general, scale)
}

/// The processing thread: pull frames, process them, publish the
/// newest to the presentation slot and wake the presentation loop.
/// `wake` reports whether the loop is still alive: it sends the
/// frame-ready user event through the loop's proxy, and returns
/// false once the loop has closed. Ends when the exit flag is set,
/// the target exits, the loop closes, or capture fails. On ending
/// it wakes the loop once more, so the loop leaves
/// `ControlFlow::Wait`, re-checks the exit flag, and closes
/// instead of sleeping forever with no frames left to wake it.
fn run_processing(
    backend: &mut impl CaptureBackend,
    launcher: &mut Launcher,
    algorithm: &str,
    scale: u32,
    slot: Arc<FrameSlot>,
    wake: impl Fn() -> bool,
    exit: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut result: Result<(), String> = Ok(());
    let mut routing = FrameRouting::default();
    match build_routed_graphs(algorithm, scale, &routing.profile) {
        Ok(mut graphs) => {
            while !exit.load(Ordering::Relaxed) {
                match backend.capture_frame() {
                    Ok(Some(frame)) => match process_frame_routed(&mut graphs, frame, &mut routing)
                    {
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
    // Wake the presentation loop one last time. It sleeps on
    // `ControlFlow::Wait` and only re-checks the exit flag in
    // `about_to_wait`, so without this event a target exit leaves
    // the window open and the process alive forever: no more
    // frames arrive to wake it after this point. A loop that
    // already closed drops the event, so the wake is harmless
    // when the window closed first.
    wake();
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

    /// A 48x16 frame: three flat tiles.
    fn three_flat_frame() -> Result<Frame, String> {
        pixel_frame(48, 16, |_, _| [200, 30, 90])
    }

    /// Run one frame through one single-algorithm graph and
    /// return its output. The graph runs whole-frame with
    /// no routing set, so the output is the direct processor
    /// output for that algorithm.
    fn run_single_graph(graph: &mut Graph, frame: Frame) -> Result<Frame, String> {
        let mut outputs = graph.run(vec![("in", frame)]).map_err(|e| e.to_string())?;
        match outputs.pop() {
            Some((_, frame)) => Ok(frame),
            None => Err("graph produced no output".to_string()),
        }
    }

    /// Assert the output tile (`tx`, `ty`) of `actual`
    /// equals the same tile of `expected`, row by row. The
    /// tile is a full 16x16 source tile at `scale`.
    fn assert_tile_region_eq(actual: &Frame, expected: &Frame, tx: u32, ty: u32, scale: u32) {
        let x0 = tx as usize * 16 * scale as usize;
        let y0 = ty as usize * 16 * scale as usize;
        let width = 16 * scale as usize;
        for row in 0..width {
            let y = (y0 + row) as u32;
            assert_eq!(
                &actual.row(y)[x0 * 3..(x0 + width) * 3],
                &expected.row(y)[x0 * 3..(x0 + width) * 3],
                "tile ({tx}, {ty}) row {row} differs"
            );
        }
    }

    /// Assert the output tile (`tx`, `ty`) of `actual`
    /// differs from the same tile of `expected` in at least
    /// one row: the tile did not come from that algorithm.
    fn assert_tile_region_differs(actual: &Frame, expected: &Frame, tx: u32, ty: u32, scale: u32) {
        let x0 = tx as usize * 16 * scale as usize;
        let y0 = ty as usize * 16 * scale as usize;
        let width = 16 * scale as usize;
        let mut differs = false;
        for row in 0..width {
            let y = (y0 + row) as u32;
            if actual.row(y)[x0 * 3..(x0 + width) * 3] != expected.row(y)[x0 * 3..(x0 + width) * 3]
            {
                differs = true;
                break;
            }
        }
        assert!(differs, "tile ({tx}, {ty}) matches, expected it to differ");
    }

    /// A 96x16 frame with one tile per class: text, sprite,
    /// flat, patterned, gradient, noise. The noise tile's
    /// bytes are the LCG stream of the region-analysis noise
    /// tile, so it classifies Unknown there and here.
    fn six_tile_frame() -> Result<Frame, String> {
        let mut state: u32 = 0x1234_5678;
        let mut next_byte = move || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            u8::try_from(state >> 24).unwrap_or(0)
        };
        let mut noise = vec![[0_u8; 3]; 256];
        for pixel in &mut noise {
            *pixel = [next_byte(), next_byte(), next_byte()];
        }
        pixel_frame(96, 16, |x, y| {
            let tile = x / 16;
            let lx = x % 16;
            match tile {
                0 => {
                    if text_tile_ink(lx, y) {
                        [20, 20, 20]
                    } else {
                        [240, 240, 240]
                    }
                }
                1 => {
                    let in_body = (4..12).contains(&lx) && (3..13).contains(&y);
                    let in_detail = (7..9).contains(&lx) && (6..8).contains(&y);
                    if in_detail {
                        [240, 220, 60]
                    } else if in_body {
                        [200, 40, 40]
                    } else {
                        [30, 30, 120]
                    }
                }
                2 => [200, 30, 90],
                3 => {
                    if (lx / 2 + y / 2) % 2 == 0 {
                        [20, 20, 20]
                    } else {
                        [235, 235, 235]
                    }
                }
                4 => {
                    let value = u8::try_from(lx * 17).unwrap_or(255);
                    [value, value, value]
                }
                _ => noise[(y * 16 + lx) as usize],
            }
        })
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

    /// A capture backend that yields queued frames, then
    /// nothing, for the processing-thread tests.
    struct FrameFeedBackend {
        queued: std::collections::VecDeque<Result<Option<Frame>, CaptureError>>,
        seen: u64,
    }

    impl FrameFeedBackend {
        fn new(frames: Vec<Frame>) -> FrameFeedBackend {
            FrameFeedBackend {
                queued: frames.into_iter().map(|frame| Ok(Some(frame))).collect(),
                seen: 0,
            }
        }

        fn failing(error: CaptureError) -> FrameFeedBackend {
            FrameFeedBackend {
                queued: std::collections::VecDeque::from([Err(error)]),
                seen: 0,
            }
        }
    }

    impl CaptureBackend for FrameFeedBackend {
        fn initialize(&mut self, _config: &CaptureConfig) -> Result<(), CaptureError> {
            Ok(())
        }

        fn capture_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
            match self.queued.pop_front() {
                Some(Ok(Some(frame))) => {
                    self.seen += 1;
                    Ok(Some(frame))
                }
                Some(other) => other,
                None => Ok(None),
            }
        }

        fn shutdown(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }

        fn frames_seen(&self) -> u64 {
            self.seen
        }
    }

    #[test]
    fn processing_thread_publishes_processed_frames_and_wakes_the_loop() -> Result<(), String> {
        let frames = vec![
            Frame::filled(4, 4, PixelFormat::Rgb8, 10).map_err(|e| e.to_string())?,
            Frame::filled(4, 4, PixelFormat::Rgb8, 20).map_err(|e| e.to_string())?,
            Frame::filled(4, 4, PixelFormat::Rgb8, 30).map_err(|e| e.to_string())?,
        ];
        let mut backend = FrameFeedBackend::new(frames);
        let mut launcher =
            Launcher::spawn("/bin/sleep", &["30".to_string()]).map_err(|e| e.to_string())?;
        let slot = Arc::new(FrameSlot::new());
        let exit = Arc::new(AtomicBool::new(false));
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wakes_in_worker = wakes.clone();
        let slot_in_worker = slot.clone();
        let exit_in_worker = exit.clone();
        let worker = std::thread::spawn(move || {
            run_processing(
                &mut backend,
                &mut launcher,
                "nearest",
                2,
                slot_in_worker,
                move || {
                    wakes_in_worker.fetch_add(1, Ordering::Relaxed);
                    true
                },
                exit_in_worker,
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while wakes.load(Ordering::Relaxed) < 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            3,
            "one wake per published frame"
        );
        let frame = slot
            .take()
            .ok_or("expected a processed frame in the slot")?;
        assert_eq!((frame.width(), frame.height()), (8, 8));
        assert_eq!(frame.data()[0], 30, "the newest frame wins");
        exit.store(true, Ordering::Relaxed);
        let result = worker
            .join()
            .map_err(|_| "processing thread panicked".to_string())?;
        assert!(
            result.is_ok(),
            "processing ends cleanly once the exit flag is set: {result:?}"
        );
        Ok(())
    }

    #[test]
    fn capture_error_ends_processing_with_an_error() -> Result<(), String> {
        let mut backend = FrameFeedBackend::failing(CaptureError::Disconnected);
        let mut launcher =
            Launcher::spawn("/bin/sleep", &["30".to_string()]).map_err(|e| e.to_string())?;
        let slot = Arc::new(FrameSlot::new());
        let exit = Arc::new(AtomicBool::new(false));
        let result = run_processing(
            &mut backend,
            &mut launcher,
            "nearest",
            2,
            slot,
            || true,
            exit,
        );
        assert!(result.is_err(), "a capture error ends processing");
        assert!(
            matches!(&result, Err(e) if e.contains("capture failed") && e.contains("disconnected")),
            "the error names the capture failure, got {result:?}"
        );
        Ok(())
    }

    #[test]
    fn target_exit_ends_processing_cleanly() -> Result<(), String> {
        let mut backend = FrameFeedBackend::new(vec![]);
        let mut launcher = Launcher::spawn("/bin/true", &[]).map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(200));
        let slot = Arc::new(FrameSlot::new());
        let exit = Arc::new(AtomicBool::new(false));
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wakes_in_worker = wakes.clone();
        let result = run_processing(
            &mut backend,
            &mut launcher,
            "nearest",
            2,
            slot,
            move || {
                wakes_in_worker.fetch_add(1, Ordering::Relaxed);
                true
            },
            exit,
        );
        assert!(
            result.is_ok(),
            "an exited target ends processing cleanly: {result:?}"
        );
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "one final wake after the target exits, so the presentation loop leaves its wait and sees the exit flag; no frames were published"
        );
        Ok(())
    }

    /// A real GLX program's intercepted frames flow through the
    /// processing thread into the presentation slot. The target is
    /// `glx_target`, a real program rendering through Mesa under a
    /// virtual X server; the frames it presents are intercepted by
    /// the shim, processed by the nearest graph at 2x, and published
    /// to the slot. The presentation window itself is not exercised
    /// here: this VM has no GPU adapter, so the window cannot open.
    #[test]
    fn intercepted_frames_flow_through_processing() -> Result<(), String> {
        use std::process::{Command, Stdio};
        // A fresh virtual X server for this test alone.
        let display = "81";
        let mut xvfb = Command::new("Xvfb")
            .args([":81", "-screen", "0", "1024x768x24"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Xvfb must start for the interception test: {e}"))?;
        let socket = std::path::PathBuf::from("/tmp/.X11-unix/X81");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if std::time::Instant::now() > deadline {
                let _ = xvfb.kill();
                return Err("Xvfb on :81 did not create its socket in time".to_string());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = intercepted_frames_flow_through_processing_inner(display);
        let _ = xvfb.kill();
        let _ = xvfb.wait();
        result
    }

    /// The body of [`intercepted_frames_flow_through_processing`],
    /// split out so the Xvfb guard above always kills the server.
    fn intercepted_frames_flow_through_processing_inner(display: &str) -> Result<(), String> {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!(
            "pixelflow-run-intercept-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let target = dir.join("glx_target");
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../pixelflow-intercept/tests/glx_target.c");
        let status = Command::new("cc")
            .args(["-O2"])
            .arg(&source)
            .arg("-o")
            .arg(&target)
            .args(["-lX11", "-l:libGL.so.1"])
            .status()
            .map_err(|e| format!("the C compiler must run: {e}"))?;
        if !status.success() {
            return Err("glx_target must compile".to_string());
        }
        let mut backend = InterceptCapture::new()
            .map_err(|e| format!("the interception backend must bind: {e}"))?;
        let mut env: Vec<(String, String)> = backend.environment().to_vec();
        env.push(("DISPLAY".to_string(), format!(":{display}")));
        let mut launcher = Launcher::spawn_with_env(
            target.to_str().ok_or("the target path must be UTF-8")?,
            &[],
            &env,
        )?;
        backend
            .initialize(&CaptureConfig::default())
            .map_err(|e| format!("the interception backend must start: {e}"))?;
        let slot = Arc::new(FrameSlot::new());
        let exit = Arc::new(AtomicBool::new(false));
        let slot_in_worker = slot.clone();
        let exit_in_worker = exit.clone();
        let worker = std::thread::spawn(move || {
            run_processing(
                &mut backend,
                &mut launcher,
                "nearest",
                2,
                slot_in_worker,
                || true,
                exit_in_worker,
            )
        });
        // The newest published frame is the processed output: 640x480
        // at 2x, with the target's yellow rectangle scaled with it.
        // The rectangle sat at frame x 80..240, y 300..420 after the
        // 2x nearest upscale of the 320x240 source.
        let mut published: Option<Frame> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if let Some(frame) = slot.take() {
                published = Some(frame);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        exit.store(true, Ordering::Relaxed);
        let worker_result = worker
            .join()
            .map_err(|_| "the processing thread panicked".to_string())?;
        let frame = published.ok_or("expected a processed frame in the slot")?;
        assert_eq!((frame.width(), frame.height()), (640, 480));
        let rect_row = frame.row(360);
        let rect = [
            rect_row[160 * 3],
            rect_row[160 * 3 + 1],
            rect_row[160 * 3 + 2],
        ];
        assert_eq!(
            rect,
            [255, 255, 0],
            "the rectangle pixel is yellow after upscale"
        );
        assert!(
            worker_result.is_ok(),
            "processing ends cleanly: {worker_result:?}"
        );
        Ok(())
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
        let mut graph = build_algorithm_graph("nearest", &AlgorithmConfig::new(), 2, &[], true)
            .map_err(|e| e.to_string())?;
        let processed = run_single_graph(&mut graph, frame)?;
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
            match build_algorithm_graph("nearest-gpu", &AlgorithmConfig::new(), 2, &[], true) {
                Err(error) => {
                    assert!(
                        error.to_string().contains("no GPU adapter"),
                        "the error names the missing adapter, got {error}"
                    );
                }
                Ok(_) => panic!("expected a no-adapter error"),
            }
        } else {
            let graph = build_algorithm_graph("nearest-gpu", &AlgorithmConfig::new(), 2, &[], true);
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
        let mut graph = build_algorithm_graph("contour", &AlgorithmConfig::new(), 2, &[], true)
            .map_err(|e| e.to_string())?;
        let processed = run_single_graph(&mut graph, frame)?;
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
        let mut graph = build_algorithm_graph("bilinear", &AlgorithmConfig::new(), 2, &[], true)
            .map_err(|e| e.to_string())?;
        let processed = run_single_graph(&mut graph, frame)?;
        assert_eq!((processed.width(), processed.height()), (8, 8));
        assert_eq!(processed.data(), direct.data());
        Ok(())
    }

    #[test]
    fn routed_frame_processes_each_tile_with_its_algorithm() -> Result<(), String> {
        use pixelflow_processing::Nearest;
        // Text tile on the left, flat tile on the right. The
        // flat color is the pinned hard-light case: Contour
        // shifts it, Nearest copies it exactly, so the two
        // routes are distinguishable in the output.
        let frame = pixel_frame(32, 16, |x, y| {
            if x < 16 {
                if text_tile_ink(x, y) {
                    [20, 20, 20]
                } else {
                    [240, 240, 240]
                }
            } else {
                [128, 64, 32]
            }
        })?;
        let direct_contour = Contour.process(&frame, 2).map_err(|e| e.to_string())?;
        let direct_nearest = Nearest.process(&frame, 2).map_err(|e| e.to_string())?;
        let mut graphs = build_routed_graphs("contour", 2, &RoutingProfile::default())?;
        let mut routing = FrameRouting::default();
        let processed =
            process_frame_routed(&mut graphs, frame, &mut routing).map_err(|e| e.to_string())?;
        assert_eq!((processed.width(), processed.height()), (64, 32));
        // The text tile went through Contour, the flat tile
        // through Nearest: each region equals the direct
        // output of its own algorithm, and the flat tile is
        // not the Contour output.
        assert_tile_region_eq(&processed, &direct_contour, 0, 0, 2);
        assert_tile_region_eq(&processed, &direct_nearest, 1, 0, 2);
        assert_tile_region_differs(&processed, &direct_contour, 1, 0, 2);
        // The class-to-algorithm resolution agrees: text
        // resolves to contour, flat to nearest.
        assert_eq!(graphs.algorithm_for(ContentClass::Text), "contour");
        assert_eq!(graphs.algorithm_for(ContentClass::Flat), "nearest");
        // The routing table on the contour graph names the
        // text branch, and the flat tile routes to the flat
        // branch on the nearest graph.
        let table = graphs
            .graphs
            .iter()
            .find(|(id, _)| *id == "contour")
            .map(|(_, graph)| graph)
            .ok_or_else(|| "no contour graph".to_string())?
            .routing()
            .ok_or_else(|| "graph has no routing after a routed frame".to_string())?;
        assert_eq!(table.branch_for(0, 0), Some("text"));
        assert_eq!(table.branch_for(1, 0), Some("flat"));
        assert_eq!(table.routed_count(), 2);
        // First frame: every tile is dirty.
        let invalidated = graphs
            .graphs
            .iter()
            .find(|(id, _)| *id == "contour")
            .map(|(_, graph)| graph)
            .ok_or_else(|| "no contour graph".to_string())?
            .routing_invalidated()
            .ok_or_else(|| "graph has no invalidation after a routed frame".to_string())?;
        assert_eq!(invalidated.count(), 2);
        Ok(())
    }

    #[test]
    fn routed_frame_matches_direct_algorithms_region_by_region() -> Result<(), String> {
        use pixelflow_processing::{Bilinear, Nearest};
        let frame = six_tile_frame()?;
        let direct_contour = Contour.process(&frame, 2).map_err(|e| e.to_string())?;
        let direct_nearest = Nearest.process(&frame, 2).map_err(|e| e.to_string())?;
        let direct_bilinear = Bilinear.process(&frame, 2).map_err(|e| e.to_string())?;
        let mut graphs = build_routed_graphs("contour", 2, &RoutingProfile::default())?;
        let mut routing = FrameRouting::default();
        let processed =
            process_frame_routed(&mut graphs, frame, &mut routing).map_err(|e| e.to_string())?;
        assert_eq!((processed.width(), processed.height()), (192, 32));
        // The retained map names each tile's class: the
        // routing the run used, not a re-derivation here.
        let map = routing
            .previous_map
            .clone()
            .ok_or_else(|| "no region map after the routed frame".to_string())?;
        let expected = [
            (ContentClass::Text, "contour"),
            (ContentClass::Sprite, "contour"),
            (ContentClass::Flat, "nearest"),
            (ContentClass::Patterned, "nearest"),
            (ContentClass::Gradient, "bilinear"),
            (ContentClass::Unknown, "contour"),
        ];
        for (index, (class, algorithm)) in expected.iter().enumerate() {
            let tx = u32::try_from(index).map_err(|err| err.to_string())?;
            assert_eq!(
                map.tile(tx, 0).map(|tile| tile.class),
                Some(*class),
                "tile {tx} class"
            );
            let direct = match *algorithm {
                "contour" => &direct_contour,
                "nearest" => &direct_nearest,
                _ => &direct_bilinear,
            };
            assert_tile_region_eq(&processed, direct, tx, 0, 2);
        }
        Ok(())
    }

    #[test]
    fn second_frame_reuses_prior_classes_and_halo_invalidates() -> Result<(), String> {
        use pixelflow_processing::Nearest;
        let mut graphs = build_routed_graphs("contour", 2, &RoutingProfile::default())?;
        let mut routing = FrameRouting::default();
        let first = three_flat_frame()?;
        process_frame_routed(&mut graphs, first, &mut routing).map_err(|e| e.to_string())?;
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
        let direct_contour = Contour.process(&second, 2).map_err(|e| e.to_string())?;
        let direct_nearest = Nearest.process(&second, 2).map_err(|e| e.to_string())?;
        let processed =
            process_frame_routed(&mut graphs, second, &mut routing).map_err(|e| e.to_string())?;
        // The side tiles stayed flat and went through
        // Nearest; the middle tile went through Contour.
        assert_tile_region_eq(&processed, &direct_nearest, 0, 0, 2);
        assert_tile_region_eq(&processed, &direct_contour, 1, 0, 2);
        assert_tile_region_eq(&processed, &direct_nearest, 2, 0, 2);
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
        let invalidated = graphs
            .graphs
            .iter()
            .find(|(id, _)| *id == "contour")
            .map(|(_, graph)| graph)
            .ok_or_else(|| "no contour graph".to_string())?
            .routing_invalidated()
            .ok_or_else(|| "graph has no invalidation after the second frame".to_string())?;
        assert_eq!(invalidated.count(), 3);
        assert!(invalidated.is_dirty(0, 0));
        assert!(invalidated.is_dirty(1, 0));
        assert!(invalidated.is_dirty(2, 0));
        // An identical third frame invalidates nothing and
        // still routes each tile to its own algorithm.
        let direct_contour = Contour
            .process(&second_again, 2)
            .map_err(|e| e.to_string())?;
        let direct_nearest = Nearest
            .process(&second_again, 2)
            .map_err(|e| e.to_string())?;
        let processed = process_frame_routed(&mut graphs, second_again, &mut routing)
            .map_err(|e| e.to_string())?;
        assert_tile_region_eq(&processed, &direct_nearest, 0, 0, 2);
        assert_tile_region_eq(&processed, &direct_contour, 1, 0, 2);
        assert_tile_region_eq(&processed, &direct_nearest, 2, 0, 2);
        let invalidated = graphs
            .graphs
            .iter()
            .find(|(id, _)| *id == "contour")
            .map(|(_, graph)| graph)
            .ok_or_else(|| "no contour graph".to_string())?
            .routing_invalidated()
            .ok_or_else(|| "graph has no invalidation after the third frame".to_string())?;
        assert_eq!(invalidated.count(), 0);
        Ok(())
    }
}
