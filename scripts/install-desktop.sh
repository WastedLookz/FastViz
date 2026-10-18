#!/bin/sh
# Installs the FastViz launcher and icon for the current user. The launcher
# runs the release build in this checkout, so rebuilding updates it, and it
# lists FastViz under "Open With" for the photo and video types below.
#   --default    also make FastViz the default app for those types
#   --uninstall  remove the launcher, the icon and any FastViz defaults
set -eu

app_id=io.github.fastviz.FastViz
types="image/jpeg image/png image/webp image/gif image/bmp image/tiff image/heif
image/heic image/avif video/mp4 video/x-m4v video/quicktime video/3gpp video/3gpp2
video/x-matroska video/webm video/vnd.avi video/x-msvideo video/x-ms-wmv
application/vnd.ms-asf video/x-flv video/mpeg video/mp2t video/ogg video/dv
application/mxf"
root=$(cd "$(dirname "$0")/.." && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
desktop="$data/applications/$app_id.desktop"
icon="$data/icons/hicolor/scalable/apps/$app_id.svg"
mimeapps=${XDG_CONFIG_HOME:-$HOME/.config}/mimeapps.list

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
    list=$(printf '%s;' $types)
    sed -e "s|@BINARY@|$root/target/release/fastviz|" -e "s|@MIMETYPES@|$list|" \
        "$root/data/$app_id.desktop" > "$desktop"
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
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
