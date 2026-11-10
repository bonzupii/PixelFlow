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
The soft branch is the repo's existing `Contour` processor, called
unmodified. The per-pixel guard weight (`guard_weight`, 0.2 to 0.5)
cross-fades the two branches through `blend_by_field`, upsampled
with `field_up`.

One mapping difference, stated plainly: the reference's soft branch
is its own `contour_impl` (a structure-tensor
anisotropic-gaussian reconstruction over 7x7 taps), while the repo's
`Contour` is an older algorithm (the combined recipe plus the edge
follower over whole scanlines). They are not the same code and do not
produce the same output, so PixelFlow's output is not bit-identical
to the reference `fr_contour_guard` where the soft branch
contributes. The reference `contour_impl` was not re-ported.

Scales are 2 and 4 only, set by the soft branch: the reference lists
2 to 6, but the repo's `Contour` supports only 2 and 4.

## Oracle verification

A harness built from the reference file (`rustc --edition 2021 -O`,
5 scenes: 3x3 L, flat, 8x8 checker, 48x40 mixed scene, 16x16
diagonal) compared against the repo port. Full comparison log:
`cg-port-oracle.log` in the goal's hidden files.

- Guard weight: bit-identical to the reference on all 5 scenes
  (exact f32 bytes, 0 differing values).
- AaCut: bit-identical on the weight-0 scenes (L, flat, diagonal)
  at 2x and 4x (max abs diff 0).
- Full `fr_contour_guard` vs repo PixelFlow: identical (max abs
  diff 0) on L, flat, and diagonal, where the guard weight is 0 and
  only the crisp branch contributes. Divergent where the soft branch
  contributes, as expected from the mapping difference above:
  checkerboard max abs diff 46 at 2x, 175 at 4x; mixed 48x40 scene
  max abs diff 62 at 2x, 167 at 4x.

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
`cg-port-baseline-fixtures.log`.

| fixture | max abs diff | pixels differing by >1 |
|---------|-------------:|-----------------------:|
| ct      |          224 |                 750558 |
| eb      |          173 |                 310052 |
| ct2     |          104 |                 256940 |
| zelda   |          255 |                 722209 |
| mario   |          248 |                 840107 |
| ctnew   |          231 |                 768947 |

## Re-measurement 3: timing

Release CLI (`target/release/pixelflow upscale --algorithm
pixelflow`), 3 runs each, deterministic synthetic scenes. Raw log:
`cg-port-measure.log`. The per-frame budget is ~16.7 ms.

| case | output | run 1 | run 2 | run 3 |
|------|--------|------:|------:|------:|
| mixed 480x270 @4x | 1920x1080 | 0.6 s | 0.7 s | 0.7 s |
| mixed 960x540 @2x | 1920x1080 | 1.0 s | 1.0 s | 0.7 s |
| flat 480x270 @4x  | 1920x1080 | 0.6 s | 0.6 s | 0.6 s |

PixelFlow is far faster than the old Contour's 9.1 s/frame at the
same output size, and still 36x to 60x over the ~16.7 ms budget.
The cost is dominated by the soft branch: the repo's Contour runs
on every output pixel regardless of the guard weight.
