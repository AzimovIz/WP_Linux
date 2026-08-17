// Reports, per monitor, whether some window currently covers that
// monitor's whole visible desktop -- either genuinely fullscreen (a
// video, a game) or just maximized to fill the screen (a document editor,
// a browser). Either way the wallpaper sitting below it in the stacking
// order can't be seen, so render-server has no reason to keep rendering
// it -- see its `SetMonitorOccluded` D-Bus handler and `OcclusionGate`,
// and adapters/kde's kwin-script for the KDE equivalent of this file.
//
// The two cases collapse into one geometry test: does this window's frame
// rect cover the work area Mutter would maximize a window to on this
// monitor (`Main.layoutManager.getWorkAreaForMonitor`)? A genuinely
// fullscreen window's frame rect always meets or exceeds that (it covers
// the *full* monitor, panels and all), so there's no separate fullscreen
// check needed.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS_NAME = 'dev.wplinux.CursorBridge';
const OBJECT_PATH = '/dev/wplinux/CursorBridge';
const INTERFACE_NAME = 'dev.wplinux.CursorBridge';

// Per-window signals whose state can flip whether a window covers its
// monitor's work area. Connected defensively (see _trackWindow) since
// some of these are less universally documented than others -- a name
// missing on a given Mutter version should mean slightly less eager
// recomputation, not a broken extension.
const WINDOW_TRACK_SIGNALS = [
    'size-changed', 'position-changed', 'workspace-changed',
    'notify::minimized', 'notify::maximized-horizontally', 'notify::maximized-vertically',
];

function rectCovers(outer, inner) {
    return outer.x <= inner.x && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height;
}

export class OcclusionWatcher {
    /**
     * @param {(monitorIndex: number) => string | null} connectorForMonitorIndex
     *   Same helper extension.js already uses to map a
     *   `Main.layoutManager.monitors` index to the wl_output connector
     *   name render-server expects as `?monitor=`.
     */
    constructor(connectorForMonitorIndex) {
        this._connectorForMonitorIndex = connectorForMonitorIndex;
        this._connection = null;
        this._lastSent = new Map(); // connector -> bool last sent
        this._trackedWindows = new Map(); // Meta.Window -> [signal ids]
        this._displaySignalIds = [];
    }

    start() {
        this._connection = Gio.DBus.session;

        for (const window of global.display.list_all_windows())
            this._trackWindow(window);

        this._connectDisplay('window-created', (_display, window) => {
            this._trackWindow(window);
            this._recompute();
        });
        this._connectDisplay('window-entered-monitor', () => this._recompute());
        this._connectDisplay('window-left-monitor', () => this._recompute());
        this._connectDisplay('workareas-changed', () => this._recompute());

        this._recompute();
    }

    stop() {
        for (const window of Array.from(this._trackedWindows.keys()))
            this._untrackWindow(window);
        for (const id of this._displaySignalIds) {
            try {
                global.display.disconnect(id);
            } catch (e) {
                // Fine -- shell is tearing down anyway.
            }
        }
        this._displaySignalIds = [];
        this._connection = null;
    }

    /** Called by extension.js's own 'monitors-changed' handler too -- a monitor being added/removed/resized can change which one a given window's rect happens to cover. */
    recompute() {
        this._recompute();
    }

    _connectDisplay(signal, handler) {
        try {
            this._displaySignalIds.push(global.display.connect(signal, handler));
        } catch (e) {
            console.warn(`wp-linux occlusion: couldn't connect display '${signal}': ${e}`);
        }
    }

    _trackWindow(window) {
        if (this._trackedWindows.has(window))
            return;

        const ids = [];
        const recompute = () => this._recompute();
        for (const signal of WINDOW_TRACK_SIGNALS) {
            try {
                ids.push(window.connect(signal, recompute));
            } catch (e) {
                // Best effort -- see WINDOW_TRACK_SIGNALS' own doc comment.
            }
        }
        try {
            ids.push(window.connect('unmanaging', () => this._untrackWindow(window)));
        } catch (e) {
            // Worst case this window's entry just outlives it until the
            // next full _recompute() pass naturally skips it via
            // list_all_windows() no longer returning it.
        }
        this._trackedWindows.set(window, ids);
    }

    _untrackWindow(window) {
        const ids = this._trackedWindows.get(window);
        if (!ids)
            return;
        for (const id of ids) {
            try {
                window.disconnect(id);
            } catch (e) {
                // Already gone with the window itself -- fine.
            }
        }
        this._trackedWindows.delete(window);
        this._recompute();
    }

    _recompute() {
        const activeWorkspace = global.workspace_manager.get_active_workspace();
        const windows = global.display.list_all_windows();

        for (const monitor of Main.layoutManager.monitors) {
            let connector;
            try {
                connector = this._connectorForMonitorIndex(monitor.index);
            } catch (e) {
                continue;
            }
            if (!connector)
                continue;

            const workArea = Main.layoutManager.getWorkAreaForMonitor(monitor.index);
            const covered = windows.some(window =>
                !window.minimized
                && window.get_monitor() === monitor.index
                && (window.is_on_all_workspaces() || window.get_workspace() === activeWorkspace)
                && rectCovers(window.get_frame_rect(), workArea));

            if (this._lastSent.get(connector) !== covered) {
                this._lastSent.set(connector, covered);
                this._send(connector, covered);
            }
        }
    }

    _send(connector, occluded) {
        try {
            this._connection.call(
                BUS_NAME, OBJECT_PATH, INTERFACE_NAME, 'SetMonitorOccluded',
                new GLib.Variant('(sb)', [connector, occluded]),
                null, Gio.DBusCallFlags.NONE, -1, null,
                (connection, result) => {
                    try {
                        connection.call_finish(result);
                    } catch (e) {
                        // render-server may not be running (yet) -- best
                        // effort, exactly like cursorForwarder.js.
                    }
                });
        } catch (e) {
            console.error(`wp-linux occlusion: call() threw: ${e}`);
        }
    }
}
