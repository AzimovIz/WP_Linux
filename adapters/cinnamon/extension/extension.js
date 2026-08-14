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
// Three earlier versions of this file fought this problem at the wrong
// level entirely -- trying to place a generic `St.Widget`/`Clutter.Actor`
// somewhere in the scene graph and keep it correctly stacked by hand
// (`global.get_background_actors()`'s parent, then a dedicated group
// pinned below `global.window_group` via `set_child_below_sibling`).
// Both were confirmed broken on real hardware. The second one's own
// diagnosis turned out to be incomplete: reading Muffin's
// `src/compositor/compositor.c` (`sync_actor_stacking`) shows
// `window_group` isn't just "real windows" -- backgrounds, the
// `bottom_window_group` desktop-icon layer (nemo-desktop), and regular
// windows are *all* children of `window_group`, classified purely by
// **GObject type** (`META_IS_BACKGROUND_ACTOR`, `META_IS_WINDOW_ACTOR`,
// etc.) and re-lowered into the right relative order on every stacking
// change. Any actor of a type Muffin doesn't recognize is never touched
// by that re-lowering, which means it drifts to the *top* over time --
// exactly the "covers everything" symptom, regardless of where it was
// first inserted or how carefully its initial position was set by hand.
//
// The actual fix is to stop being a foreign object: `MonitorLayer` now
// creates a real `Meta.BackgroundActor` (`src/meta/meta-background-actor.h`
// -- confirmed real, public, GI-exported GObject API, not private) and
// adds it straight to `global.window_group`. Because it's an actual
// `MetaBackgroundActor`, Muffin's own `sync_actor_stacking` recognizes
// and re-lowers it correctly on every single stacking change, the same
// as its own native background -- no manual `set_child_below_sibling`,
// no dedicated group, no reverse-engineering anything. Confirmed live via
// Looking Glass on real hardware: a `Meta.BackgroundActor` filled with a
// solid color stayed correctly behind both a newly opened window and the
// desktop icons, with zero stacking code of our own.
//
// Frame content goes through `Meta.Background.set_file()` (a real image
// file path, double-buffered the same way the old CSS approach was) --
// not `St`'s CSS `background-image` this file used before, and not
// `Clutter.Image` either (`new Clutter.Image()` throws "not a
// constructor" on real GNOME Shell hardware per
// adapters/gnome/extension/monitorLayer.js's own doc comment). This is
// the same native background pipeline Cinnamon's own wallpaper uses, so
// it needs no workaround.

const CDesktopEnums = imports.gi.CDesktopEnums;
const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;
const Meta = imports.gi.Meta;
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

const HTTP_TIMEOUT_MS = 3000;

/** Resolves with a Uint8Array of the response body on any 2xx status, or `null` on any failure (connection refused, timeout, non-2xx) -- every caller treats that uniformly as "try again next poll."
 *
 * Guarded by a hard `HTTP_TIMEOUT_MS` timeout via `Gio.Cancellable` --
 * confirmed missing on real hardware caused a real hang: none of
 * `connect_to_host_async`/`write_all_async`/`read_bytes_async` below had
 * a cancellable or any other time bound, so a single stalled connection
 * (render-server accepting but never responding/closing, a dropped
 * packet, anything) left this Promise permanently unresolved --
 * `MonitorLayer._pollInFlight`/`_frameInFlight` stayed `true` forever
 * once that happened, permanently freezing that monitor's frame updates
 * (and, since new frames stopped being fetched at all, cursor-reactive
 * shaders too) while the rest of the desktop kept working normally, since
 * nothing about this blocks the main loop itself. */
function httpRequest(method, path, body) {
    return new Promise(resolve => {
        let settled = false;
        let cancellable = new Gio.Cancellable();
        let timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, HTTP_TIMEOUT_MS, () => {
            timeoutId = null;
            cancellable.cancel();
            return GLib.SOURCE_REMOVE;
        });

        function finish(result) {
            if (settled)
                return;
            settled = true;
            if (timeoutId) {
                GLib.source_remove(timeoutId);
                timeoutId = null;
            }
            resolve(result);
        }

        let client = new Gio.SocketClient();
        client.connect_to_host_async(RENDER_SERVER_HOST, RENDER_SERVER_PORT, cancellable,
            (source, result) => {
                let connection;
                try {
                    connection = client.connect_to_host_finish(result);
                } catch (e) {
                    finish(null);
                    return;
                }

                let bodyBytes = body ? new TextEncoder().encode(body) : new Uint8Array(0);
                let head = `${method} ${path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: ${bodyBytes.length}\r\n\r\n`;
                let headBytes = new TextEncoder().encode(head);
                let requestBytes = new Uint8Array(headBytes.length + bodyBytes.length);
                requestBytes.set(headBytes, 0);
                requestBytes.set(bodyBytes, headBytes.length);

                connection.get_output_stream().write_all_async(
                    requestBytes, GLib.PRIORITY_DEFAULT, cancellable,
                    (stream, writeResult) => {
                        try {
                            stream.write_all_finish(writeResult);
                        } catch (e) {
                            connection.close(null);
                            finish(null);
                            return;
                        }
                        _readAll(connection.get_input_stream(), cancellable, raw => {
                            connection.close(null);
                            finish(_parseHttpResponse(raw));
                        });
                    });
            });
    });
}

/** Reads `inputStream` to EOF, calling `callback` with the accumulated bytes (a plain Uint8Array, concatenated once at the end since we don't know the total size up front). `cancellable` is `httpRequest`'s per-call timeout guard -- a cancellation mid-read lands in the same `catch` as a real error and returns whatever was read so far, same as any other dropped connection; `_parseHttpResponse`'s status-code check and each caller's own validation (`JSON.parse`, frame decoding) already treat a malformed/truncated body as a failed attempt. */
function _readAll(inputStream, cancellable, callback) {
    const CHUNK_SIZE = 65536;
    let chunks = [];

    function readNext() {
        inputStream.read_bytes_async(CHUNK_SIZE, GLib.PRIORITY_DEFAULT, cancellable,
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

/** Filesystem-safe version of a connector name for use in a temp filename. */
function sanitizeForFilename(name) {
    return name.replace(/[^A-Za-z0-9_-]/g, '_');
}

class MonitorLayer {
    constructor(connector, monitorIndex) {
        this._connector = connector;
        this._monitorIndex = monitorIndex;

        // set_file() is called on this same, reused Background object
        // every new frame (see _applyFrame) -- MetaBackgroundActor is the
        // *actor* (added to the scene graph, its type is what Muffin's
        // own sync_actor_stacking recognizes), MetaBackground is the
        // *content* it displays, a separate object by this API's design.
        this._background = Meta.Background.new(global.display);
        this._actor = Meta.BackgroundActor.new(global.display, monitorIndex);
        this._actor.set_background(this._background);
        this._actor.visible = false;
        // Confirmed live via Looking Glass: this alone is enough for
        // Muffin to size and position it to the given monitor and keep
        // it correctly stacked below every window and desktop icon on
        // every restack -- no add_child/set_position/set_size/stacking
        // call of our own needed, unlike the St.Widget this replaced.
        global.window_group.add_child(this._actor);

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
        this._background = null;
    }

    /** Called on construction and whenever monitors-changed fires. `monitor` is one of `Main.layoutManager.monitors`'s entries: `{x, y, width, height, ...}`. Unlike the St.Widget this replaced, the actor's own on-screen geometry is Muffin's job (it tracks `monitor.index` itself, re-asserted below in case hotplug ever shuffles indices) -- `_rect`/`_pushGeometry` are only about telling render-server what size frame to render, a separate concern. */
    updateGeometry(monitor) {
        this._actor.set_monitor(monitor.index);

        let rect = { x: monitor.x, y: monitor.y, width: monitor.width, height: monitor.height };

        let unchanged = this._rect
            && this._rect.x === rect.x && this._rect.y === rect.y
            && this._rect.width === rect.width && this._rect.height === rect.height;
        let sizeChanged = !this._rect
            || this._rect.width !== rect.width || this._rect.height !== rect.height;
        this._rect = rect;
        if (!unchanged)
            this._pushGeometry();
        if (sizeChanged && this._haveFrame)
            this._applyFrame(this._frameSlotPaths[this._activeSlot]);
    }

    async _pushGeometry() {
        if (!this._rect)
            return;
        let { x, y, width, height } = this._rect;
        await postText(`/geometry?monitor=${encodeURIComponent(this._connector)}`,
            `${x},${y},${width},${height}`);
    }

    /** Loads `path` into the same, reused `Meta.Background` this actor already displays -- native texture pipeline, no CSS/St theming involved. `STRETCHED`: fill the monitor exactly, ignoring aspect ratio -- same tradeoff the old CSS `background-size` (plain pixel dimensions, not `cover`/`contain`) already made, kept for parity now that render-server's frame is always exactly the pushed geometry's size anyway.
     *
     * `Meta.BackgroundImageCache` (`src/compositor/meta-background-image.c`)
     * caches decoded textures keyed by `GFile` path equality, not content
     * or mtime -- confirmed on real hardware: the first frame displayed
     * correctly, then neither animation nor switching to a different
     * wallpaper project ever changed anything, because both slots'
     * *paths* had already been seen once each and kept getting served
     * their original cached texture. `purge()` (public API, confirmed via
     * `src/meta/meta-background-image.h`) evicts this exact path from
     * that cache immediately before `set_file()`, forcing a real re-decode
     * every time -- needed on every call, not just the first, since these
     * two paths get reused for every subsequent frame by design (the
     * double-buffer in the constructor). */
    _applyFrame(path) {
        let file = Gio.File.new_for_path(path);
        Meta.BackgroundImageCache.get_default().purge(file);
        this._background.set_file(file, CDesktopEnums.BackgroundStyle.STRETCHED);
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
            this._applyFrame(path);
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
