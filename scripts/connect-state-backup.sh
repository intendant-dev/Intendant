#!/usr/bin/env bash
#
# Encrypted backup for hosted Intendant Connect state.
#
# The state file contains account records, passkey public-key records, daemon
# ownership, labels, hashed claim phrases, and audit events. Use encryption for
# normal backups; pass --allow-plaintext only for a deliberate local diagnostic.
#
set -euo pipefail

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf ':: %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }

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
CONNECT_REMOTE_STATE="${CONNECT_REMOTE_STATE:-}"
CONNECT_BACKUP_DIR="${CONNECT_BACKUP_DIR:-$HOME/.local/share/intendant/connect-backups}"

PASSPHRASE_FILE="${CONNECT_BACKUP_PASSPHRASE_FILE:-}"
ALLOW_PLAINTEXT=false

usage() {
    cat <<EOF
Usage: scripts/connect-state-backup.sh [options]

Options:
  --host <host>                SSH host. Required unless CONNECT_HOST is set
  --ssh-user <user>            SSH user. Required unless CONNECT_SSH_USER is set
  --ssh-key <path>             SSH key. Required unless CONNECT_SSH_KEY is set
  --remote-state <path>        Remote state file. Required unless CONNECT_REMOTE_STATE is set
  --output-dir <path>          Local backup directory. Default: $CONNECT_BACKUP_DIR
  --passphrase-file <path>     Encrypt with openssl AES-256-CBC/PBKDF2
  --allow-plaintext            Write a 0600 plaintext JSON backup
  -h, --help                   Show this help

CONNECT_OPS_ENV may point to a private env file containing these CONNECT_* values.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) CONNECT_HOST="${2:-}"; shift 2 ;;
        --ssh-user) CONNECT_SSH_USER="${2:-}"; shift 2 ;;
        --ssh-key) CONNECT_SSH_KEY="${2:-}"; shift 2 ;;
        --remote-state) CONNECT_REMOTE_STATE="${2:-}"; shift 2 ;;
        --output-dir) CONNECT_BACKUP_DIR="${2:-}"; shift 2 ;;
        --passphrase-file) PASSPHRASE_FILE="${2:-}"; shift 2 ;;
        --allow-plaintext) ALLOW_PLAINTEXT=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

[[ -n "$CONNECT_HOST" ]] || die "--host is required"
[[ -n "$CONNECT_SSH_USER" ]] || die "--ssh-user is required"
[[ -n "$CONNECT_REMOTE_STATE" ]] || die "--remote-state is required"
[[ -n "$CONNECT_SSH_KEY" ]] || die "--ssh-key is required"
[[ -f "$CONNECT_SSH_KEY" ]] || die "SSH key not found: $CONNECT_SSH_KEY"
command -v ssh >/dev/null 2>&1 || die "ssh is required"

if [[ -z "$PASSPHRASE_FILE" && "$ALLOW_PLAINTEXT" == false ]]; then
    die "provide --passphrase-file for encrypted backup, or --allow-plaintext deliberately"
fi
if [[ -n "$PASSPHRASE_FILE" ]]; then
    [[ -f "$PASSPHRASE_FILE" ]] || die "passphrase file not found: $PASSPHRASE_FILE"
    command -v openssl >/dev/null 2>&1 || die "openssl is required for encrypted backups"
fi

hash_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1"
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        warn "no sha256 tool found; skipping checksum"
    fi
}

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
mkdir -p "$CONNECT_BACKUP_DIR"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
plain="$CONNECT_BACKUP_DIR/intendant-connect-state-$timestamp.json"
tmp="$plain.tmp"

SSH_DEST="$CONNECT_SSH_USER@$CONNECT_HOST"
SSH_OPTS=(
    -i "$CONNECT_SSH_KEY"
    -o IdentitiesOnly=yes
    -o StrictHostKeyChecking=accept-new
)

remote_state_q="$(printf "%q" "$CONNECT_REMOTE_STATE")"
info "reading $SSH_DEST:$CONNECT_REMOTE_STATE"
ssh "${SSH_OPTS[@]}" "$SSH_DEST" "sudo cat $remote_state_q" > "$tmp"
mv "$tmp" "$plain"
validate_json "$plain"

if [[ -n "$PASSPHRASE_FILE" ]]; then
    encrypted="$plain.enc"
    openssl enc -aes-256-cbc -salt -pbkdf2 -iter 200000 \
        -in "$plain" \
        -out "$encrypted" \
        -pass "file:$PASSPHRASE_FILE"
    rm -f "$plain"
    hash_file "$encrypted" > "$encrypted.sha256" || true
    info "encrypted backup written: $encrypted"
    info "checksum written: $encrypted.sha256"
else
    warn "plaintext backup written because --allow-plaintext was provided"
    hash_file "$plain" > "$plain.sha256" || true
    info "backup written: $plain"
    info "checksum written: $plain.sha256"
fi
