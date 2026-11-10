# Algorithm integration

How upscaling algorithms plug into PixelFlow: the contract, the
registry, configuration and validation, the graph, and the CLI.
A developer who follows this document can add an algorithm
without asking anyone, and without touching capture,
presentation, or CLI code.

## The contract

The contract lives in `pixelflow-core`, in
`crates/pixelflow-core/src/algorithm.rs` (descriptions,
configuration, factories, errors) and
`crates/pixelflow-core/src/pipeline.rs` (the `Processor` trait).
Core defines it; `pixelflow-processing` implements it; the CLI
and the graph program against it. No other crate may invent its
own notion of what an algorithm is.

An algorithm has two halves:

- **Execution**: the `Processor` trait. One frame in, one frame
  out, an integer scale factor: `process(&self, frame, scale)`.
  The input frame's format must be one the algorithm declares;
  the output is the declared output format at `width * scale`
  by `height * scale`.
- **Instantiation**: the `AlgorithmFactory` trait. One value per
  algorithm, which describes the algorithm, validates a
  configuration, and builds processors and graph nodes from it.

### What `Processor` declares

Every method below has a default, so an existing processor
keeps working when the contract grows; an algorithm overrides
only what differs from the default.

| Method | Default | Meaning |
| --- | --- | --- |
| `name()` | — (required) | Short stable name, e.g. `"bilinear"`. |
| `algorithm_id()` | `name()` | Stable id: the registry key and the CLI name. |
| `description()` | `""` | One sentence saying what the algorithm does. Shown by the CLI. |
| `version()` | crate version | Algorithm version. |
| `input_formats()` / `output_formats()` | `[Rgb8]` | Pixel formats in and out. |
| `supported_scales()` | — (required) | Integer scale factors, e.g. `[2, 4]`. |
| `backends()` | `[Cpu]` | Execution backends the algorithm can run on (`Cpu`, `Gpu`). |
| `spatial_dependency()` | `Global` | What input area one output pixel depends on: `Local`, `Neighborhood { radius }`, or `Global`. Reuses the graph's `SpatialDependency`. |
| `temporal_dependency()` | `Stateless` | What history the algorithm keeps between frames. |
| `stride_support()` | `Any` | `Any` or `TightOnly` (see `Frame::packed`). |
| `tile_safe()` | `false` | Whether a cropped run produces exactly the corresponding crop of the whole-frame output. Override only when the cropped run is exact. |
| `parameters()` | `[]` | Validated algorithm-specific parameters (below). |
| `working_bytes_per_output_pixel()` | `None` | Working memory per output pixel, when the working set is a flat per-pixel figure. `None` when the working set has no such figure; the algorithm's own budget check states its model instead. |
| `intermediate_resources()` | `[]` | Intermediate resources a multi-stage algorithm holds between its stages, named so a caller can account for them. |
| `deterministic()` / `determinism_note()` | `true` / fixed note | Whether the same input, scale, and configuration always produce byte-identical output, and what that means for this algorithm. |
| `stage_names()` | `[]` | Graph node stage names, in run order, for a multi-stage algorithm. Empty means single-stage, named after the algorithm id. |
| `validate_config()` | validates against `parameters()` | Rejects an unknown parameter, a wrongly typed value, or a value outside its range. |

The honest default for `spatial_dependency()` is `Global`:
an algorithm that has not stated a tighter dependency claims
the whole frame. The graph executor and the incremental step
trust the declaration, so a wrong one is a bug, not an
optimization.

### The descriptor

`AlgorithmDescriptor` is the static description of one
algorithm: every field above, collected into one struct. It is
built from a processor by
`AlgorithmDescriptor::from_processor(&processor, origin)`,
which reads the processor's contract methods, so the
description and the implementation cannot drift apart. The
descriptor also carries:

- `origin`: `BuiltIn` or `External`.
- `stages`: one name per graph node the factory builds, in run
  order. A single-stage algorithm lists its own id.
- `supports_scale(scale)`, `supports_input_format(format)`,
  `output_dimensions(width, height, scale)` (checked
  multiplication, `None` on an unsupported scale or overflow),
  and `estimated_working_bytes(output_pixels)` (the flat
  per-pixel figure times the pixel count, or `None`).

### Multi-stage algorithms

Nothing forces a complex algorithm into a single `process()`
call. A multi-stage algorithm:

1. Declares its stages in `stage_names()` (one name per node,
   in run order) and its intermediates in
   `intermediate_resources()`.
2. Overrides `AlgorithmFactory::create_nodes()` to build one
   node per stage. The default builds one `ProcessorNode` from
   the processor `create()` returns.

The registry wires the stages linearly — graph input into the
first stage, each stage's output into the next stage's input,
the last stage's output as the graph output. An algorithm whose
stages do not wire linearly builds its own graph from
`create_nodes()` and the `GraphBuilder` directly; the registry
graph is the common case, not a constraint.

### Execution backends

`ExecutionBackend` has two variants: `Cpu` and `Gpu`. The CPU
side exists. The GPU side exists as one real node:
`pixelflow-gpu` provides `GpuNearestNode`, a WGSL
nearest-neighbour compute node registered as `nearest-gpu`
(`aea977c`), and the live `run` path resolves `nearest` to it
when an adapter is present. The WGSL shader is naga-validated,
but no GPU compute has run against a device yet: this VM has
no wgpu adapter, so the parity check has never executed, and a
device run on hardware with an adapter is required before any
GPU claim is made. An algorithm lists every backend it can run
on; a caller that asks for a backend the algorithm does not
list gets `AlgorithmError::UnsupportedBackend`.

### Errors

Execution failures use `ProcessError` (`UnsupportedScale`,
`UnsupportedFormat`, `Internal`, `MemoryBudgetExceeded`,
`SizeOverflow`). Discovery, validation, and instantiation
failures use `AlgorithmError`:

- `UnknownAlgorithm { id, available }` — the lookup failed;
  the error carries the registered ids so the caller can report
  them.
- `UnsupportedScale { algorithm, scale, supported }`.
- `UnsupportedBackend { algorithm, backend }`.
- `InvalidConfig { algorithm, parameter, message }`.
- `GraphBuild(String)` — the wired graph failed validation.

Memory budgets are caller-set, never a fixed product ceiling.
Contour's budget is the model: the `memory_budget_bytes`
parameter (zero means no budget) is checked before any
allocation for the frame, against the working-set model in
`Contour::check_memory_budget`, and a frame whose estimate
exceeds it fails with `ProcessError::MemoryBudgetExceeded`.
The budget is explicit per instantiation; there is no product
limit derived from the machine the benchmarks happened to run
on.

## The registry

The registry lives in `pixelflow-processing`,
`crates/pixelflow-processing/src/registry.rs`. Core defines the
contract; processing knows the algorithms, so the registry
lives there.

```rust
use pixelflow_processing::{AlgorithmConfig, AlgorithmRegistry};

let registry = AlgorithmRegistry::builtin();
```

- `AlgorithmRegistry::new()` — an empty registry.
- `AlgorithmRegistry::builtin()` — Contour, Nearest,
  Bilinear, and PixelFlow registered, in that order.
- `register(Box<dyn AlgorithmFactory>)` — register one
  algorithm. Registering an id that is already registered
  replaces the earlier entry, so an external algorithm can
  supersede a built-in under its own id.
- `ids()` / `descriptors()` — the registered algorithms, in
  registration order.
- `lookup(id)` / `descriptor(id)` — one algorithm, or
  `UnknownAlgorithm` listing the registered ids.
- `create_processor(id, config)` — a processor from a validated
  configuration.
- `create_nodes(id, config, scale)` — the graph nodes, in run
  order, one per declared stage.
- `build_graph(id, config, scale)` — a processing graph: graph
  input `"in"` feeding the stages in order, the last stage's
  output exposed as graph output `"out"`. The graph is the
  normal graph — it validates ports, formats, and cycles like
  any other — and runs through the normal executor. A caller
  that routes branches builds its own graph from
  `create_nodes()` instead; see the live `run` path below.

The integration boundary is source-level: a Rust crate
interface. There is no dynamic plugin ABI. Algorithms that
arrive as portable source register themselves the same way a
built-in does.

## Configuration and validation

Configuration is `AlgorithmConfig`: parameter names to
`ConfigValue`s (`Integer(i64)`, `Float(f64)`, `Boolean(bool)`,
`Text(String)`). A parameter is declared by a
`ParameterDescriptor`: name, description, kind, and default.

```rust
use pixelflow_core::{AlgorithmConfig, ConfigValue, ParameterDescriptor, ParameterKind};

static PARAMETERS: &[ParameterDescriptor] = &[ParameterDescriptor {
    name: "memory_budget_bytes",
    description: "working-memory budget in bytes; 0 means no budget",
    kind: ParameterKind::Integer { min: 0, max: i64::MAX },
    default: ConfigValue::Integer(0),
}];
```

`ParameterKind` is `Integer { min, max }`, `Float { min, max }`,
`Boolean`, or `Choice(&[&str])`. Validation
(`validate_config`, or the default `Processor::validate_config`)
rejects:

- an unknown parameter name,
- a value of the wrong type for the parameter,
- a value outside the parameter's range.

Parameters the configuration omits take their defaults. A
factory validates before it builds: `create()` calls
`validate_config` first, so an invalid configuration fails
before any processor exists.

The CLI reads `PIXELFLOW_MEMORY_BUDGET` from the environment
and sets `memory_budget_bytes` only when the selected
algorithm declares that parameter, so an algorithm without it
never sees an unknown key.

## The graph

Every registered algorithm runs through the normal graph.
Two paths exist, both through the registry's node factory:

- `registry.build_graph(id, config, scale)` — the plain graph,
  for single-path use (upscale, benchmarks).
- `registry.create_nodes(id, config, scale)` plus a
  `GraphBuilder` — for callers that route branches, like the
  live `run` path, which wires the stages linearly, routes the
  `text` and `sprite` branches and the general path to the
  first stage, and exposes the last stage as `"out"`.

There is no separate execution path that only works in a
benchmark or a demo. The `ProcessorNode` wrapper carries the
algorithm's declared spatial dependency into the graph, and
the executor packs strided frames for `TightOnly` nodes.

## The CLI

The CLI programs against the registry, never against a
per-algorithm match. `upscale`, `run`, `backends`, `info`, and
`algorithms` all read from `AlgorithmRegistry::builtin()`.

- `pixelflow upscale in.png out.png --scale 2 --algorithm bilinear`
  — builds the processor from the registry and runs it.
- `pixelflow run <program> --algorithm nearest` — builds the
  live graph from the registry's nodes. The default algorithm
  is `contour`.
- `pixelflow backends` — lists capture, presentation, and
  processing backends; the processing list comes from the
  registry, one line per algorithm with its scales and
  description.
- `pixelflow info` — lists the registered algorithm ids.
- `pixelflow algorithms` — lists every registered algorithm
  with its scales and description.
- `pixelflow algorithms contour` — prints one algorithm's full
  capabilities: description, version, origin, formats, scales,
  backends, dependencies, stride support, tile safety, stages,
  working memory, intermediate resources, determinism, and
  parameters.

An unknown algorithm or an unsupported scale fails with the
registry's available list:

```text
pixelflow: unknown algorithm "nope"; available: contour, nearest, bilinear
pixelflow: scale 3 is not supported by nearest; supported scales: [2, 4]
```

## Adding an algorithm: worked example

Bilinear interpolation is the worked example. It lives in one
module, `crates/pixelflow-processing/src/algorithms/bilinear.rs`,
plus one registration line. Follow it step by step.

### 1. Implement `Processor`

```rust
use pixelflow_core::graph::{ProcessorNode, SpatialDependency};
use pixelflow_core::{Frame, PixelFormat, ProcessError, Processor};

pub struct Bilinear;

impl Processor for Bilinear {
    fn name(&self) -> &'static str {
        "bilinear"
    }

    fn description(&self) -> &'static str {
        "bilinear interpolation: each output pixel is a weighted average of the four nearest input pixels"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Neighborhood { radius: 1 }
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        Some(3)
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "bilinear",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "bilinear",
                format: frame.format().to_string(),
            });
        }
        // ... compute the output frame at width * scale by height * scale,
        // propagate the input timestamp and sequence to the output ...
    }
}
```

Notes on the implementation, all of which the Bilinear code
shows concretely:

- Validate the scale and the format first, with the matching
  `ProcessError`. Never silently accept a format the algorithm
  does not declare.
- Size the output with checked arithmetic and fail with
  `ProcessError::Internal` on overflow; never truncate.
- Read pixels through `frame.pixel(x, y)` / `frame.row(y)`,
  which honor the stride. A strided input must produce the
  same output as the tight input.
- Propagate `frame.timestamp()` and `frame.sequence()` to the
  output with `with_timestamp` / `with_sequence`.
- State the spatial dependency honestly. Bilinear reads the
  four input pixels around each sampling position, so it
  declares `Neighborhood { radius: 1 }`. Its edge handling
  clamps to the frame boundary, so it does *not* claim
  `tile_safe`: a cropped run would clamp to the crop boundary
  instead.
- Bilinear uses fixed-point integer arithmetic for its
  weights, so its output is bit-for-bit reproducible. An
  output pixel samples the input at `(ox + 0.5) / scale - 0.5`;
  the sampling position is kept in units of `1 / (2 * scale)`,
  which makes the floor and the fraction exact integers.

The `node()` constructor wraps the processor as a graph node
for direct use:

```rust
impl Bilinear {
    #[must_use]
    pub fn node(scale: u32) -> ProcessorNode<Self> {
        ProcessorNode::new(Self, scale, SpatialDependency::Neighborhood { radius: 1 })
    }
}
```

### 2. Implement `AlgorithmFactory`

```rust
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin,
};

pub struct BilinearAlgorithm;

impl AlgorithmFactory for BilinearAlgorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        let mut descriptor =
            AlgorithmDescriptor::from_processor(&Bilinear, AlgorithmOrigin::BuiltIn);
        descriptor.display_name = "Bilinear";
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        Bilinear.validate_config(config)?;
        Ok(Box::new(Bilinear))
    }
}
```

Validate before building. When the algorithm takes
parameters, `create()` reads them from the validated config
and builds the configured processor — Contour does this with
`memory_budget_bytes`, building a `LimitedContour` when the
budget is positive. `create_nodes()` needs no override for a
single-stage algorithm: the default builds one
`ProcessorNode` from `create()`'s processor, with the spatial
dependency the processor declares.

### 3. Register it

One line in `AlgorithmRegistry::builtin()`
(`crates/pixelflow-processing/src/registry.rs`):

```rust
registry.register(Box::new(BilinearAlgorithm));
```

And the module line in `algorithms.rs`, plus the re-export in
`crates/pixelflow-processing/src/lib.rs`. That is the whole
integration. Capture, presentation, and CLI code do not
change: `pixelflow upscale --algorithm bilinear`,
`pixelflow algorithms bilinear`, and `pixelflow run --algorithm bilinear`
work immediately, and the live graph runs the new algorithm's
nodes.

### 4. Test it

- Unit tests in the algorithm module: exact output on a flat
  frame, exact interpolated values on a small gradient (for
  Bilinear: a 2x1 black-to-white frame at 2x yields
  `[0, 64, 191, 255]`), strided input matching tight input,
  unsupported scale and format rejected.
- Registry tests in
  `crates/pixelflow-processing/tests/registry.rs`: the
  algorithm appears in `ids()` and `descriptors()`, its
  descriptor states its contract, its processor and its
  registry graph produce identical output, and its
  configuration validates and rejects as specified.
- The regression suite pins Contour's numeric output; a new
  algorithm does not touch it.

## Rules the framework does not bend

- No dynamic plugin ABI. The boundary is source-level; a new
  algorithm arrives as source and registers.
- No per-algorithm match arms in the application. If adding an
  algorithm requires editing the CLI, the graph wiring, or
  capture/presentation code, the framework is incomplete.
- No fixed product memory ceiling. Budgets are caller-set
  parameters, enforced by the algorithm's own check.
- Do not rename or remove Contour's stages, and do not change
  its numeric output. Contour is unfinished; the framework
  wraps it exactly as it is.
- Report measurements only (timings, diffs, counts). Never a
  quality verdict; that judgement belongs to the owner.
