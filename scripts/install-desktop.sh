#!/bin/sh
# Installs (or with --uninstall removes) the FastViz launcher and icon for the
# current user. Adds no file associations. The launcher runs the release build
# in this checkout, so rebuilding updates it.
set -eu

app_id=io.github.fastviz.FastViz
root=$(cd "$(dirname "$0")/.." && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
desktop="$data/applications/$app_id.desktop"
icon="$data/icons/hicolor/scalable/apps/$app_id.svg"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$desktop" "$icon"
    echo "Removed $desktop and $icon"
else
    mkdir -p "$(dirname "$desktop")" "$(dirname "$icon")"
    cp "$root/data/icons/$app_id.svg" "$icon"
    sed "s|@BINARY@|$root/target/release/fastviz|" "$root/data/$app_id.desktop" > "$desktop"
    echo "Installed $desktop and $icon"
fi
update-desktop-database "$data/applications" 2>/dev/null || true
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
