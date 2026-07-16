#!/bin/bash
# Enroll ONE fresh GitHub Actions listener on this macOS host as the
# dedicated CI service account, running as a system-domain LaunchDaemon
# (run with sudo from a repo checkout:
#   sudo scripts/ci/enroll-runner-macos.sh <name> <registration-token> \
#       <runner-version> [labels] [repo-url]).
#
# Fresh-enrollment sibling of migrate-runner-macos.sh: that script MOVES an
# existing operator-account listener onto the CI account; this one creates a
# listener where none exists (a new host, or extra capacity). The .env/.path
# wiring and plist rendering are kept in lockstep with the migrate script —
# change them together.
#
# Registration tokens are minted per enrollment (expire ~1h):
#   gh api -X POST repos/<org>/<repo>/actions/runners/registration-token --jq .token
#
# Gotcha this script exists to remember: the runner tarball ships runsvc.sh
# only under bin/; `svc.sh install` (the gui-domain path we do not use)
# copies it to the runner root as a side effect. A daemon whose
# ProgramArguments point at <root>/runsvc.sh without that copy spawn-loops
# with EX_CONFIG (78) and nothing in the runner's own logs (live
# 2026-07-15).
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || die "run with sudo"
[ "$(uname -s)" = "Darwin" ] || die "macOS only"

NAME="${1:?usage: enroll-runner-macos.sh <name> <registration-token> <runner-version> [labels] [repo-url]}"
TOKEN="${2:?registration token (gh api -X POST repos/<org>/<repo>/actions/runners/registration-token --jq .token)}"
VER="${3:?runner version, no leading v (gh api repos/actions/runner/releases/latest --jq .tag_name)}"
LABELS="${4:-intendant-macos}"
URL="${5:-https://github.com/intendant-dev/Intendant}"
case "$NAME" in
    */* | *[[:space:]]*) die "runner name must be a bare word" ;;
esac
VER="${VER#v}"

CI_ACCOUNT="${INTENDANT_CI_ACCOUNT:-_intendant-ci}"
dscl . -read "/Users/$CI_ACCOUNT" UniqueID >/dev/null 2>&1 \
    || die "account $CI_ACCOUNT does not exist — run setup-ci-account-macos.sh first"
CI_HOME="$(dscl . -read "/Users/$CI_ACCOUNT" NFSHomeDirectory | awk '{print $2}')"
CI_GROUP="$(id -gn "$CI_ACCOUNT")"
LIB_DIR="/usr/local/lib/intendant-ci"
ORG_REPO="${URL#https://github.com/}"
LABEL="actions.runner.$(echo "$ORG_REPO" | tr '/' '-').$NAME"
DEST="$CI_HOME/actions-runner-$NAME"
DAEMON_PLIST="/Library/LaunchDaemons/$LABEL.plist"
ARCH="$(uname -m | sed 's/x86_64/x64/; s/arm64/arm64/')"
TARBALL="/tmp/actions-runner-osx-$ARCH-$VER.tar.gz"

[ -x "$CI_HOME/.cargo/bin/rustc" ] || die "$CI_ACCOUNT has no toolchain — run setup-ci-account-macos.sh first"
[ -x "$LIB_DIR/hooks/job-started.sh" ] || die "job hooks not installed — run setup-ci-account-macos.sh first"
[ ! -e "$DEST" ] || die "$DEST already exists"
[ ! -e "$DAEMON_PLIST" ] || die "$DAEMON_PLIST already exists"

# Run as the CI account from a cwd it can read (sudo starts children in the
# invoker's cwd, typically a 700 operator home, where the CI account cannot
# even getcwd — rustup/cargo/tar all abort on that).
ci_sh() {
    sudo -u "$CI_ACCOUNT" -H env HOME="$CI_HOME" \
        PATH="$CI_HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        sh -c "cd '$CI_HOME' && $1"
}

if [ ! -f "$TARBALL" ]; then
    echo "downloading runner v$VER ($ARCH)..."
    curl -sSfL -o "$TARBALL" \
        "https://github.com/actions/runner/releases/download/v$VER/actions-runner-osx-$ARCH-$VER.tar.gz"
fi
chmod 0644 "$TARBALL"

install -d -o "$CI_ACCOUNT" -g "$CI_GROUP" -m 0750 "$DEST"
ci_sh "cd '$DEST' && tar xzf '$TARBALL'"
echo "extracted runner v$VER into $DEST"

ci_sh "cd '$DEST' && ./config.sh --unattended --url '$URL' --token '$TOKEN' \
    --name '$NAME' --labels '$LABELS' --replace" || die "config.sh failed for $NAME"
[ -f "$DEST/.runner" ] || die "registration produced no .runner"

# The EX_CONFIG gotcha from the header: put runsvc.sh where the daemon
# plist (and svc.sh convention) expects it.
ci_sh "cd '$DEST' && cp bin/runsvc.sh runsvc.sh && chmod +x runsvc.sh"

# Job env: hooks (account-gated) + the account's supervised sccache server
# coordinates — cargo's [env] port does not reach every in-job sccache
# invocation (2026-07-10), so they ride the job env explicitly. Mirrors
# migrate-runner-macos.sh.
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
set_env_kv "$DEST/.env" INTENDANT_CI_HOOK_ACCOUNT "$CI_ACCOUNT"
set_env_kv "$DEST/.env" SCCACHE_SERVER_PORT "4227"
set_env_kv "$DEST/.env" SCCACHE_DIR "$CI_HOME/.cache/sccache"
chown "$CI_ACCOUNT:$CI_GROUP" "$DEST/.env"

# .path is the PATH runsvc.sh exports to every job step.
printf '%s\n' "$CI_HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" > "$DEST/.path"
chown "$CI_ACCOUNT:$CI_GROUP" "$DEST/.path"

# LaunchDaemon rendered from the runner's own template (same substitution
# svc.sh performs), with HOME/USER injected — system-domain daemons inherit
# neither, and rustup/cargo/the hooks need HOME. Mirrors
# migrate-runner-macos.sh.
install -d -o "$CI_ACCOUNT" -g "$CI_GROUP" -m 0755 "$CI_HOME/Library/Logs/$LABEL"
TEMPLATE="$DEST/bin/actions.runner.plist.template"
[ -f "$TEMPLATE" ] || die "runner template missing at $TEMPLATE (release layout changed?)"
sed -e "s|{{User}}|$CI_ACCOUNT|g" \
    -e "s|{{SvcName}}|$LABEL|g" \
    -e "s|{{RunnerRoot}}|$DEST|g" \
    -e "s|{{UserHome}}|$CI_HOME|g" \
    "$TEMPLATE" > "$DAEMON_PLIST.tmp"
add_env() {
    /usr/libexec/PlistBuddy -c "Add :EnvironmentVariables:$1 string $2" "$DAEMON_PLIST.tmp" 2>/dev/null \
        || /usr/libexec/PlistBuddy -c "Set :EnvironmentVariables:$1 $2" "$DAEMON_PLIST.tmp"
}
/usr/libexec/PlistBuddy -c "Add :EnvironmentVariables dict" "$DAEMON_PLIST.tmp" 2>/dev/null || true
add_env HOME "$CI_HOME"
add_env USER "$CI_ACCOUNT"
plutil -lint "$DAEMON_PLIST.tmp" >/dev/null || die "generated plist failed plutil -lint"
mv "$DAEMON_PLIST.tmp" "$DAEMON_PLIST"
chown root:wheel "$DAEMON_PLIST"
chmod 0644 "$DAEMON_PLIST"
printf '%s\n' "$DAEMON_PLIST" > "$DEST/.service"
chown "$CI_ACCOUNT:$CI_GROUP" "$DEST/.service"

launchctl bootout "system/$LABEL" 2>/dev/null || true
launchctl bootstrap system "$DAEMON_PLIST"
echo "bootstrapped $LABEL (logs: $CI_HOME/Library/Logs/$LABEL/)"

# Wait for the listener to report online (best-effort; mirrors the migrate
# script's tail).
GH_BIN=""
for cand in /opt/homebrew/bin/gh /usr/local/bin/gh /usr/bin/gh; do
    [ -x "$cand" ] && { GH_BIN="$cand"; break; }
done
if [ -n "${SUDO_USER:-}" ] && [ -n "$GH_BIN" ]; then
    echo "waiting for $NAME to report online..."
    status=""
    for _ in $(seq 1 24); do
        status="$(sudo -u "$SUDO_USER" -H "$GH_BIN" api "repos/$ORG_REPO/actions/runners" --paginate \
            --jq ".runners[] | select(.name==\"$NAME\") | .status" 2>/dev/null | head -1 || true)"
        [ "$status" = "online" ] && break
        sleep 5
    done
    if [ "$status" = "online" ]; then
        echo "listener $NAME is ONLINE as $CI_ACCOUNT"
    else
        echo "not online yet (last: '${status:-unknown}') — check:"
        echo "  tail -f $CI_HOME/Library/Logs/$LABEL/stderr.log"
        echo "  sudo launchctl print system/$LABEL | grep -E 'state|last exit'"
    fi
else
    echo "verify: gh api repos/$ORG_REPO/actions/runners --jq '.runners[] | \"\\(.name) \\(.status)\"'"
fi
echo "remember: add $LABEL to RUNNER_DAEMON_LABELS in /etc/intendant-ci/watchdog.conf"
