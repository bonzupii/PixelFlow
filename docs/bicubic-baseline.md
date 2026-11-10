# Bicubic baseline

Bicubic is the port of the reference `scale_kernel` with
`Kernel::Bicubic` (`pixelflow_scalers.rs`), registered as
`bicubic` in the repo. It is a separable Catmull-Rom
resampler: one axis table per dimension, built by
`make_axis` with 14-bit fixed-point weights, then a
horizontal pass into a temporary buffer and a vertical
pass into the output. The reference's registry entry is
`Bicubic => i("bicubic", &[], false, 1, false,
Imp::Kernel(Kernel::Bicubic))`: no native factors, cap 1,
kernel implementation, so the resampler runs directly at
the requested scale.

## Structure

The port lives in
`crates/pixelflow-processing/src/algorithms/bicubic.rs`:
`kernel_eval` (the reference `k_eval` for `Kernel::Bicubic`,
`a = -0.5`, radius 2.0), `make_axis` (the reference axis
builder: kernel widened when shrinking, weights normalized
to sum to exactly 16384 with the rounding residual carried
by the largest-magnitude tap), and `scale_kernel` (the
two-pass resampling). The reference resamples packed RGBA;
repo frames are RGB8, so the alpha channel is absent rather
than resampled, and the RGB channels are unaffected by it.
The per-tap source index, which the reference clamps on
every tap of every pixel, is precomputed once per axis
position into the axis table: same clamp, same index, same
accumulation order. The fixed-point conversions use no
float-to-integer cast: `floor_to_i32` and `round_to_i32`
round by binary search over integer midpoints, the pattern
`pixelflow.rs` already uses, because the workspace lints
deny the cast lints.

Scales are 2 and 4 only: the port is verified against the
reference at those scales, and no other scale is claimed.
The spatial dependency is a radius-2 neighborhood, and the
working set is 5 bytes per output pixel (3 output plus the
horizontal pass's temporary buffer at `3 / scale`).

## Oracle verification

A harness built from the reference file (`rustc --edition
2021 -O`, loading the reference file verbatim as a module)
compared against the repo port through the release CLI on 5
scenes: flat, gradient, checkerboard, diagonal, mixed.
Full comparison log: `cg-bicubic-oracle.log` in the goal's
hidden files.

- flat 2x: max abs diff 0, 0 bytes differing
- flat 4x: max abs diff 0, 0 bytes differing
- gradient 2x: max abs diff 0, 0 bytes differing
- gradient 4x: max abs diff 0, 0 bytes differing
- checker 2x: max abs diff 0, 0 bytes differing
- checker 4x: max abs diff 0, 0 bytes differing
- diagonal 2x: max abs diff 0, 0 bytes differing
- diagonal 4x: max abs diff 0, 0 bytes differing
- mixed 2x: max abs diff 0, 0 bytes differing
- mixed 4x: max abs diff 0, 0 bytes differing

Bicubic is bit-identical to the reference on all 5 scenes
at 2x and 4x.

## Re-measurement 1: flat-color behavior

The 14-bit weights at one output position sum to exactly
16384, so a flat input is preserved exactly: shift 0.

- Unit test `flat_image_is_preserved_exactly`: flat
  `[128, 64, 32]` 8x8 input is byte-identical after
  processing at 2x and 4x.
- CLI check: a flat 480x270 `[128, 64, 32]` PNG upscaled to
  1920x1080 at 4x with `--algorithm bicubic` decodes to
  exactly one distinct colour, `[128, 64, 32]`, all
  2,073,600 pixels. Shift 0.

## Re-measurement 2: regression baseline at 4x

New baseline numbers, measured against the old Contour
expected PNGs in the fixture set. These are not a match
criterion: the expected PNGs are old Contour outputs, and
Bicubic is a different algorithm. Test
`bicubic_fixture_baseline_at_4x`; raw output in
`cg-bicubic-baseline-fixtures.log`.

| fixture | max abs diff | pixels differing by >1 |
|---------|-------------:|-----------------------:|
| ct      |          162 |                 851605 |
| eb      |          177 |                 343985 |
| ct2     |          150 |                 268805 |
| zelda   |          255 |                 881565 |
| mario   |          251 |                 901212 |
| ctnew   |          172 |                 841765 |

## Re-measurement 3: timing

Release CLI (`target/release/pixelflow upscale --algorithm
bicubic`), 3 runs each, deterministic synthetic scenes, and
in-process `process()` timing from a throwaway bench crate
in `/tmp/bicubic-bench` (same scenes, warmup run, then 3
timed `process()` calls). Raw logs: `cg-bicubic-measure.log`
(CLI wall) and `cg-bicubic-bench.log` (in-process). The
per-frame budget is ~16.7 ms.

CLI wall time (includes process startup and PNG
decode/encode; the CLI's own one-decimal timer reads
0.0s on every run):

| case | output | run 1 | run 2 | run 3 |
|------|--------|------:|------:|------:|
| mixed 480x270 @4x | 1920x1080 | 0.21 s | 0.20 s | 0.21 s |
| mixed 960x540 @2x | 1920x1080 | 0.15 s | 0.17 s | 0.15 s |
| flat 480x270 @4x  | 1920x1080 | 0.05 s | 0.05 s | 0.05 s |

In-process `process()` time, 1920x1080 output:

| case | run 1 | run 2 | run 3 |
|------|------:|------:|------:|
| mixed 480x270 @4x | 28.3 ms | 26.0 ms | 25.5 ms |
| mixed 960x540 @2x | 33.8 ms | 33.5 ms | 33.7 ms |
| flat 480x270 @4x  | 25.3 ms | 25.2 ms | 25.2 ms |

Bicubic is 31x to 39x faster than PixelFlow in-process on
the same output size (25.5-28.3 ms vs 0.8-0.9 s at 4x,
33.5-33.8 ms vs 1.2-1.3 s at 2x), and still 1.5x to 2x over
the ~16.7 ms budget in-process. The 2x case is slower
than the 4x case because the horizontal pass reads twice
as many source rows at the same output width. Flat and
mixed cost the same: Bicubic has no flat fast path, every
output pixel runs the full 5x5 kernel.
