# Incremental, Region-Aware Processing Architecture

Specification for the PixelFlow coding agent. Implements mandate §9:
frame differencing as a performance capability and graphic-element
detection as a quality capability, built into the processing
architecture rather than bolted onto any single algorithm.

Status: specification only. None of this is implemented yet. The
processing graph it describes does not exist yet either — this spec
is written against the Stage II graph design, and the graph work
must carry the interfaces defined here from the start.

## 1. Purpose and scope

Two problems, one architecture:

- **Performance.** A captured frame usually changes in only a few
  places between frames. Reprocessing the whole frame every frame
  wastes the budget that real-time operation needs.
- **Quality.** Different kinds of visual content want different
  processing. Text wants its strokes preserved; a flat background
  wants its solid color kept; a gradient wants no banding. One
  algorithm applied uniformly cannot do all of that well.

The architecture answers both with the same machinery: the frame is
divided into tiles, each processing node declares what input area
its output depends on, dirty tiles propagate through the graph with
their dependency radii applied, intermediate tile results are
cached, and a separate region-analysis stage classifies what kind
of content each tile holds so the graph can route tiles through
different processing.

Non-goals: no machine-learning classifier, no per-asset manual
tagging, no full video codec, no motion estimation. Detection is
deterministic image analysis. Routing is metadata-driven, not a
second rendering engine.

## 2. Where it fits

- **Stage II (core architecture redesign).** The graph, the node
  interface, and the frame model carry the dependency declarations
  and the tile cache from the first version. Retrofitting the
  dependency declaration after a collection of processors exists
  is explicitly out of scope as rework — it goes in now.
- **Stage III (processing library expansion).** Region analysis
  lands as a stage, and content-aware routing lands with it.
- **Stage IV (performance and execution engine).** Incremental
  execution is measured here: frame time with and without dirty
  regions, cache hit rates, and the cost of the tile comparison
  itself.
- **Temporal stability** cuts across Stages III and IV.

## 3. Node dependency declarations

Every processing node declares its spatial and temporal
dependencies. This is part of the node interface, not metadata
added later.

```rust
/// What input area a node's output depends on.
pub enum SpatialDependency {
    /// One input pixel affects exactly one output pixel.
    Local,
    /// Output depends on a surrounding region: the radius in input
    /// pixels that can influence one output pixel. A 3x3 convolution
    /// declares 1; a 5x5 declares 2; Contour's edge follower declares
    /// the radius its fitted runs reach.
    Neighborhood { radius: u32 },
    /// Output depends on whole-frame statistics. Any input change
    /// invalidates the whole output.
    Global,
}

/// What history a node keeps between frames.
pub enum TemporalDependency {
    /// Output depends only on the current frame.
    Stateless,
    /// Output depends on previous frames. The node owns its history
    /// and states its own invalidation rule: which input changes
    /// discard which history.
    Stateful,
}
```

A node reports both. A pure per-pixel color adjustment is
`Local` + `Stateless`. Contour is `Neighborhood` (its radius is
the follower's reach) + `Stateless`. A global palette analysis is
`Global` + `Stateless`. A temporal denoiser is `Local` +
`Stateful` with its own history rule.

Rules:

- The declaration is a fact about the implementation, not a hint.
  If a node reads pixels outside its declared radius, that is a
  bug, and a test must catch it: run the node on a frame, change
  one pixel outside the declared radius of a tile, and assert the
  tile's output is unchanged.
- A node that cannot state its dependency honestly declares
  `Global`. `Global` is always correct and always slow; it is the
  fallback, not a failure.
- The declaration lives on the node trait, beside its parameter
  schema, so the graph executor and the configuration system both
  read it from one place.

## 4. Tiles and dirty regions

- The frame is divided into tiles of 16×16 source pixels by
  default. Tile size is a graph-level setting, not a per-node
  choice, so every node's cache lines up.
- A tile is dirty when any of its source pixels changed since the
  previous frame. The comparison is exact: a pixel counts as
  changed when its bytes differ. No threshold at this level —
  thresholds belong to the region analysis, not to correctness of
  the dirty set.
- The dirty set is a per-frame input to the graph executor, not
  something each node recomputes.
- Resolution changes, format changes, and graph changes
  invalidate everything: the whole frame is dirty on the first
  frame after any of those.

## 5. Dirty-region propagation

The executor propagates the dirty set through the graph in
topological order, node by node:

- A `Local` node passes the dirty set through unchanged: a dirty
  input tile means the same output tile is dirty.
- A `Neighborhood` node with radius `r` expands each dirty tile by
  `r` input pixels in every direction (rounded out to tile
  boundaries). A 16×16 dirty tile through a radius-2 node
  invalidates the 20×20 area around it, i.e. up to 4 tiles.
- A `Global` node turns any non-empty dirty set into a whole-frame
  dirty set for its output.
- A `Stateful` node applies its own invalidation rule first, then
  propagates as above for the spatial part.
- Branching: a dirty set that reaches a join node is the union of
  the dirty sets on its inputs. A branch that keeps the original
  frame while another branch reconstructs is the reason the
  propagation is per-edge, not per-graph.
- Output tiles outside the propagated dirty set are not recomputed
  at that node. They are read from that node's tile cache.

The executor must be able to explain, for any output tile, which
input tiles made it dirty. That trace is the debugging tool for
the common failure of this design: a node whose declared radius
is too small, which shows up as a tile that changes without being
recomputed.

## 6. Intermediate tile caching

- Every node caches its output per tile. The cache key is the
  tile index plus a generation counter that the executor bumps
  whenever the node's parameters change.
- The cache stores intermediate results, not only the final image.
  A change early in the graph must not force every later stage to
  redo work on tiles those stages already computed.
- Cache entries are owned by the node that produced them and
  dropped when the graph is rebuilt or the resolution changes.
  Memory cost is bounded: one cached output tile per node per
  frame tile, at the node's output resolution. The executor
  reports cache memory as a runtime metric.
- A cache hit is only valid when the node's inputs for that tile
  are unchanged and the node's parameters are unchanged. Parameter
  changes invalidate that node's whole cache, not the frame.
- If a node's cache is disabled (memory-constrained profiles may
  do this), the node recomputes every tile it is asked for. The
  graph still works, just slower. Caching is a performance
  mechanism, never a correctness mechanism.

## 7. Capture damage input

- Capture backends should supply damage regions when the platform
  offers them (Wayland damage, portal damage). The executor
  converts damage rectangles to the tile grid by rounding outward.
- When a backend supplies no damage, PixelFlow computes the dirty
  set itself by comparing each tile with the previous frame's
  tile. The comparison cost is real: a full-frame byte comparison
  every frame. Profiling must establish when tile comparison
  saves time against full reprocessing, and the comparison itself
  must be measured, not assumed free. If comparison costs more
  than it saves for a given graph, the executor may disable
  incremental execution for that graph and process whole frames.
- Damage from the backend is trusted but validated: a damage
  rectangle outside the frame is clamped, never fatal.

## 8. Region analysis

A separate region-analysis stage runs on the source frame and
produces a region map: one classification per tile, with a
confidence score. It produces metadata, not pixels. No algorithm
does its own content recognition.

### 8.1 Categories

| Class | Meaning |
| ---- | ------- |
| Text | Text and UI: thin strokes, sharp corners, glyph shapes |
| Sprite | Sprites and characters: bounded shapes with silhouettes |
| Flat | Flat backgrounds: solid colors, near-zero variance |
| Patterned | Patterned backgrounds: repeating structures |
| Gradient | Gradients: smooth directional color transitions |
| Unknown | Unknown or mixed: use the general-purpose path |

`Unknown` is a real class, not an error. Anything the detector
cannot classify with confidence is `Unknown`, and the graph's
fallback path handles it.

### 8.2 Features

Deterministic image analysis, per tile, computed from the source
pixels:

- Palette size: distinct colors in the tile.
- Color variance: per-channel variance and range.
- Edge density: fraction of pixels that are edges, from a fixed
  edge operator.
- Connected components: count and sizes of same-color connected
  regions.
- Contour geometry: length and straightness of the dominant
  contours.
- Repeated patterns: periodicity of the tile's pixel rows and
  columns.
- Local texture complexity: high-frequency energy.

The feature set is fixed by the stage's version, and the stage
reports which version it is, so a saved profile's region routing
does not silently change meaning when the detector improves.

### 8.3 Classification

- Each tile gets one class and a confidence score in [0, 1].
- Below the confidence threshold the tile is `Unknown`,
  regardless of which class scored highest.
- The classification rule is a documented decision procedure over
  the features, not a tuned black box: the spec for the rule
  lives beside the code, with the thresholds named and their
  reasons stated.
- Classification is never perfectly reliable, and the design
  assumes that: a sprite can look like a background element,
  text can sit inside a sprite, and a flattened screenshot has no
  layer structure. The fallback path and the confidence threshold
  are the answer to that, not better features alone.

### 8.4 User masks and overrides

- Users may supply masks that force a tile or a rectangle of
  tiles to a class, or to `Unknown`.
- An override always wins over the detector for its tiles.
- Overrides are part of a profile, saved and shared with it.
- The analysis stage reports which tiles were overridden, so a
  user can see where the detector was not trusted.

## 9. Content-aware routing

- The graph reads the region map and routes each tile through
  the branch assigned to its class. Text tiles go through the
  text branch, sprite tiles through the reconstruction branch,
  flat tiles through the preservation branch, and so on.
- Each branch is an ordinary subgraph: nodes, parameters, and
  its own dependency declarations. Routing does not need a
  second kind of node.
- Branches are composited back into one frame. Compositing is
  tile-aligned: a tile is produced entirely by one branch, so
  there is no per-pixel blending at branch boundaries. Seams
  between differently processed neighboring tiles are a known
  risk; the routing must be tested on tile-boundary cases, and
  a branch may declare a halo radius (treated like a
  neighborhood radius for invalidation) when its processing
  reads across the tile edge.
- A class with no branch assigned falls back to the general
  path. `Unknown` always uses the general path.
- Routing parameters (which branch per class, the confidence
  threshold, the halo radii) are profile settings with defaults.
  The default profile routes only `Text` and `Sprite`
  differently and leaves everything else on the general path
  until the detector earns trust.

## 10. Temporal stability

- A tile's class must not flicker between frames because a few
  pixels changed. Two mechanisms:
  - **Hysteresis.** Reclassification requires the new class's
    score to beat the current class's score by a margin, not
    merely to exceed it. The margin is a named constant in the
    stage.
  - **Reclassification gating.** A tile is only re-analyzed when
    its visual characteristics changed sufficiently: its feature
    vector moved beyond a threshold since the last analysis.
    A tile whose pixels changed but whose features did not
    keeps its class.
- A tile that changes class is treated as dirty for routing
  purposes even if its pixels did not change: its branch
  assignment changed, so its output must be recomputed.
- Classification flicker is a measured metric: the count of
  tiles that changed class per frame, reported alongside frame
  time.

## 11. The combined system

The two systems share the dirty set:

1. The executor computes the dirty tiles from capture damage or
   tile comparison.
2. The region analysis runs only on dirty tiles. Clean tiles keep
   their previous classification, subject to the stability rules.
3. Tiles whose features moved enough are reclassified; the rest
   keep their class.
4. The graph reprocesses only dirty tiles, through the branch
   each tile's class selects, expanding invalidation by each
   node's dependency radius.
5. Everywhere else, cached intermediate tiles are reused.

The processing budget goes where the image changed and where the
most appropriate processing is needed. That is the whole point.

## 12. Implementation order

1. **Incremental execution.** Dirty-region detection, dependency
   propagation, per-node tile caching. This has measurable
   performance benefits and influences the processing
   architecture, so it comes first and lands in Stage II/IV.
2. **Region analysis.** Segmentation, feature extraction,
   confidence scoring, classification metadata. Stage III.
3. **Content-aware routing.** Different routines per region,
   with the general-path fallback. Stage III.
4. **Temporal stability.** Hysteresis, reclassification gating,
   flicker measurement. Stages III/IV.

The dependency declaration (Section 3) is not step 1 — it is a
prerequisite to all of it, and it goes into the node interface
in Stage II before any processor is written against the new
graph.

## 13. Testing

- **Dependency honesty.** For every node: change one input
  pixel outside a tile's declared radius and assert the tile's
  output is unchanged. A node that fails this has an
  under-declared radius.
- **Propagation.** Synthetic graphs (local chain, neighborhood
  chain, global node, branch and join) with a single dirty tile;
  assert exactly the expected tiles are recomputed, counted by
  instrumenting the nodes.
- **Cache correctness.** Process a frame, change one tile,
  process again, and compare against a full reprocessing of the
  changed frame. The outputs must be identical.
- **Damage input.** Feed known damage rectangles and assert the
  tile dirty set matches. Feed out-of-frame damage and assert
  clamping, not failure.
- **Classification.** Synthetic tiles for each class: rendered
  text, a sprite silhouette, a flat color, a repeating pattern,
  a linear gradient, and a mixed tile. Assert the expected class
  and a confidence above threshold for the clean cases, and
  `Unknown` for the mixed tile.
- **Routing.** A frame with a text tile next to a flat tile;
  assert the text tile went through the text branch and the
  flat tile through the general path, and the seam between them
  shows no artifact beyond the branch halo.
- **Stability.** A sprite moving one pixel per frame across a
  static background; assert its class does not flicker and the
  per-frame reclassified tile count stays small.
- **Performance.** Frame time, cache hit rate, and comparison
  cost on a representative capture (256×224 source at 4×, and a
  larger source), with mostly-static content and with
  full-screen motion, so the benefit and the cost are both
  measured.

## 14. Risks and open questions

- **Classification reliability.** The detector will misclassify.
  The design assumes it and leans on confidence, `Unknown`, and
  overrides. If misclassification rates on real game captures
  are high, routing must stay conservative — default to the
  general path.
- **Seams.** Different processing per tile can produce visible
  seams at tile boundaries. The halo radius mitigates it; if it
  does not suffice, routing may need to operate on larger
  regions than single tiles.
- **Global nodes.** Any graph containing a `Global` node loses
  most of the incremental benefit on every frame with any change.
  Global nodes must be rare and their cost visible in profiling.
- **Cache memory.** Per-node tile caches grow with node count
  and output resolution. The executor must report cache memory
  and support disabling caches per node.
- **Tile size.** 16×16 is the default, not a measured optimum.
  The right size depends on the graph and the content; it is a
  tunable, and the tuning must be measured, not guessed.
- **Damage trust.** A backend that reports damage coarsely (whole
  frame) makes incremental execution useless for that backend.
  The executor must detect that case and fall back to whole
  frames without failing.
