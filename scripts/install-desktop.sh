#!/bin/sh
# Installs the FastViz launcher and icon for the current user. The launcher
# runs the release build in this checkout, so rebuilding updates it, and it
# lists FastViz under "Open With" for the photo and video types in the
# desktop file. (The .deb installs a system-wide launcher instead.)
#   --default    also make FastViz the default app for those types
#   --uninstall  remove the launcher, the icon and any FastViz defaults
set -eu

app_id=io.github.wastedlookz.FastViz
root=$(cd "$(dirname "$0")/.." && pwd)
source_desktop="$root/data/$app_id.desktop"
types=$(sed -n 's/^MimeType=//p' "$source_desktop" | tr ';' ' ')
data=${XDG_DATA_HOME:-$HOME/.local/share}
desktop="$data/applications/$app_id.desktop"
icon="$data/icons/hicolor/scalable/apps/$app_id.svg"
mimeapps=${XDG_CONFIG_HOME:-$HOME/.config}/mimeapps.list

# Before 0.1.0 the app ID was io.github.fastviz.FastViz: drop that launcher
# and point defaults that named it at the current one.
rm -f "$data/applications/io.github.fastviz.FastViz.desktop" \
    "$data/icons/hicolor/scalable/apps/io.github.fastviz.FastViz.svg"
[ -f "$mimeapps" ] && sed -i "s/io\.github\.fastviz\.FastViz\.desktop/$app_id.desktop/g" "$mimeapps"

case "${1:-}" in
--uninstall)
    rm -f "$desktop" "$icon"
    # Default-app entries that name only FastViz.
    [ -f "$mimeapps" ] && sed -i "/=$app_id\.desktop;\?\$/d" "$mimeapps"
    echo "Removed $desktop, $icon and FastViz defaults"
    ;;
"" | --default)
    mkdir -p "$(dirname "$desktop")" "$(dirname "$icon")"
    cp "$root/data/icons/$app_id.svg" "$icon"
    sed "s|^Exec=fastviz|Exec=$root/target/release/fastviz|" "$source_desktop" > "$desktop"
    echo "Installed $desktop and $icon"
    if [ "${1:-}" = "--default" ]; then
        # The previous choices, to restore by hand if wanted.
        if [ -f "$mimeapps" ] && [ ! -f "$mimeapps.before-fastviz" ]; then
            cp "$mimeapps" "$mimeapps.before-fastviz"
            echo "Saved the previous defaults to $mimeapps.before-fastviz"
        fi
        xdg-mime default "$app_id.desktop" $types
        echo "FastViz is now the default app for photos and videos"
    fi
    ;;
*)
    echo "usage: $0 [--default | --uninstall]" >&2
    exit 2
    ;;
esac
update-desktop-database "$data/applications" 2>/dev/null || true
# Refresh the user icon cache only if there is one: GTK trusts a cache over
# the folder, so a stale one hides icons added or removed without it.
if [ -f "$data/icons/hicolor/icon-theme.cache" ]; then
    gtk-update-icon-cache -q -f -t "$data/icons/hicolor" 2>/dev/null || true
fi
