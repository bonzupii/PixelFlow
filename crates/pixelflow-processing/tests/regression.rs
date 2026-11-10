//! Regression tests for the contour port: process the six native
//! test images at 4x and compare pixel-for-pixel against the Python
//! reference outputs from the upscaler experiments (round 6).
//!
//! The fixture images are copyrighted game screenshots and live
//! outside this repo. Point `PIXELFLOW_FIXTURES_DIR` at a local copy
//! to run these tests; without fixtures they skip.
//!
//! Limits, from the standalone port's verified numbers
//! (contour/README.md): max abs diff must be <= 1 (rounding),
//! except mario, where at most 10 pixels may differ by up to 34:
//! at the right end of mario's long hill-boundary runs a horizontal
//! and a vertical run compete for the same pixel and the winner is a
//! float comparison at a near-tie. That is the reference port's own
//! behaviour, not a regression in this crate.

use pixelflow_core::{Frame, PixelFormat, Processor};
use pixelflow_processing::{Contour, Nearest};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PIXELFLOW_FIXTURES_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
}

/// True when the fixture images are present. They are copyrighted
/// game screenshots and are never committed to the repo; a developer
/// supplies them locally (`PIXELFLOW_FIXTURES_DIR`) to run the
/// regression tests, otherwise the tests skip.
fn fixtures_present(dir: &PathBuf) -> bool {
    dir.join("inputs").join("mario.png").is_file()
        && dir
            .join("expected")
            .join("mario_20_clean_base.png")
            .is_file()
}

/// Minimal PNG loader for the tests (RGB8 out; indexed/RGBA/grey
/// inputs are expanded by the decoder, alpha dropped).
fn load_png(path: &PathBuf) -> Result<Frame, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("png header {}: {e}", path.display()))?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    let (color_type, _) = reader.output_color_type();
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| format!("png size unknown: {}", path.display()))?;
    let mut buf = vec![0u8; size];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png decode {}: {e}", path.display()))?;
    let bytes = &buf[..frame.buffer_size()];
    let mut data = vec![0u8; w as usize * h as usize * 3];
    match color_type {
        png::ColorType::Rgb => data.copy_from_slice(bytes),
        png::ColorType::Rgba => {
            for i in 0..w as usize * h as usize {
                data[i * 3] = bytes[i * 4];
                data[i * 3 + 1] = bytes[i * 4 + 1];
                data[i * 3 + 2] = bytes[i * 4 + 2];
            }
        }
        png::ColorType::Grayscale => {
            for i in 0..w as usize * h as usize {
                data[i * 3] = bytes[i];
                data[i * 3 + 1] = bytes[i];
                data[i * 3 + 2] = bytes[i];
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..w as usize * h as usize {
                data[i * 3] = bytes[i * 2];
                data[i * 3 + 1] = bytes[i * 2];
                data[i * 3 + 2] = bytes[i * 2];
            }
        }
        png::ColorType::Indexed => {
            return Err(format!(
                "indexed PNG survived expansion: {}",
                path.display()
            ));
        }
    }
    Frame::new(w, h, PixelFormat::Rgb8, data).map_err(|e| e.to_string())
}

/// Max abs channel difference and the count of pixels differing by
/// more than 1, between two frames of the same size.
fn compare(a: &Frame, b: &Frame) -> (u8, usize) {
    assert_eq!(
        (a.width(), a.height()),
        (b.width(), b.height()),
        "size mismatch"
    );
    assert_eq!(a.format(), b.format(), "format mismatch");
    let mut max = 0u8;
    let mut over_one = 0usize;
    for i in 0..a.width() as usize * a.height() as usize {
        let mut px_max = 0u8;
        for c in 0..3 {
            let d = a.data()[i * 3 + c].abs_diff(b.data()[i * 3 + c]);
            px_max = px_max.max(d);
        }
        max = max.max(px_max);
        if px_max > 1 {
            over_one += 1;
        }
    }
    (max, over_one)
}

#[test]
fn contour_matches_python_reference_at_4x() -> Result<(), String> {
    let dir = fixtures_dir();
    if !fixtures_present(&dir) {
        println!("fixtures not present, skipping regression test");
        return Ok(());
    }
    let algo = Contour;
    for name in ["ct", "eb", "ct2", "zelda", "mario", "ctnew"] {
        let input = load_png(&dir.join("inputs").join(format!("{name}.png")))?;
        let expected = load_png(
            &dir.join("expected")
                .join(format!("{name}_20_clean_base.png")),
        )?;
        let out = algo.process(&input, 4).map_err(|e| e.to_string())?;
        assert_eq!(
            (out.width(), out.height()),
            (expected.width(), expected.height()),
            "{name}: size"
        );
        let (max, over_one) = compare(&out, &expected);
        if name == "mario" {
            assert!(max <= 34, "mario: max abs diff {max}, limit 34");
            assert!(
                over_one <= 10,
                "mario: {over_one} pixels differ by >1, limit 10"
            );
        } else {
            assert!(max <= 1, "{name}: max abs diff {max}, limit 1");
            assert_eq!(over_one, 0, "{name}: {over_one} pixels differ by >1");
        }
        println!("{name}: max abs diff {max}, pixels differing by >1: {over_one}");
    }
    Ok(())
}

#[test]
fn contour_2x_dimensions() -> Result<(), String> {
    let dir = fixtures_dir();
    if !fixtures_present(&dir) {
        println!("fixtures not present, skipping 2x dimensions test");
        return Ok(());
    }
    let input = load_png(&dir.join("inputs").join("mario.png"))?;
    let out = Contour.process(&input, 2).map_err(|e| e.to_string())?;
    assert_eq!((out.width(), out.height()), (512, 448));
    Ok(())
}

#[test]
fn nearest_sanity() -> Result<(), String> {
    let f =
        Frame::new(2, 1, PixelFormat::Rgb8, vec![1, 2, 3, 4, 5, 6]).map_err(|e| e.to_string())?;
    let out = Nearest.process(&f, 2).map_err(|e| e.to_string())?;
    assert_eq!((out.width(), out.height()), (4, 2));
    assert_eq!(out.pixel(0, 0), &[1, 2, 3]);
    assert_eq!(out.pixel(1, 0), &[1, 2, 3]);
    assert_eq!(out.pixel(2, 0), &[4, 5, 6]);
    assert_eq!(out.pixel(3, 1), &[4, 5, 6]);
    Ok(())
}
