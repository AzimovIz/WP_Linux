#!/usr/bin/env bash
# GNOME adapter's install step: installs the WP Linux GNOME Shell
# extension (adapters/gnome/extension) to the per-user extensions
# directory and enables it. Called by the top-level install.sh once it
# has already placed the core binaries -- this script only ever touches
# GNOME-specific state, nothing under $BIN_DIR or ~/.config/autostart.
#
# Usage: install.sh <pkgroot>
#   <pkgroot>  the extracted release archive's root, i.e. it contains
#              adapters/gnome/extension/metadata.json

set -euo pipefail

pkgroot="${1:?usage: install.sh <pkgroot>}"
EXT_UUID="wp-linux@wplinux.dev"
EXTENSIONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions"
EXT_DIR="${EXTENSIONS_DIR}/${EXT_UUID}"
SRC_DIR="$pkgroot/adapters/gnome/extension"

log()  { echo "adapters/gnome/install.sh: $*"; }
warn() { echo "adapters/gnome/install.sh: warning: $*" >&2; }
die()  { echo "adapters/gnome/install.sh: error: $*" >&2; exit 1; }

[ -f "$SRC_DIR/metadata.json" ] || die "$SRC_DIR/metadata.json not found -- broken release archive?"

log "installing GNOME Shell extension to ${EXT_DIR}"
mkdir -p "$EXTENSIONS_DIR"
# Replace any previous install wholesale rather than merging over it, so
# a file removed between versions doesn't linger.
rm -rf "$EXT_DIR"
cp -r "$SRC_DIR" "$EXT_DIR"

if command -v gnome-extensions >/dev/null 2>&1; then
    log "enabling ${EXT_UUID}"
    # Talks to the running gnome-shell over D-Bus, which needs to have
    # already noticed the directory above (it watches extensions/ for
    # changes) -- best-effort: a brand new install right after login, or
    # a nested/headless test session with no shell running, can miss
    # this. Either way it's still enabled on the next login regardless,
    # same as re-ticking the KDE KWin script by hand would be.
    if ! gnome-extensions enable "$EXT_UUID" 2>/dev/null; then
        warn "couldn't enable ${EXT_UUID} right now -- log out and back in, then run:"
        warn "    gnome-extensions enable ${EXT_UUID}"
    fi
else
    warn "gnome-extensions not found -- enable 'WP Linux Wallpaper' by hand with the Extensions app, or run:"
    warn "    gnome-extensions enable ${EXT_UUID}"
fi

warn "GNOME support is new and unverified across hardware/driver combinations -- see adapters/gnome/README.md."
