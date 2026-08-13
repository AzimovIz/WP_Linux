#!/usr/bin/env bash
# X11 adapter's install step: installs the wp-linux-x11-adapter binary to
# ~/.local/bin and registers it to autostart via an XDG Desktop
# Application Autostart .desktop file. Unlike adapters/kde and
# adapters/gnome, this adapter is a plain standalone process -- X11 has
# no per-desktop scripting host like KWin/gnome-shell to load into --
# so, unlike those two, it does own autostart state, the same mechanism
# the top-level install.sh already uses for render-server (see
# crates/wp_linux_editor/src/autostart.rs's doc comment for why XDG
# autostart and not systemd --user: every major session manager --
# GNOME, KDE, Cinnamon, XFCE, MATE, ... -- reads ~/.config/autostart at
# login regardless of the distro's init system).
#
# Only ever invoked by the top-level install.sh after it has already
# determined the current session is X11, not Wayland (see its own
# detect_session_type()) -- this script itself does not re-check that.
# The binary re-checks it independently at every launch though (see
# main.rs's session::is_x11_session doc comment) as a defense against
# this autostart entry lingering after a later switch to a Wayland
# session.
#
# Usage: install.sh <pkgroot>
#   <pkgroot>  the extracted release archive's root, i.e. it contains
#              adapters/x11/wp-linux-x11-adapter (a prebuilt binary, not
#              source -- see release.yml's staging step)

set -euo pipefail

pkgroot="${1:?usage: install.sh <pkgroot>}"
BIN_NAME="wp-linux-x11-adapter"
BIN_DIR="${HOME}/.local/bin"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
AUTOSTART_FILE_ID="dev.wplinux.x11-adapter"

log()  { echo "adapters/x11/install.sh: $*"; }
die()  { echo "adapters/x11/install.sh: error: $*" >&2; exit 1; }

[ -f "$pkgroot/adapters/x11/$BIN_NAME" ] || die "$pkgroot/adapters/x11/$BIN_NAME not found -- broken release archive?"

log "installing ${BIN_NAME} to ${BIN_DIR}"
mkdir -p "$BIN_DIR"
install -m755 "$pkgroot/adapters/x11/$BIN_NAME" "$BIN_DIR/"

log "installing autostart entry (${AUTOSTART_DIR})"
mkdir -p "$AUTOSTART_DIR"
cat > "${AUTOSTART_DIR}/${AUTOSTART_FILE_ID}.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=WP Linux X11 Adapter
Comment=Shows the WP Linux animated wallpaper as the X11 desktop background
Exec=${BIN_DIR}/${BIN_NAME}
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled=true
EOF
# Matches the shape of render-server's own autostart entry (see the
# top-level install.sh) byte for byte in spirit -- same fields, same
# reasoning for each.

log "starting ${BIN_NAME} for this session"
# A reinstall/update may already have a copy running from a previous
# install -- kill it first so this actually picks up the binary just
# installed, same reasoning as the top-level install.sh's own
# render-server restart. No harm starting it before render-server itself
# is up (a few lines later in the top-level script) -- this adapter
# already treats "render-server isn't answering yet" as a normal
# retry-next-poll condition, not an error.
pkill -x "$BIN_NAME" >/dev/null 2>&1 || true
sleep 0.5
nohup "${BIN_DIR}/${BIN_NAME}" >/dev/null 2>&1 &
