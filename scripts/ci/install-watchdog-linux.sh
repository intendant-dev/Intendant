#!/bin/bash
# Install the fleet watchdog on a Linux runner host (run with sudo from
# a repo checkout: sudo scripts/ci/install-watchdog-linux.sh <runner-account>).
#
# Installs a root systemd service + 5-minute timer around
# scripts/ci/fleet-watchdog.sh. Idempotent: re-running upgrades the
# script and units, and leaves an existing watchdog.conf alone.
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
LIB_DIR="/usr/local/lib/intendant-ci"
CONF_DIR="/etc/intendant-ci"

install -d -m 0755 "$LIB_DIR" "$CONF_DIR"
install -m 0755 "$HERE/fleet-watchdog.sh" "$LIB_DIR/fleet-watchdog.sh"

if [ ! -f "$CONF_DIR/watchdog.conf" ]; then
    home=$(getent passwd "$RUNNER_ACCOUNT" | cut -d: -f6)
    units=$(systemctl list-unit-files 'actions.runner.*.service' --no-legend 2>/dev/null | awk '{print $1}' | tr '\n' ' ')
    sed -e "s|^CACHE_ROOTS=.*|CACHE_ROOTS=\"$home/.cache/intendant-ci/target\"|" \
        -e "s|^RUNNER_USER=.*|RUNNER_USER=\"$RUNNER_ACCOUNT\"|" \
        -e "s|^RUNNER_UNITS=.*|RUNNER_UNITS=\"${units% }\"|" \
        -e "s|^STATE_DIR=.*|STATE_DIR=\"/var/lib/intendant-ci\"|" \
        "$HERE/watchdog.conf.example" > "$CONF_DIR/watchdog.conf"
    chmod 0644 "$CONF_DIR/watchdog.conf"
    echo "seeded $CONF_DIR/watchdog.conf (detected units: ${units:-none}) — review it"
else
    echo "keeping existing $CONF_DIR/watchdog.conf"
fi

cat > /etc/systemd/system/intendant-ci-watchdog.service <<'EOF'
[Unit]
Description=Intendant fleet runner watchdog (one tick)

[Service]
Type=oneshot
ExecStart=/bin/bash /usr/local/lib/intendant-ci/fleet-watchdog.sh
EOF

cat > /etc/systemd/system/intendant-ci-watchdog.timer <<'EOF'
[Unit]
Description=Intendant fleet runner watchdog tick

[Timer]
OnBootSec=2min
OnUnitActiveSec=5min

[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable --now intendant-ci-watchdog.timer
echo "watchdog installed (systemd timer, tick 5min)"
echo "verify: journalctl -u intendant-ci-watchdog.service -f  (and /var/log/intendant-ci-watchdog.log)"
echo "rollback: sudo systemctl disable --now intendant-ci-watchdog.timer"
