# Lanczos3 baseline

Lanczos3 is the port of the reference `scale_kernel` with
`Kernel::Lanczos3` (`pixelflow_scalers.rs`), registered as
`lanczos3` in the repo. It is a separable Lanczos-3
resampler: one axis table per dimension, built by
`make_axis` with 14-bit fixed-point weights, then a
horizontal pass into a temporary buffer and a vertical
pass into the output. The reference's registry entry is
`Lanczos3 => i("lanczos3", &[], false, 1, false,
Imp::Kernel(Kernel::Lanczos3))`: no native factors, cap 1,
kernel implementation, so the resampler runs directly at
the requested scale.

## Structure

The port lives in
`crates/pixelflow-processing/src/algorithms/lanczos3.rs`:
`kernel_eval` (the reference `k_eval` for
`Kernel::Lanczos3`, the windowed sinc
`sinc(t) * sinc(t / 3)`, radius 3.0), `make_axis` (the
reference axis builder: kernel widened when shrinking,
weights normalized to sum to exactly 16384 with the
rounding residual carried by the largest-magnitude tap),
and `scale_kernel` (the two-pass resampling). The
reference resamples packed RGBA; repo frames are RGB8, so
the alpha channel is absent rather than resampled, and
the RGB channels are unaffected by it. The per-tap source
index, which the reference clamps on every tap of every
pixel, is precomputed once per axis position into the axis
table: same clamp, same index, same accumulation order.
The fixed-point conversions use no float-to-integer cast:
`floor_to_i32` and `round_to_i32` round by binary search
over integer midpoints, the pattern `pixelflow.rs` already
uses, because the workspace lints deny the cast lints.

Scales are 2 and 4 only: the port is verified against the
reference at those scales, and no other scale is claimed.
The spatial dependency is a radius-3 neighborhood, and the
working set is 5 bytes per output pixel (3 output plus the
horizontal pass's temporary buffer at `3 / scale`).

One kernel detail: `sin(pi) / pi` is not exactly zero in
`f64`, so the kernel at integer offsets 1 and 2 is a
rounding-level residue (below 1e-15), not exactly zero.
The oracle comparison below establishes that this
matches the reference bit-for-bit on the tested scenes.

## Oracle verification

A harness built from the reference file (`rustc --edition
2021 -O`, loading the reference file verbatim as a module)
compared against the repo port through the release CLI on 5
scenes: flat, gradient, checkerboard, diagonal, mixed.
Full comparison log: `cg-lanczos3-oracle.log` in the goal's
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

Lanczos3 is bit-identical to the reference on all 5 scenes
at 2x and 4x.

## Re-measurement 1: flat-color behavior

The 14-bit weights at one output position sum to exactly
16384, so a flat input is preserved exactly: shift 0.

- Unit test `flat_image_is_preserved_exactly`: flat
  `[128, 64, 32]` 8x8 input is byte-identical after
  processing at 2x and 4x.
- CLI check: a flat 480x270 `[128, 64, 32]` PNG upscaled to
  1920x1080 at 4x with `--algorithm lanczos3` decodes to
  exactly one distinct colour, `[128, 64, 32]`, all
  2,073,600 pixels. Shift 0.

## Re-measurement 2: regression baseline at 4x

New baseline numbers, measured against the old Contour
expected PNGs in the fixture set. These are not a match
criterion: the expected PNGs are old Contour outputs, and
Lanczos3 is a different algorithm. Test
`lanczos3_fixture_baseline_at_4x`; raw output in
`cg-lanczos3-baseline-fixtures.log`.

| fixture | max abs diff | pixels differing by >1 |
|---------|-------------:|-----------------------:|
| ct      |          181 |                 858672 |
| eb      |          185 |                 349837 |
| ct2     |          159 |                 269330 |
| zelda   |          255 |                 895402 |
| mario   |          255 |                 908407 |
| ctnew   |          173 |                 850092 |

## Re-measurement 3: timing

Release CLI (`target/release/pixelflow upscale --algorithm
lanczos3`), 3 runs each, deterministic synthetic scenes, and
in-process `process()` timing from a throwaway bench crate
in `/tmp/lanczos3-bench` (same scenes, warmup run, then 3
timed `process()` calls). Raw logs: `cg-lanczos3-measure.log`
(CLI wall) and `cg-lanczos3-bench.log` (in-process). The
per-frame budget is ~16.7 ms.

CLI wall time (includes process startup and PNG
decode/encode; the CLI's own one-decimal timer reads
0.0s on most runs, 0.1s once):

| case | output | run 1 | run 2 | run 3 |
|------|--------|------:|------:|------:|
| mixed 480x270 @4x | 1920x1080 | 0.27 s | 0.26 s | 0.25 s |
| mixed 960x540 @2x | 1920x1080 | 0.18 s | 0.17 s | 0.21 s |
| flat 480x270 @4x  | 1920x1080 | 0.07 s | 0.06 s | 0.06 s |

In-process `process()` time, 1920x1080 output:

| case | run 1 | run 2 | run 3 |
|------|------:|------:|------:|
| mixed 480x270 @4x | 36.8 ms | 33.6 ms | 33.4 ms |
| mixed 960x540 @2x | 45.2 ms | 44.7 ms | 45.0 ms |
| flat 480x270 @4x  | 34.0 ms | 34.2 ms | 34.3 ms |

Lanczos3 is about 1.3x slower than Bicubic in-process on
the same output size (33.4-36.8 ms vs 25.5-28.3 ms at 4x,
44.7-45.2 ms vs 33.5-33.8 ms at 2x), the cost of the
7x7 kernel against Bicubic's 5x5, and still 2x to 2.7x
over the ~16.7 ms budget in-process. The 2x case is
slower than the 4x case because the horizontal pass reads
twice as many source rows at the same output width. Flat
and mixed cost the same: Lanczos3 has no flat fast path,
every output pixel runs the full 7x7 kernel.
