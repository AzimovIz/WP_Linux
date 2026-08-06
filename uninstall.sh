#!/usr/bin/env bash
# Reverses install.sh: stops and removes the render-server systemd --user
# service, removes the two KDE packages, and deletes the three binaries
# from ~/.local/bin. Does NOT touch any wallpaper projects you saved --
# those are your data, wherever you put them, and are left alone.

set -uo pipefail

BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
PLASMA_PLUGIN_ID="dev.wplinux.wallpaper"
KWIN_SCRIPT_ID="dev.wplinux.cursorbridge"

log()  { echo "uninstall.sh: $*"; }
warn() { echo "uninstall.sh: warning: $*" >&2; }

if command -v systemctl >/dev/null 2>&1; then
    log "stopping render-server service"
    systemctl --user disable --now render-server.service >/dev/null 2>&1
    rm -f "${SYSTEMD_USER_DIR}/render-server.service"
    systemctl --user daemon-reload >/dev/null 2>&1
else
    warn "systemctl not found -- skipping service removal."
fi

if command -v kwriteconfig6 >/dev/null 2>&1; then
    kwriteconfig6 --file kwinrc --group Plugins --key "${KWIN_SCRIPT_ID}Enabled" --type bool false
fi

if command -v kpackagetool6 >/dev/null 2>&1; then
    log "removing KWin cursor-bridge script"
    kpackagetool6 --type=KWin/Script --remove "$KWIN_SCRIPT_ID" || warn "couldn't remove $KWIN_SCRIPT_ID (already gone?)"
    log "removing Plasma wallpaper plugin"
    kpackagetool6 --type=Plasma/Wallpaper --remove "$PLASMA_PLUGIN_ID" || warn "couldn't remove $PLASMA_PLUGIN_ID (already gone?)"
else
    warn "kpackagetool6 not found -- skipping KDE package removal."
fi

log "removing binaries from ${BIN_DIR}"
rm -f "${BIN_DIR}/render-server" "${BIN_DIR}/player" "${BIN_DIR}/editor"

log "done. Your saved wallpaper projects were not touched."
