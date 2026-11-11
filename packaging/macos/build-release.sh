#!/bin/bash
# Builds the macOS release zip on a Mac (Apple silicon) with Xcode: Swapdesk.app with the release bundle identifier,
# signed ad hoc (no Apple Developer certificate, decision D22), zipped with ditto so the bundle's metadata survives.
#
#   bash packaging/macos/build-release.sh [--no-build]
#
# Output: target/package/Swapdesk-macos-arm64-<version>.zip and its .sha256 (version = workspace.package.version in
# Cargo.toml; build-app.sh builds into target/, so CARGO_TARGET_DIR must be unset). An ad hoc signature is not trusted by Gatekeeper and is tied
# to this exact build, so a downloaded copy needs "Open Anyway" once and asks for its macOS permissions again after
# each update (README, "Install on macOS").
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
SWAPDESK_BUNDLE_ID="io.github.devbertini.swapdesk" bash scripts/mac/build-app.sh --adhoc "$@"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
target="$root/target"
app="$root/target/bundle/Swapdesk.app"
[ -d "$app" ] || { echo "missing $app" >&2; exit 1; }
mkdir -p "$target/package"
zip="$target/package/Swapdesk-macos-arm64-$version.zip"
rm -f "$zip" "$zip.sha256"
ditto -c -k --sequesterRsrc --keepParent "$app" "$zip"
(cd "$target/package" && shasum -a 256 "$(basename "$zip")" > "$(basename "$zip").sha256")
echo "ZIP=$zip"
cat "$zip.sha256"

# The update package (UPDATES.md 3): the signed bundle tree, read back before it is kept.
pkg="$target/package/Swapdesk-macos-arm64-$version.sdpkg"
rm -f "$pkg"
stage="$target/package/update-stage"
rm -rf "$stage" && mkdir -p "$stage"
ditto "$app" "$stage/Swapdesk.app"
cargo build --release -q -p swapdesk-release-tools --bin swapdesk-pack
"$target/release/swapdesk-pack" "$stage" "$pkg"
"$target/release/swapdesk-pack" --list "$pkg" > /dev/null
rm -rf "$stage"
echo "UPDATE=$pkg"
