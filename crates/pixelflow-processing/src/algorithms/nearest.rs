//! Nearest-neighbour upscaling: each source pixel becomes a
//! `scale` by `scale` block of identical pixels. The baseline
//! algorithm (00 in the upscaler experiments) and the reference every
//! other algorithm is judged against.

use pixelflow_core::graph::{ProcessorNode, SpatialDependency};
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin, Frame,
    PixelFormat, ProcessError, Processor,
};

/// Nearest-neighbour upscaling.
pub struct Nearest;

impl Processor for Nearest {
    fn name(&self) -> &'static str {
        "nearest"
    }

    fn description(&self) -> &'static str {
        "nearest-neighbour baseline: each source pixel becomes a scale by scale block of identical pixels"
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Local
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        Some(3)
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    /// Nearest-neighbour scaling is tile-safe: each output pixel
    /// copies one input pixel, so a cropped run produces exactly
    /// the corresponding crop of the whole-frame output.
    fn tile_safe(&self) -> bool {
        true
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "nearest",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "nearest",
                format: frame.format().to_string(),
            });
        }
        let s = scale as usize;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let out_width = frame.width().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("nearest output width exceeds u32 range".into())
        })?;
        let out_height = frame.height().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("nearest output height exceeds u32 range".into())
        })?;
        let mut data = vec![0u8; w * s * h * s * 3];
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                let src = frame.pixel(x, y);
                for dy in 0..s {
                    for dx in 0..s {
                        let dst = ((y as usize * s + dy) * w * s + (x as usize * s + dx)) * 3;
                        data[dst..dst + 3].copy_from_slice(src);
                    }
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

impl Nearest {
    /// Wrap this algorithm as a graph node running at `scale`.
    /// The node declares [`SpatialDependency::Local`]: each
    /// output pixel copies exactly one input pixel.
    #[must_use]
    pub fn node(scale: u32) -> ProcessorNode<Self> {
        ProcessorNode::new(Self, scale, SpatialDependency::Local)
    }
}

/// The registry entry for Nearest: a built-in, CPU-only,
/// single-stage algorithm with no parameters.
pub struct NearestAlgorithm;

impl AlgorithmFactory for NearestAlgorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        let mut descriptor =
            AlgorithmDescriptor::from_processor(&Nearest, AlgorithmOrigin::BuiltIn);
        descriptor.display_name = "Nearest";
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        Nearest.validate_config(config)?;
        Ok(Box::new(Nearest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_each_pixel() -> Result<(), String> {
        let f = Frame::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6])
            .map_err(|e| e.to_string())?;
        let out = Nearest.process(&f, 2).map_err(|e| e.to_string())?;
        assert_eq!(out.width(), 4);
        assert_eq!(out.height(), 2);
        assert_eq!(out.pixel(0, 0), &[1, 2, 3]);
        assert_eq!(out.pixel(1, 0), &[1, 2, 3]);
        assert_eq!(out.pixel(2, 0), &[4, 5, 6]);
        assert_eq!(out.pixel(0, 1), &[1, 2, 3]);
        Ok(())
    }

    #[test]
    fn strided_input_matches_tight_input() -> Result<(), String> {
        let tight = Frame::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6])
            .map_err(|e| e.to_string())?;
        let strided =
            Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 8, vec![1, 2, 3, 4, 5, 6, 0, 0])
                .map_err(|e| e.to_string())?
                .with_sequence(7);
        let from_tight = Nearest.process(&tight, 2).map_err(|e| e.to_string())?;
        let from_strided = Nearest.process(&strided, 2).map_err(|e| e.to_string())?;
        assert_eq!(from_strided.data(), from_tight.data());
        assert_eq!(from_strided.sequence(), 7);
        assert_eq!(from_strided.timestamp(), strided.timestamp());
        Ok(())
    }
}
