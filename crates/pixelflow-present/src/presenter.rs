//! The presentation window: a wgpu surface showing the latest
//! processed frame, letterboxed to preserve aspect ratio.
//!
//! The window runs on the thread that calls `run`; winit owns that
//! thread until the window closes or the exit flag is set. Frames
//! arrive on a channel from the processing thread; the newest frame
//! is the one drawn, and older frames are dropped without drawing.

use crate::layout::fit_rect;
use crate::texture::rgba8_from_rgb8;
use pixelflow_core::Frame;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window};

/// What can go wrong while presenting.
#[derive(Debug)]
pub enum PresentError {
    /// No GPU adapter could be found.
    Adapter(wgpu::RequestAdapterError),
    /// The logical device could not be created.
    Device(wgpu::RequestDeviceError),
    /// The surface or its configuration failed.
    Init(String),
    /// The event loop or the window could not be created.
    Window(String),
}

impl fmt::Display for PresentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PresentError::Adapter(e) => write!(f, "failed to find a GPU adapter: {e}"),
            PresentError::Device(e) => write!(f, "failed to create GPU device: {e}"),
            PresentError::Init(msg) => write!(f, "presentation failed to start: {msg}"),
            PresentError::Window(msg) => write!(f, "failed to create window: {msg}"),
        }
    }
}

impl std::error::Error for PresentError {}

const SHADER: &str = r"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0), vec2<f32>(2.0, 1.0), vec2<f32>(0.0, -1.0));
    var out: VOut;
    out.pos = vec4<f32>(positions[i], 0.0, 1.0);
    out.uv = uvs[i];
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
";

/// A physical-pixel coordinate as an f32 viewport value. Display
/// coordinates stay below 2^16, where every integer is exact in f32,
/// so the conversion loses nothing.
fn viewport_coord(value: u32) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// Build the shader, bind group layout, sampler, and render pipeline
/// for the fullscreen triangle that draws one frame.
fn create_display_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::BindGroupLayout, wgpu::Sampler, wgpu::RenderPipeline) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pixelflow display"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pixelflow frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("pixelflow frame sampler"),
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pixelflow display"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pixelflow display"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    (bind_group_layout, sampler, pipeline)
}

/// The GPU state for one window.
struct RenderState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
    texture_size: Option<(u32, u32)>,
}

impl RenderState {
    /// Create the surface, device, and pipeline for a window.
    ///
    /// # Errors
    ///
    /// Returns `Adapter` when no GPU adapter exists, `Device` when
    /// the logical device fails, and `Init` when the surface has no
    /// usable format.
    fn new(window: &Arc<Window>, instance: &wgpu::Instance) -> Result<RenderState, PresentError> {
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| PresentError::Init(e.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            },
        ))
        .or_else(|_| {
            pollster::block_on(instance.request_adapter(
                &wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: true,
                    apply_limit_buckets: false,
                },
            ))
        })
        .map_err(PresentError::Adapter)?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(PresentError::Device)?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| PresentError::Init("surface reports no formats".to_string()))?;
        let present_mode = if caps
            .present_modes
            .contains(&wgpu::PresentMode::AutoVsync)
        {
            wgpu::PresentMode::AutoVsync
        } else {
            wgpu::PresentMode::Fifo
        };
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .ok_or_else(|| PresentError::Init("surface reports no alpha modes".to_string()))?;
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let (bind_group_layout, sampler, pipeline) = create_display_pipeline(&device, format);
        Ok(RenderState {
            device,
            queue,
            surface,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            texture: None,
            bind_group: None,
            texture_size: None,
        })
    }

    /// Rebuild the texture and bind group for a new frame size.
    fn recreate_texture(&mut self, width: u32, height: u32) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pixelflow frame"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pixelflow frame"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.texture = Some(texture);
        self.bind_group = Some(bind_group);
        self.texture_size = Some((width, height));
    }

    /// Resize the surface for a new window size.
    fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload one frame and draw it letterboxed. A lost or outdated
    /// surface is reconfigured and the frame skipped; a timeout, an
    /// occluded window, or a validation error skips the frame.
    fn present(&mut self, rgba: &[u8], width: u32, height: u32) {
        if self.texture_size != Some((width, height)) {
            self.recreate_texture(width, height);
        }
        let Some(texture) = self.texture.as_ref() else {
            // recreate_texture always sets a texture, so this arm
            // cannot run; skipping the frame is the safe fallback.
            return;
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output)
            | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.resize(self.config.width, self.config.height);
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return,
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pixelflow display"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pixelflow display"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let rect = fit_rect(width, height, self.config.width, self.config.height);
            pass.set_viewport(
                viewport_coord(rect.x),
                viewport_coord(rect.y),
                viewport_coord(rect.width),
                viewport_coord(rect.height),
                0.0,
                1.0,
            );
            pass.set_pipeline(&self.pipeline);
            if let Some(bind_group) = self.bind_group.as_ref() {
                pass.set_bind_group(0, bind_group, &[]);
            }
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(output);
    }
}

/// The presentation application: owns the window, the GPU state, the
/// latest frame, and the exit flag.
pub struct Presenter {
    frames: Receiver<Frame>,
    exit: Arc<AtomicBool>,
    fullscreen: bool,
    window: Option<Arc<Window>>,
    state: Option<RenderState>,
    latest: Option<(u32, u32, Vec<u8>)>,
}

impl Presenter {
    /// Build the presenter. Nothing is created yet; the window and
    /// GPU state come up on the first `resumed` event.
    #[must_use]
    pub fn new(windowed: bool, frames: Receiver<Frame>, exit: Arc<AtomicBool>) -> Presenter {
        Presenter {
            frames,
            exit,
            fullscreen: !windowed,
            window: None,
            state: None,
            latest: None,
        }
    }

    /// Toggle between fullscreen and windowed.
    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        if let Some(window) = self.window.as_ref() {
            window.set_fullscreen(if self.fullscreen {
                Some(Fullscreen::Borderless(None))
            } else {
                None
            });
        }
    }
}

impl ApplicationHandler for Presenter {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = winit::window::WindowAttributes::default()
            .with_title("PixelFlow")
            .with_fullscreen(if self.fullscreen {
                Some(Fullscreen::Borderless(None))
            } else {
                None
            });
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                eprintln!("pixelflow: failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };
        // The display handle matters for the GLES fallback on
        // Wayland, where the GL backend needs the compositor
        // connection to present.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        match RenderState::new(&window, &instance) {
            Ok(state) => {
                self.window = Some(window);
                self.state = Some(state);
            }
            Err(e) => {
                eprintln!("pixelflow: presentation failed to start: {e}");
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit.load(Ordering::Relaxed) {
            event_loop.exit();
            return;
        }
        let mut got_frame = false;
        while let Ok(frame) = self.frames.try_recv() {
            let rgba = rgba8_from_rgb8(&frame);
            self.latest = Some((frame.width, frame.height, rgba));
            got_frame = true;
        }
        if got_frame && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(state) = self.state.as_mut() {
                    state.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(state), Some((width, height, rgba))) =
                    (self.state.as_mut(), self.latest.as_ref())
                {
                    let (width, height) = (*width, *height);
                    state.present(rgba, width, height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character(ref c) if c.as_str() == "f" => self.toggle_fullscreen(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Run the presentation window on the current thread. Blocks until the
/// window closes or the exit flag is set. Call once per process: winit
/// allows a single event loop.
///
/// # Errors
///
/// Returns `Window` when the event loop or the window cannot be
/// created. Failures starting the GPU state end the loop with a
/// message on stderr instead of an error here, because they surface
/// inside the `resumed` event.
pub fn run(
    windowed: bool,
    frames: Receiver<Frame>,
    exit: Arc<AtomicBool>,
) -> Result<(), PresentError> {
    let event_loop = EventLoop::new().map_err(|e| PresentError::Window(e.to_string()))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = Presenter::new(windowed, frames, exit);
    event_loop
        .run_app(&mut app)
        .map_err(|e| PresentError::Window(e.to_string()))
}
