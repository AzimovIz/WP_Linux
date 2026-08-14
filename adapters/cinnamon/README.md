# Cinnamon adapter

A single Cinnamon extension (`adapters/cinnamon/extension`, uuid
`wp-linux@wplinux.dev`) doing both jobs [adapters/kde](../kde) splits across
two KPackages, the same split [adapters/gnome](../gnome) already makes for
GNOME Shell -- both jobs run inside the same `cinnamon` process, since
Cinnamon (via Muffin, a Mutter fork) shares GNOME Shell's Clutter/St/GJS
lineage:

1. **Display** (`MonitorLayer` in `extension.js`): polls render-server's
   local HTTP API (`GET /meta`, `GET /frame`, `POST /geometry` -- see
   `crates/render-server/src/main.rs`'s module doc comment for the exact
   contract) and shows the frames behind desktop windows, one real
   `Meta.BackgroundActor` per monitor -- the same native background type
   Muffin's own compositor creates and manages, not a generic widget
   placed nearby (see below for why that distinction turned out to
   matter a great deal).
2. **Cursor** (`startCursorForwarder`/`_cursorTick` in `extension.js`):
   polls `global.get_pointer()` and forwards it to render-server's
   `dev.wplinux.CursorBridge` D-Bus method -- the Cinnamon equivalent of
   [adapters/kde/kwin-script/package/contents/code/main.js](../kde/kwin-script/package/contents/code/main.js).

Nothing in `crates/render-server` or `crates/wp_linux_editor` needs to
change for any of this -- the HTTP/D-Bus contract they expose is already
desktop-agnostic. Same as on KDE/GNOME, which project is assigned to which
monitor is entirely `wp_linux_editor`'s concern (it writes `monitors.json`
and pushes to render-server directly) -- this adapter just shows whatever
render-server currently has loaded.

## Why one file, and why not the GNOME extension's own X11-window ancestor

This project's X11 support started as a standalone process painting
directly into an override-redirect X11 window pinned to the bottom of the
stack (the classic `xwinwrap` technique) -- desktop-agnostic in principle,
but it risked fighting Muffin's own compositor for stacking on Cinnamon
specifically: Muffin renders the desktop background as part of its *own*
Clutter scene graph, not via the classic X11 root-pixmap convention
`feh`/`hsetroot` rely on. This extension sidesteps that question by living
inside that same scene graph instead of fighting it from outside via X11
-- and, after two failed attempts at placing a generic widget somewhere
in it by hand (see `extension.js`'s own top doc comment for the full
story), by becoming a real `Meta.BackgroundActor` rather than trying to
imitate one. Muffin's compositor (`src/compositor/compositor.c`,
`sync_actor_stacking`) classifies every child of `global.window_group` by
**GObject type** and re-lowers backgrounds, the desktop-icon layer, and
regular windows into the correct relative order on every single stacking
change; anything of a type it doesn't recognize is never touched by that
re-lowering and drifts to the top over time regardless of where it was
first placed or how carefully. A real `Meta.BackgroundActor` is
recognized and correctly managed automatically, the same as Cinnamon's
own native background -- confirmed live via Looking Glass before
rewriting `extension.js` around it.

Everything lives in one `extension.js` rather than adapters/gnome's
three-file split: Cinnamon extensions still use the classic
`imports.*`/global-function shape (`init`/`enable`/`disable`), not GNOME
Shell 45+'s ESM rewrite, and this project had no way to verify cross-file
import behavior across Cinnamon versions without real hardware. One file
keeps the failure surface to "does this file load at all," not "did the
right import mechanism resolve on this particular Cinnamon version."

The render-server HTTP client is hand-rolled over a raw `Gio.SocketClient`
TCP connection rather than libsoup -- Cinnamon systems can have either
libsoup 2 or libsoup 3's GI typelib installed, with meaningfully different
JS APIs, and guessing wrong would fail the whole extension's import. `Gio`
itself has no such version split.

## Status: **completely unverified on real hardware**

Unlike `adapters/gnome` (tested, with rough edges, on one real machine),
nothing in this adapter has run on an actual Cinnamon session yet -- it was
written against Cinnamon's own source and documentation, not against a
running instance. Concretely unverified:

- **Fixed, after three attempts, by matching Cinnamon's own approach
  instead of working around it.** The original `backgroundContainerFor`
  tried to reverse-engineer Cinnamon's own background container via
  `global.get_background_actors()` (confirmed absent on the actual
  `6.6.4` release tag -- `TypeError: ... is not a function`, found by
  diffing `cinnamon-global.c` against a newer checkout), then via
  `global.background_actor`'s parent -- both still left the wallpaper
  covering desktop icons and real windows on real hardware. A second
  redesign created a dedicated `Clutter.Actor` pinned below
  `global.window_group` on `global.stage` -- also still broken, because
  (confirmed by reading Muffin's `src/compositor/compositor.c`,
  `sync_actor_stacking`) `window_group` isn't just "real windows": real
  backgrounds, the desktop-icon layer (`bottom_window_group`, where
  `nemo-desktop` lives), and normal windows are all *children* of
  `window_group`, continuously reclassified and re-lowered into the
  correct relative order **by GObject type** on every stacking change.
  Any actor of a type Muffin doesn't recognize is never touched by that
  re-lowering and drifts to the top over time, regardless of where it was
  first placed.

  `MonitorLayer` now creates a real `Meta.BackgroundActor`
  (`src/meta/meta-background-actor.h` -- confirmed public, GI-exported
  API, not private) and adds it straight to `global.window_group`.
  Because it's an actual recognized type, Muffin's own compositor keeps
  it correctly stacked automatically, the same as its native background
  -- no manual `set_child_below_sibling`, no dedicated group, nothing to
  keep re-asserting. Confirmed live via Looking Glass before rewriting
  `extension.js` around it: a `Meta.BackgroundActor` filled with a solid
  color stayed correctly behind both a newly opened window and the
  desktop icons, with zero stacking code of any kind. Frame content goes
  through `Meta.Background.set_file()` (double-buffered, same as the CSS
  approach this replaced) instead of `St`'s CSS `background-image`.
- The raw `Gio.SocketClient` HTTP client (`httpRequest` in
  `extension.js`) has no test coverage of any kind -- no headless GJS test
  harness exists in this project for extension code.
- `cinnamon-version` in `metadata.json` is set to `["6.0", "6.2", "6.4", "6.6"]`
  -- not independently verified against Cinnamon's own compatibility-check
  logic, which is why `install.sh` also enables with a leading `!` on the
  uuid (see below), telling Cinnamon to skip that check entirely rather
  than trust this list.
- **`cinnamon-extension-tool` is not guaranteed to be installed.** Found
  missing on a real Cinnamon 6.6.4 machine this was tested against (likely
  a Linux-Mint-specific convenience script, not part of every distro's
  `cinnamon` package) -- `install.sh` now talks to the underlying
  `org.cinnamon` GSettings schema directly instead of assuming the tool
  exists, see below.
- **Fixed:** `connectorForMonitorIndex` originally went through
  `global.backend.get_monitor_manager()`, copying adapters/gnome's own
  GNOME-Shell-specific call -- `CinnamonGlobal` never installs a `backend`
  property (confirmed against `src/cinnamon-global.c`) so this threw on
  every monitor, every time, caught silently by `_syncMonitors`'s own
  try/catch, meaning no `MonitorLayer` was ever created and nothing
  displayed -- with no visible error, since Cinnamon extension `log()`
  goes to `~/.cinnamon/glass.log`, not necessarily the systemd journal.
  Found this way on the same real 6.6.4 machine above (wallpaper never
  appeared, `journalctl | grep wp-linux` empty).

  The first replacement, `global.display.get_monitor_name(index)`, turned
  out to be wrong too -- confirmed against real render-server logs on the
  same machine, which showed geometry arriving under
  `"Red Hat, Inc. 15\""` (an EDID vendor/model string, what
  `get_monitor_name` actually returns) while `wp_linux_editor` had
  separately registered the wallpaper project under `"Virtual-1"` (the
  real RandR connector name, resolved via winit's own XRandR query) --
  two different monitor ids for the same screen, so render-server never
  matched them up. Checking the muffin release tag closest to this
  Cinnamon version's `meta-monitor-manager.h` confirmed there's no
  per-monitor object with a `get_connector()`-style accessor anywhere in
  this Muffin generation -- only the reverse
  `meta_monitor_manager_get_monitor_for_connector(connector)`, which
  needs the connector name as input.

  `connectorForMonitor` (renamed from `connectorForMonitorIndex`) now
  shells out to `xrandr --query` and matches its per-output geometry
  against `Main.layoutManager.monitors` by position -- the same source
  winit's own X11 backend already resolves connector names from, so this
  reaches the same ground truth `wp_linux_editor` used, just via the CLI.
  X11-only (this Cinnamon session is X11; `xrandr` doesn't exist under
  Wayland) and depends on the `xrandr` binary being present -- both true
  for any X11 desktop Cinnamon realistically runs on today, but a
  Wayland-session fallback would need a different approach if Cinnamon's
  Wayland support matures enough to matter here.

If something doesn't work: check `journalctl --user -b 0 | grep wp-linux`,
the Extensions page in System Settings (a broken extension shows an error
icon there you can click for the actual JS exception), or run
`cinnamon --replace` from a terminal (safe -- unlike killing an X11
client, a Cinnamon extension throwing during `enable()` gets caught and
disabled by Cinnamon itself, not something that can black-screen or lock
out the session the way this project's earlier X11 adapter attempt did).

## Installing / enabling / disabling

The top-level `install.sh` calls `adapters/cinnamon/install.sh`, which
copies `adapters/cinnamon/extension` to
`~/.local/share/cinnamon/extensions/wp-linux@wplinux.dev/`, then enables
it via `cinnamon-extension-tool --enable` if that's present, falling back
to writing the `org.cinnamon enabled-extensions` GSettings key directly
(`!wp-linux@wplinux.dev`, the leading `!` skipping Cinnamon's own
`cinnamon-version` compatibility check -- see the status section above for
why) if it isn't.

To turn the wallpaper back off: disable the whole extension via the
Extensions page in System Settings, `cinnamon-extension-tool --disable
wp-linux@wplinux.dev` if you have it, or directly:

```sh
gsettings get org.cinnamon enabled-extensions   # find/confirm the exact entry first
gsettings set org.cinnamon enabled-extensions "[...]"   # same list, with our uuid removed
```

## Development

Iterate by copying `adapters/cinnamon/extension/*` straight into
`~/.local/share/cinnamon/extensions/wp-linux@wplinux.dev/`, then either
`cinnamon-extension-tool --disable`/`--enable` if you have it, or:

```sh
gsettings set org.cinnamon enabled-extensions "['!wp-linux@wplinux.dev']"
```

If that doesn't pick up a change, `Alt+F2 r Enter` restarts Cinnamon
in place (safe on X11, unlike GNOME Shell on Wayland which can't do this
at all).

To see what went wrong when something doesn't work, extension errors are
logged with a `wp-linux:` prefix:

```sh
journalctl --user -b 0 --no-pager | grep "wp-linux"
```
