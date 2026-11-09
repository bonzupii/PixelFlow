# PixelFlow

A cross-platform graphics interception, pixel-art upscaling, and
fullscreen presentation application. PixelFlow launches an
application or game through a wrapper, captures its rendered
graphics output, processes each frame through its upscaling
pipeline, and presents the processed image at the display
resolution. The wrapped application does not need to know
PixelFlow exists.

## Milestone 1: Foundation

- **Rust workspace** with three crates (see `docs/architecture.md`):
  - `pixelflow-core`: frame representation, pixel formats, and the
    processing pipeline trait. No platform code, no I/O.
  - `pixelflow-processing`: image processing algorithms. `contour`
    (algorithm 20 from the retro upscaler experiments) and a
    nearest-neighbour baseline, both behind the `Processor` trait.
  - `pixelflow-cli`: the `pixelflow` binary.
- **contour, ported exactly.** The algorithm code was moved from
  the standalone Rust program into
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

## Wayland capture

- **New crates**: `pixelflow-capture` (backend trait, source
  configuration, typed errors; no platform code),
  `pixelflow-wayland` (XDG Desktop Portal ScreenCast over D-Bus
  plus a PipeWire video stream), `pixelflow-present` (wgpu
  fullscreen borderless window, aspect-ratio preserving,
  windowed preview, vsync).
- **`pixelflow run <application> [arguments]`** launches the
  target unmodified, captures its window through the portal,
  processes each frame with contour, and presents the result in
  a separate fullscreen window. `--capture window|monitor`
  (window is the default), `--scale 2|4`, `--windowed`,
  `--fullscreen`. The compositor dialog is where the user picks
  the window. See `docs/wayland.md`.
- **No input forwarding.** ScreenCast carries no input, and this
  build does not forward keyboard, mouse, or controller input.
  RemoteDesktop and EIS/libei are future work; see
  `docs/wayland.md`.

## Usage

```bash
cargo build --release
./target/release/pixelflow upscale input.png output.png
./target/release/pixelflow upscale --scale 2 input.png output.png
./target/release/pixelflow upscale --algorithm nearest input.png output.png
./target/release/pixelflow run gtk-slashem
./target/release/pixelflow run --capture monitor --scale 2 zsnes
./target/release/pixelflow run --windowed gtk-slashem
./target/release/pixelflow backends
./target/release/pixelflow info
```

## Verification

`cargo build --release` is clean and `cargo test --release`
passes: 32 tests, 0 failures (8 CLI, 3 core, 5 presentation, 1
processing unit test, 3 regression, 12 Wayland). The CLI paths
were exercised live on the build machine: `upscale` on the Mario
fixture produces 1024x896 in 0.4 s with max diff 34 over 6
pixels, matching the table below; `run /bin/true` exits 1 with
"exited before capture started"; `run --windowed /bin/sleep 30`
exits 1 with a D-Bus connection error from the missing portal,
and the child is killed with no stray process left.

The regression test compares each 4x output against the Python reference:

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
port's own behaviour, documented in `contour/README.md`, not a
regression in this crate. The test allows at most 10 such pixels
with a max diff of 34 for mario.

## Explicitly NOT built yet

- The end-to-end wrapper on a Wayland desktop. The build
  machine has no Wayland desktop, no compositor, no portal, no
  PipeWire daemon, and no gtk-slashem, so `pixelflow run
  gtk-slashem` has not run against a live capture there. The
  acceptance test is that command on a Wayland desktop. See
  `docs/wayland.md` for what was verified and what was not.
- X11 capture, Windows Graphics Capture, DXGI, ScreenCaptureKit,
  BSD capture (Milestone 5). Wayland is the only capture backend.
- DMA-BUF fast path. DMA-BUF buffers are a typed error for now;
  mapping them for CPU access is future work.
- Input forwarding (RemoteDesktop portal, EIS/libei).
- Real-time optimization: buffer pooling, multithreading, SIMD
  (Milestone 4). Current speed is about 0.4 s per 256x224 frame
  at 4x, single threaded, correct but not yet fast enough for 60 FPS.
- Configuration files (TOML) and packaging (Milestone 6).

## License

Apache License 2.0. See `LICENSE`.
