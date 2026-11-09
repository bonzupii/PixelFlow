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

/// Save an RGB8 frame as a PNG file.
pub fn save_png(path: &str, frame: &Frame) -> Result<(), String> {
    if frame.format != PixelFormat::Rgb8 {
        return Err(format!("cannot save {} frames as PNG", frame.format));
    }
    let file = File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    let w = BufWriter::new(file);
    let mut enc = png::Encoder::new(w, frame.width, frame.height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
    writer
        .write_image_data(&frame.data)
        .map_err(|e| format!("png write: {e}"))?;
    Ok(())
}
