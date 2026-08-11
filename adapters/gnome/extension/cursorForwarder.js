// Forwards the true global cursor position to render-server's
// `dev.wplinux.CursorBridge` D-Bus service, GNOME equivalent of
// adapters/kde/kwin-script/package/contents/code/main.js. GNOME Shell
// has no exposed "pointer moved" signal as convenient as KWin's
// `workspace.cursorPosChanged` (`global.stage`'s `motion-event` only
// fires while the pointer is over an actor that's requesting motion
// events, which desktop actors below click-through UI aren't
// guaranteed to be) -- polling `global.get_pointer()` sidesteps that
// entirely and is what the throttle below is for, same ~120Hz cap the
// KWin script uses.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

const BUS_NAME = 'dev.wplinux.CursorBridge';
const OBJECT_PATH = '/dev/wplinux/CursorBridge';
const INTERFACE_NAME = 'dev.wplinux.CursorBridge';
const POLL_INTERVAL_MS = 8; // light throttle, ~120Hz cap -- matches the KWin script

export class CursorForwarder {
    constructor() {
        this._connection = null;
        this._intervalId = null;
        this._lastX = null;
        this._lastY = null;
        this._callCount = 0;
    }

    start() {
        this._connection = Gio.DBus.session;
        this._intervalId = setInterval(() => this._tick(), POLL_INTERVAL_MS);
    }

    stop() {
        if (this._intervalId) {
            clearInterval(this._intervalId);
            this._intervalId = null;
        }
        this._connection = null;
    }

    _tick() {
        // Integer pixels in practice (same as the KWin script's pos.x/y),
        // but rounded defensively since the D-Bus signature below is
        // strictly "(ii)" and a future fractional-scaling change to
        // global.get_pointer() returning floats would otherwise throw.
        const [rawX, rawY] = global.get_pointer();
        const x = Math.round(rawX);
        const y = Math.round(rawY);
        if (x === this._lastX && y === this._lastY)
            return;
        this._lastX = x;
        this._lastY = y;
        this._callCount++;

        // Don't flood the journal; one line every ~30 calls is enough to
        // confirm the signal is actually firing (same cadence the KWin
        // script's own debug logging uses).
        if (this._callCount % 30 === 0)
            console.debug(`wp-linux cursorbridge: sending ${x},${y} (call #${this._callCount})`);

        this._connection.call(
            BUS_NAME, OBJECT_PATH, INTERFACE_NAME, 'SetCursorPosition',
            new GLib.Variant('(ii)', [x, y]),
            null, Gio.DBusCallFlags.NONE, -1, null,
            (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch (e) {
                    // render-server may not be running (yet) -- best
                    // effort, exactly like the KWin script's own
                    // callDBus try/catch.
                }
            });
    }
}
