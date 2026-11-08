#!/bin/bash
# Builds Swapdesk.app (development identity) on the MacBook: the release binary of crates/swapdesk-app-mac, the bundle
# (Info.plist, localized usage strings), and a code signature that stays the same across rebuilds (D25).
#
#   bash scripts/mac/build-app.sh [--no-build] [--adhoc] [--compare]
#
#   --no-build  reuse target/release/swapdesk-app-mac instead of running cargo
#   --adhoc     sign ad hoc (no certificate); the designated requirement is then the binary's cdhash, which changes on
#               every build, so every rebuild would ask for the macOS permissions again
#   --compare   assemble and sign the bundle twice (the second with another CFBundleVersion, so the code directory
#               differs as after a real rebuild) and fail unless the designated requirement is identical; also prints
#               what an ad hoc signature would give, as the control
#
# Output: target/bundle/Swapdesk.app (target/ is ignored by git). From the Windows PC:
#   scripts/mac/remote.sh sync && scripts/mac/remote.sh run 'bash scripts/mac/build-app.sh --compare'
#
# Signing identity (D25): a self-signed code-signing certificate "Swapdesk Development" in its own keychain FILE,
# ~/.swapdesk-dev-signing/swapdesk-dev.keychain-db, created on the first run. It is never added to the keychain search
# list and is never the login keychain (the login keychain is unreachable over SSH, -25308, and holds the owner's
# secrets). The keychain's password is a random string kept in a 0600 file beside it; it protects only this
# development certificate. The certificate is not trusted system-wide (that would need the owner's authorization): the
# signature is valid for macOS's local checks and TCC, not for Gatekeeper, which only applies to downloaded apps.
# If anything in the certificate path fails the script falls back to an ad hoc signature and says why.
#
# Apple documentation read for the keys below (2026-11-08): developer.apple.com/documentation/bundleresources/
# information-property-list/{cfbundleidentifier, cfbundleexecutable, cfbundlepackagetype, cfbundledevelopmentregion,
# cfbundlelocalizations, cfbundleshortversionstring, cfbundleversion, lsminimumsystemversion, nshighresolutioncapable,
# nslocalnetworkusagedescription, nsbonjourservices}, and TN3179 (local network privacy); see docs/RESEARCH.md.
set -euo pipefail

BUNDLE_ID="io.github.devbertini.swapdesk.dev"
CERT_NAME="Swapdesk Development"
SIGN_DIR="${SWAPDESK_SIGN_DIR:-$HOME/.swapdesk-dev-signing}"
KEYCHAIN="$SIGN_DIR/swapdesk-dev.keychain-db"
PASSWORD_FILE="$SIGN_DIR/keychain-password"

build=1; adhoc=0; compare=0
for a in "$@"; do
  case "$a" in
    --no-build) build=0 ;;
    --adhoc) adhoc=1 ;;
    --compare) compare=1 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
export PATH="$HOME/.cargo/bin:$HOME/.local/cmake/CMake.app/Contents/bin:$PATH"
[ -x "$HOME/.local/cmake/CMake.app/Contents/bin/cmake" ] && export CMAKE="$HOME/.local/cmake/CMake.app/Contents/bin/cmake"

version_full="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
short_version="${version_full%%-*}"   # CFBundleShortVersionString allows only digits and periods
bundle_dir="$root/target/bundle"
app="$bundle_dir/Swapdesk.app"
binary="$root/target/release/swapdesk-app-mac"

if [ "$build" = 1 ]; then
  cargo build --release -p swapdesk-app-mac
fi
[ -x "$binary" ] || { echo "missing $binary (run without --no-build)" >&2; exit 1; }
mkdir -p "$bundle_dir"

# ---------- the bundle ----------

assemble() { # $1 = CFBundleVersion
  local build_number="$1"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/en.lproj" "$app/Contents/Resources/pt-BR.lproj"
  cp "$binary" "$app/Contents/MacOS/Swapdesk"
  printf 'APPL????' > "$app/Contents/PkgInfo"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>Swapdesk</string>
  <key>CFBundleDisplayName</key><string>Swapdesk</string>
  <key>CFBundleExecutable</key><string>Swapdesk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>$short_version</string>
  <key>CFBundleVersion</key><string>$build_number</string>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key>
  <array><string>en</string><string>pt-BR</string></array>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Swapdesk uses the local network to find your other computers and to connect to them.</string>
  <key>NSBonjourServices</key>
  <array><string>_swapdesk._udp</string></array>
</dict>
</plist>
PLIST
  cat > "$app/Contents/Resources/en.lproj/InfoPlist.strings" <<'STRINGS'
"NSLocalNetworkUsageDescription" = "Swapdesk uses the local network to find your other computers and to connect to them.";
STRINGS
  cat > "$app/Contents/Resources/pt-BR.lproj/InfoPlist.strings" <<'STRINGS'
"NSLocalNetworkUsageDescription" = "O Swapdesk usa a rede local para encontrar seus outros computadores e se conectar a eles.";
STRINGS
  plutil -lint "$app/Contents/Info.plist" "$app/Contents/Resources/en.lproj/InfoPlist.strings" "$app/Contents/Resources/pt-BR.lproj/InfoPlist.strings"
}

# ---------- the signing identity ----------

identity_hash=""
adhoc_reason=""

find_hash() {
  security find-certificate -c "$CERT_NAME" -Z "$KEYCHAIN" 2>/dev/null | sed -n 's/^SHA-1 hash: *//p' | head -1
}

# Creates the keychain and the certificate on the first run, unlocks the keychain on every run, sets `identity_hash`.
# Every step is checked by hand (the caller tests the result, which turns `set -e` off inside).
prepare_identity() {
  mkdir -p "$SIGN_DIR" && chmod 700 "$SIGN_DIR" || return 1
  local pw work
  if [ ! -f "$KEYCHAIN" ]; then
    work="$(mktemp -d "${TMPDIR:-/tmp}/swapdesk-cert.XXXXXX")" || return 1
    ( umask 077; openssl rand -hex 16 > "$PASSWORD_FILE" ) || return 1
    pw="$(cat "$PASSWORD_FILE")"
    cat > "$work/openssl.cnf" <<'CNF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = Swapdesk Development
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
    # `set-keychain-settings` with no option means no lock timeout; the partition list lets codesign use the private key
    # without a dialog. A half-made keychain is removed so the next run starts clean.
    # The system's LibreSSL, not whatever `openssl` the PATH finds first: OpenSSL 3 and later write PKCS#12 files with
    # AES and a SHA-256 MAC that `security import` rejects ("MAC verification failed"; seen with /opt/homebrew/bin/openssl
    # 4.1.0 on 2026-11-08), LibreSSL's defaults it reads.
    if ! { /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -keyout "$work/id.key" -out "$work/id.crt" -config "$work/openssl.cnf" \
        && PW="$pw" /usr/bin/openssl pkcs12 -export -inkey "$work/id.key" -in "$work/id.crt" -name "$CERT_NAME" -out "$work/identity.p12" -passout env:PW \
        && security create-keychain -p "$pw" "$KEYCHAIN" \
        && security set-keychain-settings "$KEYCHAIN" \
        && security unlock-keychain -p "$pw" "$KEYCHAIN" \
        && security import "$work/identity.p12" -k "$KEYCHAIN" -P "$pw" -T /usr/bin/codesign \
        && security set-key-partition-list -S apple-tool:,apple: -s -k "$pw" "$KEYCHAIN"; } >/dev/null 2>"$work/steps.log"; then
      tail -3 "$work/steps.log" | grep -v '^[.+*]*$' >&2
      rm -rf "$work"; security delete-keychain "$KEYCHAIN" 2>/dev/null; rm -f "$KEYCHAIN" "$PASSWORD_FILE"
      return 1
    fi
    rm -rf "$work"
    echo "created the development signing keychain $KEYCHAIN" >&2
  else
    security unlock-keychain -p "$(cat "$PASSWORD_FILE")" "$KEYCHAIN" || return 1
  fi
  identity_hash="$(find_hash)"
  [ -n "$identity_hash" ] || { echo "the certificate is not in $KEYCHAIN" >&2; return 1; }
}

sign() { # $1 = bundle path, $2 = "cert" or "adhoc"
  if [ "$2" = cert ]; then
    codesign --force --keychain "$KEYCHAIN" --sign "$identity_hash" --identifier "$BUNDLE_ID" --timestamp=none "$1"
  else
    codesign --force --sign - --identifier "$BUNDLE_ID" --timestamp=none "$1"
  fi
}

mode=adhoc
if [ "$adhoc" = 0 ]; then
  if prepare_identity 2>"$bundle_dir/identity.err"; then
    mode=cert
    cat "$bundle_dir/identity.err" >&2
  else
    adhoc_reason="the development certificate could not be prepared: $(tail -3 "$bundle_dir/identity.err" | tr '\n' ' ')"
  fi
fi

sign_with_fallback() { # $1 = bundle path
  if [ "$mode" = cert ]; then
    if ! sign "$1" cert 2>"$bundle_dir/sign.err"; then
      adhoc_reason="codesign with the development certificate failed: $(tail -3 "$bundle_dir/sign.err" | tr '\n' ' ')"
      mode=adhoc
    fi
  fi
  if [ "$mode" = adhoc ]; then
    sign "$1" adhoc
  fi
}

last_dr=""; last_cdhash=""
report() { # prints the designated requirement and the cdhash of $app
  local dr cdhash
  dr="$(codesign -d -r- "$app" 2>&1 | sed -n 's/^#* *designated => //p')"
  cdhash="$(codesign -dvvv "$app" 2>&1 | sed -n 's/^CDHash=//p')"
  echo "SIGNING=$mode${adhoc_reason:+ (fallback: $adhoc_reason)}"
  echo "DESIGNATED_REQUIREMENT=$dr"
  echo "CDHASH=$cdhash"
  codesign --verify --strict --verbose=2 "$app"
  codesign -d -r- "$app" 2>&1
  last_dr="$dr"; last_cdhash="$cdhash"
}

assemble "${SWAPDESK_BUILD_NUMBER:-1}"
sign_with_fallback "$app"
report
echo "BUNDLE=$app"

if [ "$compare" = 1 ]; then
  first_dr="$last_dr"; first_cdhash="$last_cdhash"
  assemble "$(( ${SWAPDESK_BUILD_NUMBER:-1} + 1 ))"
  sign_with_fallback "$app"
  echo "--- second build (another CFBundleVersion) ---"
  report
  echo "COMPARE first DR : $first_dr"
  echo "COMPARE second DR: $last_dr"
  echo "COMPARE first cdhash : $first_cdhash"
  echo "COMPARE second cdhash: $last_cdhash"
  [ "$first_cdhash" != "$last_cdhash" ] || { echo "COMPARE_RESULT FAIL the two builds have the same cdhash, so they prove nothing" >&2; exit 1; }
  # Control: what an ad hoc signature would give for the same second build.
  control="$bundle_dir/control.app"; rm -rf "$control"
  cp -R "$app" "$control"; codesign --force --sign - --identifier "$BUNDLE_ID" --timestamp=none "$control" 2>/dev/null
  echo "CONTROL ad hoc DR: $(codesign -d -r- "$control" 2>&1 | sed -n 's/^#* *designated => //p')"
  rm -rf "$control"
  [ -n "$first_dr" ] && [ -n "$last_dr" ] || { echo "COMPARE_RESULT FAIL no designated requirement could be read" >&2; exit 1; }
  if [ "$first_dr" = "$last_dr" ]; then
    echo "COMPARE_RESULT PASS the designated requirement is the same across rebuilds (signing: $mode)"
  else
    echo "COMPARE_RESULT FAIL the designated requirement changed (signing: $mode)" >&2
    exit 1
  fi
fi
