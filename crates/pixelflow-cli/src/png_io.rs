//! PNG input and output for the CLI. The only place in PixelFlow
//! that knows about the PNG format; the processing crates work on
//! [`pixelflow_core::Frame`] and never see a file.

use pixelflow_core::{Frame, PixelFormat};
use std::fs::File;
use std::io::BufWriter;

/// Load a PNG file into an RGB8 frame. Indexed-colour, RGBA, and
/// greyscale inputs are expanded to RGB8 on decode; alpha is dropped.
pub fn load_png(path: &str) -> Result<Frame, String> {
    let file = File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // Expand palette / tRNS / sub-8-bit grey to 8-bit RGB(A).
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("png header: {e}"))?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    let (color_type, _) = reader.output_color_type();
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "png: output size unknown".to_string())?;
    let mut buf = vec![0u8; size];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png decode: {e}"))?;
    let bytes = &buf[..frame.buffer_size()];
    let mut data = vec![0u8; w as usize * h as usize * 3];
    match color_type {
        png::ColorType::Rgb => {
            data.copy_from_slice(bytes);
        }
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
            return Err("indexed-colour PNG survived expansion; convert to RGB first".to_string());
        }
    }
    Frame::new(w, h, PixelFormat::Rgb8, data).map_err(|e| e.to_string())
}

/// Save an RGB8 frame as a PNG file. The rows are concatenated
/// into one contiguous buffer first, so a strided frame saves its
/// pixels, not its padding; `write_image_data` takes the whole
/// image in one call and rejects a single row.
pub fn save_png(path: &str, frame: &Frame) -> Result<(), String> {
    if frame.format() != PixelFormat::Rgb8 {
        return Err(format!("cannot save {} frames as PNG", frame.format()));
    }
    let file = File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    let w = BufWriter::new(file);
    let mut enc = png::Encoder::new(w, frame.width(), frame.height());
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
    let mut data = vec![0u8; frame.width() as usize * frame.height() as usize * 3];
    for y in 0..frame.height() {
        let row = frame.row(y);
        let start = y as usize * row.len();
        data[start..start + row.len()].copy_from_slice(row);
    }
    writer
        .write_image_data(&data)
        .map_err(|e| format!("png write: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_round_trips_a_strided_frame_without_its_padding() {
        // 2x2, stride 8 bytes per row: 6 pixel bytes, 2 padding.
        // The padding bytes are 0xEE; a save that copied them
        // would come back as different pixels.
        let mut data = vec![0u8; 2 * 8];
        data[0..6].copy_from_slice(&[10, 20, 30, 40, 50, 60]);
        data[8..14].copy_from_slice(&[70, 80, 90, 100, 110, 120]);
        data[6] = 0xEE;
        data[7] = 0xEE;
        data[14] = 0xEE;
        data[15] = 0xEE;
        let frame =
            Frame::new_with_stride(2, 2, PixelFormat::Rgb8, 8, data).expect("strided frame builds");
        let path = std::env::temp_dir().join("pixelflow-png-strided-test.png");
        save_png(path.to_str().expect("temp path is UTF-8"), &frame).expect("save writes the file");
        let loaded =
            load_png(path.to_str().expect("temp path is UTF-8")).expect("saved file loads");
        assert_eq!(loaded.width(), 2);
        assert_eq!(loaded.height(), 2);
        assert_eq!(
            loaded.data(),
            &[10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]
        );
        std::fs::remove_file(&path).expect("temp file removes");
    }
}
