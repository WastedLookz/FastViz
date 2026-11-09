#!/bin/sh
# Test-builds packaging/aur/PKGBUILD in an Arch Linux tree, without root or
# containers: the Arch bootstrap tarball is unpacked into target/arch and
# entered with bubblewrap (namespaced root for pacman, a plain user for
# makepkg). The source tarball is made from this checkout (HEAD plus
# uncommitted changes), so this works before the release tag exists. Run from anywhere:
#   tools/test-aur.sh            build (and run the tests) once
#   tools/test-aur.sh --update   also update the Arch tree first
#   tools/test-aur.sh --release  build from the GitHub tag's tarball instead,
#                                checking the PKGBUILD's checksum (after tagging)
# Afterwards target/arch/build has the packages and makepkg.log.
# packaging/aur/LICENSE (0BSD) covers the package files themselves, as the
# AUR guidelines ask; FastViz is GPL-3.0-or-later.
set -eu
cd "$(dirname "$0")/.."
top=$PWD/target/arch
root=$top/root.x86_64
build=$top/build
mirror=https://geo.mirror.pkgbuild.com
mkdir -p "$top" "$build/home"

as_root() {
    bwrap --bind "$root" / --dev /dev --proc /proc --tmpfs /tmp \
        --bind "$build" /build --ro-bind /etc/resolv.conf /etc/resolv.conf \
        --unshare-user --uid 0 --gid 0 --unshare-pid --die-with-parent \
        --setenv HOME /root --setenv PATH /usr/bin "$@"
}
as_user() {
    bwrap --bind "$root" / --dev /dev --proc /proc --tmpfs /tmp \
        --bind "$build" /build --ro-bind /etc/resolv.conf /etc/resolv.conf \
        --unshare-user --unshare-pid --die-with-parent \
        --setenv HOME /build/home --setenv PATH /usr/bin --chdir /build "$@"
}

if [ ! -x "$root/usr/bin/pacman" ]; then
    echo "Downloading the Arch bootstrap tree..."
    curl -fsSL -o "$top/bootstrap.tar.zst" "$mirror/iso/latest/archlinux-bootstrap-x86_64.tar.zst"
    curl -fsSL -o "$top/sha256sums.txt" "$mirror/iso/latest/sha256sums.txt"
    (cd "$top" && grep ' archlinux-bootstrap-x86_64.tar.zst$' sha256sums.txt |
        sed 's/archlinux-bootstrap-x86_64.tar.zst/bootstrap.tar.zst/' | sha256sum -c -)
    # Device nodes and a few xattrs can't be created without root; not needed.
    tar --zstd -xf "$top/bootstrap.tar.zst" -C "$top" --exclude='./dev/*' 2>/dev/null || true
    sed -i "s|^#Server = $mirror/|Server = $mirror/|" "$root/etc/pacman.d/mirrorlist"
    # pacman can't switch to its download user or check disk space in here.
    sed -i 's/^CheckSpace/#CheckSpace/; s/^DownloadUser/#DownloadUser/;
        s/^#DisableSandboxFilesystem/DisableSandboxFilesystem/;
        s/^#DisableSandboxSyscalls/DisableSandboxSyscalls/' "$root/etc/pacman.conf"
    as_root sh -c 'pacman-key --init && pacman-key --populate archlinux' >/dev/null
    update=yes
else
    update=${1:-}
fi

if [ "$update" = --release ]; then
    release=yes
    update=
fi
if [ -n "$update" ]; then
    # base-devel and the PKGBUILD's depends/makedepends.
    deps=$(sed -n '/^depends=(/,/)/p; /^makedepends=/p' packaging/aur/PKGBUILD |
        grep -o "'[^']*'" | tr -d "'" | tr '\n' ' ')
    as_root pacman -Syu --noconfirm --needed base-devel $deps
fi

version=$(sed -n 's/^pkgver=//p' packaging/aur/PKGBUILD)
rm -rf "$build/src" "$build/pkg" "$build"/*.pkg.tar.zst
cp packaging/aur/PKGBUILD "$build/"
rm -f "$build"/fastviz-*.tar.gz
if [ -n "${release:-}" ]; then
    # makepkg downloads the tag's tarball and checks it against sha256sums.
    skip=
else
    # HEAD plus uncommitted changes to tracked files. The tarball is ours,
    # not GitHub's, so its checksum differs from the PKGBUILD's.
    tree=$(git stash create)
    git archive --format=tar.gz --prefix="FastViz-$version/" -o "$build/fastviz-$version.tar.gz" "${tree:-HEAD}"
    skip=--skipchecksums
fi
as_user makepkg --nodeps --noconfirm $skip >"$build/makepkg.log" 2>&1 ||
    { tail -30 "$build/makepkg.log"; exit 1; }
grep 'test result' "$build/makepkg.log" || true
as_user makepkg --printsrcinfo >packaging/aur/.SRCINFO
ls "$build"/*.pkg.tar.zst
