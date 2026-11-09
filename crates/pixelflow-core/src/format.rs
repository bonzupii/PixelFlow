//! Pixel format definitions, shared by every `PixelFlow` crate.

use std::fmt;

/// Pixel layout of a [`crate::frame::Frame`]'s data buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8 bits per channel, red, green, blue. 3 bytes per pixel.
    Rgb8,
    /// 8 bits per channel, red, green, blue, alpha. 4 bytes per pixel.
    Rgba8,
    /// 8 bits, one grey channel. 1 byte per pixel.
    Gray8,
}

impl PixelFormat {
    /// Bytes per pixel in this format.
    #[must_use]
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb8 => 3,
            PixelFormat::Rgba8 => 4,
            PixelFormat::Gray8 => 1,
        }
    }

    /// Number of channels in this format.
    #[must_use]
    pub fn channels(self) -> usize {
        self.bytes_per_pixel()
    }

    /// Whether this format carries an alpha channel.
    #[must_use]
    pub fn has_alpha(self) -> bool {
        matches!(self, PixelFormat::Rgba8)
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixelFormat::Rgb8 => write!(f, "rgb8"),
            PixelFormat::Rgba8 => write!(f, "rgba8"),
            PixelFormat::Gray8 => write!(f, "gray8"),
        }
    }
}
