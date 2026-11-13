#!/bin/bash
# Makes the macOS release signing identity once (UPDATES.md 7, D53): a self-signed code-signing certificate named
# "Swapdesk Release" and nothing else (no organisation, no e-mail: the certificate is shipped inside every release),
# in its own keychain FILE, plus the PKCS#12 export that becomes the macOS job's GitHub secret.
#
#   bash packaging/macos/make-release-cert.sh
#
# Output in ~/.swapdesk-release-signing (0700): release.keychain-db and keychain-password (for local release builds:
# SWAPDESK_RELEASE_KEYCHAIN and SWAPDESK_RELEASE_KEYCHAIN_PASSWORD), release.p12 and p12-password (the two GitHub
# secrets MACOS_RELEASE_P12, base64 of release.p12, and MACOS_RELEASE_P12_PASSWORD). It refuses to run when the folder
# already holds an identity: a new certificate would change the designated requirement and every installed Mac app
# would lose its permissions at the next update. The keychain is never the login keychain nor in the search list.
set -euo pipefail

dir="$HOME/.swapdesk-release-signing"
keychain="$dir/release.keychain-db"
[ ! -e "$keychain" ] && [ ! -e "$dir/release.p12" ] || { echo "a release identity already exists in $dir: refusing to make another" >&2; exit 1; }
mkdir -p "$dir" && chmod 700 "$dir"
work="$(mktemp -d "${TMPDIR:-/tmp}/swapdesk-release-cert.XXXXXX")"
trap 'rm -rf "$work"' EXIT
( umask 077; /usr/bin/openssl rand -hex 24 > "$dir/keychain-password"; /usr/bin/openssl rand -hex 24 > "$dir/p12-password" )
cat > "$work/openssl.cnf" <<'CNF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = Swapdesk Release
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
# The system's LibreSSL: OpenSSL 3 writes PKCS#12 files that `security import` rejects (scripts/mac/build-app.sh).
/usr/bin/openssl req -x509 -newkey rsa:3072 -nodes -days 7300 -keyout "$work/id.key" -out "$work/id.crt" -config "$work/openssl.cnf" 2>/dev/null
PW="$(cat "$dir/p12-password")" /usr/bin/openssl pkcs12 -export -inkey "$work/id.key" -in "$work/id.crt" -name "Swapdesk Release" \
  -out "$dir/release.p12" -passout env:PW
chmod 600 "$dir/release.p12"
kpw="$(cat "$dir/keychain-password")"
security create-keychain -p "$kpw" "$keychain"
security set-keychain-settings "$keychain"
security unlock-keychain -p "$kpw" "$keychain"
security import "$dir/release.p12" -k "$keychain" -P "$(cat "$dir/p12-password")" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$kpw" "$keychain" >/dev/null
subject="$(/usr/bin/openssl x509 -in "$work/id.crt" -noout -subject)"
echo "made the release identity in $dir ($subject)"
