#!/bin/bash
# Seals the macOS release identity for the GitHub Actions secrets the release workflow reads (MACOS_RELEASE_P12, base64
# of release.p12, and MACOS_RELEASE_P12_PASSWORD), on the Mac that ran make-release-cert.sh. Each value is sealed to
# the repository's Actions public key (a libsodium sealed box, as GitHub requires) inside a throwaway Python venv with
# PyNaCl, removed at exit; only the ciphertext is printed, as the JSON call list for `github-repo-config <repo> <file>`.
#
#   bash packaging/macos/seal-release-secrets.sh <key_id> <base64 public key> > secrets.json
#   bash packaging/macos/seal-release-secrets.sh --rehearse      only the rehearsal below
#
# Before sealing it rehearses the runner's import and signature with the same files, and seals nothing if they fail.
#
# key_id and key come from GET /repos/<owner>/<repo>/actions/secrets/public-key. The identity folder defaults to
# ~/.swapdesk-release-signing (SWAPDESK_RELEASE_DIR overrides it).
set -euo pipefail

dir="${SWAPDESK_RELEASE_DIR:-$HOME/.swapdesk-release-signing}"
[ -f "$dir/release.p12" ] && [ -f "$dir/p12-password" ] || { echo "no release identity in $dir" >&2; exit 1; }

venv="$(mktemp -d "${TMPDIR:-/tmp}/swapdesk-seal.XXXXXX")"
trap 'security delete-keychain "$venv/rehearsal.keychain-db" 2>/dev/null; rm -rf "$venv"' EXIT

# The rehearsal: what build-release.sh does with the secrets on the runner (a throwaway keychain, the PKCS#12 imported,
# codesign by the certificate's hash), on a copy of /usr/bin/true. Nothing is sealed unless it signs.
rehearse() {
  local kc="$venv/rehearsal.keychain-db" kpw hash
  kpw="$(/usr/bin/openssl rand -hex 24)"
  security create-keychain -p "$kpw" "$kc"
  security set-keychain-settings "$kc"
  security unlock-keychain -p "$kpw" "$kc"
  security import "$dir/release.p12" -f pkcs12 -k "$kc" -P "$(cat "$dir/p12-password")" -T /usr/bin/codesign >&2
  security set-key-partition-list -S apple-tool:,apple: -s -k "$kpw" "$kc" >/dev/null
  hash="$(security find-certificate -c "Swapdesk Release" -Z "$kc" | sed -n 's/^SHA-1 hash: *//p' | head -1)"
  echo "rehearsal: certificate $hash; identities:" >&2
  security find-identity "$kc" >&2
  cp /usr/bin/true "$venv/probe"
  codesign --force --keychain "$kc" --sign "$hash" --timestamp=none "$venv/probe" >&2
  echo "rehearsal: signed" >&2
}
if [ "${1:-}" = --rehearse ]; then
  rehearse
  exit 0
fi
key_id="${1:?usage: seal-release-secrets.sh <key_id> <public key base64> | --rehearse}"
key="${2:?usage: seal-release-secrets.sh <key_id> <public key base64> | --rehearse}"
rehearse
/usr/bin/python3 -m venv "$venv" >&2
"$venv/bin/pip" install -q pynacl >&2

"$venv/bin/python" -I - "$key_id" "$key" "$dir" <<'PY'
import base64, json, sys
from pathlib import Path
from nacl import encoding, public

key_id, key, folder = sys.argv[1], sys.argv[2], Path(sys.argv[3])
box = public.SealedBox(public.PublicKey(key.encode(), encoding.Base64Encoder))
def seal(value: bytes) -> str:
    return base64.b64encode(box.encrypt(value)).decode()
identity = base64.b64encode((folder / "release.p12").read_bytes())
password = (folder / "p12-password").read_bytes().strip()
calls = [
    {"method": "PUT", "path": "/actions/secrets/MACOS_RELEASE_P12",
     "body": {"encrypted_value": seal(identity), "key_id": key_id}},
    {"method": "PUT", "path": "/actions/secrets/MACOS_RELEASE_P12_PASSWORD",
     "body": {"encrypted_value": seal(password), "key_id": key_id}},
]
print(json.dumps(calls))
PY
