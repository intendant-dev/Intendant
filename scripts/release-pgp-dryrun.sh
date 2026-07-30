#!/usr/bin/env bash
# End-to-end LOCAL dry-run of the PGP release lane — no tag, no GitHub, no
# fleet runner. Exercises the exact machinery release.yml uses:
#
#   1. stages a representative dist/ (app-archive stand-in + the real
#      install scripts + .sha256 sidecars, exactly the shapes a tag build
#      stages);
#   2. signs it with scripts/release-pgp-sign.sh (the same script the
#      workflow runs) using the escrowed release signing subkey;
#   3. builds the release manifest with the same construction the
#      workflow's submit step uses (twin: keep in sync with release.yml)
#      and submits it to a locally-launched intendant-connect started
#      with --release-token — the log door's fail-closed validation runs
#      for real;
#   4. proves the log door rejects an unsigned manifest (negative);
#   5. runs `intendant hosted-verify --releases <tag> --download` against
#      the local rendezvous and a local mock of the GitHub releases API
#      serving dist/'s real names/sizes/digests/bytes;
#   6. runs the documented user ritual: gpg --verify from a clean keyring
#      holding only the repo-committed RELEASE-SIGNING-KEY.asc — and
#      proves a tampered artifact fails it (negative).
#
# Inputs: target/debug/{intendant,intendant-connect} (or INTENDANT_BIN /
# CONNECT_BIN), gpg, python3, and the release-key escrow
# (~/.intendant/release-signing/, read-only; override RELEASE_SIGNING_ESCROW).
# Everything else lives under a mktemp dir and is removed on exit.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INTENDANT_BIN="${INTENDANT_BIN:-$REPO_ROOT/target/debug/intendant}"
CONNECT_BIN="${CONNECT_BIN:-$REPO_ROOT/target/debug/intendant-connect}"
ESCROW="${RELEASE_SIGNING_ESCROW:-$HOME/.intendant/release-signing}"
TAG="v0.0.0-dryrun"

for bin in "$INTENDANT_BIN" "$CONNECT_BIN"; do
    [ -x "$bin" ] || {
        echo "error: $bin not built — run: cargo build --bin intendant --bin intendant-connect" >&2
        exit 1
    }
done
command -v gpg > /dev/null || { echo "error: gpg not installed" >&2; exit 1; }
command -v python3 > /dev/null || { echo "error: python3 not installed" >&2; exit 1; }
for f in secret-subkey-ci.asc passphrase; do
    [ -f "$ESCROW/$f" ] || { echo "error: escrow $ESCROW is missing $f (see its README)" >&2; exit 1; }
done

WORK="$(mktemp -d)"
CONNECT_PID="" MOCK_PID=""
cleanup() {
    [ -n "$CONNECT_PID" ] && kill "$CONNECT_PID" 2> /dev/null || true
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2> /dev/null || true
    gpgconf --homedir "$WORK/pgp-home" --kill gpg-agent 2> /dev/null || true
    gpgconf --homedir "$WORK/ritual-home" --kill gpg-agent 2> /dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

free_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

echo "== 1/6 staging dist/ =="
DIST="$WORK/dist"
mkdir -p "$DIST" "$WORK/payload"
cp "$REPO_ROOT/scripts/install.sh" "$WORK/payload/"
ZIP_NAME="Intendant-$TAG-macos-arm64-unsigned-dev.zip"
(cd "$WORK/payload" && zip -q "$DIST/$ZIP_NAME" install.sh)
cp "$REPO_ROOT/scripts/install.sh" "$REPO_ROOT/scripts/install.ps1" "$DIST/"
(cd "$DIST" && shasum -a 256 "$ZIP_NAME" > "$ZIP_NAME.sha256")
(cd "$DIST" && shasum -a 256 install.sh > install.sh.sha256)
(cd "$DIST" && shasum -a 256 install.ps1 > install.ps1.sha256)

echo "== 2/6 signing with the escrowed subkey (scripts/release-pgp-sign.sh) =="
umask 077
PGP_HOME="$WORK/pgp-home"
mkdir -p "$PGP_HOME"
chmod 700 "$PGP_HOME"
echo "allow-loopback-pinentry" > "$PGP_HOME/gpg-agent.conf"
gpg --batch --homedir "$PGP_HOME" --import "$ESCROW/secret-subkey-ci.asc" 2> /dev/null
PGP_SIGN_HOME="$PGP_HOME" PGP_SIGN_PASSPHRASE_FILE="$ESCROW/passphrase" \
    "$REPO_ROOT/scripts/release-pgp-sign.sh" "$DIST" "$WORK/fingerprint"
FINGERPRINT="$(cat "$WORK/fingerprint")"

echo "== 3/6 submitting the release manifest to a local rendezvous =="
CONNECT_PORT="$(free_port)"
RELEASE_TOKEN="dryrun-$(date +%s)"
"$CONNECT_BIN" --listen "127.0.0.1:$CONNECT_PORT" \
    --data-file "$WORK/connect-data.json" \
    --release-token "$RELEASE_TOKEN" > "$WORK/connect.log" 2>&1 &
CONNECT_PID=$!
for _ in $(seq 1 50); do
    curl -fsS "http://127.0.0.1:$CONNECT_PORT/api/log/sth" > /dev/null 2>&1 && break
    kill -0 "$CONNECT_PID" 2> /dev/null || { echo "error: intendant-connect died:" >&2; cat "$WORK/connect.log" >&2; exit 1; }
    sleep 0.2
done
# Twin of release.yml's "Submit release manifest" construction — keep the
# two in sync (same hashing, same fields).
python3 - "$TAG" "$FINGERPRINT" "$DIST"/* > "$WORK/release-manifest.json" <<'PY'
import hashlib, json, os, sys
tag, fingerprint, *paths = sys.argv[1:]
artifacts = []
for path in sorted(paths, key=os.path.basename):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    artifacts.append({
        "name": os.path.basename(path),
        "sha256": digest.hexdigest(),
        "size": os.path.getsize(path),
    })
print(json.dumps({
    "tag": tag,
    "version": tag.removeprefix("v"),
    "platforms": ["macos-arm64"],
    "pgp_fingerprint": fingerprint,
    "artifacts": artifacts,
}, indent=2))
PY
curl -sS --fail-with-body -X POST \
    "http://127.0.0.1:$CONNECT_PORT/api/log/release-manifest" \
    -H "Authorization: Bearer $RELEASE_TOKEN" \
    -H "Content-Type: application/json" \
    --data-binary @"$WORK/release-manifest.json"
echo

echo "== 4/6 negative: the log door refuses an unsigned manifest =="
python3 - "$WORK/release-manifest.json" > "$WORK/unsigned-manifest.json" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
manifest["artifacts"] = [a for a in manifest["artifacts"] if not a["name"].endswith(".asc")]
print(json.dumps(manifest))
PY
STATUS="$(curl -s -o "$WORK/unsigned-reply.json" -w '%{http_code}' -X POST \
    "http://127.0.0.1:$CONNECT_PORT/api/log/release-manifest" \
    -H "Authorization: Bearer $RELEASE_TOKEN" \
    -H "Content-Type: application/json" \
    --data-binary @"$WORK/unsigned-manifest.json")"
if [ "$STATUS" != "400" ]; then
    echo "error: unsigned manifest was not refused (HTTP $STATUS):" >&2
    cat "$WORK/unsigned-reply.json" >&2
    exit 1
fi
echo "refused as expected (HTTP 400): $(cat "$WORK/unsigned-reply.json")"

echo "== 5/6 hosted-verify --releases against the local log + mock GitHub API =="
MOCK_PORT="$(free_port)"
python3 - "$DIST" "$MOCK_PORT" "$TAG" > "$WORK/mock-github.log" 2>&1 <<'PY' &
import hashlib, http.server, json, os, sys
dist, port, tag = sys.argv[1], int(sys.argv[2]), sys.argv[3]
def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()
assets = [
    {
        "name": name,
        "size": os.path.getsize(os.path.join(dist, name)),
        "digest": "sha256:" + sha256_of(os.path.join(dist, name)),
        "browser_download_url": f"http://127.0.0.1:{port}/dl/{name}",
    }
    for name in sorted(os.listdir(dist))
    if os.path.isfile(os.path.join(dist, name))
]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def reply(self, code, body):
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        if self.path == f"/repos/intendant-dev/Intendant/releases/tags/{tag}":
            self.reply(200, json.dumps({"tag_name": tag, "assets": assets}).encode())
        elif self.path.startswith("/dl/"):
            path = os.path.join(dist, os.path.basename(self.path[4:]))
            if os.path.isfile(path):
                self.reply(200, open(path, "rb").read())
            else:
                self.reply(404, b"not found")
        else:
            self.reply(404, b"not found")
http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
MOCK_PID=$!
sleep 0.5
INTENDANT_HOME="$WORK/ihome" "$INTENDANT_BIN" hosted-verify --releases "$TAG" --download \
    --connect "http://127.0.0.1:$CONNECT_PORT" \
    --github-api "http://127.0.0.1:$MOCK_PORT" \
    --repo intendant-dev/Intendant

echo "== 6/6 the documented gpg ritual (clean keyring, committed key only) =="
RITUAL_HOME="$WORK/ritual-home"
mkdir -p "$RITUAL_HOME"
chmod 700 "$RITUAL_HOME"
gpg --batch --homedir "$RITUAL_HOME" --import "$REPO_ROOT/RELEASE-SIGNING-KEY.asc" 2> /dev/null
gpg --batch --homedir "$RITUAL_HOME" --verify "$DIST/$ZIP_NAME.asc" "$DIST/$ZIP_NAME"
cp "$DIST/$ZIP_NAME" "$WORK/tampered.zip"
printf 'x' >> "$WORK/tampered.zip"
if gpg --batch --homedir "$RITUAL_HOME" --verify "$DIST/$ZIP_NAME.asc" "$WORK/tampered.zip" 2> /dev/null; then
    echo "error: gpg accepted a tampered artifact" >&2
    exit 1
fi
echo "tampered artifact refused by gpg, as expected"

echo
echo "DRY-RUN PASS — signed artifacts, a logged manifest (unsigned refused), a"
echo "passing hosted-verify --releases --download, and a passing gpg ritual,"
echo "end to end with key $FINGERPRINT"
