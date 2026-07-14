#!/usr/bin/env bash
#
# Deploy the hosted Intendant Connect production-alpha service.
#
# This script intentionally does not manage secrets. The deployed service should
# already have its daemon token and other runtime environment in systemd.
#
# Target details are intentionally not stored in this public repository. Pass
# them with CONNECT_* environment variables, command-line flags, or a private
# env file referenced by CONNECT_OPS_ENV.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf ':: %s\n' "$*"; }

CONNECT_OPS_ENV="${CONNECT_OPS_ENV:-}"
if [[ -n "$CONNECT_OPS_ENV" ]]; then
    [[ -f "$CONNECT_OPS_ENV" ]] || die "CONNECT_OPS_ENV not found: $CONNECT_OPS_ENV"
    set -a
    # shellcheck disable=SC1090
    source "$CONNECT_OPS_ENV"
    set +a
fi

CONNECT_HOST="${CONNECT_HOST:-}"
CONNECT_SSH_USER="${CONNECT_SSH_USER:-}"
CONNECT_SSH_KEY="${CONNECT_SSH_KEY:-}"
CONNECT_REMOTE_SOURCE="${CONNECT_REMOTE_SOURCE:-}"
CONNECT_SERVICE="${CONNECT_SERVICE:-}"
CONNECT_PUBLIC_ORIGIN="${CONNECT_PUBLIC_ORIGIN:-https://connect.intendant.dev}"
CONNECT_REMOTE_READYZ_URL="${CONNECT_REMOTE_READYZ_URL:-}"
CONNECT_PUBLIC_READYZ_URL="${CONNECT_PUBLIC_READYZ_URL:-$CONNECT_PUBLIC_ORIGIN/readyz}"

SKIP_BUILD=false
SKIP_RESTART=false

usage() {
    cat <<EOF
Usage: scripts/deploy-connect-prod-alpha.sh [options]

Options:
  --host <host>              SSH host. Required unless CONNECT_HOST is set
  --ssh-user <user>          SSH user. Required unless CONNECT_SSH_USER is set
  --ssh-key <path>           SSH key. Required unless CONNECT_SSH_KEY is set
  --remote-source <path>     Remote source directory. Required unless CONNECT_REMOTE_SOURCE is set
  --service <name>           systemd service. Required unless CONNECT_SERVICE is set
  --public-origin <url>      Public origin. Default: $CONNECT_PUBLIC_ORIGIN
  --remote-readyz-url <url>  Remote readiness URL. Required unless CONNECT_REMOTE_READYZ_URL is set
  --public-readyz-url <url>  Public readiness URL. Default: $CONNECT_PUBLIC_READYZ_URL
  --skip-build               Sync source and restart without cargo build
  --skip-restart             Sync/build only
  -h, --help                 Show this help

CONNECT_OPS_ENV may point to a private env file containing these CONNECT_* values.
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
[[ -n "$CONNECT_REMOTE_READYZ_URL" ]] || die "--remote-readyz-url is required"
[[ -n "$CONNECT_SSH_KEY" ]] || die "--ssh-key is required"
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

# Clear the previous source tree (keeping target/ warm) before extracting:
# tar layers over what's there, so a file the repo deleted or carved into a
# directory would otherwise linger remotely forever — a stale module file
# next to its carved directory fails the remote build with E0761.
info "clearing stale remote source (preserving target/)"
ssh "${SSH_OPTS[@]}" "$SSH_DEST" "find $REMOTE_SOURCE_Q -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +"

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

# Regression probe for the whole reverse-proxy chain: register a throwaway
# daemon through the public origin and require the response to echo the
# caller's address. This is route/reachability metadata only in the default
# discovery-only build. The deliberately unsigned probe is an operator action,
# so skip it when the deployment bearer is not present in this shell.
if [[ -z "${INTENDANT_CONNECT_TOKEN:-}" ]]; then
    info "skipping observed_ip operator probe (INTENDANT_CONNECT_TOKEN is unset)"
else
    info "checking observed_ip echo at $CONNECT_PUBLIC_ORIGIN"
    PROBE_RESPONSE="$(curl \
        -fsS -X POST "$CONNECT_PUBLIC_ORIGIN/api/daemon/register" \
        -H 'content-type: application/json' \
        -H "Authorization: Bearer $INTENDANT_CONNECT_TOKEN" \
        -d '{"protocol":"intendant-connect-rendezvous-v1","daemon_id":"deploy-observed-ip-probe","daemon_public_key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","claim_code_hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","issued_at_unix_ms":0,"signature":""}')"
    if ! grep -qE '"observed_ip":"[0-9a-fA-F:.]+"' <<<"$PROBE_RESPONSE"; then
        die "register response did not echo the caller address (observed_ip) — the reverse proxy in front of the service is not forwarding X-Forwarded-For/X-Real-IP. See the Reachability section of docs/src/self-hosted-rendezvous.md (Caddy applies header_up deletions after sets — do not strip-then-set). Response: $PROBE_RESPONSE"
    fi
fi

info "deploy complete"
