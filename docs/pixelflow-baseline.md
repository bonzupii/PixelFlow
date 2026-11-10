# PixelFlow baseline

PixelFlow is the port of the reference `fr_contour_guard`
(`pixelflow_scalers.rs`), named `pixelflow` in the repo. It is the
flagship algorithm. The repo's `Contour` keeps its name and stays in
the repo. This is not the reference's own `pixelflow` algorithm
(`fr_pixelflow`), which is a different algorithm, out of scope, and
not ported.

## Structure

The crisp branch is the `AaCut` stage (`st_aacut`), ported as a
supporting stage inside `crates/pixelflow-processing/src/algorithms/pixelflow.rs`.
The soft branch is the reference's own `contour_impl`, ported as the
private `contour_soft` inside the same module: a structure-tensor
anisotropic-gaussian reconstruction over 7x7 taps, with the
checkerboard and stripe dither bypass. It is not the repo's `Contour`
algorithm, which is a different, older algorithm (the combined recipe
plus the edge follower over whole scanlines) and stays untouched in
`contour.rs`. The per-pixel guard weight (`guard_weight`, 0.2 to 0.5)
cross-fades the two branches through `blend_by_field`, upsampled
with `field_up`.

The first port (commit 74b4a7c) used the repo's `Contour` as the soft
branch. That was the wrong algorithm under the flagship name: the
oracle showed divergence where the soft branch contributes
(checkerboard max abs diff 46 at 2x, 175 at 4x; mixed 48x40 scene 62
at 2x, 167 at 4x). The correction (this branch, rebased onto main
f930678) replaces the soft branch with the ported `contour_impl`.
The old numbers do not carry over; every measurement below is fresh.

Scales are 2 and 4 only: the port is verified against the reference
at those scales, and no other scale is claimed.

## Oracle verification

A harness built from the reference file (`rustc --edition 2021 -O`,
5 scenes: 3x3 L, flat, 8x8 checker, 48x40 mixed scene, 16x16
diagonal) compared against the repo port. The reference source used
for the harness is code-identical to the current reference file
(verified by stripping comments and diffing). Full comparison log:
`cg-port-oracle-v2.log` in the goal's hidden files.

- Guard weight: bit-identical to the reference on all 5 scenes
  (exact f32 bytes, 0 differing values). The `guard_weight`
  function is unchanged since the first port, verified by diffing
  it against the first port's commit.
- Full `fr_contour_guard` vs repo PixelFlow: bit-identical on all
  5 scenes at 2x and 4x (max abs diff 0, 0 bytes differing).

## Re-measurement 1: flat-color shift

Old Contour shifted flat mid-tones by up to ~16 via its hard-light
stage. PixelFlow shift on the same probe is 0.

- Unit test `flat_input_is_preserved_exactly`: flat `[128, 64, 32]`
  8x8 input is byte-identical after processing at 2x and 4x.
- CLI check: a flat 480x270 `[128, 64, 32]` PNG upscaled to
  1920x1080 at 4x with `--algorithm pixelflow` decodes to exactly
  one distinct colour, `[128, 64, 32]`. Shift 0.

## Re-measurement 2: regression baseline at 4x

New baseline numbers, measured against the old Contour expected
PNGs in the fixture set. These are not a match criterion: the
expected PNGs are old Contour outputs, and PixelFlow is a different
algorithm. Test `pixelflow_fixture_baseline_at_4x`; raw output in
`cg-port2-baseline-fixtures.log`.

| fixture | max abs diff | pixels differing by >1 |
|---------|-------------:|-----------------------:|
| ct      |          224 |                 780041 |
| eb      |          173 |                 310146 |
| ct2     |          216 |                 266583 |
| zelda   |          255 |                 725959 |
| mario   |          248 |                 841453 |
| ctnew   |          231 |                 772606 |

## Re-measurement 3: timing

Release CLI (`target/release/pixelflow upscale --algorithm
pixelflow`), 3 runs each, deterministic synthetic scenes. Raw log:
`cg-port2-measure.log`. The per-frame budget is ~16.7 ms.

| case | output | run 1 | run 2 | run 3 |
|------|--------|------:|------:|------:|
| mixed 480x270 @4x | 1920x1080 | 0.9 s | 0.8 s | 0.9 s |
| mixed 960x540 @2x | 1920x1080 | 1.3 s | 1.2 s | 1.3 s |
| flat 480x270 @4x  | 1920x1080 | 0.2 s | 0.2 s | 0.2 s |

PixelFlow is far faster than the old Contour's 9.1 s/frame at the
same output size, and still 48x to 78x over the ~16.7 ms budget.
The cost is dominated by the soft branch: the ported `contour_impl`
runs the full 7x7 anisotropic reconstruction on every output
pixel, regardless of the guard weight. Mixed scenes are slower
than in the first port (0.9 s vs 0.6-0.7 s at 4x, 1.2-1.3 s vs
0.7-1.0 s at 2x) because the corrected soft branch does more work
per pixel than the repo's Contour did; flat input is faster
(0.2 s vs 0.6 s) because the all-same fast path short-circuits
the reconstruction on uniform windows.
