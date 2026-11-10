# PixelFlow

PixelFlow upscales pixel art. It launches a game or application, captures its
window or monitor, upscales each frame through a processing graph, and
presents the result in a fullscreen window. It also upscales single PNG
images from the command line, with no display needed.

The flagship algorithm is Contour, an edge-aware upscaler built for pixel
art. Nearest-neighbour and bilinear baselines ship alongside it, and a GPU
nearest-neighbour node (`nearest-gpu`) exists for machines with a GPU
adapter. Contour is unfinished: it is slow (see
[Limitations](#limitations)) and shifts flat mid-tones by up to ~16 levels
(see [Contour](#contour)).

## Commands

```
pixelflow upscale <input.png> <output.png> [--scale 2|4] [--algorithm contour|nearest|bilinear|nearest-gpu]
pixelflow run <program> [args...] [--capture window|monitor] [--scale 2|4] [--algorithm ...] [--windowed|--fullscreen]
pixelflow backends
pixelflow info
pixelflow algorithms [name]
```

`upscale` and `run` default to `--scale 4 --algorithm contour`. `run`
defaults to window capture, fullscreen presentation. Arguments after `--`
go to the target program. `backends` lists the capture backend, the
presentation backend, and the registered algorithms with their scales.
`info` reports the version, OS, architecture, CPU count, and the registered
algorithm ids. `algorithms` lists every registered algorithm; `algorithms
<name>` prints one algorithm's full capabilities (formats, scales,
backends, dependencies, stride support, tile safety, stages, working
memory, determinism, parameters).

### `upscale`

```bash
./target/release/pixelflow upscale input.png output.png
./target/release/pixelflow upscale --scale 2 --algorithm nearest input.png output.png
./target/release/pixelflow upscale --scale 2 --algorithm bilinear input.png output.png
```

Palette, RGBA, and greyscale inputs are expanded to RGB8 on load; alpha is
dropped. Output is always an RGB8 PNG at `scale` times the input
dimensions. `nearest-gpu` requires a GPU adapter and fails loudly without
one; on a machine with no adapter use `nearest`.

### `run`

```bash
./target/release/pixelflow run gtk-slashem
./target/release/pixelflow run --capture monitor --scale 2 zsnes
./target/release/pixelflow run --windowed gtk-slashem
./target/release/pixelflow run zsnes -- -s game.smc
```

`run` launches the target unmodified, waits 2 seconds for it to start,
then opens a portal ScreenCast session. The compositor asks which window
or monitor to capture; PixelFlow does not check that the source picked
belongs to the launched program. If window capture fails to start,
PixelFlow tries monitor capture once on the same backend and reports the
effective source. Press `f` to toggle between fullscreen and a windowed
preview while the window runs.

The requested algorithm resolves through the registry: `nearest` becomes
`nearest-gpu` when a GPU adapter is present, and the run reports the
effective algorithm and backend. An explicit `nearest-gpu` request with no
adapter is an error, not a silent CPU run.

## Platform support

- Capture runs on Linux with a Wayland compositor that provides XDG
  Desktop Portal ScreenCast and PipeWire. Wayland is the only capture
  backend. `upscale`, `backends`, `info`, and `algorithms` need no display
  and work anywhere the binary builds.
- Not implemented: X11 capture, Windows and macOS capture, input
  forwarding, DMA-BUF buffers (a DMA-BUF buffer is a typed error, not a
  silent fallback).
- Presentation preserves aspect ratio by default: uniform scaling,
  contain-fit to the canvas, black bars on the one axis that does not
  fill (sides or top/bottom, not both), never stretch.

## Build

Prerequisites: Rust 1.99 or later, and the system libraries the capture
and presentation backends need. On Debian or Ubuntu:

```bash
sudo apt-get install -y libpipewire-0.3-dev libspa-0.2-dev libwayland-dev libclang-dev pkg-config
```

A Nix development shell is also available (`nix develop`; see
`flake.nix`).

```bash
cargo build --release
```

The binary is `./target/release/pixelflow`. Development: `cargo test
--workspace` runs the suite (217 tests at the current baseline; the
fixture-based regression tests need `PIXELFLOW_FIXTURES_DIR` set and skip
without it). `pixelflow-bench` benchmarks Contour on synthetic frames
(`pixelflow-bench --one <width> <height> <scale>` runs one case).

## Configuration

- `PIXELFLOW_MEMORY_BUDGET` (bytes, plain integer) sets a working-memory
  budget for algorithms that declare a `memory_budget_bytes` parameter
  (Contour); unset or invalid means no budget.
- `CONTOUR_NO_FOLLOW=1` skips Contour's edge follower.
  `CONTOUR_DEBUG_RUNS=1` prints each accepted edge-follower run to
  stderr. `CONTOUR_THREADS` overrides Contour's strip thread count.

## Contour

Contour follows edges and reconstructs detail along them instead of
blurring. It is tuned for pixel art and does not preserve flat colors
exactly: its hard-light stage at 0.5 strength moves flat mid-tones by up
to just under 16 (for example `[128, 64, 32]` becomes `[128, 48, 20]`).
Alpha cannot survive processing: the pipeline works on RGB8. Both
behaviors are pinned by tests and are open product decisions, not bugs
to fix silently; Contour output does not change until the owner decides.

## Limitations

- **Contour is not a live algorithm at 1080p 4x.** Measured on a uniform
  1920x1080 frame at 4x: 151.9 s per frame in a debug build, 9.1 s per
  frame in a release build, byte-identical output. The synthetic bench
  (`pixelflow-bench`, release, game-art-shaped frame) measures 8.0 s at
  the same size and scale. The smallest benched case, 256x224 at 2x,
  takes 80.5 ms per frame in release, about 5x over the 16.7 ms frame
  budget. No real-time claim is made. For a fast live path use
  `--algorithm nearest` at `--scale 2`.
- **Live acceptance is unverified.** No end-to-end `run` has delivered
  frames on a live Wayland desktop yet. The frame transport itself was
  verified: a synthetic-source reproduction test (capture, slot,
  processing, publish, wake, presenter receipt) passes on the base code,
  so the live stall is Contour cost, not a transport defect. The missing
  presentation window on the owner's machine is still unexplained
  in-process: the window is created at loop start, independent of
  frames. Control run on a machine with a Wayland desktop:
  `pixelflow run <game> --capture monitor --scale 2 --algorithm nearest
  --windowed` — expect a window within ~1 s; if no window appears there
  either, the defect is presenter/adapter-side, not capture.
- **GPU execution is unverified on a device.** The `nearest-gpu` node is
  wired into the registry and the live path, but no adapter exists on
  the build VM, so no GPU compute has run against hardware.
- **Region routing runs each tile through its class's algorithm.**
  Tiles are classified and routed per class: text and sprites through
  Contour, flat and patterned backgrounds through Nearest, gradients
  through Bilinear, unknown or mixed tiles through the selected
  algorithm. Each branch runs whole-frame and the outputs are
  composited tile by tile, so Contour output is unchanged where it is
  the route. The incremental executor exists in the graph, but the live
  path runs whole-frame because Contour declares a global spatial
  dependency.

## Documentation

- `docs/architecture.md` — crate layout, core types, the processing
  pipeline, capture, and presentation.
- `docs/wayland.md` — the Wayland capture lifecycle, PipeWire buffers,
  and input forwarding.
- `docs/performance.md` — benchmark measurements for Contour, with the
  build profile stated for every number.
- `docs/algorithm-integration.md` — the algorithm contract, the
  registry, and how to add an algorithm.
- `docs/incremental-region-aware-architecture.md` — the incremental,
  region-aware processing specification and what of it is implemented.
- `docs/stage2-core-design.md` — the Stage II frame-model design record.
- `docs/requirements.md` — project status record: scope, per-crate
  status, known blockers, test baselines.
- `docs/engineering-mandate.md` — the owner's engineering mandate,
  verbatim; never edited.

## License

Apache 2.0. See `LICENSE`.
