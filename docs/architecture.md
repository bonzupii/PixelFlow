# PixelFlow architecture

## Crate boundaries

```
pixelflow-core          frame, pixel format, Processor trait
       ^                     (no platform code, no I/O, no deps)
       |
pixelflow-processing    algorithms: edgeflow, nearest
       ^                     (depends on core only)
       |
pixelflow-cli           pixelflow binary: PNG I/O, clap CLI
                        (depends on core + processing + png + clap)
```

Dependencies flow one way: core <- processing <- cli. No crate
depends on a crate above it, and no processing code knows about
files, windows, or capture. Later milestones add crates beside
these (`pixelflow-capture`, `pixelflow-present`,
`pixelflow-input`, `pixelflow-launcher`) that also depend on core
and never on each other except through core types.

## Core types

- `PixelFormat`: `Rgb8`, `Rgba8`, `Gray8`. Bytes per pixel and
  channel count come from the format, never from a magic number.
- `Frame`: width, height, format, and an owned row-major data
  buffer. `Frame::new` rejects a buffer whose length does not
  match the dimensions and format instead of building a corrupt
  frame. `Frame::to_rgb8` is the single place that converts other
  formats to RGB8 (dropping alpha, expanding grey); the processing
  pipeline works on RGB8.
- `Processor` trait (`pipeline.rs`): the contract every algorithm
  implements.

```rust
trait Processor {
    fn name(&self) -> &'static str;
    fn supported_scales(&self) -> &[u32];
    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError>;
}
```

`ProcessError` covers unsupported scales, unsupported formats,
and internal failures. The trait is deliberately narrow — one
frame in, one frame out, an integer scale — so capture,
presentation, and the CLI program against it, never against an
algorithm.

## Processing: algorithm 20

`pixelflow-processing` holds one module per algorithm under
`src/algorithms/`. `edgeflow.rs` is the standalone Rust port
moved in verbatim: the algorithm body (1,163 lines, from the
image helpers through `clean_base_edge`) is byte-identical to
`edgeflow/src/main.rs`. Only three things changed around it:

1. The PNG I/O and `main()` were removed; the CLI owns I/O now.
2. A `Processor` adapter (`EdgeFlow`) was appended, converting
   `Frame` to the algorithm's internal image types and back.
3. The dead `use std::env;` import was removed, and the follower
   statistics (which the standalone program printed) are now
   reported on stderr only when `EDGEFLOW_DEBUG_RUNS=1` is set,
   which also silences the dead-code warning those fields earned
   in a library.

What was *not* changed, and must not be changed silently during
integration: the float widths (f32 image arithmetic, f64 run
geometry), the erf approximation, the gaussian/uniform filter
edge modes, the gated-merge thresholds, the labelset adaptive
sharpening, the dither-mask pair test, the grain and hard-light
strengths, the edge-follower gates, and the hardcoded
Savitzky-Golay kernel tables. Those tables are the exact f64
values numpy's `pinv` returns: the follower's apex gate reads
the sign of the fitted slope, and at the flat points of a
staircase that slope is exactly 0 in exact arithmetic, so its
sign is whatever the pinv's ~1e-15 rounding noise says.
Recomputing the kernels from the normal equations gives a
different noise sign and flips the gate on 14 samples of one
long Mario run. The tables are the port, not a detail.

The algorithm's own test switches survive the move:
`EDGEFLOW_NO_FOLLOW=1` skips the edge follower (12 only);
`EDGEFLOW_DEBUG_RUNS=1` prints each accepted run and the run
statistics to stderr.

## CLI

`pixelflow-cli` is a thin shell: it parses arguments with clap,
loads PNGs with the `png` crate (`src/png_io.rs`, the only place
that knows about the PNG format), calls a `Processor`, and writes
the result back. `backends` and `info` report what actually
exists on the machine; in Milestone 1 that is the two processing
algorithms and nothing else. There is no `run` command until
Milestone 2 builds the launcher and the first capture backend.

## Testing

- Unit tests live with the code they test (core frame
  construction, nearest-neighbour pixel repetition).
- `pixelflow-processing/tests/regression.rs` is the
  image-regression gate: with fixtures supplied locally (see
  `PIXELFLOW_FIXTURES_DIR` in the test file; the images are
  copyrighted game screenshots and are never committed), it
  processes the six fixtures in `inputs/` at 4x with edgeflow
  and compares pixel-for-pixel against `expected/`, the Python
  reference outputs from the upscaler experiments (round 6).
  Limits: max abs diff <= 1, except mario (at most 10 pixels
  differing by more than 1, max diff 34), for the float-tie
  reason recorded in the README.
- Fixtures are local-only: the test skips when they are absent,
  so the suite stays green on a clean checkout.
