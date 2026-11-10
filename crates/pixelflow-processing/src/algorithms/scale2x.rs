//! Scale2x (`AdvMAME2x`) upscaling, ported from the reference
//! `st_scale2x` (`pixelflow_scalers.rs`).
//!
//! The reference is a stage: one 2x pass over a 3x3
//! neighbourhood per source pixel, edge-clamped. Each source
//! pixel becomes a 2x2 block. The block starts as four copies
//! of the centre pixel `E`, and a corner takes its neighbour's
//! colour only when the two orthogonal neighbours on that side
//! agree and the opposite pair does not: with `B` above, `D`
//! left, `F` right, `H` below, the rule applies when `B != H`
//! and `D != F`, and then the top-left corner takes `D` when
//! `D == B`, the top-right takes `F` when `B == F`, the
//! bottom-left takes `D` when `D == H`, and the bottom-right
//! takes `F` when `H == F`. Every output pixel is a copy of one
//! of the five pixels the rule reads, so the algorithm never
//! invents a colour: a flat input is preserved exactly.
//!
//! The reference registry entry is `Scale2x => i("scale2x",
//! &[2], true, 16, false, Imp::Stage(st_scale2x))`: native
//! factor 2, rerunnable, cap 16, stage implementation. The
//! repo port supports scales 2 and 4 only: 4x is two 2x passes,
//! the reference's own chain (`choose_filters` with `rerun`
//! picks `[2, 2]` for a factor of 4), the second pass running
//! on the first pass's output, so a 4x corner is rounded by
//! the stage twice, exactly as the reference does it. The
//! reference runs the stage on packed RGBA; repo frames are
//! RGB8, so the alpha channel is absent rather than carried,
//! and the RGB channels are unaffected by it. The reference
//! reads its neighbourhood through a clamped padded plane;
//! the port clamps the same five reads directly, so the
//! border behaviour is identical: an out-of-frame neighbour
//! reads the edge pixel.

use pixelflow_core::graph::{ProcessorNode, SpatialDependency};
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin, Frame,
    PixelFormat, ProcessError, Processor, StrideSupport,
};

/// Scale2x (`AdvMAME2x`) upscaling.
pub struct Scale2x;

impl Processor for Scale2x {
    fn name(&self) -> &'static str {
        "scale2x"
    }

    fn description(&self) -> &'static str {
        "scale2x (AdvMAME2x): a corner takes its neighbour's colour when the two orthogonal neighbours on that side agree and the opposite pair does not"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Neighborhood { radius: 1 }
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        // Output 3 bytes, plus at 4x the intermediate 2x
        // frame: 3 bytes per output pixel divided by 4, 0.75.
        // Four bounds the total at both scales.
        Some(4)
    }

    fn stride_support(&self) -> StrideSupport {
        // The pass indexes a tightly packed buffer directly;
        // the input is packed at the entry point below. The
        // declaration lets the future graph executor insert the
        // pack instead.
        StrideSupport::TightOnly
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "scale2x",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "scale2x",
                format: frame.format().to_string(),
            });
        }
        let frame = frame.packed();
        let frame = &*frame;
        let out_width = frame.width().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("scale2x output width exceeds u32 range".into())
        })?;
        let out_height = frame.height().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("scale2x output height exceeds u32 range".into())
        })?;
        // An empty frame has no pixels to scale; return the
        // empty output directly.
        if frame.width() == 0 || frame.height() == 0 {
            return Frame::new(out_width, out_height, PixelFormat::Rgb8, Vec::new())
                .map_err(|e| ProcessError::Internal(e.to_string()))
                .map(|out| {
                    out.with_timestamp(frame.timestamp())
                        .with_sequence(frame.sequence())
                });
        }
        // The output byte length fits in one allocation, so the
        // pass buffers below cannot overflow their lengths:
        // the output is at most `isize::MAX / 3` pixels, and
        // the 4x intermediate is a quarter of that.
        let output_pixels = u64::from(out_width) * u64::from(out_height);
        let max_alloc = u64::try_from(isize::MAX).unwrap_or(u64::MAX);
        if output_pixels > max_alloc / 3 {
            return Err(ProcessError::SizeOverflow {
                algorithm: "scale2x",
                stage: "output",
                output_pixels,
            });
        }
        let width = frame.width();
        let height = frame.height();
        let data = if scale == 2 {
            stage_pass(frame.data(), width, height)
        } else {
            let mid = stage_pass(frame.data(), width, height);
            stage_pass(&mid, width * 2, height * 2)
        };
        Frame::new(out_width, out_height, PixelFormat::Rgb8, data)
            .map_err(|e| ProcessError::Internal(e.to_string()))
            .map(|out| {
                out.with_timestamp(frame.timestamp())
                    .with_sequence(frame.sequence())
            })
    }
}

impl Scale2x {
    /// Wrap this algorithm as a graph node running at `scale`.
    /// The node declares a radius-1 neighborhood: each output
    /// pixel copies one of the five pixels in the 3x3
    /// neighbourhood the rule reads.
    #[must_use]
    pub fn node(scale: u32) -> ProcessorNode<Self> {
        ProcessorNode::new(Self, scale, SpatialDependency::Neighborhood { radius: 1 })
    }
}

/// The registry entry for Scale2x: a built-in, CPU-only
/// algorithm with no parameters.
pub struct Scale2xAlgorithm;

impl AlgorithmFactory for Scale2xAlgorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        let mut descriptor =
            AlgorithmDescriptor::from_processor(&Scale2x, AlgorithmOrigin::BuiltIn);
        descriptor.display_name = "Scale2x";
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        Scale2x.validate_config(config)?;
        Ok(Box::new(Scale2x))
    }
}

/// One 2x pass of the reference `st_scale2x`, on RGB triples:
/// each source pixel becomes a 2x2 block, with a corner taking
/// its neighbour's colour under the rule in the module docs.
/// `data` is tightly packed RGB8, `width` and `height`
/// nonzero.
fn stage_pass(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let width_us = width as usize;
    let height_us = height as usize;
    let out_width_us = width_us * 2;
    let mut out = vec![0u8; out_width_us * height_us * 2 * 3];
    // Byte offset of pixel (x, y) in the packed input.
    let at = |x: usize, y: usize| -> usize { (y * width_us + x) * 3 };
    for y in 0..height_us {
        let up = y.saturating_sub(1);
        let down = (y + 1).min(height_us - 1);
        for x in 0..width_us {
            let left = x.saturating_sub(1);
            let right = (x + 1).min(width_us - 1);
            let centre_off = at(x, y);
            let above_off = at(x, up);
            let left_off = at(left, y);
            let right_off = at(right, y);
            let below_off = at(x, down);
            // The five pixels the rule reads, as triples.
            let centre = [data[centre_off], data[centre_off + 1], data[centre_off + 2]];
            let mut block = [centre, centre, centre, centre];
            if data[above_off..above_off + 3] != data[below_off..below_off + 3]
                && data[left_off..left_off + 3] != data[right_off..right_off + 3]
            {
                if data[left_off..left_off + 3] == data[above_off..above_off + 3] {
                    block[0] = [data[left_off], data[left_off + 1], data[left_off + 2]];
                }
                if data[above_off..above_off + 3] == data[right_off..right_off + 3] {
                    block[1] = [data[right_off], data[right_off + 1], data[right_off + 2]];
                }
                if data[left_off..left_off + 3] == data[below_off..below_off + 3] {
                    block[2] = [data[left_off], data[left_off + 1], data[left_off + 2]];
                }
                if data[below_off..below_off + 3] == data[right_off..right_off + 3] {
                    block[3] = [data[right_off], data[right_off + 1], data[right_off + 2]];
                }
            }
            let base = (y * 2 * out_width_us + x * 2) * 3;
            let next_row = base + out_width_us * 3;
            out[base..base + 3].copy_from_slice(&block[0]);
            out[base + 3..base + 6].copy_from_slice(&block[1]);
            out[next_row..next_row + 3].copy_from_slice(&block[2]);
            out[next_row + 3..next_row + 6].copy_from_slice(&block[3]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference's own corner case: a 3x3 L of white on
    /// black, the centre pixel's top-left output corner takes
    /// the L colour.
    #[test]
    fn rounds_the_inner_corner_of_an_l() -> Result<(), String> {
        let mut data = [0u8, 0, 0].repeat(9);
        // Top-left L: (0,0), (1,0), (0,1) white.
        for index in [0, 1, 3] {
            data[index * 3..index * 3 + 3].copy_from_slice(&[255, 255, 255]);
        }
        let frame = Frame::new(3, 3, PixelFormat::Rgb8, data).map_err(|e| e.to_string())?;
        let out = Scale2x.process(&frame, 2).map_err(|e| e.to_string())?;
        assert_eq!((out.width(), out.height()), (6, 6));
        assert_eq!(out.pixel(1, 1), &[255, 255, 255]);
        Ok(())
    }

    /// A lone pixel survives as exactly four copies of itself:
    /// all four neighbours agree, so the rule does not fire.
    #[test]
    fn lone_pixel_survives_as_four_copies() -> Result<(), String> {
        let mut data = [0u8, 0, 0].repeat(25);
        data[(2 * 5 + 2) * 3..(2 * 5 + 2) * 3 + 3].copy_from_slice(&[255, 255, 255]);
        let frame = Frame::new(5, 5, PixelFormat::Rgb8, data).map_err(|e| e.to_string())?;
        let out = Scale2x.process(&frame, 2).map_err(|e| e.to_string())?;
        let count = out
            .data()
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|px| **px == [255, 255, 255])
            .count();
        assert_eq!(count, 4);
        Ok(())
    }

    #[test]
    fn flat_image_is_preserved_exactly() -> Result<(), String> {
        let frame = Frame::new(8, 8, PixelFormat::Rgb8, [128, 64, 32].repeat(64))
            .map_err(|e| e.to_string())?;
        for scale in [2, 4] {
            let out = Scale2x.process(&frame, scale).map_err(|e| e.to_string())?;
            assert_eq!((out.width(), out.height()), (8 * scale, 8 * scale));
            assert!(
                out.data()
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .all(|px| *px == [128, 64, 32])
            );
        }
        Ok(())
    }

    #[test]
    fn output_uses_only_input_colours() -> Result<(), String> {
        let frame = Frame::new(
            4,
            4,
            PixelFormat::Rgb8,
            vec![
                0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 0, 0, 255,
                0, 0, 255, 255, 0, 0, 255, 0, 0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 0,
                255, 0, 0, 255, 0, 255,
            ],
        )
        .map_err(|e| e.to_string())?;
        for scale in [2, 4] {
            let out = Scale2x.process(&frame, scale).map_err(|e| e.to_string())?;
            let input: std::collections::HashSet<[u8; 3]> =
                frame.data().as_chunks::<3>().0.iter().copied().collect();
            assert!(
                out.data()
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .all(|px| input.contains(px)),
                "scale {scale}: output invented a colour"
            );
        }
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
        let from_tight = Scale2x.process(&tight, 2).map_err(|e| e.to_string())?;
        let from_strided = Scale2x.process(&strided, 2).map_err(|e| e.to_string())?;
        assert_eq!(from_strided.data(), from_tight.data());
        assert_eq!(from_strided.sequence(), 7);
        assert_eq!(from_strided.timestamp(), strided.timestamp());
        Ok(())
    }

    #[test]
    fn rejects_unsupported_scale_and_format() -> Result<(), String> {
        let frame = Frame::new(1, 1, PixelFormat::Rgb8, vec![0; 3]).map_err(|e| e.to_string())?;
        assert!(Scale2x.process(&frame, 3).is_err());
        let grey = Frame::new(1, 1, PixelFormat::Gray8, vec![0]).map_err(|e| e.to_string())?;
        assert!(Scale2x.process(&grey, 2).is_err());
        Ok(())
    }
}
