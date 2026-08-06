# WP Linux

An animated, interactive wallpaper engine for Linux -- a Wallpaper Engine-style
alternative for **KDE Plasma 6 on Wayland (KWin)**. Build a layer stack
(static pictures, looping GIFs, cursor-reactive "xray" overlays) in a small
GPU-accelerated editor, then run it as your desktop wallpaper.

## Status

Early stage, single-platform: **KDE Plasma 6 + KWin/Wayland only**. Nothing
else (X11, GNOME, other compositors) is supported or planned right now.
`render-server` has no autostart integration yet -- it's started by hand for
the time being.

## Features

- **Image** layers -- a static picture.
- **Gif** layers -- a looping animation, timed from the gif's own per-frame
  delays.
- **Xray** layers -- a base picture with a second picture revealed in a
  circle around the cursor, using the compositor's true global cursor
  position (works correctly even under desktop icons, which would otherwise
  swallow mouse hover from a normal window).
- Layers composite bottom to top with alpha blending on the GPU (wgpu,
  Vulkan or GL).
- Rendering automatically freezes on the last frame when the system's power
  profile switches to power-saver (via `power-profiles-daemon`), and resumes
  when it switches back -- nothing to pause by hand.
- Prefers the integrated GPU over a suspended discrete GPU on hybrid-GPU
  laptops, avoiding a multi-hundred-millisecond wake-up on every frame.
- `editor` has a live GPU preview built from the exact same compositor and
  shaders `render-server` uses in production, not a separate
  reimplementation -- gif animation and xray cursor reactivity look right
  while you're still editing.

## How it's put together

| Component | What it does |
|---|---|
| `crates/project-format` | Shared `project.json` schema (layers + target fps) -- the on-disk wallpaper project format. |
| `crates/player` | Shared GPU compositor library (`SceneRenderer`) used by everything below, plus its own standalone Wayland/layer-shell test binary. |
| `crates/render-server` | The actual runtime renderer: a headless wgpu process that serves composited frames to the Plasma plugin over local HTTP, receives the cursor position over D-Bus, and watches the power profile. |
| `crates/editor` | Desktop app (egui) for building and editing wallpaper projects, with the live preview. |
| `plasma-plugin/` | The Plasma "Wallpaper" plugin you pick in System Settings; talks to `render-server`. |
| `kwin-script/` | KWin script forwarding the true global cursor position to `render-server` over D-Bus. |

## Requirements

- KDE Plasma 6 running on Wayland (KWin).
- A GPU with Vulkan or OpenGL/EGL support.
- `kpackagetool6` (ships with Plasma) to install the two KDE packages below.
- Optional: `power-profiles-daemon`, for the automatic power-saver freeze --
  without it `render-server` just always renders continuously.
- To build from source: a recent stable Rust toolchain (edition 2024).

## Installation

### Option A: prebuilt release

Grab the latest release from
[Releases](https://github.com/AzimovIz/WP_Linux/releases):
`render-server`, `player`, `editor`, `wp-linux-plasma-plugin.zip`,
`wp-linux-kwin-script.zip`.

```sh
chmod +x render-server player editor
```

Install the two KDE packages:

```sh
kpackagetool6 --type=Plasma/Wallpaper --install wp-linux-plasma-plugin.zip
kpackagetool6 --type=KWin/Script --install wp-linux-kwin-script.zip
```

Then enable the script in **System Settings -> Window Management -> KWin
Scripts** (tick "WP Linux Cursor Bridge"). `render-server` also tries to
load it automatically on startup, but that only works when it's run
straight from a source checkout -- for a downloaded release binary this
manual step is required for Xray layers to react to the cursor.

### Option B: build from source

```sh
git clone https://github.com/AzimovIz/WP_Linux.git
cd WP_Linux
cargo build --release --workspace
```

Binaries land in `target/release/`. Install the KDE packages the same way
as above, pointing at the directories directly:

```sh
kpackagetool6 --type=Plasma/Wallpaper --install plasma-plugin/package
kpackagetool6 --type=KWin/Script --install kwin-script/package
```

(Run from a source checkout, `render-server` can also auto-load the KWin
script itself on startup -- installing it by hand still works and is the
more robust option either way.)

## Usage

1. Run `editor`, build a layer stack (Image / Gif / Xray), and save it as a
   project folder.
2. Run `render-server` (from a terminal -- no autostart yet).
3. Right-click the desktop -> **Configure Desktop and Wallpaper** (or
   **System Settings -> Appearance -> Wallpaper**), choose **WP Linux
   Wallpaper**, then point it at the project folder you saved.

## Known limitations

- No autostart for `render-server` yet -- start it by hand each session.
- One shared canvas and cursor position across every monitor -- the same
  wallpaper renders identically on all screens.
- `player`'s standalone Wayland renderer stretches layers to fill the
  screen with no aspect-ratio-correct cropping, unlike `render-server`
  (what the Plasma plugin actually uses for display).

## License

MIT (LICENSE file to be added).
