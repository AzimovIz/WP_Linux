// GNOME Shell equivalent of adapters/kde's two KPackages combined into
// one extension, since both jobs (drawing frames, forwarding the
// cursor) run inside the same gnome-shell process here -- see
// adapters/gnome/README.md for the split rationale. Nothing in
// crates/render-server or crates/wp_linux_editor changes for this: the
// HTTP/D-Bus contract they expose is already desktop-agnostic.

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {CursorForwarder} from './cursorForwarder.js';
import {MonitorLayer} from './monitorLayer.js';
import {OverviewBackgroundPatcher} from './overviewBackground.js';
import {RenderServerClient} from './renderServerClient.js';

/**
 * Resolves the connector name (e.g. "DP-1", "eDP-1") for a
 * `Main.layoutManager.monitors` index, the same `wl_output` protocol
 * name `wp_linux_editor` uses as a monitor id (see its own
 * `MonitorInfo` doc comment) and Plasma's `Screen.name` already uses on
 * KDE -- so it can be used as render-server's `?monitor=` value with no
 * translation, same as on KDE. `Meta.Monitor` (unlike
 * `Main.layoutManager.monitors`'s plain geometry entries) is the only
 * place this name is exposed, hence going through the monitor manager
 * instead of using `monitors[i]` directly.
 */
function connectorForMonitorIndex(index) {
    const monitorManager = global.backend.get_monitor_manager();
    for (const monitor of monitorManager.get_monitors()) {
        const connector = monitor.get_connector();
        if (connector && monitorManager.get_monitor_for_connector(connector) === index)
            return connector;
    }
    return null;
}

export default class WpLinuxExtension extends Extension {
    enable() {
        this._client = new RenderServerClient();
        this._layers = new Map(); // connector -> MonitorLayer
        this._cursorForwarder = new CursorForwarder();
        this._cursorForwarder.start();

        this._monitorsChangedId = Main.layoutManager.connect(
            'monitors-changed', () => this._syncMonitors());
        this._syncMonitors();

        // Cosmetic-only, best-effort (see overviewBackground.js's own
        // doc comment) -- started last and stopped first so it can never
        // end up outliving the state (_layers) it reads from.
        this._overviewPatcher = new OverviewBackgroundPatcher(
            index => this._layerForMonitorIndex(index));
        this._overviewPatcher.enable();
    }

    disable() {
        this._overviewPatcher.disable();
        this._overviewPatcher = null;

        if (this._monitorsChangedId) {
            Main.layoutManager.disconnect(this._monitorsChangedId);
            this._monitorsChangedId = null;
        }

        for (const layer of this._layers.values())
            layer.destroy();
        this._layers = null;

        this._cursorForwarder.stop();
        this._cursorForwarder = null;

        this._client.destroy();
        this._client = null;
    }

    _layerForMonitorIndex(index) {
        let connector;
        try {
            connector = connectorForMonitorIndex(index);
        } catch (e) {
            return undefined;
        }
        return connector ? this._layers.get(connector) : undefined;
    }

    _syncMonitors() {
        const seenConnectors = new Set();

        for (const monitor of Main.layoutManager.monitors) {
            let connector;
            try {
                connector = connectorForMonitorIndex(monitor.index);
            } catch (e) {
                console.warn(`wp-linux: couldn't resolve connector for monitor ${monitor.index}: ${e}`);
                connector = null;
            }
            // No usable id for this monitor -- skip it rather than guess
            // one that won't match what wp_linux_editor pushes `/project`
            // assignments under; it just won't show a wallpaper.
            if (!connector) {
                console.warn(`wp-linux: no connector found for monitor ${monitor.index} -- skipping it`);
                continue;
            }

            seenConnectors.add(connector);
            let layer = this._layers.get(connector);
            if (!layer) {
                layer = new MonitorLayer(connector, this._client);
                this._layers.set(connector, layer);
            }
            layer.updateGeometry(monitor);
        }

        for (const [connector, layer] of this._layers) {
            if (!seenConnectors.has(connector)) {
                layer.destroy();
                this._layers.delete(connector);
            }
        }
    }
}
