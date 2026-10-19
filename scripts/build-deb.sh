#!/bin/sh
# Builds the .deb (for Ubuntu 24.04 and distributions based on it) into
# target/debian/. Needs cargo-deb: cargo install cargo-deb --locked
#
# Source paths end up in panic messages, so this machine's folders (and its
# user name) are remapped to neutral ones; the `deb` profile keeps these
# build flags apart from the everyday release build.
set -eu
cd "$(dirname "$0")/.."
cargo_home=${CARGO_HOME:-$HOME/.cargo}
export RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$PWD=/fastviz"
export CFLAGS="-ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$PWD=/fastviz"
cargo build --profile deb
python3 tools/third-party-licenses.py target/deb/THIRD-PARTY-LICENSES.txt
cargo deb --no-build --profile deb
