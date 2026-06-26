#!/usr/bin/env bash
#
# Restore hosted Intendant Connect state from a backup made by
# scripts/connect-state-backup.sh.
#
set -euo pipefail

CONNECT_HOST="${CONNECT_HOST:-16.171.75.210}"
CONNECT_SSH_USER="${CONNECT_SSH_USER:-ubuntu}"
CONNECT_SSH_KEY="${CONNECT_SSH_KEY:-$HOME/.ssh/intendant-connect-prod-alpha-ec2}"
CONNECT_REMOTE_STATE="${CONNECT_REMOTE_STATE:-/var/lib/intendant-connect/state.json}"
CONNECT_SERVICE="${CONNECT_SERVICE:-intendant-connect}"
CONNECT_REMOTE_READYZ_URL="${CONNECT_REMOTE_READYZ_URL:-http://127.0.0.1:8787/readyz}"

PASSPHRASE_FILE="${CONNECT_BACKUP_PASSPHRASE_FILE:-}"
YES=false
BACKUP_FILE=""

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf ':: %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }

usage() {
    cat <<EOF
Usage: scripts/connect-state-restore.sh --yes [options] <backup.json|backup.json.enc>

Options:
  --host <host>                SSH host. Default: $CONNECT_HOST
  --ssh-user <user>            SSH user. Default: $CONNECT_SSH_USER
  --ssh-key <path>             SSH key. Default: $CONNECT_SSH_KEY
  --remote-state <path>        Remote state file. Default: $CONNECT_REMOTE_STATE
  --service <name>             systemd service. Default: $CONNECT_SERVICE
  --remote-readyz-url <url>    Remote readiness URL. Default: $CONNECT_REMOTE_READYZ_URL
  --passphrase-file <path>     Required for .enc backups
  --yes                        Confirm replacement of remote state
  -h, --help                   Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) CONNECT_HOST="${2:-}"; shift 2 ;;
        --ssh-user) CONNECT_SSH_USER="${2:-}"; shift 2 ;;
        --ssh-key) CONNECT_SSH_KEY="${2:-}"; shift 2 ;;
        --remote-state) CONNECT_REMOTE_STATE="${2:-}"; shift 2 ;;
        --service) CONNECT_SERVICE="${2:-}"; shift 2 ;;
        --remote-readyz-url) CONNECT_REMOTE_READYZ_URL="${2:-}"; shift 2 ;;
        --passphrase-file) PASSPHRASE_FILE="${2:-}"; shift 2 ;;
        --yes) YES=true; shift ;;
        -h|--help) usage; exit 0 ;;
        -* ) die "unknown option: $1" ;;
        * )
            [[ -z "$BACKUP_FILE" ]] || die "only one backup file may be provided"
            BACKUP_FILE="$1"
            shift
            ;;
    esac
done

[[ "$YES" == true ]] || die "restore replaces remote state; pass --yes to continue"
[[ -n "$BACKUP_FILE" ]] || die "backup file is required"
[[ -f "$BACKUP_FILE" ]] || die "backup file not found: $BACKUP_FILE"
[[ -f "$CONNECT_SSH_KEY" ]] || die "SSH key not found: $CONNECT_SSH_KEY"
command -v ssh >/dev/null 2>&1 || die "ssh is required"
command -v scp >/dev/null 2>&1 || die "scp is required"

if [[ "$BACKUP_FILE" == *.enc ]]; then
    [[ -n "$PASSPHRASE_FILE" ]] || die "--passphrase-file is required for encrypted backups"
    [[ -f "$PASSPHRASE_FILE" ]] || die "passphrase file not found: $PASSPHRASE_FILE"
    command -v openssl >/dev/null 2>&1 || die "openssl is required for encrypted restores"
fi

validate_json() {
    local file="$1"
    if command -v jq >/dev/null 2>&1; then
        jq empty "$file" >/dev/null
    elif command -v node >/dev/null 2>&1; then
        node -e 'JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))' "$file"
    else
        warn "jq/node not found; skipping JSON validation"
    fi
}

umask 077
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
plain="$tmpdir/state.json"

if [[ "$BACKUP_FILE" == *.enc ]]; then
    info "decrypting backup"
    openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
        -in "$BACKUP_FILE" \
        -out "$plain" \
        -pass "file:$PASSPHRASE_FILE"
else
    cp "$BACKUP_FILE" "$plain"
fi
validate_json "$plain"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
remote_tmp="/tmp/intendant-connect-state-restore-$timestamp.json"
SSH_DEST="$CONNECT_SSH_USER@$CONNECT_HOST"
SSH_OPTS=(
    -i "$CONNECT_SSH_KEY"
    -o IdentitiesOnly=yes
    -o StrictHostKeyChecking=accept-new
)

info "uploading restore candidate"
scp "${SSH_OPTS[@]}" "$plain" "$SSH_DEST:$remote_tmp" >/dev/null

remote_state_q="$(printf "%q" "$CONNECT_REMOTE_STATE")"
remote_tmp_q="$(printf "%q" "$remote_tmp")"
remote_service_q="$(printf "%q" "$CONNECT_SERVICE")"
remote_readyz_q="$(printf "%q" "$CONNECT_REMOTE_READYZ_URL")"

info "installing state and restarting $CONNECT_SERVICE"
ssh "${SSH_OPTS[@]}" "$SSH_DEST" "bash -lc 'set -euo pipefail
remote_state=$remote_state_q
remote_tmp=$remote_tmp_q
backup_dir=\$(dirname \"\$remote_state\")/backups
sudo install -d -m 0700 \"\$backup_dir\"
if [[ -f \"\$remote_state\" ]]; then
  sudo cp \"\$remote_state\" \"\$backup_dir/state-before-restore-$timestamp.json\"
fi
owner_group=\$(sudo stat -c \"%U:%G\" \"\$remote_state\" 2>/dev/null || true)
if [[ -z \"\$owner_group\" ]]; then
  if id intendant-connect >/dev/null 2>&1; then
    owner_group=\"intendant-connect:intendant-connect\"
  else
    owner_group=\"root:root\"
  fi
fi
owner=\${owner_group%%:*}
group=\${owner_group#*:}
sudo install -D -m 0600 -o \"\$owner\" -g \"\$group\" \"\$remote_tmp\" \"\$remote_state\"
sudo rm -f \"\$remote_tmp\"
sudo systemctl restart $remote_service_q
sudo systemctl is-active --quiet $remote_service_q
curl -fsS $remote_readyz_q >/dev/null
'"

info "restore complete"
