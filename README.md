# FastViz

A minimal, very fast photo and video browser for Linux (GNOME, Wayland).

FastViz opens large folders of photos and clips instantly, even on slow or
external drives, and keeps a thumbnail cache so folders you have visited
before show up at once. It aims for the speed of FastStone Image Viewer with
a simple, modern GTK 4 look.

![FastViz showing a folder of photos](data/screenshots/main.png)

## Features

- Folder tree and a thumbnail grid that stays smooth with tens of thousands of files
- Home folders, every drive and your phone in the folder tree; drives mount when you open them
- Persistent thumbnail cache; JPEGs use their embedded or scaled-down previews
- Fullscreen viewer: shrink-to-fit, zoom (also below fit) and pan, next and previous items preloaded
- Video playback with hardware decoding (VA-API), optional looping, and video thumbnails
- Photos: JPEG, PNG, WebP, GIF (animated), BMP, TIFF, HEIC/HEIF, AVIF
- Camera RAW (Canon CR2/CR3, Sony ARW, Nikon NEF, DNG, Fujifilm, Olympus, Panasonic, Pentax and more): thumbnails from the camera's embedded preview; fullscreen shows it at once, then the developed RAW (LibRaw)
- Fullscreen metadata panel: push the pointer to the right edge for all of the photo's EXIF
- Rotate photos and videos and crop photos in fullscreen, without losing quality: JPEGs turn by their orientation tag and crop losslessly, MP4/MOV clips turn by their header; the file keeps its date (JPEG, PNG, MP4, MOV, 3GP)
- Videos: MP4, MOV, MKV, WebM, AVI, WMV, FLV, MPEG, TS/M2TS, OGV, 3GP and more
- Sort by name, date modified, date taken, format, dimensions or size; shuffle
- Show all files, or only pictures, only videos, or both
- Search the open folder by name
- Accent color from the desktop theme, or one of nine to pick from
- Cut, copy, paste, rename, new folders and move to Trash with undo, working with Files
- Properties: type, size, dimensions, dates, camera and lens; a folder's contents and total size
- Slideshows

## Install

Both packages are on the
[Releases](https://github.com/WastedLookz/FastViz/releases) page.

**Flatpak** (most distributions): download `fastviz_0.5.0_x86_64.flatpak` and
open it in Software, or install it from a terminal:

```sh
flatpak install --user ./fastviz_0.5.0_x86_64.flatpak
```

It needs [Flathub](https://flathub.org/setup) set up, for the GNOME runtime it
runs on. FastViz itself is not on Flathub, so it doesn't update on its own:
install the next release's file the same way.

**Debian package** for **Ubuntu 24.04** and distributions based on it (Zorin
OS 18, Linux Mint 22, Pop!_OS 24.04), using the system's GTK, mpv and FFmpeg:
download `fastviz_0.5.0-1_amd64.deb` and open it, or:

```sh
sudo apt install ./fastviz_0.5.0-1_amd64.deb
```

**Arch Linux** and distributions based on it: `packaging/aur/PKGBUILD` builds
the release from source (it will also go to the AUR as `fastviz`):

```sh
git clone https://github.com/WastedLookz/FastViz.git
cd FastViz/packaging/aur
makepkg -si
```

## Build from source

Install Rust (<https://rustup.rs>) and the development packages:

```sh
sudo apt install build-essential pkg-config clang libclang-dev \
    libgtk-4-dev libadwaita-1-dev libmpv-dev libheif-dev libturbojpeg0-dev libraw-dev libegl-dev \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev \
    libavfilter-dev libavdevice-dev
```

Then build and run:

```sh
cargo build --release
./target/release/fastviz [file or folder]
```

`scripts/install-desktop.sh` adds a launcher for this build to your desktop
(and lists FastViz under "Open With"); `scripts/build-deb.sh` builds the `.deb`.

### Flatpak

`flatpak/` holds the Flatpak manifest (GNOME 51 runtime; it builds mpv and
libheif too). With the Flatpak builder from Flathub
(`flatpak install flathub org.flatpak.Builder`), `scripts/build-flatpak.sh`
builds the `.flatpak` file, or install straight from the checkout:

```sh
flatpak run org.flatpak.Builder --user --install --install-deps-from=flathub \
    --force-clean target/flatpak flatpak/io.github.wastedlookz.FastViz.yml
```

## Keys

**Thumbnails**

| Key | Action |
| --- | --- |
| Enter, double-click | Open folder, view photo or video fullscreen |
| Backspace, Alt+Up | Parent folder |
| Ctrl+mouse wheel | Thumbnail size |
| Ctrl+X, Ctrl+C, Ctrl+V | Cut, copy, paste (in the folder tree: the selected folder) |
| F2 | Rename |
| Shift+Ctrl+N | New folder |
| Alt+Enter | Properties |
| Delete, Shift+Delete | Move to Trash, delete permanently |
| F5 | Slideshow |
| F9 | Show or hide the folder tree |
| Ctrl+H | Show hidden folders |
| Ctrl+, | Preferences |

**Fullscreen**

| Key | Action |
| --- | --- |
| Mouse wheel, Left/Right, Page Up/Down | Previous, next |
| Home, End | First, last |
| Up, Down | Zoom in, out, also below fit (videos: volume) |
| Mouse wheel at the bottom edge | Zoom around the pointer |
| Pointer against the bottom edge | Tool strip: rotate, crop, exact zoom |
| R, L | Rotate right, left |
| C | Crop (then Enter to crop, Esc to cancel) |
| Right-click | Rotate and crop menu |
| Click | 100% or fit (videos: pause) |
| 0 | Fit to the screen |
| Space | Pause a video or the slideshow |
| M | Mute |
| B | Gray or black background |
| I | Info overlay |
| Delete | Move to Trash and show the next |
| Esc, double-click | Back to the thumbnails |

Settings are kept in `~/.config/photoviewer/` and the thumbnail cache in
`~/.cache/photoviewer/`.

## License

FastViz is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License, version 3 or (at your option) any
later version. See [LICENSE](LICENSE).

It uses GTK, libadwaita, mpv, FFmpeg, libheif and libjpeg-turbo from the
system, and the Rust crates listed in `Cargo.lock`; the `.deb` includes their
license notices in `/usr/share/doc/fastviz/THIRD-PARTY-LICENSES.txt`.
