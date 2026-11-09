//! Texture upload: the RGB8 frames the pipeline produces become the
//! RGBA8 textures wgpu samples.

use pixelflow_core::Frame;

/// Expand an RGB8 frame to RGBA8 with an opaque alpha channel, for
/// texture upload. The layout is row-major, top row first, matching
/// the frame.
#[must_use]
pub fn rgba8_from_rgb8(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.width as usize * frame.height as usize * 4);
    for px in frame.data.as_chunks::<3>().0 {
        out.push(px[0]);
        out.push(px[1]);
        out.push(px[2]);
        out.push(255);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelflow_core::PixelFormat;

    #[test]
    fn expands_rgb_to_opaque_rgba() -> Result<(), pixelflow_core::FrameError> {
        let frame = Frame::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6])?;
        assert_eq!(rgba8_from_rgb8(&frame), vec![1, 2, 3, 255, 4, 5, 6, 255]);
        Ok(())
    }
}
