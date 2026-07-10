#!/bin/bash
# Migrate one macOS GitHub Actions listener from the operator's account to
# the dedicated CI service account (run with sudo from a repo checkout:
# sudo scripts/ci/migrate-runner-macos.sh <listener-name>).
#
# One listener per invocation, so the fleet keeps capacity while each
# migration soaks. The move preserves the runner's registration
# (.runner/.credentials travel with the directory) — the runner keeps its
# identity and name; nothing re-registers.
#
# What it does, in order:
#   1. finds the listener's LaunchAgent under a /Users/<account> home and
#      derives everything from it (label, runner root, owning account) —
#      nothing host-specific is hardcoded;
#   2. stops the LaunchAgent (launchctl bootout gui/<uid>/<label>), waits
#      for the service tree to exit, and parks the agent plist in
#      /etc/intendant-ci/migration/ for rollback;
#   3. rewires /etc/intendant-ci/watchdog.conf (label moves from
#      RUNNER_LABELS to RUNNER_DAEMON_LABELS, CI cache root + account are
#      added) so a watchdog tick can neither resurrect the old agent nor
#      miss the new daemon;
#   4. moves the runner directory into the CI account's home, chowns it,
#      remaps .path onto the CI home, and wires the job hooks into .env;
#   5. writes a LaunchDaemon plist rendered from the runner's own
#      bin/actions.runner.plist.template (the exact template svc.sh
#      renders LaunchAgents from, so the load-bearing keys — runsvc.sh
#      ProgramArguments, WorkingDirectory, RunAtLoad, log paths,
#      ACTIONS_RUNNER_SVC=1, ProcessType Interactive, SessionCreate — stay
#      in lockstep with the runner release), with UserName swapped to the
#      CI account and HOME/USER injected into EnvironmentVariables (the
#      one deliberate divergence: gui LaunchAgents inherit them from the
#      login session, system LaunchDaemons get neither, and rustup/cargo
#      need HOME); bootstraps it in the system domain;
#   6. waits for the runner to report online (gh api, when available) and
#      prints the rollback invocation.
#
# After migration the runner's own svc.sh no longer applies (it only
# manages gui-domain LaunchAgents); control the listener with
# `launchctl bootout|bootstrap system ...` or the fleet watchdog.
set -euo pipefail

die() {
    echo "error: $*" >&2
    exit 1
}

[ "$(id -u)" -eq 0 ] || die "run with sudo"
[ "$(uname -s)" = "Darwin" ] || die "macOS only"

LISTENER="${1:-}"
[ -n "$LISTENER" ] || die "usage: sudo $0 <listener-name>   (the <name> in actions.runner.<org>-<repo>.<name>)"
case "$LISTENER" in
    */* | *[[:space:]]*) die "listener name must be a bare runner name" ;;
esac

CI_ACCOUNT="${INTENDANT_CI_ACCOUNT:-_intendant-ci}"
WATCHDOG_CONF="/etc/intendant-ci/watchdog.conf"
BACKUP_DIR="/etc/intendant-ci/migration"
LIB_DIR="/usr/local/lib/intendant-ci"
DAEMON_PLIST_DIR="/Library/LaunchDaemons"

# ---- locate the listener ------------------------------------------------
matches=""
for plist in /Users/*/Library/LaunchAgents/actions.runner.*."$LISTENER".plist; do
    if [ -f "$plist" ]; then
        matches="$matches $plist"
    fi
done
matches="${matches# }"
[ -n "$matches" ] || die "no LaunchAgent actions.runner.*.$LISTENER.plist under any /Users/<account>/Library/LaunchAgents"
case "$matches" in
    *" "*) die "listener name '$LISTENER' matches more than one LaunchAgent: $matches" ;;
esac
AGENT_PLIST="$matches"

LABEL="$(basename "$AGENT_PLIST" .plist)"
OP_ACCOUNT="$(stat -f %Su "$AGENT_PLIST")"
OP_UID="$(id -u "$OP_ACCOUNT")"
OP_AGENTS_DIR="$(dirname "$AGENT_PLIST")"
RUNNER_ROOT="$(/usr/libexec/PlistBuddy -c 'Print :WorkingDirectory' "$AGENT_PLIST")"
[ -d "$RUNNER_ROOT" ] || die "runner root $RUNNER_ROOT (from the plist WorkingDirectory) does not exist"
[ -f "$RUNNER_ROOT/.runner" ] || die "$RUNNER_ROOT has no .runner registration file"

dscl . -read "/Users/$CI_ACCOUNT" UniqueID >/dev/null 2>&1 \
    || die "account $CI_ACCOUNT does not exist — run setup-ci-account-macos.sh first"
CI_HOME="$(dscl . -read "/Users/$CI_ACCOUNT" NFSHomeDirectory | awk '{print $2}')"
CI_GROUP="$(id -gn "$CI_ACCOUNT")"
[ -x "$CI_HOME/.cargo/bin/rustc" ] || die "$CI_ACCOUNT has no toolchain — run setup-ci-account-macos.sh first"
[ -x "$LIB_DIR/hooks/job-started.sh" ] || die "job hooks not installed — run setup-ci-account-macos.sh first"

DEST="$CI_HOME/$(basename "$RUNNER_ROOT")"
DAEMON_PLIST="$DAEMON_PLIST_DIR/$LABEL.plist"
[ ! -e "$DEST" ] || die "$DEST already exists — refusing to overwrite"
[ ! -e "$DAEMON_PLIST" ] || die "$DAEMON_PLIST already exists — is $LISTENER already migrated?"

# Refuse to move a runner mid-job: Runner.Worker only exists while a job
# executes, and both the Worker and the Listener are spawned by absolute
# path, so the root shows up verbatim in their command lines.
if ps -axo command= | grep -F "$RUNNER_ROOT/bin/Runner.Worker" | grep -qv grep; then
    die "a job is running on $LISTENER (Runner.Worker alive) — retry when idle"
fi

echo "migrating listener: $LISTENER"
echo "  label:        $LABEL"
echo "  from:         $RUNNER_ROOT ($OP_ACCOUNT, uid $OP_UID)"
echo "  to:           $DEST ($CI_ACCOUNT)"
echo "  LaunchDaemon: $DAEMON_PLIST"

install -d -m 0755 "$BACKUP_DIR"

# ---- stop the LaunchAgent ------------------------------------------------
launchctl bootout "gui/$OP_UID/$LABEL" 2>/dev/null \
    && echo "stopped LaunchAgent $LABEL" \
    || echo "LaunchAgent $LABEL was not running"

# Wait for the whole service tree to exit before moving the directory.
# runsvc.sh / Runner.Listener / Runner.Worker carry the absolute root in
# their command lines; the node middleman (RunnerService.js) is spawned
# relative but keeps the root as its cwd — check both.
tree_alive() {
    ps -axo command= | grep -F "$RUNNER_ROOT/" | grep -qv grep && return 0
    lsof -a -u "$OP_ACCOUNT" -d cwd 2>/dev/null \
        | awk -v r="$RUNNER_ROOT" '$NF == r || index($NF, r "/") == 1 { found = 1 } END { exit !found }'
}
waited=0
while tree_alive; do
    [ "$waited" -lt 60 ] || die "runner processes still alive under $RUNNER_ROOT after ${waited}s — aborting (nothing moved)"
    sleep 2
    waited=$((waited + 2))
done

mv "$AGENT_PLIST" "$BACKUP_DIR/$LABEL.launchagent.plist"
echo "parked LaunchAgent plist in $BACKUP_DIR"

# ---- rewire the watchdog BEFORE the move ---------------------------------
# (so a tick during the move can't bootstrap the parked agent; if the move
# aborts, rollback-runner-macos.sh restores the entries.)
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
    cp -p "$WATCHDOG_CONF" "$BACKUP_DIR/watchdog.conf.before-migrate-$LISTENER"
    conf_set RUNNER_LABELS "$(list_remove "$(conf_get RUNNER_LABELS)" "$LABEL")"
    conf_set RUNNER_DAEMON_LABELS "$(list_add "$(conf_get RUNNER_DAEMON_LABELS)" "$LABEL")"
    conf_set RUNNER_DAEMON_PLIST_DIR "$DAEMON_PLIST_DIR"
    conf_set CACHE_ROOTS "$(list_add "$(conf_get CACHE_ROOTS)" "$CI_HOME/.cache/intendant-ci/target")"
    conf_set RUNNER_USER "$(list_add "$(conf_get RUNNER_USER)" "$CI_ACCOUNT")"
    echo "rewired $WATCHDOG_CONF (LaunchDaemon label, CI cache root, CI account)"
    echo "  note: the old cache root stays listed — the watchdog prunes its stale keys away on its own"
else
    echo "note: $WATCHDOG_CONF not found — skipping watchdog rewiring (install-watchdog-macos.sh not run?)"
fi

# ---- move the runner directory -------------------------------------------
mv "$RUNNER_ROOT" "$DEST"
chown -R "$CI_ACCOUNT:$CI_GROUP" "$DEST"
echo "moved runner dir (registration files travel with it — identity preserved)"

for f in .path .env .service; do
    if [ -f "$DEST/$f" ]; then
        cp -p "$DEST/$f" "$BACKUP_DIR/$LABEL$f"
    fi
done

# .path is the PATH runsvc.sh exports to the listener (and thus to every
# job step). Remap segments under the old home onto the CI home; keep the
# rest (homebrew, system paths) verbatim.
OP_HOME="$(dscl . -read "/Users/$OP_ACCOUNT" NFSHomeDirectory | awk '{print $2}')"
if [ -f "$DEST/.path" ]; then
    old_path="$(cat "$DEST/.path")"
else
    old_path="$OP_HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
fi
new_path=""
old_ifs="$IFS"
IFS=':'
for seg in $old_path; do
    case "$seg" in
        "$OP_HOME"/*) seg="$CI_HOME${seg#"$OP_HOME"}" ;;
        "$OP_HOME") seg="$CI_HOME" ;;
    esac
    new_path="${new_path:+$new_path:}$seg"
done
IFS="$old_ifs"
# (trailing newline for fidelity with env.sh's `echo $PATH>.path`)
printf '%s\n' "$new_path" > "$DEST/.path"
chown "$CI_ACCOUNT:$CI_GROUP" "$DEST/.path"
echo "remapped .path: $new_path"

# Job hooks (GitHub reads .env at listener startup; the daemon bootstrap
# below is that startup).
set_env_kv() {
    local env_file="$1" key="$2" val="$3"
    touch "$env_file"
    if grep -q "^${key}=" "$env_file"; then
        sed -i '' "s|^${key}=.*|${key}=${val}|" "$env_file"
    else
        printf '%s=%s\n' "$key" "$val" >> "$env_file"
    fi
}
set_env_kv "$DEST/.env" ACTIONS_RUNNER_HOOK_JOB_STARTED "$LIB_DIR/hooks/job-started.sh"
set_env_kv "$DEST/.env" ACTIONS_RUNNER_HOOK_JOB_COMPLETED "$LIB_DIR/hooks/job-completed.sh"
chown "$CI_ACCOUNT:$CI_GROUP" "$DEST/.env"
echo "wired job hooks into .env"

# ---- write + bootstrap the LaunchDaemon ----------------------------------
install -d -o "$CI_ACCOUNT" -g "$CI_GROUP" -m 0755 "$CI_HOME/Library/Logs/$LABEL"

TEMPLATE="$DEST/bin/actions.runner.plist.template"
if [ -f "$TEMPLATE" ]; then
    # Same substitution svc.sh performs, so the daemon inherits whatever
    # keys the installed runner release considers load-bearing.
    sed -e "s|{{User}}|$CI_ACCOUNT|g" \
        -e "s|{{SvcName}}|$LABEL|g" \
        -e "s|{{RunnerRoot}}|$DEST|g" \
        -e "s|{{UserHome}}|$CI_HOME|g" \
        "$TEMPLATE" > "$DAEMON_PLIST.tmp"
else
    # Fallback replica of the runner's template (actions/runner as of
    # 2026-07: src/Misc/layoutbin/actions.runner.plist.template) in case a
    # future release drops it. Keep in lockstep if the template changes.
    echo "note: $TEMPLATE missing — writing the vendored replica"
    cat > "$DAEMON_PLIST.tmp" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
      <string>$DEST/runsvc.sh</string>
    </array>
    <key>UserName</key>
    <string>$CI_ACCOUNT</string>
    <key>WorkingDirectory</key>
    <string>$DEST</string>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$CI_HOME/Library/Logs/$LABEL/stdout.log</string>
    <key>StandardErrorPath</key>
    <string>$CI_HOME/Library/Logs/$LABEL/stderr.log</string>
    <key>EnvironmentVariables</key>
    <dict>
      <key>ACTIONS_RUNNER_SVC</key>
      <string>1</string>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>SessionCreate</key>
    <true/>
  </dict>
</plist>
PLIST_EOF
fi

# The one deliberate divergence from the template: gui-domain LaunchAgents
# inherit HOME/USER from the login session, but system-domain LaunchDaemons
# get neither — and without HOME the rustup shims, cargo, and the job hooks
# have no anchor. Inject them explicitly.
add_env() {
    /usr/libexec/PlistBuddy -c "Add :EnvironmentVariables:$1 string $2" "$DAEMON_PLIST.tmp" 2>/dev/null \
        || /usr/libexec/PlistBuddy -c "Set :EnvironmentVariables:$1 $2" "$DAEMON_PLIST.tmp"
}
/usr/libexec/PlistBuddy -c "Add :EnvironmentVariables dict" "$DAEMON_PLIST.tmp" 2>/dev/null || true
add_env HOME "$CI_HOME"
add_env USER "$CI_ACCOUNT"

plutil -lint "$DAEMON_PLIST.tmp" >/dev/null || die "generated plist failed plutil -lint: $DAEMON_PLIST.tmp"
mv "$DAEMON_PLIST.tmp" "$DAEMON_PLIST"
chown root:wheel "$DAEMON_PLIST"
chmod 0644 "$DAEMON_PLIST"

# .service is svc.sh's pointer; svc.sh can no longer manage this listener,
# but the file should point at the truth for whoever reads it.
printf '%s\n' "$DAEMON_PLIST" > "$DEST/.service"
chown "$CI_ACCOUNT:$CI_GROUP" "$DEST/.service"

launchctl bootout "system/$LABEL" 2>/dev/null || true
launchctl bootstrap system "$DAEMON_PLIST"
echo "bootstrapped $LABEL in the system domain"

# ---- wait for the listener to report online ------------------------------
ORG_REPO="$(sed -n 's|.*"gitHubUrl": *"https://github.com/\([^"]*\)".*|\1|p' "$DEST/.runner" | head -1)"
ORG_REPO="${ORG_REPO%/}"
VERIFY_CMD="gh api repos/$ORG_REPO/actions/runners --paginate --jq '.runners[] | select(.name==\"$LISTENER\") | .status'"
# gh auth lives with the invoking operator; sudo's secure_path usually
# lacks the homebrew prefix, so resolve the binary explicitly.
GH_BIN=""
for cand in /opt/homebrew/bin/gh /usr/local/bin/gh /usr/bin/gh; do
    if [ -x "$cand" ]; then
        GH_BIN="$cand"
        break
    fi
done
status=""
if [ -n "${SUDO_USER:-}" ] && [ -n "$GH_BIN" ] && [ -n "$ORG_REPO" ]; then
    echo "waiting for $LISTENER to report online..."
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do
        status="$(sudo -u "$SUDO_USER" -H "$GH_BIN" api "repos/$ORG_REPO/actions/runners" --paginate \
            --jq ".runners[] | select(.name==\"$LISTENER\") | .status" 2>/dev/null | head -1 || true)"
        if [ "$status" = "online" ]; then
            break
        fi
        sleep 5
    done
    if [ "$status" = "online" ]; then
        echo "listener $LISTENER is ONLINE as $CI_ACCOUNT"
    else
        echo "listener not online yet (last status: '${status:-unknown}') — check:"
        echo "  $VERIFY_CMD"
        echo "  tail -f $CI_HOME/Library/Logs/$LABEL/stderr.log"
    fi
else
    echo "gh not available for ${SUDO_USER:-<no SUDO_USER>} — verify manually:"
    echo "  $VERIFY_CMD"
    echo "  tail -f $CI_HOME/Library/Logs/$LABEL/stderr.log"
fi

# ---- record rollback metadata --------------------------------------------
cat > "$BACKUP_DIR/$LABEL.meta" <<META_EOF
# written by migrate-runner-macos.sh — consumed by rollback-runner-macos.sh
LABEL="$LABEL"
LISTENER="$LISTENER"
OP_ACCOUNT="$OP_ACCOUNT"
OP_UID="$OP_UID"
OP_HOME="$OP_HOME"
OP_AGENTS_DIR="$OP_AGENTS_DIR"
ORIG_ROOT="$RUNNER_ROOT"
DEST="$DEST"
CI_ACCOUNT="$CI_ACCOUNT"
CI_HOME="$CI_HOME"
MIGRATED_AT="$(date '+%Y-%m-%dT%H:%M:%S%z')"
META_EOF
chmod 0644 "$BACKUP_DIR/$LABEL.meta"

echo
echo "migration of $LISTENER complete."
echo "watch a canary job before migrating the next listener."
echo "rollback: sudo scripts/ci/rollback-runner-macos.sh $LISTENER"
