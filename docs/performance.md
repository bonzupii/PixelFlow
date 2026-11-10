# PixelFlow performance

Benchmarks and memory budgets for Contour processing, measured
2026-11-09. All numbers on this page were measured by
`pixelflow-bench` (`crates/pixelflow-cli/src/bin/bench.rs`),
which times whole `Contour::process` calls on synthetic frames
generated in code (flat base, hard vertical edge, 2:1 diagonal
band, checkerboard block, gradient strip). No fixtures, no
copyrighted images.

## Hardware context

These are VM numbers, not desktop numbers. The machine that
produced them is a shared virtual machine, and the figures below
describe Contour on that machine only. Do not quote them as
PixelFlow performance on a desktop.

- CPU: AMD EPYC 9D25 (4 vCPUs visible to this VM)
- Memory: 8 GiB total in the VM
- Rust: 1.99.1, release build, single-threaded processing
- Command: `cargo run --release -p pixelflow-cli --bin pixelflow-bench`
  (and `CONTOUR_NO_FOLLOW=1` in front of it for the follower-off
  table)

Each case runs one warmup plus five timed runs; the tables report
the median wall time per frame. Peak RSS is the subprocess
`VmHWM` from `/proc/self/status`, so each case reports its own
peak, including the process baseline (roughly 15 MiB, negligible
at the sizes that matter).

## Results, edge follower on

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

## Results, edge follower off (`CONTOUR_NO_FOLLOW=1`)

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

## Reading the numbers

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
out of 18,782 ms. The per-pixel stages in `combined` (labelset,
grain, hard-light, Scale2x detail) dominate. Stage IV
optimization starts there, not in the follower.

Memory is linear in output pixels too. Comparing the 2x and 4x
rows at the same input size isolates the per-output-pixel
slope, free of input-size effects: 91, 82, 84, and 84 bytes
per output pixel at the four sizes, converging to about 84 at
the large sizes. The largest observed ratio of peak RSS to
output pixels is 114 bytes, at the smallest size, where the
process baseline dominates the ratio. The follower-off rows
give the same slope, so the follower's structures (about 29
bytes per output pixel by the review's count) are not the peak
driver either; the several full-size f32 images held at once
in `combined` are.

## Memory budget

The budget is enforced, not advisory. `Contour::process`
refuses an input before allocating anything for it when the
estimated working set exceeds the budget, with the typed
error `ProcessError::MemoryBudgetExceeded`, which carries the
algorithm name, the requested output pixels, the estimated
bytes, and the budget in bytes.

- Per-pixel cost: **128 bytes per output pixel**. This is a
  conservative bound above the measured slope (about 84) and
  above the largest observed ratio (114).
- Budget: **2 GiB (2,147,483,648 bytes)**, a quarter of an 8 GiB
  machine.
- Largest allowed output: **16,777,216 pixels** (2 GiB / 128).
  That covers every benchmarked case except 1920x1080 at 4x
  (33,177,600 output pixels, 2,759 MiB measured peak), which
  the budget now refuses.

The 1920x1080 4x row in the tables above was measured with
the pre-budget binary; the budget now refuses that input, and
`pixelflow-bench` reports a refusal line for it instead of a
time. The row stays here as the measurement that motivated
the budget.

Tests (`crates/pixelflow-processing/tests/budget.rs`):
the check passes at exactly the budget (4096x4096 output),
passes just under it (4096x4095), fails one row over
(4097x4096) with the typed error and the exact numbers, and
`process` refuses a 2048x2048 input at 4x (67,108,864 output
pixels, 8 GiB estimate) with the typed error. The boundary
tests call `Contour::check_memory_budget` directly: running
the algorithm just under the budget would take about 9
seconds in the suite, and the check is the part that decides.
