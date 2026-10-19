#!/bin/sh
# Builds the .deb (for Ubuntu 24.04 and distributions based on it) into
# target/debian/. Needs cargo-deb: cargo install cargo-deb --locked
set -eu
cd "$(dirname "$0")/.."
cargo build --release
python3 tools/third-party-licenses.py target/release/THIRD-PARTY-LICENSES.txt
cargo deb --no-build
