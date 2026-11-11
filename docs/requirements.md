# PixelFlow — Requirements and Status

Authoritative project record. Keep it accurate: do not mark planned work
as implemented, partial work as complete, or a failing feature as fixed.
The owner's engineering mandate (`docs/engineering-mandate.md`) is verbatim
and is never edited; this document records scope, status, and decisions
that follow from it.

## 1. Project scope

PixelFlow is a general-purpose, real-time pixel-art graphics processing
framework and application. It is not a PNG upscaler, a dithering experiment,
or a Wayland capture demo. The project includes:

- Live desktop/application capture.
- A configurable image-processing graph.
- Independently configurable processing operations, composable
  into sequential pipelines, branches, layers, and saved
  profiles, not only selection of complete algorithms (see §3
  and §12 item 10).
- Multiple independent restoration, reconstruction, scaling, and filtering
  algorithms.
- CPU and GPU execution. GPU-first is foundational; CPU stays available
  for fallback, reference implementations, unsupported hardware,
  deterministic testing, and algorithms without GPU implementations.
- GPU-resident processing and resource management.
- Layers, masks, compositing, and configurable processing.
- Separate presentation, fullscreen/windowed output, and frame pacing.
- Region/content analysis and appropriate processing routes.
- Offline processing and benchmarking.
- Cross-platform architecture: Wayland is the immediate capture priority,
  not the entire platform scope. X11, Windows Graphics Capture, macOS
  ScreenCaptureKit, and BSD where practical remain in scope; no empty
  backends.

A correction changes the relevant implementation or priority; it does not
erase other requirements. Only remove a requirement on the owner's explicit
direction, or on demonstrated technical incompatibility with the
incompatibility explained.

## 2. Architecture and implementation status

Repository: `bonzupii/PixelFlow`, branch `main`. HEAD `951a0b2`
(2026-11-10). Seven crates, Rust 1.99.1, edition 2024:

| Crate | Contents | Status |
| --- | --- | --- |
| `pixelflow-core` | `Frame` (private fields, stride-aware, timestamp/sequence), `PixelFormat` (Rgb8, Rgba8, Gray8), `Processor` trait, `Node` trait, `Graph`/`GraphBuilder`, incremental execution (16x16 tiles, dirty propagation, per-node tile caching) | Implemented |
| `pixelflow-processing` | Algorithms `Contour`, `Nearest`, `Bilinear`, `PixelFlow`, `Bicubic`, `Lanczos3`, `Scale2x`, `Epx`; registry (`AlgorithmRegistry::builtin`); region analysis module (six classes, confidence scores) wired into the graph and the live run path | Implemented; framework integrated (`c6accd0`) |
| `pixelflow-gpu` | `GpuContext` (adapter selection, discrete → integrated → virtual → CPU last), `GpuImage` (resident `Rgba8Uint` texture, measured upload/download), `GpuNearestNode` (WGSL nearest-neighbour compute node, scales 2/4, graph `Node`), `ExecutionBackend` / `GpuCapability` | Implemented and integrated (`c1c3a88`); `ExecutionBackend` unified on the core contract type (`3ba4f27`); wired into the live path and the algorithm registry at `aea977c` (`nearest-gpu` registered; live `run` resolves `nearest` to the GPU node when an adapter is present); device run unverified — no adapter on this VM |
| `pixelflow-capture` | Capture backend abstraction (`SourceKind`: window/monitor) | Implemented |
| `pixelflow-wayland` | Portal ScreenCast session, PipeWire frames, format conversion | Implemented; live acceptance unverified (see §7) |
| `pixelflow-present` | wgpu presentation window, aspect-ratio-preserving layout, frame slot with latest-frame-wins, event-loop wake-up via event proxy | Implemented; no GPU compute (display only) |
| `pixelflow-cli` | `upscale` (PNG in/out), `run` (launch, capture, process, present), `backends`, `info`, `algorithms`, `bench` | Implemented; algorithm selection reads the registry (`--algorithm`, default `contour`) |

Processing contracts as they exist at HEAD:

- `Processor`: `name()`, `supported_scales()`, `stride_support()`,
  `tile_safe()`, `process(frame, scale) -> Result<Frame, ProcessError>`.
  One frame in, one frame out, integer scale, CPU only.
- `Node`: input/output ports, formats, `SpatialDependency`
  (`Local` / `Neighborhood{radius}` / `Global`), `TemporalDependency`,
  `stride_support()`, `tile_safe()`, `run(inputs)`.
  `ProcessorNode<P>` wraps any `Processor` as a node.
- `GraphBuilder` validates (ports exist, every input fed exactly once,
  connected formats match, no cycles), runs nodes in topological order,
  propagates errors, keeps an intermediate frame alive only while a
  consumer needs it.
- `Contour` declares `SpatialDependency::Global` — its edge follower
  spans whole scanlines — so any change reruns it whole-frame.
- `GpuResource` (`core/src/frame.rs`) is an opaque stub (private id, no
  backend). One real GPU compute node exists in `pixelflow-gpu`
  (`GpuNearestNode`, WGSL, integer texels, explicit device sync, no
  silent CPU fallback); its parity test runs through the real graph
  executor and skips honestly with no adapter. It was wired into
  the live path and the algorithm registry at `aea977c`.

### Operation contracts (requirement; not implemented)

The composition system works at the individual processing-operation
level, not only at the whole-algorithm level. An operation is an
independently configurable pure function: it produces its output
from its declared inputs and configuration, without secretly
modifying unrelated processing state or depending on hidden
execution-order side effects. The logical processing graph has
deterministic, inspectable data dependencies.

Operations declare their inputs, outputs, types, configuration,
and compatibility requirements, so the composition system can
connect them correctly. Operations need not all consume and
produce images. The typed intermediate values the system must
support include edge and confidence fields, orientation and
directional fields, region labels and masks, palette data, and
geometric feature data. A reconstruction operation may consume an
image and geometric feature data; a blending operation may consume
multiple image layers and a weight or mask field; a compositing
operation may consume multiple processed images. The system must
not impose an image-in/image-out interface on an operation whose
useful output is a feature map, mask, geometry, classification,
or other intermediate data.

Status: not implemented. The current `Node` interface is
frame-in/frame-out only: `NodeInputs`/`NodeOutputs` carry `Frame`
values, ports are named strings checked on pixel format, and
`run` takes `&mut self`, so purity is not guaranteed by the
interface. Extending ports to typed intermediate values and
requiring operations to be pure functions of declared inputs is
part of this work (see §12 item 10). Physical execution may fuse
operations, reuse resources, or optimize execution as long as the
logical graph, configuration, and observable processing semantics
remain intact. GPU execution must support the same compositional
model as CPU execution: the GPU path must not become a collection
of indivisible, hard-coded shaders while the CPU path is
composable.

## 3. Algorithm integration framework

Status: implemented and integrated (`c6accd0`, 2026-11-10). The framework provides:

- A stable algorithm contract: identity and stable registration, name and
  description, versioning, supported input/output formats, scale factors
  and output-dimension rules, validated algorithm-specific parameters,
  processing context and neighboring-pixel access, CPU and GPU execution
  capabilities, resource requirements and memory-budget awareness,
  determinism information, clear error reporting. It must accommodate
  both simple filters and complex multi-stage reconstruction algorithms —
  a multi-stage algorithm may expand into several graph nodes or declare
  intermediate resources; it is not forced into one `process()` call.
- A real registry and discovery path: the application discovers
  available algorithms, inspects capabilities, validates configuration,
  instantiates them, and executes them through the processing graph, with
  no special-case code per algorithm. Built-in vs external, CPU vs GPU,
  and single vs multi-backend implementations are distinguished.
  Integration boundary for now is source-level (Rust crate interface);
  no dynamic plugin ABI is invented before a real delivery format
  requires one. Claude's portable algorithms arrive as source.
- Graph integration: every algorithm usable through the normal graph.
  No separate execution path that only works in a benchmark or demo.
- CPU and GPU execution: GPU-first; CPU retained. GPU stages keep GPU
  residency; transfers are explicit, justified, and measured. A
  CPU-only algorithm still integrates correctly. A GPU implementation
  uses the same application-level integration model. GPU support is not
  claimed because an output texture is displayed by wgpu.
- Configuration and application integration: enumerate, query
  capabilities, select, configure, validate, execute offline and live,
  inspect errors and performance, compare implementations and backends.
  The CLI exposes the framework; the live application uses the same
  contracts.
- Demonstration: Contour and nearest-neighbour through the framework,
  plus at least one independently implemented test algorithm. An
  algorithm is addable without modifying capture, presentation, or CLI
  code. An integration example and guide ship with it
  (`docs/algorithm-integration.md`).

Completion criteria: existing algorithms registered and executed through
the framework; a separately developed algorithm integrated through the
documented contract; capabilities discoverable; graph executes algorithms
correctly; CPU execution works; GPU execution supported where implemented
with real GPU processing; offline and live paths use the framework; tests
verify contracts and failure cases; the integration example works;
documentation matches the implementation.

### Operation-level composition

The framework must support composition at the individual
operation level, not merely selection and chaining of complete,
monolithic algorithms. The composition system must not be
restricted to choosing one complete scaler from a registry and
applying it as an indivisible black box.

Complete algorithms remain available as selectable algorithms
and presets. Their internal processing logic must not be treated
as inherently indivisible: where practical, their constituent
operations are exposed through the reusable composition system.
This applies to Contour, PixelFlow, AaCut, LabelSet, and the
other processing algorithms. The objective is both convenient,
ready-to-use complete algorithms AND freedom to inspect,
configure, replace, rearrange, and combine their constituent
operations. A user must be able to use one algorithm's edge
detection with another algorithm's reconstruction, another
interpolation method, and a different blending operation, and
must not be restricted to combinations anticipated by the
original algorithm author.

The operation library is extensible, not a closed catalog. It
includes, at minimum:

- Analysis and feature extraction: edge detection; edge
  classification and direction estimation; orientation and
  structure-tensor analysis; corner and junction detection;
  geometric and silhouette analysis; subpixel edge positioning;
  color-region and palette analysis; dither detection and
  classification; texture and grain detection; gradient and
  tonal analysis; mask generation and region classification.
- Reconstruction and transformation: geometric edge
  reconstruction; subpixel contour reconstruction; corner
  reconstruction; curve and silhouette reconstruction;
  interpolation and resampling; palette-aware reconstruction;
  dither preservation, enhancement, and reconstruction; texture
  and fine-detail reconstruction; noise and grain processing;
  contrast and tonal processing; anti-aliasing and transition
  reconstruction; spatial and scale-dependent transformations.
- Blending and compositing: alpha blending; weighted blending;
  additive blending; multiplicative blending; masked blending;
  layer compositing; edge-aware blending; palette-aware
  blending; multi-source compositing; cross-fading between
  processing results.

Status: not implemented as an operation library. The graph
machinery for chains and branches exists; no production blend,
layer, or composite node exists in any library crate, and no
operation is independently selectable today. See §12 item 10.

## 4. Contour, unfinished

PixelFlow is the flagship algorithm. Contour is unfinished. It still misses
the desired result. Do not declare it complete and do not treat recent
improvements as proof it is finished.

Contour is a complete multi-stage restoration system. Dithering
enhancement is one feature, independently controllable, not the
algorithm's purpose, and never a justification for sacrificing the rest.
Do not simplify Contour into nearest-neighbour plus arcs, strokes, or a
dither filter. Do not remove existing reconstruction stages. Nearest
neighbour remains a separate baseline algorithm. Contour's
constituent operations are exposed through the composition system
where practical (see §3): its edge detection, orientation
analysis, reconstruction stages, and blending are independently
configurable operations, not hard-coded internals. Exposing
operations does not remove or simplify any reconstruction stage.

Contour must address all of these, as complementary responsibilities, not
mutually exclusive modes:

- Edge detection and geometric edge reconstruction.
- Subpixel edge positioning and edge following.
- Corner reconstruction and junction handling.
- Shape and silhouette preservation.
- Texture and detail reconstruction.
- Palette and color-region relationships.
- Flat-color region preservation.
- Gradient and shading reconstruction.
- Dither detection, preservation, enhancement, and reconstruction.
- Noise and grain handling.
- Contrast and tonal relationships.
- Anti-aliasing and transition quality.
- Fine-detail preservation.
- Scale-dependent reconstruction behavior.

Every substantial algorithm change is evaluated against the previous
implementation across every characteristic independently, with significant
pixel changes quantified and representative outputs inspected at native
and enlarged scales. Reject changes with severe regressions in unrelated
responsibilities. Per-characteristic regression coverage (flat regions,
edges, curves, corners, gradients, ordered and irregular dithering, fine
textures, silhouettes, mixed-content images) with complete-image
comparisons, not isolated crops. Expected outputs are never updated
merely to make a changed algorithm pass.

Measured state at HEAD: strip-based redesign (commit `9ad6e45`),
~19 bytes per output pixel peak (was ~29); 1080p at 4x measured 8.0 s /
1,060 MiB (median of 5). No real-time claim: smallest measured case
(256x224 at 2x, 80.5 ms) is ~5x over the 16.7 ms frame budget. GPU
acceleration must preserve reconstruction behavior; CPU remains the
reference/fallback until GPU parity is demonstrated quantitatively and
visually.

Known pinned behaviors (locked by tests, open product decisions for the
owner): Contour shifts flat mid-tones by up to ~16 via its hard-light
stage ([128,64,32] → [128,48,20]); alpha cannot survive processing
(RGB8 only). Do not change Contour output until the owner decides;
the flat-color question is tied to §9 routing.

Regression reference: regression fixtures are copyrighted game
screenshots, never committed; durable local copies live outside the repo
and are used via `PIXELFLOW_FIXTURES_DIR`. Reference numbers that must
stay identical: all fixtures max abs diff 1, 0 pixels >1, except mario
max 34, exactly 6 pixels >1.

Quality judgment: reconstruction quality is judged by Claude, which
inspects the output images. Rusty reports measurements only (timings,
diffs, color counts), never a quality verdict. Pixel-count metrics are
evidence about one property, not a substitute for visual review.

The C retroscale "pixelflow" library (a simplified clone+arcs+strokes
algorithm sharing the name) is test/reference material, not the Rust
Contour product algorithm; do not confuse the two.

## 5. CPU/GPU architecture requirements

- Adapter fallback chain: discrete GPU → integrated graphics → CPU last
  resort. CPU-only stays available but is not the default.
- Real GPU compute via wgpu compute shaders (already a dependency via
  presentation; hardware-agnostic across Vulkan/Metal/DX12/OpenGL). Do
  not add a separate Vulkan or OpenGL dependency.
- GPU-first design target: 60 FPS / ~16.7 ms end-to-end. Measured
  quantities: capture-to-processing latency, per-node time, GPU time,
  upload/readback volume, sync stalls, pacing, drops, memory, reuse.
- Workloads: 256×224 @2x/@4x, 854×480 @2x/@4x, 1920×1080 @2x/@4x,
  3840×2160 where hardware permits. CPU vs GPU on the same
  inputs/hardware. Image-quality comparison vs CPU Contour.
- GPU compute exists as one node: `pixelflow-gpu` provides
  `GpuNearestNode` (WGSL nearest-neighbour, scales 2/4) through the
  graph executor, with adapter selection discrete → integrated →
  virtual → CPU last resort and measured transfers. The WGSL shader
  is naga-validated but has never executed: this VM has no wgpu
  adapter (no Vulkan ICD, no lavapipe), so the parity check has never
  run against a device. A device run on hardware with an adapter is
  required before any GPU claim is made. The node is wired into the
  live path and the registry at `aea977c` (`nearest-gpu` id; live
  `run` resolves `nearest` to it when an adapter is present, reports
  the effective backend, and errors loudly for an explicit GPU
  request with no adapter).

## 6. Capture and presentation requirements

- Wayland capture is the immediate priority: XDG Desktop Portal
  ScreenCast session → PipeWire frames. The implementation is the
  standard portal flow, not compositor interception.
- Live pipeline reliability: launch coordination, portal session/source
  selection, PipeWire startup, format negotiation, cancellation/timeout/
  termination, stride/offset/format/buffer lifetime, frame delivery and
  backpressure (newest frame, no stale freeze), event-loop wakeups,
  presenter init errors producing a failing exit status, clean shutdown,
  fullscreen/aspect, honest source reporting (never claim the
  portal-selected source belongs to the launched process).
- Presentation and upscaling preserve aspect ratio by default: uniform
  scaling (same factor on both axes), contain-fit to the canvas (scale
  = min(canvas_w/src_w, canvas_h/src_h)), black bars on the one axis
  that does not fill (sides OR top/bottom, not both), never stretch.
  Integer scales are the algorithms' native path; a uniform residual
  reaches the contain-fit size. For SNES sources on a widescreen canvas:
  fill the height, bars on the sides.
- The 2-second WINDOW_GRACE sleep is a known placeholder where a real
  signal is needed.

## 7. Known blockers and unverified items

- Product framing correction (2026-11-10, owner): PixelFlow wraps a
  program, intercepts its graphical output, and replaces it with an
  upscaled enhanced fullscreen of the program. Graphics API
  interception is the specified approach. PixelFlow is not a screen
  recorder, not a desktop capture application, not a screencast
  client, and not a tool for capturing arbitrary monitor contents
  through a desktop portal. The ScreenCast/portal/PipeWire stack
  described in the entries below is the superseded approach: it
  entered the record as the 2026-11-09 Milestone 2 direction, not as
  the owner's specification. The entries below are preserved as
  record of that approach. The `run` command now uses interception
  (`docs/interception.md`): an `LD_PRELOAD` shim interposes the
  target's swap calls and streams its presented frames to PixelFlow.
  The `--capture` flag is removed; there is no window/monitor source
  selection and no monitor fallback. Live verification now happens
  in this VM under a headless Wayland compositor with software
  rendering, not on the owner's machine.
- Live verification under a headless Wayland compositor
  (2026-11-10, this VM): labwc 0.7.1 headless, 1280x720 output,
  XWayland, Mesa lavapipe. A real GLX target (320x240, cycling
  background, sliding yellow rectangle) was launched through
  `pixelflow run` at 2x nearest, resolved to nearest-gpu on the
  lavapipe adapter (GPU nearest matches CPU nearest exactly at
  2x and 4x). Compositor screenshots show the upscaled target
  fullscreen with correct letterboxing (160-pixel black bars
  each side) and sustained motion (682,020 pixels differ between
  shots 5 seconds apart). Shutdown defect found by this run and
  fixed: killing the target left PixelFlow alive with its window
  open indefinitely, because the processing thread set the exit
  flag but sent no final wake and the presentation loop sleeps
  on `ControlFlow::Wait`; the fix wakes the loop once more after
  the exit flag is set (`crates/pixelflow-cli/src/run.rs`), with
  a regression test asserting exactly one wake with no frames
  published. Live after the fix, killing the target exits
  PixelFlow in 1 second with exit code 0, and a relaunch run
  starts, presents, and exits the same way. Halo finding,
  recorded not fixed: 16x16 tiles the rectangle straddles
  classify Sprite and route to Contour, whose hard-light stage
  shifts the flat background inside those tiles from
  (60, 0, 200) to (44, 0, 216); this is the known Contour
  flat-color shift at tile granularity, an open product decision.
  Still unverified: the EGL swap path, Vulkan interception,
  input forwarding, live acceptance on a real desktop with a
  hardware GPU, and 60 fps presentation (llvmpipe presents
  about 9 fps at 2x nearest-gpu here). Full detail in
  `docs/interception.md`.
- Wayland live acceptance: UNVERIFIED. The portal fix (commit `4b5cbb4`)
  was verified claim-by-claim against the code by an independent
  verification worker (2026-11-10): cursor modes queried before use,
  persist gated on interface version >= 4, selection response read
  before Start, Closed signal subscribed, partial sessions closed on
  failure paths, serial targeting with node-id fallback, single shared
  process runtime — all confirmed with file:line evidence. The mock
  portal test genuinely executes under `dbus-run-session` (1 passed,
  1.10 s, all scenarios: version 6/embedded, hidden-only,
  metadata-only, nothing-advertised, version 1, no-serial,
  hyphenated-serial, cancelled selection, closed-after-selection,
  cancelled start). No ashpd async gap: the response is awaited
  inside the call. But no desktop, portal, PipeWire daemon, or wgpu
  adapter exists on this VM, so no real compositor dialog, real
  stream, or frame has been observed. The owner's machine (NixOS,
  xdg-desktop-portal 1.20.4) is the live test environment; his next
  `pixelflow run` is the acceptance test. His compositor and
  ScreenCast backend are now known (2026-11-10 11:27): MangoWM
  0.17.4 (wlroots-based), xdg-desktop-portal 1.20.4,
  `/etc/xdg/xdg-desktop-portal/mango-portals.conf` sets
  `org.freedesktop.impl.portal.ScreenCast=wlr` (default=gtk,
  Screenshot=wlr, Secret=gnome-keyring), both gtk and wlr backend
  services running.
- SelectSources cancellation, cause confirmed on his machine: the
  wlr backend's chooser fails at SelectSources, which surfaces as
  portal-side Response code 1 (`Portal request was cancelled`).
  The fix must be backend-general, not machine-specific: wlr shows
  its chooser at SelectSources, GNOME/KDE at Start, so the same
  user action fails at different protocol steps on different
  backends. Required: robust lifecycle on both steps, fresh
  sessions on retry, no silent target changes, monitor fallback
  per the `CaptureConfig` contract. See the backend-general gaps
  below.
- New failure on the owner's machine after pulling the P1 commits
  (2026-11-10 10:53): session creates and cursor negotiation works
  (ScreenCast v5, offered SourceType(3), advertised CursorMode(3),
  selected EMBEDDED), then SelectSources fails with `portal error:
  select sources response: Portal request was cancelled`, and the
  close after failure fails with `UnknownMethod: Object does not
  exist at path`. Mechanism (verified against portal.rs and ashpd
  0.14.0 source): the error is `ResponseError::Cancelled` from the
  stored selection response — our code holds the request future to
  completion (ashpd awaits the Response signal inside the call),
  there is no timeout and no cancel path on the portal side of our
  code, the session handle is valid, and the request is well-formed
  for v5. The cancellation is a portal-side Response code 1: the
  source picker dialog was dismissed, or the compositor backend
  cancelled the request. The close-after-failure UnknownMethod is
  expected cleanup, not a second failure: the portal destroyed the
  session object when the request was cancelled, so `Close` on that
  path answers "object does not exist". Fix landed in the Wayland
  follow-up (`ffff504`): tolerant close-after-failure
  classification (`session_already_gone`), a factual
  cancelled-selection diagnostic naming the two portal-side causes,
  and a mock scenario for a gone session on close. A cancelled
  selection is still a terminal failure for that run until the
  backend-general retry/fallback work lands (see below).
- Backend-general gaps (verified against the code at `ffff504`;
  fixed and integrated at `d3c4b41`, 2026-11-10): (1) Source-type
  verification: `verify_stream_source` in `portal.rs` fails the
  open with `WaylandError::SourceMismatch` naming requested vs
  actual when the portal returns a stream of another kind; an
  unmapped type fails as a `Portal` error naming the raw type.
  (2) Monitor fallback: `run.rs` `initialize_capture` tries
  monitor once on the same backend when window initialize fails,
  eprintlns both the failure and the fallback, and reports the
  effective source (`pixelflow: capturing {effective_source}`);
  both failures named when both fail. (3) Fresh sessions on
  retry: the fallback reuses the same backend, and a failed
  `initialize` leaves it uninitialized (verified by unit test
  `failed_initialize_leaves_the_backend_reusable`), so the
  monitor attempt opens a fresh session; the mock asserts a
  different session path for the second attempt. (4)
  Cancelled-Start diagnostic: `start_response_error` gives a
  cancelled Start the same factual treatment as cancelled
  SelectSources. (5) Gap 3 from verification stands: ashpd 0.14
  parses only the `pipewire_serial` (underscore) key while the
  portal specification wires `pipewire-serial` (hyphen), so
  node-id fallback is the live path; fixing it needs a raw D-Bus
  Start call bypassing ashpd's typed `Request<Streams>`, which
  was judged not small and left as is. Full suite 199 passed /
  0 failed at `d3c4b41` (baseline 194 + 5 new); live acceptance
  (real frames from Window and Monitor separately on the owner's
  machine) still unverified.
- Live frame delivery failure (owner run 12:00 2026-11-10, HEAD
  `6bbc989`): monitor fallback starts and PipeWire connects, but
  no frames reach presentation — no output window, game stays at
  native resolution. Window capture still fails at SelectSources.
  Verdict from the frame-delivery work (`d2e057e`, 2026-11-10):
  no defect exists in the frame transport — a synthetic-source
  reproduction test (capture → slot → processing → publish → wake
  → presenter receipt) passes on the base code, and the earlier
  review findings 1, 2, 3, 5 were re-verified fixed in the current
  code. What stops frames is measured cost: Contour @4x on a
  uniform 1920x1080 frame takes 151.9 s in the debug binary
  (coordinator-measured, matches the worker's 152.2 s) and 9.1 s
  in release, byte-identical output — so for any observation under
  ~2.5 minutes the debug run looks exactly like the report. The
  owner's 13.83 s build is a debug build (a release build takes
  7m41s on this VM), so his live runs pay the debug cost. Even in
  release 9.1 s/frame is 0.11 fps, not live video: Contour @4x on
  1080p input is not a live algorithm at any build profile. The
  missing presentation window is NOT explained by any in-process
  defect found — the window is created at loop start independent
  of frames — and still needs the owner's machine: presenter /
  adapter boundary or the process having exited. Control run
  (fast path): `pixelflow run <game> --capture monitor --scale 2
  --algorithm nearest --windowed` — expect a window within ~1 s;
  if no window appears there either, the defect is
  presenter/adapter-side, not capture.
- Cancelled window selection is terminal in code (2026-11-10,
  `b08a782`/`5fd8727`/`c48540c`): a cancelled SelectSources or
  Start response maps to typed `WaylandError::SelectionCancelled`
  / `CaptureError::SelectionCancelled` (one mapping,
  `selection_response_error` in `portal.rs`, for both calls), and
  `run.rs` `initialize_capture` ends the run on it with no monitor
  attempt; an unavailable window (`SourceUnavailable`, now typed
  through `CaptureError`) still falls back to monitor once.
  Mock-portal scenarios in `run.rs` pin both paths, and
  `portal.rs` pins the mapping. Full suite 233 passed / 0 failed
  (baseline 226 + 7 new). Integrated at `38cdf67` (rebased onto
  `b0a9a95`, gate verified independently: build, suite 233/0, fmt,
  clippy gate). The window request itself is unchanged:
  it differs from the monitor request only in the `types` bit
  (`run_session`), and the wlr backend runs its chooser at
  SelectSources, so a Cancelled response there means the chooser
  failed or was dismissed. Whether any chooser UI appeared on the
  owner's machine is still open.
- Portal-fix gaps found by verification, all fixed in the
  follow-up (`ffff504`): (1) Closed detection is now checked
  immediately before Start and after Start succeeds before
  `open_pipe_wire_remote`; a `receive_closed` subscription
  failure now fails the open instead of logging and continuing.
  (2) The `capture.rs` unclosed path on `take_fd() == None` now
  closes the session explicitly. (3) See backend-general gap
  (5) above. (4) A `PORTAL_RUNTIME` build failure is retried
  instead of cached forever. (5) The mock-portal test now fails
  without a session bus (message names `dbus-run-session`); it
  passes under `dbus-run-session` with all scenarios executing.
- GPU execution: UNVERIFIED on a device. The crate is integrated;
  no adapter on this VM.
- Previous coordinator `338e91a0` (spawned 2026-11-10 07:22) is
  superseded by the current coordinator and shut down (confirmed).
  It landed the portal fix and the PNG writer fix, then went quiet.
  Its tree state was clean at handover; no uncommitted work was lost. A release
  test run (`cargo test --release`, separate target dir) observed in
  flight at 16:27 UTC may belong to its subtree.
- "Algorithm 20" removal from user-facing output: done (`c6accd0`,
  framework integration; `backends` prints the mechanism
  description with no "algorithm 20"). The `contour.rs` module
  header was scrubbed too. Internal fixture files (`*_20_*`) and
  test identifiers were not renamed.
- `docs/architecture.md` said Frame's fields are "public" — stale since
  encapsulation; fixed in the docs cleanup (`b92eb91`, integrated
  at `9f9fb0e`): fields are private with accessors.
  The same cleanup corrected the CLI section (`run` takes
  `--algorithm`; five commands), the test counts (217), the
  presentation transport (replaceable slot, not a channel), and the
  stale GPU/backends wording in `docs/algorithm-integration.md`.
- `cargo clippy -p pixelflow-wayland --all-targets` exits 0
  (2026-11-10, after `ffff504` cleared the 10 `mock_portal.rs`
  errors structurally: `MockFaults` grouping, sync zbus methods,
  recorded call arguments, scenario helpers). The wayland lib was
  already clean. No `#[allow]` anywhere.
- Wayland follow-up: integrated and pushed at `ffff504`
  (2026-11-10); full suite 194 passed / 0 failed at that HEAD.
- Per-stage Contour timing is unmeasured (the bench has no per-stage
  support); allocation profiling with actual tools is still outstanding.
- Live `--help` execution has been run on this VM since the dev
  libraries were restored (docs cleanup, 2026-11-10, release binary
  built in the worktree): `pixelflow 0.1.0`, five commands
  (`upscale`, `run`, `backends`, `info`, `algorithms`), four
  registered algorithms (`contour`, `nearest`, `bilinear`,
  `nearest-gpu`, all 2x/4x), `run` takes `--algorithm` (default
  `contour`). The README was rewritten against those outputs
  (`d7a7107`).

## 8. Test and quality gates

- `cargo fmt --check`, `cargo clippy --workspace --all-targets`
  (workspace lints deny clippy all+pedantic, unwrap/expect/panic/
  todo/unimplemented, unsafe forbidden, missing docs; no `#[allow]`
  anywhere; the 30 known `contour.rs` clippy diagnostics are a ceiling,
  not a floor — a run may fix one), `cargo test --workspace`.
- `warnings` and `unused_must_use` are denied workspace-wide (commit
  `8fb32af`).
- Full suite at `8fb32af`: 145 passed / 0 failed, regression exact,
  per the driver body. Baseline at `4b1289a` verified by the
  coordinator (debug, 2026-11-10, `cargo test --workspace`, exit 0):
  155 passed / 0 failed — core 28, graph 22, CLI 12, present 7,
  processing 2 + budget 6 + graph_nodes 6 + region_analysis 26,
  regression 3, synthetic 17, wayland 25, mock_portal 1. The
  regression run did not set `PIXELFLOW_FIXTURES_DIR`, so
  fixture-based regression identity was not exercised in this run.
  Baseline at `c1c3a88` verified by the coordinator (debug, 2026-11-10,
  `cargo test --workspace` under `dbus-run-session`, exit 0): 168 passed
  / 0 failed — the 155 from `4b1289a` plus 13 from `pixelflow-gpu`
  (12 unit, 1 parity by honest skip: no adapter on this VM).
  Baseline at `ffff504` verified by the coordinator (debug, 2026-11-10,
  `cargo test --workspace` under `dbus-run-session` with
  `PIXELFLOW_FIXTURES_DIR` set, exit 0): 194 passed / 0 failed —
  the 190 from the framework integration plus 4 new classifier
  tests from the Wayland follow-up. Regression 3 passed with
  fixtures exercised (38 s). `cargo clippy -p pixelflow-wayland
  --all-targets` exits 0; `cargo clippy --workspace --all-targets`
  still fails on the 30 known `contour.rs` diagnostics (ceiling).
  Baseline at `d3c4b41` verified by the coordinator (debug, 2026-11-10,
  `cargo test --workspace` under `dbus-run-session` with
  `PIXELFLOW_FIXTURES_DIR` set, exit 0): 199 passed / 0 failed —
  the 194 from `ffff504` plus 5 new backend-general tests (1 capture
  reusability, 4 CLI fallback). Baseline at `aea977c` verified by the
  coordinator (debug, 2026-11-10, same gates, exit 0): 213 passed
  / 0 failed — the 199 from `d3c4b41` plus 14 new GPU wiring tests
  (6 GPU factory unit, 3 registry integration, 4 `run.rs`, 1 `main.rs`).
  Baseline at `d2e057e` verified by the coordinator (debug, 2026-11-10,
  same gates, exit 0): 217 passed / 0 failed — the 213 from `aea977c`
  plus 4 new frame-path tests (3 processing-thread, 1 presenter
  receipt). The frame-path reproduction test passes on the base code
  as well as after the change: it pins the path and localizes the
  live stall to Contour execution time; it is not a
  failing-before/passing-after pair.
  Baseline at `afa6f46` verified by the coordinator (debug, 2026-11-10,
  same gates, exit 0): 226 passed / 0 failed — the 217 from `d2e057e`
  plus 9 new region-routing tests (8 `region_analysis`, 1 CLI).
  Baseline at `27e6113` verified by the coordinator (release, 2026-11-10,
  `cargo test --release --workspace` under `dbus-run-session` with
  `PIXELFLOW_FIXTURES_DIR` set, exit 0, 352 s): 226 passed / 0 failed —
  same total as the debug `afa6f46` run. Fixture-exercised evidence:
  regression 3 passed in 4.11 s (a skipped run finishes in 0.00 s);
  no skip lines. Full log: `goals/pixelflow/hidden_files/release-suite-27e6113.log`.
  Caveat, recorded 2026-11-10: the run's raw output was not preserved
  (the command piped through grep); that log file is a transcription
  of the delivered run output, not a tee'd raw log. The tally is the
  coordinator's run, not independently verified. Future suite runs
  keep their raw output.
  Correction, recorded 2026-11-10: the `27e6113` baseline is void as a
  green-tree claim. CI failed the `cargo build --release` step on
  `27e6113`, `5c03b6f`, and `a342755`: `RoutedGraphs::graph` and
  `RoutedGraphs::node_for_tile` in `crates/pixelflow-cli/src/run.rs`
  were called only from the test module, so non-test builds failed
  with a dead-code error under the workspace warning denial, while
  test builds used them and passed. The 226/0 test tally may describe
  the test run, but a tree that does not build is not green.
  Baseline at `0ba9d38` verified by the replacement coordinator
  (release, 2026-11-10, `cargo build --release` exit 0,
  `cargo test --release --workspace` under `dbus-run-session` with
  `PIXELFLOW_FIXTURES_DIR` set, exit 0): 226 passed / 0 failed,
  regression identity mario max 34 with exactly 6 pixels >1, all
  others max 1 with 0 pixels >1. Full gate re-run independently by
  the coordinator (build, suite, fmt, clippy gate), raw logs in
  `goals/pixelflow/hidden_files/ci-verify-suite-release.log`,
  `ci-verify-clippy.log`, `ci-verify-fmt.log`. The fix removed the
  two dead methods and rewrote the three routed-frame tests through
  the live paths; no routing behavior changed.
  This replaces the stale driver-run release 217 at `d2e057e`, which
  predates the region-routing commits and must not be carried forward.
  Baseline at `d2547b1` (PixelFlow soft-branch correction,
  2026-11-10): 254 passed / 0 failed, commit-recorded in the
  commit message; not independently verified by a driver run.
  Baseline at `134a052` (bicubic port, 2026-11-10): 267 passed /
  0 failed; tally read from the owner's suite log
  `goals/pixelflow/hidden_files/cg-bicubic-suite-release.log`
  (sum 267 passed, 0 failed across the result blocks, no
  failures); oracle bit-identical per
  `cg-bicubic-oracle.log`.
  Baseline at `e8b475f` verified by this driver run (release,
  2026-11-10, `cargo build --release` exit 0, `cargo fmt --check`
  exit 0, `ci/clippy-gate.sh` exit 0 with 30 known contour.rs
  diagnostics and 0 elsewhere, `cargo test --release --workspace`
  under `dbus-run-session` with `PIXELFLOW_FIXTURES_DIR` set,
  exit 0): 280 passed / 0 failed, 24 result blocks, no failures.
  Fixture-exercised evidence: regression ran 6 tests, including
  `lanczos3_fixture_baseline_at_4x`,
  `bicubic_fixture_baseline_at_4x`,
  `pixelflow_fixture_baseline_at_4x`, and
  `contour_matches_python_reference_at_4x`, all ok; no skip
  lines. Raw logs:
  `goals/pixelflow/hidden_files/driver-lanczos3-verify-fmt.log`,
  `driver-lanczos3-verify-build-release.log`,
  `driver-lanczos3-verify-clippy.log`,
  `driver-lanczos3-verify-suite-release.log`. The run verified the
  lanczos3 worktree sources at `e8b475f`, which the owner had
  already fast-forwarded onto main and pushed before the run
  finished; the tree content is identical.
  Baseline at `91bf243` verified by the coordinator run (release,
  2026-11-10, `cargo build --release` exit 0, `cargo fmt --check`
  exit 0, `ci/clippy-gate.sh` exit 0 with 30 known diagnostics
  and 0 elsewhere, `cargo test --release --workspace` under
  `dbus-run-session` with `PIXELFLOW_FIXTURES_DIR` set, exit 0):
  292 passed / 0 failed. Oracle bit-identical per the
  coordinator's own comparison run
  (`cg-scale2x-verify2-oracle.log`, reference outputs
  regenerated from the verbatim reference file). Raw logs:
  `goals/pixelflow/hidden_files/cg-scale2x-verify3-fmt.log`,
  `cg-scale2x-verify3-build-release.log`,
  `cg-scale2x-verify3-clippy.log`,
  `cg-scale2x-verify3-suite-release.log`.
  Baseline at `951a0b2` verified by the epx port run (release,
  2026-11-10, `cargo build --release` exit 0, `cargo fmt --check`
  exit 0, `ci/clippy-gate.sh` exit 0 with 30 known diagnostics
  and 0 elsewhere, `cargo test --release --workspace` under
  `dbus-run-session` with `PIXELFLOW_FIXTURES_DIR` set, exit 0):
  306 passed / 0 failed. Oracle bit-identical per the
  port run's own comparison
  (`cg-epx-oracle.log`, reference outputs
  regenerated from the verbatim reference file). Raw logs:
  `goals/pixelflow/hidden_files/cg-epx-verify2-fmt.log`,
  `cg-epx-verify2-build-release.log`,
  `cg-epx-verify2-clippy.log`,
  `cg-epx-verify2-suite-release.log`. A VM restart mid-task
  wiped the `/usr` dev libraries; `libpipewire-0.3-dev` and
  `libclang-dev` were reinstalled from Ubuntu Noble before the
  suite re-run.
  Baseline at `15a8fb6` verified by the docs worker (debug, 2026-11-10,
  same gates, exit 0): 217 passed / 0 failed — same total as
  `d2e057e` after the README/docs rewrite and the `run.rs`
  simplification (duplicate shutdown discards collapsed into one
  helper, unreachable stage-list branches removed); no behavior
  change.
- Environment note (2026-11-10): a VM restart mid-task wiped the
  `/usr` dev libraries (`libpipewire-0.3`, `libclang`, C headers).
  They were restored from Ubuntu Noble `.deb` files; `cargo` runs in
  this VM need `LIBCLANG_PATH=/usr/lib/x86_64-linux-gnu/libclang-18.so.1`
  and `BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/14/include"`,
  or `libspa-sys` bindgen fails (`stdbool.h` not found). `/usr` does
  not survive VM replacements; nothing reinstalls these automatically.
- Never claim a test passed unless its command completed successfully.
  Never claim the application works end to end unless that path was
  actually exercised. A mock is not live capture. A GPU presenter is
  not GPU processing. A shader that compiles but is not used by the
  application is not an implemented feature.
- Evaluation harness requirements: identical source inputs, identical
  scales and output settings, reproducible configurations, reference
  outputs, side-by-side generation, per-algorithm and per-stage timing,
  memory measurements, format/dimension validation, quantitative
  difference reports, machine-readable manifests, actual output images
  available for human inspection. Test material covers flat regions,
  palette boundaries, horizontal and vertical edges, diagonals, curves,
  corners, intersections, narrow features, gradients, shading, ordered
  and irregular dithering, texture, grain, silhouettes, isolated pixels,
  and mixed-content images. Previous known-good outputs are kept; references
  are never silently replaced to make regressions pass.

Composition tests: swapping one operation for another (edge
detector, reconstruction, blend) changes only the intended
output; a typed-port mismatch (an edge field connected where an
image is expected) fails at graph build, before execution; a
saved pipeline definition round-trips (save, load, identical
output); a preset customized at the operation level still runs
through the normal graph executor with the same dirty-set and
caching behavior; a GPU composition and the same CPU composition
produce matching output.

## 9. Active workstreams and ownership

Coordinator: the current coordinator session (replacing `338e91a0`).
Owns project state, architecture decisions, task assignment,
integration, commits, and final verification. Only the coordinator
integrates worktree work onto `main`.

| Workstream | Owner | Scope | State |
| --- | --- | --- | --- |
| Algorithm framework | framework worker, worktree `~/workspace/pixelflow-framework`, branch `framework/algorithm-registry` | Contract, registry, graph/CLI integration, test algorithm, "algorithm 20" removal, `docs/algorithm-integration.md` | Integrated and pushed at `c6accd0`; `ExecutionBackend` unified on the core contract type; contour header scrubbed |
| GPU execution | GPU worker, worktree `~/workspace/pixelflow-gpu`, branch `gpu/compute-path` | `pixelflow-gpu` crate, adapter fallback chain, GPU-resident images, WGSL nearest-neighbour compute node with CPU parity | Integrated and pushed at `c1c3a88`; device run unverified (no adapter on this VM) |
| GPU live-path wiring | GPU wiring worker, worktree `~/workspace/pixelflow-gpu-wire`, branch `gpu/live-path` | Register `nearest-gpu` in the CLI registry; live `run` path resolves `nearest` to the GPU node with an adapter, reports the effective backend, errors loudly for explicit GPU requests with no adapter | Integrated and pushed at `aea977c`; full suite 213 passed / 0 failed; device run and live-path parity still unverified (no adapter on this VM) |
| Frame delivery | frame-delivery worker, worktree `~/workspace/pixelflow-framepath`, branch `fix/frame-delivery` | Trace the frame path (PipeWire buffers, processing, presentation) to where frames stop on the owner's machine; smallest fix; reproduction test | Integrated and pushed at `d2e057e`; full suite 217 passed / 0 failed; verdict: no transport defect, stall is Contour cost (151.9 s debug / 9.1 s release per 1920x1080 @4x frame); missing presentation window still unexplained in-process, needs the owner's-machine control run |
| Region routing | region-routing worker, worktree `~/workspace/pixelflow-region-routing`, branch `framework/region-routing` | Meaningful per-class routing with genuinely different paths (not all-to-Contour); routing table + resolver; masks/overrides precedence; hysteresis/halo | Integrated and pushed at `afa6f46`; full suite 226 passed / 0 failed; live acceptance and any GPU-device run still unverified |
| Operation-level composition | (unassigned) | Operation library, typed ports, blend/layer/composite nodes, pipeline definitions and profiles, GUI exposure; see §3 and §12 item 10 | Not started; queued behind scaler ports (§12 item 9) |
| Docs cleanup | docs worker, worktree `~/workspace/pixelflow-docs`, branch `docs/readme-cleanup` | README rewritten as a verified product interface; per-document disposition (retained/corrected, none removed); `run.rs` simplification (shutdown discards collapsed, unreachable stage branches removed) | Integrated and pushed at `9f9fb0e`; full suite 217 passed / 0 failed; `docs/engineering-mandate.md` untouched |
| Wayland verification | verification worker, main tree read-only | Verify commit `4b5cbb4` claim-by-claim, run `cargo test -p pixelflow-wayland` | Done 2026-11-10: all 7 claims verified in code; `cargo test -p pixelflow-wayland` exit 0 (25 unit + 1 mock); mock genuinely exercised under `dbus-run-session`; 5 gaps recorded in §7 |
| Wayland follow-up | follow-up worker, worktree `~/workspace/pixelflow-wayland-fixes`, branch `wayland/closed-detection-fixes` | Fix verification gaps 1, 2, 4, 5; assess gap 3 (serial vs hyphen key); tolerant close-after-failure, cancelled-selection diagnostic, gone-session mock scenario; mock clippy cleanup | Integrated and pushed at `ffff504`; full suite 194 passed / 0 failed; wayland clippy clean |
| Wayland backend-general | backend-general worker, worktree `~/workspace/pixelflow-wayland-general`, branch `wayland/backend-general` | Backend-general selection handling (wlr chooser at SelectSources, GNOME/KDE at Start); source-type verification (no silent target changes); monitor fallback per `CaptureConfig`; fresh sessions on retry; cancelled-Start diagnostic; mock scenarios | Integrated and pushed at `d3c4b41`; full suite 199 passed / 0 failed; live acceptance (real frames, Window and Monitor) still unverified |
| Requirements/status | coordinator | This document | This file |

Concurrent writers use isolated worktrees. Workers commit in their
worktrees and do not push to origin; the coordinator reviews and
integrates. A worker being alive is not the objective; verified progress
is. Bounded checkpoints: each step produces a commit, a test result, or
a documented blocker.

## 10. Standing rules

- Pushes use the repo-scoped deploy key `~/.ssh/id_ed25519_pixelflow`
  with full `GIT_SSH_COMMAND` flags (see `~/TOOLS.md`); never
  force-push; rebase onto origin `main` before committing; verify with
  `ls-remote` after every push. A local file is not a commit, a commit
  is not a push, a push command is not proof of remote state.
- `flake.nix` and `flake.lock` are the owner's files (he fixed them
  himself, 2026-11-10). No worker modifies them without his explicit
  order. Never install or run Nix in the VM. Never use the revoked
  GitHub token.
- Commit messages: subject-only, `<subsystem>: <Imperative summary,
  capitalized, 50–72 chars, no period>`. No AI trailers, no milestone
  or session narratives.
- No `let _ =` or `let _value = value`, ever. No `#[allow]` anywhere.
  No new unsafe without the owner's explicit approval.
- Never put the owner's government name in shipped docs, comments, or
  code. In shipped artifacts he is "Bonzupii" or "the owner".
- Chaining: when a step finishes and is verified, the next step starts
  immediately; do not report and stop, do not wait for a tick.
- Main chat stays responsive: long work goes to background workers.
- Rusty does not judge reconstruction quality; Claude does.

## 11. Completed work (commit references)

- `a4f4305`, `81d7293` — portal error reporting (real D-Bus error names
  instead of the constant "FDO error").
- `70e331a` — processing graph integrated into the live `run` path.
- `28d78b3`, `8de112b` — region analysis module and wiring into the
  graph and run path.
- `8fb32af` — deny `warnings` and `unused_must_use`; chain graph
  builder calls.
- `9ad6e45` — Contour strip redesign; fixed 2 GiB / 16.7M px ceiling
  deleted; configurable budget via `Contour::with_memory_budget` /
  `LimitedContour`; checked arithmetic; `Frame::filled` overflow fix.
- `ae3f759` — incremental graph execution (16x16 tiles, per-node dirty
  propagation/caching; Contour declares Global).
- `3a911a6` — post-redesign benchmark numbers in `docs/performance.md`.
- `0e53f2e` — Nix dev shell flake (superseded by the owner's own fix,
  `dcb4de9`, `3cc8ebe` — owner added zsnes for testing).
- `5465eb2` — README rewrite.
- `4b5cbb4` — Wayland: negotiate portal capabilities before starting
  capture (cursor modes, interface version, persist gate, selection
  response, Closed subscription, close-on-failure, PipeWire serial
  targeting, shared process runtime, mock-portal test).
- `4b1289a` — CLI: pass the whole image to the PNG writer at once
  (fixes every `upscale` failing at save with wrong data size).
  Verified end to end by the coordinator (2026-11-10): `pixelflow
  upscale` on `ct.png` (256x222) at 2x nearest exits 0 and writes a
  512x444 PNG.
- `6b57eb3` — Wayland: split `PipeWire` stream setup out of
  `run_loop_inner` (fixes the `too_many_lines` clippy diagnostic
  the portal fix introduced; wayland tests 25 + 1 pass under
  `dbus-run-session`).
- `431f22c`, `c1c3a88` — GPU: add `pixelflow-gpu` crate (context,
  image, backend types) and the WGSL nearest-neighbour compute node
  with parity test. Integrated and pushed by the coordinator;
  `ls-remote` verified at `c1c3a88`.
- `bab0c90`, `dc62543`, `1ba035e`, `ad8079f` — framework: stable
  algorithm contract, registry + Bilinear, CLI registry selection,
  integration guide. Integrated and pushed by the coordinator;
  `ls-remote` verified at `c6accd0`.
- `3ba4f27` — GPU: unify `ExecutionBackend` on the core contract
  type (`pixelflow-gpu` re-exports `pixelflow_core::ExecutionBackend`;
  `name()` moved to the core type).
- `c6accd0` — Contour: drop the dead "Algorithm 20" name from the
  module header (mechanism description kept).
- `30fa31a`, `34aa8b6`, `0e5c1c2`, `fc5121e`, `456e84d` — Wayland
  follow-up: Closed checks around Start + runtime-build retry,
  close on missing PipeWire fd, mock fails loudly without a bus,
  gone-session close classification + cancelled-selection
  diagnostic, gone-session mock scenario. Integrated and pushed by
  the coordinator; `ls-remote` verified at `ffff504`.
- `ffff504` — Wayland: clear the mock portal clippy errors
  structurally (`MockFaults` grouping, sync zbus methods, recorded
  call arguments, scenario helpers; the Session version property
  nobody reads is deleted). Wayland clippy clean; full suite 194
  passed / 0 failed at `ffff504`.
- `1f4ddc1`, `d2547b1`, PixelFlow: port the reference
  `fr_contour_guard` as the flagship algorithm (id `pixelflow`,
  scales 2/4); `d2547b1` replaces the soft branch with the
  reference `contour_impl`, bit-identical to the reference on all
  5 oracle scenes at 2x and 4x (max abs diff 0), flat [128,64,32]
  shift 0, release suite 254 passed / 0 failed, commit-recorded.
  Owner-authored, on main.
- `134a052`, Bicubic: port the reference kernel as a separable
  resampler (Catmull-Rom, 14-bit fixed-point weights), id
  `bicubic`, scales 2/4, bit-identical oracle, flat shift 0,
  release suite 267 passed / 0 failed, tally read from the
  owner's suite log. Owner-authored, on main.
- `e8b475f`, Lanczos3: port the reference kernel as a separable
  resampler (windowed sinc, radius 3), id `lanczos3`, scales 2/4,
  bit-identical oracle, flat shift 0, release suite 280 passed /
  0 failed. Owner-authored, on main; the driver run verified the
  gate independently (see §8).
- `91bf243`, Scale2x: port the reference stage as a stage
  algorithm (AdvMAME2x, native factor 2, 4x as two 2x passes), id
  `scale2x`, scales 2/4, bit-identical oracle, flat shift 0,
  release suite 292 passed / 0 failed, gate verified
  independently by the coordinator run (see §8).
- `951a0b2`, EPX: port the reference stage as a stage
  algorithm (Scale2x plus the three-or-more-identical-
  neighbours guard, native factor 2, 4x as two 2x passes),
  id `epx`, scales 2/4, bit-identical oracle, flat shift 0,
  release suite 306 passed / 0 failed, gate verified twice
  including on the rebased branch (see §8).

## 12. Remaining work and acceptance criteria

In execution order (§11 of the owner directive):

1. Baseline test suite — done at `4b1289a` (155 passed / 0 failed);
   re-verified at `c1c3a88` (168), at `c6accd0` (190), at `ffff504`
   (194 passed / 0 failed, fixtures exercised), at `d3c4b41` (199),
   at `aea977c` (213), at `d2e057e` (217), at `afa6f46` (226, debug),
   at `27e6113` (226, release, fixtures exercised — baseline void,
   tree did not build; see §8), at `0ba9d38` (226, release, fixtures
   exercised, gate verified independently), at `d2547b1` (254,
   release, commit-recorded), at `134a052` (267, release, tally
   from the owner's suite log), at `e8b475f` (280, release,
   fixtures exercised, gate verified independently by the driver
   run), at `91bf243` (292, release, fixtures exercised, gate
   verified independently by the coordinator run), at `951a0b2`
   (306, release, fixtures exercised, gate verified on the
   rebased branch).
2. Wayland verification report, done 2026-11-10 (all claims verified,
   mock test genuinely exercised under `dbus-run-session`, 5 gaps
   recorded). Wayland follow-up fixes — integrated at `ffff504`.
   Backend-general fixes — integrated at `d3c4b41`. Live acceptance
   on the owner's machine still open, and the failure moved: owner
   run 12:00 2026-11-10 (HEAD `6bbc989`, clean build) — window
   capture still fails at SelectSources (cancelled); monitor
   fallback starts (selection complete, PipeWire connected, node 65
   then 119, 1920x1080, "capturing monitor") but no output window
   appears and the game stays at native resolution: no frames reach
   presentation. Frame-delivery work integrated at `d2e057e`: no
   transport defect found (reproduction test passes on base code);
   the stall is Contour cost — 151.9 s/frame debug, 9.1 s/frame
   release at 1920x1080 @4x, byte-identical output. Missing
   presentation window still unexplained in-process; control run
   on the owner's machine: `pixelflow run <game> --capture monitor
   --scale 2 --algorithm nearest --windowed`, expect a window
   within ~1 s. Acceptance, each reported separately: (1)
   window capture delivers frames, or monitor fallback delivers
   frames — separately, fallback is not window capture; (2)
   transformed output visible (game upscaled, not native
   resolution); (3) repeated capture attempts work; (4) failure
   cleanup leaves no broken sessions. Session opened, PipeWire
   connected, mock tests passing, or a better error message do not
   count as success.
3. Algorithm integration framework — integrated at `c6accd0`;
   acceptance per §3. "Algorithm 20" removal — done with it.
4. GPU processing with CPU retained — crate integrated at `c1c3a88`
   (WGSL nearest node, adapter fallback chain, no silent CPU
   fallback, `ExecutionBackend` unified on the core contract);
   live-path and registry wiring done at `aea977c` (2026-11-10):
   `nearest-gpu` registered (`NearestGpuAlgorithm`, GPU-only
   descriptor, no device at registration); the live `run` path
   resolves `nearest` to `nearest-gpu` when an adapter is present
   and reports the effective backend, `nearest-gpu` with no
   adapter errors loudly naming the CPU fallback id; `upscale
   --algorithm nearest-gpu` with no adapter fails with
   "cannot run on the cpu backend". Device run still required
   before any GPU claim: no adapter on this VM, so GPU execution
   and GPU/CPU parity through the live path are unverified.
5. Wayland backend-general — done at `d3c4b41` (2026-11-10):
   source-type verification (`SourceMismatch`); monitor fallback
   per `CaptureConfig` (window failure → fresh monitor session,
   reported explicitly); fresh sessions on retry; cancelled-Start
   diagnostic for GNOME/KDE choosers; mock scenarios for each.
   Acceptance still open: real frames from Window and
   Monitor capture separately on the owner's machine.
6. Continue Contour development without sacrificing any restoration
   responsibility (§4).
7. Verify the live application path, performance, and regression
   behavior after integration.
8. Keep this document accurate as work lands. The master mandate
   (2026-11-10 11:27) stands as the full product objective — 13
   workstreams (Capture, Frame/resource architecture, Contour,
   Algorithm framework, Processing graph, Region analysis, GPU
   compute, Real-time runtime, Presentation, Offline processing,
   Performance and quality, Platform support, Documentation).
   Wayland is urgent but does not monopolize: GPU, Contour,
   framework, and offline/benchmark work advance independently.
9. Scaler ports from the reference file
   `~/workspace/user/files/pixelflow_scalers.rs` (owner decisions,
   2026-11-10; 19 IN, 6 OUT, 0 undecided; queued behind the CI fix,
   main green 2026-11-10 at `9fc552a`, CI success on `0ba9d38` and
   `9fc552a`). IN, in port order: PixelFlow (the reference's
   `fr_contour_guard`, ported under the name PixelFlow, flagship —
   first), then nearest, bilinear, bicubic, lanczos3, scale2x, epx,
   eagle, mmpx, hq_style, xbr, sabr_style, xbrz_style, aa_cut,
   labelset, Contour (old, stays Contour), layer_grain,
   edgemix_aacut, edgemix_sabr. OUT, never ported, never
   re-proposed: sharp_bilinear, scale3x, superxbr_style, dcci,
   pixelflow, directional. The reference's own `pixelflow` algo is
   OUT and is a different algorithm from the renamed ContourGuard;
   the two must not be confused. The OUT list governs selectable
   algorithms only: it does not remove the planner's internal
   sharp-bilinear fractional close step, which is pipeline
   infrastructure. Each port is verified with the full gate before
   any push, and everything is re-measured fresh per algo, no
   findings carry over from the old Contour. Status 2026-11-10:
   PixelFlow ported at `1f4ddc1`, soft branch corrected at
   `d2547b1` (bit-identical to the reference on all 5 oracle scenes
   at 2x and 4x, release suite 254 passed / 0 failed,
   commit-recorded); bicubic ported at `134a052` (bit-identical
   oracle, release suite 267 passed / 0 failed, tally read from
   the owner's suite log `cg-bicubic-suite-release.log`);
   lanczos3 ported at `e8b475f` (bit-identical oracle, release
   suite 280 passed / 0 failed); scale2x ported at `91bf243`
   (bit-identical oracle, release suite 292 passed / 0 failed);
   epx ported at `951a0b2` (bit-identical oracle, release suite
   306 passed / 0 failed). Nearest and bilinear are the
   repo's existing implementations, not reference ports. Next in
   order: eagle. Correction 2026-11-10: the sentence in the
   5240df4 record saying the owner is porting in sequence in
   `port/*` worktrees, so the driver spawns no port worker, was
   false on the record. The owner ported only PixelFlow himself,
   in the main tree (1f4ddc1, d2547b1); every reference port
   after that (bicubic, lanczos3, scale2x, epx) was done by
   coordinator and driver workers, and the port/scale2x worktree
   was created by the coordinator's worker, not by the owner.
   The coordinator and driver spawn port workers in order; they
   do not stand down on ports. The "queued behind the CI fix" line is superseded: the
   ports landed on main with no separate CI-fix commit.
10. Operation-level composition (owner direction, 2026-11-10;
    queued behind the scaler ports). This restores and extends
    the composability requirement: composition at the individual
    processing-operation level, not merely selection and chaining
    of complete, monolithic algorithms. The requirement is
    integrated in §1 (scope), §2 (operation contracts), §3
    (operation-level composition and the operation library), §4
    (Contour's constituent operations), §8 (composition tests),
    §9 (workstream), and the architecture spec (§3.1 typed
    ports, §5 recombination, §9 routing vs composition, §11
    combined system, §12 step 5, §13, §14).
    Processing steps are sequential pure functions, the output
    of one is the input of the next, and they must be
    configurable as mix-and-matchable layers: each step (edge
    detection, blending, and the rest) is a layer the user can
    configure, swap, and compose. The composition system must
    support sequential pipelines; branching (an input or
    intermediate result feeding multiple independent branches);
    recombination (branch outputs blended, composited, or
    otherwise combined); layering (independently processed image
    layers combined into a final output); additive processing
    (effects and transformations contributing to a shared result
    without one monolithic algorithm owning the pipeline); masks
    and conditional application (operations affecting selected
    regions, features, or layers); region-dependent composition
    (different regions following different pipelines and
    subsequently recombining); parameterization (every
    configurable operation exposing its parameters
    independently, without source modification or a fixed
    collection of hard-coded combinations); and reusable pipeline
    definitions and profiles (saved, loaded, modified, shared).
    Operations are selectable, reorderable where their
    input/output requirements permit, parameterized, combined,
    replaced, and reused. The graphical interface must
    eventually expose all of it: select, add, remove, and
    configure operations; arrange sequential operations; create
    branches; add and configure layers; select blending and
    compositing operations; connect intermediate outputs and
    inputs; apply masks and region-specific processing; save,
    reload, and modify compositions; start from a complete
    algorithm preset and customize its components. Sequencing:
    mandate §4 states a graphical node editor is not required,
    and that the graph is an execution architecture first with a
    user interface considered independently later. GUI exposure
    of composition is that later step, a Stage VII capability.
    Region routing does not satisfy operation-level mix-and-match; a
    test-only blend node does not satisfy production layering;
    complete algorithm presets do not satisfy independent
    configurability. These are separate capabilities with
    separate status.
    Status, honestly: the graph machinery for linear chains,
    branches, and multi-input nodes exists, with topological
    execution and incremental execution with per-node tile
    caching. Not implemented: typed intermediate ports (the
    `Node` interface is frame-in/frame-out only); a production
    blend, layer, or composite node (the only blend node in the
    repo is `BlendNode` in `pixelflow-core` tests); the
    operation library; pipeline definitions and profiles
    (Stage VII); any graphical interface (the CLI is the only
    interface). PixelFlow in the reference is already four
    separate pure functions run sequentially (AaCut crisp
    reconstruction, Contour soft reconstruction, guard-weight
    dither field, blend); Contour's blending is inlined inside
    its implementation, not a separate function, factoring it
    out is part of this work if Contour's steps are to be
    mixable. The graph executor runs nodes sequentially in
    topological order, but the Node trait takes `&mut self` and
    permits stateful history, so purity is not guaranteed by the
    interface, the layer design must use pure frame-in/frame-out
    functions, and GPU execution must support the same
    compositional model as CPU execution. No configuration
    format is invented here: build the layer mechanism, and the
    configuration surface follows the owner's call.
