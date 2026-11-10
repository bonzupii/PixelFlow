# Scale2x baseline

Scale2x is the port of the reference `st_scale2x`
(`pixelflow_scalers.rs`), registered as `scale2x` in the
repo. It is a stage: one 2x pass over a 3x3 neighbourhood
per source pixel, edge-clamped. Each source pixel becomes
a 2x2 block. The block starts as four copies of the centre
pixel `E`, and a corner takes its neighbour's colour only
when the two orthogonal neighbours on that side agree and
the opposite pair does not: with `B` above, `D` left, `F`
right, `H` below, the rule applies when `B != H` and
`D != F`, and then the top-left corner takes `D` when
`D == B`, the top-right takes `F` when `B == F`, the
bottom-left takes `D` when `D == H`, and the bottom-right
takes `F` when `H == F`. Every output pixel is a copy of
one of the five pixels the rule reads, so the algorithm
never invents a colour.

The reference's registry entry is `Scale2x => i("scale2x",
&[2], true, 16, false, Imp::Stage(st_scale2x))`: native
factor 2, rerunnable, cap 16, stage implementation. The
repo port supports scales 2 and 4 only: 4x is two 2x
passes, the reference's own chain (`choose_filters` with
`rerun` picks `[2, 2]` for a factor of 4), the second pass
running on the first pass's output, so a 4x corner is
rounded by the stage twice, exactly as the reference does
it.

## Structure

The port lives in
`crates/pixelflow-processing/src/algorithms/scale2x.rs`:
`stage_pass` (the reference `st_scale2x`, one 2x pass),
called once at 2x and twice at 4x from `process`. The
reference runs the stage on packed RGBA; repo frames are
RGB8, so the alpha channel is absent rather than carried,
and the RGB channels are unaffected by it. The reference
reads its neighbourhood through a clamped padded plane;
the port clamps the same five reads directly, so the
border behaviour is identical: an out-of-frame neighbour
reads the edge pixel.

Scales are 2 and 4 only: the port is verified against the
reference at those scales, and no other scale is claimed.
The spatial dependency is a radius-1 neighborhood, and
the working set is 4 bytes per output pixel (3 output
plus, at 4x, the intermediate 2x frame at 0.75 bytes per
output pixel).

## Oracle verification

A harness built from the reference file (`rustc --edition
2021 -O`, loading the reference file verbatim as a module)
compared against the repo port through the release CLI on 5
scenes: flat, gradient, checkerboard, diagonal, mixed.
Full comparison log: `cg-scale2x-oracle.log` in the goal's
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

Scale2x is bit-identical to the reference on all 5 scenes
at 2x and 4x.

## Re-measurement 1: flat-color behavior

Every output pixel is a copy of one of the five pixels
the rule reads, so a flat input is preserved exactly:
shift 0.

- Unit test `flat_image_is_preserved_exactly`: flat
  `[128, 64, 32]` 8x8 input is byte-identical after
  processing at 2x and 4x.
- CLI check: a flat 480x270 `[128, 64, 32]` PNG upscaled to
  1920x1080 at 4x with `--algorithm scale2x` decodes to
  exactly one distinct colour, `[128, 64, 32]`, all
  2,073,600 pixels. Shift 0.

## Re-measurement 2: regression baseline at 4x

New baseline numbers, measured against the old Contour
expected PNGs in the fixture set. These are not a match
criterion: the expected PNGs are old Contour outputs, and
Scale2x is a different algorithm. Test
`scale2x_fixture_baseline_at_4x`; raw output in
`cg-scale2x-baseline-fixtures.log`.

| fixture | max abs diff | pixels differing by >1 |
|---------|-------------:|-----------------------:|
| ct      |          224 |                 781171 |
| eb      |          167 |                 309536 |
| ct2     |          132 |                 266465 |
| zelda   |          255 |                 726582 |
| mario   |          248 |                 841115 |
| ctnew   |          231 |                 773148 |

## Re-measurement 3: timing

Release CLI (`target/release/pixelflow upscale --algorithm
scale2x`), 3 runs each, deterministic synthetic scenes, and
in-process `process()` timing from a throwaway bench crate
in `/tmp/scale2x-bench` (same scenes, warmup run, then 3
timed `process()` calls). Raw logs: `cg-scale2x-measure.log`
(CLI wall) and `cg-scale2x-bench.log` (in-process). The
per-frame budget is ~16.7 ms.

CLI wall time (includes process startup and PNG
decode/encode; the CLI's own one-decimal timer reads
0.0s on every run):

| case | output | run 1 | run 2 | run 3 |
|------|--------|------:|------:|------:|
| mixed 480x270 @4x | 1920x1080 | 0.05 s | 0.05 s | 0.05 s |
| mixed 960x540 @2x | 1920x1080 | 0.07 s | 0.07 s | 0.07 s |
| flat 480x270 @4x  | 1920x1080 | 0.03 s | 0.04 s | 0.03 s |

In-process `process()` time, 1920x1080 output:

| case | run 1 | run 2 | run 3 |
|------|------:|------:|------:|
| mixed 480x270 @4x | 13.1 ms | 10.6 ms | 10.7 ms |
| mixed 960x540 @2x | 7.3 ms | 7.2 ms | 7.3 ms |
| flat 480x270 @4x  | 6.3 ms | 6.1 ms | 6.2 ms |

Scale2x is the fastest port so far in-process: 10.6-13.1 ms
at 4x and 7.2-7.3 ms at 2x for 1920x1080 output, against
Lanczos3's 33.4-36.8 ms at 4x and 44.7-45.2 ms at 2x on
the same output size, and inside the ~16.7 ms per-frame
budget at both scales. The 4x case costs more than the 2x
case because it runs the stage twice, over 1.25x as many
pixels in total. Flat costs less than mixed at 4x (6.1-6.3
ms vs 10.6-13.1 ms): on uniform input the rule's
comparisons all resolve the same way.
