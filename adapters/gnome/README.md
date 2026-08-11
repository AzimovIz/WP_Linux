# GNOME adapter

A single GNOME Shell extension (`adapters/gnome/extension`, uuid
`wp-linux@wplinux.dev`) doing both jobs [adapters/kde](../kde) splits across
two KPackages, since on GNOME both jobs run inside the same `gnome-shell`
process:

1. **Display** (`monitorLayer.js`): polls render-server's local HTTP API
   (`GET /meta`, `GET /frame`, `POST /geometry` -- see
   `crates/render-server/src/main.rs`'s module doc comment for the exact
   contract) and shows the frames behind desktop windows, one `St.Widget`
   per monitor, inserted into `Main.layoutManager._backgroundGroup`.
2. **Cursor** (`cursorForwarder.js`): polls `global.get_pointer()` and
   forwards it to render-server's
   `dev.wplinux.CursorBridge.SetCursorPosition` D-Bus method -- the GNOME
   equivalent of
   [adapters/kde/kwin-script/package/contents/code/main.js](../kde/kwin-script/package/contents/code/main.js).
3. **Overview preview** (`overviewBackground.js`, best-effort/cosmetic):
   clones each monitor's actor into GNOME's own zoomed-out Overview
   workspace preview, which otherwise shows the plain desktop background
   instead of the wallpaper -- see its own doc comment for why, and
   [Known limitations](#known-limitations) below for how it can fail.

Nothing in `crates/render-server` or `crates/wp_linux_editor` needs to
change for any of this -- the HTTP/D-Bus contract they expose is already
desktop-agnostic. Same as on KDE, which project is assigned to which
monitor is entirely `wp_linux_editor`'s concern (it writes `monitors.json`
and pushes to render-server directly) -- this adapter just shows whatever
render-server currently has loaded.

## Installing / enabling / disabling

The top-level `install.sh` calls `adapters/gnome/install.sh`, which copies
`adapters/gnome/extension` to
`~/.local/share/gnome-shell/extensions/wp-linux@wplinux.dev/` and runs
`gnome-extensions enable wp-linux@wplinux.dev`.

To turn the wallpaper back off: disable the whole extension -- either the
"WP Linux Wallpaper" toggle in the **Extensions** app, or:

```sh
gnome-extensions disable wp-linux@wplinux.dev
```

There's no finer-grained "unassign this monitor" yet (`wp_linux_editor`
currently only supports *assigning* a project to a monitor, see its own
`assign()`), and the extension doesn't hide itself just because
`render-server` stopped responding -- it keeps showing the last frame it
fetched forever. Disabling the extension is the only reliable "off switch"
right now.

## Known limitations

Found and fixed on a single real machine (Arch Linux, GNOME Shell 50, one
monitor) over several rounds of trial and error -- treat all of this as
"works here," not "verified everywhere":

- **Stretches instead of crops.** `monitorLayer.js` sets a fixed
  `background-size` in pixels; Clutter/St has no built-in "cover" gravity
  like Qt Quick's `Image.PreserveAspectCrop`, which
  `adapters/kde/plasma-plugin` uses. Only visible when a project's canvas
  resolution doesn't match the monitor's own.
- **Frames round-trip through a temp file.** The more direct route --
  uploading fetched pixels straight into a GPU texture via
  `Clutter.Image` -- doesn't work: `new Clutter.Image()` throws
  `TypeError: ... is not a constructor` on GNOME Shell 50 (confirmed on
  real hardware; that low-level content API isn't stable across Mutter
  versions). `monitorLayer.js` instead writes each fetched frame to
  `$XDG_RUNTIME_DIR` (tmpfs, not real disk I/O) and shows it via CSS
  `background-image`, which is slower for animated (Xray/Gif/Parallax)
  projects than KDE's more direct path. Not benchmarked; if `gnome-shell`'s
  CPU usage looks high while an animated wallpaper is running, this
  round-trip is the first thing to revisit.
- **`metadata.json`'s `shell-version` needs bumping.** GNOME Shell refuses
  to enable an extension whose `metadata.json` doesn't list its own major
  version -- it shows up as "out of date" instead
  (`gdbus call --session --dest org.gnome.Shell --object-path
  /org/gnome/Shell --method org.gnome.Shell.Extensions.GetExtensionInfo
  wp-linux@wplinux.dev` reports `state: 4` when this happens, and
  `gnome-extensions enable` fails with a confusing "does not exist").
  Currently covers 45-51; add the new number here whenever a GNOME release
  ships.
- **Overview workspace preview is best-effort and can silently stop
  working.** `overviewBackground.js` monkeypatches `WorkspaceBackground`
  (`js/ui/workspace.js`), gnome-shell's *private* Overview implementation --
  far more likely to change across GNOME versions than the stable,
  decades-old `_backgroundGroup` field the real wallpaper relies on. Every
  failure path there is caught and logged
  (`journalctl --user -b 0 | grep 'wp-linux:'`); it degrading or breaking
  never affects the real desktop wallpaper. It also shows nothing for the
  very first Overview right after login -- Activities can render before the
  extension has fetched its first frame from render-server, so early boot
  briefly shows the plain GNOME background there.
- **`_backgroundGroup` insertion point is only verified on one setup.**
  Confirmed working on GNOME Shell 50 / Arch / a single monitor. If frames
  end up hidden behind GNOME's own background instead of replacing it on
  some other version/driver/multi-monitor combination,
  `backgroundContainer()` in `monitorLayer.js` is the one place to change.
- Multi-monitor and X11 sessions are untested so far (only tried on a
  single-monitor Wayland session).

## Development

No `dev-load`-style script yet (unlike
`adapters/kde/dev-load-kwin-script.sh`) -- iterate by copying
`adapters/gnome/extension/*` straight into
`~/.local/share/gnome-shell/extensions/wp-linux@wplinux.dev/`, then:

```sh
gnome-extensions disable wp-linux@wplinux.dev
gnome-extensions enable wp-linux@wplinux.dev
```

That's enough to pick up most changes. If it doesn't seem to take effect
(e.g. right after installing the extension for the very first time), log
out and back in -- unlike KWin scripts, GNOME Shell can't reload itself on
Wayland the way `Alt+F2 r` does on X11.

To see what actually went wrong when something doesn't work, every error
path in this extension logs through `console.error`/`console.warn` with a
`wp-linux:` (or `wp-linux cursorbridge:`) prefix:

```sh
journalctl --user -b 0 --no-pager | grep "wp-linux"
```
