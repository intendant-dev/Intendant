#!/usr/bin/env bash
#
# Deploy the hosted Intendant Connect production-alpha service.
#
# This script intentionally does not manage secrets. The deployed service should
# already have its daemon token and other runtime environment in systemd.
#
# Defaults match the current production-alpha instance:
#   https://connect.intendant.dev -> ubuntu@16.171.75.210
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CONNECT_HOST="${CONNECT_HOST:-16.171.75.210}"
CONNECT_SSH_USER="${CONNECT_SSH_USER:-ubuntu}"
CONNECT_SSH_KEY="${CONNECT_SSH_KEY:-$HOME/.ssh/intendant-connect-prod-alpha-ec2}"
CONNECT_REMOTE_SOURCE="${CONNECT_REMOTE_SOURCE:-/opt/intendant/source}"
CONNECT_SERVICE="${CONNECT_SERVICE:-intendant-connect}"
CONNECT_PUBLIC_ORIGIN="${CONNECT_PUBLIC_ORIGIN:-https://connect.intendant.dev}"
CONNECT_REMOTE_READYZ_URL="${CONNECT_REMOTE_READYZ_URL:-http://127.0.0.1:8787/readyz}"
CONNECT_PUBLIC_READYZ_URL="${CONNECT_PUBLIC_READYZ_URL:-$CONNECT_PUBLIC_ORIGIN/readyz}"

SKIP_BUILD=false
SKIP_RESTART=false

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf ':: %s\n' "$*"; }

usage() {
    cat <<EOF
Usage: scripts/deploy-connect-prod-alpha.sh [options]

Options:
  --host <host>              SSH host. Default: $CONNECT_HOST
  --ssh-user <user>          SSH user. Default: $CONNECT_SSH_USER
  --ssh-key <path>           SSH key. Default: $CONNECT_SSH_KEY
  --remote-source <path>     Remote source directory. Default: $CONNECT_REMOTE_SOURCE
  --service <name>           systemd service. Default: $CONNECT_SERVICE
  --public-origin <url>      Public origin. Default: $CONNECT_PUBLIC_ORIGIN
  --remote-readyz-url <url>  Remote readiness URL. Default: $CONNECT_REMOTE_READYZ_URL
  --public-readyz-url <url>  Public readiness URL. Default: $CONNECT_PUBLIC_READYZ_URL
  --skip-build               Sync source and restart without cargo build
  --skip-restart             Sync/build only
  -h, --help                 Show this help

Environment variables with the same CONNECT_* names override defaults.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) CONNECT_HOST="${2:-}"; shift 2 ;;
        --ssh-user) CONNECT_SSH_USER="${2:-}"; shift 2 ;;
        --ssh-key) CONNECT_SSH_KEY="${2:-}"; shift 2 ;;
        --remote-source) CONNECT_REMOTE_SOURCE="${2:-}"; shift 2 ;;
        --service) CONNECT_SERVICE="${2:-}"; shift 2 ;;
        --public-origin) CONNECT_PUBLIC_ORIGIN="${2:-}"; CONNECT_PUBLIC_READYZ_URL="$CONNECT_PUBLIC_ORIGIN/readyz"; shift 2 ;;
        --remote-readyz-url) CONNECT_REMOTE_READYZ_URL="${2:-}"; shift 2 ;;
        --public-readyz-url) CONNECT_PUBLIC_READYZ_URL="${2:-}"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --skip-restart) SKIP_RESTART=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

[[ -n "$CONNECT_HOST" ]] || die "--host is required"
[[ -n "$CONNECT_SSH_USER" ]] || die "--ssh-user is required"
[[ -n "$CONNECT_REMOTE_SOURCE" ]] || die "--remote-source is required"
[[ -n "$CONNECT_SERVICE" ]] || die "--service is required"
[[ -f "$CONNECT_SSH_KEY" ]] || die "SSH key not found: $CONNECT_SSH_KEY"
command -v ssh >/dev/null 2>&1 || die "ssh is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

SSH_DEST="$CONNECT_SSH_USER@$CONNECT_HOST"
SSH_OPTS=(
    -i "$CONNECT_SSH_KEY"
    -o IdentitiesOnly=yes
    -o StrictHostKeyChecking=accept-new
)

remote_quote() {
    printf "%q" "$1"
}

REMOTE_SOURCE_Q="$(remote_quote "$CONNECT_REMOTE_SOURCE")"
REMOTE_SERVICE_Q="$(remote_quote "$CONNECT_SERVICE")"
REMOTE_READYZ_Q="$(remote_quote "$CONNECT_REMOTE_READYZ_URL")"

info "preparing $SSH_DEST:$CONNECT_REMOTE_SOURCE"
ssh "${SSH_OPTS[@]}" "$SSH_DEST" "sudo install -d -o \"$CONNECT_SSH_USER\" -g \"$CONNECT_SSH_USER\" $REMOTE_SOURCE_Q"

info "syncing source from $REPO_ROOT"
export COPYFILE_DISABLE=1
tar -C "$REPO_ROOT" \
    --format ustar \
    --exclude='.git' \
    --exclude='target' \
    --exclude='.env' \
    --exclude='.intendant' \
    --exclude='.DS_Store' \
    --exclude='._*' \
    --exclude='*.log' \
    -czf - . \
  | ssh "${SSH_OPTS[@]}" "$SSH_DEST" "tar -xzf - -C $REMOTE_SOURCE_Q"

if [[ "$SKIP_BUILD" == false ]]; then
    info "building intendant-connect on $SSH_DEST"
    ssh "${SSH_OPTS[@]}" "$SSH_DEST" "bash -lc 'set -euo pipefail; cd $REMOTE_SOURCE_Q; cargo build --release --bin intendant-connect'"
fi

if [[ "$SKIP_RESTART" == false ]]; then
    info "restarting $CONNECT_SERVICE"
    ssh "${SSH_OPTS[@]}" "$SSH_DEST" "bash -lc 'set -euo pipefail; sudo systemctl restart $REMOTE_SERVICE_Q; sudo systemctl is-active --quiet $REMOTE_SERVICE_Q; curl -fsS $REMOTE_READYZ_Q >/dev/null'"
fi

info "checking public readiness at $CONNECT_PUBLIC_READYZ_URL"
curl -fsS "$CONNECT_PUBLIC_READYZ_URL" >/dev/null

info "deploy complete"
