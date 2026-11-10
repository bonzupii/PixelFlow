//! Bilinear upscaling: each output pixel is a weighted average
//! of the four nearest input pixels, with edge pixels clamped.
//! The small second baseline beside nearest-neighbour: it
//! exercises the algorithm registry with an implementation that
//! is independent of Contour and Nearest, and it is the worked
//! example in `docs/algorithm-integration.md`.
//!
//! The weights use fixed-point integer arithmetic, so the
//! output is bit-for-bit reproducible: an output pixel at
//! (`ox`, `oy`) samples the input at (`sx`, `sy`) where `sx =
//! (ox + 0.5) / scale - 0.5`, and likewise for `sy`. The
//! sampling position is kept in units of `1 / (2 * scale)`,
//! which makes the floor and the fraction exact integers.

use pixelflow_core::graph::{ProcessorNode, SpatialDependency};
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin, Frame,
    PixelFormat, ProcessError, Processor,
};

/// Bilinear interpolation upscaling.
pub struct Bilinear;

impl Processor for Bilinear {
    fn name(&self) -> &'static str {
        "bilinear"
    }

    fn description(&self) -> &'static str {
        "bilinear interpolation: each output pixel is a weighted average of the four nearest input pixels"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Neighborhood { radius: 1 }
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        Some(3)
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "bilinear",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "bilinear",
                format: frame.format().to_string(),
            });
        }
        let out_width = frame.width().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("bilinear output width exceeds u32 range".into())
        })?;
        let out_height = frame.height().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("bilinear output height exceeds u32 range".into())
        })?;
        let len = (out_width as usize)
            .checked_mul(out_height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| {
                ProcessError::Internal("bilinear output size exceeds addressable memory".into())
            })?;
        let mut data = vec![0u8; len];
        let width = i64::from(frame.width());
        let height = i64::from(frame.height());
        let scale_i = i64::from(scale);
        for oy in 0..out_height {
            let (y0, y1, wy0, wy1) = sample_axis(i64::from(oy), scale_i, height);
            for ox in 0..out_width {
                let (x0, x1, wx0, wx1) = sample_axis(i64::from(ox), scale_i, width);
                let denom = 4 * scale_i * scale_i;
                let dst = (oy as usize * out_width as usize + ox as usize) * 3;
                for channel in 0..3 {
                    let sum = sample(frame, x0, y0, channel) * wx0 * wy0
                        + sample(frame, x1, y0, channel) * wx1 * wy0
                        + sample(frame, x0, y1, channel) * wx0 * wy1
                        + sample(frame, x1, y1, channel) * wx1 * wy1;
                    let rounded = (sum + denom / 2) / denom;
                    data[dst + channel] = u8::try_from(rounded).map_err(|_| {
                        ProcessError::Internal("bilinear sample exceeds the u8 range".into())
                    })?;
                }
            }
        }
        Frame::new(out_width, out_height, PixelFormat::Rgb8, data)
            .map_err(|e| ProcessError::Internal(e.to_string()))
            .map(|out| {
                out.with_timestamp(frame.timestamp())
                    .with_sequence(frame.sequence())
            })
    }
}

/// One sampling axis: the two clamped input positions and their
/// weights, in units of `1 / (2 * scale)`. The weights sum to
/// `2 * scale`; the caller multiplies the two axes' weights, so
/// the combined denominator is `(2 * scale)^2`.
fn sample_axis(position: i64, scale: i64, extent: i64) -> (i64, i64, i64, i64) {
    let units = 2 * scale;
    let numerator = 2 * position + 1 - scale;
    let base = numerator.div_euclid(units);
    let fraction = numerator.rem_euclid(units);
    let first = base.clamp(0, extent - 1);
    let second = (base + 1).clamp(0, extent - 1);
    (first, second, units - fraction, fraction)
}

/// One channel of one input pixel, as a weight-ready integer.
fn sample(frame: &Frame, x: i64, y: i64, channel: usize) -> i64 {
    let Ok(x) = u32::try_from(x) else {
        return 0;
    };
    let Ok(y) = u32::try_from(y) else {
        return 0;
    };
    i64::from(frame.pixel(x, y)[channel])
}

impl Bilinear {
    /// Wrap this algorithm as a graph node running at `scale`.
    /// The node declares a radius-1 neighborhood: each output
    /// pixel reads the four input pixels around its sampling
    /// position.
    #[must_use]
    pub fn node(scale: u32) -> ProcessorNode<Self> {
        ProcessorNode::new(Self, scale, SpatialDependency::Neighborhood { radius: 1 })
    }
}

/// The registry entry for Bilinear: a built-in, CPU-only
/// algorithm with no parameters.
pub struct BilinearAlgorithm;

impl AlgorithmFactory for BilinearAlgorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        let mut descriptor =
            AlgorithmDescriptor::from_processor(&Bilinear, AlgorithmOrigin::BuiltIn);
        descriptor.display_name = "Bilinear";
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        Bilinear.validate_config(config)?;
        Ok(Box::new(Bilinear))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_image_is_preserved_exactly() -> Result<(), String> {
        let frame = Frame::new(3, 2, PixelFormat::Rgb8, [200, 30, 90].repeat(6))
            .map_err(|e| e.to_string())?;
        for scale in [2, 4] {
            let out = Bilinear.process(&frame, scale).map_err(|e| e.to_string())?;
            assert_eq!((out.width(), out.height()), (3 * scale, 2 * scale));
            assert!(
                out.data()
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .all(|px| *px == [200, 30, 90])
            );
        }
        Ok(())
    }

    #[test]
    fn two_pixel_gradient_interpolates() -> Result<(), String> {
        let frame = Frame::new(2, 1, PixelFormat::Rgb8, vec![0, 0, 0, 255, 255, 255])
            .map_err(|e| e.to_string())?;
        let out = Bilinear.process(&frame, 2).map_err(|e| e.to_string())?;
        assert_eq!((out.width(), out.height()), (4, 2));
        assert_eq!(out.pixel(0, 0), &[0, 0, 0]);
        assert_eq!(out.pixel(1, 0), &[64, 64, 64]);
        assert_eq!(out.pixel(2, 0), &[191, 191, 191]);
        assert_eq!(out.pixel(3, 0), &[255, 255, 255]);
        assert_eq!(out.pixel(0, 1), &[0, 0, 0]);
        Ok(())
    }

    #[test]
    fn strided_input_matches_tight_input() -> Result<(), String> {
        let tight = Frame::new(2, 1, PixelFormat::Rgb8, vec![0, 0, 0, 255, 255, 255])
            .map_err(|e| e.to_string())?;
        let strided = Frame::new_with_stride(
            2,
            1,
            PixelFormat::Rgb8,
            8,
            vec![0, 0, 0, 255, 255, 255, 0, 0],
        )
        .map_err(|e| e.to_string())?
        .with_sequence(7);
        let from_tight = Bilinear.process(&tight, 2).map_err(|e| e.to_string())?;
        let from_strided = Bilinear.process(&strided, 2).map_err(|e| e.to_string())?;
        assert_eq!(from_strided.data(), from_tight.data());
        assert_eq!(from_strided.sequence(), 7);
        assert_eq!(from_strided.timestamp(), strided.timestamp());
        Ok(())
    }

    #[test]
    fn rejects_unsupported_scale_and_format() -> Result<(), String> {
        let frame = Frame::new(1, 1, PixelFormat::Rgb8, vec![0; 3]).map_err(|e| e.to_string())?;
        assert!(Bilinear.process(&frame, 3).is_err());
        let grey = Frame::new(1, 1, PixelFormat::Gray8, vec![0]).map_err(|e| e.to_string())?;
        assert!(Bilinear.process(&grey, 2).is_err());
        Ok(())
    }
}
