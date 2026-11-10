//! Frame representation: one image, its dimensions, its pixel
//! format, its row stride, capture metadata, and its pixel data
//! in row-major order, top row first.

use crate::format::PixelFormat;
use std::borrow::Cow;
use std::time::Instant;

/// One frame of image data. The unit every subsystem passes
/// around: capture produces frames, processing transforms them,
/// presentation displays them. The data buffer is owned and
/// contiguous across rows only up to the stride: each row is
/// `stride` bytes long, of which `width * bytes_per_pixel` hold
/// pixels and the rest is padding.
///
/// The fields are private: the constructors check the buffer
/// length and stride against the dimensions, and only the
/// accessors below may read them, so no caller can invalidate
/// those invariants after construction. The whole buffer is
/// available through [`Frame::data`]; pixel bytes without
/// padding come from [`Frame::row`] and [`Frame::pixel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Image width in pixels.
    width: u32,
    /// Image height in pixels.
    height: u32,
    /// Pixel layout of `data`.
    format: PixelFormat,
    /// Byte distance between the start of one row and the start
    /// of the next. Always `>= width * bytes_per_pixel`; equal
    /// to it for tightly packed frames.
    stride: usize,
    /// When the frame was captured, on the monotonic clock.
    /// Processing propagates the input frame's timestamp to its
    /// output, so pacing downstream still refers to the capture.
    timestamp: Instant,
    /// Capture sequence number, assigned by the capture backend,
    /// monotonically increasing per captured frame. Processing
    /// propagates it to its output, like the timestamp.
    sequence: u64,
    /// Row-major pixel data, `height * stride` bytes long.
    data: Vec<u8>,
}

/// A frame whose buffer length does not match its dimensions,
/// format, and stride.
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
    /// Build a tightly packed frame, checking the buffer length
    /// against the dimensions and format. The stride is `width *
    /// bytes_per_pixel`. The timestamp is now and the sequence is
    /// 0; producers that know the real values stamp them with
    /// [`Frame::with_timestamp`] and [`Frame::with_sequence`].
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] when the buffer length does not
    /// equal `width * height * format.bytes_per_pixel()`, or when
    /// the size computation overflows.
    pub fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        data: Vec<u8>,
    ) -> Result<Frame, FrameError> {
        let row_bytes = (width as usize)
            .checked_mul(format.bytes_per_pixel())
            .ok_or_else(|| FrameError {
                message: format!("row size overflows for {width}x{height} {format}"),
            })?;
        let expected = row_bytes
            .checked_mul(height as usize)
            .ok_or_else(|| FrameError {
                message: format!("frame size overflows for {width}x{height} {format}"),
            })?;
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
        Ok(Frame {
            width,
            height,
            format,
            stride: row_bytes,
            timestamp: Instant::now(),
            sequence: 0,
            data,
        })
    }

    /// Build a frame with an explicit row stride, checking the
    /// buffer length against the dimensions, format, and stride.
    /// The stride must be at least `width * bytes_per_pixel`;
    /// the buffer must hold exactly `height * stride` bytes, so
    /// every row the stride implies is present, padding
    /// included. The timestamp is now and the sequence is 0; see
    /// [`Frame::new`] for stamping the real values.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] when the stride is smaller than
    /// one row, when the buffer length does not equal `height *
    /// stride`, or when a size computation overflows.
    pub fn new_with_stride(
        width: u32,
        height: u32,
        format: PixelFormat,
        stride: usize,
        data: Vec<u8>,
    ) -> Result<Frame, FrameError> {
        let row_bytes = (width as usize)
            .checked_mul(format.bytes_per_pixel())
            .ok_or_else(|| FrameError {
                message: format!("row size overflows for {width}x{height} {format}"),
            })?;
        if stride < row_bytes {
            return Err(FrameError {
                message: format!(
                    "stride {stride} is smaller than row size {row_bytes} for {width}x{height} {format}"
                ),
            });
        }
        let expected = stride
            .checked_mul(height as usize)
            .ok_or_else(|| FrameError {
                message: format!(
                    "frame size overflows for {width}x{height} {format} with stride {stride}"
                ),
            })?;
        if data.len() != expected {
            return Err(FrameError {
                message: format!(
                    "buffer is {} bytes, expected {} for {}x{} {} with stride {}",
                    data.len(),
                    expected,
                    width,
                    height,
                    format,
                    stride
                ),
            });
        }
        Ok(Frame {
            width,
            height,
            format,
            stride,
            timestamp: Instant::now(),
            sequence: 0,
            data,
        })
    }

    /// Stamp the capture time. Capture backends call this with
    /// the moment the buffer was received.
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: Instant) -> Frame {
        self.timestamp = timestamp;
        self
    }

    /// Stamp the capture sequence number. Capture backends call
    /// this with the next number from their sequence counter.
    #[must_use]
    pub fn with_sequence(mut self, sequence: u64) -> Frame {
        self.sequence = sequence;
        self
    }

    /// Build a frame filled with one repeated byte value, tightly
    /// packed. The timestamp is now and the sequence is 0.
    #[must_use]
    pub fn filled(width: u32, height: u32, format: PixelFormat, value: u8) -> Frame {
        let len = width as usize * height as usize * format.bytes_per_pixel();
        Frame {
            width,
            height,
            format,
            stride: width as usize * format.bytes_per_pixel(),
            timestamp: Instant::now(),
            sequence: 0,
            data: vec![value; len],
        }
    }

    /// Bytes per pixel, from the format.
    #[must_use]
    pub fn bytes_per_pixel(&self) -> usize {
        self.format.bytes_per_pixel()
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Pixel layout of the data buffer.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Byte distance between the start of one row and the start
    /// of the next. Always `>= width * bytes_per_pixel`.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// When the frame was captured, on the monotonic clock.
    #[must_use]
    pub fn timestamp(&self) -> Instant {
        self.timestamp
    }

    /// Capture sequence number, assigned by the capture backend.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The whole data buffer, `height * stride` bytes, padding
    /// included. The single whole-buffer accessor: callers that
    /// need pixel bytes without padding use [`Frame::row`] or
    /// [`Frame::pixel`] instead.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// True when the rows carry no padding: `stride` equals
    /// `width * bytes_per_pixel`.
    #[must_use]
    pub fn is_tight(&self) -> bool {
        self.stride == self.width as usize * self.bytes_per_pixel()
    }

    /// A tightly packed copy of this frame. Returns the frame
    /// itself, borrowed, when it is already tight, so the common
    /// path pays no copy; otherwise copies the pixel rows into a
    /// fresh tight buffer. The copy keeps the timestamp and the
    /// sequence: it is the same captured moment, repacked.
    #[must_use]
    pub fn packed(&self) -> Cow<'_, Frame> {
        if self.is_tight() {
            Cow::Borrowed(self)
        } else {
            Cow::Owned(self.to_tight())
        }
    }

    /// Copy the pixel rows into a fresh tightly packed frame.
    fn to_tight(&self) -> Frame {
        let mut data =
            Vec::with_capacity(self.width as usize * self.height as usize * self.bytes_per_pixel());
        for y in 0..self.height {
            data.extend_from_slice(self.row(y));
        }
        Frame {
            width: self.width,
            height: self.height,
            format: self.format,
            stride: self.width as usize * self.bytes_per_pixel(),
            timestamp: self.timestamp,
            sequence: self.sequence,
            data,
        }
    }

    /// Byte offset of pixel (x, y) in the data buffer.
    #[must_use]
    pub fn pixel_offset(&self, x: u32, y: u32) -> usize {
        y as usize * self.stride + x as usize * self.bytes_per_pixel()
    }

    /// The channels of pixel (x, y) as a slice.
    ///
    /// # Panics
    ///
    /// Panics when (x, y) is outside the frame.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> &[u8] {
        let off = self.pixel_offset(x, y);
        &self.data[off..off + self.bytes_per_pixel()]
    }

    /// The channels of pixel (x, y) as a mutable slice.
    ///
    /// # Panics
    ///
    /// Panics when (x, y) is outside the frame.
    pub fn pixel_mut(&mut self, x: u32, y: u32) -> &mut [u8] {
        let off = self.pixel_offset(x, y);
        let bpp = self.bytes_per_pixel();
        &mut self.data[off..off + bpp]
    }

    /// The pixel bytes of row y, padding excluded. The slice is
    /// `width * bytes_per_pixel` long.
    ///
    /// # Panics
    ///
    /// Panics when y is outside the frame.
    #[must_use]
    pub fn row(&self, y: u32) -> &[u8] {
        let start = y as usize * self.stride;
        let end = start + self.width as usize * self.bytes_per_pixel();
        &self.data[start..end]
    }

    /// The pixel bytes of row y as a mutable slice, padding
    /// excluded.
    ///
    /// # Panics
    ///
    /// Panics when y is outside the frame.
    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let start = y as usize * self.stride;
        let end = start + self.width as usize * self.bytes_per_pixel();
        &mut self.data[start..end]
    }

    /// Convert to RGB8, dropping alpha and expanding grey. The
    /// processing pipeline works on RGB8; this is the single
    /// place that conversion lives. The result is tightly
    /// packed and keeps this frame's timestamp and sequence.
    #[must_use]
    pub fn to_rgb8(&self) -> Frame {
        match self.format {
            PixelFormat::Rgb8 => self.clone(),
            PixelFormat::Rgba8 => {
                let mut data = Vec::with_capacity(self.width as usize * self.height as usize * 3);
                for y in 0..self.height {
                    for px in self.row(y).as_chunks::<4>().0 {
                        data.extend_from_slice(&px[..3]);
                    }
                }
                Frame {
                    width: self.width,
                    height: self.height,
                    format: PixelFormat::Rgb8,
                    stride: self.width as usize * 3,
                    timestamp: self.timestamp,
                    sequence: self.sequence,
                    data,
                }
            }
            PixelFormat::Gray8 => {
                let mut data = Vec::with_capacity(self.width as usize * self.height as usize * 3);
                for y in 0..self.height {
                    for &g in self.row(y) {
                        data.extend_from_slice(&[g, g, g]);
                    }
                }
                Frame {
                    width: self.width,
                    height: self.height,
                    format: PixelFormat::Rgb8,
                    stride: self.width as usize * 3,
                    timestamp: self.timestamp,
                    sequence: self.sequence,
                    data,
                }
            }
        }
    }
}

/// Opaque handle to a GPU-backed image resource, allocated by a
/// presentation or capture backend. Declared here so the CPU/GPU
/// boundary has a name; no backend produces one yet, and no node
/// consumes one. A GPU-backed resource travels as this type and
/// is never smuggled inside a [`Frame`]'s data buffer. The first
/// backend that produces GPU resources adds the constructor
/// alongside its allocation code; a later step introduces a
/// `FrameData` enum so a frame can be CPU-backed or GPU-backed,
/// and the graph executor routes each frame only to nodes that
/// accept its backing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GpuResource {
    /// Backend-assigned identifier. Private: only the backend
    /// that allocated the resource may mint one.
    id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_wrong_buffer_length() {
        assert!(Frame::new(2, 2, PixelFormat::Rgb8, vec![0; 11]).is_err());
        assert!(Frame::new(2, 2, PixelFormat::Rgb8, vec![0; 12]).is_ok());
    }

    #[test]
    fn new_is_tight_with_zero_sequence() -> Result<(), String> {
        let frame = Frame::new(2, 1, PixelFormat::Rgb8, vec![0; 6]).map_err(|e| e.to_string())?;
        assert_eq!(frame.stride(), 6);
        assert!(frame.is_tight());
        assert_eq!(frame.sequence(), 0);
        Ok(())
    }

    #[test]
    fn new_with_stride_rejects_short_stride_and_wrong_length() {
        assert!(Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 5, vec![0; 5]).is_err());
        assert!(Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 8, vec![0; 6]).is_err());
        assert!(Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 8, vec![0; 8]).is_ok());
    }

    #[test]
    fn pixel_and_row_honor_stride() -> Result<(), String> {
        let mut frame = Frame::new_with_stride(2, 2, PixelFormat::Rgb8, 8, vec![0; 16])
            .map_err(|e| e.to_string())?;
        frame.pixel_mut(1, 1).copy_from_slice(&[10, 20, 30]);
        assert_eq!(frame.pixel(1, 1), &[10, 20, 30]);
        assert_eq!(frame.pixel(0, 0), &[0, 0, 0]);
        assert_eq!(frame.row(1), &[0, 0, 0, 10, 20, 30, 0, 0][..6]);
        frame.row_mut(0).copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(frame.pixel(1, 0), &[4, 5, 6]);
        assert_eq!(frame.data()[6], 0);
        assert_eq!(frame.data()[7], 0);
        Ok(())
    }

    #[test]
    fn packed_borrows_when_tight_and_copies_when_strided() -> Result<(), String> {
        let tight = Frame::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6])
            .map_err(|e| e.to_string())?;
        assert!(matches!(tight.packed(), Cow::Borrowed(_)));
        let strided =
            Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 8, vec![1, 2, 3, 4, 5, 6, 0, 0])
                .map_err(|e| e.to_string())?;
        let packed = strided.packed();
        assert!(matches!(packed, Cow::Owned(_)));
        assert!(packed.is_tight());
        assert_eq!(packed.data(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(packed.sequence(), strided.sequence());
        Ok(())
    }

    #[test]
    fn metadata_stamps_and_propagates() -> Result<(), String> {
        let stamp = Instant::now();
        let frame = Frame::new(1, 1, PixelFormat::Rgb8, vec![0; 3])
            .map_err(|e| e.to_string())?
            .with_timestamp(stamp)
            .with_sequence(41);
        assert_eq!(frame.timestamp(), stamp);
        assert_eq!(frame.sequence(), 41);
        assert_eq!(frame.to_rgb8().sequence(), 41);
        assert_eq!(frame.to_rgb8().timestamp(), stamp);
        Ok(())
    }

    #[test]
    fn pixel_round_trip() {
        let mut f = Frame::filled(2, 1, PixelFormat::Rgb8, 0);
        f.pixel_mut(1, 0).copy_from_slice(&[10, 20, 30]);
        assert_eq!(f.pixel(1, 0), &[10, 20, 30]);
        assert_eq!(f.pixel(0, 0), &[0, 0, 0]);
    }

    #[test]
    fn to_rgb8_drops_alpha_and_expands_grey() -> Result<(), String> {
        let rgba =
            Frame::new(1, 1, PixelFormat::Rgba8, vec![1, 2, 3, 4]).map_err(|e| e.to_string())?;
        assert_eq!(rgba.to_rgb8().data(), vec![1, 2, 3]);
        let grey = Frame::new(1, 1, PixelFormat::Gray8, vec![7]).map_err(|e| e.to_string())?;
        assert_eq!(grey.to_rgb8().data(), vec![7, 7, 7]);
        Ok(())
    }

    #[test]
    fn to_rgb8_ignores_row_padding() -> Result<(), String> {
        let rgba = Frame::new_with_stride(
            1,
            2,
            PixelFormat::Rgba8,
            8,
            vec![1, 2, 3, 4, 9, 9, 9, 9, 5, 6, 7, 8, 9, 9, 9, 9],
        )
        .map_err(|e| e.to_string())?;
        assert_eq!(rgba.to_rgb8().data(), vec![1, 2, 3, 5, 6, 7]);
        Ok(())
    }
}
