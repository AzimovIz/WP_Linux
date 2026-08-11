// One instance per connected monitor -- owns the widget that shows that
// monitor's render-server output, and drives the same three jobs
// adapters/kde/plasma-plugin's main.qml does per wallpaper item: push
// this monitor's placement (`/geometry`), poll whether a new frame is
// ready (`/meta`), and fetch+display it (`/frame`) when it is.
//
// Unlike main.qml, there's no separate "xray needs the true global
// cursor" wiring here -- that's cursorForwarder.js, shared by every
// monitor, same as adapters/kde splits it into a separate KWin script.
//
// Frames are written to a temp file and shown via St's CSS
// `background-image`, not `Clutter.Image`/raw `Cogl` pixel upload --
// `new Clutter.Image()` throws "not a constructor" on GNOME Shell 50
// (confirmed on real hardware), i.e. that low-level content API isn't
// stable across Mutter versions. `St.Widget.set_style()` +
// `background-image`/`background-size` is the same mechanism GNOME
// Shell's own theming uses everywhere and has been stable for a very
// long time, at the cost of a disk write per frame (see _fetchFrame).

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const DEFAULT_FPS = 30;
const MIN_POLL_INTERVAL_MS = 8;

/**
 * Where to insert each monitor's render widget. `_backgroundGroup` sits
 * below `global.window_group` (real windows) but above nothing else --
 * i.e. exactly where GNOME Shell's own desktop background lives, which
 * is the same "behind everything, itself opaque" spot
 * adapters/kde/plasma-plugin's WallpaperItem occupies in Plasma. This is
 * private API (no public "put an actor behind the desktop" entry point
 * exists), and unverified on real hardware -- see adapters/gnome's
 * README once written. If frames end up hidden behind GNOME's own
 * background instead of replacing it, this is the line to change.
 */
function backgroundContainer() {
    return Main.layoutManager._backgroundGroup;
}

/** Filesystem-safe version of a connector name for use in a temp filename -- connector names are normally already plain (`DP-1`, `eDP-1`, `Virtual-1`), this is just insurance against a stranger one. */
function sanitizeForFilename(name) {
    return name.replace(/[^A-Za-z0-9_-]/g, '_');
}

export class MonitorLayer {
    /** Read-only: lets overviewBackground.js clone this monitor's rendered output into GNOME's own (otherwise unrelated) Overview workspace-preview background. */
    get actor() {
        return this._actor;
    }

    constructor(connector, client) {
        this._connector = connector;
        this._client = client;

        this._actor = new St.Widget();
        backgroundContainer().add_child(this._actor);

        this._rect = null; // {x, y, width, height}, last pushed to render-server
        this._hasGeometry = false;
        this._lastFrameId = -1;
        this._fps = DEFAULT_FPS;
        this._pollIntervalId = null;
        this._pollInFlight = false;
        this._frameInFlight = false;
        this._cancellable = new Gio.Cancellable();
        this._destroyed = false;

        // Two on-disk slots, ping-ponged so a frame currently referenced
        // by the actor's style is never the one being overwritten --
        // same reasoning as main.qml's imgA/imgB double buffer, just one
        // level down (files instead of Image elements) since St's CSS
        // background-image loader has no equivalent of Qt Quick's
        // asynchronous decode-then-swap.
        const runtimeDir = GLib.get_user_runtime_dir();
        const base = sanitizeForFilename(connector);
        this._frameSlotPaths = [0, 1].map(
            i => GLib.build_filenamev([runtimeDir, `wp-linux-frame-${base}-${i}.tmp`]));
        this._activeSlot = 0;
        this._haveFrame = false;

        this._schedulePoll();
        // Don't wait a full interval for the first /meta -- matches
        // main.qml's Timer `triggeredOnStart: true`.
        this._poll();
    }

    destroy() {
        this._destroyed = true;
        this._cancellable.cancel();
        if (this._pollIntervalId) {
            clearInterval(this._pollIntervalId);
            this._pollIntervalId = null;
        }
        for (const path of this._frameSlotPaths) {
            try {
                Gio.File.new_for_path(path).delete(null);
            } catch (e) {
                // Never written, or already gone -- fine either way.
            }
        }
        this._actor.destroy();
    }

    /** Called by extension.js on startup and whenever monitors-changed fires. `monitor` is one of `Main.layoutManager.monitors`'s entries: `{x, y, width, height, ...}` in the same global/stage coordinate space `global.get_pointer()` reports in -- see cursorForwarder.js. */
    updateGeometry(monitor) {
        const rect = {
            x: monitor.x, y: monitor.y,
            width: monitor.width, height: monitor.height,
        };
        this._actor.set_position(rect.x, rect.y);
        this._actor.set_size(rect.width, rect.height);

        const unchanged = this._rect
            && this._rect.x === rect.x && this._rect.y === rect.y
            && this._rect.width === rect.width && this._rect.height === rect.height;
        const sizeChanged = !this._rect
            || this._rect.width !== rect.width || this._rect.height !== rect.height;
        this._rect = rect;
        if (!unchanged)
            this._pushGeometry();
        // Resize target changed -- reapply so the currently-shown frame
        // (if any) stretches to the new size instead of the old one
        // until the next frame happens to arrive.
        if (sizeChanged && this._haveFrame)
            this._applyStyle(this._frameSlotPaths[this._activeSlot]);
    }

    _pushGeometry() {
        if (!this._rect)
            return;
        const {x, y, width, height} = this._rect;
        this._client.postText(
            `/geometry?monitor=${encodeURIComponent(this._connector)}`,
            `${x},${y},${width},${height}`,
            this._cancellable);
    }

    _applyStyle(path) {
        const uri = GLib.filename_to_uri(path, null);
        // Plain pixel dimensions rather than the `cover`/`100%` CSS
        // keywords -- St's CSS engine is a limited subset of real CSS
        // and length values are the one thing every version of it is
        // certain to support. Stretches rather than crops, same
        // documented tradeoff as before (no built-in "cover" here
        // either); only visibly differs from KDE's PreserveAspectCrop
        // when a project's canvas resolution doesn't match this
        // monitor's.
        const size = this._rect
            ? `${Math.round(this._rect.width)}px ${Math.round(this._rect.height)}px` : 'contain';
        this._actor.set_style(
            `background-image: url("${uri}"); background-size: ${size};`);
    }

    _schedulePoll() {
        const interval = Math.max(
            MIN_POLL_INTERVAL_MS, Math.round(1000 / this._fps));
        if (this._pollIntervalId)
            clearInterval(this._pollIntervalId);
        this._pollIntervalId = setInterval(() => this._poll(), interval);
    }

    async _poll() {
        if (this._destroyed || this._pollInFlight)
            return;
        this._pollInFlight = true;
        try {
            const meta = await this._client.getJson(
                `/meta?monitor=${encodeURIComponent(this._connector)}`,
                this._cancellable);
            if (this._destroyed || !meta)
                return;

            this._hasGeometry = !!meta.has_geometry;
            // render-server may have restarted (losing whatever geometry
            // we last pushed) since we last pushed it -- resend until it
            // sticks, same reasoning as main.qml's own `hasGeometry` doc
            // comment. `/project` needs no equivalent: its assignment is
            // persisted to disk and reloaded by render-server itself.
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
            // Shouldn't happen -- getJson() itself never rejects -- but
            // catch here too rather than let it become a silent unhandled
            // rejection (see _fetchFrame's own doc comment on the same).
            console.error(`wp-linux: _poll for monitor ${this._connector} threw: ${e}`);
        } finally {
            this._pollInFlight = false;
        }
    }

    async _fetchFrame() {
        // Dropping a refresh request while the previous one is still
        // being fetched/written just means the next poll tick offers the
        // (by then newer) frame again -- same tradeoff main.qml's
        // framePoll.refresh() makes, for the same reason: never let
        // fetches pile up faster than they can be handled.
        if (this._frameInFlight)
            return;
        this._frameInFlight = true;
        try {
            const bytes = await this._client.getBytes(
                `/frame?monitor=${encodeURIComponent(this._connector)}`,
                this._cancellable);
            if (this._destroyed || !bytes)
                return;

            const targetSlot = 1 - this._activeSlot;
            const path = this._frameSlotPaths[targetSlot];
            try {
                Gio.File.new_for_path(path).replace_contents(
                    bytes.get_data(), null, false,
                    Gio.FileCreateFlags.REPLACE_DESTINATION, null);
            } catch (e) {
                console.error(`wp-linux: couldn't write frame to ${path}: ${e}`);
                return;
            }

            this._activeSlot = targetSlot;
            this._haveFrame = true;
            this._applyStyle(path);
        } catch (e) {
            // getBytes() itself never rejects -- this is here so a bug
            // anywhere else in this function surfaces as a logged error
            // instead of a bare "Unhandled promise rejection" with no
            // message (this method isn't awaited by its caller, _poll).
            console.error(`wp-linux: _fetchFrame for monitor ${this._connector} threw: ${e}`);
        } finally {
            this._frameInFlight = false;
        }
    }
}
