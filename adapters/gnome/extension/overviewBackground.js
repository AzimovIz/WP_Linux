// Clones each monitor's wallpaper actor into GNOME's own Overview
// zoomed-out workspace preview, which otherwise has no idea render-server
// is drawing anything and just shows the desktop's real GNOME background
// (org.gnome.desktop.background) instead -- confirmed against
// gnome-shell's own source (js/ui/workspace.js), not guessed:
// `WorkspaceBackground` builds its own independent
// `Background.BackgroundManager` per workspace-preview, entirely
// unconnected to `Main.layoutManager._backgroundGroup` (where
// monitorLayer.js's actors live).
//
// Monkeypatches `WorkspaceBackground.prototype._init` to append a
// `Clutter.Clone` of the matching monitor's actor on top of the real
// background. This is standard but inherently more fragile than the
// rest of this adapter -- it depends on gnome-shell's *private* Overview
// implementation (`js/ui/workspace.js`), which gets rewritten far more
// often than the stable, decades-old `_backgroundGroup` field
// monitorLayer.js relies on. Purely cosmetic (the real desktop wallpaper
// in monitorLayer.js doesn't depend on any of this), so every failure
// path here is caught and logged rather than allowed to break anything
// else -- including `WorkspaceBackground` no longer existing/exporting
// the same way on some future GNOME version, which is why the import
// below is dynamic instead of a static one at module load time.

import Clutter from 'gi://Clutter';

export class OverviewBackgroundPatcher {
    /** `getLayerForMonitorIndex(index)` resolves a `Main.layoutManager.monitors` index to that monitor's `MonitorLayer` (or `undefined`) -- same lookup extension.js already does for `monitors-changed`. */
    constructor(getLayerForMonitorIndex) {
        this._getLayerForMonitorIndex = getLayerForMonitorIndex;
        this._workspaceBackgroundClass = null;
        this._originalInit = null;
        this._clones = new Set();
        this._enabled = false;
    }

    enable() {
        this._enabled = true;
        import('resource:///org/gnome/shell/ui/workspace.js')
            .then(({WorkspaceBackground}) => {
                // enable() raced with a disable() before the import
                // resolved -- don't patch something we're about to be
                // asked to leave alone.
                if (!this._enabled)
                    return;

                this._workspaceBackgroundClass = WorkspaceBackground;
                this._originalInit = WorkspaceBackground.prototype._init;
                const self = this;
                WorkspaceBackground.prototype._init = function (monitorIndex, stateAdjustment) {
                    self._originalInit.call(this, monitorIndex, stateAdjustment);
                    self._attachClone(this, monitorIndex);
                };
            })
            .catch(e => {
                console.error(
                    'wp-linux: Overview background clone unavailable, skipping it -- ' +
                    `the real desktop wallpaper is unaffected: ${e}`);
            });
    }

    disable() {
        this._enabled = false;
        if (this._workspaceBackgroundClass && this._originalInit)
            this._workspaceBackgroundClass.prototype._init = this._originalInit;
        this._workspaceBackgroundClass = null;
        this._originalInit = null;

        for (const clone of this._clones)
            clone.destroy();
        this._clones.clear();
    }

    _attachClone(workspaceBackground, monitorIndex) {
        try {
            const layer = this._getLayerForMonitorIndex(monitorIndex);
            // No wallpaper assigned to this monitor, or this instance
            // didn't build the container we expect (a future gnome-shell
            // version renamed/removed it) -- leave the real background
            // alone rather than risk adding a clone with nothing sane to
            // attach it to.
            if (!layer || !workspaceBackground._backgroundGroup)
                return;

            const clone = new Clutter.Clone({
                source: layer.actor,
                x_expand: true,
                y_expand: true,
            });
            // Added last -- and thus painted on top -- of the real
            // Background.BackgroundManager actor _init already created,
            // fully covering it rather than needing to hide it.
            workspaceBackground._backgroundGroup.add_child(clone);
            this._clones.add(clone);
            clone.connect('destroy', () => this._clones.delete(clone));
        } catch (e) {
            console.error(`wp-linux: couldn't attach Overview background clone: ${e}`);
        }
    }
}
