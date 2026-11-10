//! Typed capture errors. Callers match on the variant to decide
//! recovery; the message carries the detail.

use crate::SourceKind;
use std::fmt;

/// What can go wrong while capturing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// No capture backend exists on this machine, or the platform
    /// service it needs is absent. The message names what is missing.
    BackendUnavailable(String),
    /// The capture session could not be created or started.
    SessionFailed(String),
    /// The video stream failed after the session started.
    StreamFailed(String),
    /// The source offers a pixel format no backend converts.
    UnsupportedFormat(String),
    /// The stream ended: the source closed or the session was revoked.
    Disconnected,
    /// The wrapped application exited.
    TargetExited,
    /// The requested source kind is not offered. The caller may
    /// fall back to another source kind.
    SourceUnavailable {
        /// The source kind that was requested.
        requested: SourceKind,
        /// The source kinds the backend does offer.
        available: Vec<SourceKind>,
    },
    /// Source selection was cancelled: the picker was dismissed,
    /// or the backend cancelled the request. Terminal for the
    /// run: falling back to another source would capture
    /// something the selection refused.
    SelectionCancelled(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::BackendUnavailable(msg) => write!(f, "capture unavailable: {msg}"),
            CaptureError::SessionFailed(msg) => write!(f, "capture session failed: {msg}"),
            CaptureError::StreamFailed(msg) => write!(f, "capture stream failed: {msg}"),
            CaptureError::UnsupportedFormat(msg) => write!(f, "unsupported capture format: {msg}"),
            CaptureError::Disconnected => write!(f, "capture stream disconnected"),
            CaptureError::TargetExited => write!(f, "wrapped application exited"),
            CaptureError::SourceUnavailable {
                requested,
                available,
            } => {
                let offered: Vec<String> = available.iter().map(ToString::to_string).collect();
                write!(
                    f,
                    "{requested} capture is not offered; available sources: {}",
                    offered.join(", ")
                )
            }
            CaptureError::SelectionCancelled(msg) => {
                write!(f, "capture selection cancelled: {msg}")
            }
        }
    }
}

impl std::error::Error for CaptureError {}
