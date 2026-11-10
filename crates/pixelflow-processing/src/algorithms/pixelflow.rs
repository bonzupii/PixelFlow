//! `PixelFlow`: crisp `AaCut` where the picture is solid art,
//! the repo's `Contour` where it is dithered, cross-faded by a
//! per-pixel dither weight.
//!
//! This is the port of the reference `fr_contour_guard`
//! (`pixelflow_scalers.rs`): the `AaCut` stage runs the crisp
//! branch, the per-pixel guard weight (`guard_weight`, 0.2 to
//! 0.5) measures how dither-like each neighbourhood is, and
//! `blend_by_field` cross-fades the crisp branch into the soft
//! branch by that weight, upsampled to output resolution with
//! `field_up`.
//!
//! One mapping decision, stated plainly because the reference
//! and the repo use the name `Contour` for two different
//! algorithms. The reference's soft branch is its own
//! `contour_impl`: a structure-tensor anisotropic-gaussian
//! reconstruction over 7x7 taps. The repo's `Contour` is an
//! older algorithm: the combined recipe plus the edge follower
//! over whole scanlines. They are not the same code and do not
//! produce the same output, so the soft branch here is the
//! repo's existing `Contour` processor, exactly as assigned,
//! and this algorithm's output is not bit-identical to the
//! reference `fr_contour_guard` where the soft branch
//! contributes. The reference `contour_impl` was not re-ported.
//!
//! Naming: this is the reference `fr_contour_guard`, named
//! `PixelFlow` in the repo, the flagship algorithm. The repo's
//! `Contour` keeps its name and stays in the repo. This is not
//! the reference's own `pixelflow` algorithm (`fr_pixelflow`),
//! which is a different algorithm, out of scope, and not
//! ported here.
//!
//! The `AaCut` stage is a supporting stage inside this module,
//! not a standalone registered algorithm: the reference uses it
//! only as the stage `fr_contour_guard` runs for its crisp
//! branch, and no other reference algorithm is in scope for
//! this port.
//!
//! Reference types map to repo types as follows: the reference
//! `Plane` (padded, packed RGBA8 `Px`) becomes the repo's
//! tightly packed RGB8 `Frame`, read through clamped
//! neighbourhood indices; the reference packed-integer `mix`
//! becomes a per-channel integer mix, which is bit-identical
//! on the RGB channels; `guard_weight`, `field_up`, `mix_f`,
//! and `blend_by_field` are direct transliterations on RGB8
//! and the `f32` weight field.
//!
//! Scales are 2 and 4 only, set by the soft branch: the
//! reference lists 2 to 6, but the repo's `Contour` supports
//! only 2 and 4, and the soft branch cannot run anywhere else.

use pixelflow_core::graph::{ProcessorNode, SpatialDependency};
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin, Frame,
    PixelFormat, ProcessError, Processor, StrideSupport,
};

use crate::algorithms::contour::Contour;

/// Round `value`, clamped to `[0, max]`, to the nearest integer.
/// Binary search over integer midpoints, whose `f32` values are
/// exact, so this is the round exactly, with no float-to-integer
/// cast. Callers only pass `max` values that fit in a `u16`
/// (255, 256).
fn round_clamped_u32(value: f32, max: u32) -> u32 {
    let Ok(max_u16) = u16::try_from(max) else {
        return max;
    };
    let max_f = f32::from(max_u16);
    let clamped = value.clamp(0.0, max_f);
    // `floor(clamped + 0.5)`: the `>= max` case is the floor at
    // the top of the range, and the binary search below keeps
    // `f32(lo) <= shifted < f32(hi)` throughout.
    let shifted = clamped + 0.5;
    if shifted >= max_f {
        return max;
    }
    let mut lo = 0u32;
    let mut hi = max;
    while hi - lo > 1 {
        let mid = lo.midpoint(hi);
        let Ok(mid_u16) = u16::try_from(mid) else {
            return max;
        };
        if f32::from(mid_u16) <= shifted {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// One `u8` channel, rounded and clamped to `[0, 255]`.
fn round_clamped_u8(value: f32) -> u8 {
    u8::try_from(round_clamped_u32(value, 255)).unwrap_or(u8::MAX)
}

/// Clamp `pos + delta` into `[0, hi]` as a `usize` index, the
/// same edge clamp the reference padded plane performs. `hi` is
/// the last valid index; callers only call this when the length
/// is nonzero, so `hi` is a real index.
fn clamp_coord(pos: usize, delta: isize, hi: usize) -> usize {
    pos.saturating_add_signed(delta).min(hi)
}

/// One RGB pixel of the tight input at `(x, y)`. Callers pass
/// coordinates inside the frame.
fn pixel_at(data: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let idx = (y * width + x) * 3;
    [data[idx], data[idx + 1], data[idx + 2]]
}

/// The 3x3 neighbourhood of `(x, y)`, row-major, edge-clamped.
/// `width` and `height` are nonzero at every call site.
fn neighbourhood(data: &[u8], width: usize, height: usize, x: usize, y: usize) -> [[u8; 3]; 9] {
    let mut out = [[0u8; 3]; 9];
    for (row, dy) in (-1isize..=1).enumerate() {
        for (col, dx) in (-1isize..=1).enumerate() {
            let sx = clamp_coord(x, dx, width - 1);
            let sy = clamp_coord(y, dy, height - 1);
            out[row * 3 + col] = pixel_at(data, width, sx, sy);
        }
    }
    out
}

/// Per-channel integer mix, the reference packed `mix` on RGB:
/// `a * (256 - w) + b * w >> 8` per channel, `w` in `0..=256`.
/// Bit-identical to the reference on the RGB channels: each
/// channel product stays below `2^16`, so the packed form's
/// fields cannot carry into each other.
fn mix_channel(a: u8, b: u8, w: u32) -> u8 {
    let top = u32::from(a) * (256 - w) + u32::from(b) * w;
    u8::try_from(top >> 8).unwrap_or(u8::MAX)
}

/// One pixel of the reference `mix`, on RGB triples.
fn mix_rgb(a: [u8; 3], b: [u8; 3], w: u32) -> [u8; 3] {
    [
        mix_channel(a[0], b[0], w),
        mix_channel(a[1], b[1], w),
        mix_channel(a[2], b[2], w),
    ]
}

/// Coverage (0..=256) of the corner cut for a `k` by `k` block,
/// the reference `cut_cov`: the 45-degree line through the
/// midpoints of the two block edges, anti-aliased with exact
/// pixel-centre distance coverage. One table serves all four
/// quadrants; the quadrant only selects the colour. The
/// subpixel offsets are counted in `f32`, exactly as the
/// reference computes `(i + 0.5) / k - 0.5` per cell.
fn cut_cov(k: usize) -> Vec<u32> {
    let Ok(k_u16) = u16::try_from(k) else {
        return Vec::new();
    };
    let kf = f32::from(k_u16);
    let mut table = vec![0u32; k * k];
    let mut v_off = 0.0f32;
    for j in 0..k {
        let v = (v_off + 0.5) / kf - 0.5;
        let mut u_off = 0.0f32;
        for i in 0..k {
            let u = (u_off + 0.5) / kf - 0.5;
            let d = (u.abs() + v.abs() - 0.5) / std::f32::consts::SQRT_2;
            table[j * k + i] = round_clamped_u32((0.5 + d * kf).clamp(0.0, 1.0) * 256.0, 256);
            u_off += 1.0;
        }
        v_off += 1.0;
    }
    table
}

/// The `AaCut` stage, the reference `st_aacut` on tight RGB8:
/// pixel replication, with a corner "cut" where the two
/// neighbours on one side agree with each other and not with
/// the pixel nor with the opposite neighbours. Straight edges,
/// thin lines, and dither fail the test and stay sharp, so no
/// new colours appear except along the cut.
fn aacut(data: &[u8], width: usize, height: usize, k: usize) -> Vec<u8> {
    let out_w = width * k;
    let out_h = height * k;
    let mut out = vec![0u8; out_w * out_h * 3];
    let cov = cut_cov(k);
    for y in 0..height {
        for x in 0..width {
            let neigh = neighbourhood(data, width, height, x, y);
            let centre = neigh[4];
            if neigh.iter().all(|px| *px == centre) {
                for j in 0..k {
                    for i in 0..k {
                        let dst = ((y * k + j) * out_w + x * k + i) * 3;
                        out[dst..dst + 3].copy_from_slice(&centre);
                    }
                }
                continue;
            }
            let (up, left, right, down) = (neigh[1], neigh[3], neigh[5], neigh[7]);
            let mut on = [false; 4];
            let mut col = [centre; 4];
            for qi in 0..4 {
                let (hn, ho) = if qi & 1 == 1 {
                    (right, left)
                } else {
                    (left, right)
                };
                let (vn, vo) = if qi >= 2 { (down, up) } else { (up, down) };
                on[qi] = hn == vn && hn != vo && vn != ho && hn != centre;
                col[qi] = hn;
            }
            for j in 0..k {
                for i in 0..k {
                    let qi = usize::from(2 * j >= k) * 2 + usize::from(2 * i >= k);
                    let mut value = centre;
                    if on[qi] {
                        let weight = cov[j * k + i];
                        if weight > 0 {
                            value = mix_rgb(centre, col[qi], weight);
                        }
                    }
                    let dst = ((y * k + j) * out_w + x * k + i) * 3;
                    out[dst..dst + 3].copy_from_slice(&value);
                }
            }
        }
    }
    out
}

/// Packed-YUV triple of one RGB pixel, the reference `yuv_pack`
/// unpacked: Y in `[0]`, U in `[1]`, V in `[2]`. Each component
/// lands in `0..=255`, so the triple equals the packed bytes the
/// reference compares.
const fn yuv_of(px: [u8; 3]) -> [i32; 3] {
    let r = px[0] as i32;
    let g = px[1] as i32;
    let b = px[2] as i32;
    [
        (77 * r + 150 * g + 29 * b) >> 8,
        ((-43 * r - 85 * g + 128 * b) >> 8) + 128,
        ((128 * r - 107 * g - 21 * b) >> 8) + 128,
    ]
}

/// The reference similarity test on YUV triples: the Y, U, V
/// differences are within (30, 9, 9).
const fn similar(a: [i32; 3], b: [i32; 3]) -> bool {
    (a[0] - b[0]).abs() <= 30 && (a[1] - b[1]).abs() <= 9 && (a[2] - b[2]).abs() <= 9
}

/// Smoothstep, the reference `smooth`: `t * t * (3 - 2 * t)`
/// with `t` clamped to `[0, 1]`.
fn smooth(x: f32) -> f32 {
    let t = x.clamp(0.0, 1.0);
    t.powi(2) * (3.0 - 2.0 * t)
}

/// The per-pixel dither weight, the reference `guard_weight`:
/// a pixel is "lonely" when none of its four neighbours has a
/// similar colour; the 3x3 mean of that map, smoothstepped
/// between 0.2 and 0.5, is the weight. Solid strokes and fills
/// score 0, checkerboard and noise score 1.
fn guard_weight(data: &[u8], width: usize, height: usize) -> Vec<f32> {
    let mut yuv = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            yuv.push(yuv_of(pixel_at(data, width, x, y)));
        }
    }
    let mut lonely = vec![0.0f32; width * height];
    for y in 0..height {
        for x in 0..width {
            let centre = yuv[y * width + x];
            let neighbours = [
                yuv[clamp_coord(y, -1, height - 1) * width + x],
                yuv[clamp_coord(y, 1, height - 1) * width + x],
                yuv[y * width + clamp_coord(x, -1, width - 1)],
                yuv[y * width + clamp_coord(x, 1, width - 1)],
            ];
            if !neighbours.iter().any(|nb| similar(centre, *nb)) {
                lonely[y * width + x] = 1.0;
            }
        }
    }
    let mut weight = vec![0.0f32; width * height];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0f32;
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    let sx = clamp_coord(x, dx, width - 1);
                    let sy = clamp_coord(y, dy, height - 1);
                    sum += lonely[sy * width + sx];
                }
            }
            weight[y * width + x] = smooth((sum / 9.0 - 0.2) / (0.5 - 0.2));
        }
    }
    weight
}

/// One `field_up` axis: the two clamped source indices
/// bracketing output position `position` at `scale`, and the
/// fraction between them. The sampling position is
/// `(position + 0.5) / scale - 0.5`; in units of
/// `1 / (2 * scale)` that is `2 * position + 1 - scale`, so the
/// floor and the fraction are exact integers (`div_euclid`
/// floors), with no float-to-integer cast anywhere. The
/// fraction uses the unclamped floor, as in the reference;
/// only the indices clamp.
fn field_axis(position: u32, scale: u32, extent: u32) -> (usize, usize, f32) {
    let scale_i = i64::from(scale);
    let extent_i = i64::from(extent);
    let numerator = 2 * i64::from(position) + 1 - scale_i;
    let denom = 2 * scale_i;
    let base = numerator.div_euclid(denom);
    let frac_num = numerator.rem_euclid(denom);
    let first = base.clamp(0, extent_i - 1);
    let second = (base + 1).clamp(0, extent_i - 1);
    let frac = f32::from(u16::try_from(frac_num).unwrap_or(0))
        / f32::from(u16::try_from(denom).unwrap_or(1));
    (
        usize::try_from(first).unwrap_or(0),
        usize::try_from(second).unwrap_or(0),
        frac,
    )
}

/// Bilinear (pixel-centre aligned) upsample of a per-source-pixel
/// field to `k`x, the reference `field_up`. `width` and
/// `height` are the source dimensions, nonzero.
fn field_up(field: &[f32], width: u32, height: u32, k: u32, x: u32, y: u32) -> f32 {
    let (xa, xb, fx) = field_axis(x, k, width);
    let (ya, yb, fy) = field_axis(y, k, height);
    let width_us = width as usize;
    let top = field[ya * width_us + xa] * (1.0 - fx) + field[ya * width_us + xb] * fx;
    let bottom = field[yb * width_us + xa] * (1.0 - fx) + field[yb * width_us + xb] * fx;
    top * (1.0 - fy) + bottom * fy
}

/// Float blend of two RGB pixels, the reference `mix_f` on RGB:
/// `x + (y - x) * t` per channel, rounded and clamped.
fn mix_f(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [
        round_clamped_u8(f32::from(a[0]) + (f32::from(b[0]) - f32::from(a[0])) * t),
        round_clamped_u8(f32::from(a[1]) + (f32::from(b[1]) - f32::from(a[1])) * t),
        round_clamped_u8(f32::from(a[2]) + (f32::from(b[2]) - f32::from(a[2])) * t),
    ]
}

/// Cross-fade the crisp branch `crisp` into the soft branch
/// `soft` by the guard field, the reference `blend_by_field`:
/// the field is upsampled with `field_up`, and each output pixel
/// takes the crisp pixel at weight 0, the soft pixel at weight
/// 1, and the float blend between. `width` and `height` are the
/// source dimensions, nonzero; `k` is 2 or 4.
fn blend_by_field(
    crisp: &[u8],
    soft: &[u8],
    field: &[f32],
    width: u32,
    height: u32,
    k: u32,
) -> Vec<u8> {
    let Some(out_w) = width.checked_mul(k) else {
        return Vec::new();
    };
    let Some(out_h) = height.checked_mul(k) else {
        return Vec::new();
    };
    let out_width_us = out_w as usize;
    let out_height_us = out_h as usize;
    let len = out_width_us
        .checked_mul(out_height_us)
        .and_then(|pixels| pixels.checked_mul(3));
    let Some(len) = len else {
        return Vec::new();
    };
    let mut out = vec![0u8; len];
    for oy in 0..out_h {
        for ox in 0..out_w {
            let t = field_up(field, width, height, k, ox, oy);
            let dst = (oy as usize * out_width_us + ox as usize) * 3;
            if t <= 0.0 {
                out[dst..dst + 3].copy_from_slice(&crisp[dst..dst + 3]);
            } else if t >= 1.0 {
                out[dst..dst + 3].copy_from_slice(&soft[dst..dst + 3]);
            } else {
                let a = [crisp[dst], crisp[dst + 1], crisp[dst + 2]];
                let b = [soft[dst], soft[dst + 1], soft[dst + 2]];
                out[dst..dst + 3].copy_from_slice(&mix_f(a, b, t));
            }
        }
    }
    out
}

/// `PixelFlow`: crisp `AaCut` where the picture is solid art,
/// the repo's `Contour` where it is dithered, cross-faded by a
/// per-pixel dither weight.
pub struct PixelFlow;

impl Processor for PixelFlow {
    fn name(&self) -> &'static str {
        "pixelflow"
    }

    fn description(&self) -> &'static str {
        "AaCut where the picture is solid art, Contour where it is dithered, cross-faded by a per-pixel dither weight"
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Global
    }

    fn working_bytes_per_output_pixel(&self) -> Option<u64> {
        // Crisp branch 3, soft branch 3, output 3, plus the
        // guard field at 4 bytes per source pixel: at most 1
        // more byte per output pixel, at 2x. Ten bounds the
        // total at every supported scale.
        Some(10)
    }

    fn supported_scales(&self) -> &[u32] {
        &[2, 4]
    }

    fn stride_support(&self) -> StrideSupport {
        // The stages index a tightly packed buffer directly;
        // the input is packed at the entry point below. The
        // declaration lets the future graph executor insert the
        // pack instead.
        StrideSupport::TightOnly
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if !self.supported_scales().contains(&scale) {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "pixelflow",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "pixelflow",
                format: frame.format().to_string(),
            });
        }
        let frame = frame.packed();
        let frame = &*frame;
        let out_width = frame.width().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("pixelflow output width exceeds u32 range".into())
        })?;
        let out_height = frame.height().checked_mul(scale).ok_or_else(|| {
            ProcessError::Internal("pixelflow output height exceeds u32 range".into())
        })?;
        // An empty frame has no pixels to guard or blend, and
        // the soft branch has no runs to follow; return the
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
        // stage buffers below cannot overflow their lengths:
        // the output is at most `isize::MAX / 3` pixels.
        let output_pixels = u64::from(out_width) * u64::from(out_height);
        let max_alloc = u64::try_from(isize::MAX).unwrap_or(u64::MAX);
        if output_pixels > max_alloc / 3 {
            return Err(ProcessError::SizeOverflow {
                algorithm: "pixelflow",
                stage: "output",
                output_pixels,
            });
        }
        let width = frame.width() as usize;
        let height = frame.height() as usize;
        let Ok(scale_us) = usize::try_from(scale) else {
            return Err(ProcessError::Internal(
                "pixelflow scale exceeds usize range".into(),
            ));
        };
        let crisp = aacut(frame.data(), width, height, scale_us);
        let soft = Contour.process(frame, scale)?;
        let weight = guard_weight(frame.data(), width, height);
        let out_data = blend_by_field(
            &crisp,
            soft.data(),
            &weight,
            frame.width(),
            frame.height(),
            scale,
        );
        Frame::new(out_width, out_height, PixelFormat::Rgb8, out_data)
            .map_err(|e| ProcessError::Internal(e.to_string()))
            .map(|out| {
                out.with_timestamp(frame.timestamp())
                    .with_sequence(frame.sequence())
            })
    }
}

impl PixelFlow {
    /// Wrap this algorithm as a graph node running at `scale`.
    /// The node declares [`SpatialDependency::Global`]: the soft
    /// branch is the repo's `Contour`, whose edge follower runs
    /// span whole scanlines, so no small radius bounds the input
    /// area one output pixel depends on.
    #[must_use]
    pub fn node(scale: u32) -> ProcessorNode<Self> {
        ProcessorNode::new(Self, scale, SpatialDependency::Global)
    }
}

/// The registry entry for `PixelFlow`: a built-in, CPU-only,
/// single-stage algorithm with no parameters.
pub struct PixelFlowAlgorithm;

impl AlgorithmFactory for PixelFlowAlgorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        let mut descriptor =
            AlgorithmDescriptor::from_processor(&PixelFlow, AlgorithmOrigin::BuiltIn);
        descriptor.display_name = "PixelFlow";
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        PixelFlow.validate_config(config)?;
        Ok(Box::new(PixelFlow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::nearest::Nearest;

    /// Build an RGB8 frame from a per-pixel function.
    fn rgb_frame(
        width: usize,
        height: usize,
        pixel: impl Fn(usize, usize) -> [u8; 3],
    ) -> Result<Frame, String> {
        let mut data = Vec::with_capacity(width * height * 3);
        for row in 0..height {
            for col in 0..width {
                data.extend_from_slice(&pixel(col, row));
            }
        }
        let width_u = u32::try_from(width).map_err(|err| err.to_string())?;
        let height_u = u32::try_from(height).map_err(|err| err.to_string())?;
        Frame::new(width_u, height_u, PixelFormat::Rgb8, data).map_err(|err| err.to_string())
    }

    /// True when two `f32` values agree within `1e-6`.
    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn cut_cov_2x_is_uniform_128() {
        assert_eq!(cut_cov(2), vec![128; 4]);
    }

    #[test]
    fn cut_cov_4x_matches_the_corner_table() {
        // Corners (|u| + |v| = 0.75) cover fully, edge centres
        // (0.5) half, inner cells (0.25) not at all.
        assert_eq!(
            cut_cov(4),
            vec![
                256, 128, 128, 256, 128, 0, 0, 128, 128, 0, 0, 128, 256, 128, 128, 256
            ]
        );
    }

    #[test]
    fn guard_weight_zero_on_flat_one_on_checkerboard() -> Result<(), String> {
        let flat = rgb_frame(4, 4, |_, _| [90, 40, 200])?;
        let weight = guard_weight(flat.data(), 4, 4);
        assert!(weight.iter().all(|w| close(*w, 0.0)));
        let checker = rgb_frame(8, 8, |col, row| {
            if (col + row) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?;
        // Every interior pixel is lonely, but the border is not:
        // a border pixel's clamped neighbour is itself, which is
        // similar, and the 3x3 blur spreads that one pixel inward
        // to the border ring and the four diagonal points beside
        // it. Everywhere else the weight is 1.
        let weight = guard_weight(checker.data(), 8, 8);
        for y in 0..8 {
            for x in 0..8 {
                let w = weight[y * 8 + x];
                let border = x == 0 || x == 7 || y == 0 || y == 7;
                let diagonal = (x == 1 || x == 6) && (y == 1 || y == 6);
                if border || diagonal {
                    assert!(w < 1.0, "weight at ({x}, {y}) is {w}");
                } else {
                    assert!(close(w, 1.0), "weight at ({x}, {y}) is {w}");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn guard_weight_lone_pixel_does_not_trigger() -> Result<(), String> {
        // One black pixel in a white field: the pixel is lonely,
        // but its 3x3 mean is 1/9, below the 0.2 smoothstep foot,
        // so no weight anywhere.
        let frame = rgb_frame(5, 5, |col, row| {
            if col == 2 && row == 2 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?;
        let weight = guard_weight(frame.data(), 5, 5);
        assert!(weight.iter().all(|w| close(*w, 0.0)));
        // A 3x3 checker patch in the same field: the patch centre
        // sits in a fully lonely 3x3, so its weight is 1, while
        // the far field stays 0.
        let patched = rgb_frame(5, 5, |col, row| {
            if (1..4).contains(&col) && (1..4).contains(&row) && (col + row) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?;
        let weight = guard_weight(patched.data(), 5, 5);
        assert!(close(weight[2 * 5 + 2], 1.0));
        assert!(close(weight[0], 0.0));
        Ok(())
    }

    #[test]
    fn field_up_samples_corners_and_centre() {
        let field = [0.0f32, 1.0];
        let got: Vec<f32> = (0..4).map(|x| field_up(&field, 2, 1, 2, x, 0)).collect();
        assert_eq!(got.len(), 4);
        for (got, want) in got.iter().zip([0.0f32, 0.25, 0.75, 1.0]) {
            assert!(close(*got, want), "field_up: got {got}, want {want}");
        }
    }

    #[test]
    fn mix_f_endpoints_and_midpoint() {
        assert_eq!(mix_f([0, 0, 0], [255, 255, 255], 0.0), [0, 0, 0]);
        assert_eq!(mix_f([0, 0, 0], [255, 255, 255], 1.0), [255, 255, 255]);
        // 127.5 rounds to 128.
        assert_eq!(mix_f([0, 0, 0], [255, 255, 255], 0.5), [128, 128, 128]);
    }

    #[test]
    fn aacut_flat_image_is_identity() -> Result<(), String> {
        let frame = rgb_frame(4, 4, |_, _| [90, 40, 200])?;
        for scale in [2, 4] {
            let out = aacut(frame.data(), 4, 4, scale);
            let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
            assert_eq!(out.len(), 4 * scale_us * 4 * scale_us * 3);
            assert!(
                out.as_chunks::<3>().0.iter().all(|px| *px == [90, 40, 200]),
                "flat at {scale}x changed"
            );
        }
        Ok(())
    }

    #[test]
    fn aacut_straight_edge_matches_nearest() -> Result<(), String> {
        let frame = rgb_frame(
            8,
            8,
            |col, _| {
                if col < 4 { [0, 0, 0] } else { [255, 255, 255] }
            },
        )?;
        for scale in [2u32, 4] {
            let scale_u = u32::try_from(scale).map_err(|err| err.to_string())?;
            let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
            let from_aacut = aacut(frame.data(), 8, 8, scale_us);
            let from_nearest = Nearest
                .process(&frame, scale_u)
                .map_err(|err| err.to_string())?;
            assert_eq!(from_aacut, from_nearest.data(), "straight edge at {scale}x");
        }
        Ok(())
    }

    #[test]
    fn aacut_cuts_an_l_corner() -> Result<(), String> {
        // 3x3 white L on black, at 2x. The two arm-end pixels
        // cut at their outer corners toward black, and the black
        // centre's inner corner cuts toward white; every cut
        // mixes at coverage 128, so the cut pixel is 127 grey.
        // The corner pixel (0, 0) sits in a clamped
        // neighbourhood of all white, so its block stays white.
        // The whole 6x6 grid below is hand-derived from the cut
        // rule.
        let frame = rgb_frame(3, 3, |col, row| {
            if (col == 0 && row == 0) || (col == 1 && row == 0) || (col == 0 && row == 1) {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        })?;
        let out = aacut(frame.data(), 3, 3, 2);
        let grey = |x: usize, y: usize| out[(y * 6 + x) * 3];
        let want: [[u8; 6]; 6] = [
            [255, 255, 255, 255, 0, 0],
            [255, 255, 255, 127, 0, 0],
            [255, 255, 127, 0, 0, 0],
            [255, 127, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0],
        ];
        for y in 0..6 {
            for x in 0..6 {
                assert_eq!(grey(x, y), want[y][x], "L corner at ({x},{y})");
                assert_eq!(out[(y * 6 + x) * 3 + 1], want[y][x]);
                assert_eq!(out[(y * 6 + x) * 3 + 2], want[y][x]);
            }
        }
        Ok(())
    }

    #[test]
    fn flat_input_is_preserved_exactly() -> Result<(), String> {
        // A flat image has no lonely pixel, so the guard weight
        // is 0 everywhere and the output is the crisp branch,
        // which leaves flat input untouched: shift 0, against
        // the old Contour's shift of up to ~16.
        let algo = PixelFlow;
        let input = rgb_frame(8, 8, |_, _| [128, 64, 32])?;
        for scale in [2, 4] {
            let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
            assert!(
                out.data()
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .all(|px| *px == [128, 64, 32]),
                "flat [128,64,32] at {scale}x moved"
            );
        }
        Ok(())
    }

    #[test]
    fn checkerboard_takes_the_soft_branch() -> Result<(), String> {
        // A checkerboard is dither-like in the interior, so the
        // guard weight is 1 there and the output is the soft
        // branch exactly. The border band blends toward the crisp
        // branch (the clamped edge makes border pixels non-lonely),
        // so only the central region is compared: output pixels
        // with coordinates in [3k, 5k), where the upsampled field
        // samples only weight-1 source pixels.
        let guard = PixelFlow;
        let contour = Contour;
        let input = rgb_frame(8, 8, |col, row| {
            if (col + row) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?;
        for scale in [2, 4] {
            let from_guard = guard
                .process(&input, scale)
                .map_err(|err| err.to_string())?;
            let from_contour = contour
                .process(&input, scale)
                .map_err(|err| err.to_string())?;
            let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
            let out_w = 8 * scale_us;
            for oy in 3 * scale_us..5 * scale_us {
                for ox in 3 * scale_us..5 * scale_us {
                    let dst = (oy * out_w + ox) * 3;
                    assert_eq!(
                        &from_guard.data()[dst..dst + 3],
                        &from_contour.data()[dst..dst + 3],
                        "checkerboard centre at {scale}x differs from the soft branch at ({ox}, {oy})"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn straight_edge_takes_the_crisp_branch() -> Result<(), String> {
        // A straight edge has no lonely pixel, so the guard is 0
        // and the output is the crisp branch, which equals
        // nearest neighbour on a straight edge.
        let guard = PixelFlow;
        let nearest = Nearest;
        let input = rgb_frame(
            16,
            16,
            |col, _| {
                if col < 8 { [0, 0, 0] } else { [255, 255, 255] }
            },
        )?;
        for scale in [2, 4] {
            let from_guard = guard
                .process(&input, scale)
                .map_err(|err| err.to_string())?;
            let from_nearest = nearest
                .process(&input, scale)
                .map_err(|err| err.to_string())?;
            assert_eq!(
                from_guard.data(),
                from_nearest.data(),
                "straight edge at {scale}x differs from the crisp branch"
            );
        }
        Ok(())
    }

    #[test]
    fn strided_input_matches_tight_input() -> Result<(), String> {
        let tight = rgb_frame(4, 4, |col, row| {
            if col + row < 4 {
                [200, 30, 30]
            } else {
                [30, 30, 200]
            }
        })?;
        let mut data = vec![0u8; 4 * 16];
        for y in 0..4 {
            data[y * 16..y * 16 + 12]
                .copy_from_slice(tight.row(u32::try_from(y).map_err(|err| err.to_string())?));
        }
        let strided = Frame::new_with_stride(4, 4, PixelFormat::Rgb8, 16, data)
            .map_err(|err| err.to_string())?
            .with_sequence(9);
        let algo = PixelFlow;
        let from_tight = algo.process(&tight, 2).map_err(|err| err.to_string())?;
        let from_strided = algo.process(&strided, 2).map_err(|err| err.to_string())?;
        assert_eq!(from_strided.data(), from_tight.data());
        assert_eq!(from_strided.sequence(), 9);
        assert_eq!(from_strided.timestamp(), strided.timestamp());
        Ok(())
    }

    #[test]
    fn rejects_unsupported_scale_and_format() -> Result<(), String> {
        let algo = PixelFlow;
        let input = rgb_frame(4, 4, |_, _| [10, 20, 30])?;
        assert!(matches!(
            algo.process(&input, 3),
            Err(ProcessError::UnsupportedScale {
                algorithm: "pixelflow",
                scale: 3
            })
        ));
        let rgba =
            Frame::new(2, 2, PixelFormat::Rgba8, vec![0; 16]).map_err(|err| err.to_string())?;
        assert!(matches!(
            algo.process(&rgba, 2),
            Err(ProcessError::UnsupportedFormat { .. })
        ));
        Ok(())
    }

    #[test]
    fn repeated_processing_is_deterministic() -> Result<(), String> {
        let algo = PixelFlow;
        let input = rgb_frame(16, 16, |col, row| {
            if col + row < 16 {
                [200, 30, 30]
            } else {
                [30, 30, 200]
            }
        })?;
        for scale in [2, 4] {
            let first = algo.process(&input, scale).map_err(|err| err.to_string())?;
            let second = algo.process(&input, scale).map_err(|err| err.to_string())?;
            assert_eq!(
                first.data(),
                second.data(),
                "at {scale}x: second pass differs"
            );
        }
        Ok(())
    }
}
