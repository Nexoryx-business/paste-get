#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Builds the desktop packages: the tray app with the `pasteget` command inside.
#
#   packaging/build-desktop.sh nsis        # Windows installer (run on Windows, in Git Bash)
#   packaging/build-desktop.sh appimage    # Linux AppImage
#
# Needs tauri-cli (`cargo install tauri-cli --version =2.12.4 --locked`). Packages land in
# target/release/bundle/<kind>/. See docs/RELEASING.md.
set -euo pipefail

bundles="${1:?usage: build-desktop.sh <nsis|appimage>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

host="$(rustc -vV | sed -n 's/^host: //p')"
exe=""
case "$host" in *windows*) exe=".exe" ;; esac

# The `pasteget` command ships next to the tray (Tauri calls it a sidecar). Tauri looks for it
# under the host's target triple.
cargo build --release --locked -p pasteget-desktop
mkdir -p desktop/tray/binaries
cp "target/release/pasteget$exe" "desktop/tray/binaries/pasteget-$host$exe"

# The sidecar is only declared here, not in tauri.conf.json, so a plain `cargo build` doesn't
# need it.
cd desktop/tray
cargo tauri build --config tauri.bundle.conf.json --bundles "$bundles" -- --locked
