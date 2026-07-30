#!/usr/bin/env bash
# Detach-sign every release artifact with the Intendant release signing key,
# stage the repo-committed public key beside them, and self-verify each
# signature against exactly that committed key before anything publishes.
#
# Usage: release-pgp-sign.sh <dist-dir> <fingerprint-out-file>
#
# Environment (release.yml's "Import PGP release signing key" step, or a
# local dry-run rig, provides both):
#   PGP_SIGN_HOME             GNUPGHOME holding the imported secret signing
#                             subkey (never the primary key — the escrow
#                             README's custody design)
#   PGP_SIGN_PASSPHRASE_FILE  file carrying that subkey's passphrase
#
# Every non-.asc file in <dist-dir> gets a detached armored `<name>.asc`;
# the committed RELEASE-SIGNING-KEY.asc is copied in beside them; then a
# throwaway verify-only GNUPGHOME imports ONLY the committed public key and
# verifies every signature — a CI key that is not the committed key's
# signing subkey fails here, before the publish step could ship a release
# whose own documented ritual would reject it. The committed key's primary
# fingerprint (uppercase hex) is written to <fingerprint-out-file> for the
# release-manifest submission; `intendant hosted-verify` pins the same
# fingerprint at compile time (src/bin/caller/pgp_identity.rs).
set -euo pipefail

DIST_DIR="${1:?usage: release-pgp-sign.sh <dist-dir> <fingerprint-out-file>}"
FINGERPRINT_OUT="${2:?usage: release-pgp-sign.sh <dist-dir> <fingerprint-out-file>}"
: "${PGP_SIGN_HOME:?PGP_SIGN_HOME must name the GNUPGHOME holding the release signing subkey}"
: "${PGP_SIGN_PASSPHRASE_FILE:?PGP_SIGN_PASSPHRASE_FILE must name the passphrase file}"

command -v gpg > /dev/null || {
    echo "error: gnupg is not installed on this machine (the release runner needs it)" >&2
    exit 1
}

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY_ASSET="RELEASE-SIGNING-KEY.asc"
KEY_FILE="$REPO_ROOT/$KEY_ASSET"
[ -f "$KEY_FILE" ] || { echo "error: $KEY_FILE is missing" >&2; exit 1; }

shopt -s nullglob
artifacts=()
for file in "$DIST_DIR"/*; do
    [ -f "$file" ] || continue
    case "$file" in *.asc) continue ;; esac
    artifacts+=("$file")
done
[ "${#artifacts[@]}" -gt 0 ] || { echo "error: no artifacts to sign in $DIST_DIR" >&2; exit 1; }

for file in "${artifacts[@]}"; do
    rm -f "$file.asc"
    gpg --batch --homedir "$PGP_SIGN_HOME" --pinentry-mode loopback \
        --passphrase-file "$PGP_SIGN_PASSPHRASE_FILE" \
        --armor --detach-sign --output "$file.asc" "$file"
done

# The published key is byte-identical to the reviewed, repo-committed one.
cp "$KEY_FILE" "$DIST_DIR/$KEY_ASSET"

# Self-verify in a clean home that trusts ONLY the committed public key.
VERIFY_HOME="$(mktemp -d)"
chmod 700 "$VERIFY_HOME"
cleanup() {
    gpgconf --homedir "$VERIFY_HOME" --kill gpg-agent 2> /dev/null || true
    rm -rf "$VERIFY_HOME"
}
trap cleanup EXIT
gpg --batch --homedir "$VERIFY_HOME" --import "$KEY_FILE" > /dev/null 2>&1 \
    || { echo "error: the committed $KEY_ASSET does not import as a public key" >&2; exit 1; }
for file in "${artifacts[@]}"; do
    gpg --batch --homedir "$VERIFY_HOME" --verify "$file.asc" "$file" || {
        echo "error: $file.asc does not verify against the repo-committed $KEY_ASSET —" \
            "the configured signing key is not the committed key's signing subkey" >&2
        exit 1
    }
done

# The primary fingerprint of the COMMITTED key: what the release manifest
# logs as pgp_fingerprint.
FINGERPRINT="$(gpg --homedir "$VERIFY_HOME" --with-colons --list-keys 2> /dev/null \
    | awk -F: '/^fpr/{print $10; exit}')"
[ -n "$FINGERPRINT" ] || { echo "error: could not read the committed key's fingerprint" >&2; exit 1; }
printf '%s' "$FINGERPRINT" > "$FINGERPRINT_OUT"

echo "PGP-signed ${#artifacts[@]} artifact(s) in $DIST_DIR with key $FINGERPRINT (self-verified against $KEY_ASSET)"
