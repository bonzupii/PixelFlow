//! Nearest-neighbour upscaling: each source pixel becomes a
//! `scale` by `scale` block of identical pixels. The baseline
//! algorithm (00 in the upscaler experiments) and the reference every
//! other algorithm is judged against.

use pixelflow_core::{Frame, PixelFormat, ProcessError, Processor};

/// Nearest-neighbour upscaling.
pub struct Nearest;

impl Processor for Nearest {
    fn name(&self) -> &'static str {
        "nearest"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale { algorithm: "nearest", scale });
        }
        if frame.format != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "nearest",
                format: frame.format.to_string(),
            });
        }
        let s = scale as usize;
        let (w, h) = (frame.width as usize, frame.height as usize);
        let mut data = vec![0u8; w * s * h * s * 3];
        for y in 0..h {
            for x in 0..w {
                let src = &frame.data[(y * w + x) * 3..(y * w + x) * 3 + 3];
                for dy in 0..s {
                    for dx in 0..s {
                        let dst = ((y * s + dy) * w * s + (x * s + dx)) * 3;
                        data[dst..dst + 3].copy_from_slice(src);
                    }
                }
            }
        }
        Ok(Frame { width: frame.width * scale, height: frame.height * scale, format: PixelFormat::Rgb8, data })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn repeats_each_pixel() {
        let f = Frame::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6]).unwrap();
        let out = Nearest.process(&f, 2).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 2);
        assert_eq!(out.pixel(0, 0), &[1, 2, 3]);
        assert_eq!(out.pixel(1, 0), &[1, 2, 3]);
        assert_eq!(out.pixel(2, 0), &[4, 5, 6]);
        assert_eq!(out.pixel(0, 1), &[1, 2, 3]);
    }
}
