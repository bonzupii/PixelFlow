# EPX baseline

EPX is the port of the reference `st_epx`
(`pixelflow_scalers.rs`), registered as `epx` in the
repo. It is a stage: one 2x pass over a 3x3 neighbourhood
per source pixel, edge-clamped. Each source pixel becomes
a 2x2 block. The block starts as four copies of the centre
pixel, and a corner takes its neighbour's colour only when
the guard does not fire: with `A` above, `C` left, `F`
right, `H` below, the top-left corner takes `A` when
`C == A`, the top-right takes `F` when `A == F`, the
bottom-left takes `C` when `H == C`, and the bottom-right
takes `H` when `F == H`. The guard fires when any three of
the four neighbours agree: `A == F == C`, `A == F == H`,
`A == C == H`, or `F == C == H`. Every output pixel is a
copy of the centre pixel or one of its four neighbours, so
the algorithm never invents a colour.

The reference's registry entry is `Epx => i("epx",
&[2], true, 16, false, Imp::Stage(st_epx))`: native
factor 2, rerunnable, cap 16, stage implementation. The
repo port supports scales 2 and 4 only: 4x is two 2x
passes, the reference's own chain (`choose_filters` with
`rerun` picks `[2, 2]` for a factor of 4), the second pass
running on the first pass's output, so a 4x corner is
rounded by the stage twice, exactly as the reference does
it.

## EPX and Scale2x are the same rule

The fixture baselines below are identical to Scale2x's
on all six fixtures, and that is correct, not a test
artifact. The guard is subsumed by Scale2x's outer
condition: whenever three neighbours agree, Scale2x's
`B != H && D != F` is false (each agreeing triple
contains an equal opposing pair), and whenever Scale2x's
outer condition holds, no three agree. Where the corner
rules differ in wording they take equal neighbours: the
top-left corner takes the left pixel under Scale2x when
left equals above and the above pixel under EPX under the
same condition, and the two are the same colour. The
same holds for the other three corners. Verified
empirically as well as by the argument: the repo CLI run
with `--algorithm scale2x` and `--algorithm epx` on
random 64x64 noise, checkerboard, and diagonal scenes
produced byte-identical output at 2x and 4x. The two
ports differ in cost, not output: EPX builds the five
neighbour triples per pixel and evaluates the guard,
while Scale2x compares slices and fails fast on flat
input (see the timing below).

## Structure

The port lives in
`crates/pixelflow-processing/src/algorithms/epx.rs`:
`stage_pass` (the reference `st_epx`, one 2x pass),
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
Full comparison log: `cg-epx-oracle.log` in the goal's
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

EPX is bit-identical to the reference on all 5 scenes
at 2x and 4x.

## Re-measurement 1: flat-color behavior

Every output pixel is a copy of the centre pixel or one
of its four neighbours, so a flat input is preserved
exactly: shift 0.

- Unit test `flat_image_is_preserved_exactly`: flat
  `[128, 64, 32]` 8x8 input is byte-identical after
  processing at 2x and 4x.
- CLI check: a flat 480x270 `[128, 64, 32]` PNG upscaled to
  1920x1080 at 4x with `--algorithm epx` decodes to
  exactly one distinct colour, `[128, 64, 32]`, all
  2,073,600 pixels. Shift 0.

## Re-measurement 2: regression baseline at 4x

New baseline numbers, measured against the old Contour
expected PNGs in the fixture set. These are not a match
criterion: the expected PNGs are old Contour outputs, and
EPX is a different algorithm. Test
`epx_fixture_baseline_at_4x`; raw output in
`cg-epx-baseline-fixtures.log`.

| fixture | max abs diff | pixels differing by >1 |
|---------|-------------:|-----------------------:|
| ct      |          224 |                 781171 |
| eb      |          167 |                 309536 |
| ct2     |          132 |                 266465 |
| zelda   |          255 |                 726582 |
| mario   |          248 |                 841115 |
| ctnew   |          231 |                 773148 |

These coincide exactly with the Scale2x baseline: the
two algorithms produce identical output, as shown above.

## Re-measurement 3: timing

Release CLI (`target/release/pixelflow upscale --algorithm
epx`), 3 runs each, deterministic synthetic scenes, and
in-process `process()` timing from a throwaway bench crate
in `/tmp/epx-bench` (same scenes, warmup run, then 3
timed `process()` calls). Raw logs: `cg-epx-measure.log`
(CLI wall) and `cg-epx-bench.log` (in-process). The
per-frame budget is ~16.7 ms. The mixed scenes are
quadrants split at w/2, h/2: top-left gradient,
top-right gradient, bottom-left checkerboard,
bottom-right multiplicative hash; the same pattern is
generated in-process by the bench crate. An earlier CLI
wall run taken while the release suite was running
concurrently read 0.19-0.41 s and was discarded as
contaminated; the numbers below are the unloaded re-run.

CLI wall time (includes process startup and PNG
decode/encode; the CLI's own one-decimal timer reads
0.0s on every run):

| case | output | run 1 | run 2 | run 3 |
|------|--------|------:|------:|------:|
| mixed 480x270 @4x | 1920x1080 | 0.06 s | 0.06 s | 0.06 s |
| mixed 960x540 @2x | 1920x1080 | 0.07 s | 0.07 s | 0.07 s |
| flat 480x270 @4x  | 1920x1080 | 0.04 s | 0.04 s | 0.04 s |

In-process `process()` time, 1920x1080 output:

| case | run 1 | run 2 | run 3 |
|------|------:|------:|------:|
| mixed 480x270 @4x | 15.4 ms | 14.8 ms | 15.7 ms |
| mixed 960x540 @2x | 12.1 ms | 9.8 ms | 9.7 ms |
| flat 480x270 @4x  | 13.2 ms | 10.9 ms | 11.6 ms |

EPX is inside the ~16.7 ms per-frame budget at both
scales, but slower than Scale2x on the same output
(10.6-13.1 ms at 4x mixed, 7.2-7.3 ms at 2x mixed,
6.1-6.3 ms flat at 4x): it builds the five neighbour
triples per pixel and evaluates the guard, while
Scale2x's slice comparisons fail fast on uniform input.
The 4x mixed case at 14.8-15.7 ms is the closest to the
budget; it still passes, and no optimization was
attempted as part of this port.
