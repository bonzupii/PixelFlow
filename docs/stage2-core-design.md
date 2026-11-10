# Stage II core design: the frame model

Status: implemented in this step, except where the text says
otherwise. This document settles the frame model the rest of
Stage II builds on: ownership, layout, color semantics,
metadata, and the CPU/GPU boundary. The graph representation,
the node interface with dependency declarations, and the
configuration model are separate Stage II work and are listed
at the end, not settled here.

## 1. Pixel ownership

Frames own their bytes. A `Frame` holds a `Vec<u8>`; ownership
moves with the frame through the pipeline — capture produces
it, the processing thread takes it by value, the presentation
thread takes the processed frame by value. Nobody borrows pixel
data across a subsystem boundary, so no subsystem can observe
another subsystem mutating bytes under it, and the bounded
1-frame channel between processing and presentation stays a
plain move.

Cloning a frame is an explicit deep copy. The hot path must
not clone: capture hands the buffer to processing, processing
hands the output to presentation, and the only copy on that
path is the one a tight-only algorithm makes when it packs a
strided input (section 6).

Buffer pooling is not implemented in this step, and the model
must not need a redesign to get it. The model allows it:
allocation goes through `Vec<u8>` handed to `Frame::new` or
`Frame::new_with_stride`, so a future pool hands out a reused
`Vec` and the frame takes it over. Pooling is Stage IV work;
it is enabled here, not built here.

## 2. Memory layout: stride

A frame has a row stride in bytes: the distance between the
start of one row and the start of the next. The invariants,
checked in the constructors with checked arithmetic:

- `stride >= width * bytes_per_pixel`. A stride smaller than
  one row is a corrupt frame, never a truncated read.
- `data.len() == height * stride` exactly. The buffer holds
  every row the stride implies, padding included. A short or
  long buffer is an error.
- Tight packing is the special case `stride == width *
  bytes_per_pixel`. Nothing in the model treats tight frames
  as a different kind of object.

`Frame::new(width, height, format, data)` keeps its existing
shape and builds a tight frame. `Frame::new_with_stride(width,
height, format, stride, data)` builds a strided one. Both
validate and return `FrameError` on mismatch or overflow.

Accessors honor stride: `pixel(x, y)` and `pixel_mut(x, y)`
index through `y * stride + x * bytes_per_pixel`, and
`row(y)` returns the `width * bytes_per_pixel` pixel bytes of
one row, padding excluded. Code that reads `frame.data`
directly as if it were tight is a bug after this step; the
accessors are the way to read pixels, and the call sites that
read the buffer flat were migrated to them.

Why stride lives on the frame instead of only at the capture
boundary: the Wayland converter used to compact every buffer
to tight RGB8 on the way in, destroying the layout the
compositor produced. Keeping the stride means capture does
one pass over the pixels (converting channel order and
dropping alpha in place per row) instead of compacting and
then having the first tight-only algorithm copy the rows
again. Stride-aware consumers — nearest-neighbour, the
texture upload, `to_rgb8` — read strided frames directly and
never pay for a pack.

Why stride is in bytes, not pixels: backends report strides
in bytes (PipeWire chunk stride), and padding need not be a
whole number of pixels. A byte stride is the unit the
producers speak.

## 3. Color semantics

The formats are unchanged: `Rgb8`, `Rgba8`, `Gray8`. No new
format is added in this step — no YUV, no 16-bit, no planar.
Capture negotiates packed RGB formats only, and processing
math is RGB8-only. A format with no producer and no consumer
is speculation, and the format set grows when a backend or an
algorithm needs it, not before.

`Rgb8` stays the processing format. `Frame::to_rgb8` is still
the single place that converts other formats to RGB8, and it
still drops alpha and expands grey there. That is a deliberate
loss, stated plainly: Contour's math is RGB-only, and alpha
does not survive processing. The synthetic tests pin it
(RGBA in, RGB out, alpha gone). Alpha-aware processing would
be a new capability with its own design, not a side effect of
this step.

## 4. Metadata

A frame carries two metadata fields, both public like the
other fields:

- `timestamp: std::time::Instant` — when the frame was
  captured. `Instant` is monotonic, which is what frame
  pacing needs: the presentation side measures inter-frame
  intervals and lateness against it. Wall-clock time would
  be the wrong clock; it jumps.
- `sequence: u64` — the capture sequence number, assigned by
  the capture backend, monotonically increasing per captured
  frame. The section 9 dirty-region work keys its
  per-frame bookkeeping on it.

Constructors default to `Instant::now()` and sequence 0, so
every existing call site keeps working and test frames need
no clock. `with_timestamp` and `with_sequence` are consuming
setters for producers that know the real values. Capture
stamps both: the PipeWire loop thread stamps
`Instant::now()` when it dequeues a buffer and takes the
next number from a dedicated sequence counter. Processors
propagate both fields from input to output — the processed
frame shows the same captured moment, so pacing and
dirty-region bookkeeping downstream still refer to the
capture, not to the processing.

Whole-frame equality includes timestamp and sequence. That
is by design: two frames captured at different moments are
not the same frame. Tests compare pixels, dimensions, and
formats, not whole frames.

## 5. Intermediate resources and the CPU/GPU boundary

`Frame` is CPU-accessible data: its bytes live in a `Vec<u8>`
the CPU can read. That is all of the frame model this step.
The model must distinguish CPU-accessible data from
GPU-backed resources without forcing every operation through
a CPU byte buffer, and it does so by giving the GPU side a
name and a boundary rule, not an implementation:

- `GpuResource` is declared in core as an opaque handle to a
  GPU-backed image resource. It has no public constructor:
  the first backend that produces GPU resources adds the
  constructor alongside its allocation code. Fabricating a
  handle from nothing would be worse than having none.
- The rule: a GPU-backed resource travels as `GpuResource`
  and is never smuggled inside a `Frame`'s data buffer. A
  later step introduces a `FrameData` enum so a frame can be
  CPU-backed (`Vec<u8>`) or GPU-backed (`GpuResource`), and
  the graph executor routes each frame only to nodes that
  accept its backing. Until that step exists, every frame is
  CPU-backed, and every node reads CPU bytes.

Why not the enum now: no producer makes GPU resources and
no node consumes them, so an enum would churn every call
site for zero consumers. The declared type plus the routing
rule is the boundary; the enum is the step that first needs
it.

## 6. Stride support: nodes declare, the pack happens at the boundary

The key interface question: nodes that assume tight packing —
Contour's kernel indexes slices directly — do they declare
stride support and get packed input from the executor, or
must every node handle stride?

Nodes declare. `Processor::stride_support()` returns
`StrideSupport::Any` (the default: the node reads through
the stride-aware accessors) or `StrideSupport::TightOnly`
(the node indexes a tight buffer). `Frame::packed()` returns
a `Cow<Frame>`: the frame itself when it is already tight,
a tight copy otherwise. A tight-only node packs at its own
entry point, using its declaration and the helper.

Reasons, in order:

- Contour's kernel is a transliteration with pinned float
  behavior, including the exact noise sign of the hardcoded
  Savitzky-Golay tables. Rewriting its indexing to be
  stride-aware would touch every direct index in 1,755
  lines and risk the pinned output (the Mario 34/6
  regression) for no behavior gain. The pack is one pass
  over the input rows; Contour's own cost dominates it by
  orders of magnitude.
- Forcing every node to handle stride would make the
  declaration meaningless and push the same indexing risk
  onto every future algorithm. The nodes that are naturally
  stride-aware (nearest-neighbour, texture upload) handle
  stride; the one that is not declares so.
- The declaration is the stable part. There is no graph
  executor yet, so the pack currently happens at the
  tight-only node's entry. When the executor exists, the
  pack moves out of the node and into the graph — the
  executor reads `stride_support()` and inserts a pack node
  before a tight-only node fed with a strided frame — and
  the node itself does not change. The declaration is
  written so that move is mechanical.

The output side is symmetric: processors produce tight
frames. A processor that wants to produce strided output
would need a reason stated at the time; none has one.

## 7. Migration in this step

- `pixelflow-core`: `Frame` gains `stride`, `timestamp`,
  and `sequence`; `Frame::new` keeps its shape and builds
  tight frames; `Frame::new_with_stride` builds strided
  ones; both validate with checked arithmetic.
  `pixel_offset`, `pixel`, `pixel_mut` honor stride;
  `row` and `row_mut` are new. `to_rgb8` iterates rows
  instead of flat chunks. `Frame::packed` returns a tight
  copy as `Cow`, borrowing when already tight.
  `Processor::stride_support` and `StrideSupport` are new
  in `pipeline.rs`. `GpuResource` is declared in core.
- `pixelflow-processing`: Contour declares `TightOnly` and
  packs at entry; its output frame carries the input
  timestamp and sequence. Nearest reads through
  `pixel()` and stays stride-aware; its output likewise
  propagates metadata.
- `pixelflow-wayland`: `packed_to_frame` builds a strided
  RGB8 frame whose stride is the PipeWire chunk stride,
  writing RGB pixels at each row start and leaving the
  row padding zeroed. The PipeWire loop stamps timestamp
  and sequence on each delivered frame.
- `pixelflow-present`: `rgba8_from_rgb8` iterates rows
  instead of flat chunks. The texture upload is unchanged:
  the RGBA vector it receives is tight.
- `pixelflow-cli`: `save_png` writes row by row through
  `row()`, so a strided frame saves correctly. `load_png`
  already returns tight frames and is unchanged.

## 8. What Stage II still owes after this step

- The graph representation: nodes, edges, and execution
  order. DONE in the graph step (`pixelflow-core/src/graph.rs`):
  the `Node` trait, `ProcessorNode` wrapping `Processor`,
  `GraphBuilder` validation (ports exist, every input fed
  exactly once, formats agree, no cycles), and a whole-frame
  executor that packs strided input for tight-only nodes and
  drops intermediate frames when their last consumer runs.
  The executor's execution path is the place dirty-region
  tracking hooks in.
- The node interface with dependency declarations
  (local / neighborhood / global / temporal), per the
  section 9 spec. DONE with the graph: `SpatialDependency`
  and `TemporalDependency` on the node trait. Nearest declares
  `Local`. Contour declares `Global`, not `Neighborhood`:
  its labelset reads a 3x3 neighborhood, but the edge
  follower's runs and gates span whole scanlines, so no small
  radius bounds the input area one output pixel depends on.
  The incremental step derives Contour's actual invalidation
  requirements before it trusts anything tighter; until then
  the honest declaration is global.
- The configuration model: stable algorithm and filter
  identifiers, parameter validation, versioning, and
  save/load of processing graphs. The CLI's `--algorithm`
  string is not that model.
- The `FrameData` enum (CPU vs GPU backing) and the
  executor routing that uses it, when the first GPU-backed
  producer exists.
- Buffer pooling for the capture-to-presentation path
  (Stage IV performance work, enabled by section 1).
