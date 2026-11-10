# PixelFlow

A cross-platform graphics interception, pixel-art upscaling, and
fullscreen presentation application. PixelFlow launches an
application or game through a wrapper, captures its rendered
graphics output, processes each frame through its upscaling
pipeline, and presents the processed image at the display
resolution. The wrapped application does not need to know
PixelFlow exists.

## Crates

Six crates in one workspace (Rust edition 2024, `rust-version`
1.99; see `docs/architecture.md`):

- `pixelflow-core`: frame representation, pixel formats, and the
  `Processor` trait. No platform code, no I/O, no dependencies.
- `pixelflow-processing`: image processing algorithms. `contour`,
  the flagship, and a nearest-neighbour baseline, both behind the
  `Processor` trait.
- `pixelflow-capture`: the capture backend trait, source
  configuration, and typed errors. No platform code.
- `pixelflow-wayland`: the Wayland capture backend. XDG Desktop
  Portal ScreenCast over D-Bus plus a PipeWire video stream.
- `pixelflow-present`: the presentation window. A wgpu fullscreen
  borderless window, aspect-ratio preserving, vsync, with a
  windowed preview.
- `pixelflow-cli`: the `pixelflow` binary. PNG I/O, the `upscale`
  and `run` commands, `backends`, `info`.

## Contour

Contour is the flagship algorithm, ported from the standalone
Rust program built in the retro upscaler experiments. Gated
low-contrast merging, contrast-adaptive label selection,
pair-consistent dithering, grain reconstruction, hard-light
detail enhancement, Savitzky-Golay quadratic edge fitting,
coherent boundary tracking, and subpixel edge reconstruction.
Its output is verified against the Python reference outputs
(see below).

Two behaviours to know about. Contour does not preserve flat
colors exactly: the hard-light stage at 0.5 strength moves flat
mid-tones by up to just under 16 (for example [128, 64, 32]
becomes [128, 48, 20]). And it takes RGB8 only; other formats
are a typed error, never a silent conversion.

## Wayland capture

`pixelflow run <application> [arguments]` launches the target
unmodified, opens the portal session, processes each frame with
contour, and presents the result in a separate fullscreen
window. `--capture window|monitor` (window is the default),
`--scale 2|4` (4 is the default), `--windowed` for a preview
instead of fullscreen. Fullscreen is the default, and `f`
toggles it while the presentation window runs. The compositor
dialog is where the user picks the source, and PixelFlow does
not verify that the picked source belongs to the launched
process: the two-second wait before the dialog is startup
grace for the target, not detection of its window. See
`docs/wayland.md`.

No input forwarding. ScreenCast carries no input, and this
build does not forward keyboard, mouse, or controller input.
Until RemoteDesktop and EIS/libei exist, the product is
read-only capture and presentation: the user interacts with
the target window directly, and PixelFlow shows the processed
copy. See `docs/wayland.md`.

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
passes: 64 tests, 0 failures (8 CLI, 13 core, 7 presentation, 2
processing unit tests, 3 regression, 14 Wayland, 17 synthetic).
62 of the 64 run anywhere with no fixtures; the other 2 are
regression tests that skip without the copyrighted screenshots
(the third regression test needs no fixtures). The regression
tests compare each 4x output against the Python reference.
CI runs `cargo fmt --check`, the clippy gate, and `cargo build
--release` plus `cargo test --release` on every push to main
and every pull request (see `.github/workflows/ci.yml`).

The clippy gate allows at most 30 diagnostics in contour.rs (26
`cast_possible_truncation`, 4 `cast_sign_loss`), all float
narrowing at deliberate f32/f64 boundaries in the reference
algorithm. Changing the edge follower's best-distance array to
f64 moves the Mario regression from 34/6 to 34/7, so the
narrowings stay. The 30 are a ceiling, not a floor: the gate
fails on any new diagnostic, and a fix that removes a known
diagnostic passes (see `ci/clippy-gate.sh`).

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
algorithm's own behaviour, not a regression in this crate. The test
allows at most 10 such pixels with a max diff of 34 for mario.

The CLI paths were exercised live on the build machine:
`upscale` on the Mario fixture produces 1024x896 in about half
a second with max diff 34 over 6 pixels, matching the table
above; `run /bin/true` exits 1 with "exited before capture
started"; `run --windowed /bin/sleep 30` exits 1 with a D-Bus
connection error from the missing portal, and the child is
killed with no stray process left.

## What is not built yet

- The end-to-end wrapper on a Wayland desktop. The build
  machine has no Wayland desktop, no compositor, no portal, no
  PipeWire daemon, and no gtk-slashem, so `pixelflow run
  gtk-slashem` has not run against a live capture there. The
  acceptance test is that command on a Wayland desktop. See
  `docs/wayland.md` for what was verified and what was not.
- X11 capture, Windows Graphics Capture, DXGI, ScreenCaptureKit,
  BSD capture. Wayland is the only capture backend. That is
  Stage VI work in the mandate.
- DMA-BUF fast path. DMA-BUF buffers are a typed error for now;
  mapping them for CPU access is future work.
- Input forwarding (RemoteDesktop portal, EIS/libei).
- Speed: about half a second per 256x224 frame at 4x, single
  threaded, correct but not yet fast enough for 60 FPS. That is
  Stage IV work in the mandate.
- Configuration files (TOML) and packaging. That is Stage VII
  work in the mandate.
- The processing framework itself: the composable graph, region
  analysis, and content-aware routing. That is Stage II and
  Stage III work, specified in
  `docs/incremental-region-aware-architecture.md`. The frame
  model with stride and metadata landed as the first Stage II
  step (see `docs/stage2-core-design.md`). What exists now is a
  single-frame-in, single-frame-out `Processor` with an integer
  scale.

## License

Apache License 2.0. See `LICENSE`.
