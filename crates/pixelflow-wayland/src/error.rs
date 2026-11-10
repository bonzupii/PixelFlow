//! Typed Wayland capture errors. The portal and `PipeWire` failures
//! keep their own variants so callers can tell a missing portal from
//! a failed stream without parsing a message.

use pixelflow_capture::SourceKind;
use std::fmt;

/// What can go wrong in the Wayland capture backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaylandError {
    /// A D-Bus or portal call failed. The message is the portal error.
    Portal(String),
    /// The requested source kind is not offered by this compositor.
    SourceUnavailable {
        /// The source kind that was requested.
        requested: SourceKind,
        /// The source kinds the portal does offer.
        available: Vec<SourceKind>,
    },
    /// The portal returned a stream of a different source kind
    /// than the one requested.
    SourceMismatch {
        /// The source kind that was requested.
        requested: SourceKind,
        /// The source kind the portal actually returned.
        actual: SourceKind,
    },
    /// The portal started the session but returned no stream.
    NoStream,
    /// A `PipeWire` call failed. The message is the `PipeWire` error.
    PipeWire(String),
    /// The negotiated pixel format is one this backend does not convert.
    UnsupportedFormat(String),
    /// A frame arrived malformed: bad size, bad stride, short buffer.
    InvalidFrame(String),
    /// The stream ended: the source closed or the session was revoked.
    Disconnected,
}

impl fmt::Display for WaylandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaylandError::Portal(msg) => write!(f, "portal error: {msg}"),
            WaylandError::SourceUnavailable {
                requested,
                available,
            } => {
                let offered: Vec<String> = available.iter().map(ToString::to_string).collect();
                write!(
                    f,
                    "{requested} capture is not offered; portal offers: {}",
                    offered.join(", ")
                )
            }
            WaylandError::SourceMismatch { requested, actual } => {
                write!(
                    f,
                    "requested {requested} capture but the portal returned a {actual} stream"
                )
            }
            WaylandError::NoStream => write!(f, "portal returned no stream"),
            WaylandError::PipeWire(msg) => write!(f, "pipewire error: {msg}"),
            WaylandError::UnsupportedFormat(msg) => {
                write!(f, "unsupported capture format: {msg}")
            }
            WaylandError::InvalidFrame(msg) => write!(f, "invalid captured frame: {msg}"),
            WaylandError::Disconnected => write!(f, "capture stream disconnected"),
        }
    }
}

impl std::error::Error for WaylandError {}

impl From<WaylandError> for pixelflow_capture::CaptureError {
    fn from(e: WaylandError) -> pixelflow_capture::CaptureError {
        use pixelflow_capture::CaptureError;
        match e {
            WaylandError::Portal(msg) => CaptureError::SessionFailed(msg),
            WaylandError::SourceUnavailable { .. }
            | WaylandError::SourceMismatch { .. }
            | WaylandError::NoStream => CaptureError::SessionFailed(e.to_string()),
            WaylandError::PipeWire(msg) | WaylandError::InvalidFrame(msg) => {
                CaptureError::StreamFailed(msg)
            }
            WaylandError::UnsupportedFormat(msg) => CaptureError::UnsupportedFormat(msg),
            WaylandError::Disconnected => CaptureError::Disconnected,
        }
    }
}
