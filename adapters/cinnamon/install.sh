#!/usr/bin/env bash
# Cinnamon adapter's install step: installs the WP Linux Cinnamon
# extension (adapters/cinnamon/extension) to the per-user extensions
# directory and enables it. Called by the top-level install.sh once it
# has already placed the core binaries -- this script only ever touches
# Cinnamon-specific state, nothing under $BIN_DIR or ~/.config/autostart.
#
# Usage: install.sh <pkgroot>
#   <pkgroot>  the extracted release archive's root, i.e. it contains
#              adapters/cinnamon/extension/metadata.json

set -euo pipefail

pkgroot="${1:?usage: install.sh <pkgroot>}"
EXT_UUID="wp-linux@wplinux.dev"
EXTENSIONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/cinnamon/extensions"
EXT_DIR="${EXTENSIONS_DIR}/${EXT_UUID}"
SRC_DIR="$pkgroot/adapters/cinnamon/extension"

log()  { echo "adapters/cinnamon/install.sh: $*"; }
warn() { echo "adapters/cinnamon/install.sh: warning: $*" >&2; }
die()  { echo "adapters/cinnamon/install.sh: error: $*" >&2; exit 1; }

[ -f "$SRC_DIR/metadata.json" ] || die "$SRC_DIR/metadata.json not found -- broken release archive?"

log "installing Cinnamon extension to ${EXT_DIR}"
mkdir -p "$EXTENSIONS_DIR"
# Replace any previous install wholesale rather than merging over it, so
# a file removed between versions doesn't linger.
rm -rf "$EXT_DIR"
cp -r "$SRC_DIR" "$EXT_DIR"

# `enabled-extensions` is a plain `as` (array-of-string) GSettings key
# (org.cinnamon.gschema.xml), the same mechanism the Extensions page in
# System Settings and `cinnamon-extension-tool` both ultimately write to
# -- confirmed present on this machine even where the latter tool wasn't
# (cinnamon-extension-tool turned out not to be universally packaged,
# e.g. it's absent on at least one real Cinnamon 6.6.4 install this was
# tested against). Talking to gsettings directly needs nothing but glib2
# itself, which a running Cinnamon session guarantees. A leading "!" on
# the uuid tells Cinnamon to skip its own `cinnamon-version` compatibility
# check -- used here rather than relying on metadata.json's declared
# version list alone, since this project has no way to verify that list
# against Cinnamon's actual comparison logic without a matrix of real
# installs; if this extension ever needs a hard version floor, the metadata.json list is still checked whenever the "!" is dropped by hand.
add_to_gsettings_list() {
    local schema="$1" key="$2" value="$3"
    local current
    current="$(gsettings get "$schema" "$key" 2>/dev/null)" || return 1
    case "$current" in
        "@as "*) current="${current#@as }" ;;
    esac
    if [[ "$current" == *"$value"* ]]; then
        return 0
    fi
    if [ "$current" = "[]" ]; then
        gsettings set "$schema" "$key" "['$value']"
    else
        gsettings set "$schema" "$key" "${current%]}, '$value']"
    fi
}

remove_from_gsettings_list() {
    local schema="$1" key="$2" value="$3"
    local current
    current="$(gsettings get "$schema" "$key" 2>/dev/null)" || return 1
    if [[ "$current" != *"$value"* ]]; then
        return 0
    fi
    gsettings set "$schema" "$key" \
        "$(gsettings get "$schema" "$key" | sed -E "s/'!?${value//./\\.}'(, )?//g; s/, \]/]/")"
}

enabled=false
if command -v cinnamon-extension-tool >/dev/null 2>&1; then
    log "enabling ${EXT_UUID} via cinnamon-extension-tool"
    # Talks to the running cinnamon process, which needs to have already
    # noticed the directory above -- best-effort: a brand new install
    # right after login can miss this, same as re-ticking the KDE KWin
    # script by hand would.
    if cinnamon-extension-tool --enable "$EXT_UUID" 2>/dev/null; then
        enabled=true
    fi
fi

if [ "$enabled" = false ] && command -v gsettings >/dev/null 2>&1; then
    log "enabling ${EXT_UUID} via gsettings"
    remove_from_gsettings_list org.cinnamon disabled-extensions "$EXT_UUID" || true
    if add_to_gsettings_list org.cinnamon enabled-extensions "!${EXT_UUID}"; then
        enabled=true
    fi
fi

if [ "$enabled" = false ]; then
    warn "couldn't enable ${EXT_UUID} automatically -- log out and back in, then enable"
    warn "'WP Linux Wallpaper' by hand in System Settings > Extensions, or run:"
    warn "    gsettings set org.cinnamon enabled-extensions \"['!${EXT_UUID}']\""
    warn "(prepending existing entries from 'gsettings get org.cinnamon enabled-extensions'"
    warn "instead of replacing them, if you already have other extensions enabled)"
fi

warn "Cinnamon support is brand new and unverified across hardware/driver combinations -- see adapters/cinnamon/README.md."
