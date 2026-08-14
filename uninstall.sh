#!/usr/bin/env bash
# Reverses install.sh: stops render-server, removes its autostart entry,
# the desktop-agnostic core binaries from ~/.local/bin, and whatever
# desktop adapter install.sh installed -- the two KDE packages (see
# adapters/kde) and/or the GNOME Shell extension (see adapters/gnome),
# removed below via the same `command -v kpackagetool6`/`kwriteconfig6`/
# `gnome-extensions` guards install.sh's own adapter steps use, which is
# why this is safe to run unconditionally regardless of which
# desktop you're on -- each block below is a no-op on any desktop it
# doesn't apply to. This script is distributed standalone (curl | bash,
# no release archive download), so unlike install.sh it can't hand off to
# an adapters/<de>/uninstall.sh file -- if a future adapter needs its own
# cleanup, inline it here the same way, guarded the same way. Does NOT
# touch any wallpaper projects you saved -- those are your data, wherever
# you put them, and are left alone. This also includes the wallpaper
# library install.sh populates with the example wallpapers
# (~/.local/share/wp_linux/wallpapers/ by default).

set -uo pipefail

BIN_DIR="${HOME}/.local/bin"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
AUTOSTART_FILE_ID="dev.wplinux.render-server"
APPLICATIONS_DIR="${HOME}/.local/share/applications"
ICON_THEME_DIR="${HOME}/.local/share/icons/hicolor"
# Matches crates/wp_linux_editor/src/library.rs's LIBRARY_SUBDIR / dirs::data_dir().
WALLPAPERS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/wp_linux/wallpapers"
PLASMA_PLUGIN_ID="dev.wplinux.wallpaper"
KWIN_SCRIPT_ID="dev.wplinux.cursorbridge"
GNOME_EXT_UUID="wp-linux@wplinux.dev"
GNOME_EXT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/${GNOME_EXT_UUID}"
DESKTOP_FILE_ID="dev.wplinux.editor"

log()  { echo "uninstall.sh: $*"; }
warn() { echo "uninstall.sh: warning: $*" >&2; }

log "removing autostart entry and stopping render-server"
rm -f "${AUTOSTART_DIR}/${AUTOSTART_FILE_ID}.desktop"
pkill -x render-server >/dev/null 2>&1 || true

# Leftover from a pre-autostart-spec install (see install.sh) -- harmless
# no-op if this was never set up.
if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now render-server.service >/dev/null 2>&1 || true
    rm -f "${HOME}/.config/systemd/user/render-server.service"
    systemctl --user daemon-reload >/dev/null 2>&1 || true
fi

# KDE adapter cleanup (see adapters/kde) -- no-op on any other desktop
# since these commands simply won't exist there.
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

# GNOME adapter cleanup (see adapters/gnome) -- no-op on any other desktop
# since neither gnome-extensions nor GNOME_EXT_DIR will exist there.
if command -v gnome-extensions >/dev/null 2>&1; then
    log "disabling WP Linux GNOME Shell extension"
    gnome-extensions disable "$GNOME_EXT_UUID" >/dev/null 2>&1 || true
fi
if [ -d "$GNOME_EXT_DIR" ]; then
    log "removing GNOME Shell extension from ${GNOME_EXT_DIR}"
    rm -rf "$GNOME_EXT_DIR"
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
