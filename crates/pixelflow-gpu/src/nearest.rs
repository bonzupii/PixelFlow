//! GPU nearest-neighbour upscaling: a WGSL compute shader
//! that turns each source pixel into a `scale` by `scale`
//! block of identical pixels, exposed as a graph [`Node`].
//!
//! The shader reads integer texels (`rgba8uint`), so values
//! pass through the GPU unchanged: the output is
//! pixel-identical to the CPU nearest algorithm on the same
//! inputs, and that parity is the acceptance check for this
//! execution path. The node uploads the input frame,
//! dispatches the shader into a resident output texture, and
//! downloads the result, each step measured; a failure at any
//! step is a [`GpuError`] converted to
//! [`ProcessError::Internal`], never a silent fall back to CPU.

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::image::GpuImage;
use pixelflow_core::graph::{Node, NodeInputs, NodeOutputs, SpatialDependency, TemporalDependency};
use pixelflow_core::{Frame, PixelFormat, ProcessError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The WGSL compute shader: one invocation per output pixel,
/// each copying its source pixel. The bounds check covers
/// the padded edge when the output size is not a multiple of
/// the workgroup size; the scale, source width, and source
/// height arrive in the uniform buffer.
pub const NEAREST_WGSL: &str = r"
struct Params {
    scale: u32,
    src_width: u32,
    src_height: u32,
    _pad: u32,
}

@group(0) @binding(0) var src_tex: texture_2d<u32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba8uint, write>;
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dst_size = textureDimensions(dst_tex);
    if (gid.x >= dst_size.x || gid.y >= dst_size.y) {
        return;
    }
    let sx = gid.x / params.scale;
    let sy = gid.y / params.scale;
    let px = textureLoad(src_tex, vec2<i32>(i32(sx), i32(sy)), 0);
    textureStore(dst_tex, vec2<i32>(i32(gid.x), i32(gid.y)), px);
}
";

/// Scale factors the GPU nearest node supports, matching the
/// CPU nearest algorithm.
pub const NEAREST_SUPPORTED_SCALES: &[u32] = &[2, 4];

/// Workgroup edge: invocations per workgroup side.
const WORKGROUP_SIZE: u32 = 16;

/// Timings for one GPU nearest run: the measured upload, the
/// dispatch from submit to device completion, and the measured
/// download. The compute work itself is the dispatch; upload
/// and download are the explicit transfer costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuRunTimings {
    /// Time the upload took.
    upload: Duration,
    /// Time from compute submit to device completion.
    dispatch: Duration,
    /// Time the download took.
    download: Duration,
}

impl GpuRunTimings {
    /// Time the upload took.
    #[must_use]
    pub fn upload(&self) -> Duration {
        self.upload
    }

    /// Time from compute submit to device completion.
    #[must_use]
    pub fn dispatch(&self) -> Duration {
        self.dispatch
    }

    /// Time the download took.
    #[must_use]
    pub fn download(&self) -> Duration {
        self.download
    }

    /// Total time for the run: upload plus dispatch plus
    /// download.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.upload + self.dispatch + self.download
    }
}

/// A GPU nearest-neighbour node: it runs the WGSL shader on
/// the shared [`GpuContext`] at a fixed integer scale. The
/// compute pipeline is built at construction, so a shader
/// validation failure surfaces when the node is created, not
/// at the first frame.
pub struct GpuNearestNode {
    /// Shared GPU context: device, queue, adapter record.
    context: Arc<GpuContext>,
    /// Integer scale factor for this node.
    scale: u32,
    /// Compiled nearest shader.
    pipeline: wgpu::ComputePipeline,
    /// Bind group layout: source texture, destination storage
    /// texture, uniform parameters.
    bind_group_layout: wgpu::BindGroupLayout,
}

impl GpuNearestNode {
    /// Build the node on `context` at `scale`, compiling the
    /// shader now. The pipeline creation runs inside a wgpu
    /// validation error scope, so an invalid shader is a
    /// returned error, not a logged validation failure.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::UnsupportedScale`] for a scale
    /// outside [`NEAREST_SUPPORTED_SCALES`], and
    /// [`GpuError::Shader`] when the shader fails wgpu
    /// validation.
    pub fn new(context: Arc<GpuContext>, scale: u32) -> Result<Self, GpuError> {
        if !NEAREST_SUPPORTED_SCALES.contains(&scale) {
            return Err(GpuError::UnsupportedScale { scale });
        }
        let device = context.device();
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pixelflow nearest"),
            source: wgpu::ShaderSource::Wgsl(NEAREST_WGSL.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pixelflow nearest bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pixelflow nearest layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pixelflow nearest"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            return Err(GpuError::Shader(error.to_string()));
        }
        Ok(Self {
            context,
            scale,
            pipeline,
            bind_group_layout,
        })
    }

    /// Integer scale factor this node runs at.
    #[must_use]
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// The shared GPU context this node runs on.
    #[must_use]
    pub fn context(&self) -> &Arc<GpuContext> {
        &self.context
    }

    /// Run the GPU nearest upscale on one frame and return the
    /// output frame with its timings. Upload, dispatch, and
    /// download are separate measured steps; the output stays
    /// resident as a [`GpuImage`] between dispatch and
    /// download. An empty input frame holds no pixels, so it
    /// returns the empty scaled frame without dispatching:
    /// a zero-sized texture cannot exist, and there is
    /// nothing for the shader to copy.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError`] for an unsupported format, an
    /// oversized frame, or any upload, dispatch, or download
    /// failure. A GPU node that cannot run fails with the
    /// reason; it never falls back to CPU.
    pub fn run_gpu(&self, frame: &Frame) -> Result<(Frame, GpuRunTimings), GpuError> {
        if frame.format() != PixelFormat::Rgb8 {
            return Err(GpuError::UnsupportedFormat {
                format: frame.format().to_string(),
            });
        }
        let out_width = frame
            .width()
            .checked_mul(self.scale)
            .ok_or(GpuError::SizeOverflow("output width"))?;
        let out_height = frame
            .height()
            .checked_mul(self.scale)
            .ok_or(GpuError::SizeOverflow("output height"))?;
        if frame.width() == 0 || frame.height() == 0 {
            let output = Frame::new(out_width, out_height, PixelFormat::Rgb8, Vec::new())
                .map_err(|error| GpuError::Compute(error.to_string()))?
                .with_timestamp(frame.timestamp())
                .with_sequence(frame.sequence());
            return Ok((
                output,
                GpuRunTimings {
                    upload: Duration::ZERO,
                    dispatch: Duration::ZERO,
                    download: Duration::ZERO,
                },
            ));
        }
        let (input, upload) = GpuImage::upload(&self.context, frame)?;
        let output = GpuImage::empty(
            &self.context,
            out_width,
            out_height,
            frame.timestamp(),
            frame.sequence(),
        )?;
        let dispatch = self.dispatch(&input, &output)?;
        let (frame, download) = output.download(&self.context)?;
        Ok((
            frame,
            GpuRunTimings {
                upload: upload.duration(),
                dispatch,
                download: download.duration(),
            },
        ))
    }

    /// Dispatch the shader from a resident input image into a
    /// resident output image and wait for the device to finish
    /// it. Returns the dispatch time, from submit to device
    /// completion. The wait is explicit: `device.poll` to
    /// completion, so `run_gpu` never reads a texture the GPU
    /// has not finished writing.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::PollFailed`] when waiting for the
    /// device fails.
    fn dispatch(&self, input: &GpuImage, output: &GpuImage) -> Result<Duration, GpuError> {
        let device = self.context.device();
        let params: [u32; 4] = [self.scale, input.width(), input.height(), 0];
        let mut param_bytes = [0u8; 16];
        for (index, value) in params.iter().enumerate() {
            param_bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pixelflow nearest params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.context.queue().write_buffer(&uniform, 0, &param_bytes);
        let input_view = input
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let output_view = output
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pixelflow nearest bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&input_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&output_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &uniform,
                        offset: 0,
                        size: None,
                    }),
                },
            ],
        });
        let start = Instant::now();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pixelflow nearest"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("pixelflow nearest"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(
                output.width().div_ceil(WORKGROUP_SIZE),
                output.height().div_ceil(WORKGROUP_SIZE),
                1,
            );
        }
        self.context
            .queue()
            .submit(std::iter::once(encoder.finish()));
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| GpuError::PollFailed(error.to_string()))?;
        Ok(start.elapsed())
    }
}

impl Node for GpuNearestNode {
    fn name(&self) -> &'static str {
        "nearest-gpu"
    }

    fn input_ports(&self) -> &[&'static str] {
        &["in"]
    }

    fn output_ports(&self) -> &[&'static str] {
        &["out"]
    }

    fn input_format(&self, port: &str) -> Option<PixelFormat> {
        if port == "in" {
            Some(PixelFormat::Rgb8)
        } else {
            None
        }
    }

    fn output_format(&self, port: &str) -> Option<PixelFormat> {
        if port == "out" {
            Some(PixelFormat::Rgb8)
        } else {
            None
        }
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Local
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    /// Nearest-neighbour scaling is tile-safe: each output
    /// pixel copies one input pixel, so a cropped run produces
    /// exactly the corresponding crop of the whole-frame
    /// output, same as the CPU algorithm.
    fn tile_safe(&self) -> bool {
        true
    }

    fn run(&mut self, inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError> {
        let input = inputs.get("in").ok_or_else(|| {
            ProcessError::Internal("nearest-gpu node ran without its input frame".to_string())
        })?;
        let (output, _) = self.run_gpu(input).map_err(ProcessError::from)?;
        let mut outputs = NodeOutputs::new();
        outputs.add("out", output);
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_parses_and_validates_with_naga() -> Result<(), String> {
        let module =
            naga::front::wgsl::parse_str(NEAREST_WGSL).map_err(|error| error.to_string())?;
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );
        validator
            .validate(&module)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    #[test]
    fn supported_scales_match_the_cpu_algorithm() {
        assert_eq!(NEAREST_SUPPORTED_SCALES, &[2, 4]);
    }
}
