# Graphics interception

PixelFlow wraps a program, intercepts its graphical output, and
replaces it with an upscaled enhanced fullscreen of the program.
This document describes the interception mechanism that the `run`
command uses. It supersedes the ScreenCast approach described in
`docs/wayland.md`: PixelFlow is not a screen recorder, not a
desktop capture application, not a screencast client, and not a
tool for capturing arbitrary monitor contents through a desktop
portal. The portal/PipeWire stack remains in the tree in
`pixelflow-wayland` as record, but `run` no longer uses it.

## Mechanism

PixelFlow launches the target itself, so it controls the target's
environment. The launcher preloads an interception shim into the
target with `LD_PRELOAD` and names a Unix socket with
`PIXELFLOW_INTERCEPT_SOCKET`. The shim interposes the swap calls
the program makes to present a frame:

- `glXSwapBuffers` for GLX programs.
- `eglSwapBuffers` for EGL programs.

On a swap, the shim reads the presented frame back with
`glReadPixels`, flips it to top-row-first order, and streams it to
PixelFlow over the socket. When the frame is delivered, the shim
does not call the real swap: the program's own window never
presents the frame. That suppression is the replacement: the
program's own presentation is replaced by PixelFlow's fullscreen
window, not duplicated or recorded. When the socket is unset, or
the PixelFlow link is down, every call passes through to the real
swap untouched, so a program run outside PixelFlow behaves exactly
as it would without the shim.

The shim is C, in `crates/pixelflow-intercept/shim/intercept.c`. It
declares the few GL, GLX, and EGL entry points it needs itself
instead of including system headers, so it builds on machines
without GL development headers. All of them are resolved from the
program's own loaded libraries with `dlsym(RTLD_NEXT)`, so the
shim links against libc only. The Rust side of the crate uses no
unsafe code: the FFI boundary lives entirely in the C shim, and
the workspace's unsafe ban stays intact.

## Crates

`pixelflow-intercept` owns the shim and the capture backend.
`pixelflow-capture` holds the backend trait the CLI programs
against. `pixelflow-cli` wires the pieces together in the `run`
command: it binds the interception listener, launches the target
with the shim preloaded, waits for the first intercepted frame (or
for the target to exit without producing one), then runs the
processing thread and the presentation loop exactly as before.
Capture produces `pixelflow-core` frames only.

## Wire protocol

Per frame: a 24-byte header, then the payload. The header is the
magic `PF01`, the width, height, and payload byte count as
little-endian `u32`s, and the program's window id as a
little-endian `u64` (the GLX drawable, or the opaque EGL surface
handle). The payload is tightly packed RGB8, top row first. The
backend validates the header: a bad magic, an out-of-range
dimension, or a payload length that does not match the dimensions
ends the stream with a sticky typed error. A clean stream end (the
program exited and closed the socket) is not an error: the reader
waits for a new connection, and the caller learns the program
exited from the launcher.

The reader thread queues at most two frames. When the queue is
full the oldest frame is dropped and counted, so the newest frame
is always the one that reaches processing. The backend reports the
program's window id from the last frame received; input
forwarding reads it to find the program's window.

## Lifecycle

`pixelflow run <application> [arguments]` runs these steps in
order.

1. Bind the interception listener on a socket unique to the run.
2. Launch the target with the shim preloaded and the socket named
   in its environment. The target is otherwise unmodified.
3. Start the reader thread. The shim connects on the target's
   first swap.
4. Wait up to two seconds for the first intercepted frame, or for
   the target to exit without producing one. A target that exits
   before delivering a frame never started. A target still running
   after the wait may be slow to its first swap, and the processing
   loop keeps waiting for it.
5. Read frames, process each with the configured algorithm, and
   present the result. A replaceable frame slot holds the newest
   processed frame between the processing thread and the
   presentation window, and the processing thread wakes the
   presentation loop through an event-loop proxy each time it
   publishes. When presentation falls behind, a newer frame
   replaces the one still waiting in the slot, so the presented
   frame is always the freshest; replaced frames are counted, not
   hidden.
6. Shut down. Either side ending sets the shared exit flag. The
   processing thread stops capture, closes the listener, and kills
   the child when it is still running. The presentation window
   closes. The child never survives PixelFlow.

## What was verified, and what was not

Verified in this VM (2026-11-10, Ubuntu 24.04, Mesa software GL
under Xvfb): a real GLX program's frames arrive through the shim
and the backend, with the program's own pixels checked, and the
same program run without the shim delivers zero frames to
PixelFlow. The intercepted frames flow through the processing
thread into the presentation slot at 2x. See
`crates/pixelflow-intercept/tests/intercept_glx.rs` and the
processing-thread test in `crates/pixelflow-cli/src/run.rs`.

Verified in this VM under a headless Wayland compositor with
software rendering: see the live verification section below.

Not verified: the EGL swap path has no test in this VM (no EGL
library is installed here), so it is implemented but unverified
until a target using EGL runs under it. The Vulkan path is not
implemented: a Vulkan layer intercepting `vkQueuePresentKHR` is
the follow-up for Vulkan targets. Input forwarding is not
implemented: the backend reports the target window id, which is
the prerequisite, but no input is forwarded yet, so an
interactive target cannot be driven through PixelFlow's window
until that lands.

## Live verification under a headless Wayland compositor

Verified 2026-11-10 in this VM (Ubuntu 24.04) with labwc 0.7.1
headless (wlroots), a 1280x720 output, XWayland for the GLX
target, and Mesa llvmpipe/lavapipe for software rendering
(Vulkan 1.4.318, llvmpipe LLVM 20.1.2). Weston 13 was tried
first: its headless output does advertise a 1280x720 mode
(checked with a minimal wl_output probe client), but
weston-screenshooter aborts on a width-0 assertion and weston
has no wlr-screencopy support, so no compositor screenshot was
obtainable under weston; labwc plus grim provided the
screenshots below.

GPU probe: wgpu enumerates the llvmpipe adapter, and the GPU
nearest node matches CPU nearest exactly at 2x and 4x
(`cargo run --release -p pixelflow-gpu --example probe` under
the compositor: scale 2 parity exact, scale 4 parity exact).

Live run: `pixelflow run /tmp/glx_loop --algorithm nearest
--scale 2` under labwc. The target is a real GLX program
(320x240, background red cycling as (i*7)%256 with blue 200,
an 80x60 yellow rectangle sliding right 3 pixels per frame).
PixelFlow resolves `nearest` to `nearest-gpu` because an
adapter is present.

Screenshot evidence, captured with grim from the compositor
output at 1280x720:

- The upscaled target fills a 960x720 viewport centered with
  160-pixel black bars on each side: 230,297 black pixels
  against 230,400 expected for two 160x720 bars.
- The background is the target's own cycling color: (60, 0,
  200) in one shot, (105, 0, 200) in the next, (158, 0, 200)
  and (180, 0, 200) in later runs.
- The yellow rectangle is present at 43,200 pixels, which is
  the 160x120 texture rectangle at the 1.5x present scale.
- 682,020 pixels differ between two screenshots taken 5
  seconds apart: frames keep arriving and presenting, no
  stall, no frozen frame.
- The target's own window never presents a frame: the shim
  delivers the frame to PixelFlow and the real swap is not
  called, so the replacement is real, not a duplicate.

Frame counts from the runs: capture saw 1573 frames and
dropped 3630 in the first long run; saw 74 and dropped 197 in
about 8 seconds in the second; saw 52 and dropped 145 in the
relaunch run. The target runs unthrottled, faster than the
software-rendered pipeline can consume, so the bounded queue
drops oldest and presentation stays on the latest frame. On
llvmpipe the pipeline presents roughly 9 frames per second at
2x nearest-gpu against a roughly 34 fps source. No 60 fps
claim is made from this VM.

Shutdown defect found by this verification, and fixed: before
the fix, killing the target left PixelFlow alive with its
window open indefinitely. The processing thread set the shared
exit flag when the target exited, but sent no further event;
the presentation loop sleeps on `ControlFlow::Wait` and only
re-checks the flag in `about_to_wait`, so with no more frames
arriving nothing ever woke it. The fix is in
`crates/pixelflow-cli/src/run.rs`: `run_processing` wakes the
loop once more after setting the exit flag, so the loop leaves
its wait, sees the flag, and closes. The regression test
`target_exit_ends_processing_cleanly` now asserts exactly one
wake with no frames published. Live, after the fix, killing
the target exits PixelFlow in 1 second with exit code 0, and a
relaunch run starts, presents, and exits the same way.

Halo finding from this verification, recorded not fixed: the
16x16 tiles the rectangle straddles classify as Sprite (the
rectangle covers 25 to 85 percent of the tile) and route to
Contour, while corner tiles under 25 percent stay on the
general path. Contour's hard-light stage shifts the flat
background inside those tiles from (60, 0, 200) to
(44, 0, 216), a 16-level shift, forming a sharp-edged halo
exactly covering the Sprite tiles. Corner tiles keep the exact
background. This is the known Contour flat-color shift, already
an open product decision, manifesting at tile granularity in
routed output. Routing and Contour are unchanged by this
verification.

Not verified: the EGL swap path has no test in this VM (no EGL
library is installed here); the Vulkan path is not
implemented; input forwarding is not implemented (the backend
reports the target window id, but no input is forwarded, so an
interactive target cannot be driven through PixelFlow's window
yet); live acceptance on a real desktop with a hardware GPU;
presentation at 60 frames per second (llvmpipe presents about
9 fps at 2x nearest-gpu here).
