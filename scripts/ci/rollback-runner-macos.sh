#!/bin/bash
# Roll one migrated macOS listener back to the operator account (run with
# sudo from a repo checkout: sudo scripts/ci/rollback-runner-macos.sh
# <listener-name>). Exact inverse of migrate-runner-macos.sh, driven by the
# metadata that script parked in /etc/intendant-ci/migration/:
#
#   1. bootout the LaunchDaemon and wait for the service tree to exit;
#   2. move the runner directory back to its original path and chown it to
#      the operator account (registration files travel back — identity
#      preserved);
#   3. restore the saved .path/.env/.service;
#   4. restore the original LaunchAgent plist and bootstrap it in the
#      operator's gui domain (deferred to next login when no gui session);
#   5. restore the watchdog.conf entries (label back to RUNNER_LABELS; the
#      CI account and its cache root drop out once no daemon listener
#      remains).
set -euo pipefail

die() {
    echo "error: $*" >&2
    exit 1
}

[ "$(id -u)" -eq 0 ] || die "run with sudo"
[ "$(uname -s)" = "Darwin" ] || die "macOS only"

LISTENER="${1:-}"
[ -n "$LISTENER" ] || die "usage: sudo $0 <listener-name>"
case "$LISTENER" in
    */* | *[[:space:]]*) die "listener name must be a bare runner name" ;;
esac

WATCHDOG_CONF="/etc/intendant-ci/watchdog.conf"
BACKUP_DIR="/etc/intendant-ci/migration"

# ---- locate the migration record -----------------------------------------
metas=""
for meta in "$BACKUP_DIR"/actions.runner.*."$LISTENER".meta; do
    if [ -f "$meta" ]; then
        metas="$metas $meta"
    fi
done
metas="${metas# }"
[ -n "$metas" ] || die "no migration record for '$LISTENER' in $BACKUP_DIR — was it migrated by migrate-runner-macos.sh?"
case "$metas" in
    *" "*) die "'$LISTENER' matches more than one migration record: $metas" ;;
esac
META="$metas"

# Root-owned metadata written by migrate-runner-macos.sh.
LABEL="" OP_ACCOUNT="" OP_UID="" OP_AGENTS_DIR="" ORIG_ROOT="" DEST="" CI_ACCOUNT="" CI_HOME=""
# shellcheck disable=SC1090
. "$META"
for var in LABEL OP_ACCOUNT OP_UID OP_AGENTS_DIR ORIG_ROOT DEST CI_ACCOUNT CI_HOME; do
    eval "val=\"\${$var:-}\""
    [ -n "$val" ] || die "migration record $META is missing $var"
done

DAEMON_PLIST="/Library/LaunchDaemons/$LABEL.plist"
AGENT_BACKUP="$BACKUP_DIR/$LABEL.launchagent.plist"
[ -d "$DEST" ] || die "migrated runner dir $DEST does not exist"
[ ! -e "$ORIG_ROOT" ] || die "original path $ORIG_ROOT is occupied — refusing to overwrite"
[ -f "$AGENT_BACKUP" ] || die "parked LaunchAgent plist $AGENT_BACKUP is missing"

if ps -axo command= | grep -F "$DEST/bin/Runner.Worker" | grep -qv grep; then
    die "a job is running on $LISTENER (Runner.Worker alive) — retry when idle"
fi

echo "rolling back listener: $LISTENER"
echo "  label: $LABEL"
echo "  from:  $DEST ($CI_ACCOUNT)"
echo "  to:    $ORIG_ROOT ($OP_ACCOUNT, uid $OP_UID)"

# ---- stop the LaunchDaemon ------------------------------------------------
launchctl bootout "system/$LABEL" 2>/dev/null \
    && echo "stopped LaunchDaemon $LABEL" \
    || echo "LaunchDaemon $LABEL was not running"

tree_alive() {
    ps -axo command= | grep -F "$DEST/" | grep -qv grep && return 0
    lsof -a -u "$CI_ACCOUNT" -d cwd 2>/dev/null \
        | awk -v r="$DEST" '$NF == r || index($NF, r "/") == 1 { found = 1 } END { exit !found }'
}
waited=0
while tree_alive; do
    [ "$waited" -lt 60 ] || die "runner processes still alive under $DEST after ${waited}s — aborting (nothing moved)"
    sleep 2
    waited=$((waited + 2))
done

rm -f "$DAEMON_PLIST"

# ---- move the runner directory back ---------------------------------------
mv "$DEST" "$ORIG_ROOT"
OP_GROUP="$(id -gn "$OP_ACCOUNT")"
chown -R "$OP_ACCOUNT:$OP_GROUP" "$ORIG_ROOT"
echo "moved runner dir back (registration preserved)"

# True inverse: restore each dotfile from its backup, or remove it when the
# original runner had none (migrate writes .path/.env/.service either way).
for f in .path .env .service; do
    if [ -f "$BACKUP_DIR/$LABEL$f" ]; then
        cp -p "$BACKUP_DIR/$LABEL$f" "$ORIG_ROOT/$f"
        chown "$OP_ACCOUNT:$OP_GROUP" "$ORIG_ROOT/$f"
    else
        rm -f "$ORIG_ROOT/$f"
    fi
done
echo "restored .path/.env/.service"

# ---- restore the LaunchAgent ----------------------------------------------
install -d -o "$OP_ACCOUNT" -g "$OP_GROUP" "$OP_AGENTS_DIR"
AGENT_PLIST="$OP_AGENTS_DIR/$LABEL.plist"
cp -p "$AGENT_BACKUP" "$AGENT_PLIST"
chown "$OP_ACCOUNT:$OP_GROUP" "$AGENT_PLIST"
chmod 0644 "$AGENT_PLIST"

# ---- restore the watchdog entries -----------------------------------------
conf_get() {
    # shellcheck disable=SC1090 # host conf, root-owned
    ( . "$WATCHDOG_CONF" 2>/dev/null; eval "printf '%s' \"\${$1:-}\"" )
}
conf_set() {
    local key="$1" val="$2"
    if grep -q "^${key}=" "$WATCHDOG_CONF"; then
        sed -i '' "s|^${key}=.*|${key}=\"${val}\"|" "$WATCHDOG_CONF"
    else
        printf '%s="%s"\n' "$key" "$val" >> "$WATCHDOG_CONF"
    fi
}
list_remove() {
    local out="" w
    for w in $1; do
        [ "$w" = "$2" ] || out="$out $w"
    done
    printf '%s' "${out# }"
}
list_add() {
    local w
    for w in $1; do
        if [ "$w" = "$2" ]; then
            printf '%s' "$1"
            return 0
        fi
    done
    if [ -n "$1" ]; then
        printf '%s %s' "$1" "$2"
    else
        printf '%s' "$2"
    fi
}
if [ -f "$WATCHDOG_CONF" ]; then
    cp -p "$WATCHDOG_CONF" "$BACKUP_DIR/watchdog.conf.before-rollback-$LISTENER"
    conf_set RUNNER_DAEMON_LABELS "$(list_remove "$(conf_get RUNNER_DAEMON_LABELS)" "$LABEL")"
    conf_set RUNNER_LABELS "$(list_add "$(conf_get RUNNER_LABELS)" "$LABEL")"
    conf_set RUNNER_UID "$OP_UID"
    conf_set RUNNER_PLIST_DIR "$OP_AGENTS_DIR"
    conf_set RUNNER_USER "$(list_add "$(conf_get RUNNER_USER)" "$OP_ACCOUNT")"
    if [ -z "$(conf_get RUNNER_DAEMON_LABELS)" ]; then
        # Last daemon listener gone: the CI account and its cache root drop
        # out of the watchdog's purview.
        conf_set RUNNER_USER "$(list_remove "$(conf_get RUNNER_USER)" "$CI_ACCOUNT")"
        conf_set CACHE_ROOTS "$(list_remove "$(conf_get CACHE_ROOTS)" "$CI_HOME/.cache/intendant-ci/target")"
    fi
    echo "restored watchdog.conf entries"
else
    echo "note: $WATCHDOG_CONF not found — skipping watchdog rewiring"
fi

# ---- start the LaunchAgent -------------------------------------------------
if launchctl bootstrap "gui/$OP_UID" "$AGENT_PLIST" 2>/dev/null; then
    echo "bootstrapped LaunchAgent $LABEL in gui/$OP_UID"
else
    echo "could not bootstrap gui/$OP_UID (no gui session for $OP_ACCOUNT?) —"
    echo "the LaunchAgent will start at $OP_ACCOUNT's next login, or run:"
    echo "  launchctl bootstrap gui/$OP_UID $AGENT_PLIST"
fi

# Consume the migration record (the backups it points to were restored).
rm -f "$META" "$AGENT_BACKUP" "$BACKUP_DIR/$LABEL.path" "$BACKUP_DIR/$LABEL.env" "$BACKUP_DIR/$LABEL.service"

ORG_REPO="$(sed -n 's|.*"gitHubUrl": *"https://github.com/\([^"]*\)".*|\1|p' "$ORIG_ROOT/.runner" | head -1 || true)"
ORG_REPO="${ORG_REPO%/}"
echo
echo "rollback of $LISTENER complete."
if [ -n "$ORG_REPO" ]; then
    echo "verify: gh api repos/$ORG_REPO/actions/runners --paginate --jq '.runners[] | select(.name==\"$LISTENER\") | .status'"
fi
