# PixelFlow architecture

## Crate boundaries

- `pixelflow-core`: frame, pixel format, `Processor` trait. No
  platform code, no I/O, no dependencies.
- `pixelflow-processing`: algorithms `contour`, `nearest`, and
  `bilinear`; the algorithm registry. Depends on core only.
- `pixelflow-gpu`: GPU context, GPU-resident images, the WGSL
  nearest-neighbour compute node (`nearest-gpu`). Depends on core
  and processing.
- `pixelflow-capture`: capture backend trait, source
  configuration, typed errors. Depends on core only.
- `pixelflow-intercept`: the `LD_PRELOAD` shim that interposes the
  target's swap calls, and the capture backend that turns the
  streamed frames into core frames. Depends on core and capture.
- `pixelflow-wayland`: XDG Desktop Portal ScreenCast plus a
  PipeWire video stream. Superseded; kept as record, no longer used
  by `run`. Depends on core and capture.
- `pixelflow-present`: the wgpu window. Depends on core only.
- `pixelflow-cli`: the `pixelflow` binary. PNG I/O, the `upscale`
  and `run` commands, `backends`, `info`. Depends on core,
  processing, capture, intercept, present, png, clap.

Dependencies flow one way: core at the bottom, the CLI at the
top. No crate depends on a crate above it, and no processing
code knows about files, windows, or capture. The launcher is a
module inside the CLI (`src/launcher.rs`), not a crate, and
input forwarding does not exist yet.

## Core types

- `PixelFormat`: `Rgb8`, `Rgba8`, `Gray8`. Bytes per pixel and
  channel count come from the format, never from a magic number.
- `Frame`: width, height, format, row stride, capture
  metadata, and an owned row-major data buffer. All fields are
  private, read through accessors (`width()`, `height()`,
  `format()`, `stride()`, `timestamp()`, `sequence()`, `data()`). The buffer is `height * stride` bytes long, and the
  stride is at least `width * format.bytes_per_pixel()`; tight
  packing is the special case where the stride equals one row.
  `Frame::new` builds a tight frame and `Frame::new_with_stride`
  builds a strided one; both reject a buffer whose length does
  not match the dimensions, format, and stride instead of
  building a corrupt frame. `pixel(x, y)` and `row(y)` read
  through the stride, and `Frame::packed` returns a tight copy
  (borrowing the frame itself when it is already tight).
  Metadata is a capture timestamp (monotonic clock, for frame
  pacing) and a capture sequence number (for the dirty-region
  work); processors propagate both from input to output.
  `Frame::to_rgb8` is the single place that converts other
  formats to RGB8 (dropping alpha, expanding grey); the
  processing pipeline works on RGB8. PipeWire buffers do have
  strides; the Wayland converter keeps the chunk stride in the
  frame it builds (see below).
- `Processor` trait (`pipeline.rs`): the contract every algorithm
  implements.

```rust
trait Processor {
    fn name(&self) -> &'static str;
    fn supported_scales(&self) -> &[u32];
    fn stride_support(&self) -> StrideSupport;
    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError>;
}
```

`ProcessError` covers unsupported scales, unsupported formats,
and internal failures. The trait is deliberately narrow: one
frame in, one frame out, an integer scale. `stride_support`
declares whether the node reads any stride or needs tight
input packed first; contour is the tight-only node. Capture,
presentation, and the CLI program against it, never against an
algorithm.

## Processing: contour

`pixelflow-processing` holds one module per algorithm under
`src/algorithms/`. `contour.rs` is the standalone Rust port,
brought in with its math unchanged and its output verified
against the Python reference (see Testing). The code around
the math changed since the port: the workspace lints deny
`#[allow]` everywhere, so the file was restructured to pass
them without one (the run geometry now carries `i64` sample
positions, the Savitzky-Golay tables live in module-level
constants, long functions split into named helpers). It is no
longer a verbatim copy of the standalone program.

What did not change, and must not be changed silently: the
float widths (f32 image arithmetic, f64 run geometry), the erf
approximation, the gaussian/uniform filter edge modes, the
gated-merge thresholds, the labelset adaptive sharpening, the
dither-mask pair test, the grain and hard-light strengths, the
edge-follower gates, and the hardcoded Savitzky-Golay kernel
tables. Those tables are the exact f64 values numpy's `pinv`
returns: the follower's apex gate reads the sign of the fitted
slope, and at the flat points of a staircase that slope is
exactly 0 in exact arithmetic, so its sign is whatever the
pinv's ~1e-15 rounding noise says. Recomputing the kernels
from the normal equations gives a different noise sign and
flips the gate on 14 samples of one long Mario run. The tables
are the port, not a detail.

Contour does not preserve flat colors exactly. The hard-light
stage at 0.5 strength is the only stage that moves a flat
color: on flat input the merge, the labelset, the dither
blend, and the grain layer are all the identity, and the edge
follower finds no runs. What remains is `out = 0.5 * v +
0.5 * hardlight(v, v)`, which moves flat mid-tones by up to
just under 16 (for example [128, 64, 32] becomes
[128, 48, 20]). The synthetic tests pin this with an
independent computation of the formula, not with values
copied from a run.

The algorithm's own test switches survive the move:
`CONTOUR_NO_FOLLOW=1` skips the edge follower (12 only);
`CONTOUR_DEBUG_RUNS=1` prints each accepted run and the run
statistics to stderr. Contour computes its output dimensions
with checked multiplication; an overflow is an `Internal`
error, never a truncated size.

## Capture

`pixelflow-capture` holds the backend trait: `initialize`,
`capture_frame`, `shutdown`, plus `frames_seen` and
`frames_dropped` counters. `SourceKind` is `Window` or
`Monitor`. `pixelflow-wayland` implements the trait: it opens
a portal ScreenCast session, connects a PipeWire stream to the
selected node, and converts buffers to `pixelflow-core`
frames. The conversion honors the chunk offset and row stride
of PipeWire buffers, so padded rows convert correctly, and the
frame it builds keeps the chunk stride with its row padding
zeroed; a
buffer too short for its stride is an error. DMA-BUF buffers
are a typed error, not a silent fallback. The full lifecycle
is in `docs/wayland.md`.

## Presentation

`pixelflow-present` owns the window that shows processed
frames. Fullscreen by default; `--windowed` opens a preview
window instead, and `f` toggles between the two while the
window runs. Frames are drawn letterboxed to preserve aspect
ratio (`fit_rect` in `layout.rs`). Vsync is `AutoVsync` where
the surface offers it, `Fifo` otherwise, with at most 2 frames
of latency. Frames arrive through a replaceable frame slot
shared with the processing thread: a newer frame replaces one
still waiting in the slot, so the presented frame is always the
freshest; replaced frames are counted, not hidden. The
processing thread wakes the presentation loop through an
event-loop proxy each time it publishes, so the loop leaves
`ControlFlow::Wait` as soon as a frame is ready.

## CLI

`pixelflow-cli` is a thin shell: it parses arguments with clap,
loads PNGs with the `png` crate (`src/png_io.rs`, the only place
that knows about the PNG format), calls a `Processor`, and writes
the result back. Five commands:

- `upscale <in.png> <out.png>`: `--scale 2|4` (default 4),
  `--algorithm contour|nearest|bilinear|nearest-gpu` (default
  contour).
- `run <program> [args]`: launches the target, captures the
  source the user picks in the compositor dialog, processes
  every frame through the processing graph, presents it. The
  live graph is built from the selected algorithm's registry
  nodes; Contour's node declares the Global spatial dependency,
  so the graph runs it whole-frame.
  `--capture window|monitor` (default window), `--scale 2|4`
  (default 4), `--algorithm` (default contour; `nearest`
  resolves to the `nearest-gpu` node when a GPU adapter is
  present, and an explicit `nearest-gpu` request with no adapter
  is an error), `--windowed` for a preview instead of fullscreen
  (fullscreen is the default; `--fullscreen` and `--windowed`
  conflict). Arguments after `--` go to the target. The launcher
  waits 2 seconds for the target to start, then opens the portal
  session; the wait is startup grace, not detection, and
  PixelFlow never verifies that the picked source belongs to
  the launched process. If window capture fails to start, the
  run tries monitor capture once on the same backend and
  reports the effective source. A target that exits before
  capture starts is an error, and the child is killed on
  shutdown.
- `backends`: lists the capture backend (Wayland), the
  presentation backend (wgpu), and the registered algorithms
  with their scales.
- `info`: version, OS, architecture, CPU count, and the
  registered algorithm ids. The display is probed when the
  presentation window opens, not before.
- `algorithms [name]`: lists every registered algorithm, or
  prints one algorithm's full capabilities from its registry
  descriptor.

## Testing

- Unit tests live with the code they test: frame
  construction, the letterbox math, format conversion, the
  CLI argument parsing, the portal and PipeWire error paths.
- `pixelflow-processing/tests/synthetic.rs` (17 tests) is the
  deterministic suite: every input image is generated in code,
  no PNG files, nothing copyrighted, so it runs on a clean
  checkout. It covers single-pixel and very small images, flat
  colors, hard edges, diagonals and curves, checkerboard and
  dither patterns, gradients, transparency, odd dimensions,
  large images, and repeated processing. Exact values in it
  come from the algorithm's documented behavior or an
  independent computation; measured invariants state their
  rationale in comments.
- `pixelflow-processing/tests/regression.rs` (3 tests) is the
  image-regression gate: with fixtures supplied locally (see
  `PIXELFLOW_FIXTURES_DIR` in the test file; the images are
  copyrighted game screenshots and are never committed), it
  processes the six fixtures in `inputs/` at 4x with contour
  and compares against `expected/`, the Python reference
  outputs from the upscaler experiments (round 6). Two of the
  three tests skip without fixtures; the third checks nearest
  on a 2x1 frame built in code and needs no fixtures. Limits:
  max abs diff <= 1, except mario (at most 10 pixels differing
  by more than 1, max diff 34), for the float-tie reason
  recorded in the Contour section above. Fixtures are local-only: the test
  skips when they are absent, so the suite stays green on a
  clean checkout.
- Counts: 217 tests in total at the `d2e057e` baseline (30 CLI,
  50 core — 28 unit plus 22 graph — 22 GPU, 8 presentation, 76
  processing — 6 algorithm unit, 17 synthetic, 3 regression, 6
  budget, 6 graph nodes, 26 region analysis, 12 registry — 31
  Wayland — 30 unit plus 1 mock-portal). 215 run anywhere with
  no fixtures; the 2 fixture-based regression tests skip.
- CI (`.github/workflows/ci.yml`, on every push to main and
  every pull request) runs three jobs: `cargo fmt --check`,
  the clippy gate (`ci/clippy-gate.sh`), and `cargo build
  --release` plus `cargo test --release` with no fixtures
  set, so the 2 fixture regression tests skip there by design.
  The clippy gate allows at most 30 diagnostics in contour.rs
  (26 `cast_possible_truncation`, 4 `cast_sign_loss`): float
  narrowing at deliberate f32/f64 boundaries, proven
  load-bearing by experiment (changing the edge follower's
  best-distance array to f64 moved the Mario regression from
  34/6 to 34/7). The 30 are a ceiling, not a floor: the gate
  fails on any new diagnostic, and a fix that removes a known
  diagnostic passes. There is no `#[allow]`
  anywhere in the workspace.
