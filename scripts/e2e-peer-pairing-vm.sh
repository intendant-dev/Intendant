#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

REMOTE="${INTENDANT_PEER_E2E_REMOTE:-vm@192.168.66.7}"
REMOTE_HOST="${INTENDANT_PEER_E2E_REMOTE_HOST:-}"
REMOTE_ROOT="${INTENDANT_PEER_E2E_REMOTE_ROOT:-/tmp/intendant-peer-e2e-src}"
LOCAL_BIN="${INTENDANT_PEER_E2E_LOCAL_BIN:-target/release/intendant}"
REMOTE_BIN="${INTENDANT_PEER_E2E_REMOTE_BIN:-}"
LOCAL_PORT="${INTENDANT_PEER_E2E_LOCAL_PORT:-}"
REMOTE_PORT="${INTENDANT_PEER_E2E_REMOTE_PORT:-}"
PROFILE="${INTENDANT_PEER_E2E_PROFILE:-peer-daemon}"
SYNC_REMOTE=1
BUILD_LOCAL=1
BUILD_REMOTE=1
KEEP_ARTIFACTS=0

usage() {
    cat <<'EOF'
Usage:
  scripts/e2e-peer-pairing-vm.sh [options]

Runs the VM-style peer pairing E2E:
  1. Build/sync current worktree.
  2. Create isolated local and remote access cert stores.
  3. Start the remote daemon with default TLS/mTLS.
  4. Send a public access request, approve it headlessly on the VM, and complete it locally.
  5. Start a local dashboard daemon and wait until /api/peers reports the VM connected.

Options:
  --remote USER@HOST       SSH target (default: vm@192.168.66.7)
  --remote-host HOST       Host/IP used in HTTPS URLs (default: host part of --remote)
  --remote-root PATH       Remote source/build directory (default: /tmp/intendant-peer-e2e-src)
  --local-bin PATH         Local intendant binary (default: target/release/intendant)
  --remote-bin PATH        Remote intendant binary (default: <remote-root>/target/release/intendant)
  --local-port PORT        Local dashboard port (default: auto)
  --remote-port PORT       Remote dashboard port (default: auto on VM)
  --profile NAME           Requested/approved access profile (default: peer-daemon)
  --no-sync                Do not sync this worktree to the VM
  --skip-local-build       Do not run cargo build locally
  --skip-remote-build      Do not run cargo build on the VM
  --keep-artifacts         Keep temp homes/logs even after a successful run
  -h, --help               Show this help

Environment variables with the INTENDANT_PEER_E2E_ prefix mirror these options.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote)
            REMOTE="${2:?--remote requires a value}"
            shift 2
            ;;
        --remote-host)
            REMOTE_HOST="${2:?--remote-host requires a value}"
            shift 2
            ;;
        --remote-root)
            REMOTE_ROOT="${2:?--remote-root requires a value}"
            shift 2
            ;;
        --local-bin)
            LOCAL_BIN="${2:?--local-bin requires a value}"
            shift 2
            ;;
        --remote-bin)
            REMOTE_BIN="${2:?--remote-bin requires a value}"
            shift 2
            ;;
        --local-port)
            LOCAL_PORT="${2:?--local-port requires a value}"
            shift 2
            ;;
        --remote-port)
            REMOTE_PORT="${2:?--remote-port requires a value}"
            shift 2
            ;;
        --profile)
            PROFILE="${2:?--profile requires a value}"
            shift 2
            ;;
        --no-sync)
            SYNC_REMOTE=0
            shift
            ;;
        --skip-local-build)
            BUILD_LOCAL=0
            shift
            ;;
        --skip-remote-build)
            BUILD_REMOTE=0
            shift
            ;;
        --keep-artifacts)
            KEEP_ARTIFACTS=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$REMOTE_HOST" ]]; then
    REMOTE_HOST="${REMOTE#*@}"
    REMOTE_HOST="${REMOTE_HOST%%:*}"
fi

if [[ "$LOCAL_BIN" != /* ]]; then
    LOCAL_BIN="$ROOT_DIR/$LOCAL_BIN"
fi

if [[ -z "$REMOTE_BIN" ]]; then
    REMOTE_BIN="$REMOTE_ROOT/target/release/intendant"
fi

shell_quote() {
    python3 -c 'import shlex, sys; print(shlex.quote(sys.argv[1]))' "$1"
}

remote_bash() {
    local script=$1
    ssh "$REMOTE" "bash -lc $(shell_quote "$script")"
}

free_local_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

free_remote_port() {
    remote_bash 'python3 - <<'"'"'PY'"'"'
import socket
s = socket.socket()
s.bind(("0.0.0.0", 0))
print(s.getsockname()[1])
s.close()
PY'
}

wait_for_http() {
    local url=$1
    local name=$2
    local curl_flags=$3
    local code
    for _ in $(seq 1 60); do
        code=$(curl $curl_flags --max-time 2 -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || true)
        case "$code" in
            200|400|401|403|404)
                return 0
                ;;
        esac
        sleep 1
    done
    echo "timed out waiting for $name at $url" >&2
    return 1
}

LOCAL_WORK=""
REMOTE_WORK=""
LOCAL_DAEMON_PID=""

cleanup() {
    local status=$?
    set +e

    if [[ -n "$LOCAL_DAEMON_PID" ]]; then
        kill "$LOCAL_DAEMON_PID" >/dev/null 2>&1
        wait "$LOCAL_DAEMON_PID" >/dev/null 2>&1
    fi

    if [[ -n "$REMOTE_WORK" ]]; then
        remote_bash "
if [[ -f $(shell_quote "$REMOTE_WORK/remote-daemon.pid") ]]; then
  kill \$(cat $(shell_quote "$REMOTE_WORK/remote-daemon.pid")) >/dev/null 2>&1 || true
fi
"
    fi

    if [[ "$status" -eq 0 && "$KEEP_ARTIFACTS" -eq 0 ]]; then
        [[ -n "$LOCAL_WORK" ]] && rm -rf "$LOCAL_WORK"
        [[ -n "$REMOTE_WORK" ]] && remote_bash "rm -rf $(shell_quote "$REMOTE_WORK")"
    else
        [[ -n "$LOCAL_WORK" ]] && echo ":: local artifacts: $LOCAL_WORK" >&2
        [[ -n "$REMOTE_WORK" ]] && echo ":: remote artifacts: $REMOTE:$REMOTE_WORK" >&2
    fi

    exit "$status"
}
trap cleanup EXIT

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required command: $1" >&2
        exit 2
    }
}

require_cmd cargo
require_cmd curl
require_cmd python3
require_cmd ssh
if [[ "$SYNC_REMOTE" -eq 1 ]]; then
    require_cmd tar
fi

if [[ -z "$LOCAL_PORT" ]]; then
    LOCAL_PORT="$(free_local_port)"
fi
if [[ -z "$REMOTE_PORT" ]]; then
    REMOTE_PORT="$(free_remote_port)"
fi

echo ":: remote target: $REMOTE"
echo ":: remote URL host: $REMOTE_HOST"
echo ":: local port: $LOCAL_PORT"
echo ":: remote port: $REMOTE_PORT"

if [[ "$BUILD_LOCAL" -eq 1 ]]; then
    echo ":: building local intendant"
    (cd "$ROOT_DIR" && cargo build --release --bin intendant)
fi
if [[ ! -x "$LOCAL_BIN" ]]; then
    echo "local intendant binary not executable: $LOCAL_BIN" >&2
    exit 2
fi

if [[ "$SYNC_REMOTE" -eq 1 ]]; then
    echo ":: syncing worktree to $REMOTE:$REMOTE_ROOT"
    if command -v rsync >/dev/null 2>&1 && remote_bash "command -v rsync >/dev/null 2>&1"; then
        remote_bash "mkdir -p $(shell_quote "$REMOTE_ROOT")"
        rsync -az --delete \
            --exclude .git \
            --exclude .worktrees \
            --exclude target \
            --exclude .env \
            "$ROOT_DIR/" "$REMOTE:$REMOTE_ROOT/"
    else
        echo ":: rsync unavailable on one side; using tar-over-ssh sync"
        remote_bash "command -v tar >/dev/null 2>&1"
        tar \
            --exclude .git \
            --exclude .worktrees \
            --exclude target \
            --exclude .env \
            -czf - \
            -C "$ROOT_DIR" . \
            | remote_bash "
mkdir -p $(shell_quote "$REMOTE_ROOT")
tar -xzf - -C $(shell_quote "$REMOTE_ROOT")
"
    fi
fi

if [[ "$BUILD_REMOTE" -eq 1 ]]; then
    echo ":: building remote intendant"
    remote_bash "cd $(shell_quote "$REMOTE_ROOT") && cargo build --release --bin intendant"
fi

LOCAL_WORK="$(mktemp -d /tmp/intendant-peer-e2e-local.XXXXXX)"
REMOTE_WORK="$(remote_bash 'mktemp -d /tmp/intendant-peer-e2e-remote.XXXXXX')"
LOCAL_HOME="$LOCAL_WORK/home"
LOCAL_PROJECT="$LOCAL_WORK/project"
REMOTE_HOME="$REMOTE_WORK/home"
REMOTE_PROJECT="$REMOTE_WORK/project"
mkdir -p "$LOCAL_HOME" "$LOCAL_PROJECT"
: > "$LOCAL_PROJECT/intendant.toml"
remote_bash "mkdir -p $(shell_quote "$REMOTE_HOME") $(shell_quote "$REMOTE_PROJECT") && : > $(shell_quote "$REMOTE_PROJECT/intendant.toml")"

local_intendant() {
    (cd "$LOCAL_PROJECT" && HOME="$LOCAL_HOME" XDG_CONFIG_HOME="$LOCAL_HOME/.config" "$LOCAL_BIN" "$@")
}

if [[ "$REMOTE_HOST" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ || "$REMOTE_HOST" == *:* ]]; then
    REMOTE_ACCESS_NAME_ARGS="--ip $(shell_quote "$REMOTE_HOST")"
else
    REMOTE_ACCESS_NAME_ARGS="--host $(shell_quote "$REMOTE_HOST")"
fi

echo ":: generating isolated local access certs"
local_intendant access setup \
    --ip 127.0.0.1 \
    --host localhost \
    --name local-peer-e2e \
    --port "$LOCAL_PORT" \
    --no-serve-certs \
    --force > "$LOCAL_WORK/local-access-setup.log" 2>&1

echo ":: generating isolated remote access certs"
remote_bash "
cd $(shell_quote "$REMOTE_PROJECT")
HOME=$(shell_quote "$REMOTE_HOME") XDG_CONFIG_HOME=$(shell_quote "$REMOTE_HOME/.config") \
  $(shell_quote "$REMOTE_BIN") access setup \
  $REMOTE_ACCESS_NAME_ARGS \
  --name remote-peer-e2e \
  --port $(shell_quote "$REMOTE_PORT") \
  --no-serve-certs \
  --force > $(shell_quote "$REMOTE_WORK/remote-access-setup.log") 2>&1
"

echo ":: starting remote mTLS daemon"
remote_bash "
cd $(shell_quote "$REMOTE_PROJECT")
HOME=$(shell_quote "$REMOTE_HOME") XDG_CONFIG_HOME=$(shell_quote "$REMOTE_HOME/.config") \
  nohup $(shell_quote "$REMOTE_BIN") \
  --web $(shell_quote "$REMOTE_PORT") \
  --no-tui \
  --bind 0.0.0.0 \
  --advertise-url $(shell_quote "wss://$REMOTE_HOST:$REMOTE_PORT/ws") \
  > $(shell_quote "$REMOTE_WORK/remote-daemon.out") \
  2> $(shell_quote "$REMOTE_WORK/remote-daemon.err") &
echo \$! > $(shell_quote "$REMOTE_WORK/remote-daemon.pid")
"
wait_for_http "https://$REMOTE_HOST:$REMOTE_PORT/api/peer-pairing/requests/not-found" "remote public doorbell" "-sk"

echo ":: requesting access from local to remote"
REQUEST_OUT="$(
    local_intendant peer request "https://$REMOTE_HOST:$REMOTE_PORT" \
        --label local-peer-e2e \
        --profile "$PROFILE"
)"
printf '%s\n' "$REQUEST_OUT" > "$LOCAL_WORK/request.out"
REQUEST_ID="$(printf '%s\n' "$REQUEST_OUT" | sed -n 's/^:: request id: //p' | head -n 1)"
APPROVAL_CODE="$(printf '%s\n' "$REQUEST_OUT" | sed -n 's/^:: approval code: //p' | head -n 1)"
if [[ -z "$REQUEST_ID" || -z "$APPROVAL_CODE" ]]; then
    echo "could not parse request id/code from output:" >&2
    printf '%s\n' "$REQUEST_OUT" >&2
    exit 1
fi
echo ":: request id: $REQUEST_ID"
echo ":: approval code: $APPROVAL_CODE"

echo ":: approving request headlessly on remote"
remote_bash "
cd $(shell_quote "$REMOTE_PROJECT")
HOME=$(shell_quote "$REMOTE_HOME") XDG_CONFIG_HOME=$(shell_quote "$REMOTE_HOME/.config") \
  $(shell_quote "$REMOTE_BIN") peer requests > $(shell_quote "$REMOTE_WORK/remote-requests.out")
HOME=$(shell_quote "$REMOTE_HOME") XDG_CONFIG_HOME=$(shell_quote "$REMOTE_HOME/.config") \
  $(shell_quote "$REMOTE_BIN") peer approve $(shell_quote "$APPROVAL_CODE") \
  --profile $(shell_quote "$PROFILE") > $(shell_quote "$REMOTE_WORK/remote-approve.out")
"

echo ":: completing request locally"
local_intendant peer complete "$REQUEST_ID" --label remote-peer-e2e > "$LOCAL_WORK/complete.out"

echo ":: starting local dashboard daemon"
(
    cd "$LOCAL_PROJECT"
    HOME="$LOCAL_HOME" XDG_CONFIG_HOME="$LOCAL_HOME/.config" \
        nohup "$LOCAL_BIN" \
        --web "$LOCAL_PORT" \
        --no-tui \
        --no-tls \
        --bind 127.0.0.1 \
        --advertise-url "ws://127.0.0.1:$LOCAL_PORT/ws" \
        > "$LOCAL_WORK/local-daemon.out" \
        2> "$LOCAL_WORK/local-daemon.err" &
    LOCAL_DAEMON_PID=$!
    echo "$LOCAL_DAEMON_PID" > "$LOCAL_WORK/local-daemon.pid"
)
wait_for_http "http://127.0.0.1:$LOCAL_PORT/config" "local dashboard" "-s"

echo ":: waiting for local /api/peers to report remote connected"
PEERS_JSON="$LOCAL_WORK/peers.json"
for _ in $(seq 1 90); do
    curl -s --max-time 2 "http://127.0.0.1:$LOCAL_PORT/api/peers" > "$PEERS_JSON" || true
    if python3 - "$PEERS_JSON" <<'PY'
import json
import sys
from pathlib import Path

try:
    data = json.loads(Path(sys.argv[1]).read_text())
except Exception:
    sys.exit(1)

for peer in data.get("peers", []):
    state = peer.get("connection_state", {}).get("state")
    label = peer.get("label", "")
    if state == "connected" and "remote" in label:
        sys.exit(0)
sys.exit(1)
PY
    then
        break
    fi
    sleep 1
done

python3 - "$PEERS_JSON" <<'PY'
import json
import sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text())
peers = data.get("peers", [])
connected = [
    p for p in peers
    if p.get("connection_state", {}).get("state") == "connected"
]
if not connected:
    print(json.dumps(data, indent=2), file=sys.stderr)
    raise SystemExit("remote peer never reached connected state")

print(":: connected peers:")
for peer in connected:
    print(f"   - {peer.get('label')} ({peer.get('id')})")
PY

echo ":: remote inbound identities"
remote_bash "
cd $(shell_quote "$REMOTE_PROJECT")
HOME=$(shell_quote "$REMOTE_HOME") XDG_CONFIG_HOME=$(shell_quote "$REMOTE_HOME/.config") \
  $(shell_quote "$REMOTE_BIN") peer identities
" | tee "$LOCAL_WORK/remote-identities.out"

if ! grep -q 'Approved' "$LOCAL_WORK/remote-identities.out"; then
    echo "expected an approved remote identity" >&2
    exit 1
fi

echo ":: peer pairing VM E2E passed"
