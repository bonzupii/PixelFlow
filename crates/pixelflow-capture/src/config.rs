//! Capture configuration: which source a backend records.

use std::fmt;

/// What a capture backend records: one window, or a whole monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// One application window, picked through the compositor.
    Window,
    /// One whole monitor.
    Monitor,
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceKind::Window => write!(f, "window"),
            SourceKind::Monitor => write!(f, "monitor"),
        }
    }
}

/// Configuration for one capture session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Which source to record. Window is the default; monitor is the
    /// fallback when window capture is unavailable.
    pub source: SourceKind,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            source: SourceKind::Window,
        }
    }
}

impl CaptureConfig {
    /// Configuration for the given source kind.
    #[must_use]
    pub fn new(source: SourceKind) -> CaptureConfig {
        CaptureConfig { source }
    }
}
