//! Conversion from `PipeWire` packed video buffers to Frames.
//!
//! The portal negotiates one of the packed RGB formats in
//! `SUPPORTED_FORMATS`; planar formats are refused at negotiation and
//! never reach this module. Conversion drops the alpha channel and
//! writes RGB pixels at each row start, keeping the chunk stride:
//! the frame's rows are `stride` bytes apart with the row padding
//! zeroed, so the layout the compositor produced survives into the
//! pipeline instead of being compacted away here.

use crate::WaylandError;
use pipewire::spa::param::video::VideoFormat;
use pixelflow_core::{Frame, PixelFormat};

/// Packed formats this backend converts, in negotiation preference
/// order. Four-byte formats first: compositors emit them natively and
/// the alpha channel drops cleanly.
pub const SUPPORTED_FORMATS: &[VideoFormat] = &[
    VideoFormat::BGRx,
    VideoFormat::RGBx,
    VideoFormat::BGRA,
    VideoFormat::RGBA,
    VideoFormat::BGR,
    VideoFormat::RGB,
];

/// True when the negotiated format is one this backend converts.
#[must_use]
pub fn is_supported(format: VideoFormat) -> bool {
    SUPPORTED_FORMATS.contains(&format)
}

/// Channel order of one pixel in the buffer: (red, green, blue,
/// bytes per pixel). The fourth byte of a four-byte format is alpha
/// or padding and is dropped.
fn channel_map(format: VideoFormat) -> Option<(usize, usize, usize, usize)> {
    match format {
        VideoFormat::BGRx | VideoFormat::BGRA => Some((2, 1, 0, 4)),
        VideoFormat::RGBx | VideoFormat::RGBA => Some((0, 1, 2, 4)),
        VideoFormat::BGR => Some((2, 1, 0, 3)),
        VideoFormat::RGB => Some((0, 1, 2, 3)),
        _ => None,
    }
}

/// Build an RGB8 frame from one packed plane, keeping the chunk
/// stride.
///
/// `data` is the whole mapped buffer, `offset` is the chunk offset
/// into it, and `stride` is the byte distance between row starts,
/// which may exceed `width * bytes_per_pixel` when rows are padded.
/// The buffer must hold every row the stride implies; a short buffer
/// is an error, never a truncated read. The returned frame's stride
/// is the chunk stride: each row holds `width * 3` RGB bytes
/// followed by zeroed padding, so stride-aware consumers read it
/// directly and tight-only algorithms pack it at their entry point.
///
/// # Errors
///
/// Returns `UnsupportedFormat` for a format outside
/// `SUPPORTED_FORMATS`, and `InvalidFrame` for a zero size, a
/// negative or too-small stride, or a buffer too short for the rows
/// the stride implies.
///
/// # Panics
///
/// Never panics: the length check above guarantees every index the
/// copy loop reads is inside `data`.
pub fn packed_to_frame(
    format: VideoFormat,
    width: u32,
    height: u32,
    stride: i32,
    offset: u32,
    data: &[u8],
) -> Result<Frame, WaylandError> {
    let Some((r, g, b, bpp)) = channel_map(format) else {
        return Err(WaylandError::UnsupportedFormat(format!("{format:?}")));
    };
    if width == 0 || height == 0 {
        return Err(WaylandError::InvalidFrame("zero-size frame".to_string()));
    }
    let stride = usize::try_from(stride)
        .map_err(|_| WaylandError::InvalidFrame(format!("negative stride {stride}")))?;
    let row_bytes = width as usize * bpp;
    if stride < row_bytes {
        return Err(WaylandError::InvalidFrame(format!(
            "stride {stride} is smaller than row size {row_bytes}"
        )));
    }
    let start = offset as usize;
    let needed = start
        .checked_add(
            stride
                .saturating_mul(height as usize - 1)
                .saturating_add(row_bytes),
        )
        .ok_or_else(|| WaylandError::InvalidFrame("frame size overflows".to_string()))?;
    if data.len() < needed {
        let len = data.len();
        return Err(WaylandError::InvalidFrame(format!(
            "buffer is {len} bytes, need {needed} for {width}x{height}"
        )));
    }
    let mut out = vec![0u8; height as usize * stride];
    for y in 0..height as usize {
        let row = start + y * stride;
        let out_row = y * stride;
        for x in 0..width as usize {
            let px = row + x * bpp;
            out[out_row + x * 3] = data[px + r];
            out[out_row + x * 3 + 1] = data[px + g];
            out[out_row + x * 3 + 2] = data[px + b];
        }
    }
    Frame::new_with_stride(width, height, PixelFormat::Rgb8, stride, out)
        .map_err(|e| WaylandError::InvalidFrame(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_drops_alpha_and_orders_channels() -> Result<(), WaylandError> {
        let data = vec![30, 20, 10, 255, 60, 50, 40, 0];
        let f = packed_to_frame(VideoFormat::BGRA, 2, 1, 8, 0, &data)?;
        assert_eq!(f.stride(), 8);
        assert_eq!(f.row(0), vec![10, 20, 30, 40, 50, 60]);
        assert_eq!(f.data(), vec![10, 20, 30, 40, 50, 60, 0, 0]);
        Ok(())
    }

    #[test]
    fn rgba_keeps_channel_order() -> Result<(), WaylandError> {
        let data = vec![10, 20, 30, 255, 40, 50, 60, 255];
        let f = packed_to_frame(VideoFormat::RGBA, 2, 1, 8, 0, &data)?;
        assert_eq!(f.stride(), 8);
        assert_eq!(f.row(0), vec![10, 20, 30, 40, 50, 60]);
        Ok(())
    }

    #[test]
    fn bgrx_and_rgbx_match_their_alpha_twins() -> Result<(), WaylandError> {
        let data = vec![3, 2, 1, 9, 6, 5, 4, 9];
        assert_eq!(
            packed_to_frame(VideoFormat::BGRx, 2, 1, 8, 0, &data)?.row(0),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            packed_to_frame(VideoFormat::RGBx, 2, 1, 8, 0, &data)?.row(0),
            vec![3, 2, 1, 6, 5, 4]
        );
        Ok(())
    }

    #[test]
    fn three_byte_formats_convert() -> Result<(), WaylandError> {
        let data = vec![30, 20, 10, 60, 50, 40];
        assert_eq!(
            packed_to_frame(VideoFormat::BGR, 2, 1, 6, 0, &data)?.data(),
            vec![10, 20, 30, 40, 50, 60]
        );
        assert_eq!(
            packed_to_frame(VideoFormat::RGB, 2, 1, 6, 0, &data)?.data(),
            vec![30, 20, 10, 60, 50, 40]
        );
        Ok(())
    }

    #[test]
    fn stride_padding_and_offset_are_honored() -> Result<(), WaylandError> {
        // Two rows, 4 bytes of pixels then 4 bytes of padding per
        // row, chunk starting 2 bytes into the buffer.
        let mut data = vec![0, 0];
        data.extend_from_slice(&[10, 20, 30, 255, 0, 0, 0, 0]);
        data.extend_from_slice(&[40, 50, 60, 255, 0, 0, 0, 0]);
        let f = packed_to_frame(VideoFormat::BGRA, 1, 2, 8, 2, &data)?;
        assert_eq!(f.stride(), 8);
        assert_eq!(f.row(0), vec![30, 20, 10]);
        assert_eq!(f.row(1), vec![60, 50, 40]);
        assert_eq!(
            f.data(),
            vec![30, 20, 10, 0, 0, 0, 0, 0, 60, 50, 40, 0, 0, 0, 0, 0]
        );
        Ok(())
    }

    #[test]
    fn short_buffer_is_an_error_not_a_truncation() {
        let data = vec![1, 2, 3, 4];
        assert!(matches!(
            packed_to_frame(VideoFormat::BGRA, 2, 1, 8, 0, &data),
            Err(WaylandError::InvalidFrame(_))
        ));
    }

    #[test]
    fn stride_smaller_than_row_is_an_error() {
        let data = vec![0; 8];
        assert!(matches!(
            packed_to_frame(VideoFormat::BGRA, 2, 1, 4, 0, &data),
            Err(WaylandError::InvalidFrame(_))
        ));
    }

    #[test]
    fn zero_size_and_negative_stride_are_errors() {
        let data = vec![0; 8];
        assert!(matches!(
            packed_to_frame(VideoFormat::BGRA, 0, 1, 8, 0, &data),
            Err(WaylandError::InvalidFrame(_))
        ));
        assert!(matches!(
            packed_to_frame(VideoFormat::BGRA, 2, 1, -8, 0, &data),
            Err(WaylandError::InvalidFrame(_))
        ));
    }

    #[test]
    fn planar_formats_are_unsupported() {
        assert!(!is_supported(VideoFormat::I420));
        assert!(!is_supported(VideoFormat::YUY2));
        assert!(!is_supported(VideoFormat::Unknown));
        for f in SUPPORTED_FORMATS {
            assert!(is_supported(*f));
        }
        let data = vec![0; 8];
        assert!(matches!(
            packed_to_frame(VideoFormat::I420, 2, 1, 8, 0, &data),
            Err(WaylandError::UnsupportedFormat(_))
        ));
    }
}
