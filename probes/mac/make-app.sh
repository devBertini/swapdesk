#!/usr/bin/env bash
# Builds the probe, assembles SwapdeskProbe.app, signs it inside-out and installs it in /Applications.
# Runs on the MacBook only (needs Xcode command line tools: codesign, plutil, ditto, xcrun).
#
#   bash make-app.sh <devid|adhoc|selfsigned> [identity] [--notarize <keychain-profile>] [--no-install]
#
#   devid       identity = "Developer ID Application: <Name> (<TEAMID>)"; hardened runtime + secure timestamp
#   selfsigned  identity = the name of a Code Signing certificate made in Keychain Access
#   adhoc       no identity ("-"); every rebuild loses its permission grants
#
# Environment:
#   PROBE_NONCE       any text; changes the binary, so a rebuild has a new cdhash (checklist P7)
#   PROBE_FEATURES    extra cargo features, e.g. prelogin-section (checklist P6)
#   AGENT_PROFILE     path of a .provisionprofile for the agent app; with it the agent is signed with the
#                     Persistent Content Capture entitlement (only if Apple granted it)
#   SKIP_BUILD=1      reuse target/release/swapdesk-probe
#
# --notarize uploads the app to Apple (xcrun notarytool): run it only when the owner agrees. The probe does not
# need notarization (it is built locally and is not quarantined).
set -euo pipefail

SIGNER="${1:-}"
[ $# -gt 0 ] && shift
IDENTITY=""
if [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; then IDENTITY="$1"; shift; fi
NOTARY_PROFILE=""
INSTALL=1
while [ $# -gt 0 ]; do
  case "$1" in
    --notarize) NOTARY_PROFILE="${2:-}"; shift 2 ;;
    --no-install) INSTALL=0; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

case "$SIGNER" in
  devid|selfsigned) [ -n "$IDENTITY" ] || { echo "signer $SIGNER needs an identity name" >&2; exit 2; } ;;
  adhoc) IDENTITY="-" ;;
  *) echo "usage: bash make-app.sh <devid|adhoc|selfsigned> [identity] [--notarize <profile>] [--no-install]" >&2; exit 2 ;;
esac
[ "$(uname -s)" = "Darwin" ] || { echo "run this on the Mac" >&2; exit 1; }

HERE="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="SwapdeskProbe.app"
OUT="$HERE/out"
APP="$OUT/$APP_NAME"
AGENT="$APP/Contents/Helpers/SwapdeskProbeAgent.app"
DEST="/Applications/$APP_NAME"
EXE="$HERE/target/release/swapdesk-probe"

if [ "${SKIP_BUILD:-0}" != "1" ]; then
  FEATURES=()
  [ -n "${PROBE_FEATURES:-}" ] && FEATURES=(--features "$PROBE_FEATURES")
  ( cd "$HERE" && cargo build --release ${FEATURES[@]+"${FEATURES[@]}"} )
fi
[ -x "$EXE" ] || { echo "missing $EXE (cargo build --release failed?)" >&2; exit 1; }

echo "== lint plists"
for f in "$HERE"/bundle/*.plist "$HERE"/bundle/*/*.plist; do plutil -lint "$f" >/dev/null || { echo "bad plist: $f" >&2; exit 1; }; done

echo "== assemble $APP"
[ "$(basename "$APP")" = "$APP_NAME" ] && rm -rf "$APP"   # the build folder only, never /Applications here
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/legacy" "$APP/Contents/Library/LaunchDaemons" \
         "$APP/Contents/Library/LaunchAgents" "$AGENT/Contents/MacOS" "$AGENT/Contents/Resources"
cp "$HERE/bundle/Info.plist" "$APP/Contents/Info.plist"
cp "$HERE/bundle/Agent-Info.plist" "$AGENT/Contents/Info.plist"
cp "$EXE" "$APP/Contents/MacOS/swapdesk-probe"
cp "$EXE" "$AGENT/Contents/MacOS/swapdesk-probe"
cp "$HERE"/bundle/LaunchDaemons/*.plist "$APP/Contents/Library/LaunchDaemons/"
cp "$HERE"/bundle/LaunchAgents/*.plist "$APP/Contents/Library/LaunchAgents/"
cp "$HERE"/bundle/legacy/*.plist "$APP/Contents/Resources/legacy/"
# The probe reads its signer (devid|adhoc|selfsigned) from Contents/Resources/signer for the log lines.
printf '%s\n' "$SIGNER" > "$APP/Contents/Resources/signer"
printf '%s\n' "$SIGNER" > "$AGENT/Contents/Resources/signer"

AGENT_ENT="$HERE/bundle/entitlements/agent.plist"
if [ -n "${AGENT_PROFILE:-}" ]; then
  [ -f "$AGENT_PROFILE" ] || { echo "AGENT_PROFILE not found: $AGENT_PROFILE" >&2; exit 1; }
  cp "$AGENT_PROFILE" "$AGENT/Contents/embedded.provisionprofile"
  AGENT_ENT="$HERE/bundle/entitlements/agent-persistent-content-capture.plist"
  echo "agent signed with the Persistent Content Capture entitlement"
fi

sign() { # sign <path> <entitlements> [identifier]
  local path="$1" ent="$2" ident="${3:-}"
  local args=(--force --options runtime --sign "$IDENTITY" --entitlements "$ent")
  [ "$SIGNER" = "devid" ] && args+=(--timestamp)
  [ -n "$ident" ] && args+=(--identifier "$ident")
  codesign "${args[@]}" "$path"
}

echo "== sign inside-out ($SIGNER)"
sign "$AGENT/Contents/MacOS/swapdesk-probe" "$AGENT_ENT" "local.swapdesk.probe.agent"
sign "$AGENT" "$AGENT_ENT"
sign "$APP/Contents/MacOS/swapdesk-probe" "$HERE/bundle/entitlements/main.plist" "local.swapdesk.probe"
sign "$APP" "$HERE/bundle/entitlements/main.plist"

echo "== verify"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dvvv "$APP" 2>&1 | grep -E 'Identifier=|CDHash=|Authority=|TeamIdentifier=|flags=' || true
codesign -d -r- "$APP" 2>&1 | grep -E 'designated' || true

if [ -n "$NOTARY_PROFILE" ]; then
  [ "$SIGNER" = "devid" ] || { echo "notarization needs the devid signer" >&2; exit 1; }
  echo "== notarize (uploads $APP_NAME to Apple)"
  ZIP="$OUT/SwapdeskProbe.zip"
  ditto -c -k --keepParent "$APP" "$ZIP"
  xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$APP"
fi

if [ "$INSTALL" = "1" ]; then
  echo "== install $DEST"
  [ -d "$DEST" ] && rm -rf "$DEST"   # the previous probe copy only; one copy in /Applications (TN3179)
  ditto "$APP" "$DEST"
  codesign --verify --deep --strict "$DEST"
  echo "installed: $DEST (signer $SIGNER, nonce ${PROBE_NONCE:-0})"
else
  echo "built: $APP (not installed)"
fi
