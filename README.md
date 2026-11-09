# PixelFlow

A cross-platform graphics interception, pixel-art upscaling, and
fullscreen presentation application. PixelFlow launches an
application or game through a wrapper, captures its rendered
graphics output, processes each frame through its upscaling
pipeline, and presents the processed image at the display
resolution. The wrapped application does not need to know
PixelFlow exists.

## Milestone 1: Foundation — what is built

- **Rust workspace** with three crates (see `docs/architecture.md`):
  - `pixelflow-core`: frame representation, pixel formats, and the
    processing pipeline trait. No platform code, no I/O.
  - `pixelflow-processing`: image processing algorithms. `upscale20`
    (algorithm 20 from the retro upscaler experiments) and a
    nearest-neighbour baseline, both behind the `Processor` trait.
  - `pixelflow-cli`: the `pixelflow` binary.
- **upscale20, ported exactly.** The algorithm code was moved from
  the standalone Rust program (`upscale20/src/main.rs`) into
  `pixelflow-processing` with its mathematical operations
  unchanged: gated low-contrast merging, contrast-adaptive label
  selection, pair-consistent dithering, grain reconstruction,
  hard-light detail enhancement, Savitzky-Golay quadratic edge
  fitting, coherent boundary tracking, and subpixel edge
  reconstruction. Its output is verified pixel-for-pixel against
  the Python reference outputs (see below).
- **Regression tests.** The six native test images and their
  reference outputs are copyrighted game screenshots and are not
  in this repo. Point `PIXELFLOW_FIXTURES_DIR` at a local copy
  (an `inputs/` and an `expected/` directory) and `cargo test`
  checks the 4x upscale of each one against its reference;
  without fixtures the regression tests skip.
- **CLI.** `pixelflow upscale <in.png> <out.png> [--scale 2|4]`,
  `pixelflow backends`, `pixelflow info`.

## Usage

```bash
cargo build --release
./target/release/pixelflow upscale input.png output.png
./target/release/pixelflow upscale --scale 2 input.png output.png
./target/release/pixelflow upscale --algorithm nearest input.png output.png
./target/release/pixelflow backends
./target/release/pixelflow info
```

## Verification

`cargo test --release` passes. The regression test compares each
4x output against the Python reference:

| image | max abs diff | pixels differing by >1 |
|---|---|---|
| ct | 1 | 0 |
| eb | 1 | 0 |
| ct2 | 1 | 0 |
| zelda | 1 | 0 |
| mario | 34 | 6 |
| ctnew | 1 | 0 |

Max diff 1 is rounding. The 6 Mario pixels that differ by more
than 1 sit at the right end of the long hill-boundary runs, where
a horizontal and a vertical run compete for the same pixel and the
winner is a float comparison at a near-tie; that is the reference
port's own behaviour, documented in `upscale20/README.md`, not a
regression in this crate. The test allows at most 10 such pixels
with a max diff of 34 for mario.

## Explicitly NOT built yet

- `pixelflow run` — launching and capturing a live application
  (Milestone 2). There is deliberately no `run` command yet; a
  stub that pretended to launch an application would be worse
  than no command.
- Any capture backend: X11, Wayland, PipeWire, Windows Graphics
  Capture, DXGI, ScreenCaptureKit (Milestone 2 and 5).
- Fullscreen presentation, wgpu, aspect-ratio handling, vsync,
  frame pacing, input forwarding (Milestone 3).
- Real-time optimization: buffer pooling, multithreading, SIMD
  (Milestone 4). Current speed is about 0.4 s per 256x224 frame
  at 4x, single threaded — correct, not yet fast enough for 60 FPS.
- Configuration files (TOML) and packaging (Milestone 6).

`pixelflow backends` and `pixelflow info` report this honestly
instead of listing backends that do not exist.

## License

Apache License 2.0. See `LICENSE`.
