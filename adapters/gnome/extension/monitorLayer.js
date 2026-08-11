// One instance per connected monitor -- owns the Clutter actor that
// shows that monitor's render-server output, and drives the same three
// jobs adapters/kde/plasma-plugin's main.qml does per wallpaper item:
// push this monitor's placement (`/geometry`), poll whether a new frame
// is ready (`/meta`), and fetch+display it (`/frame`) when it is.
//
// Unlike main.qml, there's no separate "xray needs the true global
// cursor" wiring here -- that's cursorForwarder.js, shared by every
// monitor, same as adapters/kde splits it into a separate KWin script.

import Clutter from 'gi://Clutter';
import Cogl from 'gi://Cogl';
import Gio from 'gi://Gio';
import GdkPixbuf from 'gi://GdkPixbuf';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const DEFAULT_FPS = 30;
const MIN_POLL_INTERVAL_MS = 8;

/**
 * Where to insert each monitor's render actor. `_backgroundGroup` sits
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

export class MonitorLayer {
    constructor(connector, client) {
        this._connector = connector;
        this._client = client;

        // RESIZE_FILL stretches to the actor's exact allocated size rather
        // than cropping like main.qml's Image.PreserveAspectCrop does --
        // Clutter has no built-in "cover" gravity. Only visibly differs
        // from KDE when a project's canvas resolution doesn't match this
        // monitor's, which normally doesn't happen (projects are authored
        // for a specific monitor's resolution).
        this._actor = new Clutter.Actor({
            content_gravity: Clutter.ContentGravity.RESIZE_FILL,
        });
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
        this._rect = rect;
        if (!unchanged)
            this._pushGeometry();
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
        } finally {
            this._pollInFlight = false;
        }
    }

    async _fetchFrame() {
        // Dropping a refresh request while the previous one is still
        // being fetched/decoded just means the next poll tick offers the
        // (by then newer) frame again -- same tradeoff main.qml's
        // framePoll.refresh() makes, for the same reason: never let
        // fetches pile up faster than they can be decoded.
        if (this._frameInFlight)
            return;
        this._frameInFlight = true;
        try {
            const bytes = await this._client.getBytes(
                `/frame?monitor=${encodeURIComponent(this._connector)}`,
                this._cancellable);
            if (this._destroyed || !bytes)
                return;

            const image = imageFromEncodedBytes(bytes);
            if (image)
                this._actor.set_content(image);
        } finally {
            this._frameInFlight = false;
        }
    }
}

/** Decodes a PNG (static project) or BMP (dynamic project) frame -- see main.rs's `/frame` doc comment for which -- into a `Clutter.Image` ready to hand to `Clutter.Actor.set_content()`. GdkPixbuf sniffs the format itself, so the caller never needs to know which one it got. */
function imageFromEncodedBytes(bytes) {
    let pixbuf;
    try {
        const loader = new GdkPixbuf.PixbufLoader();
        loader.write_bytes(bytes);
        loader.close();
        pixbuf = loader.get_pixbuf();
    } catch (e) {
        return null;
    }
    if (!pixbuf)
        return null;

    const image = new Clutter.Image();
    const format = pixbuf.get_has_alpha()
        ? Cogl.PixelFormat.RGBA_8888 : Cogl.PixelFormat.RGB_888;
    try {
        image.set_bytes(
            pixbuf.read_pixel_bytes(), format,
            pixbuf.get_width(), pixbuf.get_height(), pixbuf.get_rowstride());
    } catch (e) {
        return null;
    }
    return image;
}
