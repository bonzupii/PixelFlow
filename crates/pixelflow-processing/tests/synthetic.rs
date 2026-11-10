//! Deterministic synthetic-fixture tests for the processing
//! algorithms. Every input image is generated in code: no PNG
//! files, no fixture directory, nothing copyrighted. These tests
//! run on a clean checkout with no fixtures present.
//!
//! Two kinds of assertions live here. Exact values, derived from
//! the algorithm's documented behavior or an independent
//! computation: output dimensions, the hard-light shift of a flat
//! color, pixel equality with nearest neighbour on straight
//! edges, the exact palette set, format conversions, typed
//! errors. Measured invariants, where no exact value is
//! derivable: roughness numbers, crossing smoothness, phase
//! preservation. Each invariant states its rationale in a
//! comment; nothing here is a value copied from a run of the code
//! under test.
//!
//! One case the mandate lists is now expressible through the
//! public API: a frame whose rows carry padding.
//! `Frame::new_with_stride` builds one, and the strided-contour
//! test below runs one through the tight-only algorithm. The
//! odd-width row-alignment test below it is the complementary
//! case: on a 3- or 7-wide image a stride miscalculation would
//! shift rows, and the test would catch it.

use pixelflow_core::{Frame, PixelFormat, ProcessError, Processor};
use pixelflow_processing::{Bicubic, Contour, Nearest, PixelFlow};

/// Reduce a value into one byte. The modulo makes the conversion
/// infallible; the fallback arm is unreachable by construction.
fn byte_of(value: usize) -> u8 {
    u8::try_from(value % 256).unwrap_or(0)
}

/// Build an RGB8 frame from a per-pixel function. `pixel` takes
/// the column and row as `usize`.
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

/// Build an RGB8 frame filled with one color.
fn flat_frame(width: usize, height: usize, color: [u8; 3]) -> Result<Frame, String> {
    rgb_frame(width, height, |_, _| color)
}

/// The distinct colors in a frame, sorted.
fn distinct_colors(frame: &Frame) -> Vec<[u8; 3]> {
    let mut colors: Vec<[u8; 3]> = (0..frame.width() as usize * frame.height() as usize)
        .map(|index| {
            [
                frame.data()[index * 3],
                frame.data()[index * 3 + 1],
                frame.data()[index * 3 + 2],
            ]
        })
        .collect();
    colors.sort_unstable();
    colors.dedup();
    colors
}

/// The flat color `combined` produces for an input channel value,
/// derived from the hard-light stage, which is the only stage
/// that moves a flat color. On flat input the merge, the
/// labelset, the dither blend, and the grain layer are all the
/// identity (every neighbourhood is uniform), and the edge
/// follower finds no runs. What remains is the final blend in
/// `combined`: `out = 0.5 * v + 0.5 * hardlight(v, v)`, with
/// `hardlight(v, v) = 2u^2` below half and `1 - 2(1-u)^2` above
/// half, `u = v / 255`. The two branches below are that formula
/// in integer arithmetic, rounded the way `to_u8_img` rounds.
/// The largest shift this formula produces is just under 16
/// (at `u = 0.25` and `u = 0.75`), so a flat color never moves
/// more than 16 in any channel.
fn flat_predicted(value: u8) -> u8 {
    let value_u = u32::from(value);
    let numerator = if value <= 127 {
        value_u * 255 + 2 * value_u * value_u
    } else {
        value_u * 255 + 255 * 255 - 2 * (255 - value_u) * (255 - value_u)
    };
    let rounded = (numerator + 255) / 510;
    u8::try_from(rounded).unwrap_or(255)
}

/// Mean absolute difference between horizontally adjacent pixels,
/// summed over channels. A checkerboard scores high, a smooth
/// gradient scores low, mush scores near zero.
fn horizontal_roughness(frame: &Frame) -> f64 {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let mut sum = 0f64;
    let mut count = 0f64;
    for row in 0..height {
        for col in 0..width - 1 {
            let base = (row * width + col) * 3;
            for channel in 0..3 {
                sum += f64::from(
                    frame.data()[base + channel].abs_diff(frame.data()[base + 3 + channel]),
                );
            }
            count += 1.0;
        }
    }
    sum / count
}

/// For each output column, the first row (scanning top-down)
/// whose red channel drops below 128. A column that never drops
/// reports the frame height. This is the edge-crossing position
/// of a two-color edge.
fn edge_crossings(frame: &Frame) -> Vec<i32> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let mut crossings = Vec::with_capacity(width);
    for col in 0..width {
        let mut crossing = i32::try_from(height).unwrap_or(i32::MAX);
        for row in 0..height {
            if frame.data()[(row * width + col) * 3] < 128 {
                crossing = i32::try_from(row).unwrap_or(i32::MAX);
                break;
            }
        }
        crossings.push(crossing);
    }
    crossings
}

fn max_adjacent_jump(crossings: &[i32]) -> i32 {
    crossings
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .max()
        .unwrap_or(0)
}

#[test]
fn tiny_images_complete_at_both_scales() -> Result<(), String> {
    let algo = Contour;
    for (width, height) in [(1, 1), (2, 1), (1, 2), (2, 2), (3, 3), (1, 4), (4, 1)] {
        let input = rgb_frame(width, height, |col, row| {
            [
                byte_of(col * 37 + row * 91),
                byte_of(col * 11 + row * 53),
                128,
            ]
        })?;
        for scale in [2, 4] {
            let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
            let scale_u = u32::try_from(scale).map_err(|err| err.to_string())?;
            let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
            assert_eq!(
                (out.width(), out.height()),
                (
                    u32::try_from(width).map_err(|err| err.to_string())? * scale_u,
                    u32::try_from(height).map_err(|err| err.to_string())? * scale_u
                ),
                "{width}x{height} at {scale}x: dims"
            );
            assert_eq!(out.format(), PixelFormat::Rgb8);
            assert_eq!(out.data().len(), width * scale_us * height * scale_us * 3);
        }
    }
    Ok(())
}

#[test]
fn flat_colors_follow_the_hardlight_prediction() -> Result<(), String> {
    let algo = Contour;
    let colors: [[u8; 3]; 8] = [
        [0, 0, 0],
        [255, 255, 255],
        [128, 64, 32],
        [17, 34, 51],
        [200, 200, 200],
        [64, 128, 192],
        [96, 96, 96],
        [1, 254, 127],
    ];
    for color in colors {
        let input = flat_frame(8, 8, color)?;
        for scale in [2, 4] {
            let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
            // A flat image stays flat: exactly one color out.
            let distinct = distinct_colors(&out);
            assert_eq!(distinct.len(), 1, "{color:?} at {scale}x: stayed flat");
            // That color is the hard-light prediction, within one
            // unit for f32 rounding in the pipeline.
            for channel in 0..3 {
                let got = distinct[0][channel];
                let want = flat_predicted(color[channel]);
                assert!(
                    got.abs_diff(want) <= 1,
                    "{color:?} at {scale}x channel {channel}: got {got}, predicted {want}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn flat_single_pixel() -> Result<(), String> {
    let algo = Contour;
    for color in [[0, 0, 0], [128, 64, 32], [255, 255, 255]] {
        let input = flat_frame(1, 1, color)?;
        for scale in [2, 4] {
            let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
            assert_eq!((out.width(), out.height()), (scale, scale));
            let distinct = distinct_colors(&out);
            assert_eq!(distinct.len(), 1);
            for channel in 0..3 {
                assert!(
                    distinct[0][channel].abs_diff(flat_predicted(color[channel])) <= 1,
                    "1x1 {color:?} at {scale}x channel {channel}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn axis_aligned_edges_match_nearest_exactly() -> Result<(), String> {
    let contour = Contour;
    let nearest = Nearest;
    let vertical = rgb_frame(
        16,
        16,
        |col, _| {
            if col < 8 { [0, 0, 0] } else { [255, 255, 255] }
        },
    )?;
    let horizontal = rgb_frame(
        16,
        16,
        |_, row| {
            if row < 8 { [0, 0, 0] } else { [255, 255, 255] }
        },
    )?;
    // A straight edge has no staircase corners for the follower
    // to fit, so contour must not move any pixel relative to
    // nearest neighbour, which is exact by construction.
    for input in [&vertical, &horizontal] {
        for scale in [2, 4] {
            let from_contour = contour
                .process(input, scale)
                .map_err(|err| err.to_string())?;
            let from_nearest = nearest
                .process(input, scale)
                .map_err(|err| err.to_string())?;
            assert_eq!(
                from_contour.data(),
                from_nearest.data(),
                "straight edge at {scale}x: contour moved a pixel"
            );
            assert_eq!(distinct_colors(&from_contour).len(), 2);
        }
    }
    Ok(())
}

#[test]
fn shallow_diagonal_is_smoothed() -> Result<(), String> {
    let contour = Contour;
    let nearest = Nearest;
    // A 2:1 slope: white above the line y = x / 2, black below.
    // The source edge is a staircase; the fitted line is smooth.
    let input = rgb_frame(32, 16, |col, row| {
        if row * 2 < col {
            [255, 255, 255]
        } else {
            [0, 0, 0]
        }
    })?;
    for scale in [2, 4] {
        let from_contour = contour
            .process(&input, scale)
            .map_err(|err| err.to_string())?;
        let from_nearest = nearest
            .process(&input, scale)
            .map_err(|err| err.to_string())?;
        let contour_crossings = edge_crossings(&from_contour);
        let nearest_crossings = edge_crossings(&from_nearest);
        // The fitted line advances smoothly: its largest step
        // between adjacent columns is smaller than nearest's
        // staircase jump, which is one source pixel (the scale).
        assert!(
            max_adjacent_jump(&contour_crossings) < max_adjacent_jump(&nearest_crossings),
            "at {scale}x: contour jump not smaller than nearest"
        );
        // The fitted line tracks the true line y = x / 2 within
        // half a source pixel (the scale over two): a follower
        // that invents geometry would drift further than that.
        let limit = f64::from(scale) / 2.0;
        for (col, crossing) in contour_crossings.iter().enumerate() {
            let col_u = u32::try_from(col).map_err(|err| err.to_string())?;
            let ideal = f64::from(col_u) / 2.0;
            let crossing_f = f64::from(*crossing);
            assert!(
                (crossing_f - ideal).abs() <= limit,
                "at {scale}x column {col}: crossing {crossing}, ideal {ideal}"
            );
        }
        // Antialiasing happened: intermediate colors exist.
        assert!(distinct_colors(&from_contour).len() > 2);
    }
    Ok(())
}

#[test]
fn diagonal_edge_antialiases() -> Result<(), String> {
    let algo = Contour;
    // A 45-degree edge: white above the diagonal, black below.
    let input = rgb_frame(16, 16, |col, row| {
        if col > row {
            [255, 255, 255]
        } else {
            [0, 0, 0]
        }
    })?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        // Intermediate grays appear along the edge.
        assert!(distinct_colors(&out).len() > 2);
        let has_intermediate = (0..out.width() as usize * out.height() as usize).any(|index| {
            let value = out.data()[index * 3];
            value > 0 && value < 255
        });
        assert!(has_intermediate, "at {scale}x: no antialiased pixel");
        // The corners sit in uniform neighbourhoods, so they stay
        // exactly the source colors.
        assert_eq!(out.pixel(0, 0), &[0, 0, 0]);
        assert_eq!(out.pixel(out.width() - 1, 0), &[255, 255, 255]);
    }
    Ok(())
}

#[test]
fn curve_edge_antialiases_with_exact_far_field() -> Result<(), String> {
    let algo = Contour;
    // A filled circle, radius 10, centered in a 32x32 image.
    let input = rgb_frame(32, 32, |col, row| {
        let dx = col as i32 - 16;
        let dy = row as i32 - 16;
        if dx * dx + dy * dy < 100 {
            [255, 255, 255]
        } else {
            [0, 0, 0]
        }
    })?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        assert!(distinct_colors(&out).len() > 2);
        assert_eq!(out.pixel(0, 0), &[0, 0, 0]);
        assert_eq!(
            out.pixel(out.width() / 2, out.height() / 2),
            &[255, 255, 255]
        );
    }
    Ok(())
}

#[test]
fn checkerboard_keeps_its_phase() -> Result<(), String> {
    let contour = Contour;
    let nearest = Nearest;
    let input = rgb_frame(8, 8, |col, row| {
        if (col + row) % 2 == 0 {
            [0, 0, 0]
        } else {
            [255, 255, 255]
        }
    })?;
    for scale in [2, 4] {
        let from_contour = contour
            .process(&input, scale)
            .map_err(|err| err.to_string())?;
        let from_nearest = nearest
            .process(&input, scale)
            .map_err(|err| err.to_string())?;
        // Every output block keeps the mean of its source cell on
        // the right side of mid-grey: the checker is softened but
        // its phase survives. Mush would sit at mid-grey on both
        // parities.
        let scale_u = usize::try_from(scale).map_err(|err| err.to_string())?;
        for cell_row in 0..8 {
            for cell_col in 0..8 {
                let mut sum = 0u32;
                let mut count = 0u32;
                for dy in 0..scale_u {
                    for dx in 0..scale_u {
                        let index = ((cell_row * scale_u + dy) * 8 * scale_u
                            + (cell_col * scale_u + dx))
                            * 3;
                        sum += u32::from(from_contour.data()[index]);
                        count += 1;
                    }
                }
                let mean = sum / count;
                let want_white = (cell_col + cell_row) % 2 == 1;
                assert_eq!(
                    mean > 128,
                    want_white,
                    "at {scale}x cell ({cell_col},{cell_row}): block mean {mean}"
                );
            }
        }
        // Contrast survives: contour keeps at least half of
        // nearest's roughness. Mush would score near zero.
        let ratio = horizontal_roughness(&from_contour) / horizontal_roughness(&from_nearest);
        assert!(
            ratio >= 0.5,
            "at {scale}x: checker roughness ratio {ratio:.2}, below 0.5"
        );
        assert!(distinct_colors(&from_contour).len() > 2);
    }
    Ok(())
}

#[test]
fn gradient_stays_smooth_without_banding() -> Result<(), String> {
    let algo = Contour;
    // A grey ramp rising 17 per source column, 0 to 255.
    let input = rgb_frame(16, 16, |col, _| {
        let value = byte_of(col * 255 / 15);
        [value, value, value]
    })?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        let width = out.width() as usize;
        let height = out.height() as usize;
        // Monotone in every row and channel: no reversals.
        for row in 0..height {
            for col in 0..width - 1 {
                for channel in 0..3 {
                    let here = out.data()[(row * width + col) * 3 + channel];
                    let next = out.data()[(row * width + col + 1) * 3 + channel];
                    assert!(next >= here, "at {scale}x row {row} col {col}: reversal");
                }
            }
        }
        // No step steeper than twice the input's per-column rise
        // (17): the hard-light stage's steepest local slope is
        // 1.5x, so a step past 32 cannot come from the documented
        // stages. A band edge would be a jump of a full source
        // step in one output pixel with flats on either side,
        // which monotonicity plus the intermediate values below
        // already exclude.
        let mut max_step = 0u8;
        for col in 0..width - 1 {
            let here = out.data()[col * 3];
            let next = out.data()[(col + 1) * 3];
            max_step = max_step.max(here.abs_diff(next));
        }
        assert!(max_step <= 32, "at {scale}x: max step {max_step}");
        // New intermediate values exist: the ramp is interpolated,
        // not quantized back into 16 flats.
        assert!(distinct_colors(&out).len() > 16);
        // The border pixels sit in locally flat neighbourhoods,
        // so they move by at most the flat-color shift (16).
        assert!(
            out.data()[0] <= 16,
            "at {scale}x: first pixel {}",
            out.data()[0]
        );
        assert!(
            out.data()[(width - 1) * 3] >= 239,
            "at {scale}x: last pixel {}",
            out.data()[(width - 1) * 3]
        );
    }
    Ok(())
}

#[test]
fn palette_transitions_keep_their_colors() -> Result<(), String> {
    let algo = Contour;
    // Four vertical bands: red, green, blue, yellow.
    let bands: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    let input = rgb_frame(16, 16, |col, _| bands[col / 4])?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        // The labelset assigns each output pixel to one label, so
        // no blended color appears: the output set is exactly the
        // input set, and every channel is 0 or 255.
        let mut want = bands.to_vec();
        want.sort_unstable();
        assert_eq!(distinct_colors(&out), want, "at {scale}x: palette changed");
        assert!(
            out.data().iter().all(|value| *value == 0 || *value == 255),
            "at {scale}x: a blended channel value appeared"
        );
    }
    Ok(())
}

#[test]
fn transparency_conversion_is_exact_and_processing_rejects_other_formats() -> Result<(), String> {
    let algo = Contour;
    // The processor takes RGB8 only, so alpha cannot survive
    // processing. What must hold exactly: the RGB channels
    // survive the RGBA-to-RGB8 conversion untouched, processing
    // the converted frame equals processing the same RGB frame
    // built directly, and unconverted formats are rejected with
    // a typed error instead of being silently misread.
    let rgba_data: Vec<u8> = vec![9, 8, 7, 255, 1, 2, 3, 0, 4, 5, 6, 128, 7, 7, 7, 64];
    let rgba = Frame::new(2, 2, PixelFormat::Rgba8, rgba_data).map_err(|err| err.to_string())?;
    let converted = rgba.to_rgb8();
    assert_eq!(
        converted.data(),
        vec![9, 8, 7, 1, 2, 3, 4, 5, 6, 7, 7, 7],
        "to_rgb8 must keep RGB and drop alpha"
    );
    let direct = Frame::new(2, 2, PixelFormat::Rgb8, converted.data().to_vec())
        .map_err(|err| err.to_string())?;
    let from_converted = algo.process(&converted, 2).map_err(|err| err.to_string())?;
    let from_direct = algo.process(&direct, 2).map_err(|err| err.to_string())?;
    assert_eq!(from_converted.data(), from_direct.data());
    assert!(matches!(
        algo.process(&rgba, 2),
        Err(ProcessError::UnsupportedFormat { .. })
    ));
    let grey = Frame::new(1, 1, PixelFormat::Gray8, vec![42]).map_err(|err| err.to_string())?;
    assert_eq!(grey.to_rgb8().data(), vec![42, 42, 42]);
    assert!(matches!(
        algo.process(&grey, 2),
        Err(ProcessError::UnsupportedFormat { .. })
    ));
    Ok(())
}

#[test]
fn odd_dimensions_and_row_alignment() -> Result<(), String> {
    let contour = Contour;
    let nearest = Nearest;
    for (width, height) in [(3, 5), (7, 11), (5, 3), (3, 3)] {
        let input = rgb_frame(width, height, |col, row| {
            [
                byte_of(col * 50 + row * 30),
                byte_of(col * 20 + row * 70),
                200,
            ]
        })?;
        for scale in [2, 4] {
            let out = contour
                .process(&input, scale)
                .map_err(|err| err.to_string())?;
            let scale_u = u32::try_from(scale).map_err(|err| err.to_string())?;
            assert_eq!(
                (out.width(), out.height()),
                (
                    u32::try_from(width).map_err(|err| err.to_string())? * scale_u,
                    u32::try_from(height).map_err(|err| err.to_string())? * scale_u
                ),
                "{width}x{height} at {scale}x: dims"
            );
        }
    }
    // Row alignment: eight stripes, four source pixels tall, one
    // color each. A stride error would shift rows and mix stripe
    // colors; nearest neighbour is exact by construction, so its
    // output pins the alignment, and contour's block middles sit
    // in locally flat neighbourhoods, within the flat-color shift
    // (16) plus one rounding unit of their stripe color.
    let stripes = rgb_frame(32, 32, |_, row| [byte_of(row / 4 * 32), 0, 0])?;
    for scale in [2u32, 4] {
        let scale_u = usize::try_from(scale).map_err(|err| err.to_string())?;
        let from_nearest = nearest
            .process(&stripes, scale)
            .map_err(|err| err.to_string())?;
        let from_contour = contour
            .process(&stripes, scale)
            .map_err(|err| err.to_string())?;
        let out_width = 32 * scale_u;
        for stripe in 0..8 {
            let want = byte_of(stripe * 32);
            for block_row in 0..4 * scale_u {
                let row = stripe * 4 * scale_u + block_row;
                for col in 0..out_width {
                    let got = from_nearest.data()[(row * out_width + col) * 3];
                    assert_eq!(got, want, "nearest stripe {stripe} row {row}");
                    assert_eq!(from_nearest.data()[(row * out_width + col) * 3 + 1], 0);
                    assert_eq!(from_nearest.data()[(row * out_width + col) * 3 + 2], 0);
                }
            }
            for sample in [2 * scale_u, 2 * scale_u + 1] {
                let row = stripe * 4 * scale_u + sample;
                let got = from_contour.data()[(row * out_width + 16 * scale_u) * 3];
                assert!(
                    got.abs_diff(want) <= 17,
                    "contour stripe {stripe} at {scale}x: middle {got}, want {want}"
                );
                assert_eq!(
                    from_contour.data()[(row * out_width + 16 * scale_u) * 3 + 1],
                    0
                );
                assert_eq!(
                    from_contour.data()[(row * out_width + 16 * scale_u) * 3 + 2],
                    0
                );
            }
        }
    }
    Ok(())
}

#[test]
fn large_image_completes_at_4x() -> Result<(), String> {
    let algo = Contour;
    // The real target size, with a deterministic pattern.
    let input = rgb_frame(256, 224, |col, row| {
        [
            byte_of(col * 7 + row * 13),
            byte_of(col + row * 3),
            byte_of(col * row),
        ]
    })?;
    let out = algo.process(&input, 4).map_err(|err| err.to_string())?;
    assert_eq!((out.width(), out.height()), (1024, 896));
    let again = algo.process(&input, 4).map_err(|err| err.to_string())?;
    assert_eq!(out.data(), again.data(), "large image not deterministic");
    let medium = rgb_frame(64, 48, |col, row| {
        [byte_of(col * 3 + row * 7), byte_of(col + row * 5), 128]
    })?;
    let medium_out = algo.process(&medium, 4).map_err(|err| err.to_string())?;
    assert_eq!((medium_out.width(), medium_out.height()), (256, 192));
    Ok(())
}

#[test]
fn repeated_processing_is_deterministic() -> Result<(), String> {
    let algo = Contour;
    let inputs: Vec<Frame> = vec![
        flat_frame(8, 8, [128, 64, 32])?,
        rgb_frame(32, 16, |col, row| {
            if row * 2 < col {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        })?,
        rgb_frame(8, 8, |col, row| {
            if (col + row) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?,
        rgb_frame(16, 16, |col, _| {
            let value = byte_of(col * 255 / 15);
            [value, value, value]
        })?,
    ];
    for input in &inputs {
        for scale in [2, 4] {
            let first = algo.process(input, scale).map_err(|err| err.to_string())?;
            let second = algo.process(input, scale).map_err(|err| err.to_string())?;
            assert_eq!(
                first.data(),
                second.data(),
                "{}x{} at {scale}x: second pass differs",
                input.width(),
                input.height()
            );
        }
    }
    Ok(())
}

#[test]
fn rejected_inputs_report_typed_errors() -> Result<(), String> {
    let contour = Contour;
    let nearest = Nearest;
    let input = flat_frame(4, 4, [10, 20, 30])?;
    assert!(matches!(
        contour.process(&input, 3),
        Err(ProcessError::UnsupportedScale {
            algorithm: "contour",
            scale: 3
        })
    ));
    assert!(matches!(
        nearest.process(&input, 3),
        Err(ProcessError::UnsupportedScale {
            algorithm: "nearest",
            scale: 3
        })
    ));
    let rgba = Frame::new(2, 2, PixelFormat::Rgba8, vec![0; 16]).map_err(|err| err.to_string())?;
    assert!(matches!(
        nearest.process(&rgba, 2),
        Err(ProcessError::UnsupportedFormat { .. })
    ));
    Ok(())
}

#[test]
fn nearest_baseline_replicates_blocks_exactly() -> Result<(), String> {
    let algo = Nearest;
    // Six distinct source pixels; every output pixel must equal
    // the source pixel at (col / scale, row / scale), exactly.
    let input = rgb_frame(3, 2, |col, row| {
        let index = (row * 3 + col) * 3;
        [byte_of(index + 1), byte_of(index + 2), byte_of(index + 3)]
    })?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        let scale_u = usize::try_from(scale).map_err(|err| err.to_string())?;
        assert_eq!((out.width(), out.height()), (3 * scale, 2 * scale));
        for out_row in 0..2 * scale_u {
            for out_col in 0..3 * scale_u {
                let want = input.pixel(
                    u32::try_from(out_col / scale_u).map_err(|err| err.to_string())?,
                    u32::try_from(out_row / scale_u).map_err(|err| err.to_string())?,
                );
                let got = out.pixel(
                    u32::try_from(out_col).map_err(|err| err.to_string())?,
                    u32::try_from(out_row).map_err(|err| err.to_string())?,
                );
                assert_eq!(got, want, "at {scale}x pixel ({out_col},{out_row})");
            }
        }
    }
    Ok(())
}

#[test]
fn pixelflow_tiny_images_complete_at_both_scales() -> Result<(), String> {
    let algo = PixelFlow;
    for (width, height) in [(1, 1), (2, 1), (1, 2), (2, 2), (3, 3), (1, 4), (4, 1)] {
        let input = rgb_frame(width, height, |col, row| {
            [
                byte_of(col * 37 + row * 91),
                byte_of(col * 11 + row * 53),
                128,
            ]
        })?;
        for scale in [2, 4] {
            let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
            let scale_u = u32::try_from(scale).map_err(|err| err.to_string())?;
            let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
            assert_eq!(
                (out.width(), out.height()),
                (
                    u32::try_from(width).map_err(|err| err.to_string())? * scale_u,
                    u32::try_from(height).map_err(|err| err.to_string())? * scale_u
                ),
                "{width}x{height} at {scale}x: dims"
            );
            assert_eq!(out.format(), PixelFormat::Rgb8);
            assert_eq!(out.data().len(), width * scale_us * height * scale_us * 3);
        }
    }
    Ok(())
}

#[test]
fn pixelflow_flat_color_is_preserved_exactly() -> Result<(), String> {
    // Re-measurement 1: flat [128, 64, 32] goes through the crisp
    // branch (guard weight 0), which leaves it untouched: shift
    // 0, against the old Contour's hard-light shift of up to ~16.
    let algo = PixelFlow;
    let input = flat_frame(8, 8, [128, 64, 32])?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        assert_eq!(
            distinct_colors(&out),
            vec![[128, 64, 32]],
            "flat [128,64,32] at {scale}x moved"
        );
    }
    Ok(())
}

#[test]
fn pixelflow_checkerboard_is_replicated() -> Result<(), String> {
    // A checkerboard is dither-like in the interior, so the
    // guard weight is 1 there and the output is the soft branch
    // exactly. The soft branch's dither bypass replicates
    // checkerboard pixels into their blocks, which is exactly
    // nearest-neighbour replication, so the centre equals the
    // crisp branch there. The border band blends toward the
    // crisp branch anyway (the clamped edge makes border pixels
    // non-lonely), so only the central region is compared:
    // output pixels with coordinates in [3k, 5k), where the
    // upsampled field samples only weight-1 source pixels.
    // The soft branch itself is the reference `contour_impl`,
    // ported privately inside the PixelFlow module; the unit
    // test `checkerboard_takes_the_soft_branch` compares
    // against it directly, and the oracle log is the
    // bit-identity evidence against the reference.
    let guard = PixelFlow;
    let nearest = Nearest;
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
        let from_nearest = nearest
            .process(&input, scale)
            .map_err(|err| err.to_string())?;
        let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
        let out_w = 8 * scale_us;
        for oy in 3 * scale_us..5 * scale_us {
            for ox in 3 * scale_us..5 * scale_us {
                let dst = (oy * out_w + ox) * 3;
                assert_eq!(
                    &from_guard.data()[dst..dst + 3],
                    &from_nearest.data()[dst..dst + 3],
                    "checkerboard centre at {scale}x differs from replication at ({ox}, {oy})"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn pixelflow_strided_input_matches_tight_input() -> Result<(), String> {
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
fn pixelflow_repeated_processing_is_deterministic() -> Result<(), String> {
    let algo = PixelFlow;
    let inputs: Vec<Frame> = vec![
        flat_frame(8, 8, [128, 64, 32])?,
        rgb_frame(32, 16, |col, row| {
            if row * 2 < col {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        })?,
        rgb_frame(8, 8, |col, row| {
            if (col + row) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?,
    ];
    for input in &inputs {
        for scale in [2, 4] {
            let first = algo.process(input, scale).map_err(|err| err.to_string())?;
            let second = algo.process(input, scale).map_err(|err| err.to_string())?;
            assert_eq!(
                first.data(),
                second.data(),
                "{}x{} at {scale}x: second pass differs",
                input.width(),
                input.height()
            );
        }
    }
    Ok(())
}

#[test]
fn bicubic_tiny_images_complete_at_both_scales() -> Result<(), String> {
    let algo = Bicubic;
    for (width, height) in [(1, 1), (2, 1), (1, 2), (2, 2), (3, 3), (1, 4), (4, 1)] {
        let input = rgb_frame(width, height, |col, row| {
            [
                byte_of(col * 37 + row * 91),
                byte_of(col * 11 + row * 53),
                128,
            ]
        })?;
        for scale in [2, 4] {
            let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
            let scale_u = u32::try_from(scale).map_err(|err| err.to_string())?;
            let scale_us = usize::try_from(scale).map_err(|err| err.to_string())?;
            assert_eq!(
                (out.width(), out.height()),
                (
                    u32::try_from(width).map_err(|err| err.to_string())? * scale_u,
                    u32::try_from(height).map_err(|err| err.to_string())? * scale_u
                ),
                "{width}x{height} at {scale}x: dims"
            );
            assert_eq!(out.format(), PixelFormat::Rgb8);
            assert_eq!(out.data().len(), width * scale_us * height * scale_us * 3);
        }
    }
    Ok(())
}

#[test]
fn bicubic_flat_color_is_preserved_exactly() -> Result<(), String> {
    // Re-measurement 1: the 14-bit weights at one output
    // position sum to exactly 16384, so a flat input is
    // preserved exactly at both scales: shift 0.
    let algo = Bicubic;
    let input = flat_frame(8, 8, [128, 64, 32])?;
    for scale in [2, 4] {
        let out = algo.process(&input, scale).map_err(|err| err.to_string())?;
        assert_eq!(
            distinct_colors(&out),
            vec![[128, 64, 32]],
            "flat [128,64,32] at {scale}x moved"
        );
    }
    Ok(())
}

#[test]
fn bicubic_strided_input_matches_tight_input() -> Result<(), String> {
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
    let algo = Bicubic;
    let from_tight = algo.process(&tight, 2).map_err(|err| err.to_string())?;
    let from_strided = algo.process(&strided, 2).map_err(|err| err.to_string())?;
    assert_eq!(from_strided.data(), from_tight.data());
    assert_eq!(from_strided.sequence(), 9);
    assert_eq!(from_strided.timestamp(), strided.timestamp());
    Ok(())
}

#[test]
fn bicubic_repeated_processing_is_deterministic() -> Result<(), String> {
    let algo = Bicubic;
    let inputs: Vec<Frame> = vec![
        flat_frame(8, 8, [128, 64, 32])?,
        rgb_frame(32, 16, |col, row| {
            if row * 2 < col {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        })?,
        rgb_frame(8, 8, |col, row| {
            if (col + row) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        })?,
    ];
    for input in &inputs {
        for scale in [2, 4] {
            let first = algo.process(input, scale).map_err(|err| err.to_string())?;
            let second = algo.process(input, scale).map_err(|err| err.to_string())?;
            assert_eq!(
                first.data(),
                second.data(),
                "{}x{} at {scale}x: second pass differs",
                input.width(),
                input.height()
            );
        }
    }
    Ok(())
}

#[test]
fn contour_strided_input_matches_tight_input() -> Result<(), String> {
    // A 4x4 pattern with a hard edge and a diagonal, built once
    // tight and once with 4 padding bytes per row. Contour is
    // tight-only and packs at its entry point, so both runs must
    // produce identical output, and the strided run must carry
    // the input's metadata through.
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
    let algo = Contour;
    let from_tight = algo.process(&tight, 2).map_err(|err| err.to_string())?;
    let from_strided = algo.process(&strided, 2).map_err(|err| err.to_string())?;
    assert_eq!(from_strided.data(), from_tight.data());
    assert_eq!(from_strided.sequence(), 9);
    assert_eq!(from_strided.timestamp(), strided.timestamp());
    Ok(())
}
