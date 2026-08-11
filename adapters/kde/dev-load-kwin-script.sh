#!/usr/bin/env bash
# Dev convenience: load the KWin cursor-bridge script straight from this
# checkout via KWin's own scripting D-Bus interface
# (org.kde.kwin.Scripting on /Scripting), the same interface KWin's own
# scripting console/dev tools use to load an unpackaged script file on the
# fly. Lets you iterate on adapters/kde/kwin-script without a
# `kpackagetool6 --install` + re-login cycle every time.
#
# Not needed for a real install: install.sh / adapters/kde/install.sh
# already install the script as a proper KPackage and enable it via
# kwinrc, which persists across KWin/session restarts on its own -- this
# script exists purely so `render-server` itself doesn't need to know
# anything about KWin (see crates/render-server's module doc comment).
#
# Run this by hand once per KWin restart while developing from source;
# render-server doesn't call it for you.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
plugin_name="dev.wplinux.cursorbridge"
script_path="${script_dir}/kwin-script/package/contents/code/main.js"

log()  { echo "dev-load-kwin-script.sh: $*"; }
die()  { echo "dev-load-kwin-script.sh: error: $*" >&2; exit 1; }

qdbus_bin="qdbus6"
command -v "$qdbus_bin" >/dev/null 2>&1 || qdbus_bin="qdbus"
command -v "$qdbus_bin" >/dev/null 2>&1 || die "need qdbus6 or qdbus in PATH"

[ -f "$script_path" ] || die "script not found at $script_path"

already_loaded="$("$qdbus_bin" org.kde.KWin /Scripting isScriptLoaded "$plugin_name" 2>/dev/null || true)"
if [ "$already_loaded" = "true" ]; then
    log "already loaded"
    exit 0
fi

"$qdbus_bin" org.kde.KWin /Scripting loadScript "$script_path" "$plugin_name" >/dev/null
"$qdbus_bin" org.kde.KWin /Scripting start >/dev/null
log "loaded and started $script_path"
