#!/usr/bin/env python3
"""Writes the license notices of the Rust crates compiled into FastViz.

Usage: tools/third-party-licenses.py <output file>   (run from the checkout)

Only crates linked into the binary count (normal dependencies, not build
tools). Each license text is printed once, followed by the crates it covers.
"""

import collections
import json
import os
import subprocess
import sys

TARGET = "x86_64-unknown-linux-gnu"
LICENSE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")


def linked_crates():
    tree = subprocess.run(
        ["cargo", "tree", "-e", "normal", "--prefix", "none", "-f", "{p}", "--target", TARGET],
        check=True, capture_output=True, text=True,
    ).stdout
    crates = set()
    for line in tree.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            crates.add((parts[0], parts[1].lstrip("v")))
    return crates


def main():
    out = sys.argv[1]
    linked = linked_crates()
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1"],
        check=True, capture_output=True, text=True,
    ).stdout)
    texts = collections.OrderedDict()  # license text -> crates it covers
    without_file = []
    packages = sorted(metadata["packages"], key=lambda p: (p["name"], p["version"]))
    for package in packages:
        key = (package["name"], package["version"])
        if key not in linked or package["name"] == "fastviz":
            continue
        label = f'{package["name"]} {package["version"]} ({package.get("license") or "no license field"})'
        folder = os.path.dirname(package["manifest_path"])
        files = sorted(f for f in os.listdir(folder) if f.upper().startswith(LICENSE_PREFIXES))
        if not files:
            without_file.append(label)
        for name in files:
            with open(os.path.join(folder, name), encoding="utf-8", errors="replace") as f:
                texts.setdefault(f.read().strip(), []).append(label)

    with open(out, "w", encoding="utf-8") as f:
        f.write("Third-party software in FastViz\n")
        f.write("===============================\n\n")
        f.write("FastViz is licensed under the GNU GPL, version 3 or later (see\n")
        f.write("/usr/share/common-licenses/GPL-3). It contains the Rust crates below,\n")
        f.write("each under its own license; their license texts follow. The bundled\n")
        f.write("SQLite (in libsqlite3-sys) is in the public domain.\n\n")
        for text, crates in texts.items():
            f.write("-" * 72 + "\n")
            f.write("Applies to: " + ", ".join(crates) + "\n\n")
            f.write(text + "\n\n")
        if without_file:
            f.write("-" * 72 + "\n")
            f.write("These crates ship no license file; their license is as declared:\n")
            for label in without_file:
                f.write("  " + label + "\n")
    print(f"{out}: {len(linked) - 1} crates, {len(texts)} license texts")


if __name__ == "__main__":
    main()
