//! GPU-resident image: a wgpu texture holding one frame, with
//! explicit, measured upload and download. A frame stays
//! resident between compatible GPU stages: a stage reads the
//! texture and writes the next texture, and only the final
//! stage downloads. There is no hidden readback anywhere in
//! this module: every transfer names its bytes and its time.

use crate::context::GpuContext;
use crate::error::GpuError;
use pixelflow_core::{Frame, PixelFormat};
use std::time::{Duration, Instant};

/// Bytes per pixel in the resident texture format. Frames are
/// RGB8 on the CPU; the texture is RGBA8, with alpha 255 on
/// upload and alpha dropped on download, so the color bytes
/// pass through unchanged.
pub const TEXTURE_BYTES_PER_PIXEL: u32 = 4;

/// Bytes the upload of a `width` by `height` RGB8 frame
/// writes into its texture, or `None` when the size
/// computation overflows.
#[must_use]
pub fn upload_bytes(width: u32, height: u32) -> Option<u64> {
    (u64::from(width))
        .checked_mul(u64::from(height))?
        .checked_mul(u64::from(TEXTURE_BYTES_PER_PIXEL))
}

/// Bytes per row of a `width`-pixel texture in the staging
/// buffer, padded up to the copy alignment wgpu requires, or
/// `None` when the size computation overflows.
#[must_use]
pub fn padded_row_bytes(width: u32) -> Option<u32> {
    let row = width.checked_mul(TEXTURE_BYTES_PER_PIXEL)?;
    row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        .checked_mul(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
}

/// Bytes the download staging buffer holds for a `width` by
/// `height` image, rows padded to the copy alignment, or
/// `None` when the size computation overflows.
#[must_use]
pub fn download_buffer_bytes(width: u32, height: u32) -> Option<u64> {
    (u64::from(padded_row_bytes(width)?)).checked_mul(u64::from(height))
}

/// Which way a measured transfer moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferDirection {
    /// CPU frame into a GPU texture.
    Upload,
    /// GPU texture into a CPU frame.
    Download,
}

/// One measured transfer: its byte count, its wall time, and
/// its direction. Upload and download report these instead of
/// moving bytes silently, so the cost of crossing the CPU/GPU
/// boundary is visible to every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferMeasurement {
    /// Bytes moved.
    bytes: u64,
    /// Wall time the transfer took.
    duration: Duration,
    /// Which way the bytes moved.
    direction: TransferDirection,
}

impl TransferMeasurement {
    /// Bytes moved.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Wall time the transfer took.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Which way the bytes moved.
    #[must_use]
    pub fn direction(&self) -> TransferDirection {
        self.direction
    }
}

impl std::fmt::Display for TransferMeasurement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let direction = match self.direction {
            TransferDirection::Upload => "upload",
            TransferDirection::Download => "download",
        };
        write!(
            f,
            "{direction} of {} bytes in {:?}",
            self.bytes, self.duration
        )
    }
}

/// One frame resident on the GPU: a texture plus the
/// dimensions and capture metadata of the frame it holds. The
/// texture is RGBA8; the alpha channel is 255 throughout, so
/// download drops it and returns the original RGB8 bytes.
pub struct GpuImage {
    /// The resident texture.
    texture: wgpu::Texture,
    /// Image width in pixels.
    width: u32,
    /// Image height in pixels.
    height: u32,
    /// Capture time of the frame this image holds, carried
    /// through stages so the downloaded frame keeps it.
    timestamp: Instant,
    /// Capture sequence of the frame this image holds.
    sequence: u64,
    /// Bytes the texture occupies on the device.
    resident_bytes: u64,
}

impl GpuImage {
    /// Upload a CPU frame into a resident texture. The input
    /// must be RGB8; use [`Frame::to_rgb8`] first for anything
    /// else. Row padding in the input is skipped, so a strided
    /// frame uploads exactly its pixel bytes. Returns the
    /// resident image and the measured upload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::UnsupportedFormat`] for a non-RGB8
    /// frame, [`GpuError::Upload`] for an empty frame (a
    /// zero-sized texture cannot exist), and
    /// [`GpuError::SizeOverflow`] when the size computation
    /// overflows.
    pub fn upload(
        context: &GpuContext,
        frame: &Frame,
    ) -> Result<(Self, TransferMeasurement), GpuError> {
        if frame.format() != PixelFormat::Rgb8 {
            return Err(GpuError::UnsupportedFormat {
                format: frame.format().to_string(),
            });
        }
        if frame.width() == 0 || frame.height() == 0 {
            return Err(GpuError::Upload(
                "cannot upload an empty frame: a texture needs a nonzero size".to_string(),
            ));
        }
        let bytes = upload_bytes(frame.width(), frame.height())
            .ok_or(GpuError::SizeOverflow("upload bytes"))?;
        let row_bytes = frame
            .width()
            .checked_mul(TEXTURE_BYTES_PER_PIXEL)
            .ok_or(GpuError::SizeOverflow("upload row bytes"))?;
        let start = Instant::now();
        let mut rgba = Vec::with_capacity(
            usize::try_from(bytes).map_err(|_| GpuError::SizeOverflow("upload bytes"))?,
        );
        for y in 0..frame.height() {
            for px in frame.row(y).as_chunks::<3>().0 {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        let texture = context.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("pixelflow gpu image"),
            size: wgpu::Extent3d {
                width: frame.width(),
                height: frame.height(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        context.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes),
                rows_per_image: Some(frame.height()),
            },
            wgpu::Extent3d {
                width: frame.width(),
                height: frame.height(),
                depth_or_array_layers: 1,
            },
        );
        Ok((
            Self {
                texture,
                width: frame.width(),
                height: frame.height(),
                timestamp: frame.timestamp(),
                sequence: frame.sequence(),
                resident_bytes: bytes,
            },
            TransferMeasurement {
                bytes,
                duration: start.elapsed(),
                direction: TransferDirection::Upload,
            },
        ))
    }

    /// Allocate an empty resident image for a stage to write
    /// into: a texture with storage binding for compute output
    /// and copy source for download. The timestamp and
    /// sequence are the producing stage's input metadata, so
    /// the downloaded frame carries them.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::Compute`] for empty dimensions (a
    /// zero-sized texture cannot exist), and
    /// [`GpuError::SizeOverflow`] when the size computation
    /// overflows.
    pub fn empty(
        context: &GpuContext,
        width: u32,
        height: u32,
        timestamp: Instant,
        sequence: u64,
    ) -> Result<Self, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::Compute(
                "cannot allocate an empty output image: a texture needs a nonzero size".to_string(),
            ));
        }
        let bytes =
            upload_bytes(width, height).ok_or(GpuError::SizeOverflow("output image bytes"))?;
        let texture = context.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("pixelflow gpu output"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Uint,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok(Self {
            texture,
            width,
            height,
            timestamp,
            sequence,
            resident_bytes: bytes,
        })
    }

    /// Download this image to a CPU frame: copy the texture
    /// into a staging buffer, wait for the copy, map the
    /// buffer, and unpack the padded RGBA rows into a tightly
    /// packed RGB8 frame. The wait is explicit
    /// (`device.poll` to completion) and the mapping result
    /// is checked; a failed copy or map is an error, never a
    /// partial frame. Returns the frame and the measured
    /// download.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::Download`] when the staging copy
    /// cannot be sized, [`GpuError::MapFailed`] when the
    /// mapping callback never arrives, [`GpuError::PollFailed`]
    /// when waiting for the device fails, and
    /// [`GpuError::SizeOverflow`] when a size computation
    /// overflows.
    pub fn download(&self, context: &GpuContext) -> Result<(Frame, TransferMeasurement), GpuError> {
        let buffer_bytes = download_buffer_bytes(self.width, self.height)
            .ok_or(GpuError::SizeOverflow("download buffer bytes"))?;
        let padded_row =
            padded_row_bytes(self.width).ok_or(GpuError::SizeOverflow("download row bytes"))?;
        let start = Instant::now();
        let staging = context.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("pixelflow download staging"),
            size: buffer_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder =
            context
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("pixelflow download"),
                });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        context.queue().submit(std::iter::once(encoder.finish()));
        let (sender, receiver) = std::sync::mpsc::channel();
        staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                if sender.send(result).is_err() {
                    // The receiver is gone, so the mapping result
                    // has nowhere to go; the download already failed
                    // on the receiving side.
                }
            });
        context
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| GpuError::PollFailed(error.to_string()))?;
        let mapped = receiver.recv().map_err(|_| GpuError::MapFailed)?;
        mapped.map_err(|error| GpuError::Download(error.to_string()))?;
        let view = staging
            .slice(..)
            .get_mapped_range()
            .map_err(|error| GpuError::Download(error.to_string()))?;
        let mut data = Vec::with_capacity(self.width as usize * self.height as usize * 3);
        for y in 0..self.height {
            let row_start = y as usize * padded_row as usize;
            let row = &view[row_start..row_start + self.width as usize * 4];
            for px in row.as_chunks::<4>().0 {
                data.extend_from_slice(&px[..3]);
            }
        }
        drop(view);
        staging.unmap();
        let frame = Frame::new(self.width, self.height, PixelFormat::Rgb8, data)
            .map_err(|error| GpuError::Download(error.to_string()))?
            .with_timestamp(self.timestamp)
            .with_sequence(self.sequence);
        Ok((
            frame,
            TransferMeasurement {
                bytes: buffer_bytes,
                duration: start.elapsed(),
                direction: TransferDirection::Download,
            },
        ))
    }

    /// The resident texture. Stages read it through a view;
    /// the texture itself never leaves this crate's stages.
    #[must_use]
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Image width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Capture time of the frame this image holds.
    #[must_use]
    pub fn timestamp(&self) -> Instant {
        self.timestamp
    }

    /// Capture sequence of the frame this image holds.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Bytes the texture occupies on the device.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.resident_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_sizes_are_exact_and_checked() {
        assert_eq!(upload_bytes(2, 3), Some(24));
        assert_eq!(upload_bytes(0, 0), Some(0));
        assert_eq!(upload_bytes(u32::MAX, u32::MAX), None);
        assert_eq!(padded_row_bytes(1), Some(256));
        assert_eq!(padded_row_bytes(64), Some(256));
        assert_eq!(padded_row_bytes(65), Some(512));
        assert_eq!(padded_row_bytes(u32::MAX), None);
        assert_eq!(download_buffer_bytes(64, 2), Some(512));
        assert_eq!(download_buffer_bytes(65, 2), Some(1024));
        assert_eq!(download_buffer_bytes(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn transfer_measurement_reports_its_fields() {
        let measurement = TransferMeasurement {
            bytes: 512,
            duration: Duration::from_millis(2),
            direction: TransferDirection::Download,
        };
        assert_eq!(measurement.bytes(), 512);
        assert_eq!(measurement.duration(), Duration::from_millis(2));
        assert_eq!(measurement.direction(), TransferDirection::Download);
        assert_eq!(measurement.to_string(), "download of 512 bytes in 2ms");
    }
}
