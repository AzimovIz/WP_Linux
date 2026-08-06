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
