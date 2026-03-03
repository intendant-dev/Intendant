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

Use display **:50** (intendant reserves :99+).

```bash
Xvfb :50 -screen 0 1280x720x24 &
x11vnc -display :50 -rfbport 5950 -nopw -forever -quiet &
DISPLAY=:50 xterm -geometry 120x35 -fa Monospace -fs 12 \
  -e bash -c 'source .env && ./target/release/intendant \
    --direct --autonomy low --control-socket \
    "your task" 2>/tmp/intendant-tui-stderr.log; sleep 120' &
```

## Screenshot

```bash
DISPLAY=:50 import -window root /tmp/tui-screenshot.png
```

## Control socket

Path: `/tmp/intendant-<PID>.sock` (find PID with `pgrep -a intendant`).

```bash
echo '{"action":"status"}' | socat - UNIX-CONNECT:/tmp/intendant-<PID>.sock
echo '{"action":"approve","id":<TURN>}' | socat - UNIX-CONNECT:/tmp/intendant-<PID>.sock
```

Messages: `status`, `approve` (needs `id`), `deny` (needs `id`), `skip` (needs `id`), `approve_all` (needs `id`), `set_autonomy` (needs `level`), `set_verbosity` (needs `level`: quiet/normal/verbose/debug), `input` (needs `text`), `quit`.

## Cleanup

```bash
pkill -f 'intendant --direct'; pkill -f 'Xvfb :50'
```
