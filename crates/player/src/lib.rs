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
use project_format::{Effect, EffectKind, Layer, Mask, Project, TextSource, Transform2D};

const IMAGE_SHADER: &str = include_str!("shader.wgsl");
const XRAY_SHADER: &str = include_str!("xray.wgsl");
const PARALLAX_SHADER: &str = include_str!("parallax.wgsl");
const VIGNETTE_SHADER: &str = include_str!("vignette.wgsl");
const COLORADJUST_SHADER: &str = include_str!("coloradjust.wgsl");
const BLUR_SHADER: &str = include_str!("blur.wgsl");
const SMOKE_SHADER: &str = include_str!("smoke.wgsl");
const MASK_BLEND_SHADER: &str = include_str!("mask_blend.wgsl");

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
    effects: Option<EffectChain>,
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
    effects: Option<EffectChain>,
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
    effects: Option<EffectChain>,
}

/// Post-processing stack attached to a layer -- see `Ideas.md`,
/// "Эффекты как стек пост-обработки на слое". Only built when a layer's
/// `effects` list is non-empty (see `SceneRenderer::create_effect_chain`)
/// so the common case -- a layer with no effects -- doesn't pay for any
/// offscreen textures at all.
struct EffectChain {
    effects: Vec<LoadedEffect>,
    // Two fixed offscreen buffers, reused for the whole chain and every
    // frame -- not per-effect, and never ping-ponged by swapping roles.
    // `accumulator` always holds this layer's current effect-applied
    // appearance: starts as its raw content (see `record_draw`'s
    // "effect-base-pass"), ends as the final result once every enabled
    // effect has run. `scratch` only ever holds one effect's raw,
    // unmasked output, just long enough for the following mask-blend
    // pass to read it back into `accumulator`.
    // Kept alive only so `accumulator_view`/`scratch_view` aren't
    // dangling -- their contents are never read back on the Rust side.
    _accumulator_texture: wgpu::Texture,
    accumulator_view: wgpu::TextureView,
    _scratch_texture: wgpu::Texture,
    scratch_view: wgpu::TextureView,
    // A third offscreen buffer, unconditionally allocated alongside the
    // two above (same rationale as `scratch` existing even for a chain
    // that's all Vignette/ColorAdjust) -- used only by `Blur`, as the
    // landing spot for its horizontal pass before its vertical pass
    // reads it back out into `scratch` (see `record_draw`'s
    // `LoadedEffectKind::Blur` handling). Every other effect kind
    // ignores it entirely.
    _blur_temp_texture: wgpu::Texture,
    blur_temp_view: wgpu::TextureView,
    // Samples `accumulator_view` -- what the final composite pass in
    // `record_draw` draws through `image_pipeline` in place of the
    // layer's own native bind group, once its whole effect chain has
    // run. Built once here since `accumulator_texture`'s identity never
    // changes across frames, only its contents.
    composite_bind_group: wgpu::BindGroup,
}

/// One loaded effect in a layer's stack -- GPU-side counterpart of
/// [`project_format::Effect`].
struct LoadedEffect {
    kind: LoadedEffectKind,
    mask: LoadedMask,
    // Not baked into any bind group or shader -- checked in plain Rust
    // by `record_draw`'s effect loop (`if !effect.enabled { continue }`),
    // so toggling this needs no GPU write at all, just a field flip
    // (see `LoadedLayer::set_effect_params`).
    enabled: bool,
}

/// GPU-side counterpart of [`project_format::EffectKind`]. Each variant
/// owns its own uniform buffer(s)/bind group(s) against its own
/// pipeline/bind-group-layout on `SceneRenderer`; `uniform_buffer` (and
/// `Blur`'s pair) are genuinely read again by
/// `LoadedLayer::set_effect_params` on every editor frame -- unlike
/// `LoadedMask`'s texture handles below, these aren't a "kept alive
/// only" situation.
enum LoadedEffectKind {
    Vignette {
        uniform_buffer: wgpu::Buffer,
        bind_group: wgpu::BindGroup,
    },
    ColorAdjust {
        uniform_buffer: wgpu::Buffer,
        bind_group: wgpu::BindGroup,
    },
    // Two independent passes (see `EffectChain::blur_temp_view`'s doc
    // comment) -- each needs its own bind group since one samples the
    // accumulator and the other samples `blur_temp`, and its own
    // uniform buffer since `direction` differs between them (baked in
    // at creation time, never touched again -- only `radius` changes
    // live, into both buffers identically).
    Blur {
        horizontal_uniform_buffer: wgpu::Buffer,
        horizontal_bind_group: wgpu::BindGroup,
        vertical_uniform_buffer: wgpu::Buffer,
        vertical_bind_group: wgpu::BindGroup,
    },
    /// The first stateful/feedback effect -- see `Ideas.md`, "Техника
    /// отрисовки для стейтфул-эффектов". Everything else in this enum
    /// is a pure function of the layer's current content each frame;
    /// Smoke instead carries `state_texture` forward frame to frame,
    /// completely independent of the layer's own picture (only
    /// combined with it later, generically, by the mask-blend pass --
    /// same as every other effect kind). Boxed (see `SmokeEffect`'s own
    /// doc comment) so this variant's extra state doesn't inflate every
    /// other, much smaller variant's footprint too.
    Smoke(Box<SmokeEffect>),
}

/// Payload of `LoadedEffectKind::Smoke`, boxed out of the enum itself --
/// substantially bigger than every sibling variant (two textures/views,
/// its own uniform buffer and bind group), and Rust sizes an enum by
/// its largest variant, so without this every `LoadedEffect` would pay
/// for Smoke's footprint even when it's actually a Vignette (Clippy's
/// `large_enum_variant`).
struct SmokeEffect {
    // CPU-cached copies of this effect's own params. Unlike every other
    // effect kind's `set_effect_params` handling, these writes never
    // touch the GPU directly -- `update_smoke_cursors` (called every
    // redraw tick, not just when a slider moves) is what actually
    // rewrites the uniform buffer, combining these with the live cursor
    // position in one write. Exactly the pattern `XrayLayer::radius` +
    // `update_xray_cursors` already established; `set_effect_params`
    // alone changing it would mean waiting for the next cursor tick to
    // see the color/decay/radius take effect, and them not being here
    // at all would mean `update_smoke_cursors` has nothing to write
    // back each frame.
    color: [f32; 4],
    decay: f32,
    radius: f32,
    // This effect's own chain's resolution -- needed to convert a
    // shared canvas-pixel cursor position into this effect's own UV
    // space in `update_smoke_cursors`.
    width: u32,
    height: u32,
    // Persistent GPU state -- unlike `scratch`/`blur_temp` (which every
    // other effect kind treats as this-frame-only, always
    // `LoadOp::Clear`ed), never cleared after its one-time zero-init at
    // creation (see `SceneRenderer::clear_texture`). Holds last frame's
    // decayed+splatted result, and `record_draw` feeds it straight into
    // the generic mask-blend pass as this effect's raw output -- there's
    // no separate scratch write for Smoke, `state_view` fills that role
    // directly.
    state_texture: wgpu::Texture,
    state_view: wgpu::TextureView,
    // A fresh copy of `state_texture`, taken every frame right before
    // the decay+splat pass -- exists purely so that pass can sample
    // "last frame's state" while writing into `state_texture` itself; a
    // texture can't be both a pass's input and its render target at
    // once. See `record_draw`.
    temp_texture: wgpu::Texture,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// GPU-side counterpart of [`project_format::Mask`] -- always has a
/// uniform buffer and bind group, even for `Mask::None` (the shader
/// branches on `mask_mode` -- see `mask_blend.wgsl`), so `record_draw`'s
/// mask-blend pass never needs its own special case for "unmasked".
struct LoadedMask {
    // Read again by `LoadedLayer::set_effect_params` (live transform/
    // feather/invert updates), same rationale as `LoadedEffectKind`'s
    // buffers above.
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    // Only `Some` for `Mask::Texture` -- kept alive so the bind group's
    // texture view isn't dangling; never read back on the Rust side.
    _mask_texture: Option<wgpu::Texture>,
    // `mask_uniform_bytes`' aspect-correction input -- stashed here so
    // `set_effect_params` can rebuild this mask's uniform bytes on a
    // live update without needing the layer's width/height threaded
    // all the way back down to it.
    canvas_aspect: f32,
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
    Text(TextLayer),
}

/// What a Text layer currently displays, recomputed fresh on every call
/// -- no internal history/caching of its own (that's [`TextLayer::text`]'s
/// job, the single point comparisons happen against). `now` is always
/// caller-supplied, matching this module's existing convention
/// (`advance_gifs`'s `elapsed_ms`, `update_parallax`'s `dt_ms`) --
/// nothing in here ever reads the wall clock itself, so
/// [`ClockSource::poll`] stays trivially unit-testable with a fixed
/// injected time.
trait LiveTextSource {
    fn poll(&self, now: chrono::DateTime<chrono::Local>) -> String;
    /// Whether this source can ever produce a different string over
    /// time -- used by a caller that redraws on its own timer (editor's
    /// preview) to decide whether to keep requesting repaints for a Text
    /// layer, same question `project_format::TextSource::is_dynamic`
    /// answers at the schema level before anything is ever loaded.
    fn is_dynamic(&self) -> bool;
}

struct LiteralSource {
    text: String,
}

impl LiveTextSource for LiteralSource {
    fn poll(&self, _now: chrono::DateTime<chrono::Local>) -> String {
        self.text.clone()
    }

    fn is_dynamic(&self) -> bool {
        false
    }
}

struct ClockSource {
    format: String,
}

impl LiveTextSource for ClockSource {
    fn poll(&self, now: chrono::DateTime<chrono::Local>) -> String {
        // `DateTime::format` panics (via its `Display` impl) on an
        // unrecognized `%x` specifier instead of returning an error --
        // confirmed the hard way, by a test that reproduced exactly
        // that panic. `self.format` is hand-typed into the editor's
        // format field, so it has to be validated before ever reaching
        // `format()`, not just trusted. Showing the raw format string
        // back verbatim on failure (instead of a generic placeholder)
        // doubles as the error signal: it very visibly isn't a
        // formatted time.
        match chrono::format::StrftimeItems::new(&self.format).parse() {
            Ok(items) => now.format_with_items(items.into_iter()).to_string(),
            Err(_) => self.format.clone(),
        }
    }

    fn is_dynamic(&self) -> bool {
        true
    }
}

/// Fixed timeout for a single `Command` source invocation -- not
/// configurable per-layer in this pass. A command that hangs past this
/// gets killed (best-effort) and treated exactly like any other failure.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often a live `CommandSource`'s background thread checks whether
/// it's been asked to stop, while otherwise sleeping out its
/// `interval_secs` between runs -- bounds shutdown latency (e.g. when a
/// project is reloaded and the old `LoadedTextSource` is dropped) to
/// this, not to a whole possibly-long interval.
const COMMAND_STOP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Runs `command` via `sh -c`, waits up to [`COMMAND_TIMEOUT`], and
/// returns its trimmed stdout -- `None` on any failure, non-zero exit,
/// timeout, or empty output, all treated identically (the already-decided
/// "NULL" display convention lives in [`CommandSource::poll`], not here).
/// The real reason is always logged to stderr.
fn run_command_with_timeout(command: &str) -> Option<String> {
    run_command_with_timeout_inner(command, COMMAND_TIMEOUT)
}

/// The actual implementation, with the timeout as a parameter purely so
/// tests can exercise the kill path (see `command_that_hangs_gets_killed_
/// instead_of_hanging_the_caller`) against a short timeout instead of
/// waiting out the real several-second one.
///
/// Spawns a second "waiter" thread that owns the blocking
/// `wait_with_output()` call, so this function's own timeout can give up
/// on a hung command without leaking a blocked thread forever: the
/// waiter still exits on its own once the (best-effort-killed) child is
/// reaped, its channel send just lands nowhere. Same
/// spawn-thread-plus-`mpsc`-channel-plus-`recv_timeout` shape this
/// crate's GPU-readback and `editor`'s thumbnail generation already use
/// for the same "bound how long we wait for someone else's work"
/// problem.
fn run_command_with_timeout_inner(command: &str, timeout: std::time::Duration) -> Option<String> {
    let child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("player: failed to spawn command {command:?}: {e}");
            return None;
        }
    };
    let pid = child.id();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if !output.status.success() {
                eprintln!(
                    "player: command {command:?} exited with {:?}, stderr: {:?}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim(),
                );
                return None;
            }
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        }
        Ok(Err(e)) => {
            eprintln!("player: failed to wait for command {command:?}: {e}");
            None
        }
        Err(_) => {
            eprintln!("player: command {command:?} timed out after {timeout:?}, killing it");
            // Best-effort: `kill` failing (already exited, no
            // permission, PID already reused by something else in the
            // astronomically unlikely case) just means there's nothing
            // left to clean up.
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
            None
        }
    }
}

/// Background-polled shell command output -- see
/// [`Layer::Text`](project_format::Layer::Text)'s `TextSource::Command`.
/// Owns a thread that re-runs `command` every `interval_secs` (starting
/// immediately, not after the first interval) and writes its result into
/// `shared`; [`CommandSource::poll`] is then just a cheap non-blocking
/// read -- the real work already happened off the render thread.
struct CommandSource {
    command: String,
    interval_secs: u32,
    shared: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    // Set by `Drop` so the background thread notices and exits instead
    // of leaking forever once this value (and the thread's only other
    // handle back to it) is gone -- see `COMMAND_STOP_POLL_INTERVAL`.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CommandSource {
    /// Builds a `CommandSource` that never actually runs `command` --
    /// used for an untrusted project (see `SceneRenderer::load_scene`'s
    /// `allow_commands` parameter). `shared` permanently stays `None`
    /// (so `poll` permanently shows "NULL"), matching this crate's
    /// established behavior for a Command source that produced nothing:
    /// the process is never spawned at all here, not merely hidden after
    /// running.
    fn untrusted(command: String, interval_secs: u32) -> Self {
        CommandSource {
            command,
            interval_secs,
            shared: std::sync::Arc::new(std::sync::Mutex::new(None)),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn spawn(command: String, interval_secs: u32) -> Self {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interval = std::time::Duration::from_secs(u64::from(interval_secs.max(1)));

        let thread_command = command.clone();
        let thread_shared = std::sync::Arc::clone(&shared);
        let thread_stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !thread_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let result = run_command_with_timeout(&thread_command);
                *thread_shared.lock().unwrap() = result;
                sleep_unless_stopped(interval, &thread_stop);
            }
        });

        CommandSource {
            command,
            interval_secs,
            shared,
            stop,
        }
    }
}

impl Drop for CommandSource {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn sleep_unless_stopped(total: std::time::Duration, stop: &std::sync::atomic::AtomicBool) {
    let mut remaining = total;
    while !remaining.is_zero() && !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let chunk = remaining.min(COMMAND_STOP_POLL_INTERVAL);
        std::thread::sleep(chunk);
        remaining -= chunk;
    }
}

impl LiveTextSource for CommandSource {
    fn poll(&self, _now: chrono::DateTime<chrono::Local>) -> String {
        self.shared
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "NULL".to_string())
    }

    fn is_dynamic(&self) -> bool {
        true
    }
}

/// Exhaustive enum wrapping each [`LiveTextSource`] impl -- not
/// `Box<dyn>`, matching this codebase's existing preference (see
/// `LoadedLayer`) for compiler-enforced exhaustiveness over dynamic
/// dispatch. The genuine trait-strategy application for this
/// feature: three real implementations behind one interface, dispatched
/// by an exhaustive match.
enum LoadedTextSource {
    Literal(LiteralSource),
    Clock(ClockSource),
    Command(CommandSource),
}

impl LoadedTextSource {
    /// `allow_commands` gates only the `Command` variant -- see
    /// `SceneRenderer::load_scene`'s doc comment for what decides it.
    fn from_project(source: &TextSource, allow_commands: bool) -> Self {
        match source {
            TextSource::Literal { text } => {
                LoadedTextSource::Literal(LiteralSource { text: text.clone() })
            }
            TextSource::Clock { format } => LoadedTextSource::Clock(ClockSource {
                format: format.clone(),
            }),
            TextSource::Command {
                command,
                interval_secs,
            } => {
                if allow_commands {
                    LoadedTextSource::Command(CommandSource::spawn(command.clone(), *interval_secs))
                } else {
                    eprintln!(
                        "player: refusing to run command {command:?} for an untrusted project \
                         -- displaying NULL instead"
                    );
                    LoadedTextSource::Command(CommandSource::untrusted(
                        command.clone(),
                        *interval_secs,
                    ))
                }
            }
        }
    }

    /// Whether `source` describes the exact same configuration this
    /// value was already built from -- lets a caller like
    /// `LoadedLayer::set_text_params` (invoked every editor frame, not
    /// just on an actual edit) skip rebuilding entirely when nothing
    /// really changed. For `Command` this isn't just an optimization:
    /// rebuilding spawns a whole new background thread and starts
    /// re-running the command on its own interval immediately, so doing
    /// that on every frame regardless of whether the command string
    /// actually changed would be a real (if `Drop`-bounded) resource
    /// leak, not just wasted work.
    fn matches(&self, source: &TextSource) -> bool {
        match (self, source) {
            (LoadedTextSource::Literal(current), TextSource::Literal { text }) => {
                &current.text == text
            }
            (LoadedTextSource::Clock(current), TextSource::Clock { format }) => {
                &current.format == format
            }
            (
                LoadedTextSource::Command(current),
                TextSource::Command {
                    command,
                    interval_secs,
                },
            ) => &current.command == command && current.interval_secs == *interval_secs,
            _ => false,
        }
    }
}

impl LiveTextSource for LoadedTextSource {
    fn poll(&self, now: chrono::DateTime<chrono::Local>) -> String {
        match self {
            LoadedTextSource::Literal(source) => source.poll(now),
            LoadedTextSource::Clock(source) => source.poll(now),
            LoadedTextSource::Command(source) => source.poll(now),
        }
    }

    fn is_dynamic(&self) -> bool {
        match self {
            LoadedTextSource::Literal(source) => source.is_dynamic(),
            LoadedTextSource::Clock(source) => source.is_dynamic(),
            LoadedTextSource::Command(source) => source.is_dynamic(),
        }
    }
}

/// A positioned string drawn via glyphon -- see
/// [`Layer::Text`](project_format::Layer::Text). `x`/`y`/`font_size` are
/// kept as the normalized (0.0..=1.0) values from the schema, not
/// pre-converted to pixels, so [`SceneRenderer::set_text_viewport`] can
/// re-lay-out against a new canvas size without needing the original
/// fractions back from anywhere else. `text` is whatever `source` last
/// resolved to and got actually shaped into `buffer` -- the single point
/// [`SceneRenderer::advance_text_sources`] compares a fresh `source.poll`
/// against to decide whether an expensive re-shape is even needed.
pub struct TextLayer {
    buffer: glyphon::Buffer,
    text: String,
    source: LoadedTextSource,
    x: f32,
    y: f32,
    font_size: f32,
    color: [f32; 4],
}

impl TextLayer {
    /// Pixel width/height of the shaped `buffer`'s bounding box --
    /// `(0.0, 0.0)` for an empty string, which lays out to zero runs.
    /// Width is the widest line; height is the bottom of the last line
    /// (`line_top + line_height` rather than a running sum, so it's
    /// correct regardless of how many lines came before).
    fn measure(&self) -> (f32, f32) {
        let mut width: f32 = 0.0;
        let mut height: f32 = 0.0;
        for run in self.buffer.layout_runs() {
            width = width.max(run.line_w);
            height = height.max(run.line_top + run.line_height);
        }
        (width, height)
    }
}

/// Bundles every piece of glyphon/cosmic-text state needed to shape and
/// draw text layers. One instance per [`SceneRenderer`] (not per scene,
/// not per layer) -- glyphon's `TextAtlas`/`TextRenderer` are meant to be
/// long-lived and shared across everything drawn through them. Held
/// behind a `Mutex` purely to satisfy the borrow checker inside
/// `&self`-taking methods like `record_draw`; every real caller
/// (render-server's tick thread, editor's owned `Preview`, player's own
/// owned `App`) is single-owner already, so there's no real contention.
struct GlyphonState {
    font_system: glyphon::FontSystem,
    swash_cache: glyphon::SwashCache,
    atlas: glyphon::TextAtlas,
    text_renderer: glyphon::TextRenderer,
    viewport: glyphon::Viewport,
    // Last size passed to `set_text_viewport` -- the pixel basis that
    // every Text layer's normalized x/y/font_size get multiplied
    // against. Seeded to a sane fallback so a layer loaded before the
    // first `set_text_viewport` call still renders at *some* reasonable
    // size instead of collapsing to font_size 0.
    canvas_width: u32,
    canvas_height: u32,
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
            // Text has no source asset of its own to derive a canvas
            // size from -- callers that pick canvas size from
            // `layers.first()` (render-server, editor) should put a
            // picture layer first if precise sizing matters; this is a
            // soft product constraint, not solved generally here.
            LoadedLayer::Text(_) => (1920, 1080),
        }
    }

    /// Whether this layer needs continuous re-rendering to look right --
    /// mirrors `project_format::Layer::is_dynamic`, but at the loaded/
    /// GPU-side representation. Meant for a caller that redraws on its
    /// own timer and wants to know whether to keep doing so (editor's
    /// preview); render-server instead asks the schema-level question up
    /// front, before anything is ever loaded (see `Layer::is_dynamic`,
    /// which `LoadedProject::dynamic` is computed from).
    pub fn is_dynamic(&self) -> bool {
        match self {
            LoadedLayer::Image(_) => false,
            LoadedLayer::Gif { .. } | LoadedLayer::Xray(_) | LoadedLayer::Parallax(_) => true,
            LoadedLayer::Text(text) => text.source.is_dynamic(),
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

    /// Reconfigures an already-loaded Text layer's position, size, color
    /// and *source* (the literal string, the clock format string, or the
    /// command/interval) in place -- lets a caller like editor keep a
    /// text-editing UI feeling live, same rationale as
    /// `set_xray_radius`/`set_parallax_params`. Doesn't touch `buffer`
    /// itself: unlike those two, actually re-shaping glyphon's buffer
    /// needs `&mut FontSystem`, which lives on `SceneRenderer`, not here
    /// -- the next `SceneRenderer::advance_text_sources` call (every real
    /// caller already ticks it every frame/frame-tick) picks up whatever
    /// the reconfigured source currently resolves to. No-op on any other
    /// layer kind.
    ///
    /// Only actually rebuilds `source` when its configuration differs
    /// from what's already loaded (see `LoadedTextSource::matches`) --
    /// this method gets called every editor frame regardless of whether
    /// anything was actually edited, and for a `Command` source
    /// rebuilding means spawning a whole new background thread/process,
    /// not just a cheap field write. `allow_commands` is always `true`
    /// from editor's own call site: the editor is the self-authoring
    /// context, trusted the moment it saves (see `SceneRenderer::
    /// load_scene`'s doc comment for the untrusted case this skips).
    pub fn set_text_params(
        &mut self,
        source: &TextSource,
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
        allow_commands: bool,
    ) {
        if let LoadedLayer::Text(text) = self {
            if !text.source.matches(source) {
                text.source = LoadedTextSource::from_project(source, allow_commands);
            }
            text.x = x;
            text.y = y;
            text.font_size = font_size;
            text.color = color;
        }
    }

    /// Pushes each of `effects`' current numeric params (per-kind
    /// params, mask transform/feather/invert, enabled) into an
    /// already-loaded layer's effect chain in place, via
    /// `queue.write_buffer` -- no GPU reload, same live-update
    /// rationale as `set_xray_radius`/`set_text_params`. No-op on a
    /// layer kind that never has an effect chain (Text), or one whose
    /// chain is currently `None` (no effects).
    ///
    /// `effects` must be the *same shape* this layer's chain was last
    /// built with -- same length, and each entry's `kind`/mask variant
    /// matching what's already loaded at that index. A structural
    /// change (added/removed effect, changed kind or mask variant,
    /// changed a `Mask::Texture` path) needs a real reload instead (see
    /// editor's `LayerSignature`, which is what decides whether this
    /// method or a reload runs for a given frame). Any index past
    /// whichever list is shorter, or whose kind/mask variant doesn't
    /// match, is silently skipped rather than panicking -- a caller
    /// approaching a reload decision may still call this once more
    /// with the old (about-to-be-stale) shape before the reload lands.
    pub fn set_effect_params(&mut self, queue: &wgpu::Queue, effects: &[Effect]) {
        let chain = match self {
            LoadedLayer::Image(image) | LoadedLayer::Gif { image, .. } => &mut image.effects,
            LoadedLayer::Xray(xray) => &mut xray.effects,
            LoadedLayer::Parallax(parallax) => &mut parallax.effects,
            LoadedLayer::Text(_) => return,
        };
        let Some(chain) = chain else { return };

        for (loaded, effect) in chain.effects.iter_mut().zip(effects) {
            loaded.enabled = effect.enabled;

            match (&mut loaded.kind, &effect.kind) {
                (
                    LoadedEffectKind::Vignette { uniform_buffer, .. },
                    EffectKind::Vignette { strength, softness },
                ) => {
                    queue.write_buffer(
                        uniform_buffer,
                        0,
                        &vignette_uniform_bytes(*strength, *softness),
                    );
                }
                (
                    LoadedEffectKind::ColorAdjust { uniform_buffer, .. },
                    EffectKind::ColorAdjust {
                        brightness,
                        contrast,
                        saturation,
                    },
                ) => {
                    queue.write_buffer(
                        uniform_buffer,
                        0,
                        &coloradjust_uniform_bytes(*brightness, *contrast, *saturation),
                    );
                }
                (
                    LoadedEffectKind::Blur {
                        horizontal_uniform_buffer,
                        vertical_uniform_buffer,
                        ..
                    },
                    EffectKind::Blur { radius },
                ) => {
                    queue.write_buffer(
                        horizontal_uniform_buffer,
                        0,
                        &blur_uniform_bytes(*radius, (1.0, 0.0)),
                    );
                    queue.write_buffer(
                        vertical_uniform_buffer,
                        0,
                        &blur_uniform_bytes(*radius, (0.0, 1.0)),
                    );
                }
                (
                    LoadedEffectKind::Smoke(loaded_smoke),
                    EffectKind::Smoke {
                        color,
                        decay,
                        radius,
                    },
                ) => {
                    // No GPU write here, deliberately -- see
                    // `LoadedEffectKind::Smoke`'s doc comment.
                    // `update_smoke_cursors` is what actually rewrites
                    // this effect's uniform buffer, every redraw tick,
                    // combining these CPU-cached fields with the live
                    // cursor position it alone has on hand.
                    loaded_smoke.color = *color;
                    loaded_smoke.decay = *decay;
                    loaded_smoke.radius = *radius;
                }
                // Mismatched shape (mid-reload) -- this effect's kind
                // params are skipped for this frame, but its mask
                // below is independent of `kind` and still applies.
                _ => {}
            }

            queue.write_buffer(
                &loaded.mask.uniform_buffer,
                0,
                &mask_uniform_bytes(&effect.mask, loaded.mask.canvas_aspect),
            );
        }
    }

    /// Pixel width/height of a Text layer's shaped text, in the same
    /// canvas-pixel space `x`/`y` (multiplied by canvas width/height)
    /// already use -- i.e. before any further scaling a caller (editor's
    /// preview, which usually renders smaller than the project's actual
    /// resolution) applies on top. `None` for any other layer kind.
    /// Meant for a caller that wants to draw a bounding box around a Text
    /// layer (editor's on-preview move/resize handles) instead of just a
    /// point at its `x`/`y` origin.
    pub fn text_size(&self) -> Option<(f32, f32)> {
        match self {
            LoadedLayer::Text(text) => Some(text.measure()),
            _ => None,
        }
    }
}

/// Builds the bind group layout shared by every single-input-texture
/// effect pass (Vignette/ColorAdjust/Blur -- each direction of Blur
/// alike): one sampled texture, one sampler, one uniform buffer of
/// that effect's own params. `mask_pipeline`'s layout is a fourth
/// binding (the mask texture) on top of this same shape, so it builds
/// its own rather than reusing this helper.
fn build_effect_bind_group_layout(device: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
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
    })
}

/// Packs `VignetteParams`' bytes (`vignette.wgsl`) -- shared by
/// `create_effect_chain`'s initial load and
/// `LoadedLayer::set_effect_params`'s live update, so the two can never
/// drift out of sync with each other's layout.
fn vignette_uniform_bytes(strength: f32, softness: f32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&strength.to_le_bytes());
    bytes[4..8].copy_from_slice(&softness.to_le_bytes());
    bytes
}

/// Packs `ColorAdjustParams`' bytes (`coloradjust.wgsl`) -- same
/// load/live-update sharing rationale as [`vignette_uniform_bytes`].
fn coloradjust_uniform_bytes(brightness: f32, contrast: f32, saturation: f32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&brightness.to_le_bytes());
    bytes[4..8].copy_from_slice(&contrast.to_le_bytes());
    bytes[8..12].copy_from_slice(&saturation.to_le_bytes());
    bytes
}

/// Packs `BlurParams`' bytes (`blur.wgsl`) for one pass -- `direction`
/// is `(1.0, 0.0)` for the horizontal pass, `(0.0, 1.0)` for the
/// vertical one (see `LoadedEffectKind::Blur`'s doc comment). Same
/// load/live-update sharing rationale as [`vignette_uniform_bytes`].
fn blur_uniform_bytes(radius: f32, direction: (f32, f32)) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&radius.to_le_bytes());
    bytes[4..8].copy_from_slice(&direction.0.to_le_bytes());
    bytes[8..12].copy_from_slice(&direction.1.to_le_bytes());
    bytes
}

/// Packs `SmokeParams`' bytes (`smoke.wgsl`) -- shared by
/// `create_effect_chain`'s initial load and
/// `SceneRenderer::update_smoke_cursors`'s per-frame write. Unlike
/// every other `*_uniform_bytes` helper here, this is never called
/// from `LoadedLayer::set_effect_params` -- see `LoadedEffectKind::
/// Smoke`'s doc comment for why that setter only updates the CPU-side
/// fields this reads, and leaves the actual GPU write to whichever
/// caller also has the live cursor position on hand.
fn smoke_uniform_bytes(color: [f32; 4], cursor_uv: (f32, f32), decay: f32, radius: f32) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&color[0].to_le_bytes());
    bytes[4..8].copy_from_slice(&color[1].to_le_bytes());
    bytes[8..12].copy_from_slice(&color[2].to_le_bytes());
    bytes[12..16].copy_from_slice(&color[3].to_le_bytes());
    bytes[16..20].copy_from_slice(&cursor_uv.0.to_le_bytes());
    bytes[20..24].copy_from_slice(&cursor_uv.1.to_le_bytes());
    bytes[24..28].copy_from_slice(&decay.to_le_bytes());
    bytes[28..32].copy_from_slice(&radius.to_le_bytes());
    bytes
}

/// Packs `MaskParams`' bytes (`mask_blend.wgsl`) -- shared by
/// `create_loaded_mask`'s initial load and
/// `LoadedLayer::set_effect_params`'s live update. `canvas_aspect`
/// isn't derived from `mask` itself (see `LoadedMask::canvas_aspect`'s
/// doc comment for why a live update still has it on hand).
fn mask_uniform_bytes(mask: &Mask, canvas_aspect: f32) -> [u8; 32] {
    let (transform, feather, invert, mode): (Transform2D, f32, bool, f32) = match mask {
        Mask::None => (Transform2D::default(), 0.0, false, 0.0),
        Mask::Circle {
            transform,
            feather,
            invert,
        } => (*transform, *feather, *invert, 1.0),
        Mask::Gradient {
            transform,
            feather,
            invert,
        } => (*transform, *feather, *invert, 2.0),
        Mask::Texture { invert, .. } => (Transform2D::default(), 0.0, *invert, 3.0),
    };
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&transform.x.to_le_bytes());
    bytes[4..8].copy_from_slice(&transform.y.to_le_bytes());
    bytes[8..12].copy_from_slice(&transform.scale.to_le_bytes());
    bytes[12..16].copy_from_slice(&transform.rotation.to_le_bytes());
    bytes[16..20].copy_from_slice(&feather.to_le_bytes());
    bytes[20..24].copy_from_slice(&(if invert { 1.0f32 } else { 0.0f32 }).to_le_bytes());
    bytes[24..28].copy_from_slice(&mode.to_le_bytes());
    bytes[28..32].copy_from_slice(&canvas_aspect.to_le_bytes());
    bytes
}

/// Builds a single-fragment-shader, fullscreen-triangle render pipeline
/// -- the shape every pipeline in this module shares (image/xray/
/// parallax/vignette/mask all draw one full-canvas triangle through a
/// `vs_main`/`fs_main` pair, differing only in shader, bind group
/// layout, target format, and blend state).
fn build_fullscreen_pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    target_format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Owns the GPU device/pipelines used to composite a project's layers.
/// Cheap to keep around for the life of a process -- construct once, load
/// as many scenes (`Vec<LoadedLayer>`) against it as you like.
pub struct SceneRenderer {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    image_pipeline: wgpu::RenderPipeline,
    // Same shader/bind-group-layout as `image_pipeline`, but always
    // targets `OFFSCREEN_FORMAT` and never blends -- see its own
    // creation site in `build` for why a second variant is needed.
    image_pipeline_offscreen: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    xray_pipeline: wgpu::RenderPipeline,
    xray_pipeline_offscreen: wgpu::RenderPipeline,
    xray_bind_group_layout: wgpu::BindGroupLayout,
    parallax_pipeline: wgpu::RenderPipeline,
    parallax_pipeline_offscreen: wgpu::RenderPipeline,
    parallax_bind_group_layout: wgpu::BindGroupLayout,
    vignette_pipeline: wgpu::RenderPipeline,
    vignette_bind_group_layout: wgpu::BindGroupLayout,
    coloradjust_pipeline: wgpu::RenderPipeline,
    coloradjust_bind_group_layout: wgpu::BindGroupLayout,
    blur_pipeline: wgpu::RenderPipeline,
    blur_bind_group_layout: wgpu::BindGroupLayout,
    smoke_pipeline: wgpu::RenderPipeline,
    smoke_bind_group_layout: wgpu::BindGroupLayout,
    mask_pipeline: wgpu::RenderPipeline,
    mask_bind_group_layout: wgpu::BindGroupLayout,
    dummy_mask_view: wgpu::TextureView,
    _dummy_mask_texture: wgpu::Texture,
    sampler: wgpu::Sampler,
    glyphon: std::sync::Mutex<GlyphonState>,
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

        let image_pipeline = build_fullscreen_pipeline(
            &device,
            "image-pipeline",
            &image_pipeline_layout,
            &image_shader,
            target_format,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );
        // Same shader and bind group layout, but always targets
        // `OFFSCREEN_FORMAT` and never blends -- used only to draw a
        // layer's *base* content into its own effect chain's
        // accumulator texture (see `EffectChain`), which is always that
        // format regardless of what `target_format` this renderer was
        // built for (on-screen `player` uses a swapchain format that
        // can differ), and has nothing to blend against yet on a
        // freshly cleared texture.
        let image_pipeline_offscreen = build_fullscreen_pipeline(
            &device,
            "image-pipeline-offscreen",
            &image_pipeline_layout,
            &image_shader,
            OFFSCREEN_FORMAT,
            None,
        );

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

        let xray_pipeline = build_fullscreen_pipeline(
            &device,
            "xray-pipeline",
            &xray_pipeline_layout,
            &xray_shader,
            target_format,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );
        let xray_pipeline_offscreen = build_fullscreen_pipeline(
            &device,
            "xray-pipeline-offscreen",
            &xray_pipeline_layout,
            &xray_shader,
            OFFSCREEN_FORMAT,
            None,
        );

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

        let parallax_pipeline = build_fullscreen_pipeline(
            &device,
            "parallax-pipeline",
            &parallax_pipeline_layout,
            &parallax_shader,
            target_format,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );
        let parallax_pipeline_offscreen = build_fullscreen_pipeline(
            &device,
            "parallax-pipeline-offscreen",
            &parallax_pipeline_layout,
            &parallax_shader,
            OFFSCREEN_FORMAT,
            None,
        );

        // -- Effect pipelines: always `OFFSCREEN_FORMAT`, since they
        // only ever draw into a layer's own accumulator/scratch
        // textures (see `EffectChain`), never the caller's real
        // `target_view`. --

        let vignette_bind_group_layout =
            build_effect_bind_group_layout(&device, "vignette-bind-group-layout");

        let vignette_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vignette-shader"),
            source: wgpu::ShaderSource::Wgsl(VIGNETTE_SHADER.into()),
        });

        let vignette_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("vignette-pipeline-layout"),
                bind_group_layouts: &[Some(&vignette_bind_group_layout)],
                immediate_size: 0,
            });

        // No blend -- writes its full, unmasked result straight into a
        // freshly cleared scratch texture; the following mask-blend
        // pass is what actually mixes it back against the layer's
        // accumulated appearance. Same rationale for `coloradjust_pipeline`/
        // `blur_pipeline` below.
        let vignette_pipeline = build_fullscreen_pipeline(
            &device,
            "vignette-pipeline",
            &vignette_pipeline_layout,
            &vignette_shader,
            OFFSCREEN_FORMAT,
            None,
        );

        let coloradjust_bind_group_layout =
            build_effect_bind_group_layout(&device, "coloradjust-bind-group-layout");

        let coloradjust_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("coloradjust-shader"),
            source: wgpu::ShaderSource::Wgsl(COLORADJUST_SHADER.into()),
        });

        let coloradjust_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("coloradjust-pipeline-layout"),
                bind_group_layouts: &[Some(&coloradjust_bind_group_layout)],
                immediate_size: 0,
            });

        let coloradjust_pipeline = build_fullscreen_pipeline(
            &device,
            "coloradjust-pipeline",
            &coloradjust_pipeline_layout,
            &coloradjust_shader,
            OFFSCREEN_FORMAT,
            None,
        );

        // Same bind group layout shape as vignette/coloradjust (one
        // input texture, reused for both the horizontal and vertical
        // pass -- see `LoadedEffectKind::Blur`'s doc comment) -- one
        // pipeline serves both passes, only the bind group (which
        // texture, which `direction`) differs between them.
        let blur_bind_group_layout =
            build_effect_bind_group_layout(&device, "blur-bind-group-layout");

        let blur_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blur-shader"),
            source: wgpu::ShaderSource::Wgsl(BLUR_SHADER.into()),
        });

        let blur_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blur-pipeline-layout"),
            bind_group_layouts: &[Some(&blur_bind_group_layout)],
            immediate_size: 0,
        });

        let blur_pipeline = build_fullscreen_pipeline(
            &device,
            "blur-pipeline",
            &blur_pipeline_layout,
            &blur_shader,
            OFFSCREEN_FORMAT,
            None,
        );

        // Same bind group layout shape as vignette/coloradjust/blur --
        // one input texture (`temp`, see `LoadedEffectKind::Smoke`), a
        // sampler, and this effect's own uniform params.
        let smoke_bind_group_layout =
            build_effect_bind_group_layout(&device, "smoke-bind-group-layout");

        let smoke_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("smoke-shader"),
            source: wgpu::ShaderSource::Wgsl(SMOKE_SHADER.into()),
        });

        let smoke_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("smoke-pipeline-layout"),
            bind_group_layouts: &[Some(&smoke_bind_group_layout)],
            immediate_size: 0,
        });

        // No blend -- the shader unconditionally writes a full replacement
        // color for every pixel (`previous * decay + splat`, clamped),
        // never leaving any of the target's existing content showing
        // through, so there's nothing for hardware blending to mix in.
        let smoke_pipeline = build_fullscreen_pipeline(
            &device,
            "smoke-pipeline",
            &smoke_pipeline_layout,
            &smoke_shader,
            OFFSCREEN_FORMAT,
            None,
        );

        let mask_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("mask-bind-group-layout"),
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            });

        let mask_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mask-blend-shader"),
            source: wgpu::ShaderSource::Wgsl(MASK_BLEND_SHADER.into()),
        });

        let mask_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mask-pipeline-layout"),
            bind_group_layouts: &[Some(&mask_bind_group_layout)],
            immediate_size: 0,
        });

        // Alpha-blended, unlike every other offscreen pipeline above --
        // this is the pass that actually mixes an effect's raw output
        // back into the layer's accumulator, by writing `(color, mask)`
        // and letting the GPU's own blend unit compute `mix(before,
        // color, mask)` against whatever's already in the target (see
        // `mask_blend.wgsl`'s module doc comment).
        let mask_pipeline = build_fullscreen_pipeline(
            &device,
            "mask-pipeline",
            &mask_pipeline_layout,
            &mask_shader,
            OFFSCREEN_FORMAT,
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );

        // 1x1 opaque white -- bound at the mask pipeline's mask-texture
        // slot whenever a mask isn't `Mask::Texture`. The shader
        // branches on `mask_mode` and never actually samples it in that
        // case, but wgpu still requires every declared binding to point
        // at *something* valid.
        let dummy_mask_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dummy-mask-texture"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &dummy_mask_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let dummy_mask_view =
            dummy_mask_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let glyphon_cache = glyphon::Cache::new(&device);
        let glyphon_viewport = glyphon::Viewport::new(&device, &glyphon_cache);
        let mut glyphon_atlas =
            glyphon::TextAtlas::new(&device, &queue, &glyphon_cache, target_format);
        let glyphon_text_renderer = glyphon::TextRenderer::new(
            &mut glyphon_atlas,
            &device,
            wgpu::MultisampleState::default(),
            None,
        );
        let glyphon = std::sync::Mutex::new(GlyphonState {
            font_system: glyphon::FontSystem::new(),
            swash_cache: glyphon::SwashCache::new(),
            atlas: glyphon_atlas,
            text_renderer: glyphon_text_renderer,
            viewport: glyphon_viewport,
            canvas_width: 1920,
            canvas_height: 1080,
        });

        Self {
            adapter,
            device,
            queue,
            image_pipeline,
            image_pipeline_offscreen,
            image_bind_group_layout,
            xray_pipeline,
            xray_pipeline_offscreen,
            xray_bind_group_layout,
            parallax_pipeline,
            parallax_pipeline_offscreen,
            parallax_bind_group_layout,
            vignette_pipeline,
            vignette_bind_group_layout,
            coloradjust_pipeline,
            coloradjust_bind_group_layout,
            blur_pipeline,
            blur_bind_group_layout,
            smoke_pipeline,
            smoke_bind_group_layout,
            mask_pipeline,
            mask_bind_group_layout,
            dummy_mask_view,
            _dummy_mask_texture: dummy_mask_texture,
            sampler,
            glyphon,
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

    /// A render target a layer's effect chain draws into and samples
    /// from (see `EffectChain`'s `accumulator`/`scratch`) -- unlike
    /// [`Self::create_texture`], never written via `write_texture`, only
    /// ever a render pass's color attachment or a shader's sampled
    /// input.
    fn create_offscreen_render_target(
        &self,
        width: u32,
        height: u32,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("effect-offscreen-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    /// Fills `view` with fully-transparent black via a one-off render
    /// pass, then submits it immediately -- used to zero-initialize a
    /// persistent effect's own state texture at creation time (see
    /// `LoadedEffectKind::Smoke::state_texture`), which -- unlike
    /// `create_texture`/`write_texture` -- is never written from decoded
    /// image bytes, so without this its first frame would sample
    /// whatever undefined content the GPU happened to allocate. A rare,
    /// one-time-per-effect-load cost, not a per-frame one.
    fn clear_texture(&self, view: &wgpu::TextureView) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear-texture-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
        }
        self.queue.submit(Some(encoder.finish()));
    }

    /// Builds a layer's post-processing stack from its `effects` list --
    /// `None` if empty (the common case: most layers have no effects,
    /// and shouldn't pay for any offscreen textures at all -- see
    /// `EffectChain`'s doc comment). `width`/`height` must match the
    /// layer's own native size, not the canvas's -- a layer's effect
    /// chain runs at its own resolution, same as everything else about
    /// how a layer is loaded.
    fn create_effect_chain(
        &self,
        project_dir: &Path,
        width: u32,
        height: u32,
        effects: &[Effect],
    ) -> Result<Option<EffectChain>, String> {
        if effects.is_empty() {
            return Ok(None);
        }

        let (accumulator_texture, accumulator_view) =
            self.create_offscreen_render_target(width, height);
        let (scratch_texture, scratch_view) = self.create_offscreen_render_target(width, height);
        let (blur_temp_texture, blur_temp_view) =
            self.create_offscreen_render_target(width, height);

        let composite_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("effect-composite-bind-group"),
            layout: &self.image_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&accumulator_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let canvas_aspect = width as f32 / (height.max(1) as f32);
        let mut loaded_effects = Vec::with_capacity(effects.len());
        for effect in effects {
            let kind = match &effect.kind {
                EffectKind::Vignette { strength, softness } => {
                    let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("vignette-params"),
                        size: 16,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    self.queue.write_buffer(
                        &uniform_buffer,
                        0,
                        &vignette_uniform_bytes(*strength, *softness),
                    );

                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("vignette-bind-group"),
                        layout: &self.vignette_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&accumulator_view),
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

                    LoadedEffectKind::Vignette {
                        uniform_buffer,
                        bind_group,
                    }
                }
                EffectKind::ColorAdjust {
                    brightness,
                    contrast,
                    saturation,
                } => {
                    let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("coloradjust-params"),
                        size: 16,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    self.queue.write_buffer(
                        &uniform_buffer,
                        0,
                        &coloradjust_uniform_bytes(*brightness, *contrast, *saturation),
                    );

                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("coloradjust-bind-group"),
                        layout: &self.coloradjust_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&accumulator_view),
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

                    LoadedEffectKind::ColorAdjust {
                        uniform_buffer,
                        bind_group,
                    }
                }
                EffectKind::Blur { radius } => {
                    let horizontal_uniform_buffer =
                        self.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("blur-horizontal-params"),
                            size: 16,
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                    self.queue.write_buffer(
                        &horizontal_uniform_buffer,
                        0,
                        &blur_uniform_bytes(*radius, (1.0, 0.0)),
                    );
                    // Horizontal pass reads the layer's own accumulated
                    // content (same input every other effect kind reads)
                    // and writes into `blur_temp`.
                    let horizontal_bind_group =
                        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("blur-horizontal-bind-group"),
                            layout: &self.blur_bind_group_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(
                                        &accumulator_view,
                                    ),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 2,
                                    resource: horizontal_uniform_buffer.as_entire_binding(),
                                },
                            ],
                        });

                    let vertical_uniform_buffer =
                        self.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("blur-vertical-params"),
                            size: 16,
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                    self.queue.write_buffer(
                        &vertical_uniform_buffer,
                        0,
                        &blur_uniform_bytes(*radius, (0.0, 1.0)),
                    );
                    // Vertical pass reads the horizontal pass's own
                    // output and writes into `scratch`, same as every
                    // other effect kind's single pass does.
                    let vertical_bind_group =
                        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("blur-vertical-bind-group"),
                            layout: &self.blur_bind_group_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&blur_temp_view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 2,
                                    resource: vertical_uniform_buffer.as_entire_binding(),
                                },
                            ],
                        });

                    LoadedEffectKind::Blur {
                        horizontal_uniform_buffer,
                        horizontal_bind_group,
                        vertical_uniform_buffer,
                        vertical_bind_group,
                    }
                }
                EffectKind::Smoke {
                    color,
                    decay,
                    radius,
                } => {
                    let state_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("smoke-state-texture"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: OFFSCREEN_FORMAT,
                        // RENDER_ATTACHMENT: the decay+splat pass's own
                        // target. TEXTURE_BINDING: sampled by the
                        // mask-blend pass. COPY_SRC: copied from every
                        // frame in `record_draw` (see `temp_texture`
                        // below).
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING
                            | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    });
                    let state_view =
                        state_texture.create_view(&wgpu::TextureViewDescriptor::default());
                    // Unlike every other offscreen target in this
                    // module, `state_texture` must start fully
                    // transparent rather than whatever the GPU happened
                    // to allocate -- it's never written from decoded
                    // image bytes, and its very first read (this
                    // effect's first frame) happens before any pass of
                    // its own has written to it yet.
                    self.clear_texture(&state_view);

                    let temp_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("smoke-temp-texture"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: OFFSCREEN_FORMAT,
                        // Only ever a copy destination and a sampled
                        // input -- never a render target itself.
                        usage: wgpu::TextureUsages::TEXTURE_BINDING
                            | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    let temp_view =
                        temp_texture.create_view(&wgpu::TextureViewDescriptor::default());

                    let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("smoke-params"),
                        size: 32,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    // Off-canvas sentinel until the first real cursor
                    // arrives via `update_smoke_cursors` (called every
                    // redraw tick by every consumer -- editor/player/
                    // render-server all already do the same for
                    // `update_xray_cursors`) -- matches the convention
                    // `cursor_px_from_uv`/`App::pointer_frame`'s `Leave`
                    // handling already use elsewhere for "no cursor
                    // here".
                    self.queue.write_buffer(
                        &uniform_buffer,
                        0,
                        &smoke_uniform_bytes(*color, (-1.0e6, -1.0e6), *decay, *radius),
                    );

                    let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("smoke-bind-group"),
                        layout: &self.smoke_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&temp_view),
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

                    LoadedEffectKind::Smoke(Box::new(SmokeEffect {
                        color: *color,
                        decay: *decay,
                        radius: *radius,
                        width,
                        height,
                        state_texture,
                        state_view,
                        temp_texture,
                        uniform_buffer,
                        bind_group,
                    }))
                }
            };

            // Every other effect kind writes its raw output into the
            // chain's shared, every-frame-cleared `scratch` -- Smoke's
            // is its own persistent `state_view` instead (see
            // `LoadedEffectKind::Smoke`'s doc comment), so the mask
            // built below has to read from whichever one actually holds
            // this effect's output.
            let mask_source_view = match &kind {
                LoadedEffectKind::Smoke(smoke) => &smoke.state_view,
                _ => &scratch_view,
            };
            let mask =
                self.create_loaded_mask(project_dir, &effect.mask, mask_source_view, canvas_aspect)?;

            loaded_effects.push(LoadedEffect {
                kind,
                mask,
                enabled: effect.enabled,
            });
        }

        Ok(Some(EffectChain {
            effects: loaded_effects,
            _accumulator_texture: accumulator_texture,
            accumulator_view,
            _scratch_texture: scratch_texture,
            scratch_view,
            _blur_temp_texture: blur_temp_texture,
            blur_temp_view,
            composite_bind_group,
        }))
    }

    /// Builds the generic mask-blend infrastructure for one effect --
    /// shared by every `EffectKind`, not something each effect's own
    /// shader implements (see `Ideas.md`, "Развилка 2"). `scratch_view`
    /// is the texture this mask's bind group reads from (the *effect*
    /// this mask belongs to writes its raw output there -- see
    /// `record_draw`'s effect-chain loop).
    fn create_loaded_mask(
        &self,
        project_dir: &Path,
        mask: &Mask,
        scratch_view: &wgpu::TextureView,
        canvas_aspect: f32,
    ) -> Result<LoadedMask, String> {
        // Only `Mask::Texture` actually loads a picture -- every other
        // mode's bind group points at `dummy_mask_view` instead (see its
        // own doc comment), since the shader never samples it for them.
        let mask_texture = match mask {
            Mask::Texture { path, .. } => {
                let (rgba, width, height) = open_rgba(&project_dir.join(path))?;
                let texture = self.create_texture(width, height);
                self.write_texture(&texture, &rgba, width, height);
                Some(texture)
            }
            _ => None,
        };
        let mask_texture_view = mask_texture
            .as_ref()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
        let mask_texture_view_ref = mask_texture_view.as_ref().unwrap_or(&self.dummy_mask_view);

        let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mask-params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(
            &uniform_buffer,
            0,
            &mask_uniform_bytes(mask, canvas_aspect),
        );

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask-bind-group"),
            layout: &self.mask_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(scratch_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(mask_texture_view_ref),
                },
            ],
        });

        Ok(LoadedMask {
            uniform_buffer,
            bind_group,
            _mask_texture: mask_texture,
            canvas_aspect,
        })
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
            effects: None,
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
        self.write_texture(
            &overlay_texture,
            overlay_rgba,
            overlay_width,
            overlay_height,
        );
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
            effects: None,
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
            effects: None,
        }
    }

    fn create_text_layer(
        &self,
        source: &TextSource,
        x: f32,
        y: f32,
        font_size: f32,
        color: [f32; 4],
        allow_commands: bool,
    ) -> TextLayer {
        let source = LoadedTextSource::from_project(source, allow_commands);
        // Resolved once up front (e.g. a Clock source's format string
        // applied against "now") so the buffer starts out already
        // showing the right thing -- `advance_text_sources` re-polls and
        // re-shapes from here on, this is just the initial value.
        let text = source.poll(chrono::Local::now());

        let mut glyphon = self.glyphon.lock().unwrap();
        let pixel_font_size = (font_size * glyphon.canvas_height as f32).max(1.0);
        let canvas_width = glyphon.canvas_width as f32;
        let canvas_height = glyphon.canvas_height as f32;
        let mut buffer = glyphon::Buffer::new(
            &mut glyphon.font_system,
            glyphon::Metrics::new(pixel_font_size, pixel_font_size * 1.2),
        );
        buffer.set_size(
            &mut glyphon.font_system,
            Some(canvas_width),
            Some(canvas_height),
        );
        buffer.set_text(
            &mut glyphon.font_system,
            &text,
            &glyphon::Attrs::new(),
            glyphon::Shaping::Advanced,
            None,
        );
        TextLayer {
            buffer,
            text,
            source,
            x,
            y,
            font_size,
            color,
        }
    }

    /// Loads every layer of `project` onto the GPU, ready to draw. Asset
    /// paths in `project` are resolved relative to `project_dir`.
    ///
    /// `allow_commands` gates whether any `TextSource::Command` layer in
    /// `project` actually gets to run its shell command -- `false` for
    /// an untrusted project makes it behave exactly like a Command
    /// source that has never produced output (permanently "NULL"),
    /// without ever spawning anything. Deciding *what* counts as trusted
    /// is entirely the caller's job (render-server consults its trust
    /// store; `player`'s own standalone binary always passes `true` --
    /// see its module doc comment for that scope cut).
    pub fn load_scene(
        &self,
        project_dir: &Path,
        project: &Project,
        allow_commands: bool,
    ) -> Result<Vec<LoadedLayer>, String> {
        let mut layers = Vec::with_capacity(project.layers.len());
        for layer in &project.layers {
            match layer {
                Layer::Image { path, effects } => {
                    let (rgba, width, height) = open_rgba(&project_dir.join(path))?;
                    let mut image_layer = self.create_image_layer(&rgba, width, height);
                    image_layer.effects =
                        self.create_effect_chain(project_dir, width, height, effects)?;
                    layers.push(LoadedLayer::Image(image_layer));
                }
                Layer::Gif { path, effects } => {
                    let (frames, width, height) = decode_gif(&project_dir.join(path))?;
                    let cumulative_ms = cumulative_delays(&frames);
                    let total_ms = *cumulative_ms.last().expect("gif has at least one frame");
                    let mut image = self.create_image_layer(&frames[0].rgba, width, height);
                    image.effects = self.create_effect_chain(project_dir, width, height, effects)?;
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
                    effects,
                } => {
                    let (base_rgba, base_width, base_height) = open_rgba(&project_dir.join(base))?;
                    let (overlay_rgba, overlay_width, overlay_height) =
                        open_rgba(&project_dir.join(overlay))?;
                    let mut xray_layer = self.create_xray_layer(
                        &base_rgba,
                        base_width,
                        base_height,
                        &overlay_rgba,
                        overlay_width,
                        overlay_height,
                        *radius,
                    );
                    xray_layer.effects =
                        self.create_effect_chain(project_dir, base_width, base_height, effects)?;
                    layers.push(LoadedLayer::Xray(xray_layer));
                }
                Layer::Parallax {
                    path,
                    strength,
                    smoothing,
                    effects,
                } => {
                    let (rgba, width, height) = open_rgba(&project_dir.join(path))?;
                    let mut parallax_layer =
                        self.create_parallax_layer(&rgba, width, height, *strength, *smoothing);
                    parallax_layer.effects =
                        self.create_effect_chain(project_dir, width, height, effects)?;
                    layers.push(LoadedLayer::Parallax(parallax_layer));
                }
                Layer::Text {
                    x,
                    y,
                    font_size,
                    color,
                    source,
                } => {
                    layers.push(LoadedLayer::Text(self.create_text_layer(
                        source,
                        *x,
                        *y,
                        *font_size,
                        *color,
                        allow_commands,
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
                let index = cumulative_ms
                    .partition_point(|&c| c <= t)
                    .min(frames.len() - 1);
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

    /// Updates every Smoke effect's cursor position -- like
    /// `update_xray_cursors`, one shared cursor position (the same
    /// canvas/target pixel coordinates) across every layer's effect
    /// chain in a scene. Converted into each effect's own UV space
    /// using the *canvas's* resolution (`set_text_viewport`'s
    /// `canvas_width`/`canvas_height`, the same space `cursor_px`
    /// itself is expressed in) -- deliberately *not*
    /// `LoadedEffectKind::Smoke`'s own `width`/`height`, which is the
    /// layer's native asset resolution and can be a wildly different
    /// scale from the canvas (see the comment inline below).
    ///
    /// Combines the live cursor with each Smoke effect's CPU-cached
    /// `color`/`decay`/`radius` (see `LoadedLayer::set_effect_params`,
    /// which never touches the GPU buffer itself) into one full
    /// uniform-buffer write, every call -- cheap, and matches
    /// `update_xray_cursors`'s own "just rewrite the whole thing every
    /// tick" rationale.
    pub fn update_smoke_cursors(&self, layers: &[LoadedLayer], cursor_px: (f32, f32)) {
        // `cursor_px` arrives in *canvas* pixel space (the same space
        // `set_text_viewport`'s `width`/`height` establishes, and that
        // `text.x * canvas_width` already relies on) -- not each Smoke
        // effect's own offscreen chain resolution, which is a given
        // layer's *native asset* pixel size and can be a completely
        // different scale (e.g. a 3840x2160 source image against a
        // 480px-wide editor preview canvas). Normalizing by
        // `smoke.width`/`smoke.height` instead of this would leave the
        // splat clamped into whatever fraction of the canvas
        // `smoke.width`/`canvas_width` works out to, stuck near the
        // top-left corner -- exactly the bug this comment is here to
        // prevent regressing.
        let (canvas_width, canvas_height) = {
            let glyphon = self.glyphon.lock().unwrap();
            (glyphon.canvas_width, glyphon.canvas_height)
        };
        for layer in layers {
            let chain = match layer {
                LoadedLayer::Image(image) | LoadedLayer::Gif { image, .. } => &image.effects,
                LoadedLayer::Xray(xray) => &xray.effects,
                LoadedLayer::Parallax(parallax) => &parallax.effects,
                LoadedLayer::Text(_) => continue,
            };
            let Some(chain) = chain else { continue };
            for effect in &chain.effects {
                if let LoadedEffectKind::Smoke(smoke) = &effect.kind {
                    let cursor_uv = (
                        cursor_px.0 / canvas_width.max(1) as f32,
                        cursor_px.1 / canvas_height.max(1) as f32,
                    );
                    self.queue.write_buffer(
                        &smoke.uniform_buffer,
                        0,
                        &smoke_uniform_bytes(smoke.color, cursor_uv, smoke.decay, smoke.radius),
                    );
                }
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
    pub fn update_parallax(
        &self,
        layers: &mut [LoadedLayer],
        cursor_px: (f32, f32),
        dt_ms: u64,
    ) -> bool {
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

    /// Updates the pixel size that every Text layer's normalized x/y/
    /// font_size get converted against, and re-lays-out every
    /// already-loaded Text layer in `layers` to match. Call once right
    /// after `load_scene` (canvas size usually isn't known until then --
    /// see `LoadedLayer::size`'s Text fallback) and again any time the
    /// actual render target is resized, passing its real pixel
    /// dimensions -- editor's preview in particular renders at a
    /// downscaled size, not a project's "natural" size, so callers must
    /// pass whatever size they're really about to draw into.
    pub fn set_text_viewport(&self, layers: &mut [LoadedLayer], width: u32, height: u32) {
        let mut glyphon = self.glyphon.lock().unwrap();
        glyphon.canvas_width = width;
        glyphon.canvas_height = height;
        glyphon
            .viewport
            .update(&self.queue, glyphon::Resolution { width, height });
        for layer in layers {
            if let LoadedLayer::Text(text) = layer {
                let pixel_font_size = (text.font_size * height as f32).max(1.0);
                text.buffer.set_metrics_and_size(
                    &mut glyphon.font_system,
                    glyphon::Metrics::new(pixel_font_size, pixel_font_size * 1.2),
                    Some(width as f32),
                    Some(height as f32),
                );
            }
        }
    }

    /// Re-polls every Text layer's source against `now` and re-shapes
    /// only the ones whose displayed string actually changed -- the
    /// shared tick mechanism every consumer of this crate drives once
    /// per frame/tick (render-server's tick loop, player's own frame
    /// callback, editor's preview redraw), same contract shape as
    /// `advance_gifs`/`update_parallax`: returns whether anything
    /// changed, so a caller that only wants to redraw on actual change
    /// (render-server) can act on it. `now` is always caller-supplied,
    /// never read internally -- see [`LiveTextSource`]'s own doc comment
    /// for why.
    ///
    /// Metrics (font size/canvas dimensions) are re-applied
    /// unconditionally every call rather than compared by hand first --
    /// `Buffer::set_metrics_and_size` already no-ops internally when
    /// nothing actually changed, so there's no reason to duplicate that
    /// check here.
    pub fn advance_text_sources(
        &self,
        layers: &mut [LoadedLayer],
        now: chrono::DateTime<chrono::Local>,
    ) -> bool {
        let mut any_changed = false;
        let mut glyphon = self.glyphon.lock().unwrap();
        let canvas_width = glyphon.canvas_width as f32;
        let canvas_height = glyphon.canvas_height as f32;
        for layer in layers {
            if let LoadedLayer::Text(text) = layer {
                let pixel_font_size = (text.font_size * canvas_height).max(1.0);
                text.buffer.set_metrics_and_size(
                    &mut glyphon.font_system,
                    glyphon::Metrics::new(pixel_font_size, pixel_font_size * 1.2),
                    Some(canvas_width),
                    Some(canvas_height),
                );

                let new_text = text.source.poll(now);
                if new_text != text.text {
                    text.buffer.set_text(
                        &mut glyphon.font_system,
                        &new_text,
                        &glyphon::Attrs::new(),
                        glyphon::Shaping::Advanced,
                        None,
                    );
                    text.text = new_text;
                    any_changed = true;
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
        // glyphon's `prepare` only touches the device/queue/atlas -- like
        // `update_xray_cursors`'s `queue.write_buffer` calls, it can't run
        // once `pass` below already holds `encoder` mutably borrowed, so
        // it has to happen first. Always called (even with zero text
        // layers) so it clears out any glyphs prepared for a *previous*
        // frame that had text when this one doesn't.
        let mut glyphon = self.glyphon.lock().unwrap();
        let text_areas: Vec<glyphon::TextArea> = layers
            .iter()
            .filter_map(|layer| match layer {
                LoadedLayer::Text(text) => Some(glyphon::TextArea {
                    buffer: &text.buffer,
                    left: text.x * glyphon.canvas_width as f32,
                    top: text.y * glyphon.canvas_height as f32,
                    scale: 1.0,
                    bounds: glyphon::TextBounds::default(),
                    default_color: glyphon::Color::rgba(
                        srgb_u8_from_linear(text.color[0]),
                        srgb_u8_from_linear(text.color[1]),
                        srgb_u8_from_linear(text.color[2]),
                        (text.color[3].clamp(0.0, 1.0) * 255.0).round() as u8,
                    ),
                    custom_glyphs: &[],
                }),
                _ => None,
            })
            .collect();
        {
            let GlyphonState {
                font_system,
                swash_cache,
                atlas,
                viewport,
                text_renderer,
                ..
            } = &mut *glyphon;
            text_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    font_system,
                    atlas,
                    viewport,
                    text_areas,
                    swash_cache,
                )
                .expect("glyphon text layout failed");
        }

        // Effect chains run first, each in its own render pass(es) --
        // they can't be interleaved with the shared composite pass below
        // since every pass targets exactly one color attachment, and
        // these target a layer's own accumulator/scratch textures, not
        // `target_view`. A layer with no effects (`chain` is `None`,
        // the common case) is skipped entirely here and draws straight
        // into the composite pass below instead, same as before this
        // existed.
        for layer in layers {
            let (base_pipeline, base_bind_group, chain) = match layer {
                LoadedLayer::Image(image) | LoadedLayer::Gif { image, .. } => (
                    &self.image_pipeline_offscreen,
                    &image.bind_group,
                    &image.effects,
                ),
                LoadedLayer::Xray(xray) => {
                    (&self.xray_pipeline_offscreen, &xray.bind_group, &xray.effects)
                }
                LoadedLayer::Parallax(parallax) => (
                    &self.parallax_pipeline_offscreen,
                    &parallax.bind_group,
                    &parallax.effects,
                ),
                // Text never has an effect chain -- see M1's scope note
                // on `project_format::Layer::Text`.
                LoadedLayer::Text(_) => continue,
            };
            let Some(chain) = chain else { continue };

            // Base draw: the layer's own native content, straight into
            // the accumulator -- no blending needed, it's the first and
            // only thing drawn onto a texture just cleared to
            // transparent.
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("effect-base-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &chain.accumulator_view,
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
                pass.set_pipeline(base_pipeline);
                pass.set_bind_group(0, base_bind_group, &[]);
                pass.draw(0..3, 0..1);
            }

            for effect in &chain.effects {
                if !effect.enabled {
                    continue;
                }

                // Effect pass(es): this effect's raw, unmasked result
                // into scratch -- reads whatever the accumulator holds
                // so far (the layer's base content, or the previous
                // effect's already-masked-in result). Every kind but
                // Blur is a single pass straight into `scratch`; Blur
                // needs its own horizontal-then-vertical pair first
                // (see `LoadedEffectKind::Blur`'s doc comment), so it's
                // the one case with two passes here instead of one.
                match &effect.kind {
                    LoadedEffectKind::Vignette { bind_group, .. } => {
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("effect-pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &chain.scratch_view,
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
                        pass.set_pipeline(&self.vignette_pipeline);
                        pass.set_bind_group(0, bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                    LoadedEffectKind::ColorAdjust { bind_group, .. } => {
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("effect-pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &chain.scratch_view,
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
                        pass.set_pipeline(&self.coloradjust_pipeline);
                        pass.set_bind_group(0, bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                    LoadedEffectKind::Blur {
                        horizontal_bind_group,
                        vertical_bind_group,
                        ..
                    } => {
                        {
                            let mut pass =
                                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                    label: Some("blur-horizontal-pass"),
                                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                        view: &chain.blur_temp_view,
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
                            pass.set_pipeline(&self.blur_pipeline);
                            pass.set_bind_group(0, horizontal_bind_group, &[]);
                            pass.draw(0..3, 0..1);
                        }
                        {
                            let mut pass =
                                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                    label: Some("blur-vertical-pass"),
                                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                        view: &chain.scratch_view,
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
                            pass.set_pipeline(&self.blur_pipeline);
                            pass.set_bind_group(0, vertical_bind_group, &[]);
                            pass.draw(0..3, 0..1);
                        }
                    }
                    LoadedEffectKind::Smoke(smoke) => {
                        // Snapshot last frame's persistent state into
                        // `temp` -- a plain GPU-to-GPU copy, not a
                        // render pass, so it doesn't hit the
                        // can't-read-and-write-the-same-texture
                        // restriction the decay+splat pass below would
                        // otherwise run into by sampling and targeting
                        // `state_texture` at once.
                        encoder.copy_texture_to_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture: &smoke.state_texture,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::TexelCopyTextureInfo {
                                texture: &smoke.temp_texture,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::Extent3d {
                                width: smoke.width,
                                height: smoke.height,
                                depth_or_array_layers: 1,
                            },
                        );

                        // Reads `temp` (the snapshot just taken above),
                        // writes the decayed-plus-splatted result back
                        // into `state_view` directly -- there's no
                        // separate `scratch` write for Smoke, this *is*
                        // both its next frame's persistent state and
                        // this frame's raw output for the mask-blend
                        // pass below (see `create_effect_chain`'s
                        // `mask_source_view` selection).
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("smoke-pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &smoke.state_view,
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
                        pass.set_pipeline(&self.smoke_pipeline);
                        pass.set_bind_group(0, &smoke.bind_group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                }

                // Mask-blend pass: mixes scratch back into the
                // accumulator by the mask's value, in place -- see
                // `mask_blend.wgsl`'s module doc comment for why this
                // only needs to read `scratch`, not the accumulator too.
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("mask-blend-pass"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &chain.accumulator_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_pipeline(&self.mask_pipeline);
                    pass.set_bind_group(0, &effect.mask.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
        }

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
            // Every variant here draws the same way -- one fullscreen
            // triangle through its own pipeline and bind group -- so
            // this only needs to pick which of those two, not repeat the
            // three draw calls per variant. A layer with an effect
            // chain (already fully resolved by the pre-pass loop above)
            // composites through the ordinary `image_pipeline` sampling
            // its accumulator, exactly like a plain Image layer would --
            // its own native pipeline/bind group was only needed for
            // that chain's now-finished base-draw pass.
            let (pipeline, bind_group) = match layer {
                LoadedLayer::Image(image) | LoadedLayer::Gif { image, .. } => {
                    match &image.effects {
                        Some(chain) => (&self.image_pipeline, &chain.composite_bind_group),
                        None => (&self.image_pipeline, &image.bind_group),
                    }
                }
                LoadedLayer::Xray(xray) => match &xray.effects {
                    Some(chain) => (&self.image_pipeline, &chain.composite_bind_group),
                    None => (&self.xray_pipeline, &xray.bind_group),
                },
                LoadedLayer::Parallax(parallax) => match &parallax.effects {
                    Some(chain) => (&self.image_pipeline, &chain.composite_bind_group),
                    None => (&self.parallax_pipeline, &parallax.bind_group),
                },
                // Text isn't part of this per-layer pipeline dispatch at
                // all -- glyphon batches every prepared text area into
                // one `render()` call below instead, so it always draws
                // on top of everything else regardless of its position
                // in the project's own layer list.
                LoadedLayer::Text(_) => continue,
            };
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        let GlyphonState {
            atlas,
            viewport,
            text_renderer,
            ..
        } = &*glyphon;
        text_renderer
            .render(atlas, viewport, &mut pass)
            .expect("glyphon text render failed");
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
            eprintln!(
                "WPLINUX_GPU={want:?} matched adapter {:?}",
                adapter.get_info().name
            );
            return adapter;
        }
        eprintln!(
            "WPLINUX_GPU={want:?} matched no candidate adapter, falling back to automatic selection"
        );
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

/// Converts one linear-light channel (0.0..=1.0, out-of-range clamped) to
/// an sRGB-gamma-encoded byte -- the standard sRGB transfer function,
/// matching `ecolor::gamma_u8_from_linear_f32` (egui's own equivalent,
/// not reused directly since `player` doesn't otherwise depend on egui).
///
/// `Layer::Text::color` is filled in by editor's
/// `ui.color_edit_button_rgba_unmultiplied`, which egui documents as
/// operating in **linear** RGB space -- but glyphon's `TextAtlas` (built
/// with its default `ColorMode::Accurate`) expects the vertex color it's
/// handed to already be gamma-encoded, and un-does that encoding itself
/// (`srgb_to_linear` in its `shader.wgsl`) before using it. Passing the
/// linear float straight through as a byte (a naive `* 255.0`) skips the
/// encoding this step assumes already happened, so glyphon's own decode
/// then applies to a value that was never encoded -- silently shifting
/// every non-primary color, most visibly in the midtones. Confirmed with
/// a color picker against the actual rendered output, not just reasoned
/// about from the two API docs alone.
fn srgb_u8_from_linear(linear: f32) -> u8 {
    if linear <= 0.0 {
        0
    } else if linear <= 0.0031308 {
        (linear * 3294.6).round() as u8
    } else if linear <= 1.0 {
        (269.025 * linear.powf(1.0 / 2.4) - 14.025).round() as u8
    } else {
        255
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A fixed, arbitrary point in time (nowhere near a DST transition,
    /// so `Local.with_ymd_and_hms` resolves unambiguously regardless of
    /// which timezone this test happens to run in) -- these tests only
    /// assert on the hour/minute/day fields formatting comes out to, not
    /// on any real wall-clock behavior, so no GPU/device is needed at
    /// all (unlike almost everything else in this module).
    fn fixed_now() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2024, 6, 15, 9, 5, 30)
            .unwrap()
    }

    #[test]
    fn literal_source_always_returns_its_fixed_text() {
        let source = LiteralSource {
            text: "hello".to_string(),
        };
        assert_eq!(source.poll(fixed_now()), "hello");
    }

    #[test]
    fn srgb_u8_from_linear_matches_known_reference_points() {
        assert_eq!(srgb_u8_from_linear(0.0), 0);
        assert_eq!(srgb_u8_from_linear(1.0), 255);
        // Well-known sRGB fact: linear 0.5 (physical half-brightness)
        // encodes to byte 188, not the naively-expected 128 -- this is
        // exactly the compression the naive `* 255.0` this replaced was
        // missing.
        assert_eq!(srgb_u8_from_linear(0.5), 188);
        // Out-of-range input clamps instead of wrapping/panicking.
        assert_eq!(srgb_u8_from_linear(-1.0), 0);
        assert_eq!(srgb_u8_from_linear(2.0), 255);
    }

    #[test]
    fn clock_source_formats_against_the_given_time() {
        let source = ClockSource {
            format: "%H:%M".to_string(),
        };
        assert_eq!(source.poll(fixed_now()), "09:05");
    }

    #[test]
    fn clock_source_with_a_garbled_format_falls_back_to_the_raw_string_instead_of_panicking() {
        let format = "%Q not a real specifier %".to_string();
        let source = ClockSource {
            format: format.clone(),
        };
        assert_eq!(source.poll(fixed_now()), format);
    }

    #[test]
    fn loaded_text_source_from_project_dispatches_to_the_matching_variant() {
        let literal = LoadedTextSource::from_project(
            &TextSource::Literal {
                text: "hi".to_string(),
            },
            true,
        );
        assert_eq!(literal.poll(fixed_now()), "hi");

        let clock = LoadedTextSource::from_project(
            &TextSource::Clock {
                format: "%Y".to_string(),
            },
            true,
        );
        assert_eq!(clock.poll(fixed_now()), "2024");
    }

    #[test]
    fn untrusted_command_source_never_runs_and_always_shows_null() {
        let source = CommandSource::untrusted("echo should-never-run".to_string(), 60);
        // No thread was ever spawned for this one, so there's nothing to
        // wait for -- the shared cell simply starts and stays `None`.
        assert_eq!(source.poll(fixed_now()), "NULL");
    }

    #[test]
    fn loaded_text_source_matches_compares_configuration_not_identity() {
        let literal = LoadedTextSource::from_project(
            &TextSource::Literal {
                text: "hi".to_string(),
            },
            true,
        );
        assert!(literal.matches(&TextSource::Literal {
            text: "hi".to_string()
        }));
        assert!(!literal.matches(&TextSource::Literal {
            text: "bye".to_string()
        }));
        assert!(!literal.matches(&TextSource::Clock {
            format: "%H".to_string()
        }));

        let command = LoadedTextSource::from_project(
            &TextSource::Command {
                command: "date".to_string(),
                interval_secs: 5,
            },
            false,
        );
        assert!(command.matches(&TextSource::Command {
            command: "date".to_string(),
            interval_secs: 5
        }));
        assert!(!command.matches(&TextSource::Command {
            command: "date".to_string(),
            interval_secs: 6
        }));
    }

    #[test]
    fn run_command_with_timeout_returns_trimmed_stdout_on_success() {
        assert_eq!(
            run_command_with_timeout("echo hello"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn run_command_with_timeout_returns_none_on_nonzero_exit() {
        assert_eq!(run_command_with_timeout("sh -c 'exit 1'"), None);
    }

    #[test]
    fn run_command_with_timeout_returns_none_for_empty_output() {
        assert_eq!(run_command_with_timeout("printf ''"), None);
    }

    #[test]
    fn command_that_hangs_gets_killed_instead_of_hanging_the_caller() {
        let start = std::time::Instant::now();
        let result =
            run_command_with_timeout_inner("sleep 10", std::time::Duration::from_millis(300));
        assert_eq!(result, None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "should give up around the 300ms timeout, not wait out the full sleep"
        );
    }
}
