//! Presentation: the fullscreen window that shows processed frames.

pub mod layout;
pub mod presenter;
pub mod texture;

pub use layout::{Rect, fit_rect};
pub use presenter::{PresentError, Presenter, run};
pub use texture::rgba8_from_rgb8;
