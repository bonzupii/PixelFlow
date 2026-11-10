# Contour performance

Benchmarks and memory measurements for the Contour algorithm.
Two sets of numbers live here: the pre-redesign baseline,
measured with `pixelflow-bench` at commit `adbbba2`, and the
post-redesign measurements, taken 2026-11-10 after the strip
redesign landed. Every number below is measured; the model
section says where the working-set model stands against the
measurements.

## Environment

All measurements were taken on this VM: 1.5 shared CPUs, 6 GiB
shared RAM. Those are benchmark-environment limits, not product
limits. Contour sets no fixed size ceiling and no default budget
derived from this machine; a frame that fits in the machine's
memory is attempted. A caller that wants a limit sets one
explicitly (see [Configurable budget](#configurable-budget)).

Methodology: synthetic frames generated in code (flat base,
hard vertical edge, 2:1 diagonal band, checkerboard block,
gradient strip), one warmup plus five timed runs per case, the
tables report the median wall time per frame. Peak RSS is the
subprocess `VmHWM` from `/proc/self/status`, so each case
reports its own peak, including the process baseline (roughly
15 MiB, negligible at the sizes that matter). The benchmark
binary is `pixelflow-bench` in
`crates/pixelflow-cli/src/bin/bench.rs`.

## Pre-redesign baseline (measured, `adbbba2`)

Follower on:

| input | scale | output pixels | median ms | fps | peak RSS MiB |
| --- | --- | --- | --- | --- | --- |
| 256x224 | 2 | 229,376 | 117.0 | 8.55 | 25 |
| 256x224 | 4 | 917,504 | 405.4 | 2.47 | 85 |
| 854x480 | 2 | 1,639,680 | 843.4 | 1.19 | 164 |
| 854x480 | 4 | 6,558,720 | 3,516.6 | 0.28 | 548 |
| 1280x720 | 2 | 3,686,400 | 2,094.8 | 0.48 | 342 |
| 1280x720 | 4 | 14,745,600 | 8,321.7 | 0.12 | 1,228 |
| 1920x1080 | 2 | 8,294,400 | 5,116.1 | 0.20 | 768 |
| 1920x1080 | 4 | 33,177,600 | 18,782.3 | 0.05 | 2,759 |

Follower off (`CONTOUR_NO_FOLLOW=1`):

| input | scale | output pixels | median ms | fps | peak RSS MiB |
| --- | --- | --- | --- | --- | --- |
| 256x224 | 2 | 229,376 | 112.6 | 8.88 | 25 |
| 256x224 | 4 | 917,504 | 398.3 | 2.51 | 85 |
| 854x480 | 2 | 1,639,680 | 831.1 | 1.20 | 164 |
| 854x480 | 4 | 6,558,720 | 3,160.1 | 0.32 | 546 |
| 1280x720 | 2 | 3,686,400 | 1,945.7 | 0.51 | 338 |
| 1280x720 | 4 | 14,745,600 | 7,941.6 | 0.13 | 1,224 |
| 1920x1080 | 2 | 8,294,400 | 4,876.8 | 0.21 | 758 |
| 1920x1080 | 4 | 33,177,600 | 18,655.5 | 0.05 | 2,751 |

Time is linear in output pixels: across the eight cases the
cost is 0.44 to 0.62 microseconds per output pixel, typically
about 0.55, or about 1.8 million output pixels per second. A
60 fps frame budget is 16.7 ms, which buys about 30,000 output
pixels. The smallest case here, 256x224 at 2x (229,376 output
pixels, 117 ms), is 7 times over that budget; at 4x (405 ms)
it is 24 times over. No measured case is within a factor of 7
of real time.

The edge follower is not the bottleneck on this frame shape.
Its cost (follower-on time minus follower-off time) is 4 ms to
380 ms across the cases, 0.7% to 10% of frame time, and the
share does not grow with size: at 1920x1080 4x it is 127 ms
out of 18,782 ms. The per-pixel stages in `combined`
(labelset, grain, hard-light, Scale2x detail) dominate. Speed
work starts there, not in the follower.

Memory is linear in output pixels too. Comparing the 2x and 4x
rows at the same input size isolates the per-output-pixel
slope, free of input-size effects: 91, 82, 84, and 84 bytes
per output pixel at the four sizes, converging to about 84 at
the large sizes. The largest observed ratio of peak RSS to
output pixels is 114 bytes, at the smallest size, where the
process baseline dominates the ratio.

## Per-stage memory breakdown (analytic, cross-checked)

The numbers below come from the allocation sites in
`crates/pixelflow-processing/src/algorithms/contour.rs`, and
the total matches the measured 84 bytes per output pixel slope
exactly. An `FImg` is 12 bytes per pixel (3 channels of f32);
a `UImg` is 3 bytes per pixel; the follower's best-distance map
is 4 bytes per pixel (f32).

- **`grain_layer` — the peak driver before the redesign, about 72 B/output px.**
  At its peak, five full-size f32 images are alive at once
  (`near`, `blur`, `near2`, `sq`, `mean`) plus the blur's
  temporary, while the labelset output (12 B/output px) is
  held alongside: 72 + 12 = 84 B/output px, matching the
  measured slope. The grain layer alone accounts for most of
  the peak, because it chains three gaussian blurs whose
  intermediate images are all kept alive.
- **`labelset` — 144 B/input px** (9 FImg planes at 108
  B/input px plus 9 id planes at 36 B/input px). That is
  36 B/output px at 2x and 9 B/output px at 4x. It is
  transient: the planes are freed when the stage returns, so
  it does not set the peak, but it is the largest per-input
  allocation.
- **`gated_merge` / `smooth_mask` — about 23 B/output px at
  2x**, in temporary f32 images. Transient.
- **`dither_mask` — about 5.75 B/output px at 2x** (an i64
  key image at 8 B/input px plus shifted copies). Transient.
- **Hard-light detail — 24 B/output px** before the redesign:
  the detail image and the hard-light image were both
  materialized as full-size f32 images. Fused away by the
  redesign (see below).
- **`edge_follow2` — 29 B/output px before the redesign:**
  the base image (12), a clone of it (12), the best-distance
  map (4), and a touched map (1). The follower runs on the
  whole output because its runs span whole scanlines
  (`SpatialDependency::Global`); it cannot be stripped, and
  its structures are the one full-frame cost the redesign
  keeps. After the redesign the clone and the touched map
  are gone (the base is passed by value, touched is derived
  from the best map), so the follower holds 16 B/output px.

Why the peak required full-frame storage before the redesign:
`combined()` built the whole output as one `FImg` and every
stage inside it allocated full-frame f32 images, several of
which were alive at once. The stages are per-pixel or local
(3x3 neighbourhoods, blurs with radius 4 output rows), so
there is no reason for the whole frame to be live at once —
except the follower, whose runs span scanlines.

## The redesign: strips, not a pixel ceiling

`combined()` now runs per full-width strip: input rows plus a
halo of 4 true rows on each side, exactly as on a whole image,
and only the interior rows are copied into the single output
`FImg`. The halo covers every stage's reach: 3x3 neighbourhoods
reach 1 input row and dither shifts reach 3 input rows, the
grain blur radius is 4 output rows (at most 2 input rows), and
4 rows covers both with 1 row of margin. Strip starts are even
absolute rows: `dither_blend` aligns its 2x2 blocks to the
frame origin, so a strip starting on an odd row would blend
different pixel pairs than the whole-frame run and change the
output where the dither mask fires. Strips are processed
in parallel across bands with `std::thread::scope`; strips
write disjoint output rows, so the output is identical at any
thread count. Strip height is chosen from a 256 MiB target
working set, clamped to 8–64 input rows; the 8-row minimum
means a very wide frame's strip can exceed the 256 MiB target,
and the budget check uses the actual computed strip term. The follower still runs on the whole output, after
the strips, because its runs span whole scanlines.

Storage changes, in order of effect:

- The strip bound: one strip's working set is live per worker
  thread (the bands run in parallel, each holding one strip at
  a time), about 96 B per output pixel in the strip, bounded by
  the 256 MiB target for ordinary widths (the 8-row minimum
  can exceed it on a very wide frame). The several full-size f32 images in
  `combined()` are now strip-size images.
- The duplicate `gaussian_blur` in `grain_layer` is gone:
  `mean` was the identical call as `blur` (same input, same
  sigma, pure function), so the standard-deviation computation
  now reads `blur` directly. One full-size f32 image and one
  blur pass removed.
- The hard-light stage is fused in place: the hard-light value
  is computed per pixel from the Scale2x u8 detail directly,
  blended with the same arithmetic in the same order. The f32
  detail image and the hard-light image (24 B/output px) are
  no longer materialized.
- `dither_mask` no longer materializes 8 shifted copies: the
  shifts are direct clamped reads, the same values.
- `labelset` no longer re-reads its 9 planes per output pixel:
  the loop is source-pixel-outer / subpixel-inner, the 9 ids
  and colours are read once per source pixel, the id-group
  partition is computed once, and the label weights are one
  sum per group (bit-identical add order). The tap weights are
  precomputed once per subpixel row and column.
- The follower no longer clones the base image and no longer
  keeps a touched map: the base is passed by value, and
  touched is derived from the best map (the follower only
  writes finite distances; untouched pixels keep `INFINITY`).

New peak model: output image (12 B/output px) + follower best
map (4) + final u8 conversion (3, transient) + input (3
B/input px) + one strip's working set per worker thread
(bounded by the 256 MiB target for ordinary widths) = about 19
B/output px + 3 B/input px + the strip bound per thread. The
output image and the follower map are the only
full-frame structures left, and both are required: the output
is the result, and the follower needs whole scanlines. The
model is a conservative upper bound: measured peaks sit below
it (see below), because the 96 B/output px strip bound has
headroom over the strip's true peak. Counting one strip per
thread matters: a one-strip model puts 1080p at 4x at about
810 MiB, below the measured 1,049 MiB peak, while the
per-thread model puts it at about 1,417 MiB, above it. A
budget enforced on the one-strip model would allow frames
whose true peak exceeds the budget.

## Configurable budget

`Contour` on its own sets no budget. `Contour::with_memory_budget(bytes)`
returns a `LimitedContour` that enforces one; both implement
`Processor`, so existing call sites are unchanged. The budget
is checked before any allocation for the frame, against the
working-set model above, in checked u64 arithmetic. A frame
whose estimate exceeds the budget fails with
`ProcessError::MemoryBudgetExceeded`, which names the stage
(`"budget"`), the output pixel count, the estimated bytes, and
the budget. Size arithmetic that overflows fails with
`ProcessError::SizeOverflow` instead of wrapping or panicking.

The CLI reads `PIXELFLOW_MEMORY_BUDGET` (plain integer bytes)
for the `contour` algorithm in both `run` and `process`;
unset or invalid means no budget. The strip thread count comes
from `available_parallelism` (capped at 16), overridden by
`CONTOUR_THREADS`; an invalid value falls back to the default.

## Post-redesign measurements (measured, 2026-11-10)

Same bench, same VM, after the strip redesign. Follower on:

| input | scale | output pixels | median ms | fps | peak RSS MiB |
| --- | --- | --- | --- | --- | --- |
| 256x224 | 2 | 229,376 | 91.2 | 10.97 | 28 |
| 256x224 | 4 | 917,504 | 250.3 | 4.00 | 98 |
| 854x480 | 2 | 1,639,680 | 550.6 | 1.82 | 115 |
| 854x480 | 4 | 6,558,720 | 2,150.7 | 0.46 | 352 |
| 1280x720 | 2 | 3,686,400 | 1,425.3 | 0.70 | 171 |
| 1280x720 | 4 | 14,745,600 | 5,155.3 | 0.19 | 573 |
| 1920x1080 | 2 | 8,294,400 | 3,075.1 | 0.33 | 338 |
| 1920x1080 | 4 | 33,177,600 | 11,015.9 | 0.09 | 1,049 |
| 3840x2160 | 2 | 33,177,600 | 13,477.0 | 0.07 | 729 |
| 3840x2160 | 4 | 132,710,400 | 52,127.5 | 0.02 | 2,380 |

Follower off (`CONTOUR_NO_FOLLOW=1`):

| input | scale | output pixels | median ms | fps | peak RSS MiB |
| --- | --- | --- | --- | --- | --- |
| 256x224 | 2 | 229,376 | 90.5 | 11.05 | 27 |
| 256x224 | 4 | 917,504 | 226.0 | 4.42 | 98 |
| 854x480 | 2 | 1,639,680 | 556.9 | 1.80 | 112 |
| 854x480 | 4 | 6,558,720 | 2,046.3 | 0.49 | 339 |
| 1280x720 | 2 | 3,686,400 | 1,504.6 | 0.66 | 154 |
| 1280x720 | 4 | 14,745,600 | 4,834.0 | 0.21 | 580 |
| 1920x1080 | 2 | 8,294,400 | 2,931.8 | 0.34 | 315 |
| 1920x1080 | 4 | 33,177,600 | 10,857.2 | 0.09 | 1,052 |
| 3840x2160 | 2 | 33,177,600 | 12,023.8 | 0.08 | 744 |
| 3840x2160 | 4 | 132,710,400 | 51,314.8 | 0.02 | 2,274 |

Time is still linear in output pixels, at 0.27 to 0.41
microseconds per output pixel across the cases, typically
about 0.33 to 0.41 at the large sizes — down from about 0.55
before the redesign, a 1.3x to 1.7x speedup across the cases
(1080p at 4x: 18,782 ms to 11,016 ms, 1.7x). The labelset restructure, the
removed duplicate blur, the fused hard-light stage, and strip
parallelism across bands each contributed; the bench does not
separate them. No measured case is within a factor of 5 of
real time: the smallest case, 256x224 at 2x (91 ms), is about
5 times over the 16.7 ms frame budget.

The edge follower is still not the bottleneck: follower-on
minus follower-off is 159 ms at 1080p 4x (1.4% of frame
time) and 813 ms at 4K 4x (1.6%). The per-pixel stages in
`combined` dominate, as before.

Memory no longer follows the old 84 bytes per output pixel
slope. Peak RSS at 1080p 4x is 1,049 MiB, down from 2,759
MiB (2.6x); at 4K 4x it is 2,380 MiB, where the old slope
put it at about 10.38 GiB (4K at 4x was never run before the
redesign: it exceeds this VM's 6 GiB).

Model against measurement (4 worker threads): the corrected
working-set model puts 1080p at 2x at about 359 MiB against
338 measured, 1080p at 4x at about 1,417 MiB against 1,049,
4K at 2x at about 1,030 MiB against 729, and 4K at 4x at
about 3,440 MiB against 2,380. The model overestimates by
6% to 45%: it is a conservative upper bound, which is the
safe direction for a budget check. Regression numbers stayed
identical through the redesign: ct/eb/ct2/zelda/ctnew max abs
diff 1 with 0 pixels over 1, mario max 34 over exactly
6 pixels.

## What the numbers say

Memory no longer scales as 84 bytes per output pixel with
several full-frame f32 images live at once; it scales as about
19 bytes per output pixel plus one strip working set per
worker thread. 1080p at 4x fits in 1,049 MiB where it took
2,759 MiB, and 4K at 4x fits in 2,380 MiB where the old slope
put it at about 10.38 GiB. Speed improved 1.3x to 1.7x, to
about 0.33 microseconds per output pixel at the large sizes,
but it is still the open problem: the per-pixel rate has to
come down by roughly two orders of magnitude for real-time
use, and the redesign does not change the order of magnitude
on its own.
