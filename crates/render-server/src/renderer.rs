//! Headless wgpu rendering: composites a project's layers (bottom to
//! top, alpha-blended) into an offscreen canvas and reads the result
//! back to the CPU as raw, tightly-packed RGBA bytes. No window, no
//! surface -- this never touches Wayland at all. PNG/BMP encoding
//! happens in main.rs, not here.

const IMAGE_SHADER: &str = include_str!("shader.wgsl");
const XRAY_SHADER: &str = include_str!("xray.wgsl");

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    image_pipeline: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    xray_pipeline: wgpu::RenderPipeline,
    xray_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

/// A single opaque texture drawn full-canvas -- backs both `Image` and
/// `Gif` layers (a gif is just an image layer whose texture contents get
/// replaced every tick as the animation advances).
pub struct ImageLayer {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// A base picture with a second picture ("overlay") only visible in a
/// circle around the cursor.
pub struct XrayLayer {
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    radius: f32,
    // Kept alive only so the textures the bind group points at aren't
    // dropped -- their contents are never read back on the Rust side.
    _base_texture: wgpu::Texture,
    _overlay_texture: wgpu::Texture,
}

pub enum DrawLayer<'a> {
    Image(&'a ImageLayer),
    Xray(&'a XrayLayer),
}

impl Renderer {
    pub fn new() -> Self {
        pollster::block_on(Self::new_async())
    }

    async fn new_async() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        // Purely offscreen: no surface needed to pick an adapter.
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: None,
                ..Default::default()
            })
            .await
            .expect("failed to find a suitable GPU adapter");

        let info = adapter.get_info();
        eprintln!(
            "render-server: using adapter {:?} ({:?}, backend {:?})",
            info.name, info.device_type, info.backend
        );
        if matches!(info.device_type, wgpu::DeviceType::Cpu) {
            eprintln!(
                "render-server: WARNING -- this is a software/CPU adapter, not a real GPU. \
                 Rendering will be slow and will load the CPU heavily. If a real GPU is \
                 available, this usually means Vulkan/EGL can't see it in this session."
            );
        }

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("failed to open device");

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scene-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let image_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("image-bind-group-layout"),
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

        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image-shader"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER.into()),
        });

        let image_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("image-pipeline-layout"),
                bind_group_layouts: &[Some(&image_bind_group_layout)],
                immediate_size: 0,
            });

        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image-pipeline"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let xray_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("xray-bind-group-layout"),
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
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let xray_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xray-shader"),
            source: wgpu::ShaderSource::Wgsl(XRAY_SHADER.into()),
        });

        let xray_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("xray-pipeline-layout"),
            bind_group_layouts: &[Some(&xray_bind_group_layout)],
            immediate_size: 0,
        });

        let xray_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xray-pipeline"),
            layout: Some(&xray_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &xray_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &xray_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            device,
            queue,
            image_pipeline,
            image_bind_group_layout,
            xray_pipeline,
            xray_bind_group_layout,
            sampler,
        }
    }

    fn create_texture(&self, width: u32, height: u32) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layer-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn write_texture(&self, texture: &wgpu::Texture, rgba: &[u8], width: u32, height: u32) {
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
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn create_image_layer(&self, rgba: &[u8], width: u32, height: u32) -> ImageLayer {
        let texture = self.create_texture(width, height);
        self.write_texture(&texture, rgba, width, height);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image-layer-bind-group"),
            layout: &self.image_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        ImageLayer { texture, bind_group }
    }

    /// Replaces an image layer's pixel contents in place (used to
    /// advance gif frames) -- `width`/`height` must match what it was
    /// created with.
    pub fn update_image_layer(&self, layer: &ImageLayer, rgba: &[u8], width: u32, height: u32) {
        self.write_texture(&layer.texture, rgba, width, height);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_xray_layer(
        &self,
        base_rgba: &[u8],
        base_width: u32,
        base_height: u32,
        overlay_rgba: &[u8],
        overlay_width: u32,
        overlay_height: u32,
        radius: f32,
    ) -> XrayLayer {
        let base_texture = self.create_texture(base_width, base_height);
        self.write_texture(&base_texture, base_rgba, base_width, base_height);
        let base_view = base_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let overlay_texture = self.create_texture(overlay_width, overlay_height);
        self.write_texture(&overlay_texture, overlay_rgba, overlay_width, overlay_height);
        let overlay_view = overlay_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xray-params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xray-layer-bind-group"),
            layout: &self.xray_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&base_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&overlay_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        });

        XrayLayer {
            bind_group,
            uniform_buffer,
            radius,
            _base_texture: base_texture,
            _overlay_texture: overlay_texture,
        }
    }

    /// Updates the cursor position (in canvas pixel coordinates) used
    /// for this xray layer's mask. Pass a far off-canvas value to fully
    /// hide the overlay (e.g. when the pointer isn't over the
    /// wallpaper).
    pub fn update_xray_cursor(&self, layer: &XrayLayer, cursor_px: (f32, f32)) {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&cursor_px.0.to_le_bytes());
        bytes[4..8].copy_from_slice(&cursor_px.1.to_le_bytes());
        bytes[8..12].copy_from_slice(&layer.radius.to_le_bytes());
        self.queue.write_buffer(&layer.uniform_buffer, 0, &bytes);
    }

    /// Allocates the render target and readback buffer for a canvas of
    /// this size, once, so a continuous render loop (gif/xray) isn't
    /// churning fresh GPU allocations every single tick.
    pub fn create_canvas(&self, width: u32, height: u32) -> Canvas {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let target_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("render-target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Rows in a copy-to-buffer destination must be padded to a
        // multiple of COPY_BYTES_PER_ROW_ALIGNMENT; the image itself has
        // no such requirement, so we strip the padding back out on
        // readback.
        let unpadded_bytes_per_row = 4 * width;
        let padding = (wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
            - unpadded_bytes_per_row % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row + padding;

        let readback_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Canvas {
            width,
            height,
            target_texture,
            target_view,
            readback_buffer,
            padded_bytes_per_row,
            unpadded_bytes_per_row,
        }
    }

    /// Composites all layers (bottom to top) into `canvas` and reads the
    /// result back as tightly-packed RGBA bytes.
    pub fn render_frame(&self, canvas: &Canvas, layers: &[DrawLayer]) -> Vec<u8> {
        let size = wgpu::Extent3d {
            width: canvas.width,
            height: canvas.height,
            depth_or_array_layers: 1,
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &canvas.target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            for layer in layers {
                match layer {
                    DrawLayer::Image(image) => {
                        pass.set_pipeline(&self.image_pipeline);
                        pass.set_bind_group(0, &image.bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                    DrawLayer::Xray(xray) => {
                        pass.set_pipeline(&self.xray_pipeline);
                        pass.set_bind_group(0, &xray.bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                }
            }
        }

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &canvas.target_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &canvas.readback_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(canvas.padded_bytes_per_row),
                    rows_per_image: Some(canvas.height),
                },
            },
            size,
        );

        self.queue.submit(Some(encoder.finish()));

        let (tx, rx) = std::sync::mpsc::channel();
        canvas
            .readback_buffer
            .map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = tx.send(result);
            });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll failed");
        rx.recv()
            .expect("map_async callback never fired")
            .expect("failed to map readback buffer");

        let mut pixels =
            Vec::with_capacity((canvas.unpadded_bytes_per_row * canvas.height) as usize);
        {
            let view = canvas
                .readback_buffer
                .get_mapped_range(..)
                .expect("buffer not mapped");
            for row in 0..canvas.height {
                let start = (row * canvas.padded_bytes_per_row) as usize;
                let end = start + canvas.unpadded_bytes_per_row as usize;
                pixels.extend_from_slice(&view[start..end]);
            }
        }
        canvas.readback_buffer.unmap();

        pixels
    }
}

/// A reusable render target + readback buffer for one canvas size.
/// Created once per loaded project and reused across every tick of a
/// continuous (gif/xray) render loop.
pub struct Canvas {
    width: u32,
    height: u32,
    target_texture: wgpu::Texture,
    target_view: wgpu::TextureView,
    readback_buffer: wgpu::Buffer,
    padded_bytes_per_row: u32,
    unpadded_bytes_per_row: u32,
}
