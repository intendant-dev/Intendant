#!/usr/bin/env bash
# Build the scoped-human mTLS rig: isolated HOME with a daemon CA (via
# `intendant access setup`), plus two extra client certificates minted from
# that CA with openssl — one to bind as role:files-write, one as
# role:operator. The setup-minted client.crt/key stays the unbound owner
# (root) identity.
set -euo pipefail

RIG=${RIG:-/tmp/scoped-human-rig}
BIN=${BIN:-$PWD/target/release/intendant}
PORT=${PORT:-18820}

rm -rf "$RIG"
mkdir -p "$RIG"/{home,proj,files,outside}
printf 'inside the grant\n' > "$RIG/files/seed.txt"
printf 'secret outside the grant\n' > "$RIG/outside/secret.txt"

HOME="$RIG/home" "$BIN" access setup --name mtls-rig --port "$PORT" --ip 127.0.0.1 --no-serve-certs

CERTS="$RIG/home/.intendant/access-certs"

# webpki accepts only v3 certificates with a clientAuth EKU — a bare
# `openssl x509 -req` mints v1, which the daemon's verifier alerts on.
cat > "$RIG/client-ext.cnf" <<'EXT'
basicConstraints = CA:FALSE
keyUsage = digitalSignature
extendedKeyUsage = clientAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid,issuer
EXT

# RSA to match the key type `access setup` itself mints — EC P-256 client
# keys trip TLS-stack quirks on macOS clients (LibreSSL curl cannot even
# load them), and cipher diversity is not what this rig is testing.
mint() {
  local name=$1 cn=$2
  openssl req -new -newkey rsa:2048 -nodes \
    -keyout "$RIG/$name.key" -subj "/CN=$cn" -out "$RIG/$name.csr" 2>/dev/null
  openssl x509 -req -in "$RIG/$name.csr" -CA "$CERTS/ca.crt" -CAkey "$CERTS/ca.key" \
    -CAcreateserial -days 7 -sha256 -extfile "$RIG/client-ext.cnf" \
    -out "$RIG/$name.crt" 2>/dev/null
  # Hex sha256 of the DER — the daemon's fingerprint_der format.
  openssl x509 -in "$RIG/$name.crt" -outform DER | shasum -a 256 | awk '{print $1}' > "$RIG/$name.fp"
}

mint scoped scoped-human
mint operator operator-human

echo "rig ready: $RIG (scoped fp $(cat "$RIG/scoped.fp"), operator fp $(cat "$RIG/operator.fp"))"
