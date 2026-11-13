#!/bin/bash
# Seals the macOS release identity for the GitHub Actions secrets the release workflow reads (MACOS_RELEASE_P12, base64
# of release.p12, and MACOS_RELEASE_P12_PASSWORD), on the Mac that ran make-release-cert.sh. Each value is sealed to
# the repository's Actions public key (a libsodium sealed box, as GitHub requires) inside a throwaway Python venv with
# PyNaCl, removed at exit; only the ciphertext is printed, as the JSON call list for `github-repo-config <repo> <file>`.
#
#   bash packaging/macos/seal-release-secrets.sh <key_id> <base64 public key> > secrets.json
#
# key_id and key come from GET /repos/<owner>/<repo>/actions/secrets/public-key. The identity folder defaults to
# ~/.swapdesk-release-signing (SWAPDESK_RELEASE_DIR overrides it).
set -euo pipefail

key_id="${1:?usage: seal-release-secrets.sh <key_id> <public key base64>}"
key="${2:?usage: seal-release-secrets.sh <key_id> <public key base64>}"
dir="${SWAPDESK_RELEASE_DIR:-$HOME/.swapdesk-release-signing}"
[ -f "$dir/release.p12" ] && [ -f "$dir/p12-password" ] || { echo "no release identity in $dir" >&2; exit 1; }

venv="$(mktemp -d "${TMPDIR:-/tmp}/swapdesk-seal.XXXXXX")"
trap 'rm -rf "$venv"' EXIT
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
