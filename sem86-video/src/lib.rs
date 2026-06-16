use std::iter::{self, once};
use std::num::NonZero;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use bilge::prelude::*;
use bytemuck::{NoUninit, Pod, Zeroable, bytes_of, cast_slice};
use liblisa::utils::EitherIter;
use log::{error, info, trace};
use sem86_core::hw::MouseMove;
use sem86_core::hw::vga::{DEFAULT_DAC_COLORS, MemoryAddressing, ModeSet, VideoMemory};
use wgpu::util::DeviceExt;
use wgpu::{
    BufferUsages, CompilationMessageType, ErrorFilter, ExperimentalFeatures, Extent3d, Instance, PollType, PresentMode, Surface,
    TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureView, TextureViewDescriptor,
};
use winit::dpi::{PhysicalPosition, PhysicalSize};

pub mod framebuffer;
pub mod texture;

pub const FB_WIDTH: u32 = 320;
pub const FB_HEIGHT: u32 = 200;

const VRAM_SIZE: usize = 16 << 20;

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 3],
    tex_coords: [f32; 2],
}

impl Vertex {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

const VERTICES: &[Vertex] = &[
    // Top right
    Vertex {
        position: [1.0, 1.0, 0.0],
        tex_coords: [1.0, 0.0],
    },
    // Top left
    Vertex {
        position: [-1.0, 1.0, 0.0],
        tex_coords: [0.0, 0.0],
    },
    // Bottom right
    Vertex {
        position: [1.0, -1.0, 0.0],
        tex_coords: [1.0, 1.0],
    },
    // Bottom left
    Vertex {
        position: [-1.0, -1.0, 0.0],
        tex_coords: [0.0, 1.0],
    },
];

const VGA_PALETTE: [u32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Zeroable, Pod)]
#[repr(C)]
struct ModeOptions {
    blink: bool,
    is_graphics: bool,
    force_43_aspect: bool,
    memory_addressing: MemoryAddressing,
    reserved: u1,
}

#[derive(Copy, Clone, Debug, Zeroable, NoUninit)]
#[repr(C)]
struct ModeConfig {
    width: u16,
    height: u16,
    mode_options: ModeOptions,
    _reserved: [u8; 3],
}

pub struct VideoRenderer<'a> {
    surface: wgpu::Surface<'a>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: winit::dpi::PhysicalSize<u32>,
    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    #[allow(dead_code)]
    diffuse_texture: texture::Texture,
    diffuse_bind_group: wgpu::BindGroup,
    modeset_recv: ModeSetReceiver,
    memory: VideoMemory,
    frame_buffer: wgpu::Buffer,
    mode_buffer: wgpu::Buffer,
    start: Instant,
    dac_palette_buffer: wgpu::Buffer,
    vga_palette_buffer: wgpu::Buffer,
    surface_format: TextureFormat,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    window_size_buffer: wgpu::Buffer,
    last_vram_clean: Instant,
    need_full_update: bool,
}

impl<'a> VideoRenderer<'a> {
    pub async fn new(
        surface: Surface<'a>, instance: Instance, size: PhysicalSize<u32>, modeset_recv: ModeSetReceiver, memory: VideoMemory,
    ) -> Self {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: Default::default(),
                experimental_features: ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .unwrap();

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        info!("Picked surface format: {surface_format:?}");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        surface.configure(&device, &config);

        let font_data = include_bytes!("../../fonts/VGA-ROM.F08")
            .iter()
            .flat_map(|b| [7, 6, 5, 4, 3, 2, 1, 0].map(|n| ((b >> n) & 1) * 0xff))
            .collect::<Vec<_>>();
        let font_texture =
            texture::Texture::new(&device, &queue, 8, font_data.len() as u32 / 8, &font_data, Some("font")).unwrap();

        let texture_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float {
                            filterable: false,
                        },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage {
                            read_only: true,
                        },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage {
                            read_only: true,
                        },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage {
                            read_only: true,
                        },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
            label: Some("texture_bind_group_layout"),
        });

        let frame_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("framebuffer"),
            contents: &vec![0x46; VRAM_SIZE],
            usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        });

        let dac_palette_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("dac_palette"),
            contents: bytemuck::bytes_of(&DEFAULT_DAC_COLORS),
            usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        });

        let vga_palette_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vga_palette"),
            contents: bytemuck::bytes_of(&VGA_PALETTE),
            usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        });

        let window_size_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("window_size"),
            contents: bytemuck::bytes_of(&[size.width, size.height]),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        let mode_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mode"),
            contents: bytemuck::bytes_of(&ModeConfig {
                width: 40,
                height: 25,
                mode_options: ModeOptions::new(true, false, true, MemoryAddressing::default()),
                _reserved: Default::default(),
            }),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        let diffuse_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&font_texture.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&font_texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer(mode_buffer.as_entire_buffer_binding()),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Buffer(frame_buffer.as_entire_buffer_binding()),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Buffer(dac_palette_buffer.as_entire_buffer_binding()),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Buffer(vga_palette_buffer.as_entire_buffer_binding()),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::Buffer(window_size_buffer.as_entire_buffer_binding()),
                },
            ],
            label: Some("diffuse_bind_group"),
        });

        let render_pipeline = load_render_pipeline(
            &device,
            surface_format,
            &texture_bind_group_layout,
            include_str!("shader.wgsl"),
        )
        .await
        .expect("shader should compile");

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(VERTICES),
            usage: BufferUsages::VERTEX,
        });

        let result = Self {
            surface,
            device,
            queue,
            config,
            size,
            render_pipeline,
            vertex_buffer,
            dac_palette_buffer,
            vga_palette_buffer,
            window_size_buffer,
            diffuse_texture: font_texture,
            diffuse_bind_group,
            mode_buffer,
            modeset_recv,
            memory,
            frame_buffer,
            surface_format,
            start: Instant::now(),
            texture_bind_group_layout,
            last_vram_clean: Instant::now(),
            need_full_update: true,
        };

        result.write_mode(&result.modeset_recv.last_modeset);
        result
    }

    pub async fn reload_pipeline(&mut self, shader: &str) {
        if let Some(render_pipeline) =
            load_render_pipeline(&self.device, self.surface_format, &self.texture_bind_group_layout, shader).await
        {
            self.render_pipeline = render_pipeline;
        }
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);

            self.queue.write_buffer(
                &self.window_size_buffer,
                0,
                bytemuck::bytes_of(&[new_size.width, new_size.height]),
            );
        }
    }

    fn mode(&self) -> ModeConfig {
        let mode = self.modeset_recv.get();
        let blink = mode.enable_blink && (self.start.elapsed().as_millis() % 1000 < 500);
        let (width, height) = if mode.is_graphics {
            (mode.width, mode.height)
        } else {
            (mode.width * 8, mode.height * 8)
        };

        ModeConfig {
            width,
            height,
            mode_options: ModeOptions::new(blink, mode.is_graphics, mode.force_43_aspect_ratio, mode.memory_addressing),
            _reserved: Default::default(),
        }
    }

    pub fn update(&mut self) {
        if let Some(new_mode) = self.modeset_recv.try_update() {
            trace!("CGA: Modeset {new_mode:X?}");

            let new_mode = *new_mode;
            self.write_mode(&new_mode);
            self.need_full_update = true;
        }

        let ranges = if self.need_full_update {
            // Copy entire vram, ignoring clean/dirty pages.
            // This is needed when the surface was destroyed/changed.
            self.need_full_update = false;
            EitherIter::Left(once(0..self.memory.size() as u32))
        } else if self.last_vram_clean.elapsed() > Duration::from_secs(10) {
            self.last_vram_clean = Instant::now();
            self.need_full_update = true;
            EitherIter::Right(EitherIter::Left(self.memory.get_and_clear_dirty_ranges()))
        } else {
            EitherIter::Right(EitherIter::Right(self.memory.dirty_ranges()))
        };

        let start_address = self.modeset_recv.get().start_address;
        for mut range in ranges {
            // Do not copy memory we are not planning to display
            if range.end <= start_address {
                continue
            } else if range.start < start_address {
                range.start = start_address;
            }

            let size = NonZero::new(range.len() as u64).unwrap();
            let mut w = self
                .queue
                .write_buffer_with(&self.frame_buffer, (range.start - start_address) as u64, size)
                .unwrap();
            self.memory.read_slice(range.start, &mut w);
        }
    }

    fn write_mode(&self, new_mode: &ModeSet) {
        self.queue
            .write_buffer(&self.dac_palette_buffer, 0, cast_slice(&new_mode.dac_palette));
        self.queue
            .write_buffer(&self.vga_palette_buffer, 0, bytes_of(&new_mode.vga_palette.map(|n| n as u32)));

        self.queue.write_buffer(&self.mode_buffer, 0, bytes_of(&self.mode()));
    }

    pub fn render(&mut self, pre_present_notify: impl FnOnce()) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.render_to_view(view)?;
        pre_present_notify();
        output.present();

        Ok(())
    }

    pub fn render_to_view(&mut self, view: TextureView) -> Result<(), wgpu::SurfaceError> {
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.1,
                            g: 0.2,
                            b: 0.3,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_bind_group(0, &self.diffuse_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            render_pass.draw(0..VERTICES.len() as u32, 0..1);
        }

        self.queue.submit(iter::once(encoder.finish()));

        Ok(())
    }

    pub fn capture(&mut self, size: PhysicalSize<u32>) -> (u32, u32, Vec<u8>) {
        let texture = self.device.create_texture(&TextureDescriptor {
            label: Some("Offscreen Render Texture"),
            size: Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: self.config.format, // e.g., TextureFormat::Bgra8Unorm
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let view = texture.create_view(&TextureViewDescriptor::default());
        self.render_to_view(view).unwrap();

        let buffer_desc = wgpu::BufferDescriptor {
            size: (4 * size.width * size.height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            label: Some("Screenshot Buffer"),
            mapped_at_creation: false,
        };
        let buffer = self.device.create_buffer(&buffer_desc);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &buffer,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * size.width),
                    rows_per_image: Some(size.height),
                },
            },
            wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let buffer_slice = buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |v| {
            sender.send(v).unwrap();
        });
        self.device
            .poll(PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .unwrap();
        receiver.recv().unwrap().unwrap();

        let data = buffer_slice.get_mapped_range();
        (size.width, size.height, data.to_vec())
    }

    pub fn into_inner(self) -> ModeSetReceiver {
        self.modeset_recv
    }
}

async fn load_render_pipeline(
    device: &wgpu::Device, format: TextureFormat, texture_bind_group_layout: &wgpu::BindGroupLayout, shader: &str,
) -> Option<wgpu::RenderPipeline> {
    let mut compilation_failed = false;
    device.push_error_scope(ErrorFilter::Validation);
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Shader"),
        source: wgpu::ShaderSource::Wgsl(shader.into()),
    });

    for msg in shader.get_compilation_info().await.messages {
        error!("Message: {}", msg.message);
        println!("Message: {}", msg.message);

        compilation_failed |= msg.message_type == CompilationMessageType::Error;
    }

    device.pop_error_scope().await;

    if compilation_failed {
        return None
    }

    let render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Render Pipeline Layout"),
        bind_group_layouts: &[texture_bind_group_layout],
        push_constant_ranges: &[],
    });

    Some(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Render Pipeline"),
        layout: Some(&render_pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[Vertex::desc()],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent::REPLACE,
                    alpha: wgpu::BlendComponent::REPLACE,
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview: None,
        cache: None,
    }))
}

#[derive(Copy, Clone, Debug, Default)]
pub struct MouseState {
    pub left_pressed: bool,
    pub right_pressed: bool,
}

impl MouseState {
    pub fn event_from_delta(&mut self, new: MouseState, delta: PhysicalPosition<f64>) -> MouseMove {
        *self = new;

        MouseMove {
            left_pressed: new.left_pressed,
            right_pressed: new.right_pressed,
            x: delta.x,
            y: delta.y,
            z: 0.,
        }
    }

    pub fn scroll_event(&mut self, z_delta: f32) -> MouseMove {
        MouseMove {
            left_pressed: self.left_pressed,
            right_pressed: self.right_pressed,
            x: 0.,
            y: 0.,
            z: z_delta,
        }
    }
}

pub struct ModeSetReceiver {
    recv: Receiver<ModeSet>,
    last_modeset: ModeSet,
}

impl ModeSetReceiver {
    pub fn new(recv: Receiver<ModeSet>) -> Self {
        Self {
            recv,
            last_modeset: ModeSet::default(),
        }
    }

    pub fn try_update(&mut self) -> Option<&ModeSet> {
        let mut updated = false;
        while let Ok(new_mode) = self.recv.try_recv() {
            updated = true;
            self.last_modeset = new_mode;
            trace!("CGA: Modeset {new_mode:X?}");
        }

        if updated { Some(&self.last_modeset) } else { None }
    }

    fn get(&self) -> &ModeSet {
        &self.last_modeset
    }
}
