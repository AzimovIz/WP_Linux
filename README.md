# WP Linux

*[Читать на русском](README_RU.md)*

<p align="center">
  <img src="assets/wp_linux_logo.svg" alt="WP Linux logo" width="140">
</p>

An animated, interactive wallpaper engine for Linux -- a Wallpaper Engine-style
alternative for **KDE Plasma 6 on Wayland (KWin)**, with experimental
**GNOME Shell 45+ (Wayland)** support. Build a layer stack (static pictures,
looping GIFs, cursor-reactive "xray"/parallax overlays) in a small
GPU-accelerated editor, then run it as your desktop wallpaper.

## Screenshots

<p align="center">
  <img src="assets/main_window.png" alt="The main window where you can apply a wallpaper to the desktop. " width="49%">
  <img src="assets/wp_linux_editor.png" alt="Building a layer stack in the WP Linux editor, with the live GPU preview on the left" width="49%">
  <img src="assets/KDE_settings.png" alt="Picking WP Linux Wallpaper in KDE System Settings" width="49%">
</p>

Parallax layer in action, reacting to the cursor on the real desktop:

[paralax_preview.webm](https://github.com/user-attachments/assets/04d47d50-a1a5-4c80-b399-aed54b54ba50)

<p align="center"><sub>
Artwork in the parallax demo above is from the
<a href="https://webflow.com/made-in-webflow/website/parallax-template-cloneable">Webflow parallax template</a>,
used here for demonstration only -- not original work, all rights remain with its creators.
</sub></p>

## Status

Early stage. The renderer and editor (`crates/`) are desktop-agnostic.
**KDE Plasma 6 + KWin/Wayland** (`adapters/kde`) is the stable, well-exercised
adapter. **GNOME Shell 45+ on Wayland** (`adapters/gnome`) also works now, but
is new and comes with real rough edges -- see
[adapters/gnome/README.md](adapters/gnome/README.md) and this file's own
[Known limitations](#known-limitations) below before relying on it. Other
desktops aren't supported yet.

## Features

- **Image** layers -- a static picture.
- **Gif** layers -- a looping animation, timed from the gif's own per-frame
  delays.
- **Xray** layers -- a base picture with a second picture revealed in a
  circle around the cursor, using the compositor's true global cursor
  position (works correctly even under desktop icons, which would otherwise
  swallow mouse hover from a normal window).
- **Parallax** layers -- a picture that pans opposite the cursor to fake
  depth; stack several with increasing strength for a full parallax effect.
  Auto-zoomed just enough that no edge is ever exposed, regardless of the
  source picture's own size.
- **Text** layers -- a string drawn at an arbitrary position, dragged
  directly on the live preview rather than typed into sliders. Three
  sources: a fixed string, a live clock (any strftime-style format), or a
  shell command's output, re-run on a timer. Uses a bundled font (Noto
  Sans, full Latin/Cyrillic coverage) by default, or your own `.ttf`/`.otf`.
- **Adjustment** layers -- no picture of their own; take whatever every
  layer below them has already composited and run their own effect stack
  on *that* -- one color-grade or pulse effect over the whole finished
  picture instead of repeating it on every layer.
- Any Image/Gif/Xray/Parallax/Adjustment layer can carry a stack of
  **post-processing effects**: Vignette, Color adjust, Blur, a persistent
  cursor-following Smoke trail, or a fully custom effect you write
  yourself in WGSL (see
  [Writing your own Shader effect](#writing-your-own-shader-effect)) --
  edited live, with the preview updating immediately.
- Each effect can be **masked** to only part of the layer: a circle or
  gradient (dragged/scaled/rotated directly on the preview, not just
  sliders), or a picture -- either picked from disk or painted by hand
  with a brush right on the preview.
- A Text layer's Command source only ever runs for projects you've saved
  yourself in the editor (auto-trusted on save) -- opening someone else's
  project file never silently executes a shell command; an untrusted
  Command source just shows "NULL".
- Layers composite bottom to top with alpha blending on the GPU (wgpu,
  Vulkan or GL), in the order they appear in the layer list.
- Rendering automatically freezes on the last frame when the system's power
  profile switches to power-saver (via `power-profiles-daemon`), and resumes
  when it switches back -- nothing to pause by hand.
- Prefers the integrated GPU over a suspended discrete GPU on hybrid-GPU
  laptops, avoiding a multi-hundred-millisecond wake-up on every frame.
- `wp_linux_editor` has a live GPU preview built from the exact same compositor and
  shaders `render-server` uses in production, not a separate
  reimplementation -- gif animation, xray cursor reactivity, and every
  effect/mask above look right while you're still editing.

## How it's put together

| Component | What it does |
|---|---|
| `crates/project-format` | Shared `project.json` schema -- layers (their post-processing effects, masks, and, for Text, its font) plus target fps -- the on-disk wallpaper project format. |
| `crates/player` | Shared GPU compositor library (`SceneRenderer`) used by everything below, plus its own standalone Wayland/layer-shell test binary. |
| `crates/render-server` | The actual runtime renderer: a headless wgpu process that serves composited frames to the Plasma plugin over local HTTP, receives the cursor position over D-Bus, and watches the power profile. |
| `crates/wp_linux_editor` | Desktop app (egui) for building and editing wallpaper projects, with the live preview. |
| `adapters/kde/plasma-plugin` | The Plasma "Wallpaper" plugin you pick in System Settings; talks to `render-server`. |
| `adapters/kde/kwin-script` | KWin script forwarding the true global cursor position to `render-server` over D-Bus. |
| `adapters/gnome/extension` | GNOME Shell extension doing both of the above jobs in one process (GJS) -- see [adapters/gnome/README.md](adapters/gnome/README.md). |

## Requirements

- **KDE Plasma 6 on Wayland (KWin)**, or **GNOME Shell 45+ on Wayland**
  (experimental -- see [Known limitations](#known-limitations)).
- A GPU with Vulkan or OpenGL/EGL support.
- KDE: `kpackagetool6` (ships with Plasma) to install the two KDE packages.
- GNOME: `gnome-extensions` (ships with GNOME Shell) to install/enable the
  extension.
- Optional: `power-profiles-daemon`, for the automatic power-saver freeze --
  without it `render-server` just always renders continuously.
- To build from source: a recent stable Rust toolchain (edition 2024).

## Installation

### Install script (any distro)

Downloads the latest release archive and sets everything up under
`$HOME` -- binaries in `~/.local/bin`, the desktop-specific integration
(KDE: the two Plasma/KWin packages via `kpackagetool6`; GNOME: the Shell
extension via `gnome-extensions`, auto-detected from `$XDG_CURRENT_DESKTOP`),
and `render-server` registered to autostart via an XDG autostart `.desktop`
file (`~/.config/autostart/`) so it starts automatically with your graphical
session -- no systemd dependency. Toggle this later from `wp_linux_editor`
itself with the "Launch at login" checkbox on the Wallpapers tab.

```sh
curl -fsSL https://github.com/AzimovIz/WP_Linux/releases/latest/download/install.sh | bash
```

To remove everything it installed later:

```sh
curl -fsSL https://github.com/AzimovIz/WP_Linux/releases/latest/download/uninstall.sh | bash
```

(Your saved wallpaper projects aren't touched by either script.)

The install script also downloads a handful of [example wallpapers](https://github.com/AzimovIz/WP_Linux/releases/tag/WallpaperExamples)
into `~/.local/share/wp_linux/wallpapers/` so there's something to pick in
**WP Linux Wallpaper** right away. You can also grab that archive yourself
and unzip it into the same folder.

### Build from source

```sh
git clone https://github.com/AzimovIz/WP_Linux.git
cd WP_Linux
cargo build --release --workspace
```

Binaries land in `target/release/`. Install the KDE packages by pointing
`kpackagetool6` at the directories directly:

```sh
kpackagetool6 --type=Plasma/Wallpaper --install adapters/kde/plasma-plugin/package
kpackagetool6 --type=KWin/Script --install adapters/kde/kwin-script/package
```

(While developing from a source checkout, `adapters/kde/dev-load-kwin-script.sh`
loads the KWin script straight from the checkout via KWin's scripting
D-Bus interface, so you don't need to reinstall the KPackage on every
change -- run it by hand once per KWin restart. Not needed for a normal
install, where the KPackage install above is enough.)

On GNOME, install the extension by copying it into place and enabling it
(there's no `dev-load`-style script yet -- see
[adapters/gnome/README.md](adapters/gnome/README.md#development)):

```sh
mkdir -p ~/.local/share/gnome-shell/extensions/wp-linux@wplinux.dev
cp -r adapters/gnome/extension/. ~/.local/share/gnome-shell/extensions/wp-linux@wplinux.dev/
gnome-extensions enable wp-linux@wplinux.dev
```

Log out and back in afterwards -- GNOME Shell only picks up a brand new
extension on a fresh start.

## Usage

1. Run `wp_linux_editor`, build a layer stack (Image / Gif / Xray / Parallax /
   Text / Adjustment), and save it as a project folder.
2. Make sure `render-server` is running (the install script sets it up as
   a background service; from a source build, run it by hand).
3. **KDE**: Right-click the desktop -> **Configure Desktop and Wallpaper**
   (or **System Settings -> Appearance -> Wallpaper**), choose **WP Linux
   Wallpaper**, then point it at the project folder you saved. Each
   monitor has its own wallpaper config in Plasma, so different screens
   can point at entirely different projects (e.g. parallax on one,
   xray on another).

   **GNOME**: nothing to pick on the desktop side -- there's no wallpaper
   picker UI. As long as the extension is enabled (the install script does
   this for you), assigning a project to a monitor from `wp_linux_editor`'s
   Wallpapers tab is enough; the extension shows whatever render-server has
   loaded for each monitor. To turn a GNOME wallpaper back off, disable the
   whole extension for now (Extensions app, or
   `gnome-extensions disable wp-linux@wplinux.dev`) -- see
   [adapters/gnome/README.md](adapters/gnome/README.md) for why there's no
   finer-grained toggle yet.

## Known limitations

- `player`'s standalone Wayland renderer stretches layers to fill the
  screen with no aspect-ratio-correct cropping, unlike `render-server`
  (what the Plasma plugin actually uses for display). It also still
  shares one cursor position across every output it draws to, unlike
  `render-server` which tracks each monitor's project and cursor
  independently.
- The GNOME adapter (`adapters/gnome`) is new and experimental -- see
  [adapters/gnome/README.md](adapters/gnome/README.md) for its full list of
  known issues (no aspect-ratio-correct cropping, frames round-trip through
  a temp file, `metadata.json` needs a version bump on every new GNOME
  release, the Overview workspace preview integration is best-effort and can
  silently stop working, and more).

## Writing your own Shader effect

A Shader effect (available on any layer that supports effects, or on an
Adjustment layer to post-process the whole composite) is a `.wgsl`
fragment shader you write and point the editor at -- no engine changes,
no recompiling anything. It has to follow one fixed shape:

- Bind group 0 has exactly three bindings, in this order: the input
  texture (`binding(0)`), a sampler (`binding(1)`), and a uniform buffer
  with your parameters (`binding(2)`).
- Two entry points, `vs_main` and `fs_main` -- `vs_main` is always the
  same fullscreen-triangle boilerplate below, only `fs_main` is yours to
  write.
- Your uniform struct's first three fields, in this exact order, are
  filled in by the engine every frame: `cursor: vec2<f32>` (current
  cursor position, 0.0..=1.0, or far off-canvas if the pointer isn't over
  the wallpaper), `time: f32` (seconds since the layer loaded), and
  `canvas_aspect: f32` (width / height).
- Any `f32` field after that becomes a parameter, with a slider
  automatically generated for it in the editor -- annotate each one with
  a trailing JSON comment: `// {"label": "...", "default": ...,
  "range": [min, max]}`. Only `f32` params are supported for now (no
  color/vec or texture params yet).

A minimal effect that pulses the picture's brightness over time:

```wgsl
struct ShaderEffectParams {
    cursor: vec2<f32>,
    time: f32,
    canvas_aspect: f32,
    u_speed: f32,    // {"label": "Speed", "default": 2.0, "range": [0.1, 10.0]}
    u_strength: f32, // {"label": "Strength", "default": 0.3, "range": [0.0, 1.0]}
};

@group(0) @binding(0) var input_texture: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> params: ShaderEffectParams;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let pos = positions[vertex_index];
    var out: VertexOutput;
    out.clip_position = vec4<f32>(pos, 0.0, 1.0);
    out.uv = vec2<f32>(pos.x * 0.5 + 0.5, 0.5 - pos.y * 0.5);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let color = textureSample(input_texture, input_sampler, in.uv);
    let pulse = 1.0 + sin(params.time * params.u_speed) * params.u_strength;
    return vec4<f32>(color.rgb * pulse, color.a);
}
```

Pick this file in the effect's "Shader" panel like any other asset; a
shader that fails to compile shows the error right there instead of
crashing the editor. Whatever mask is set on this effect (see Features
above) still applies on top, same as any built-in effect.

## License

[MIT](LICENSE)
