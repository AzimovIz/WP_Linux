#!/usr/bin/env bash
# Installs WP Linux for the current user: downloads the latest release
# archive, drops the desktop-agnostic core binaries (render-server,
# wp_linux_editor, player) in ~/.local/bin, registers render-server to
# autostart via an XDG Desktop Application Autostart .desktop file
# (~/.config/autostart/) -- no systemd dependency, works the same
# regardless of init system -- and then hands off to whichever
# adapters/<desktop>/install.sh matches the desktop you're running, which
# installs whatever that desktop needs to actually show the render as
# your wallpaper (see adapters/kde and adapters/gnome). No root required
# -- everything lands under $HOME.

set -euo pipefail

REPO="AzimovIz/WP_Linux"
ARCHIVE_URL="https://github.com/${REPO}/releases/latest/download/wp-linux-linux-x86_64.tar.gz"
# Fixed tag, not "latest" -- this release only ever holds the example
# wallpapers and must stay independent of the app's dated release tags
# (see release.yml), otherwise it could get picked up by releases/latest.
EXAMPLES_URL="https://github.com/${REPO}/releases/download/WallpaperExamples/WallpaperExamples.zip"
BIN_DIR="${HOME}/.local/bin"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
AUTOSTART_FILE_ID="dev.wplinux.render-server"
APPLICATIONS_DIR="${HOME}/.local/share/applications"
ICON_THEME_DIR="${HOME}/.local/share/icons/hicolor"
# Matches crates/wp_linux_editor/src/library.rs's LIBRARY_SUBDIR / dirs::data_dir().
WALLPAPERS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/wp_linux/wallpapers"
DESKTOP_FILE_ID="dev.wplinux.editor"

log()  { echo "install.sh: $*"; }
warn() { echo "install.sh: warning: $*" >&2; }
die()  { echo "install.sh: error: $*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not found in PATH."
}

need curl
need tar

# Best-effort desktop detection, used only to pick which adapters/<de>
# script to hand off to below -- nothing above this point (archive
# download, core binaries) depends on it. $XDG_CURRENT_DESKTOP can list
# several colon-separated values (e.g. "ubuntu:GNOME"); a substring match
# is enough since these values are short, well-known identifiers.
detect_de() {
    local id="${XDG_CURRENT_DESKTOP:-}${DESKTOP_SESSION:-}"
    case "$id" in
        *KDE*|*Plasma*) echo "kde" ;;
        *GNOME*)        echo "gnome" ;;
        *)              echo "unknown" ;;
    esac
}

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

log "downloading ${ARCHIVE_URL}"
curl -fsSL "$ARCHIVE_URL" -o "$tmpdir/wp-linux.tar.gz"

log "extracting"
tar xzf "$tmpdir/wp-linux.tar.gz" -C "$tmpdir"
pkgroot="$tmpdir/wp-linux"

log "installing binaries to ${BIN_DIR}"
mkdir -p "$BIN_DIR"
install -m755 "$pkgroot/bin/render-server" "$pkgroot/bin/player" "$pkgroot/bin/wp_linux_editor" "$BIN_DIR/"

log "installing application menu entry for the editor"
mkdir -p "$APPLICATIONS_DIR"
sed "s|@EDITOR_BIN@|${BIN_DIR}/wp_linux_editor|" "$pkgroot/dev.wplinux.editor.desktop" \
    > "$APPLICATIONS_DIR/${DESKTOP_FILE_ID}.desktop"

mkdir -p "$ICON_THEME_DIR/128x128/apps" "$ICON_THEME_DIR/256x256/apps" "$ICON_THEME_DIR/scalable/apps"
install -m644 "$pkgroot/assets/wp_linux_editor_128.png" "$ICON_THEME_DIR/128x128/apps/${DESKTOP_FILE_ID}.png"
install -m644 "$pkgroot/assets/wp_linux_editor_256.png" "$ICON_THEME_DIR/256x256/apps/${DESKTOP_FILE_ID}.png"
install -m644 "$pkgroot/assets/wp_linux_editor.svg" "$ICON_THEME_DIR/scalable/apps/${DESKTOP_FILE_ID}.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$ICON_THEME_DIR" >/dev/null 2>&1 || true
fi

de="$(detect_de)"
# Falls back to "does kpackagetool6 exist" when detection is inconclusive
# (some KDE session managers don't set $XDG_CURRENT_DESKTOP the same way)
# -- matches this script's old behavior of always assuming KDE.
if [ "$de" = "kde" ] || { [ "$de" = "unknown" ] && command -v kpackagetool6 >/dev/null 2>&1; }; then
    log "detected KDE Plasma -- running its adapter install step"
    bash "$pkgroot/adapters/kde/install.sh" "$pkgroot"
elif [ "$de" = "gnome" ] || { [ "$de" = "unknown" ] && command -v gnome-extensions >/dev/null 2>&1; }; then
    log "detected GNOME -- running its adapter install step"
    bash "$pkgroot/adapters/gnome/install.sh" "$pkgroot"
else
    warn "no supported desktop adapter for \$XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-<unset>}."
    warn "render-server and the editor are installed below regardless; see adapters/ for what's supported."
fi

log "installing autostart entry (${AUTOSTART_DIR})"
mkdir -p "$AUTOSTART_DIR"
cat > "${AUTOSTART_DIR}/${AUTOSTART_FILE_ID}.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=WP Linux Render Server
Comment=Renders the WP Linux animated wallpaper in the background
Exec=${BIN_DIR}/render-server
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled=true
EOF
# Matches crates/wp_linux_editor/src/autostart.rs's own format byte for
# byte -- the "Launch at login" checkbox there just overwrites/removes
# this same file, so a user who unticks it and reticks it later gets
# back exactly this.

# Leftover from a pre-autostart-spec install: an enabled systemd --user
# unit pointed at the binary we're about to replace would otherwise keep
# running the old copy (or double up with the autostart entry above)
# until the next logout. Best-effort and silent -- most installs never
# had this.
if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now render-server.service >/dev/null 2>&1 || true
    rm -f "${HOME}/.config/systemd/user/render-server.service"
    systemctl --user daemon-reload >/dev/null 2>&1 || true
fi

log "starting render-server for this session"
# On a reinstall/update an old copy may already be running (and holding
# the HTTP port the new one needs) -- kill it first so this actually
# picks up the binary we just installed, same intent as the old
# `systemctl restart` this replaced. A brief pause gives it time to
# release the port before the new one tries to bind it.
pkill -x render-server >/dev/null 2>&1 || true
sleep 0.5
# Backgrounded and detached from this (non-interactive) script's stdio;
# no job control here to `disown` from, and the child isn't part of any
# job that would get SIGHUP'd when the script itself exits.
nohup "${BIN_DIR}/render-server" >/dev/null 2>&1 &

if command -v unzip >/dev/null 2>&1; then
    log "downloading example wallpapers"
    if curl -fsSL "$EXAMPLES_URL" -o "$tmpdir/wallpaper-examples.zip"; then
        mkdir -p "$WALLPAPERS_DIR"
        unzip -oq "$tmpdir/wallpaper-examples.zip" -d "$WALLPAPERS_DIR"
        log "installed example wallpapers to ${WALLPAPERS_DIR}"
    else
        warn "failed to download example wallpapers -- skipping (get them by hand, see README)"
    fi
else
    warn "'unzip' not found -- skipping example wallpapers."
    warn "install unzip and rerun, or download them by hand from:"
    warn "  https://github.com/${REPO}/releases/tag/WallpaperExamples"
    warn "and unzip into ${WALLPAPERS_DIR}"
fi

if ! command -v wp_linux_editor >/dev/null 2>&1; then
    warn "'wp_linux_editor' was not found on your PATH (${BIN_DIR} isn't in it)."
    warn "the menu entry launches it fine either way, but to run 'wp_linux_editor'/'player' by"
    warn "name from a terminal, add this to your shell profile (~/.bashrc, ~/.zshrc, ...):"
    warn "    export PATH=\"\${HOME}/.local/bin:\$PATH\""
fi

log "done. Run 'wp_linux_editor' (or launch 'WP Linux Editor' from the application menu) to"
log "build a project. On KDE Plasma, pick 'WP Linux Wallpaper' in System Settings >"
log "Appearance > Wallpaper and point it at the saved project folder."
