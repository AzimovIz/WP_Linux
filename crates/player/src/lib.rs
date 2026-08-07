//! Shared GPU compositor for wallpaper projects: builds the image/xray
//! render pipelines, loads a project's layers onto the GPU, and composites
//! them into whatever render target the caller hands it. Used by:
//!
//!  - `render-server`: draws into an offscreen canvas, then reads the
//!    result back to the CPU to serve over HTTP.
//!  - `player`'s own `main.rs`: draws straight into a wlr-layer-shell
//!    surface via `wgpu::Surface::present()`.
//!  - `editor`: draws into a small offscreen texture registered directly
//!    with egui-wgpu's renderer, for a live project preview.
//!
//! This exists so the actual compositing logic -- pipelines, shaders,
//! layer loading, gif timing, xray cursor handling, parallax panning --
//! lives in exactly one place instead of three copies that would drift
//! out of sync.
//!
//! This crate owns the workspace's only direct `wgpu` dependency and
//! re-exports it as [`wgpu`] so every consumer resolves to the exact same
//! wgpu version -- including `editor`'s `eframe`/`egui-wgpu`, which brings
//! its own wgpu version requirement. Two different `wgpu` crate versions
//! linked into one binary means two incompatible `Device`/`Texture`/etc.
//! types that can't be passed to each other's APIs at all, so importing
//! wgpu types via this re-export (instead of adding a second direct `wgpu`
//! dependency elsewhere in the workspace) is load-bearing, not just tidy.

pub use wgpu;

/// Format required by any offscreen render target that gets read back to
/// the CPU (render-server's canvas) or registered directly as an
/// egui-wgpu native texture (editor's preview, which specifically
/// requires `Rgba8Unorm` -- see `egui_wgpu::Renderer::register_native_texture`).
/// Not tied to any real display, so there's no reason to match an
/// on-screen swapchain's typical `Bgra8UnormSrgb`.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use image::AnimationDecoder;
use project_format::{Layer, Project};

const IMAGE_SHADER: &str = include_str!("shader.wgsl");
const XRAY_SHADER: &str = include_str!("xray.wgsl");
const PARALLAX_SHADER: &str = include_str!("parallax.wgsl");

pub struct GifFrame {
    rgba: Vec<u8>,
    delay_ms: u64,
}

/// A single opaque texture drawn full-canvas -- backs both `Image` and
/// `Gif` layers (a gif is just an image layer whose texture contents get
/// replaced as the animation advances).
pub struct ImageLayer {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

/// A base picture with a second picture ("overlay") only visible in a
/// circle around the cursor.
pub struct XrayLayer {
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    radius: f32,
    pub width: u32,
    pub height: u32,
    // Kept alive only so the textures the bind group points at aren't
    // dropped -- their contents are never read back on the Rust side.
    _base_texture: wgpu::Texture,
    _overlay_texture: wgpu::Texture,
}

/// A single picture panned opposite the cursor to fake depth -- see
/// [`Layer::Parallax`](project_format::Layer::Parallax).
pub struct ParallaxLayer {
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    strength: f32,
    smoothing: f32,
    // Eased pan target, in the same `[-strength, strength]` UV units as
    // what ends up in the uniform buffer -- kept here (not just written
    // straight through) so `update_parallax` can ease towards a new
    // cursor position frame over frame instead of snapping to it.
    current_offset: (f32, f32),
    pub width: u32,
    pub height: u32,
    // Kept alive only so the texture the bind group points at isn't
    // dropped -- its contents are never read back on the Rust side.
    _texture: wgpu::Texture,
}

pub enum LoadedLayer {
    Image(ImageLayer),
    Gif {
        image: ImageLayer,
        frames: Vec<GifFrame>,
        // Running total of delay_ms up to and including frame i --
        // lets the current frame be found from wall-clock elapsed time
        // alone, so callers don't need to track per-tick state (and
        // several callers redrawing at different, uncoordinated rates
        // can't drift out of sync with each other).
        cumulative_ms: Vec<u64>,
        total_ms: u64,
        current: usize,
        width: u32,
        height: u32,
    },
    Xray(XrayLayer),
    Parallax(ParallaxLayer),
}

pub enum DrawLayer<'a> {
    Image(&'a ImageLayer),
    Xray(&'a XrayLayer),
    Parallax(&'a ParallaxLayer),
}

impl LoadedLayer {
    /// Pixel dimensions of this layer's own source asset(s) -- for
    /// `Xray`, its base picture. Callers that need one canvas size for a
    /// whole scene (render-server, editor) use the first layer's size,
    /// matching how render-server has always picked a canvas size: from
    /// whichever layer happens to load first.
    pub fn size(&self) -> (u32, u32) {
        match self {
            LoadedLayer::Image(image) => (image.width, image.height),
            LoadedLayer::Gif { width, height, .. } => (*width, *height),
            LoadedLayer::Xray(xray) => (xray.width, xray.height),
            LoadedLayer::Parallax(parallax) => (parallax.width, parallax.height),
        }
    }

    /// Changes an already-loaded Xray layer's mask radius in place -- no
    /// GPU reload needed, just a plain field write picked up next time
    /// [`SceneRenderer::update_xray_cursors`] writes the uniform buffer.
    /// No-op on any other layer kind. Meant for a caller like editor that
    /// wants a radius slider to feel live without re-decoding images and
    /// re-uploading textures on every frame of a drag.
    pub fn set_xray_radius(&mut self, radius: f32) {
        if let LoadedLayer::Xray(xray) = self {
            xray.radius = radius;
        }
    }

    /// Changes an already-loaded Parallax layer's strength/smoothing in
    /// place, same rationale as [`LoadedLayer::set_xray_radius`] -- lets a
    /// caller like editor keep sliders feeling live without reloading the
    /// picture. No-op on any other layer kind.
    pub fn set_parallax_params(&mut self, strength: f32, smoothing: f32) {
        if let LoadedLayer::Parallax(parallax) = self {
            parallax.strength = strength;
            parallax.smoothing = smoothing;
        }
    }
}

/// Owns the GPU device/pipelines used to composite a project's layers.
/// Cheap to keep around for the life of a process -- construct once, load
/// as many scenes (`Vec<LoadedLayer>`) against it as you like.
pub struct SceneRenderer {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    image_pipeline: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    xray_pipeline: wgpu::RenderPipeline,
    xray_bind_group_layout: wgpu::BindGroupLayout,
    parallax_pipeline: wgpu::RenderPipeline,
    parallax_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl SceneRenderer {
    /// For a headless consumer with no on-screen surface (render-server):
    /// picks an adapter itself, preferring the integrated GPU -- see
    /// [`pick_adapter`] for why that matters on hybrid-GPU laptops.
    pub async fn new_headless(target_format: wgpu::TextureFormat) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pick_adapter(&instance).await;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("failed to open device");
        Self::build(device, queue, adapter, target_format)
    }

    /// For a consumer presenting to an on-screen surface (player): needs
    /// `request_adapter`'s surface-compatibility filtering, so adapter
    /// selection is left to it rather than to [`pick_adapter`].
    pub async fn new_with_surface(
        instance: &wgpu::Instance,
        compatible_surface: &wgpu::Surface<'_>,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(compatible_surface),
                ..Default::default()
            })
            .await
            .expect("failed to find a suitable GPU adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("failed to open device");
        Self::build(device, queue, adapter, target_format)
    }

    /// For a consumer that already has a device/queue/adapter from
    /// elsewhere (editor: eframe's own `egui_wgpu::RenderState`) and just
    /// wants pipelines built against them, so its rendering shares the
    /// exact same GPU context as the rest of the app -- no second GPU
    /// context, no readback needed to get pixels into egui.
    pub fn from_existing(
        device: wgpu::Device,
        queue: wgpu::Queue,
        adapter: wgpu::Adapter,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        Self::build(device, queue, adapter, target_format)
    }

    fn build(
        device: wgpu::Device,
        queue: wgpu::Queue,
        adapter: wgpu::Adapter,
        target_format: wgpu::TextureFormat,
    ) -> Self {
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
                    format: target_format,
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
                    format: target_format,
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

        let parallax_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("parallax-bind-group-layout"),
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
                ],
            });

        let parallax_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("parallax-shader"),
            source: wgpu::ShaderSource::Wgsl(PARALLAX_SHADER.into()),
        });

        let parallax_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("parallax-pipeline-layout"),
                bind_group_layouts: &[Some(&parallax_bind_group_layout)],
                immediate_size: 0,
            });

        let parallax_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("parallax-pipeline"),
            layout: Some(&parallax_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &parallax_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &parallax_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
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
            adapter,
            device,
            queue,
            image_pipeline,
            image_bind_group_layout,
            xray_pipeline,
            xray_bind_group_layout,
            parallax_pipeline,
            parallax_bind_group_layout,
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

    fn create_image_layer(&self, rgba: &[u8], width: u32, height: u32) -> ImageLayer {
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
        ImageLayer {
            texture,
            bind_group,
            width,
            height,
        }
    }

    /// Replaces an image layer's pixel contents in place (used to advance
    /// gif frames) -- `width`/`height` must match what it was created
    /// with.
    fn update_image_layer(&self, layer: &ImageLayer, rgba: &[u8], width: u32, height: u32) {
        self.write_texture(&layer.texture, rgba, width, height);
    }

    #[allow(clippy::too_many_arguments)]
    fn create_xray_layer(
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
            width: base_width,
            height: base_height,
            _base_texture: base_texture,
            _overlay_texture: overlay_texture,
        }
    }

    fn create_parallax_layer(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
        strength: f32,
        smoothing: f32,
    ) -> ParallaxLayer {
        let texture = self.create_texture(width, height);
        self.write_texture(&texture, rgba, width, height);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("parallax-params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("parallax-layer-bind-group"),
            layout: &self.parallax_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        });

        ParallaxLayer {
            bind_group,
            uniform_buffer,
            strength,
            smoothing,
            current_offset: (0.0, 0.0),
            width,
            height,
            _texture: texture,
        }
    }

    /// Loads every layer of `project` onto the GPU, ready to draw. Asset
    /// paths in `project` are resolved relative to `project_dir`.
    pub fn load_scene(
        &self,
        project_dir: &Path,
        project: &Project,
    ) -> Result<Vec<LoadedLayer>, String> {
        let mut layers = Vec::with_capacity(project.layers.len());
        for layer in &project.layers {
            match layer {
                Layer::Image { path } => {
                    let (rgba, width, height) = open_rgba(&project_dir.join(path))?;
                    layers.push(LoadedLayer::Image(self.create_image_layer(&rgba, width, height)));
                }
                Layer::Gif { path } => {
                    let (frames, width, height) = decode_gif(&project_dir.join(path))?;
                    let cumulative_ms = cumulative_delays(&frames);
                    let total_ms = *cumulative_ms.last().expect("gif has at least one frame");
                    let image = self.create_image_layer(&frames[0].rgba, width, height);
                    layers.push(LoadedLayer::Gif {
                        image,
                        frames,
                        cumulative_ms,
                        total_ms,
                        current: 0,
                        width,
                        height,
                    });
                }
                Layer::Xray {
                    base,
                    overlay,
                    radius,
                } => {
                    let (base_rgba, base_width, base_height) = open_rgba(&project_dir.join(base))?;
                    let (overlay_rgba, overlay_width, overlay_height) =
                        open_rgba(&project_dir.join(overlay))?;
                    layers.push(LoadedLayer::Xray(self.create_xray_layer(
                        &base_rgba,
                        base_width,
                        base_height,
                        &overlay_rgba,
                        overlay_width,
                        overlay_height,
                        *radius,
                    )));
                }
                Layer::Parallax {
                    path,
                    strength,
                    smoothing,
                } => {
                    let (rgba, width, height) = open_rgba(&project_dir.join(path))?;
                    layers.push(LoadedLayer::Parallax(self.create_parallax_layer(
                        &rgba, width, height, *strength, *smoothing,
                    )));
                }
            }
        }
        Ok(layers)
    }

    /// Advances any gif layers to whatever frame `elapsed_ms` (time since
    /// the scene was loaded) lands on, uploading a new texture only when
    /// the frame actually changed. Returns whether anything changed, so a
    /// caller that only wants to redraw on actual change (render-server)
    /// can act on it -- callers that redraw unconditionally anyway
    /// (player, editor) can ignore it.
    pub fn advance_gifs(&self, layers: &mut [LoadedLayer], elapsed_ms: u64) -> bool {
        let mut any_changed = false;
        for layer in layers {
            if let LoadedLayer::Gif {
                image,
                frames,
                cumulative_ms,
                total_ms,
                current,
                width,
                height,
            } = layer
            {
                let t = elapsed_ms % *total_ms;
                let index = cumulative_ms.partition_point(|&c| c <= t).min(frames.len() - 1);
                if index != *current {
                    *current = index;
                    self.update_image_layer(image, &frames[index].rgba, *width, *height);
                    any_changed = true;
                }
            }
        }
        any_changed
    }

    /// Updates every xray layer's cursor uniform to the same `cursor_px`
    /// (canvas/target pixel coordinates) -- every consumer of this crate
    /// only ever drives one shared cursor position across all xray layers
    /// in a scene, so there's no per-layer cursor to plumb through.
    pub fn update_xray_cursors(&self, layers: &[LoadedLayer], cursor_px: (f32, f32)) {
        for layer in layers {
            if let LoadedLayer::Xray(xray) = layer {
                let mut bytes = [0u8; 16];
                bytes[0..4].copy_from_slice(&cursor_px.0.to_le_bytes());
                bytes[4..8].copy_from_slice(&cursor_px.1.to_le_bytes());
                bytes[8..12].copy_from_slice(&xray.radius.to_le_bytes());
                self.queue.write_buffer(&xray.uniform_buffer, 0, &bytes);
            }
        }
    }

    /// Eases every parallax layer's pan towards a cursor-driven target
    /// and writes the result into its uniform buffer. `cursor_px` mirrors
    /// `update_xray_cursors` -- one shared cursor position, in the
    /// layer's own pixel dimensions (not the output surface's, matching
    /// how xray's `radius` is likewise authored against the layer's
    /// native resolution). `dt_ms` is elapsed time since the *previous*
    /// call to this method, not since the scene was loaded -- easing
    /// needs a delta, unlike gif timing's running total.
    ///
    /// Returns whether any layer's pan actually moved -- mirrors
    /// [`SceneRenderer::advance_gifs`]'s return for the same reason: a
    /// caller like render-server that only redraws on real change can't
    /// just compare `cursor_px` to the last one it saw (unlike xray,
    /// smoothing means the pan can still be mid-ease towards an old
    /// target after the cursor itself has stopped moving).
    pub fn update_parallax(&self, layers: &mut [LoadedLayer], cursor_px: (f32, f32), dt_ms: u64) -> bool {
        // Below this, the pan's uniform-buffer bytes are indistinguishable
        // from where it already was -- calling it "settled" here instead
        // of only at exact equality is what lets an exponential ease
        // (which in theory never fully reaches its target) stop
        // triggering redraws.
        const SETTLED_EPSILON: f32 = 1.0e-4;

        let mut any_changed = false;
        let dt = dt_ms as f32 / 1000.0;
        for layer in layers {
            if let LoadedLayer::Parallax(parallax) = layer {
                // Cursor at the near edge of the layer -> target =
                // -strength; far edge -> +strength. In the shader this
                // pans the *sampled window* the same direction the
                // cursor moved, which makes the visible content shift
                // the other way -- the usual "background drifts opposite
                // your viewpoint" parallax feel. Flip `strength`'s sign
                // to invert that for a layer meant to feel like it's in
                // front of the cursor instead of behind it.
                // Clamped to `strength`'s own range: besides being the
                // contract the zoom margin is sized against, this also
                // keeps a cursor pushed far off-canvas (every caller's
                // way of saying "hide/neutral" when the pointer leaves,
                // e.g. `App::pointer_frame`'s `Leave` handling) from
                // producing a wildly out-of-range pan -- it just parks
                // at the nearest edge instead.
                let max_offset = parallax.strength.abs();
                let target = (
                    ((cursor_px.0 / parallax.width.max(1) as f32 - 0.5) * 2.0 * parallax.strength)
                        .clamp(-max_offset, max_offset),
                    ((cursor_px.1 / parallax.height.max(1) as f32 - 0.5) * 2.0 * parallax.strength)
                        .clamp(-max_offset, max_offset),
                );
                let ease = if parallax.smoothing <= 0.0 {
                    1.0
                } else {
                    1.0 - (-dt / parallax.smoothing).exp()
                };
                let step = (
                    (target.0 - parallax.current_offset.0) * ease,
                    (target.1 - parallax.current_offset.1) * ease,
                );
                if step.0.abs() > SETTLED_EPSILON || step.1.abs() > SETTLED_EPSILON {
                    parallax.current_offset.0 += step.0;
                    parallax.current_offset.1 += step.1;
                    any_changed = true;

                    let zoom = zoom_for_strength(parallax.strength);
                    let mut bytes = [0u8; 16];
                    bytes[0..4].copy_from_slice(&parallax.current_offset.0.to_le_bytes());
                    bytes[4..8].copy_from_slice(&parallax.current_offset.1.to_le_bytes());
                    bytes[8..12].copy_from_slice(&zoom.to_le_bytes());
                    self.queue.write_buffer(&parallax.uniform_buffer, 0, &bytes);
                }
            }
        }
        any_changed
    }

    /// Records a composite pass -- `layers` bottom to top, alpha-blended
    /// -- into `target_view`, as part of `encoder`. Low-level: lets a
    /// caller that needs to record more commands in the same submission
    /// (render-server chains a copy-to-buffer for CPU readback) do so.
    /// Most callers want [`SceneRenderer::render_to_texture`] instead.
    pub fn record_draw(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        layers: &[LoadedLayer],
        clear_color: wgpu::Color,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("composite-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear_color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        for layer in layers {
            let draw_layer = match layer {
                LoadedLayer::Image(image) => DrawLayer::Image(image),
                LoadedLayer::Gif { image, .. } => DrawLayer::Image(image),
                LoadedLayer::Xray(xray) => DrawLayer::Xray(xray),
                LoadedLayer::Parallax(parallax) => DrawLayer::Parallax(parallax),
            };
            match draw_layer {
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
                DrawLayer::Parallax(parallax) => {
                    pass.set_pipeline(&self.parallax_pipeline);
                    pass.set_bind_group(0, &parallax.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
        }
    }

    /// Composites `layers` into `target_view` and submits immediately --
    /// the common case for a caller that isn't chaining any other GPU
    /// work onto the same submission (player, editor).
    pub fn render_to_texture(
        &self,
        target_view: &wgpu::TextureView,
        layers: &[LoadedLayer],
        clear_color: wgpu::Color,
    ) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.record_draw(&mut encoder, target_view, layers, clear_color);
        self.queue.submit(Some(encoder.finish()));
    }
}

/// Picks which GPU actually does the rendering, instead of leaving it to
/// `request_adapter`'s default heuristics. Only used by
/// [`SceneRenderer::new_headless`] -- a consumer with an on-screen surface
/// needs `request_adapter`'s surface-compatibility filtering instead (see
/// [`SceneRenderer::new_with_surface`]).
///
/// This matters specifically on hybrid-GPU laptops (integrated + discrete,
/// e.g. Ryzen APU + NVIDIA Optimus): with no `compatible_surface` to guide
/// it, `request_adapter` may hand back the discrete GPU. Under a
/// power-saving profile that GPU is typically runtime-suspended between
/// uses, so every render tick can pay a GPU wake-up (D3cold resume) on top
/// of the actual draw -- easily hundreds of ms, which is consistent with
/// single-digit fps despite the workload here being a handful of
/// alpha-blended full-screen quads. The integrated GPU doesn't have that
/// suspend/resume path and is more than enough for this workload, so it's
/// preferred by default.
///
/// Set `WPLINUX_GPU=<substring>` to force a specific adapter instead (case
/// insensitive match against the adapter name, e.g. `WPLINUX_GPU=nvidia`)
/// -- useful for comparing GPUs on the same machine.
pub async fn pick_adapter(instance: &wgpu::Instance) -> wgpu::Adapter {
    let mut candidates = instance.enumerate_adapters(wgpu::Backends::all()).await;
    for adapter in &candidates {
        let info = adapter.get_info();
        eprintln!(
            "candidate adapter {:?} ({:?}, backend {:?})",
            info.name, info.device_type, info.backend
        );
    }

    if let Ok(want) = std::env::var("WPLINUX_GPU") {
        let want_lower = want.to_lowercase();
        if let Some(pos) = candidates
            .iter()
            .position(|a| a.get_info().name.to_lowercase().contains(&want_lower))
        {
            let adapter = candidates.remove(pos);
            eprintln!("WPLINUX_GPU={want:?} matched adapter {:?}", adapter.get_info().name);
            return adapter;
        }
        eprintln!("WPLINUX_GPU={want:?} matched no candidate adapter, falling back to automatic selection");
    }

    candidates.sort_by_key(|a| adapter_rank(a.get_info().device_type));
    candidates
        .into_iter()
        .next()
        .expect("no GPU adapters found -- is Vulkan/GL available in this session?")
}

/// Lower sorts first, i.e. gets picked -- integrated over discrete over
/// everything else, software/CPU dead last (see `pick_adapter`).
fn adapter_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::IntegratedGpu => 0,
        wgpu::DeviceType::DiscreteGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    }
}

/// The zoom-in factor that reserves just enough margin around a panned
/// picture that a pan of up to `strength` (a `Layer::Parallax::strength`
/// value) never samples outside `[0, 1]` UV. Comes from requiring the
/// sampled window `[0.5 + offset - 0.5/zoom, 0.5 + offset + 0.5/zoom]` to
/// stay inside `[0, 1]` at `offset = strength.abs()`, which reduces to
/// `zoom = 1 / (1 - 2 * strength)`. Clamped so a `strength` at or past
/// 0.5 (which would need infinite zoom) degrades to a tight but still
/// valid crop instead of dividing by zero or going negative.
fn zoom_for_strength(strength: f32) -> f32 {
    1.0 / (1.0 - 2.0 * strength.abs()).max(0.1)
}

fn open_rgba(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let image = image::open(path)
        .map_err(|e| format!("failed to open image {path:?}: {e}"))?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

fn decode_gif(path: &Path) -> Result<(Vec<GifFrame>, u32, u32), String> {
    let file = File::open(path).map_err(|e| format!("failed to open gif {path:?}: {e}"))?;
    let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file))
        .map_err(|e| format!("failed to decode gif {path:?}: {e}"))?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|e| format!("failed to decode gif frames {path:?}: {e}"))?;
    if frames.is_empty() {
        return Err(format!("gif {path:?} has no frames"));
    }

    let (width, height) = frames[0].buffer().dimensions();
    let gif_frames = frames
        .into_iter()
        .map(|frame| {
            let (numer, denom) = frame.delay().numer_denom_ms();
            let delay_ms = if denom == 0 {
                100
            } else {
                (numer / denom).max(20) as u64
            };
            GifFrame {
                rgba: frame.into_buffer().into_raw(),
                delay_ms,
            }
        })
        .collect();
    Ok((gif_frames, width, height))
}

/// Running total of `delay_ms` up to and including each frame -- see
/// [`SceneRenderer::advance_gifs`].
fn cumulative_delays(frames: &[GifFrame]) -> Vec<u64> {
    let mut total = 0u64;
    frames
        .iter()
        .map(|f| {
            total += f.delay_ms;
            total
        })
        .collect()
}
