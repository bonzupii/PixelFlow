# PixelFlow

PixelFlow upscales pixel art. It launches a game or application, captures its
window, upscales each frame, and presents the result fullscreen, so
low-resolution pixel art fills a modern display. It also upscales single PNG
images from the command line.

## Features

- `upscale`: scale a PNG image 2x or 4x. Two algorithms: Contour, an
  edge-aware upscaler built for pixel art, and a nearest-neighbour baseline.
- `run`: launch a program, capture its window or monitor, upscale every frame
  with Contour, and present the result in a fullscreen window. Press `f` to
  toggle between fullscreen and a windowed preview.
- Contour follows edges and reconstructs detail along them instead of
  blurring. It is tuned for pixel art and does not preserve flat colors
  exactly.
- `backends` and `info` report the available capture and presentation
  backends, the algorithms, and the machine's operating system, architecture,
  and CPU count.

## Platform support

- Capture runs on Linux with a Wayland compositor that provides XDG Desktop
  Portal ScreenCast and PipeWire. Wayland is the only capture backend.
- The `upscale` command needs no display and works anywhere the binary
  builds.
- Not implemented: X11 capture, Windows and macOS capture, input forwarding,
  and DMA-BUF buffers.
- The end-to-end `run` flow has not been validated on a live Wayland desktop.

## Getting started

Prerequisites: Rust 1.99 or later, and the system libraries the capture and
presentation backends need. On Debian or Ubuntu:

```bash
sudo apt-get install -y libpipewire-0.3-dev libspa-0.2-dev libwayland-dev libclang-dev pkg-config
```

A Nix development shell is also available:

```bash
nix develop
```

It provides the Rust toolchain and the system libraries above (see `flake.nix`).

Build:

```bash
cargo build --release
```

The binary is `./target/release/pixelflow`.

Quick start — upscale a PNG (no display needed):

```bash
./target/release/pixelflow upscale input.png output.png
```

The default is 4x with Contour. The output is a PNG at four times the input
dimensions.

## Usage

### `upscale`

```bash
./target/release/pixelflow upscale input.png output.png
./target/release/pixelflow upscale --scale 2 input.png output.png
./target/release/pixelflow upscale --scale 2 --algorithm nearest input.png output.png
```

Defaults: `--scale 4`, `--algorithm contour`. Palette, RGBA, and greyscale
inputs are expanded to RGB8 on load; alpha is dropped. Output is always an
RGB8 PNG.

### `run`

```bash
./target/release/pixelflow run gtk-slashem
./target/release/pixelflow run --capture monitor --scale 2 zsnes
./target/release/pixelflow run --windowed gtk-slashem
./target/release/pixelflow run zsnes -- -s game.smc
```

Defaults: window capture, 4x, fullscreen. `run` always uses Contour; it takes
no `--algorithm` option. Arguments after `--` go to the target program. The
compositor asks which window or monitor to capture; PixelFlow does not check
that the source picked belongs to the launched program.

`PIXELFLOW_MEMORY_BUDGET` (bytes, plain integer) sets a working-memory budget
for Contour; unset means no budget.

### `backends` and `info`

```bash
./target/release/pixelflow backends
./target/release/pixelflow info
```

`backends` lists the capture backend, the presentation backend, and the
algorithms with their scales. `info` reports the version, operating system,
architecture, CPU count, and the same backend and algorithm list.

## Documentation

- `docs/architecture.md` — crate layout, core types, the processing pipeline,
  capture, and presentation.
- `docs/wayland.md` — the Wayland capture lifecycle, PipeWire buffers, and
  input forwarding.
- `docs/performance.md` — benchmark measurements for Contour.

Development: `cargo test --release` runs the test suite.
`pixelflow-bench` benchmarks Contour on synthetic frames
(`pixelflow-bench --one <width> <height> <scale>` runs one case).

## License

Apache License 2.0. See `LICENSE`.
