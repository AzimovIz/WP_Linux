// Forwards the global cursor position to the wp-linux cursor-bridge
// process over D-Bus every time KWin reports it changed. KWin always
// knows the true pointer position, regardless of which QML item inside
// plasmashell (e.g. Folder View's icon layer) currently "owns" hover --
// that's the whole reason this script exists instead of reading the
// cursor from within the wallpaper's own QML.

print("wplinux cursorbridge: script loaded");

let lastSent = 0;
let callCount = 0;
const MIN_INTERVAL_MS = 8; // light throttle, ~120Hz cap

// Reused across every call instead of a fresh closure per cursor move
// (this fires up to ~120Hz): if cursor-bridge is down and replies never
// arrive, a per-call closure would mean a new function object pinned
// alive by each still-pending call, piling up for as long as the service
// stays unreachable. One shared function has no such per-call cost.
function onCursorBridgeReply(...args) {
    // zbus methods with no return value reply with zero arguments; if
    // this never fires at all, the call itself is failing (wrong bus
    // name/path/interface, or the service isn't running).
    if (callCount % 30 === 0) {
        print("wplinux cursorbridge: callDBus reply " + JSON.stringify(args));
    }
}

function sendCursorPosition() {
    const now = Date.now();
    if (now - lastSent < MIN_INTERVAL_MS) {
        return;
    }
    lastSent = now;

    const pos = workspace.cursorPos;
    callCount++;
    if (callCount % 30 === 0) {
        // Don't flood the journal; one line every ~30 calls is enough to
        // confirm the signal is actually firing.
        print("wplinux cursorbridge: sending " + pos.x + "," + pos.y + " (call #" + callCount + ")");
    }

    try {
        callDBus(
            "dev.wplinux.CursorBridge",
            "/dev/wplinux/CursorBridge",
            "dev.wplinux.CursorBridge",
            "SetCursorPosition",
            pos.x,
            pos.y,
            onCursorBridgeReply
        );
    } catch (e) {
        print("wplinux cursorbridge: callDBus threw: " + e);
    }
}

print("wplinux cursorbridge: connecting to cursorPosChanged, initial pos = " + JSON.stringify(workspace.cursorPos));
workspace.cursorPosChanged.connect(sendCursorPosition);

// Reports, per output, whether some window currently covers that output's
// whole visible desktop -- either genuinely fullscreen (a video, a game)
// or just maximized to fill the screen (a document editor, a browser).
// Either way, the wallpaper sitting below it in the stacking order can't
// be seen, so render-server has no reason to keep rendering it -- see its
// `SetMonitorOccluded` D-Bus handler and `OcclusionGate`.
//
// The two cases collapse into one geometry test: does this window's
// `frameGeometry` cover the area a maximized window would occupy on this
// output (`KWin.MaximizeArea`)? A genuinely fullscreen window's geometry
// always meets or exceeds that (it covers the *full* screen, struts and
// all), so there's no need to check `fullScreen` separately.

const lastSentOcclusion = new Map(); // output.name -> bool last sent to render-server

function rectCovers(outer, inner) {
    return outer.x <= inner.x && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height;
}

// `desktops`/`onAllDesktops` are what actually gate visibility on Wayland
// (there's no single "the" desktop a window is or isn't on) -- an empty
// `desktops` list means "on every desktop" too, same as `onAllDesktops`.
function windowOnCurrentDesktop(window) {
    if (window.onAllDesktops) {
        return true;
    }
    const desktops = window.desktops;
    return !desktops || desktops.length === 0 || desktops.includes(workspace.currentDesktop);
}

function windowCoversOutput(window, output, maximizeArea) {
    return !window.minimized
        && window.output === output
        && windowOnCurrentDesktop(window)
        && rectCovers(window.frameGeometry, maximizeArea);
}

function sendMonitorOccluded(outputName, occluded) {
    try {
        callDBus(
            "dev.wplinux.CursorBridge",
            "/dev/wplinux/CursorBridge",
            "dev.wplinux.CursorBridge",
            "SetMonitorOccluded",
            outputName,
            occluded,
            onCursorBridgeReply
        );
    } catch (e) {
        print("wplinux occlusion: callDBus threw: " + e);
    }
}

function recomputeOcclusion() {
    try {
        const windows = workspace.windowList();
        for (const output of workspace.screens) {
            const maximizeArea = workspace.clientArea(KWin.MaximizeArea, output);
            const covered = windows.some(w => windowCoversOutput(w, output, maximizeArea));
            if (lastSentOcclusion.get(output.name) !== covered) {
                lastSentOcclusion.set(output.name, covered);
                print("wplinux occlusion: " + output.name + " -> " + covered);
                sendMonitorOccluded(output.name, covered);
            }
        }
    } catch (e) {
        print("wplinux occlusion: recompute threw: " + e);
    }
}

// Every window this script currently has fine-grained state-change
// signals connected to, so `windowRemoved` can disconnect them again
// instead of leaking one listener per window ever opened over the life of
// the session.
const trackedWindows = new Set();

function trackWindow(window) {
    if (trackedWindows.has(window)) {
        return;
    }
    trackedWindows.add(window);
    window.fullScreenChanged.connect(recomputeOcclusion);
    window.maximizedChanged.connect(recomputeOcclusion);
    window.outputChanged.connect(recomputeOcclusion);
    window.minimizedChanged.connect(recomputeOcclusion);
    window.desktopsChanged.connect(recomputeOcclusion);
}

function untrackWindow(window) {
    if (!trackedWindows.has(window)) {
        return;
    }
    trackedWindows.delete(window);
    window.fullScreenChanged.disconnect(recomputeOcclusion);
    window.maximizedChanged.disconnect(recomputeOcclusion);
    window.outputChanged.disconnect(recomputeOcclusion);
    window.minimizedChanged.disconnect(recomputeOcclusion);
    window.desktopsChanged.disconnect(recomputeOcclusion);
}

// Guarded like the block above rather than left to throw at script load:
// a missing/renamed API here (e.g. a `clientArea` enum member that
// doesn't exist on some KWin version) would otherwise take the whole
// script down, killing cursor forwarding along with it.
try {
    for (const window of workspace.windowList()) {
        trackWindow(window);
    }
    workspace.windowAdded.connect(window => {
        trackWindow(window);
        recomputeOcclusion();
    });
    workspace.windowRemoved.connect(window => {
        untrackWindow(window);
        recomputeOcclusion();
    });
    workspace.currentDesktopChanged.connect(recomputeOcclusion);
    workspace.screensChanged.connect(recomputeOcclusion);
    recomputeOcclusion();
    print("wplinux occlusion: tracking " + trackedWindows.size + " window(s)");
} catch (e) {
    print("wplinux occlusion: setup threw: " + e);
}
