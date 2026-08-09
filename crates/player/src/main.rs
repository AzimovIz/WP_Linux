//! Test prototype for the "render straight to the compositor" fix: loads
//! one wallpaper project (path given on the command line) and draws its
//! layers -- Image, Gif, Xray, Parallax -- directly into a wlr-layer-shell surface
//! via `wgpu::Surface::present()`. No CPU readback, no PNG/BMP encode, no
//! HTTP, no Qt image decode -- unlike the render-server + plasma/plasma-plugin
//! pipeline this exists to compare against.
//!
//! The actual compositing (pipelines, layer loading, gif timing, xray
//! cursor handling, parallax panning) lives in this crate's own `lib.rs`, shared with
//! render-server and editor -- see its module doc comment. This file is
//! just the wlr-layer-shell/Wayland-specific glue: opening a surface per
//! output, driving redraws off frame callbacks, and pointer input.
//!
//! Not wired into Plasma settings or autostart -- run by hand, pointed at
//! a project directory containing a project.json (see project-format).
//! One layer surface per connected output, all sharing a single
//! `SceneRenderer`; the loaded layers (textures, bind groups) are created
//! once and drawn on every output.
//!
//! Cursor-follow for Xray and Parallax layers uses this process's own Wayland pointer
//! input (surface-local coordinates from `wl_pointer`) rather than the
//! D-Bus `CursorBridge` render-server relies on -- simpler for a
//! throughput test, but it means the cursor appears to "stop" wherever
//! something above this background layer (desktop icons, windows) steals
//! pointer focus. Good enough to measure frame rate, not production
//! cursor behavior.
//!
//! Also simplified versus render-server: layers are stretched to fill
//! each output's surface with no aspect-ratio correction, and with
//! multiple monitors every output shares the exact same cursor pixel
//! coordinates for Xray and Parallax (render-server already only ever
//! drives one shared canvas for every screen, so this isn't a new
//! limitation).

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::Instant;

use player::wgpu;
use player::{LoadedLayer, SceneRenderer};
use project_format::Project;
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, Proxy, QueueHandle,
};

const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

struct OutputSurface {
    wl_output: wl_output::WlOutput,
    layer: LayerSurface,
    surface: wgpu::Surface<'static>,
    width: u32,
    height: u32,
    configured: bool,
}

struct App {
    conn: Connection,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,

    instance: wgpu::Instance,
    renderer: Option<SceneRenderer>,
    outputs: Vec<OutputSurface>,
    pointer: Option<wl_pointer::WlPointer>,
    start: Instant,
    // Elapsed time (since `start`) as of the last parallax ease step --
    // parallax needs a per-tick delta, unlike gif timing's running total
    // from `start`, so this tracks where the previous tick left off.
    last_parallax_update_ms: u64,
    exit: bool,

    project: Project,
    project_dir: PathBuf,
    // Empty until the first output shows up and a SceneRenderer exists to
    // build GPU-side resources on.
    layers: Vec<LoadedLayer>,
    // Surface-local pixel coordinates of the pointer, shared by every
    // output (see module doc comment) -- pushed far off-canvas while the
    // pointer isn't over any of our surfaces so Xray masks hide fully.
    cursor: (f32, f32),
}

fn main() {
    env_logger::init();

    let Some(requested_dir) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: player <project-dir>");
        std::process::exit(1);
    };
    let (project, project_dir) = Project::load(&requested_dir).unwrap_or_else(|e| {
        eprintln!("player: failed to load project {requested_dir:?}: {e}");
        std::process::exit(1);
    });
    if project.layers.is_empty() {
        eprintln!("player: project {project_dir:?} has no layers");
        std::process::exit(1);
    }

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland compositor");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("failed to init registry");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let layer_shell = LayerShell::bind(&globals, &qh)
        .expect("wlr-layer-shell not available -- is this compositor wlroots/KWin-based?");

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let mut app = App {
        conn: conn.clone(),
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        instance,
        renderer: None,
        outputs: Vec::new(),
        pointer: None,
        start: Instant::now(),
        last_parallax_update_ms: 0,
        exit: false,
        project,
        project_dir,
        layers: Vec::new(),
        cursor: (f32::MIN / 2.0, f32::MIN / 2.0),
    };

    // Populate OutputState and fire `new_output` for every already-present
    // monitor before we enter the main loop.
    event_queue.roundtrip(&mut app).expect("initial roundtrip failed");

    if app.outputs.is_empty() {
        panic!("no outputs were reported by the compositor");
    }

    log::info!(
        "running on {} output(s), {} layer(s) loaded from {:?}",
        app.outputs.len(),
        app.layers.len(),
        app.project_dir
    );

    while !app.exit {
        event_queue.blocking_dispatch(&mut app).unwrap();
    }
}

impl App {
    fn add_output(&mut self, wl_output: wl_output::WlOutput, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("wplinux-player"),
            Some(&wl_output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();

        let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.conn.backend().display_ptr() as *mut _).unwrap(),
        ));
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).unwrap(),
        ));

        let wgpu_surface = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display_handle),
                    raw_window_handle,
                })
                .expect("failed to create wgpu surface for output")
        };

        if self.renderer.is_none() {
            let renderer = pollster::block_on(SceneRenderer::new_with_surface(
                &self.instance,
                &wgpu_surface,
                SURFACE_FORMAT,
            ));
            // Scope cut: this standalone binary is a secondary/test path
            // (the primary supported path is render-server + the Plasma
            // plugin, which does enforce the trust store -- see
            // `render-server/src/trust_store.rs`). It always trusts
            // whatever project it's pointed at, since it's only ever run
            // by hand against a project directory the invoking user
            // already chose themselves.
            self.layers = renderer
                .load_scene(&self.project_dir, &self.project, true)
                .unwrap_or_else(|e| panic!("failed to load project layers: {e}"));
            self.renderer = Some(renderer);
        }

        self.outputs.push(OutputSurface {
            wl_output,
            layer,
            surface: wgpu_surface,
            width: 0,
            height: 0,
            configured: false,
        });
    }

    fn draw(&mut self, wl_surface: &wl_surface::WlSurface, qh: &QueueHandle<Self>) {
        let Some(renderer) = &self.renderer else { return };
        let Some(out) = self
            .outputs
            .iter_mut()
            .find(|o| o.layer.wl_surface() == wl_surface)
        else {
            return;
        };
        if !out.configured {
            return;
        }

        let elapsed_ms = self.start.elapsed().as_millis() as u64;
        renderer.advance_gifs(&mut self.layers, elapsed_ms);
        renderer.update_xray_cursors(&self.layers, self.cursor);
        let parallax_dt_ms = elapsed_ms.saturating_sub(self.last_parallax_update_ms);
        self.last_parallax_update_ms = elapsed_ms;
        renderer.update_parallax(&mut self.layers, self.cursor, parallax_dt_ms);
        renderer.advance_text_sources(&mut self.layers, chrono::Local::now());

        let frame = match out.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex)
            | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => tex,
            _ => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        renderer.render_to_texture(&view, &self.layers, wgpu::Color::BLACK);

        // Ask for the next frame before presenting this one.
        out.layer
            .wl_surface()
            .frame(qh, FrameCallbackData(out.layer.wl_surface().clone()));

        frame.present();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(surface, qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(output, qh);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.outputs.retain(|o| o.wl_output != output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.outputs.retain(|o| &o.layer != layer);
        if self.outputs.is_empty() {
            self.exit = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(renderer) = &self.renderer else { return };
        let Some(out) = self.outputs.iter_mut().find(|o| &o.layer == layer) else {
            return;
        };

        out.width = NonZeroU32::new(configure.new_size.0).map_or(out.width, NonZeroU32::get);
        out.height = NonZeroU32::new(configure.new_size.1).map_or(out.height, NonZeroU32::get);
        if out.width == 0 || out.height == 0 {
            return;
        }

        // All outputs share one `self.layers` scene (see module doc
        // comment), so with differently-sized monitors this ends up
        // tuned for whichever output configured most recently -- a real
        // but narrow limitation of this secondary/test binary (the
        // primary path is render-server, which only ever drives one
        // canvas). Not solved here.
        renderer.set_text_viewport(&mut self.layers, out.width, out.height);

        let caps = out.surface.get_capabilities(&renderer.adapter);
        out.surface.configure(
            &renderer.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: SURFACE_FORMAT,
                width: out.width,
                height: out.height,
                present_mode: wgpu::PresentMode::Mailbox,
                desired_maximum_frame_latency: 2,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
            },
        );
        out.configured = true;

        let wl_surface = out.layer.wl_surface().clone();
        self.draw(&wl_surface, qh);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.cursor = (event.position.0 as f32, event.position.1 as f32);
                }
                PointerEventKind::Leave { .. } => {
                    // Push the mask far off-canvas so it fades out instead of
                    // sticking to the last known position.
                    self.cursor = (f32::MIN / 2.0, f32::MIN / 2.0);
                }
                _ => {}
            }
        }
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
