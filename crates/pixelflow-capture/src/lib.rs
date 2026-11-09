//! Capture backends: the trait every capture implementation satisfies,
//! the configuration that selects a source, and the typed errors a
//! backend reports. No platform code lives here; platform crates
//! implement the trait and the CLI programs against it.

pub mod backend;
pub mod config;
pub mod error;

pub use backend::CaptureBackend;
pub use config::{CaptureConfig, SourceKind};
pub use error::CaptureError;
