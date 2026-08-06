#!/usr/bin/env bash
# Installs WP Linux for the current user: downloads the latest release
# archive, drops the binaries in ~/.local/bin, installs the two KDE
# packages (Plasma wallpaper plugin + KWin cursor-bridge script), enables
# the KWin script, and installs+starts the render-server systemd --user
# service. No root required -- everything lands under $HOME.
#
# For Arch Linux, prefer packaging/archlinux/PKGBUILD instead (system-wide
# install via pacman).

set -euo pipefail

REPO="AzimovIz/WP_Linux"
ARCHIVE_URL="https://github.com/${REPO}/releases/latest/download/wp-linux-linux-x86_64.tar.gz"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
PLASMA_PLUGIN_ID="dev.wplinux.wallpaper"
KWIN_SCRIPT_ID="dev.wplinux.cursorbridge"

log()  { echo "install.sh: $*"; }
warn() { echo "install.sh: warning: $*" >&2; }
die()  { echo "install.sh: error: $*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not found in PATH."
}

need curl
need tar
need kpackagetool6

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

log "downloading ${ARCHIVE_URL}"
curl -fsSL "$ARCHIVE_URL" -o "$tmpdir/wp-linux.tar.gz"

log "extracting"
tar xzf "$tmpdir/wp-linux.tar.gz" -C "$tmpdir"
pkgroot="$tmpdir/wp-linux"

log "installing binaries to ${BIN_DIR}"
mkdir -p "$BIN_DIR"
install -m755 "$pkgroot/bin/render-server" "$pkgroot/bin/player" "$pkgroot/bin/editor" "$BIN_DIR/"

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
install_kpackage "Plasma/Wallpaper" "$PLASMA_PLUGIN_ID" "$pkgroot/plasma/plasma-plugin/package"

log "installing KWin cursor-bridge script"
install_kpackage "KWin/Script" "$KWIN_SCRIPT_ID" "$pkgroot/plasma/kwin-script/package"

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

if command -v systemctl >/dev/null 2>&1; then
    log "installing render-server systemd user service"
    mkdir -p "$SYSTEMD_USER_DIR"
    cp "$pkgroot/systemd/render-server.service" "$SYSTEMD_USER_DIR/"
    systemctl --user daemon-reload
    systemctl --user enable --now render-server.service
else
    warn "systemctl not found -- start render-server by hand: ${BIN_DIR}/render-server"
fi

case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *) warn "${BIN_DIR} is not on your PATH -- add it to run 'editor'/'player' by name." ;;
esac

log "done. Run 'editor' to build a project, then pick 'WP Linux Wallpaper' in"
log "System Settings > Appearance > Wallpaper and point it at the saved project folder."
