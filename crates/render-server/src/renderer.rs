//! Offscreen readback on top of `player::SceneRenderer`, which owns the
//! actual GPU compositing (pipelines, layer loading, gif/xray handling --
//! see its module doc comment for why that logic lives in one shared
//! place instead of being duplicated here). This module only adds what's
//! specific to render-server: an offscreen render target with no on-screen
//! surface, and reading its contents back to the CPU as raw,
//! tightly-packed RGBA bytes so `main.rs` can PNG/BMP-encode and serve
//! them over HTTP. No window, no surface -- this never touches Wayland at
//! all.

use player::wgpu;
pub use player::{LoadedLayer, SceneRenderer};

/// `player::OFFSCREEN_FORMAT` under a locally meaningful name -- happens
/// to also be exactly the byte order `image`'s encoders and the
/// hand-rolled BMP encoder in `main.rs` expect, no per-pixel channel swap
/// needed.
pub const CANVAS_FORMAT: wgpu::TextureFormat = player::OFFSCREEN_FORMAT;

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

/// Allocates the render target and readback buffer for a canvas of this
/// size, once, so a continuous render loop (gif/xray) isn't churning
/// fresh GPU allocations every single tick.
pub fn create_canvas(renderer: &SceneRenderer, width: u32, height: u32) -> Canvas {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    let target_texture = renderer.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("render-target"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: CANVAS_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Rows in a copy-to-buffer destination must be padded to a multiple
    // of COPY_BYTES_PER_ROW_ALIGNMENT; the image itself has no such
    // requirement, so we strip the padding back out on readback.
    let unpadded_bytes_per_row = 4 * width;
    let padding = (wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
        - unpadded_bytes_per_row % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row + padding;

    let readback_buffer = renderer.device.create_buffer(&wgpu::BufferDescriptor {
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
pub fn render_frame(renderer: &SceneRenderer, canvas: &Canvas, layers: &[LoadedLayer]) -> Vec<u8> {
    let size = wgpu::Extent3d {
        width: canvas.width,
        height: canvas.height,
        depth_or_array_layers: 1,
    };

    let mut encoder = renderer
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    renderer.record_draw(
        &mut encoder,
        &canvas.target_view,
        layers,
        wgpu::Color::TRANSPARENT,
    );

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

    renderer.queue.submit(Some(encoder.finish()));

    let (tx, rx) = std::sync::mpsc::channel();
    canvas
        .readback_buffer
        .map_async(wgpu::MapMode::Read, .., move |result| {
            let _ = tx.send(result);
        });
    renderer
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    rx.recv()
        .expect("map_async callback never fired")
        .expect("failed to map readback buffer");

    let mut pixels = Vec::with_capacity((canvas.unpadded_bytes_per_row * canvas.height) as usize);
    {
        let view = canvas.readback_buffer.get_mapped_range(..);
        for row in 0..canvas.height {
            let start = (row * canvas.padded_bytes_per_row) as usize;
            let end = start + canvas.unpadded_bytes_per_row as usize;
            pixels.extend_from_slice(&view[start..end]);
        }
    }
    canvas.readback_buffer.unmap();

    pixels
}
