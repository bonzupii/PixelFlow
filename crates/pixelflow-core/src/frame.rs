//! Frame representation: one image, its dimensions, its pixel format,
//! and its pixel data in row-major order, top row first.

use crate::format::PixelFormat;

/// One frame of image data. The unit every subsystem passes around:
/// capture produces frames, processing transforms them, presentation
/// displays them. The data buffer is owned and contiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Pixel layout of `data`.
    pub format: PixelFormat,
    /// Row-major pixel data, `width * height * format.bytes_per_pixel()`
    /// bytes long.
    pub data: Vec<u8>,
}

/// A frame whose buffer length does not match its dimensions and format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    /// Human-readable description of the mismatch.
    pub message: String,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid frame: {}", self.message)
    }
}

impl std::error::Error for FrameError {}

impl Frame {
    /// Build a frame, checking the buffer length against the dimensions
    /// and format. Returns an error instead of a short or long buffer.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] when the buffer length does not equal
    /// `width * height * format.bytes_per_pixel()`.
    pub fn new(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Frame, FrameError> {
        let expected = width as usize * height as usize * format.bytes_per_pixel();
        if data.len() != expected {
            return Err(FrameError {
                message: format!(
                    "buffer is {} bytes, expected {} for {}x{} {}",
                    data.len(),
                    expected,
                    width,
                    height,
                    format
                ),
            });
        }
        Ok(Frame { width, height, format, data })
    }

    /// Build a frame filled with one repeated byte value.
    #[must_use]
    pub fn filled(width: u32, height: u32, format: PixelFormat, value: u8) -> Frame {
        let len = width as usize * height as usize * format.bytes_per_pixel();
        Frame { width, height, format, data: vec![value; len] }
    }

    /// Bytes per pixel, from the format.
    #[must_use]
    pub fn bytes_per_pixel(&self) -> usize {
        self.format.bytes_per_pixel()
    }

    /// Byte offset of pixel (x, y) in the data buffer.
    #[must_use]
    pub fn pixel_offset(&self, x: u32, y: u32) -> usize {
        (y as usize * self.width as usize + x as usize) * self.bytes_per_pixel()
    }

    /// The channels of pixel (x, y) as a slice.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> &[u8] {
        let off = self.pixel_offset(x, y);
        &self.data[off..off + self.bytes_per_pixel()]
    }

    /// The channels of pixel (x, y) as a mutable slice.
    pub fn pixel_mut(&mut self, x: u32, y: u32) -> &mut [u8] {
        let off = self.pixel_offset(x, y);
        let bpp = self.bytes_per_pixel();
        &mut self.data[off..off + bpp]
    }

    /// Convert to RGB8, dropping alpha and expanding grey. The
    /// processing pipeline works on RGB8; this is the single place
    /// that conversion lives.
    #[must_use]
    pub fn to_rgb8(&self) -> Frame {
        match self.format {
            PixelFormat::Rgb8 => self.clone(),
            PixelFormat::Rgba8 => {
                let mut data = Vec::with_capacity(self.width as usize * self.height as usize * 3);
                for px in self.data.as_chunks::<4>().0 {
                    data.extend_from_slice(&px[..3]);
                }
                Frame { width: self.width, height: self.height, format: PixelFormat::Rgb8, data }
            }
            PixelFormat::Gray8 => {
                let mut data = Vec::with_capacity(self.width as usize * self.height as usize * 3);
                for &g in &self.data {
                    data.extend_from_slice(&[g, g, g]);
                }
                Frame { width: self.width, height: self.height, format: PixelFormat::Rgb8, data }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_wrong_buffer_length() {
        assert!(Frame::new(2, 2, PixelFormat::Rgb8, vec![0; 11]).is_err());
        assert!(Frame::new(2, 2, PixelFormat::Rgb8, vec![0; 12]).is_ok());
    }

    #[test]
    fn pixel_round_trip() {
        let mut f = Frame::filled(2, 1, PixelFormat::Rgb8, 0);
        f.pixel_mut(1, 0).copy_from_slice(&[10, 20, 30]);
        assert_eq!(f.pixel(1, 0), &[10, 20, 30]);
        assert_eq!(f.pixel(0, 0), &[0, 0, 0]);
    }

    #[test]
    fn to_rgb8_drops_alpha_and_expands_grey() {
        let rgba = Frame::new(1, 1, PixelFormat::Rgba8, vec![1, 2, 3, 4]).unwrap();
        assert_eq!(rgba.to_rgb8().data, vec![1, 2, 3]);
        let grey = Frame::new(1, 1, PixelFormat::Gray8, vec![7]).unwrap();
        assert_eq!(grey.to_rgb8().data, vec![7, 7, 7]);
    }
}
