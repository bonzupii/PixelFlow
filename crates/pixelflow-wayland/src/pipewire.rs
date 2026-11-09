//! `PipeWire` capture: the stream that receives video buffers from the
//! portal remote and hands frames to the capture backend.
//!
//! The stream runs its own thread with a `PipeWire` main loop. Frames
//! cross to the polling side on a bounded channel that holds one
//! frame: when processing falls behind, the oldest unprocessed frame
//! is dropped and the freshest is kept, so latency never accumulates.
//!
//! Format negotiation offers packed RGB formats only, and the stream
//! is created with `MAP_BUFFERS` so the compositor maps buffers for CPU
//! access. A DMA-BUF buffer arriving anyway is a typed error, not a
//! silent fallback: the fast path for DMA-BUF is future work, and
//! pretending a GPU buffer is a CPU buffer would corrupt frames.

use crate::{WaylandError, convert};
use pipewire as pw;
use pipewire::context::ContextBox;
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::{ParamType, format::FormatProperties, format::MediaSubtype, format::MediaType};
use pipewire::spa::pod::{Pod, serialize::PodSerializer, Value};
use pipewire::spa::utils::{Direction, Fraction, Rectangle};
use pipewire::stream::{StreamBox, StreamFlags, StreamState};
use pixelflow_core::Frame;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// State shared between the `PipeWire` loop thread and the polling side.
struct Shared {
    /// The first fatal stream error, if one happened. The polling side
    /// reads it on every call and reports it until shutdown.
    error: Mutex<Option<WaylandError>>,
    /// Frames delivered to the polling side.
    frames: AtomicU64,
    /// Frames dropped: out of buffers, or the channel already full.
    dropped: AtomicU64,
    /// Whether a DMA-BUF buffer was already reported as an error.
    dmabuf_reported: AtomicBool,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            error: Mutex::new(None),
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            dmabuf_reported: AtomicBool::new(false),
        }
    }
}

/// Record the first fatal stream error. Later errors are ignored so
/// the first failure is the one reported.
fn set_error(shared: &Shared, error: WaylandError) {
    let mut slot = shared.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(error);
    }
}

/// Listener user data: the shared state, the frame channel, the stop
/// flag the loop thread checks, and the negotiated format.
struct StreamData {
    shared: Arc<Shared>,
    sender: mpsc::SyncSender<Frame>,
    stop: Arc<AtomicBool>,
    format: Option<VideoFormat>,
    size: Option<(u32, u32)>,
}

/// Convert one dequeued buffer and send it on. Any failure is recorded
/// in the shared error slot; the buffer itself is released by its Drop.
fn handle_buffer(mut buffer: pw::buffer::Buffer, data: &mut StreamData) {
    let Some(format) = data.format else {
        return;
    };
    let Some((width, height)) = data.size else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(first) = datas.first_mut() else {
        data.shared.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if first.type_() == DataType::DmaBuf {
        if !data.shared.dmabuf_reported.swap(true, Ordering::Relaxed) {
            set_error(
                &data.shared,
                WaylandError::UnsupportedFormat(
                    "DMA-BUF buffers are not mapped for CPU access".to_string(),
                ),
            );
        }
        return;
    }
    let chunk = first.chunk();
    let (offset, stride) = (chunk.offset(), chunk.stride());
    let Some(bytes) = first.data() else {
        set_error(
            &data.shared,
            WaylandError::InvalidFrame("buffer has no mapped data".to_string()),
        );
        return;
    };
    match convert::packed_to_frame(format, width, height, stride, offset, bytes) {
        Ok(frame) => {
            if data.sender.try_send(frame).is_err() {
                data.shared.dropped.fetch_add(1, Ordering::Relaxed);
            } else {
                data.shared.frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(e) => set_error(&data.shared, e),
    }
}

/// Serialize the `EnumFormat` pod offering the packed formats this
/// backend converts. The list must match `convert::SUPPORTED_FORMATS`;
/// the acceptance gate in `param_changed` is authoritative and refuses
/// anything outside it, so drift fails loudly, never silently.
fn build_format_params() -> Result<Vec<u8>, WaylandError> {
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(
            FormatProperties::MediaType,
            Id,
            MediaType::Video
        ),
        pw::spa::pod::property!(
            FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::RGBx,
            VideoFormat::BGRA,
            VideoFormat::RGBA,
            VideoFormat::BGR,
            VideoFormat::RGB
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 120, denom: 1 }
        ),
    );
    let (cursor, _) = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .map_err(|e| WaylandError::PipeWire(format!("failed to build format parameters: {e}")))?;
    Ok(cursor.into_inner())
}

/// Everything the loop thread needs, in one value.
struct LoopSetup {
    stop: Arc<AtomicBool>,
    fd: OwnedFd,
    node_id: u32,
    shared: Arc<Shared>,
    sender: mpsc::SyncSender<Frame>,
    ready: mpsc::Sender<Result<(), WaylandError>>,
}

/// Build the context, core, and stream on the portal fd, report
/// readiness, then drive the main loop with bounded iterations until
/// the stop flag is set. Iterations are bounded so the stop flag is
/// checked regularly: the owning loop type cannot be cloned into a
/// callback, so there is no channel that asks the loop to quit.
fn run_loop_inner(setup: LoopSetup) -> Result<(), WaylandError> {
    let main_loop = MainLoopBox::new(None)
        .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    let context = ContextBox::new(main_loop.loop_(), None)
        .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    let core = context
        .connect_fd(setup.fd, None)
        .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    let stream = StreamBox::new(
        &core,
        "pixelflow-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    let data = StreamData {
        shared: setup.shared.clone(),
        sender: setup.sender,
        stop: setup.stop.clone(),
        format: None,
        size: None,
    };
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _old, new| {
            if let StreamState::Error(msg) = new {
                set_error(&data.shared, WaylandError::PipeWire(msg));
                data.stop.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(|_, data, id, param| {
            if ParamType::from_raw(id) != ParamType::Format {
                return;
            }
            let Some(pod) = param else {
                return;
            };
            let mut info = VideoInfoRaw::new();
            if info.parse(pod).is_err() {
                set_error(
                    &data.shared,
                    WaylandError::InvalidFrame("unparsable video format".to_string()),
                );
                return;
            }
            let raw = info.as_raw();
            let format = VideoFormat::from_raw(raw.format);
            if !convert::is_supported(format) {
                set_error(
                    &data.shared,
                    WaylandError::UnsupportedFormat(format!("{format:?}")),
                );
                return;
            }
            data.format = Some(format);
            data.size = Some((raw.size.width, raw.size.height));
        })
        .process(|stream, data| match stream.dequeue_buffer() {
            None => {
                data.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Some(buffer) => handle_buffer(buffer, data),
        })
        .register()
        .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    let values = build_format_params()?;
    let Some(pod) = Pod::from_bytes(&values) else {
        return Err(WaylandError::PipeWire(
            "serialized format parameters did not parse back".to_string(),
        ));
    };
    let mut params = [pod];
    stream
        .connect(
            Direction::Input,
            Some(setup.node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    stream
        .set_active(true)
        .map_err(|e| WaylandError::PipeWire(e.to_string()))?;
    if setup.ready.send(Ok(())).is_err() {
        // The caller left; tear down by dropping the stream.
        return Ok(());
    }
    let loop_ = main_loop.loop_();
    while !setup.stop.load(Ordering::Relaxed) {
        loop_.iterate(Timeout::Finite(Duration::from_millis(100)));
    }
    drop(listener);
    Ok(())
}

/// The loop thread entry: setup failures go back on `ready` so `start`
/// fails fast instead of hanging on a bad fd.
fn run_loop(setup: LoopSetup) -> Result<(), WaylandError> {
    let ready = setup.ready.clone();
    let result = run_loop_inner(setup);
    if let Err(e) = &result
        && ready.send(Err(e.clone())).is_err()
    {
        // The caller already left; nothing else to report to.
    }
    result
}

/// A `PipeWire` video stream, running on its own thread.
pub struct PipeWireCapture {
    receiver: mpsc::Receiver<Frame>,
    shared: Arc<Shared>,
    stop: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<Result<(), WaylandError>>>,
}

impl PipeWireCapture {
    /// Connect to the portal's `PipeWire` remote and start the stream.
    /// Blocks briefly while the stream negotiates; the main loop keeps
    /// running on its own thread after this returns.
    ///
    /// # Errors
    ///
    /// Returns `PipeWire` when the remote, the context, the stream,
    /// or the format negotiation fails, and when the loop thread does
    /// not report readiness within ten seconds.
    pub fn start(fd: OwnedFd, node_id: u32) -> Result<PipeWireCapture, WaylandError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (frame_tx, frame_rx) = mpsc::sync_channel::<Frame>(1);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), WaylandError>>();
        let shared = Arc::new(Shared::new());
        let setup = LoopSetup {
            stop: stop.clone(),
            fd,
            node_id,
            shared: shared.clone(),
            sender: frame_tx,
            ready: ready_tx,
        };
        let thread = std::thread::spawn(move || run_loop(setup));
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(PipeWireCapture {
                receiver: frame_rx,
                shared,
                stop: Some(stop),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(WaylandError::PipeWire(
                "timed out starting the capture stream".to_string(),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(WaylandError::PipeWire(
                "capture thread ended before starting".to_string(),
            )),
        }
    }

    /// The recorded stream error, if any. A poisoned lock yields its
    /// inner guard, the same policy `set_error` uses.
    fn check_error(&self) -> Option<WaylandError> {
        let slot = self.shared.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.as_ref().cloned()
    }

    /// Take the next frame, waiting briefly for one to arrive.
    /// `Ok(None)` means no frame arrived in that wait, not end of
    /// stream; the stream error, when one is recorded, is reported
    /// instead of a frame.
    ///
    /// # Errors
    ///
    /// Returns the recorded stream error, or `Disconnected` when the
    /// loop thread ended without recording one.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, WaylandError> {
        if let Some(e) = self.check_error() {
            return Err(e);
        }
        match self.receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(e) = self.check_error() {
                    return Err(e);
                }
                Err(WaylandError::Disconnected)
            }
        }
    }

    /// Frames delivered to the polling side so far.
    #[must_use]
    pub fn frames_seen(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Frames dropped so far: out of buffers, or the channel full.
    #[must_use]
    pub fn frames_dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Stop the stream and join the loop thread.
    ///
    /// # Errors
    ///
    /// Returns `PipeWire` when the loop thread ended with an error or
    /// panicked. Stopping an already stopped stream succeeds.
    pub fn stop(&mut self) -> Result<(), WaylandError> {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(WaylandError::PipeWire(
                    "capture thread ended unexpectedly".to_string(),
                )),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        // Best effort: errors have nowhere to go during drop.
        if let Ok(()) = self.stop() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::OwnedFd;

    #[test]
    fn start_with_a_non_pipewire_fd_is_an_error_not_a_panic(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file = File::open("/dev/null")?;
        let fd = OwnedFd::from(file);
        let result = PipeWireCapture::start(fd, 42);
        assert!(result.is_err(), "expected failure connecting to /dev/null");
        if let Err(e) = result {
            assert!(
                matches!(e, WaylandError::PipeWire(_)),
                "expected a PipeWire error, got {e}"
            );
        }
        Ok(())
    }
}
