//! Graphics interception capture.
//!
//! The wrapped program is launched with an `LD_PRELOAD` shim (see
//! `shim/intercept.c`) that interposes the calls the program makes
//! to present a frame: its swap calls for GL programs, its X11
//! image puts for software-rendered programs. Instead of
//! presenting a frame to its own window, the program's frame is
//! read back or converted and streamed to `PixelFlow` over a
//! `Unix` socket. This backend owns the listening end of that
//! socket and turns the streamed frames into [`Frame`] values for
//! the processing thread.
//!
//! The backend implements [`CaptureBackend`], so the CLI drives it
//! exactly like any other capture source. It knows nothing about
//! windows, monitors, or compositors: interception replaces the
//! program's presentation at its own rendering boundary, which is
//! why there is no source selection, no permission dialog, and no
//! desktop portal involved.
//!
//! Wire protocol, per frame: 24 header bytes, then the payload.
//! The header is the magic `PF01`, the width, height, and payload
//! byte count as little-endian `u32`s, and the program's window id
//! as a little-endian `u64` (the GLX drawable, the opaque EGL
//! surface handle, or the X window id). The payload is tightly
//! packed RGB8, top row first. A frame whose header fails
//! validation ends the stream with a sticky error: the reader
//! thread stops and [`CaptureBackend::capture_frame`] reports that
//! error. A clean stream end (the program exited and closed the
//! socket) is not an error: the reader waits for a new connection,
//! and the caller learns the program exited from the launcher
//! instead.

use pixelflow_capture::{CaptureBackend, CaptureConfig, CaptureError};
use pixelflow_core::{Frame, PixelFormat};
use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Environment variable the shim reads for the socket path.
const SOCKET_ENV: &str = "PIXELFLOW_INTERCEPT_SOCKET";

/// The `LD_PRELOAD` variable the launcher sets to the shim.
const PRELOAD_ENV: &str = "LD_PRELOAD";

/// Frame header length in bytes: magic, width, height, byte count,
/// window id.
const HEADER_LEN: usize = 24;

/// Magic bytes at the start of every frame header.
const MAGIC: [u8; 4] = *b"PF01";

/// Largest frame dimension the backend accepts, matching the shim.
const MAX_DIMENSION: u32 = 16384;

/// How long [`CaptureBackend::capture_frame`] waits for a frame
/// before reporting none arrived.
const POLL_WAIT: Duration = Duration::from_millis(20);

/// How long the reader waits on a connected stream before checking
/// the shutdown flag again.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// How many frames the queue holds. When it is full the oldest
/// frame is dropped and counted, so the newest frame is always the
/// one that reaches processing.
const QUEUE_CAPACITY: usize = 2;

/// Path of the compiled interception shim, baked in at build time
/// by `build.rs`.
#[must_use]
pub fn shim_path() -> &'static str {
    env!("PIXELFLOW_INTERCEPT_SHIM")
}

/// Path of the 32-bit interception shim, baked in at build time
/// by `build.rs`; empty when the toolchain had no 32-bit support.
#[must_use]
pub fn shim32_path() -> &'static str {
    env!("PIXELFLOW_INTERCEPT_SHIM32")
}

/// ELF class byte values, the fifth byte of an ELF header.
const ELF_CLASS_32: u8 = 1;
const ELF_CLASS_64: u8 = 2;

/// The ELF class of the file at `path`: 1 for a 32-bit object, 2
/// for a 64-bit object, `None` when the file is not an ELF object
/// or cannot be read.
fn elf_class(path: &std::path::Path) -> Option<u8> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0_u8; 5];
    file.read_exact(&mut header).ok()?;
    if !header.starts_with(b"\x7fELF") {
        return None;
    }
    match header[4] {
        ELF_CLASS_32 | ELF_CLASS_64 => Some(header[4]),
        _ => None,
    }
}

/// Resolve `program` to a path: the path as given when it names a
/// file, else the first match on `PATH`.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(program);
    if candidate.is_file() {
        return Some(candidate);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The program whose ELF class decides the shim: the program
/// itself, unless it is a qemu-user emulator, in which case the
/// first argument naming an existing file is the guest program the
/// shim must match. The shim loads into the guest, not into qemu.
fn effective_target(program: &str, args: &[String]) -> Option<PathBuf> {
    let resolved = resolve_program(program)?;
    let name = resolved.file_name()?.to_string_lossy();
    if name.starts_with("qemu-") {
        for arg in args {
            let candidate = PathBuf::from(arg);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        return None;
    }
    Some(resolved)
}

/// A delivered frame with the window id the shim reported for it.
struct DeliveredFrame {
    /// The frame pixels.
    frame: Frame,
    /// The program's window id: the GLX drawable, the opaque EGL
    /// surface handle, or the X window id.
    window_id: u64,
}

/// The bounded frame queue between the reader thread and the
/// polling side. The reader pushes; the polling side pops the
/// oldest. A push on a full queue drops the oldest frame and counts
/// it, so the queue always holds the newest frames.
struct FrameQueue {
    /// Queued frames, oldest first.
    frames: Mutex<VecDeque<DeliveredFrame>>,
    /// Signalled on every push, so a waiting pop wakes promptly.
    ready: Condvar,
    /// Frames dropped because the queue was full.
    dropped: AtomicU64,
}

impl FrameQueue {
    /// An empty queue.
    fn new() -> FrameQueue {
        FrameQueue {
            frames: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            dropped: AtomicU64::new(0),
        }
    }

    /// Queue `frame`, dropping the oldest queued frame when the
    /// queue is full. The mutex is never poisoned by code that
    /// handles the poison by taking the guard anyway: a poisoned
    /// mutex means a thread panicked while holding it, and the
    /// frames inside are still valid to drain.
    fn push(&self, frame: DeliveredFrame) {
        let mut frames = self
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if frames.len() >= QUEUE_CAPACITY {
            frames.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        frames.push_back(frame);
        self.ready.notify_one();
    }

    /// Take the oldest queued frame, waiting up to `wait` for one
    /// to arrive. Returns `None` when the wait expires with the
    /// queue still empty.
    fn pop(&self, wait: Duration) -> Option<DeliveredFrame> {
        let mut frames = self
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if frames.is_empty() {
            let (guard, _) = self
                .ready
                .wait_timeout(frames, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            frames = guard;
        }
        frames.pop_front()
    }
}

/// Read one frame from `stream`: validate the header, read the
/// payload, and build the frame. The sequence number counts frames
/// on this stream, so two sequential connections do not share one.
///
/// # Errors
///
/// Returns [`CaptureError::Disconnected`] when the stream ends, and
/// [`CaptureError::StreamFailed`] when a frame is structurally
/// invalid: a bad magic, an out-of-range dimension, a payload
/// length that does not match the dimensions, or a payload read
/// failure.
fn read_frame(stream: &mut UnixStream, sequence: u64) -> Result<DeliveredFrame, CaptureError> {
    let mut header = [0_u8; HEADER_LEN];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(CaptureError::Disconnected);
        }
        Err(e) => {
            return Err(CaptureError::StreamFailed(format!(
                "failed to read interception frame header: {e}"
            )));
        }
    }
    if header[0..4] != MAGIC {
        return Err(CaptureError::StreamFailed(
            "interception frame header has bad magic".to_string(),
        ));
    }
    let width = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let height = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    let byte_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    let window_id = u64::from_le_bytes([
        header[16], header[17], header[18], header[19], header[20], header[21], header[22],
        header[23],
    ]);
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(CaptureError::StreamFailed(format!(
            "interception frame has invalid dimensions {width}x{height}"
        )));
    }
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .filter(|bytes| *bytes == byte_len)
        .ok_or_else(|| {
            CaptureError::StreamFailed(format!(
                "interception frame byte count {byte_len} does not match {width}x{height} RGB8"
            ))
        })?;
    let mut data = vec![0_u8; expected as usize];
    match stream.read_exact(&mut data) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(CaptureError::Disconnected);
        }
        Err(e) => {
            return Err(CaptureError::StreamFailed(format!(
                "failed to read interception frame payload: {e}"
            )));
        }
    }
    let frame = Frame::new(width, height, PixelFormat::Rgb8, data)
        .map_err(|e| CaptureError::StreamFailed(format!("interception frame is invalid: {e}")))?;
    Ok(DeliveredFrame {
        frame: frame.with_sequence(sequence),
        window_id,
    })
}

/// The interception capture backend: a Unix socket listener plus a
/// reader thread that turns streamed frames into [`Frame`] values.
pub struct InterceptCapture {
    /// Where the shim connects.
    socket_path: PathBuf,
    /// The listener, shared with the reader thread. Nonblocking, so
    /// the reader polls the shutdown flag between accepts.
    listener: Arc<UnixListener>,
    /// Frames the reader delivered, shared with the reader thread.
    queue: Arc<FrameQueue>,
    /// Frames the reader received from the shim so far, including
    /// frames still queued. [`CaptureBackend::frames_seen`] counts
    /// frames handed to the polling side instead; this count is the
    /// one the startup wait uses, because no polling happens while
    /// waiting for the first frame.
    received: Arc<AtomicU64>,
    /// Frames handed to the polling side so far.
    seen: AtomicU64,
    /// The last window id the shim reported, 0 when no frame has
    /// arrived yet. The input-forwarding follow-up reads this to
    /// find the program's window.
    window_id: Arc<AtomicU64>,
    /// The first fatal stream error the reader hit, if any. The
    /// reader stops on it, and [`CaptureBackend::capture_frame`]
    /// reports it.
    fatal_error: Arc<Mutex<Option<CaptureError>>>,
    /// True once the reader thread has exited.
    reader_done: Arc<AtomicBool>,
    /// Set by [`InterceptCapture::shutdown`]; the reader thread
    /// exits when it sees this.
    shutdown: Arc<AtomicBool>,
    /// The reader thread, while it runs.
    reader: Option<JoinHandle<()>>,
    /// True once [`CaptureBackend::initialize`] started the reader.
    started: bool,
}

impl InterceptCapture {
    /// Bind a fresh listener on a socket path unique to this
    /// process, and prepare the reader. Nothing listens for frames
    /// until [`CaptureBackend::initialize`] starts the reader, and
    /// the launcher must set the socket path and shim path from
    /// [`InterceptCapture::environment`] on the child before then.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::SessionFailed`] when the socket path
    /// cannot be bound, which means the temporary directory is
    /// unusable.
    pub fn new() -> Result<InterceptCapture, CaptureError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let socket_path = std::env::temp_dir().join(format!(
            "pixelflow-intercept-{}-{id}.sock",
            std::process::id()
        ));
        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            CaptureError::SessionFailed(format!(
                "failed to bind interception socket {}: {e}",
                socket_path.display()
            ))
        })?;
        listener.set_nonblocking(true).map_err(|e| {
            CaptureError::SessionFailed(format!("failed to configure interception socket: {e}"))
        })?;
        Ok(InterceptCapture {
            socket_path,
            listener: Arc::new(listener),
            queue: Arc::new(FrameQueue::new()),
            received: Arc::new(AtomicU64::new(0)),
            seen: AtomicU64::new(0),
            window_id: Arc::new(AtomicU64::new(0)),
            fatal_error: Arc::new(Mutex::new(None)),
            reader_done: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            reader: None,
            started: false,
        })
    }

    /// The socket path the shim connects to.
    #[must_use]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// The environment the launcher sets on the wrapped program:
    /// the shim preloaded, and the socket it streams frames to.
    /// This is the 64-bit shim, for targets the caller already
    /// knows are 64-bit; [`InterceptCapture::environment_for`]
    /// selects by the target's ELF class instead.
    #[must_use]
    pub fn environment(&self) -> [(String, String); 2] {
        [
            (PRELOAD_ENV.to_string(), shim_path().to_string()),
            (
                SOCKET_ENV.to_string(),
                self.socket_path.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// The environment the launcher sets on the wrapped program,
    /// with the shim matching the target's ELF class: the 32-bit
    /// shim for a 32-bit target, the 64-bit shim otherwise. A
    /// program that cannot be resolved, or is not an ELF object,
    /// gets the 64-bit shim.
    ///
    /// # Errors
    ///
    /// Returns a message when the target is 32-bit and no 32-bit
    /// shim was built: preloading the 64-bit shim into a 32-bit
    /// process fails silently in the loader and captures zero
    /// frames, so this is a loud failure instead.
    pub fn environment_for(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<[(String, String); 2], String> {
        let shim = match effective_target(program, args).and_then(|target| elf_class(&target)) {
            Some(ELF_CLASS_32) => {
                if shim32_path().is_empty() {
                    return Err(format!(
                        "target {program} is 32-bit but no 32-bit interception shim was built (no 32-bit C toolchain)"
                    ));
                }
                shim32_path().to_string()
            }
            _ => shim_path().to_string(),
        };
        Ok([
            (PRELOAD_ENV.to_string(), shim),
            (
                SOCKET_ENV.to_string(),
                self.socket_path.to_string_lossy().into_owned(),
            ),
        ])
    }

    /// The window id of the last frame received, or 0 when no frame
    /// has arrived yet.
    #[must_use]
    pub fn target_window(&self) -> u64 {
        self.window_id.load(Ordering::Relaxed)
    }

    /// Frames the reader received from the shim so far, including
    /// frames still queued and frames dropped from a full queue.
    #[must_use]
    pub fn frames_received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

impl CaptureBackend for InterceptCapture {
    fn initialize(&mut self, _config: &CaptureConfig) -> Result<(), CaptureError> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let listener = self.listener.clone();
        let queue = self.queue.clone();
        let received = self.received.clone();
        let shutdown = self.shutdown.clone();
        let reader_done = self.reader_done.clone();
        let fatal_error = self.fatal_error.clone();
        let window_id = self.window_id.clone();
        let reader = std::thread::Builder::new()
            .name("pixelflow-intercept-reader".to_string())
            .spawn(move || {
                let mut sequence: u64 = 0;
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                            loop {
                                if shutdown.load(Ordering::Relaxed) {
                                    break;
                                }
                                match read_frame(&mut stream, sequence) {
                                    Ok(delivered) => {
                                        sequence += 1;
                                        window_id.store(delivered.window_id, Ordering::Relaxed);
                                        received.fetch_add(1, Ordering::Relaxed);
                                        queue.push(delivered);
                                    }
                                    Err(CaptureError::Disconnected) => break,
                                    Err(error) => {
                                        if let Ok(mut slot) = fatal_error.lock() {
                                            *slot = Some(error);
                                        }
                                        break;
                                    }
                                }
                            }
                            if fatal_error.lock().is_ok_and(|slot| slot.is_some()) {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
                reader_done.store(true, Ordering::Relaxed);
            })
            .map_err(|e| {
                CaptureError::SessionFailed(format!("failed to start interception reader: {e}"))
            })?;
        self.reader = Some(reader);
        Ok(())
    }

    fn capture_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
        if let Ok(slot) = self.fatal_error.lock()
            && let Some(error) = slot.as_ref()
        {
            return Err(error.clone());
        }
        if let Some(delivered) = self.queue.pop(POLL_WAIT) {
            self.seen.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(delivered.frame));
        }
        if self.reader_done.load(Ordering::Relaxed) {
            return Err(CaptureError::Disconnected);
        }
        Ok(None)
    }

    fn shutdown(&mut self) -> Result<(), CaptureError> {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            // The reader polls the flag between accepts and between
            // frame reads, so it exits promptly once the flag is set.
            // A join failure means the reader panicked; report it
            // instead of silently continuing.
            reader.join().map_err(|_| {
                CaptureError::SessionFailed("interception reader ended unexpectedly".to_string())
            })?;
        }
        std::fs::remove_file(&self.socket_path).ok();
        Ok(())
    }

    fn frames_seen(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    fn frames_dropped(&self) -> u64 {
        self.queue.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = std::env::temp_dir().join(format!("pixelflow-intercept-test-{name}"));
        let mut file = std::fs::File::create(&path)
            .map_err(|e| format!("the test file must be writable: {e}"))?;
        file.write_all(bytes)
            .map_err(|e| format!("the test file must be writable: {e}"))?;
        Ok(path)
    }

    #[test]
    fn elf_class_reads_both_classes() -> Result<(), String> {
        let path32 = write_temp("32", &[0x7f, b'E', b'L', b'F', 1, 0, 0, 0])?;
        let path64 = write_temp("64", &[0x7f, b'E', b'L', b'F', 2, 0, 0, 0])?;
        let path_text = write_temp("text", b"not an elf object")?;
        assert_eq!(elf_class(&path32), Some(1));
        assert_eq!(elf_class(&path64), Some(2));
        assert_eq!(elf_class(&path_text), None);
        assert_eq!(
            elf_class(&PathBuf::from("/nonexistent/pixelflow-intercept-test")),
            None
        );
        std::fs::remove_file(path32).ok();
        std::fs::remove_file(path64).ok();
        std::fs::remove_file(path_text).ok();
        Ok(())
    }

    #[test]
    fn effective_target_uses_the_qemu_guest() -> Result<(), String> {
        let qemu_path = std::env::temp_dir().join("qemu-i386-pixelflow-test");
        std::fs::write(&qemu_path, b"fake emulator")
            .map_err(|e| format!("the test file must be writable: {e}"))?;
        let guest = write_temp("guest32", &[0x7f, b'E', b'L', b'F', 1, 0, 0, 0])?;
        let target = effective_target(
            qemu_path.to_str().ok_or("the qemu path must be UTF-8")?,
            &[guest.to_string_lossy().into_owned()],
        );
        assert_eq!(target, Some(guest.clone()));
        // Without a guest argument there is no target to match.
        let target = effective_target(
            qemu_path.to_str().ok_or("the qemu path must be UTF-8")?,
            &[],
        );
        assert_eq!(target, None);
        // A plain program resolves to itself.
        let target = effective_target(guest.to_str().ok_or("the guest path must be UTF-8")?, &[]);
        assert_eq!(target, Some(guest.clone()));
        std::fs::remove_file(qemu_path).ok();
        std::fs::remove_file(guest).ok();
        Ok(())
    }

    #[test]
    fn environment_for_selects_the_shim_by_elf_class() -> Result<(), String> {
        let mut backend = InterceptCapture::new()
            .map_err(|e| format!("the interception backend must bind: {e}"))?;
        let guest32 = write_temp("guest32b", &[0x7f, b'E', b'L', b'F', 1, 0, 0, 0])?;
        let result =
            backend.environment_for(guest32.to_str().ok_or("the guest path must be UTF-8")?, &[]);
        if shim32_path().is_empty() {
            assert!(result.is_err());
        } else {
            let env = result?;
            assert_eq!(env[0].1, shim32_path());
        }
        let guest64 = write_temp("guest64b", &[0x7f, b'E', b'L', b'F', 2, 0, 0, 0])?;
        let env = backend
            .environment_for(guest64.to_str().ok_or("the guest path must be UTF-8")?, &[])?;
        assert_eq!(env[0].1, shim_path());
        std::fs::remove_file(guest32).ok();
        std::fs::remove_file(guest64).ok();
        backend.shutdown().ok();
        Ok(())
    }
}
