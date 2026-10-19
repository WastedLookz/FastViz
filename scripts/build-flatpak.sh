#!/bin/sh
# Builds the Flatpak bundle (a single .flatpak file for GitHub Releases) into
# target/flatpak-bundle/. Needs the Flatpak builder from Flathub:
#   flatpak install flathub org.flatpak.Builder
# The bundle names Flathub as the source of its runtime (GNOME 51), so
# installing it fetches the runtime from there.
set -eu
cd "$(dirname "$0")/.."
app=io.github.wastedlookz.FastViz
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=target/flatpak-bundle/fastviz_${version}_x86_64.flatpak
mkdir -p target/flatpak-bundle
flatpak run org.flatpak.Builder --user --install-deps-from=flathub --force-clean \
    --default-branch=stable --state-dir=target/flatpak-state \
    --repo=target/flatpak-repo target/flatpak "flatpak/$app.yml"
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    target/flatpak-repo "$out" "$app" stable
echo "$out"
