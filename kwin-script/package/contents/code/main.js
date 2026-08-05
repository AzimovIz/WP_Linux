// Forwards the global cursor position to the wp-linux cursor-bridge
// process over D-Bus every time KWin reports it changed. KWin always
// knows the true pointer position, regardless of which QML item inside
// plasmashell (e.g. Folder View's icon layer) currently "owns" hover --
// that's the whole reason this script exists instead of reading the
// cursor from within the wallpaper's own QML.

let lastSent = 0;
const MIN_INTERVAL_MS = 8; // light throttle, ~120Hz cap

function sendCursorPosition() {
    const now = Date.now();
    if (now - lastSent < MIN_INTERVAL_MS) {
        return;
    }
    lastSent = now;

    const pos = workspace.cursorPos;
    callDBus(
        "dev.wplinux.CursorBridge",
        "/dev/wplinux/CursorBridge",
        "dev.wplinux.CursorBridge",
        "SetCursorPosition",
        pos.x,
        pos.y
    );
}

workspace.cursorPosChanged.connect(sendCursorPosition);
