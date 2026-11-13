#!/bin/bash
# Builds the macOS release zip on a Mac (Apple silicon) with Xcode: Swapdesk.app with the release bundle identifier,
# signed with the self-signed "Swapdesk Release" certificate (UPDATES.md 7; no Apple Developer certificate, D22),
# zipped with ditto so the bundle's metadata survives.
#
#   bash packaging/macos/build-release.sh [--no-build] [--adhoc]
#
# The identity comes from the GitHub secrets (SWAPDESK_RELEASE_P12_BASE64 and SWAPDESK_RELEASE_P12_PASSWORD, imported
# into a throwaway keychain) or from the keychain packaging/macos/make-release-cert.sh made on this Mac
# (SWAPDESK_RELEASE_KEYCHAIN and SWAPDESK_RELEASE_KEYCHAIN_PASSWORD). Without either the build stops; --adhoc signs ad
# hoc on purpose (an update then loses the macOS permissions).
#
# Output: target/package/Swapdesk-macos-arm64-<version>.zip and its .sha256 (version = workspace.package.version in
# Cargo.toml; build-app.sh builds into target/, so CARGO_TARGET_DIR must be unset). A self-signed app is not trusted by
# Gatekeeper, so a downloaded copy needs "Open Anyway" once (README, "Install on macOS"); its designated requirement is
# the same for every release, so updates keep the macOS permissions.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
sign_args=(--release-identity); args=()
for a in "$@"; do
  case "$a" in
    --adhoc) sign_args=(--adhoc) ;;
    *) args+=("$a") ;;
  esac
done
if [ "${sign_args[0]}" = --release-identity ]; then
  if [ -n "${SWAPDESK_RELEASE_P12_BASE64:-}" ]; then
    ci_dir="$(mktemp -d "${TMPDIR:-/tmp}/swapdesk-release-kc.XXXXXX")"
    trap 'security delete-keychain "$ci_dir/release.keychain-db" 2>/dev/null; rm -rf "$ci_dir"' EXIT
    bundle="$ci_dir/identity.bin"
    printf '%s' "$SWAPDESK_RELEASE_P12_BASE64" | base64 --decode > "$bundle"
    SWAPDESK_RELEASE_KEYCHAIN_PASSWORD="$(/usr/bin/openssl rand -hex 24)"
    export SWAPDESK_RELEASE_KEYCHAIN="$ci_dir/release.keychain-db" SWAPDESK_RELEASE_KEYCHAIN_PASSWORD
    security create-keychain -p "$SWAPDESK_RELEASE_KEYCHAIN_PASSWORD" "$SWAPDESK_RELEASE_KEYCHAIN"
    security set-keychain-settings "$SWAPDESK_RELEASE_KEYCHAIN"
    security unlock-keychain -p "$SWAPDESK_RELEASE_KEYCHAIN_PASSWORD" "$SWAPDESK_RELEASE_KEYCHAIN"
    security import "$bundle" -f pkcs12 -k "$SWAPDESK_RELEASE_KEYCHAIN" \
      -P "${SWAPDESK_RELEASE_P12_PASSWORD:?the identity needs SWAPDESK_RELEASE_P12_PASSWORD}" -T /usr/bin/codesign >/dev/null
    security set-key-partition-list -S apple-tool:,apple: -s -k "$SWAPDESK_RELEASE_KEYCHAIN_PASSWORD" \
      "$SWAPDESK_RELEASE_KEYCHAIN" >/dev/null
    rm -f "$bundle"
  elif [ -z "${SWAPDESK_RELEASE_KEYCHAIN:-}" ]; then
    echo "no release identity: set the GitHub secrets, or SWAPDESK_RELEASE_KEYCHAIN(_PASSWORD), or pass --adhoc" >&2
    exit 1
  fi
fi
SWAPDESK_BUNDLE_ID="io.github.devbertini.swapdesk" bash scripts/mac/build-app.sh "${sign_args[@]}" ${args[@]+"${args[@]}"}

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
