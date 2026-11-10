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

Repository: `bonzupii/PixelFlow`, branch `main`. HEAD `4b1289a`
(2026-11-10). Six crates, Rust 1.99.1, edition 2024:

| Crate | Contents | Status |
| --- | --- | --- |
| `pixelflow-core` | `Frame` (private fields, stride-aware, timestamp/sequence), `PixelFormat` (Rgb8, Rgba8, Gray8), `Processor` trait, `Node` trait, `Graph`/`GraphBuilder`, incremental execution (16x16 tiles, dirty propagation, per-node tile caching) | Implemented |
| `pixelflow-processing` | Algorithms `Contour`, `Nearest`; region analysis module (six classes, confidence scores) wired into the graph and the live run path | Implemented; no algorithm registry yet (framework work in flight) |
| `pixelflow-capture` | Capture backend abstraction (`SourceKind`: window/monitor) | Implemented |
| `pixelflow-wayland` | Portal ScreenCast session, PipeWire frames, format conversion | Implemented; live acceptance unverified (see §7) |
| `pixelflow-present` | wgpu presentation window, aspect-ratio-preserving layout, frame slot with latest-frame-wins, event-loop wake-up via event proxy | Implemented; no GPU compute (display only) |
| `pixelflow-cli` | `upscale` (PNG in/out), `run` (launch, capture, process, present), `backends`, `info`, `bench` | Implemented; algorithm selection hardcoded (framework work in flight) |

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
  backend). No GPU compute path exists.

## 3. Algorithm integration framework

Status: in flight (workstream, see §9). The framework must provide:

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

## 4. Contour — unfinished

Contour is the flagship algorithm and it is unfinished. It still misses
the desired result. Do not declare it complete and do not treat recent
improvements as proof it is finished.

Contour is a complete multi-stage restoration system. Dithering
enhancement is one feature, independently controllable, not the
algorithm's purpose, and never a justification for sacrificing the rest.
Do not simplify Contour into nearest-neighbour plus arcs, strokes, or a
dither filter. Do not remove existing reconstruction stages. Nearest
neighbour remains a separate baseline algorithm.

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
- No GPU compute exists yet. The presenter uses wgpu for display only.
  This VM has no wgpu adapter, so GPU execution cannot be verified
  here; a device run on hardware with an adapter is required before any
  GPU claim is made.

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
  ScreenCast backend are still unknown.
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
  cancelled the request. Open question: whether the picker dialog
  appeared on his screen — that distinguishes the two causes and
  cannot be answered from this VM. The close-after-failure
  UnknownMethod is expected cleanup, not a second failure: the portal
  destroyed the session object when the request was cancelled, so
  `Close` on that path answers "object does not exist". Fix assigned
  to the Wayland follow-up workstream: tolerant close-after-failure
  classification, a factual cancelled-selection diagnostic naming the
  two portal-side causes, and a mock scenario for a gone session on
  close. No retry and no second SelectSources attempt: a cancelled
  selection is a terminal failure for that run.
- Portal-fix gaps found by verification (none fail the claimed fix;
  follow-up work assigned): (1) Closed detection is a single
  non-blocking poll after selection and in the Start error branch —
  a Close arriving in the race window is detected via the Start
  error, not prevented; no Closed check after Start succeeds before
  `open_pipe_wire_remote`; a `receive_closed` subscription failure
  logs and continues with detection off. (2) Latent unclosed path in
  `capture.rs:51-55` if `take_fd()` returned `None` — practically
  unreachable (`open` always sets the fd), no `Drop` close by design.
  (3) Serial targeting usually will not engage against a real portal:
  ashpd 0.14 parses only the `pipewire_serial` (underscore) key while
  the portal specification wires `pipewire-serial` (hyphen), so the
  node-id fallback is the live path. (4) A `PORTAL_RUNTIME` build
  failure is cached forever with no retry. (5) The mock-portal test
  silently skips without a session bus — a plain `cargo test` run
  reports a green mock test that exercised nothing; the gate must run
  it under `dbus-run-session` or the test must fail loudly.
- GPU execution: UNVERIFIED and unimplemented. No adapter on this VM.
- Previous coordinator `338e91a0` (spawned 2026-11-10 07:22) is
  superseded by the current coordinator. It landed the portal fix and
  the PNG writer fix, then went quiet; it cannot be closed from the
  coordinator's scope and must be closed by the main agent. Its tree
  state was clean at handover; no uncommitted work was lost. A release
  test run (`cargo test --release`, separate target dir) observed in
  flight at 16:27 UTC may belong to its subtree.
- "Algorithm 20" removal from user-facing output: owed, assigned to the
  framework workstream (CLI `backends` line). Internal fixture files
  (`*_20_*`) and test identifiers are not renamed.
- `docs/architecture.md` says Frame's fields are "public" — stale since
  encapsulation; unfixed.
- Per-stage Contour timing is unmeasured (the bench has no per-stage
  support); allocation profiling with actual tools is still outstanding.
- Live `--help` execution has not been run on this VM since the dev
  libraries were restored.

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

## 9. Active workstreams and ownership

Coordinator: the current coordinator session (replacing `338e91a0`).
Owns project state, architecture decisions, task assignment,
integration, commits, and final verification. Only the coordinator
integrates worktree work onto `main`.

| Workstream | Owner | Scope | State |
| --- | --- | --- | --- |
| Algorithm framework | framework worker, worktree `~/workspace/pixelflow-framework`, branch `framework/algorithm-registry` | Contract, registry, graph/CLI integration, test algorithm, "algorithm 20" removal, `docs/algorithm-integration.md` | Running |
| GPU execution | GPU worker, worktree `~/workspace/pixelflow-gpu`, branch `gpu/compute-path` | `pixelflow-gpu` crate, adapter fallback chain, GPU-resident images, WGSL nearest-neighbour compute node with CPU parity | Running |
| Wayland verification | verification worker, main tree read-only | Verify commit `4b5cbb4` claim-by-claim, run `cargo test -p pixelflow-wayland` | Done 2026-11-10: all 7 claims verified in code; `cargo test -p pixelflow-wayland` exit 0 (25 unit + 1 mock); mock genuinely exercised under `dbus-run-session`; 5 gaps recorded in §7 |
| Wayland follow-up | to be spawned, worktree `~/workspace/pixelflow-wayland-fixes`, branch `wayland/closed-detection-fixes` | Fix verification gaps 1, 2, 4, 5; assess gap 3 (serial vs hyphen key) | Not started |
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

## 12. Remaining work and acceptance criteria

In execution order (§11 of the owner directive):

1. Baseline test suite at `4b1289a` — running; record result.
2. Wayland verification report — done 2026-11-10 (all claims verified,
   mock test genuinely exercised under `dbus-run-session`, 5 gaps
   recorded); live acceptance on the owner's machine still open.
   Wayland follow-up fixes (gaps 1, 2, 4, 5; assess 3) — to be spawned.
3. Algorithm integration framework — in flight; acceptance per §3.
4. GPU processing with CPU retained — in flight; acceptance: real wgpu
   compute node through the graph, pixel-identical to CPU nearest,
   adapter fallback chain implemented, no silent CPU fallback, device
   run required before any GPU claim.
5. "Algorithm 20" removal from user-facing output — assigned to the
   framework workstream.
6. Continue Contour development without sacrificing any restoration
   responsibility (§4).
7. Verify the live application path, performance, and regression
   behavior after integration.
8. Keep this document accurate as work lands.
