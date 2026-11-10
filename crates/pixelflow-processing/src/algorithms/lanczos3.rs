//! Lanczos3 upscaling: separable Lanczos-3 resampling, ported
//! from the reference `scale_kernel` (`pixelflow_scalers.rs`).
//!
//! The reference is a separable-kernel resampler: one axis table
//! per dimension, built by `make_axis` with 14-bit fixed-point
//! weights, then a horizontal pass into a temporary buffer and a
//! vertical pass into the output. The kernel is Lanczos-3
//! (`k_eval` with `Kernel::Lanczos3`, radius 3.0), so each output
//! pixel reads seven input pixels per axis, edge-clamped. The
//! weights are normalized to sum to exactly 16384 (`1 << 14`)
//! per output position, with the rounding residual carried by
//! the largest-magnitude tap, so a flat input is preserved
//! exactly: every tap reads the same value and the weights sum
//! to one.
//!
//! Two details differ from the reference, both value-identical.
//! The reference resamples packed RGBA; repo frames are RGB8,
//! so the alpha channel is absent rather than resampled, and
//! the RGB channels are unaffected by it. And the per-tap
//! source index, which the reference clamps on every tap of
//! every pixel, is precomputed once per axis position into the
//! axis table: same clamp, same index, same accumulation order.
//!
//! The fixed-point conversions use no float-to-integer cast:
//! the workspace lints deny the cast lints, and the reference
//! port in `pixelflow.rs` already rounds the same way, by
//! binary search over integer midpoints. `floor_to_i32` and
//! `round_to_i32` below are that pattern for `f64`.

use pixelflow_core::graph::{ProcessorNode, SpatialDependency};
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin, Frame,
    PixelFormat, ProcessError, Processor, StrideSupport,
};

/// Lanczos3 interpolation upscaling.
pub struct Lanczos3;

impl Processor for Lanczos3 {
    fn name(&self) -> &'static str {
        "lanczos3"
    }

    fn description(&self) -> &'static str {
        "lanczos3 interpolation: separable Lanczos-3 resampling with 14-bit fixed-point weights"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Neighborhood { radius: 3 }
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        // Output 3 bytes plus the horizontal pass's temporary
        // buffer: one intermediate row per output row, `3 /
        // scale` bytes per output pixel, at most 1.5 at 2x.
        // The axis tables hold `taps` entries per output
        // position and stay far below one byte per output
        // pixel at every supported scale. Five bounds the total.
        Some(5)
    }

    fn stride_support(&self) -> StrideSupport {
        // The passes index a tightly packed buffer directly;
        // the input is packed at the entry point below. The
        // declaration lets the future graph executor insert the
        // pack instead.
        StrideSupport::TightOnly
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "lanczos3",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "lanczos3",
                format: frame.format().to_string(),
            });
        }
        // The axis tables index with `i32` positions, so a
        // dimension past `i32::MAX` has no table to build.
        // Such a frame cannot exist in memory: one row alone
        // would be over 6 GiB of RGB8.
        if i32::try_from(frame.width()).is_err() || i32::try_from(frame.height()).is_err() {
            return Err(ProcessError::Internal(
                "lanczos3 input dimension exceeds i32 range".into(),
            ));
        }
        let frame = frame.packed();
        let frame = &*frame;
        let out_width = frame.width().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("lanczos3 output width exceeds u32 range".into())
        })?;
        let out_height = frame.height().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("lanczos3 output height exceeds u32 range".into())
        })?;
        // An empty frame has no pixels to resample; return the
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
        // the output is at most `isize::MAX / 3` pixels.
        let output_pixels = u64::from(out_width) * u64::from(out_height);
        let max_alloc = u64::try_from(isize::MAX).unwrap_or(u64::MAX);
        if output_pixels > max_alloc / 3 {
            return Err(ProcessError::SizeOverflow {
                algorithm: "lanczos3",
                stage: "output",
                output_pixels,
            });
        }
        let width = frame.width();
        let height = frame.height();
        let data = scale_kernel(frame.data(), width, height, scale);
        Frame::new(out_width, out_height, PixelFormat::Rgb8, data)
            .map_err(|e| ProcessError::Internal(e.to_string()))
            .map(|out| {
                out.with_timestamp(frame.timestamp())
                    .with_sequence(frame.sequence())
            })
    }
}

impl Lanczos3 {
    /// Wrap this algorithm as a graph node running at `scale`.
    /// The node declares a radius-3 neighborhood: each output
    /// pixel reads seven input pixels per axis around its
    /// sampling position.
    #[must_use]
    pub fn node(scale: u32) -> ProcessorNode<Self> {
        ProcessorNode::new(Self, scale, SpatialDependency::Neighborhood { radius: 3 })
    }
}

/// The registry entry for Lanczos3: a built-in, CPU-only
/// algorithm with no parameters.
pub struct Lanczos3Algorithm;

impl AlgorithmFactory for Lanczos3Algorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        let mut descriptor =
            AlgorithmDescriptor::from_processor(&Lanczos3, AlgorithmOrigin::BuiltIn);
        descriptor.display_name = "Lanczos3";
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        Lanczos3.validate_config(config)?;
        Ok(Box::new(Lanczos3))
    }
}

/// The reference `k_eval` for `Kernel::Lanczos3`: the
/// windowed sinc, `sinc(t) * sinc(t / 3)`, radius 3.0, where
/// `sinc(x) = sin(pi * x) / (pi * x)`.
fn kernel_eval(t: f64) -> f64 {
    let t = t.abs();
    if t >= 3.0 {
        0.0
    } else if t < 1e-9 {
        1.0
    } else {
        let p = std::f64::consts::PI * t;
        (p.sin() / p) * ((p / 3.0).sin() / (p / 3.0))
    }
}

/// `value` rounded down to the nearest `i32`, by binary search
/// over integer midpoints, whose `f64` values are exact, so
/// this is the floor exactly, with no float-to-integer cast.
/// Callers pass bounds bracketing `value`: `f64(lo) <= value <
/// f64(hi)`.
fn floor_to_i32(value: f64, mut lo: i32, mut hi: i32) -> i32 {
    while hi - lo > 1 {
        let mid = lo.midpoint(hi);
        if f64::from(mid) <= value {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The reference `(v).round()`: halves round away from zero.
/// `floor(v + 0.5)` on the non-negative side, `-floor(0.5 -
/// v)` on the negative side, both through `floor_to_i32`.
/// Callers only pass values with `|v| < 1_000_000`: kernel
/// weights satisfy `|k_eval| <= 1` and the normalizing sum is
/// within `f64` epsilon of 1, so `|v| <= 16384` with wide
/// margin at every call site.
fn round_to_i32(value: f64) -> i32 {
    if value >= 0.0 {
        floor_to_i32(value + 0.5, 0, 1_000_001)
    } else {
        -floor_to_i32(0.5 - value, 0, 1_000_001)
    }
}

/// One resampling axis: the reference `make_axis` for
/// `Kernel::Lanczos3`. `entries` holds, per output position,
/// `taps` pairs of (clamped source index, 14-bit fixed-point
/// weight), in tap order; the weights at one position sum to
/// exactly 16384.
struct Axis {
    taps: usize,
    entries: Vec<(usize, i32)>,
}

/// The reference `make_axis(sn, cn, Kernel::Lanczos3)`: the
/// kernel widens when shrinking (area-correct), with radius
/// 3.0. The repo only upscales, so the shrink factor is 1
/// and the support stays 3.0; the formula is kept general.
/// `sn` and `cn` fit in `i32` at every call site: `process`
/// rejects wider inputs.
fn make_axis(sn: u32, cn: u32) -> Axis {
    let scale = f64::from(cn) / f64::from(sn);
    let shrink = scale.min(1.0);
    let support = 3.0 / shrink;
    let two_support = 2.0 * support;
    // `ceil(x) + 1` without comparing floats for equality:
    // `ceil(x)` is `-floor(-x)`.
    let taps = -floor_to_i32(-two_support, -1_000_000, 0) + 1;
    let taps_us = usize::try_from(taps).unwrap_or(0);
    let sn_i32 = i32::try_from(sn).unwrap_or(i32::MAX);
    let mut entries = Vec::with_capacity(cn as usize * taps_us);
    for o in 0..cn {
        let center = (f64::from(o) + 0.5) / scale - 0.5;
        let start = floor_to_i32(center - support, -8, sn_i32.saturating_add(1)) + 1;
        let mut weights: Vec<f64> = (0..taps_us)
            .map(|t| {
                let tap = i32::try_from(t).unwrap_or(i32::MAX);
                kernel_eval((center - f64::from(start + tap)) * shrink)
            })
            .collect();
        let sum: f64 = weights.iter().sum();
        for weight in &mut weights {
            *weight /= sum;
        }
        let mut fixed = vec![0i32; taps_us];
        let mut acc = 0i32;
        let mut big = 0usize;
        for (t, (slot, &weight)) in fixed.iter_mut().zip(weights.iter()).enumerate() {
            let value = round_to_i32(weight * 16384.0);
            *slot = value;
            acc += value;
            if weight.abs() > weights[big].abs() {
                big = t;
            }
        }
        fixed[big] += 16384 - acc;
        for (t, &value) in fixed.iter().enumerate() {
            let tap = i32::try_from(t).unwrap_or(i32::MAX);
            let index = (start + tap).clamp(0, sn_i32 - 1);
            let index_us = u32::try_from(index).unwrap_or(0) as usize;
            entries.push((index_us, value));
        }
    }
    Axis {
        taps: taps_us,
        entries,
    }
}

/// One fixed-point accumulator value back to a byte: the
/// reference `clamp_u8((v + 8192) >> 14)`.
fn quantize(value: i32) -> u8 {
    let shifted = (value + 8192) >> 14;
    u8::try_from(shifted.clamp(0, 255)).unwrap_or(u8::MAX)
}

/// The reference `scale_kernel` for `Kernel::Lanczos3`, on RGB
/// triples: a horizontal pass into a temporary buffer, then a
/// vertical pass into the output. `data` is tightly packed
/// RGB8, `width` and `height` nonzero, `scale` 2 or 4.
fn scale_kernel(data: &[u8], width: u32, height: u32, scale: u32) -> Vec<u8> {
    let out_width = width * scale;
    let out_height = height * scale;
    let width_us = width as usize;
    let height_us = height as usize;
    let out_width_us = out_width as usize;
    let out_height_us = out_height as usize;
    let axis_x = make_axis(width, out_width);
    let axis_y = make_axis(height, out_height);
    // Horizontal pass: `height` rows by `out_width`.
    let mut tmp = vec![0u8; height_us * out_width_us * 3];
    for y in 0..height_us {
        let src_row = &data[y * width_us * 3..(y + 1) * width_us * 3];
        let dst_row = &mut tmp[y * out_width_us * 3..(y + 1) * out_width_us * 3];
        for x in 0..out_width_us {
            let taps = &axis_x.entries[x * axis_x.taps..(x + 1) * axis_x.taps];
            let mut acc = [0i32; 3];
            for &(index, weight) in taps {
                let px = &src_row[index * 3..index * 3 + 3];
                for channel in 0..3 {
                    acc[channel] += weight * i32::from(px[channel]);
                }
            }
            for channel in 0..3 {
                dst_row[x * 3 + channel] = quantize(acc[channel]);
            }
        }
    }
    // Vertical pass straight into the output.
    let mut out = vec![0u8; out_width_us * out_height_us * 3];
    let mut acc = vec![0i32; out_width_us * 3];
    for y in 0..out_height_us {
        let taps = &axis_y.entries[y * axis_y.taps..(y + 1) * axis_y.taps];
        acc.fill(0);
        for &(index, weight) in taps {
            let row = &tmp[index * out_width_us * 3..(index + 1) * out_width_us * 3];
            for (slot, px) in acc
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .zip(row.as_chunks::<3>().0.iter())
            {
                for channel in 0..3 {
                    slot[channel] += weight * i32::from(px[channel]);
                }
            }
        }
        let dst_row = &mut out[y * out_width_us * 3..(y + 1) * out_width_us * 3];
        for (dst, value) in dst_row.iter_mut().zip(acc.iter()) {
            *dst = quantize(*value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_eval_spot_values() {
        assert_eq!(kernel_eval(0.0), 1.0);
        assert!((kernel_eval(0.5) - 0.607_927_101_854_026_7).abs() < 1e-15);
        // `sin(pi) / pi` is not exactly zero in `f64`, so the
        // kernel at integer offsets 1 and 2 is a rounding-level
        // residue, not exactly zero.
        assert!(kernel_eval(1.0).abs() < 1e-15);
        assert!((kernel_eval(1.5) + 0.135_094_911_523_117_03).abs() < 1e-15);
        assert!(kernel_eval(2.0).abs() < 1e-15);
        assert!((kernel_eval(2.5) - 0.024_317_084_074_161_062).abs() < 1e-15);
        assert_eq!(kernel_eval(3.0), 0.0);
        assert_eq!(kernel_eval(-0.5), kernel_eval(0.5));
    }

    #[test]
    fn round_to_i32_rounds_halves_away_from_zero() {
        assert_eq!(round_to_i32(2.5), 3);
        assert_eq!(round_to_i32(-2.5), -3);
        assert_eq!(round_to_i32(2.4), 2);
        assert_eq!(round_to_i32(-2.4), -2);
        assert_eq!(round_to_i32(0.5), 1);
        assert_eq!(round_to_i32(-0.5), -1);
        assert_eq!(round_to_i32(8192.0), 8192);
    }

    #[test]
    fn make_axis_2x_weights_sum_to_16384() {
        let axis = make_axis(2, 4);
        assert_eq!(axis.taps, 7);
        for o in 0..4 {
            let taps = &axis.entries[o * axis.taps..(o + 1) * axis.taps];
            let sum: i32 = taps.iter().map(|&(_, w)| w).sum();
            assert_eq!(sum, 16384, "output {o}: weights sum to {sum}");
        }
        assert_eq!(
            &axis.entries[0..7],
            &[
                (0, 121),
                (0, -1114),
                (0, 4440),
                (0, 14628),
                (1, -2184),
                (1, 493),
                (1, 0),
            ]
        );
    }

    #[test]
    fn flat_image_is_preserved_exactly() -> Result<(), String> {
        let frame = Frame::new(8, 8, PixelFormat::Rgb8, [128, 64, 32].repeat(64))
            .map_err(|e| e.to_string())?;
        for scale in [2, 4] {
            let out = Lanczos3.process(&frame, scale).map_err(|e| e.to_string())?;
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
    fn two_pixel_gradient_interpolates() -> Result<(), String> {
        // Hand-computed from the 2x axis weights above: the
        // undershoot at the dark edge clamps to 0 and the
        // overshoot at the bright edge clamps to 255.
        let frame = Frame::new(2, 1, PixelFormat::Rgb8, vec![0, 0, 0, 255, 255, 255])
            .map_err(|e| e.to_string())?;
        let out = Lanczos3.process(&frame, 2).map_err(|e| e.to_string())?;
        assert_eq!((out.width(), out.height()), (4, 2));
        assert_eq!(out.pixel(0, 0), &[0, 0, 0]);
        assert_eq!(out.pixel(1, 0), &[54, 54, 54]);
        assert_eq!(out.pixel(2, 0), &[201, 201, 201]);
        assert_eq!(out.pixel(3, 0), &[255, 255, 255]);
        assert_eq!(out.pixel(0, 1), &[0, 0, 0]);
        assert_eq!(out.pixel(3, 1), &[255, 255, 255]);
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
        let from_tight = Lanczos3.process(&tight, 2).map_err(|e| e.to_string())?;
        let from_strided = Lanczos3.process(&strided, 2).map_err(|e| e.to_string())?;
        assert_eq!(from_strided.data(), from_tight.data());
        assert_eq!(from_strided.sequence(), 7);
        assert_eq!(from_strided.timestamp(), strided.timestamp());
        Ok(())
    }

    #[test]
    fn rejects_unsupported_scale_and_format() -> Result<(), String> {
        let frame = Frame::new(1, 1, PixelFormat::Rgb8, vec![0; 3]).map_err(|e| e.to_string())?;
        assert!(Lanczos3.process(&frame, 3).is_err());
        let grey = Frame::new(1, 1, PixelFormat::Gray8, vec![0]).map_err(|e| e.to_string())?;
        assert!(Lanczos3.process(&grey, 2).is_err());
        Ok(())
    }
}
