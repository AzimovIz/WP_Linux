# GNOME adapter -- not implemented yet

This is a placeholder. WP Linux's core (`render-server` + `wp_linux_editor`)
is desktop-agnostic -- see the top-level README's "How it's put together"
table -- but showing the rendered frames *as the desktop background* and
forwarding the true global cursor position both need code that talks to a
specific desktop environment. [adapters/kde](../kde) is the reference
implementation of that for KDE Plasma/KWin; this directory will hold the
GNOME equivalent.

## What a GNOME adapter needs to do

Same two jobs `adapters/kde` splits across two KPackages, most likely as a
single GNOME Shell extension (GJS) here since both jobs run inside the same
`gnome-shell` process:

1. **Display**: poll `render-server`'s local HTTP API
   (`GET /meta`, `GET /frame`, `POST /geometry` -- see
   `crates/render-server/src/main.rs`'s module doc comment for the exact
   contract) and paint the frames into a Clutter/St actor placed behind
   desktop icons and windows (e.g. via `global.background_actors` /
   `window_group`), one per monitor, using `Meta.Display`'s monitor
   geometry the same way `adapters/kde/plasma-plugin`'s QML uses
   `Screen.virtualX/Y/width/height`.
2. **Cursor**: forward `global.get_pointer()` to render-server's
   `dev.wplinux.CursorBridge.SetCursorPosition` D-Bus method whenever it
   changes -- see `adapters/kde/kwin-script/package/contents/code/main.js`
   for the KWin equivalent of exactly this.

Project (which wallpaper is assigned to which monitor) is *not* this
adapter's concern -- that's entirely handled by `wp_linux_editor` writing
`monitors.json` and pushing to render-server directly, same as on KDE.

Nothing in `crates/render-server` or `crates/wp_linux_editor` needs to
change to support this -- the HTTP/D-Bus contract they expose is already
desktop-agnostic.
