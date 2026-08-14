// WP Linux Cinnamon extension -- the Cinnamon equivalent of
// adapters/gnome/extension, doing the same two jobs GNOME's does from
// inside its own process (Display + Cursor), talking to the exact same
// desktop-agnostic render-server HTTP/D-Bus contract (see
// crates/render-server/src/main.rs's module doc comment). Nothing in
// render-server or wp_linux_editor changes for this to work.
//
// Everything lives in this one file, unlike adapters/gnome's three-file
// split -- Cinnamon extensions predate GNOME Shell 45's ESM rewrite and
// still use the classic `imports.*`/global-function style (`init`/
// `enable`/`disable`, no `import`/`export`), and multi-file loading
// across Cinnamon versions is not something this project could verify
// without real hardware (see this extension's own README for how little
// of this has been tested). One file keeps the failure surface to "does
// this one file load or not," not "did the right cross-file import
// mechanism resolve on this particular Cinnamon version."
//
// ## Why not the X11-window approach this project used to take
//
// An earlier version of this project's X11 support was a standalone
// process painting directly into an override-redirect X11 window
// pinned to the bottom of the stack (the classic `xwinwrap` technique).
// On Cinnamon that risked fighting Muffin's own compositor for
// stacking -- Muffin (a Mutter fork) renders the desktop background as
// part of its *own* Clutter scene graph (`MetaBackgroundActor`,
// confirmed via Mutter's own source -- background is driven by
// GSettings/textures, never by the classic X11 root-pixmap convention
// `feh`/`hsetroot` rely on), and Mutter has separate, special-cased
// handling for a *fullscreen* override-redirect window (originally for
// Wine/Proton games) that could plausibly out-prioritize a plain
// wallpaper-sized one. This extension sidesteps that entire question by
// inserting its own actor directly into the same trusted scene graph
// Cinnamon already uses to paint its own background -- there's no
// separate X11 client fighting for stacking, because there's no
// separate client at all.
//
// ## Finding "where the background goes"
//
// Cinnamon has no direct equivalent of GNOME Shell's
// `Main.layoutManager._backgroundGroup` (confirmed by reading
// js/ui/layout.js -- no such field exists there). What Cinnamon does
// expose, on versions that have it (see `backgroundContainerFor`'s own
// doc comment -- not present on 6.6.4), is `global.get_background_actors()`,
// a read accessor onto the actors Muffin's own native (C) background code
// already created and positioned per-monitor -- see `backgroundManager.js`
// in Cinnamon's own source, which only ever calls `.show()`/`.hide()` on
// them, never creates or reparents them itself. `backgroundContainerFor`
// below finds the real background actor whose position matches the
// monitor we're placing a layer on, and returns *its parent* -- our own
// actor is inserted into that same parent and then explicitly lowered to
// the bottom of it (`MonitorLayer.updateGeometry`'s own
// `set_child_below_sibling(this._actor, null)` call). An earlier version
// of this file assumed the default "last-added child paints on top"
// ordering alone was enough and skipped that explicit lower -- confirmed
// wrong on real hardware, where it left the wallpaper covering both
// desktop icons (a separate nemo-desktop window) and regular application
// windows, meaning whatever container we land in isn't reliably isolated
// from `global.window_group` on every Muffin version/session. If no
// matching background actor is found (unexpected, but not fatal), this
// falls back to `global.window_group` directly -- the explicit lower
// keeps it below every real window either way.
//
// Frames are written to a temp file and shown via St's CSS
// `background-image`, not `Clutter.Image` -- same reasoning
// adapters/gnome/extension/monitorLayer.js already documents
// (`new Clutter.Image()` throwing "not a constructor" on real GNOME
// Shell hardware); Cinnamon shares the same St/Clutter lineage, so this
// reuses the proven-working approach rather than re-testing the
// low-level path blind.

const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;
const St = imports.gi.St;
const Main = imports.ui.main;

const RENDER_SERVER_HOST = '127.0.0.1';
const RENDER_SERVER_PORT = 47824;

const CURSOR_BUS_NAME = 'dev.wplinux.CursorBridge';
const CURSOR_OBJECT_PATH = '/dev/wplinux/CursorBridge';
const CURSOR_INTERFACE_NAME = 'dev.wplinux.CursorBridge';
// Same ~120Hz cap adapters/kde/kwin-script and
// adapters/gnome/extension/cursorForwarder.js both use.
const CURSOR_POLL_INTERVAL_MS = 8;

const DEFAULT_FPS = 30;
const MIN_POLL_INTERVAL_MS = 8;

// -----------------------------------------------------------------
// Minimal render-server HTTP client, hand-rolled over a raw
// Gio.SocketClient TCP connection instead of libsoup -- Cinnamon can
// run on systems with either libsoup 2 or libsoup 3's GI typelib
// installed (or, in principle, neither), and their JS APIs differ
// enough (`Soup.Session.new()` + `queue_message` vs. `new Soup.Session()`
// + `send_and_read_async`) that picking one blind risks the whole
// extension failing to import at all on a system with the other. Gio
// itself has no such version split -- it's the one dependency every
// Cinnamon system is guaranteed to have, the same reasoning
// crates/render-server's own hand-rolled HTTP server (tiny_http aside)
// already applies to hand-writing its own JSON/query-string handling
// for a protocol it fully controls both ends of.
// HTTP/1.0, not 1.1: 1.0's default is "close after this response," so
// reading the connection to EOF is always correct without parsing
// Content-Length or risking a hang on an unexpected keep-alive.
// -----------------------------------------------------------------

/** Resolves with a Uint8Array of the response body on any 2xx status, or `null` on any failure (connection refused, timeout, non-2xx) -- every caller treats that uniformly as "try again next poll." */
function httpRequest(method, path, body) {
    return new Promise(resolve => {
        let client = new Gio.SocketClient();
        client.connect_to_host_async(RENDER_SERVER_HOST, RENDER_SERVER_PORT, null,
            (source, result) => {
                let connection;
                try {
                    connection = client.connect_to_host_finish(result);
                } catch (e) {
                    resolve(null);
                    return;
                }

                let bodyBytes = body ? new TextEncoder().encode(body) : new Uint8Array(0);
                let head = `${method} ${path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: ${bodyBytes.length}\r\n\r\n`;
                let headBytes = new TextEncoder().encode(head);
                let requestBytes = new Uint8Array(headBytes.length + bodyBytes.length);
                requestBytes.set(headBytes, 0);
                requestBytes.set(bodyBytes, headBytes.length);

                connection.get_output_stream().write_all_async(
                    requestBytes, GLib.PRIORITY_DEFAULT, null,
                    (stream, writeResult) => {
                        try {
                            stream.write_all_finish(writeResult);
                        } catch (e) {
                            connection.close(null);
                            resolve(null);
                            return;
                        }
                        _readAll(connection.get_input_stream(), raw => {
                            connection.close(null);
                            resolve(_parseHttpResponse(raw));
                        });
                    });
            });
    });
}

/** Reads `inputStream` to EOF, calling `callback` with the accumulated bytes (a plain Uint8Array, concatenated once at the end since we don't know the total size up front). */
function _readAll(inputStream, callback) {
    const CHUNK_SIZE = 65536;
    let chunks = [];

    function readNext() {
        inputStream.read_bytes_async(CHUNK_SIZE, GLib.PRIORITY_DEFAULT, null,
            (stream, result) => {
                let bytes;
                try {
                    bytes = stream.read_bytes_finish(result);
                } catch (e) {
                    callback(_concatChunks(chunks));
                    return;
                }
                if (bytes.get_size() === 0) {
                    callback(_concatChunks(chunks));
                    return;
                }
                chunks.push(bytes.get_data());
                readNext();
            });
    }
    readNext();
}

function _concatChunks(chunks) {
    let total = chunks.reduce((sum, c) => sum + c.length, 0);
    let out = new Uint8Array(total);
    let offset = 0;
    for (let c of chunks) {
        out.set(c, offset);
        offset += c.length;
    }
    return out;
}

/** Splits a raw HTTP/1.0 response into status code + body, byte-wise (the body may be arbitrary binary -- a PNG/BMP frame -- so this never decodes the whole response as text, only the header portion up to the blank line). Returns `null` on a malformed response or non-2xx status. */
function _parseHttpResponse(raw) {
    if (!raw)
        return null;

    let splitIndex = -1;
    for (let i = 0; i + 3 < raw.length; i++) {
        if (raw[i] === 13 && raw[i + 1] === 10 && raw[i + 2] === 13 && raw[i + 3] === 10) {
            splitIndex = i;
            break;
        }
    }
    if (splitIndex === -1)
        return null;

    let headerText = new TextDecoder().decode(raw.subarray(0, splitIndex));
    let statusLine = headerText.split('\r\n')[0];
    let statusCode = parseInt(statusLine.split(' ')[1], 10);
    if (!(statusCode >= 200 && statusCode < 300))
        return null;

    return raw.subarray(splitIndex + 4);
}

async function getBytes(path) {
    return httpRequest('GET', path, null);
}

async function getJson(path) {
    let bytes = await getBytes(path);
    if (!bytes)
        return null;
    try {
        return JSON.parse(new TextDecoder().decode(bytes));
    } catch (e) {
        logError(e, `wp-linux: couldn't parse JSON from ${path}`);
        return null;
    }
}

async function postText(path, body) {
    await httpRequest('POST', path, body);
}

// -----------------------------------------------------------------
// Cursor forwarding -- the Cinnamon equivalent of
// adapters/kde/kwin-script and
// adapters/gnome/extension/cursorForwarder.js. No compositor-side
// privilege needed beyond what any Cinnamon extension already has:
// `global.get_pointer()` is the same global-stage-coordinate pointer
// query GNOME Shell's own `global.get_pointer()` provides (both are the
// same Mutter/Muffin-lineage `MetaCursorTracker`-backed call).
// -----------------------------------------------------------------

let _cursorTimeoutId = null;
let _cursorLastX = null;
let _cursorLastY = null;
let _cursorCallCount = 0;

function _cursorTick() {
    let [rawX, rawY] = global.get_pointer();
    let x = Math.round(rawX);
    let y = Math.round(rawY);
    if (x === _cursorLastX && y === _cursorLastY)
        return GLib.SOURCE_CONTINUE;
    _cursorLastX = x;
    _cursorLastY = y;
    _cursorCallCount++;

    try {
        Gio.DBus.session.call(
            CURSOR_BUS_NAME, CURSOR_OBJECT_PATH, CURSOR_INTERFACE_NAME, 'SetCursorPosition',
            new GLib.Variant('(ii)', [x, y]),
            null, Gio.DBusCallFlags.NONE, -1, null,
            (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch (e) {
                    // render-server may not be running (yet) -- best
                    // effort, same as every other adapter's cursor
                    // forwarder.
                }
            });
    } catch (e) {
        logError(e, 'wp-linux cursorbridge: call() threw');
    }
    return GLib.SOURCE_CONTINUE;
}

function startCursorForwarder() {
    if (_cursorTimeoutId)
        return;
    _cursorTimeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, CURSOR_POLL_INTERVAL_MS, _cursorTick);
}

function stopCursorForwarder() {
    if (_cursorTimeoutId) {
        GLib.source_remove(_cursorTimeoutId);
        _cursorTimeoutId = null;
    }
    _cursorLastX = null;
    _cursorLastY = null;
}

// -----------------------------------------------------------------
// Per-monitor display layer -- the Cinnamon equivalent of
// adapters/gnome/extension/monitorLayer.js's MonitorLayer.
// -----------------------------------------------------------------

/** Runs `xrandr --query` and parses out every connected output's connector
 * name and geometry -- e.g. `{ connector: "Virtual-1", x: 0, y: 0, width:
 * 1280, height: 800 }`. Used by `connectorForMonitor` below to resolve
 * the real RandR connector name for a `Main.layoutManager.monitors`
 * entry, matched by position.
 *
 * Why this exists at all: neither `global` nor `Meta.MonitorManager`
 * exposes a connector name anywhere on this Cinnamon/Muffin generation.
 * `global.display.get_monitor_name(index)` (tried first, see this file's
 * git history) turns out to return an EDID vendor/model string like
 * `"Red Hat, Inc. 15\""`, not a connector -- confirmed against real
 * render-server logs, which showed geometry pushed under that EDID
 * string while `wp_linux_editor` (via winit's own XRandR query) had
 * separately registered the wallpaper project under the real connector
 * name `"Virtual-1"`, so the two never matched. And unlike modern
 * GNOME Shell's Mutter, this Muffin fork's `meta-monitor-manager.h` (a
 * frozen, older-generation API -- checked against the muffin release
 * tag closest to this Cinnamon version) has no per-monitor object with a
 * `get_connector()`-style accessor at all -- only the reverse
 * `meta_monitor_manager_get_monitor_for_connector(connector)`, which
 * needs the name as input, not output. `xrandr` is the same source
 * winit's own X11 backend already resolves connector names from, so
 * this reaches the same ground truth `wp_linux_editor` used, just via
 * the CLI instead of a GI binding -- X11-only, matching this Cinnamon
 * session (`xrandr` doesn't exist under Wayland), and it's ubiquitous on
 * any X11 desktop, unlike a new GI typelib dependency would be. */
function _queryXrandrOutputs() {
    let outputs = [];

    let ok, stdout, stderr, status;
    try {
        [ok, stdout, stderr, status] = GLib.spawn_sync(
            null, ['xrandr', '--query'], null, GLib.SpawnFlags.SEARCH_PATH, null);
    } catch (e) {
        log(`wp-linux: couldn't run xrandr -- is it installed? (${e})`);
        return outputs;
    }
    if (!ok || status !== 0) {
        log(`wp-linux: xrandr --query exited with status ${status}: ${new TextDecoder().decode(stderr)}`);
        return outputs;
    }

    // e.g. "Virtual-1 connected primary 1280x800+0+0 (normal left ...) 508mm x 317mm"
    let re = /^(\S+)\s+connected\s+(?:primary\s+)?(\d+)x(\d+)\+(-?\d+)\+(-?\d+)/;
    for (let line of new TextDecoder().decode(stdout).split('\n')) {
        let m = re.exec(line);
        if (m) {
            outputs.push({
                connector: m[1],
                width: parseInt(m[2], 10),
                height: parseInt(m[3], 10),
                x: parseInt(m[4], 10),
                y: parseInt(m[5], 10),
            });
        }
    }
    return outputs;
}

/** Matches a `Main.layoutManager.monitors` entry against `xrandr --query` output by position, returning its real connector name (e.g. "Virtual-1", "eDP-1") -- the same id `wp_linux_editor` already uses (see its own `MonitorInfo` doc comment), so it can be used as render-server's `?monitor=` value with no translation. `outputs` is `_queryXrandrOutputs()`'s result, queried once per `_syncMonitors()` pass rather than once per monitor. */
function connectorForMonitor(monitor, outputs) {
    for (let output of outputs) {
        if (output.x === monitor.x && output.y === monitor.y)
            return output.connector;
    }
    return null;
}

/** Finds the real background actor's parent container, the insertion point our own actor should join as a sibling. See this file's top doc comment for why there's no dedicated `_backgroundGroup` to use directly.
 *
 * `global.get_background_actors()` (plural, one actor per monitor,
 * matched below by position) does not exist on every Cinnamon version --
 * confirmed absent by diffing the actual 6.6.4 release tag's
 * cinnamon-global.c against a newer checkout: 6.6.4 only has the older,
 * singular `global.background_actor` (one actor for the whole X11
 * background, not per-monitor -- `meta_get_x11_background_actor_for_display`).
 * `master` keeps `background_actor` around too (with a runtime deprecation
 * warning pointing at the plural form), so preferring the plural form
 * when present and falling back to the singular one covers both without
 * needing to know which version is actually running.
 *
 * Falls back to `global.window_group` if neither exists -- still below
 * every real window, just without the same guaranteed adjacency to
 * Cinnamon's own background. */
function backgroundContainerFor(monitor) {
    if (typeof global.get_background_actors === 'function') {
        let actors = global.get_background_actors();
        for (let actor of actors) {
            let [x, y] = actor.get_position();
            if (Math.round(x) === monitor.x && Math.round(y) === monitor.y)
                return actor.get_parent();
        }
        if (actors.length > 0)
            return actors[0].get_parent();
    }
    if (global.background_actor)
        return global.background_actor.get_parent();
    return global.window_group;
}

/** Filesystem-safe version of a connector name for use in a temp filename. */
function sanitizeForFilename(name) {
    return name.replace(/[^A-Za-z0-9_-]/g, '_');
}

class MonitorLayer {
    constructor(connector, monitorIndex) {
        this._connector = connector;
        this._monitorIndex = monitorIndex;

        this._actor = new St.Widget();
        this._actor.visible = false;

        this._rect = null;
        this._hasGeometry = false;
        this._lastFrameId = -1;
        this._fps = DEFAULT_FPS;
        this._pollTimeoutId = null;
        this._pollInFlight = false;
        this._frameInFlight = false;
        this._destroyed = false;

        // Two on-disk slots, ping-ponged so a frame currently referenced
        // by the actor's style is never the one being overwritten --
        // same reasoning as adapters/gnome's own double buffer.
        let runtimeDir = GLib.get_user_runtime_dir();
        let base = sanitizeForFilename(connector);
        this._frameSlotPaths = [0, 1].map(i =>
            GLib.build_filenamev([runtimeDir, `wp-linux-cinnamon-frame-${base}-${i}.tmp`]));
        this._activeSlot = 0;
        this._haveFrame = false;

        this._schedulePoll();
        this._poll();
    }

    destroy() {
        this._destroyed = true;
        if (this._pollTimeoutId) {
            GLib.source_remove(this._pollTimeoutId);
            this._pollTimeoutId = null;
        }
        for (let path of this._frameSlotPaths) {
            try {
                Gio.File.new_for_path(path).delete(null);
            } catch (e) {
                // Never written, or already gone -- fine either way.
            }
        }
        this._actor.destroy();
    }

    /** Called on construction and whenever monitors-changed fires. `monitor` is one of `Main.layoutManager.monitors`'s entries: `{x, y, width, height, ...}`. */
    updateGeometry(monitor) {
        let rect = { x: monitor.x, y: monitor.y, width: monitor.width, height: monitor.height };

        // Re-parent every time -- a monitor hot-plug/reconfigure can
        // change which background actor (and therefore which parent) is
        // the right one to sit above, and Clutter actors can only ever
        // have one parent at a time.
        let parent = backgroundContainerFor(rect);
        if (this._actor.get_parent() !== parent) {
            if (this._actor.get_parent())
                this._actor.get_parent().remove_child(this._actor);
            parent.add_child(this._actor);
        }
        // Explicitly forced to the bottom of whatever container we're
        // in, every call -- relying on "the last-added child paints on
        // top" (this file's original assumption, by analogy with
        // adapters/gnome) was confirmed wrong on real hardware: the
        // wallpaper covered both desktop icons (a separate nemo-desktop
        // window) and regular application windows, meaning the container
        // `backgroundContainerFor` resolves to isn't reliably isolated
        // from `global.window_group` on every Muffin version/session --
        // either the same container legitimately holds both, or the
        // `global.window_group` fallback was actually the one taken.
        // Forcing to the bottom is correct either way and removes the
        // dependency on insertion order entirely; harmless/idempotent
        // when called again with nothing else having been added since.
        parent.set_child_below_sibling(this._actor, null);

        this._actor.set_position(rect.x, rect.y);
        this._actor.set_size(rect.width, rect.height);

        let unchanged = this._rect
            && this._rect.x === rect.x && this._rect.y === rect.y
            && this._rect.width === rect.width && this._rect.height === rect.height;
        let sizeChanged = !this._rect
            || this._rect.width !== rect.width || this._rect.height !== rect.height;
        this._rect = rect;
        if (!unchanged)
            this._pushGeometry();
        if (sizeChanged && this._haveFrame)
            this._applyStyle(this._frameSlotPaths[this._activeSlot]);
    }

    async _pushGeometry() {
        if (!this._rect)
            return;
        let { x, y, width, height } = this._rect;
        await postText(`/geometry?monitor=${encodeURIComponent(this._connector)}`,
            `${x},${y},${width},${height}`);
    }

    _applyStyle(path) {
        let uri = GLib.filename_to_uri(path, null);
        let size = this._rect
            ? `${Math.round(this._rect.width)}px ${Math.round(this._rect.height)}px` : 'contain';
        this._actor.set_style(`background-image: url("${uri}"); background-size: ${size};`);
    }

    _schedulePoll() {
        let interval = Math.max(MIN_POLL_INTERVAL_MS, Math.round(1000 / this._fps));
        if (this._pollTimeoutId)
            GLib.source_remove(this._pollTimeoutId);
        this._pollTimeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, interval, () => {
            this._poll();
            return GLib.SOURCE_CONTINUE;
        });
    }

    async _poll() {
        if (this._destroyed || this._pollInFlight)
            return;
        this._pollInFlight = true;
        try {
            let meta = await getJson(`/meta?monitor=${encodeURIComponent(this._connector)}`);
            if (this._destroyed || !meta)
                return;

            this._hasGeometry = !!meta.has_geometry;
            if (!this._hasGeometry)
                this._pushGeometry();

            this._actor.visible = !!meta.ready;

            if (meta.fps && meta.fps !== this._fps) {
                this._fps = meta.fps;
                this._schedulePoll();
            }

            if (meta.ready && meta.frame_id !== this._lastFrameId) {
                this._lastFrameId = meta.frame_id;
                this._fetchFrame();
            }
        } catch (e) {
            logError(e, `wp-linux: _poll for monitor ${this._connector} threw`);
        } finally {
            this._pollInFlight = false;
        }
    }

    async _fetchFrame() {
        if (this._frameInFlight)
            return;
        this._frameInFlight = true;
        try {
            let bytes = await getBytes(`/frame?monitor=${encodeURIComponent(this._connector)}`);
            if (this._destroyed || !bytes)
                return;

            let targetSlot = 1 - this._activeSlot;
            let path = this._frameSlotPaths[targetSlot];
            try {
                Gio.File.new_for_path(path).replace_contents(
                    bytes, null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null);
            } catch (e) {
                logError(e, `wp-linux: couldn't write frame to ${path}`);
                return;
            }

            this._activeSlot = targetSlot;
            this._haveFrame = true;
            this._applyStyle(path);
        } catch (e) {
            logError(e, `wp-linux: _fetchFrame for monitor ${this._connector} threw`);
        } finally {
            this._frameInFlight = false;
        }
    }
}

// -----------------------------------------------------------------
// Extension entry points -- classic Cinnamon extension shape (not
// GNOME Shell 45+'s ESM `export default class`), see this file's top
// doc comment for why.
// -----------------------------------------------------------------

let _layers = null; // connector -> MonitorLayer
let _monitorsChangedId = null;

function _syncMonitors() {
    let seenConnectors = new Set();
    // Queried once per pass, not once per monitor -- monitors-changed
    // fires rarely (startup, hotplug), so one extra `xrandr` process per
    // event is negligible either way, but there's no reason to run it
    // once per connected monitor when one query already lists all of them.
    let xrandrOutputs = _queryXrandrOutputs();

    for (let monitor of Main.layoutManager.monitors) {
        let connector = connectorForMonitor(monitor, xrandrOutputs);
        if (!connector) {
            log(`wp-linux: no xrandr output found for monitor ${monitor.index} at (${monitor.x},${monitor.y}) -- skipping it`);
            continue;
        }

        seenConnectors.add(connector);
        let layer = _layers.get(connector);
        if (!layer) {
            layer = new MonitorLayer(connector, monitor.index);
            _layers.set(connector, layer);
        }
        layer.updateGeometry(monitor);
    }

    for (let [connector, layer] of _layers) {
        if (!seenConnectors.has(connector)) {
            layer.destroy();
            _layers.delete(connector);
        }
    }
}

function init(extensionMeta) {
    // Nothing to do at load time -- see enable().
}

function enable() {
    _layers = new Map();
    startCursorForwarder();

    _monitorsChangedId = Main.layoutManager.connect('monitors-changed', () => _syncMonitors());
    _syncMonitors();
}

function disable() {
    if (_monitorsChangedId) {
        Main.layoutManager.disconnect(_monitorsChangedId);
        _monitorsChangedId = null;
    }

    if (_layers) {
        for (let layer of _layers.values())
            layer.destroy();
        _layers = null;
    }

    stopCursorForwarder();
}
