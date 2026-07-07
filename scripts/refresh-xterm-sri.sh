#!/usr/bin/env bash
# Re-vendor the pinned xterm.js assets into static/.
#
# The dashboard serves xterm from embedded copies (static/xterm.min.js,
# static/xterm-addon-fit.min.js, static/xterm.css — see
# web_gateway/static_assets.rs); nothing is fetched from a CDN at runtime.
# When bumping the pinned version: edit the versions below, run this
# script, compare the printed SHA-384 digests against the upstream
# release (npm tarball or jsdelivr's own SRI), then commit the new files
# together with any loader changes in static/app/44-shell-frames.js and
# static/app/23-voice-dialogs.html.
set -euo pipefail

XTERM_VERSION="5.5.0"
ADDON_FIT_VERSION="0.11.0"

cd "$(dirname "$0")/../static"

curl -fsS -o xterm.min.js \
  "https://cdn.jsdelivr.net/npm/@xterm/xterm@${XTERM_VERSION}/lib/xterm.min.js"
curl -fsS -o xterm-addon-fit.min.js \
  "https://cdn.jsdelivr.net/npm/@xterm/addon-fit@${ADDON_FIT_VERSION}/lib/addon-fit.min.js"
curl -fsS -o xterm.css \
  "https://cdn.jsdelivr.net/npm/@xterm/xterm@${XTERM_VERSION}/css/xterm.css"

for f in xterm.min.js xterm-addon-fit.min.js xterm.css; do
  echo "$f sha384-$(openssl dgst -sha384 -binary "$f" | openssl base64 -A)"
done
