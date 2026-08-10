#!/usr/bin/env bash
# Reverses install.sh: stops and removes the render-server systemd --user
# service, removes the two KDE packages, and deletes the three binaries
# from ~/.local/bin. Does NOT touch any wallpaper projects you saved --
# those are your data, wherever you put them, and are left alone. This
# also includes the wallpaper library install.sh populates with the
# example wallpapers (~/.local/share/wp_linux/wallpapers/ by default).

set -uo pipefail

BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
APPLICATIONS_DIR="${HOME}/.local/share/applications"
ICON_THEME_DIR="${HOME}/.local/share/icons/hicolor"
# Matches crates/wp_linux_editor/src/library.rs's LIBRARY_SUBDIR / dirs::data_dir().
WALLPAPERS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/wp_linux/wallpapers"
PLASMA_PLUGIN_ID="dev.wplinux.wallpaper"
KWIN_SCRIPT_ID="dev.wplinux.cursorbridge"
DESKTOP_FILE_ID="dev.wplinux.editor"

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
rm -f "${BIN_DIR}/render-server" "${BIN_DIR}/player" "${BIN_DIR}/wp_linux_editor"

log "removing application menu entry and icons"
rm -f "${APPLICATIONS_DIR}/${DESKTOP_FILE_ID}.desktop"
rm -f "${ICON_THEME_DIR}/128x128/apps/${DESKTOP_FILE_ID}.png" \
      "${ICON_THEME_DIR}/256x256/apps/${DESKTOP_FILE_ID}.png" \
      "${ICON_THEME_DIR}/scalable/apps/${DESKTOP_FILE_ID}.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$ICON_THEME_DIR" >/dev/null 2>&1 || true
fi

log "done. Your saved wallpaper projects were not touched."
log "your wallpaper library (including any downloaded examples) is still at ${WALLPAPERS_DIR} -- remove it by hand if you don't want it anymore."
