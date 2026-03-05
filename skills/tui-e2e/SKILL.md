---
name: tui-e2e
description: >
  E2E test the intendant TUI on a virtual display. Launches Xvfb, runs the
  TUI in xterm, takes screenshots, and controls it via the Unix socket.
compatibility: Requires Xvfb, xterm, ImageMagick (import), socat, x11vnc
allowed-tools: Bash Read
disable-model-invocation: true
---

# Test TUI E2E

Install prerequisites: `sudo apt-get install -y socat x11vnc`

## Launch

**IMPORTANT:** Always use display **:50** (intendant reserves :99+ for its own Xvfb).
Always start `x11vnc` so the human can follow along via VNC on port 5950.
Both Xvfb and x11vnc MUST be started before launching xterm.

```bash
# 1. Kill stale processes from prior runs
pkill -f 'Xvfb :50' 2>/dev/null; pkill -f 'x11vnc.*:50' 2>/dev/null
pkill -f 'intendant.*control-socket' 2>/dev/null; sleep 0.5

# 2. Start Xvfb + x11vnc (MANDATORY — human needs VNC to observe)
Xvfb :50 -screen 0 1280x720x24 &
sleep 0.5
x11vnc -display :50 -rfbport 5950 -nopw -forever -quiet &
sleep 0.5

# 3. Launch intendant in xterm on display :50
# NOTE: 100x30 fits inside 1280x720. Larger geometries (e.g. 120x35 at fs 12)
# overflow the screen and clip the bottom panel rows.
DISPLAY=:50 xterm -geometry 100x30 -fa Monospace -fs 12 \
  -e bash -c 'source .env && ./target/release/intendant \
    --direct --autonomy low --control-socket \
    "your task" 2>/tmp/intendant-tui-stderr.log; sleep 120' &
```

## Screenshot

Always use `DISPLAY=:50`:

```bash
DISPLAY=:50 import -window root /tmp/tui-screenshot.png
```

## Control socket

Path: `/tmp/intendant-<PID>.sock` (find PID with `pgrep -a intendant`).

```bash
echo '{"action":"status"}' | socat - UNIX-CONNECT:/tmp/intendant-<PID>.sock
echo '{"action":"approve","id":<TURN>}' | socat - UNIX-CONNECT:/tmp/intendant-<PID>.sock
```

Messages: `status`, `approve` (needs `id`), `deny` (needs `id`), `skip` (needs `id`), `approve_all` (needs `id`), `set_autonomy` (needs `level`), `set_verbosity` (needs `level`: quiet/normal/verbose/debug), `input` (needs `text` — for askHuman only), `follow_up` (needs `text` — for follow-up input after a round completes), `quit`.

## Cleanup

```bash
pkill -f 'intendant.*control-socket'; pkill -f 'Xvfb :50'; pkill -f 'x11vnc.*:50'
```
