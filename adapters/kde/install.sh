#!/usr/bin/env bash
# KDE adapter's install step: installs the two KDE packages (Plasma
# wallpaper plugin + KWin cursor-bridge script) and enables the KWin
# script. Called by the top-level install.sh once it has already placed
# the core binaries -- this script only ever touches KDE-specific state,
# nothing under $BIN_DIR or ~/.config/autostart.
#
# Usage: install.sh <pkgroot>
#   <pkgroot>  the extracted release archive's root, i.e. it contains
#              adapters/kde/{plasma-plugin,kwin-script}/package

set -euo pipefail

pkgroot="${1:?usage: install.sh <pkgroot>}"
PLASMA_PLUGIN_ID="dev.wplinux.wallpaper"
KWIN_SCRIPT_ID="dev.wplinux.cursorbridge"

log()  { echo "adapters/kde/install.sh: $*"; }
warn() { echo "adapters/kde/install.sh: warning: $*" >&2; }
die()  { echo "adapters/kde/install.sh: error: $*" >&2; exit 1; }

command -v kpackagetool6 >/dev/null 2>&1 || die "'kpackagetool6' is required but not found in PATH."

install_kpackage() {
    local type="$1" id="$2" path="$3"
    if kpackagetool6 --type="$type" --install "$path" 2>/dev/null; then
        log "installed $id ($type)"
    elif kpackagetool6 --type="$type" --upgrade "$path"; then
        log "upgraded $id ($type)"
    else
        die "failed to install/upgrade $id ($type)"
    fi
}

log "installing Plasma wallpaper plugin"
install_kpackage "Plasma/Wallpaper" "$PLASMA_PLUGIN_ID" "$pkgroot/adapters/kde/plasma-plugin/package"

log "installing KWin cursor-bridge script"
install_kpackage "KWin/Script" "$KWIN_SCRIPT_ID" "$pkgroot/adapters/kde/kwin-script/package"

if command -v kwriteconfig6 >/dev/null 2>&1; then
    log "enabling KWin cursor-bridge script"
    kwriteconfig6 --file kwinrc --group Plugins --key "${KWIN_SCRIPT_ID}Enabled" --type bool true
    if command -v qdbus6 >/dev/null 2>&1; then
        qdbus6 org.kde.KWin /KWin reconfigure >/dev/null 2>&1 || true
    elif command -v qdbus >/dev/null 2>&1; then
        qdbus org.kde.KWin /KWin reconfigure >/dev/null 2>&1 || true
    else
        warn "couldn't find qdbus/qdbus6 to reload KWin -- log out and back in for the cursor script to take effect."
    fi
else
    warn "kwriteconfig6 not found -- enable 'WP Linux Cursor Bridge' by hand in System Settings > Window Management > KWin Scripts."
fi
