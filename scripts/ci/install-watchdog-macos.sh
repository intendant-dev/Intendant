#!/bin/bash
# Install the fleet watchdog on a macOS runner host (run with sudo from
# a repo checkout: sudo scripts/ci/install-watchdog-macos.sh <runner-account>).
#
# Installs a root LaunchDaemon that ticks scripts/ci/fleet-watchdog.sh
# every 5 minutes. Idempotent: re-running upgrades the script and
# daemon, and leaves an existing /etc/intendant-ci/watchdog.conf alone.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "run with sudo" >&2
    exit 1
fi

RUNNER_ACCOUNT="${1:-}"
if [ -z "$RUNNER_ACCOUNT" ]; then
    echo "usage: sudo $0 <runner-account>" >&2
    exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
LABEL="dev.intendant.ci-watchdog"
LIB_DIR="/usr/local/lib/intendant-ci"
CONF_DIR="/etc/intendant-ci"
PLIST="/Library/LaunchDaemons/$LABEL.plist"

install -d -m 0755 "$LIB_DIR" "$CONF_DIR"
install -m 0755 "$HERE/fleet-watchdog.sh" "$LIB_DIR/fleet-watchdog.sh"

if [ ! -f "$CONF_DIR/watchdog.conf" ]; then
    uid=$(id -u "$RUNNER_ACCOUNT")
    home=$(dscl . -read "/Users/$RUNNER_ACCOUNT" NFSHomeDirectory | awk '{print $2}')
    labels=""
    for plist in "$home"/Library/LaunchAgents/actions.runner.*.plist; do
        [ -f "$plist" ] || continue
        base=$(basename "$plist" .plist)
        labels="$labels $base"
    done
    sed -e "s|^CACHE_ROOTS=.*|CACHE_ROOTS=\"$home/.cache/intendant-ci/target\"|" \
        -e "s|^RUNNER_USER=.*|RUNNER_USER=\"$RUNNER_ACCOUNT\"|" \
        -e "s|^RUNNER_UID=.*|RUNNER_UID=\"$uid\"|" \
        -e "s|^RUNNER_LABELS=.*|RUNNER_LABELS=\"${labels# }\"|" \
        -e "s|^RUNNER_PLIST_DIR=.*|RUNNER_PLIST_DIR=\"$home/Library/LaunchAgents\"|" \
        "$HERE/watchdog.conf.example" > "$CONF_DIR/watchdog.conf"
    chmod 0644 "$CONF_DIR/watchdog.conf"
    echo "seeded $CONF_DIR/watchdog.conf (detected listeners:${labels:- none}) — review it"
else
    echo "keeping existing $CONF_DIR/watchdog.conf"
fi

cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>$LIB_DIR/fleet-watchdog.sh</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>StartInterval</key><integer>300</integer>
</dict>
</plist>
PLIST_EOF
chmod 0644 "$PLIST"

launchctl bootout system "$PLIST" 2>/dev/null || true
launchctl bootstrap system "$PLIST"
echo "watchdog installed and running (label $LABEL, tick 300s)"
echo "verify: tail -f /var/log/intendant-ci-watchdog.log"
echo "rollback: sudo launchctl bootout system $PLIST && sudo rm $PLIST"
