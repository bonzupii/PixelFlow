# Wayland capture

PixelFlow captures a Wayland application window through the XDG
Desktop Portal ScreenCast interface and reads the video frames
from PipeWire. The compositor owns source selection: it shows its
own permission and source dialog, and PixelFlow records whatever
window or monitor the user picks. Nothing here reads the
compositor's private state or injects input into the target.

## Crates

`pixelflow-capture` holds the backend trait, the source
configuration, and the error type. It has no platform code.
`pixelflow-wayland` implements the trait for Wayland. It owns the
portal session and the PipeWire stream. `pixelflow-present` owns
the window that shows processed frames, and `pixelflow-cli` wires
the pieces together in the `run` command. Capture produces
`pixelflow-core` frames only. No Wayland, portal, PipeWire, or
wgpu dependency appears in `pixelflow-core` or
`pixelflow-processing`.

## Lifecycle

`pixelflow run <application> [arguments]` runs these steps in
order.

1. Launch the target unmodified. It inherits stdio and runs as a
   plain child process.
2. Wait two seconds for the target's window to appear. The wait
   covers application startup only. There is no window list to
   poll, so the compositor dialog is where the window is picked.
3. Open a portal ScreenCast session over D-Bus with ashpd.
   PixelFlow asks for the source types the compositor offers and
   requests window capture, or monitor capture when the caller
   asked for it with `--capture monitor`. The session start call
   blocks until the user answers the compositor dialog.
4. Take the PipeWire remote file descriptor from the session and
   connect a PipeWire stream to the selected node id. The portal
   also reports a PipeWire serial when one exists; it is recorded
   on the session for diagnostics. The stream connects by node id,
   so portals that report no serial still work.
5. Negotiate the format. The stream offers packed RGB formats
   only, in this preference order: BGRx, RGBx, BGRA, RGBA, BGR,
   RGB. The compositor picks one and reports it back through a
   format parameter. Anything outside that list is a typed error,
   never a silent conversion.
6. Read buffers, convert each to a frame, process it with
   contour, and present it. A bounded channel holds one
   processed frame between the processing thread and the
   presentation window. When presentation falls behind, the oldest
   unprocessed frame is dropped and the freshest is kept, so
   latency never accumulates.
7. Shut down. Either side ending sets the shared exit flag. The
   processing thread stops the stream, closes the portal session,
   and kills the child when it is still running. The presentation
   window closes. The child never survives PixelFlow.

## The PipeWire loop thread

The stream runs on its own thread with a PipeWire main loop.
PipeWire 0.10 splits its objects into owning Box types and
borrowed views: `MainLoopBox`, `ContextBox`, and `StreamBox`
construct, and the borrowed `MainLoop`, `Context`, and `Stream`
carry the methods. The loop thread drives the loop with bounded
iterations (`Loop::iterate` with a 100 ms timeout) and checks a
shared stop flag between iterations. That shape exists because
the Box loop cannot be cloned into a callback, so no channel can
ask the loop to quit from another thread. A stream error sets the
same stop flag, and the polling side reports the recorded error.

## Buffers

Buffers arrive as mapped CPU memory. The conversion honors the
chunk offset and the row stride, so padded rows convert
correctly, and a buffer too short for the rows its stride implies
is an error, never a truncated read. Resolution changes arrive as
format parameters and take effect on the next buffer. A DMA-BUF
buffer arriving anyway is a typed error the first time, not a
silent fallback: mapping GPU buffers for CPU access is future
work, and pretending a GPU buffer is a CPU buffer would corrupt
frames. Dropped frames are counted, never hidden: the capture
object exposes frames seen and frames dropped, and the processing
thread prints how many processed frames it dropped at shutdown.

## Input forwarding

ScreenCast carries no input. PixelFlow does not forward
keyboard, mouse, or controller input yet, and it does not install
global input interception. Input forwarding is future work
through the XDG RemoteDesktop portal and EIS/libei, which need
their own permission from the user. Until that lands, the target
keeps its own input: the user interacts with the target window
directly, and PixelFlow shows the processed copy.

## What was verified, and what was not

The build machine has no Wayland desktop, no compositor, no
portal, no PipeWire daemon, and no gtk-slashem. What was verified
there: the full workspace builds (`cargo build --release` clean)
and passes its tests (`cargo test --release`: 48 passed, 0
failed: 8 CLI, 3 core, 5 presentation, 1 processing unit test, 3
regression, 12 Wayland, 16 synthetic). The format conversion and stride
handling pass unit tests, the letterbox math passes unit tests,
the CLI parses the `run` command, and the portal and PipeWire
error paths fail fast with typed errors instead of hanging or
panicking. The CLI paths were exercised live: `upscale` on the
Mario fixture produces 1024x896 in 0.4 s with max diff 34 over 6
pixels, matching the regression table; `run /bin/true` exits 1
with "exited before capture started"; `run --windowed /bin/sleep
30` exits 1 with a D-Bus connection error from the missing
portal, and the child is killed with no stray process left.

What was not verified there, and needs a Wayland desktop:
the end-to-end wrapper. `pixelflow run gtk-slashem` on a Wayland
desktop is the acceptance test: the application starts, the
compositor asks for capture permission and window selection,
PixelFlow receives the stream, contour processes frames, and a
separate PixelFlow window shows the result. A mocked success is
not a live capture, so this document claims nothing about that
run until it happens on a desktop.
