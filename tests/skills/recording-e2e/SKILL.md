---
name: recording-e2e
description: >
  E2E test the display recording and replay system. Launches intendant --web
  with recording enabled, triggers Xvfb display creation, verifies ffmpeg
  recording starts, segments are created and serveable, and the replay UI
  loads in the browser. Asserts via /recordings and /debug HTTP endpoints.
  Human monitors via VNC on port 5950.
compatibility: Requires Xvfb, Firefox, x11vnc, ffmpeg, curl, xdotool
allowed-tools: Bash Read
disable-model-invocation: true
---

# Display Recording & Replay E2E Testing

## Overview

This skill tests the **display recording pipeline** end-to-end: intendant
auto-launches Xvfb, the recording listener detects `DisplayReady`, spawns
ffmpeg to record the display, segments accumulate on disk, and the web
dashboard replay UI loads and plays them back.

Tests the full path:
```
Agent task → Xvfb auto-launch → DisplayReady event → RecordingStarted →
ffmpeg x11grab → segmented MP4s → /recordings API → RecordingPlayer UI
```

All assertions use HTTP endpoints (`/recordings`, `/debug`) — no screenshots needed.
The graphical stack (Firefox on Xvfb :50) runs for human VNC observation.

## Prerequisites

```bash
# Install if missing — ffmpeg is REQUIRED, recording silently does nothing without it
sudo apt-get install -y xvfb x11vnc firefox-esr ffmpeg xdotool imagemagick curl
```

ffmpeg must support `libx264` and `x11grab`:
```bash
ffmpeg -hide_banner -encoders 2>/dev/null | grep libx264
ffmpeg -hide_banner -devices 2>/dev/null | grep x11grab
```

**IMPORTANT — Worktree builds**: When running from a git worktree (e.g.
`.worktrees/<name>/`), `cargo build` outputs to the **worktree's own
`target/` directory**, not the main repo's. Always use the binary from the
worktree's target dir:
```bash
# Correct (worktree binary):
/path/to/worktree/target/release/intendant

# Wrong (main repo binary — won't have your changes):
/home/user/projects/intendant/target/release/intendant
```

## Setup

### 1. Build the binary

```bash
REPO=$(git rev-parse --show-toplevel)
BIN="$REPO/target/release/intendant"
cd "$REPO"
cargo build --release --bin intendant --bin intendant-runtime
```

### 2. Reserve isolated test resources

Never stop another agent's Intendant, browser, Xvfb, or VNC process. This
example uses display `:50`, VNC port `5950`, and dashboard port `18766`; if
any is occupied, choose unused values and update the variables. The setup
persists every value that later snippets need in one deterministic,
worktree-local state file. An existing state file is treated as an active or
unclean run and is never overwritten; inspect and clean it up first.

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
BIN="$REPO/target/release/intendant"
STATE_FILE="$REPO/target/recording-e2e.state"
mkdir -p "$REPO/target"
if [ -e "$STATE_FILE" ] || [ -L "$STATE_FILE" ]; then
  echo "Refusing to overwrite stale recording E2E state: $STATE_FILE" >&2
  exit 1
fi

PORT=18766
VNC_PORT=5950
DISPLAY_NUM=50
DEBUG_PORT=6000
BASE="http://127.0.0.1:$PORT"
RUN_DIR=$(mktemp -d /tmp/intendant-recording-e2e.XXXXXX)
TESTDIR=$(mktemp -d /tmp/intendant-rec-test-XXXXXX)
PROFILE="$RUN_DIR/firefox-profile"
if pgrep -fa "^Xvfb :${DISPLAY_NUM}([[:space:]]|$)|^x11vnc .*:${DISPLAY_NUM}"; then
  echo "Display :$DISPLAY_NUM is already owned; choose an unused display" >&2
  rmdir "$TESTDIR" "$RUN_DIR"
  exit 1
fi
ss -ltn 2>/dev/null | grep -E ":(${VNC_PORT}|${PORT}|${DEBUG_PORT})\\b" && {
  echo "Choose unused VNC/dashboard/debugger ports before continuing" >&2
  rmdir "$TESTDIR" "$RUN_DIR"
  exit 1
}

# Create the final pathname exclusively. If this shell is interrupted after
# this point, the leftover file deliberately blocks a second run.
umask 077
if ! (set -o noclobber; : > "$STATE_FILE") 2>/dev/null; then
  echo "Another run created $STATE_FILE" >&2
  rmdir "$TESTDIR" "$RUN_DIR"
  exit 1
fi
{
  printf 'STATE_VERSION=1\n'
  printf 'STATE_REPO=%q\n' "$REPO"
  printf 'BIN=%q\nPORT=%q\nVNC_PORT=%q\nDISPLAY_NUM=%q\nDEBUG_PORT=%q\n' \
    "$BIN" "$PORT" "$VNC_PORT" "$DISPLAY_NUM" "$DEBUG_PORT"
  printf 'BASE=%q\nRUN_DIR=%q\nTESTDIR=%q\nPROFILE=%q\nSTREAM=%q\n' \
    "$BASE" "$RUN_DIR" "$TESTDIR" "$PROFILE" ""
  cat <<'STATE_VALIDATION'
# This tail runs whenever a later snippet sources the file. Keep the state
# owner-only: it is shell syntax, not an untrusted interchange format.
_recording_state_ok=1
[ "${STATE_VERSION:-}" = 1 ] || _recording_state_ok=0
_recording_current_repo=$(git rev-parse --show-toplevel 2>/dev/null) ||
  _recording_state_ok=0
[ "${STATE_REPO:-}" = "$_recording_current_repo" ] ||
  _recording_state_ok=0
[ "${STATE_FILE:-}" = "$STATE_REPO/target/recording-e2e.state" ] ||
  _recording_state_ok=0
[ "${BIN:-}" = "$STATE_REPO/target/release/intendant" ] ||
  _recording_state_ok=0
case "${RUN_DIR:-}" in
  /tmp/intendant-recording-e2e.?*) [ -d "$RUN_DIR" ] && [ ! -L "$RUN_DIR" ] ||
    _recording_state_ok=0 ;;
  *) _recording_state_ok=0 ;;
esac
case "${TESTDIR:-}" in
  /tmp/intendant-rec-test-?*) [ -d "$TESTDIR" ] && [ ! -L "$TESTDIR" ] ||
    _recording_state_ok=0 ;;
  *) _recording_state_ok=0 ;;
esac
[ "${PROFILE:-}" = "$RUN_DIR/firefox-profile" ] ||
  _recording_state_ok=0
for _recording_number in "${PORT:-}" "${VNC_PORT:-}" "${DISPLAY_NUM:-}" \
  "${DEBUG_PORT:-}"; do
  case "$_recording_number" in
    ''|*[!0-9]*) _recording_state_ok=0 ;;
  esac
done
[ "${BASE:-}" = "http://127.0.0.1:$PORT" ] ||
  _recording_state_ok=0
if [ "$_recording_state_ok" != 1 ]; then
  echo "Invalid or misplaced recording E2E state" >&2
  unset _recording_state_ok _recording_current_repo _recording_number
  return 1 2>/dev/null || exit 1
fi
unset _recording_state_ok _recording_current_repo _recording_number

recording_e2e_pid_start_ticks() {
  python3 - "$1" <<'PY'
import pathlib, sys
raw = pathlib.Path(f"/proc/{sys.argv[1]}/stat").read_text()
print(raw[raw.rfind(")") + 2:].split()[19])
PY
}
recording_e2e_pid_matches() {
  _recording_pid=$1 _recording_start=$2 _recording_match=$3
  case "$_recording_pid" in ''|*[!0-9]*) return 1 ;; esac
  [ "$_recording_pid" -gt 1 ] || return 1
  case "$_recording_start" in ''|*[!0-9]*) return 1 ;; esac
  [ "$(recording_e2e_pid_start_ticks "$_recording_pid" 2>/dev/null)" = \
    "$_recording_start" ] || return 1
  _recording_cmd=$(tr '\0' ' ' < "/proc/$_recording_pid/cmdline" 2>/dev/null) ||
    return 1
  case "$_recording_cmd" in *"$_recording_match"*) return 0 ;; *) return 1 ;; esac
}
recording_e2e_record_pid() {
  _recording_key=$1 _recording_pid=$2 _recording_match=$3
  case "$_recording_key" in XVFB|VNC|INTENDANT|FIREFOX) ;; *) return 1 ;; esac
  case "$_recording_pid" in ''|*[!0-9]*) return 1 ;; esac
  [ "$_recording_pid" -gt 1 ] || return 1
  _recording_start=$(recording_e2e_pid_start_ticks "$_recording_pid") ||
    return 1
  printf '%s_PID=%q\n%s_START=%q\n%s_MATCH=%q\n' \
    "$_recording_key" "$_recording_pid" \
    "$_recording_key" "$_recording_start" \
    "$_recording_key" "$_recording_match" >> "$STATE_FILE"
}
recording_e2e_stop_pid() {
  _recording_label=$1 _recording_pid=$2 _recording_start=$3 _recording_match=$4
  [ -n "$_recording_pid" ] || return 0
  case "$_recording_pid" in ''|*[!0-9]*) return 1 ;; esac
  [ "$_recording_pid" -gt 1 ] || return 1
  kill -0 "$_recording_pid" 2>/dev/null || return 0
  recording_e2e_pid_matches "$_recording_pid" "$_recording_start" \
    "$_recording_match" ||
    { echo "Refusing unverified $_recording_label PID $_recording_pid" >&2;
      return 1; }
  kill "$_recording_pid"
}
STATE_VALIDATION
} > "$STATE_FILE"
chmod 600 "$STATE_FILE"
echo "Recording E2E state: $STATE_FILE"
```

### 3. Create intendant.toml with recording enabled

Recording is disabled by default. Create a temporary project directory with
recording enabled and short segment duration for faster testing:

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
cat > "$TESTDIR/intendant.toml" << 'EOF'
[recording]
enabled = true
framerate = 10
segment_duration_secs = 8
quality = "low"
EOF
echo "Test project dir: $TESTDIR"
```

**Why these values?**
- `framerate = 10`: Lower than default (30) to reduce CPU during testing
- `segment_duration_secs = 8`: Short segments so we don't wait 60s for the first one
- `quality = "low"`: CRF 35, smallest files, faster encoding

### 4. Start Xvfb + x11vnc for human monitoring

The observer display is separate from Intendant's agent display. The example
uses `:50` only after the availability check above; Intendant reserves `:99+`
for its own Xvfb instances.

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

nohup Xvfb ":$DISPLAY_NUM" -screen 0 1280x720x24 > /dev/null 2>&1 &
XVFB_PID=$!
recording_e2e_record_pid XVFB "$XVFB_PID" "Xvfb :$DISPLAY_NUM" ||
  { kill "$XVFB_PID" 2>/dev/null || true; exit 1; }
sleep 0.5
nohup x11vnc -display ":$DISPLAY_NUM" -rfbport "$VNC_PORT" \
  -passwd intendant -forever -quiet > /dev/null 2>&1 &
VNC_PID=$!
recording_e2e_record_pid VNC "$VNC_PID" "-rfbport $VNC_PORT" ||
  { kill "$VNC_PID" 2>/dev/null || true; exit 1; }
sleep 0.5
```

### 5. Launch intendant --web with recording enabled

The task must trigger Xvfb auto-launch. Good tasks:
- "take a screenshot of the display" (triggers captureScreen → Xvfb launch)
- "run xeyes for 30 seconds then close it" (triggers execAsAgent with GUI app)
- "open xterm and run 'echo hello && sleep 30'" (triggers GUI terminal)

Use `--autonomy high` so the agent auto-approves safe commands without prompting.
Run from `$TESTDIR` so intendant picks up the `intendant.toml` config.

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

(
  cd "$TESTDIR"
  [ ! -f "$REPO/.env" ] || source "$REPO/.env"
  exec "$BIN" --direct --autonomy high --web "$PORT" --no-tls \
    --bind 127.0.0.1 \
    "run 'xeyes' in the background, then take a screenshot after 5 seconds. After the screenshot, run 'xclock' and wait 20 seconds."
) >"$RUN_DIR/intendant.log" 2>&1 &
INTENDANT_PID=$!
recording_e2e_record_pid INTENDANT "$INTENDANT_PID" "--web $PORT" ||
  { kill "$INTENDANT_PID" 2>/dev/null || true; exit 1; }
sleep 3
cat "$RUN_DIR/intendant.log"
```

### 6. Launch Firefox on display :50

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

mkdir -p "$PROFILE"
DISPLAY=":$DISPLAY_NUM" nohup firefox --no-remote --profile "$PROFILE" \
  --new-window "$BASE/app" > /dev/null 2>&1 &
FIREFOX_PID=$!
recording_e2e_record_pid FIREFOX "$FIREFOX_PID" "$PROFILE" ||
  { kill "$FIREFOX_PID" 2>/dev/null || true; exit 1; }
sleep 8
```

## Asserting on Recording State

Each block below deliberately reloads the owner-only state file. Do not omit
that preamble or rely on variables from a previous agent/runtime invocation.

### Wait for recording to start

The recording starts automatically when the agent triggers Xvfb. Poll the
`/recordings` endpoint until a stream appears:

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

for i in $(seq 1 60); do
  STREAMS=$(curl -s "$BASE/recordings" 2>/dev/null)
  COUNT=$(echo "$STREAMS" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(len(data))
except: print(0)
" 2>/dev/null)
  [ "$COUNT" != "0" ] && [ "$COUNT" != "" ] && break
  sleep 1
done
echo "Recording streams found: $COUNT"
echo "$STREAMS" | python3 -m json.tool 2>/dev/null
```

### Verify recording stream metadata

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

curl -s "$BASE/recordings" | python3 -c "
import sys, json
data = json.load(sys.stdin)
assert len(data) > 0, 'No recording streams found'
stream = data[0]
name = stream.get('stream_name', '')
print(f'Stream: {name}')
assert name.startswith('display_'), f'Expected display_ stream, got: {name}'

manifest = stream.get('manifest', {})
print(f'Source: {manifest.get(\"source\", \"unknown\")}')
print(f'Codec: {manifest.get(\"codec\", \"unknown\")}')
print(f'FPS: {manifest.get(\"framerate\", \"unknown\")}')
print(f'Resolution: {manifest.get(\"resolution\", \"unknown\")}')
assert manifest.get('source') == 'x11grab', f'Expected x11grab source'
assert manifest.get('codec') == 'h264', f'Expected h264 codec'
print('Stream metadata OK')
"
```

### Wait for first segment to appear

With `segment_duration_secs = 8`, the first segment finalizes after ~8 seconds
of recording. ffmpeg writes to `segments.csv` when a segment completes.

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

# Extract stream name first
STREAM=$(curl -s "$BASE/recordings" | python3 -c "
import sys, json
data = json.load(sys.stdin)
print(data[0]['stream_name'] if data else '')
" 2>/dev/null)
case "$STREAM" in
  display_*) case "$STREAM" in *[!A-Za-z0-9_.-]*) false ;; *) true ;; esac ;;
  *) false ;;
esac || { echo "Unsafe or missing stream name: $STREAM" >&2; exit 1; }
printf 'STREAM=%q\n' "$STREAM" >> "$STATE_FILE"
echo "Waiting for segments on stream: $STREAM"

for i in $(seq 1 30); do
  SEGCOUNT=$(curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(len(data))
except: print(0)
" 2>/dev/null)
  [ "$SEGCOUNT" != "0" ] && [ "$SEGCOUNT" != "" ] && break
  sleep 1
done
echo "Segments found: $SEGCOUNT"
```

### Verify segment metadata

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
[ -n "${STREAM:-}" ] ||
  { echo "STREAM is not persisted in $STATE_FILE" >&2; exit 1; }

curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys, json
segments = json.load(sys.stdin)
assert len(segments) > 0, 'No segments found'

seg = segments[0]
print(f'First segment: {seg[\"filename\"]}')
print(f'  Start: {seg[\"start_secs\"]}s')
print(f'  End: {seg[\"end_secs\"]}s')
print(f'  Duration: {seg[\"end_secs\"] - seg[\"start_secs\"]}s')
assert seg['filename'].startswith('seg_'), f'Bad filename: {seg[\"filename\"]}'
assert seg['filename'].endswith('.mp4'), f'Not MP4: {seg[\"filename\"]}'
assert seg['end_secs'] > seg['start_secs'], 'Segment has zero duration'
print(f'Total segments: {len(segments)}')
print('Segment metadata OK')
"
```

### Verify segment file is serveable and valid MP4

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
[ -n "${STREAM:-}" ] ||
  { echo "STREAM is not persisted in $STATE_FILE" >&2; exit 1; }

# Get first segment filename
SEG_FILE=$(curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys, json
data = json.load(sys.stdin)
print(data[0]['filename'] if data else '')
" 2>/dev/null)

# Download segment and check it's a valid MP4
curl -s "$BASE/recordings/$STREAM/$SEG_FILE" -o "$RUN_DIR/test_segment.mp4"
FILE_SIZE=$(stat -c%s "$RUN_DIR/test_segment.mp4" 2>/dev/null || echo 0)
echo "Segment file size: $FILE_SIZE bytes"

# Verify it's a valid MP4 with ffprobe
ffprobe -v quiet -print_format json -show_format -show_streams "$RUN_DIR/test_segment.mp4" | python3 -c "
import sys, json
data = json.load(sys.stdin)
fmt = data.get('format', {})
streams = data.get('streams', [])
assert len(streams) > 0, 'No streams in MP4'

video = [s for s in streams if s.get('codec_type') == 'video']
assert len(video) > 0, 'No video stream in MP4'

v = video[0]
print(f'Codec: {v.get(\"codec_name\")}')
print(f'Resolution: {v.get(\"width\")}x{v.get(\"height\")}')
print(f'Duration: {fmt.get(\"duration\", \"unknown\")}s')
print(f'Format: {fmt.get(\"format_name\")}')
assert v.get('codec_name') == 'h264', f'Expected h264, got {v.get(\"codec_name\")}'
print('Segment file valid MP4 OK')
"
```

### Verify path traversal protection

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
[ -n "${STREAM:-}" ] ||
  { echo "STREAM is not persisted in $STATE_FILE" >&2; exit 1; }

# These should return 400 or 404, not serve files
STATUS=$(curl --path-as-is -s -o /dev/null -w "%{http_code}" "$BASE/recordings/$STREAM/../../../etc/passwd")
echo "Path traversal attempt: HTTP $STATUS"
[ "$STATUS" = "400" ] || [ "$STATUS" = "404" ] && echo "Path traversal blocked OK" || echo "WARN: unexpected status"

STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/recordings/$STREAM/notaseg.mp4")
echo "Invalid filename: HTTP $STATUS"
[ "$STATUS" = "400" ] && echo "Invalid filename rejected OK" || echo "WARN: unexpected status"
```

### Verify multiple segments accumulate over time

Wait for at least 2 segments (requires ~16s with segment_duration_secs=8):

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
[ -n "${STREAM:-}" ] ||
  { echo "STREAM is not persisted in $STATE_FILE" >&2; exit 1; }

for i in $(seq 1 30); do
  SEGCOUNT=$(curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys, json
try: print(len(json.load(sys.stdin)))
except: print(0)
" 2>/dev/null)
  [ "$SEGCOUNT" -ge 2 ] 2>/dev/null && break
  sleep 2
done
echo "Segments after waiting: $SEGCOUNT"

curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys, json
segments = json.load(sys.stdin)
assert len(segments) >= 2, f'Expected >= 2 segments, got {len(segments)}'
# Verify segments are contiguous
for i in range(1, len(segments)):
    gap = abs(segments[i]['start_secs'] - segments[i-1]['end_secs'])
    assert gap < 1.0, f'Gap between segments {i-1} and {i}: {gap}s'
print(f'{len(segments)} contiguous segments OK')

# Verify total duration makes sense
total = segments[-1]['end_secs']
print(f'Total recorded duration: {total:.1f}s')
"
```

## Asserting on Replay UI via Firefox

### Verify recording section is visible in browser

Use `ff-eval.py` if Firefox debugger is active, or use xdotool to navigate.

If Firefox was launched with `--start-debugger-server 6000`:
```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

# Configure only this run's isolated Firefox profile.
grep -q 'devtools.debugger.remote-enabled' "$PROFILE/user.js" 2>/dev/null || \
cat >> "$PROFILE/user.js" << 'DEOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
DEOF
```

Then relaunch Firefox with debugger:
```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

if kill -0 "${FIREFOX_PID:-}" 2>/dev/null; then
  recording_e2e_stop_pid Firefox "$FIREFOX_PID" "${FIREFOX_START:-}" \
    "${FIREFOX_MATCH:-}" || exit 1
  for _ in $(seq 1 20); do
    kill -0 "$FIREFOX_PID" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$FIREFOX_PID" 2>/dev/null; then
    recording_e2e_pid_matches "$FIREFOX_PID" "$FIREFOX_START" \
      "$FIREFOX_MATCH" ||
      { echo "Firefox PID identity changed while stopping" >&2; exit 1; }
    kill -9 "$FIREFOX_PID"
  fi
fi
sleep 1
DISPLAY=":$DISPLAY_NUM" nohup firefox --no-remote --profile "$PROFILE" \
  --start-debugger-server "$DEBUG_PORT" --new-window "$BASE/app" \
  > /dev/null 2>&1 &
FIREFOX_PID=$!
recording_e2e_record_pid FIREFOX "$FIREFOX_PID" "$PROFILE" ||
  { kill "$FIREFOX_PID" 2>/dev/null || true; exit 1; }
sleep 8
```

Check recording UI state via JavaScript:
```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
cd "$REPO" || exit 1

# Switch to the Live display destination (internal route id: displays).
python3 scripts/ff-eval.py "switchTab('displays'); activeTab"
sleep 1

# Check recording section visibility
python3 scripts/ff-eval.py "
  const section = document.getElementById('recording-section');
  const hidden = section?.classList.contains('hidden');
  const select = document.getElementById('recording-stream-select');
  const options = select ? select.options.length : 0;
  JSON.stringify({visible: !hidden, streamCount: options})
"
# Expected: {"visible":true,"streamCount":1} (or more)
```

### Verify RecordingPlayer loaded segments

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
cd "$REPO" || exit 1

python3 scripts/ff-eval.py "
  const player = recPlayer;
  if (!player) 'no player';
  else JSON.stringify({
    streamName: player.streamName,
    segmentCount: player.segments.length,
    totalDuration: player.totalDuration,
    currentSegIdx: player.currentSegIdx,
    playing: player.playing
  })
"
# Expected: segments > 0, totalDuration > 0
```

### Test playback controls

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
cd "$REPO" || exit 1

# Start playback
python3 scripts/ff-eval.py "
  const btn = document.getElementById('rec-play-btn');
  btn?.click();
  'play clicked'
"
sleep 2

# Check if playing
python3 scripts/ff-eval.py "
  const player = recPlayer;
  JSON.stringify({
    playing: player?.playing,
    currentTime: player?.globalTime(),
    videoReadyState: player?.video?.readyState
  })
"
# Expected: playing: true, currentTime > 0

# Pause
python3 scripts/ff-eval.py "
  document.getElementById('rec-play-btn')?.click();
  'pause clicked'
"
```

### Test speed control

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
cd "$REPO" || exit 1

python3 scripts/ff-eval.py "
  const select = document.getElementById('rec-speed');
  select.value = '4';
  select.dispatchEvent(new Event('change'));
  'speed set to 4x'
"
sleep 1
python3 scripts/ff-eval.py "window.recPlayer?.video?.playbackRate"
# Expected: 4
```

### Test timeline seeking

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1
cd "$REPO" || exit 1

# Seek to middle of recording
python3 scripts/ff-eval.py "
  const player = window.recPlayer;
  const mid = player.totalDuration / 2;
  player.seekToGlobal(mid);
  JSON.stringify({seekedTo: mid, currentTime: player.globalTime()})
"
# Expected: currentTime near seekedTo value
```

## WebSocket Event Verification

Connect via WebSocket and verify recording events are broadcast:

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

python3 -c "
import asyncio, json, websockets

async def check_events():
    async with websockets.connect('ws://127.0.0.1:$PORT') as ws:
        events_seen = set()
        for _ in range(50):  # Read up to 50 messages
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=2)
                data = json.loads(msg)
                event = data.get('event', '')
                if 'recording' in event.lower():
                    events_seen.add(event)
                    print(f'Recording event: {event} — {json.dumps(data)}')
            except asyncio.TimeoutError:
                break
            except: pass
        print(f'Recording events seen: {events_seen}')

asyncio.run(check_events())
" 2>/dev/null
```

**Note:** `recording_started` events may have already been broadcast before the
WebSocket connects. Use `/recordings` endpoint for reliable state checks.

## Example Full Test Scenario

```bash
# ── Setup (standalone condensed variant; use unused endpoints) ──
REPO=$(git rev-parse --show-toplevel)
BIN="$REPO/target/release/intendant"
PORT=18766
BASE="http://127.0.0.1:$PORT"
RUN_DIR=$(mktemp -d /tmp/intendant-recording-e2e.XXXXXX)
TESTDIR=$(mktemp -d /tmp/intendant-rec-test-XXXXXX)
pid_start_ticks() {
  python3 - "$1" <<'PY'
import pathlib, sys
raw = pathlib.Path(f"/proc/{sys.argv[1]}/stat").read_text()
print(raw[raw.rfind(")") + 2:].split()[19])
PY
}
stop_owned_pid() {
  _pid=$1 _start=$2 _match=$3
  [ -n "$_pid" ] || return 0
  case "$_pid" in *[!0-9]*) return 1 ;; esac
  [ "$_pid" -gt 1 ] || return 1
  case "$_start" in ''|*[!0-9]*) return 1 ;; esac
  kill -0 "$_pid" 2>/dev/null || return 0
  [ "$(pid_start_ticks "$_pid" 2>/dev/null)" = "$_start" ] || return 1
  _cmd=$(tr '\0' ' ' < "/proc/$_pid/cmdline" 2>/dev/null) || return 1
  case "$_cmd" in *"$_match"*) kill "$_pid" ;; *) return 1 ;; esac
}
cat > "$TESTDIR/intendant.toml" << 'EOF'
[recording]
enabled = true
framerate = 10
segment_duration_secs = 8
quality = "low"
EOF

# Refuse resource collisions; never kill unrelated processes.
if pgrep -fa '^Xvfb :50([[:space:]]|$)|^x11vnc .*:50'; then
  echo "Display :50 is already owned; choose an unused display"; exit 1
fi
ss -ltn 2>/dev/null | grep -E ':(5950|18766)\b' && exit 1

# Start observer Xvfb
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
XVFB_PID=$!
XVFB_START=$(pid_start_ticks "$XVFB_PID") || exit 1
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &
VNC_PID=$!
VNC_START=$(pid_start_ticks "$VNC_PID") || exit 1

# Launch intendant
(
  cd "$TESTDIR"
  [ ! -f "$REPO/.env" ] || source "$REPO/.env"
  exec "$BIN" --direct --autonomy high --web "$PORT" --no-tls \
    --bind 127.0.0.1 "run 'xeyes' in the background, then wait 30 seconds"
) >"$RUN_DIR/intendant.log" 2>&1 &
INTENDANT_PID=$!
INTENDANT_START=$(pid_start_ticks "$INTENDANT_PID") || exit 1
sleep 3
cat "$RUN_DIR/intendant.log"

# ── Assert 1: Recording stream exists ──
for i in $(seq 1 60); do
  COUNT=$(curl -s "$BASE/recordings" 2>/dev/null | python3 -c "
import sys,json
try: print(len(json.load(sys.stdin)))
except: print(0)" 2>/dev/null)
  [ "$COUNT" != "0" ] && [ "$COUNT" != "" ] && break; sleep 1
done
echo "ASSERT 1: Streams found: $COUNT"

# ── Assert 2: Manifest has correct metadata ──
curl -s "$BASE/recordings" | python3 -c "
import sys, json
data = json.load(sys.stdin)
assert len(data) > 0, 'FAIL: No streams'
s = data[0]
assert s['manifest']['source'] == 'x11grab'
assert s['manifest']['codec'] == 'h264'
print(f'ASSERT 2 PASS: {s[\"stream_name\"]} — {s[\"manifest\"][\"source\"]}, {s[\"manifest\"][\"codec\"]}')"

# ── Assert 3: Wait for segments ──
STREAM=$(curl -s "$BASE/recordings" | python3 -c "
import sys,json; print(json.load(sys.stdin)[0]['stream_name'])" 2>/dev/null)

for i in $(seq 1 45); do
  SEGCOUNT=$(curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys,json
try: print(len(json.load(sys.stdin)))
except: print(0)" 2>/dev/null)
  [ "$SEGCOUNT" -ge 1 ] 2>/dev/null && break; sleep 1
done
echo "ASSERT 3: Segments: $SEGCOUNT"

# ── Assert 4: Segment is valid MP4 ──
SEG_FILE=$(curl -s "$BASE/recordings/$STREAM/segments" | python3 -c "
import sys,json; print(json.load(sys.stdin)[0]['filename'])" 2>/dev/null)
curl -s "$BASE/recordings/$STREAM/$SEG_FILE" -o "$RUN_DIR/test_seg.mp4"
ffprobe -v quiet -print_format json -show_streams "$RUN_DIR/test_seg.mp4" | python3 -c "
import sys, json
data = json.load(sys.stdin)
video = [s for s in data['streams'] if s['codec_type']=='video']
assert len(video) > 0, 'FAIL: No video stream'
assert video[0]['codec_name'] == 'h264', 'FAIL: Not h264'
print(f'ASSERT 4 PASS: {video[0][\"codec_name\"]} {video[0][\"width\"]}x{video[0][\"height\"]}')"

# ── Assert 5: Path traversal blocked ──
STATUS=$(curl --path-as-is -s -o /dev/null -w "%{http_code}" "$BASE/recordings/$STREAM/../../../etc/passwd")
echo "ASSERT 5: Path traversal HTTP $STATUS (expect 400 or 404)"

# ── Cleanup ──
stop_owned_pid "$INTENDANT_PID" "$INTENDANT_START" "--web $PORT" &&
  stop_owned_pid "$VNC_PID" "$VNC_START" "-rfbport 5950" &&
  stop_owned_pid "$XVFB_PID" "$XVFB_START" "Xvfb :50" ||
  { echo "Refusing cleanup: an owned PID could not be verified" >&2; exit 1; }
case "$TESTDIR" in /tmp/intendant-rec-test-?*) rm -rf -- "$TESTDIR" ;; *) exit 1 ;; esac
case "$RUN_DIR" in /tmp/intendant-recording-e2e.?*) rm -rf -- "$RUN_DIR" ;; *) exit 1 ;; esac
echo "Done"
```

## Troubleshooting

### No recording streams appear

1. **Check intendant.toml**: Recording must be `enabled = true`. Verify intendant
   found the config:
   ```bash
   REPO=$(git rev-parse --show-toplevel) || exit 1
   STATE_FILE="$REPO/target/recording-e2e.state"
   [ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
     { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
   . "$STATE_FILE" || exit 1
   grep -i record "$RUN_DIR/intendant.log"
   ```
2. **Check ffmpeg**: `ffmpeg -version` must succeed. libx264 and x11grab required.
3. **Check Xvfb auto-launch**: The agent must trigger a command that needs a display.
   If the task doesn't involve GUI commands, no Xvfb launches, no recording starts.
4. **Check DisplayReady event**: Connect via WebSocket and look for `display_ready`:
   ```bash
   REPO=$(git rev-parse --show-toplevel) || exit 1
   STATE_FILE="$REPO/target/recording-e2e.state"
   [ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
     { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
   . "$STATE_FILE" || exit 1
   python3 -c "
   import asyncio, json, websockets
   async def watch():
       async with websockets.connect('ws://127.0.0.1:$PORT') as ws:
           for _ in range(100):
               msg = await asyncio.wait_for(ws.recv(), timeout=30)
               d = json.loads(msg)
               if d.get('event') in ('display_ready','recording_started','recording_error'):
                   print(json.dumps(d, indent=2))
   asyncio.run(watch())
   " 2>/dev/null
   ```

### Segments never appear (recording started but no segments.csv)

- ffmpeg only writes to `segments.csv` when a segment **completes** (i.e., after
  `segment_duration_secs` of recording). With default 60s, you'd wait a full minute.
  Use `segment_duration_secs = 8` for testing.
- Check if ffmpeg is actually running:
  ```bash
  pgrep -fa 'ffmpeg.*x11grab'
  ```
- Check the session recording directory directly:
  ```bash
  IHOME="${INTENDANT_HOME:-$HOME/.intendant}"
  find "$IHOME/logs/" -path '*/recordings/*' -name '*.mp4' -ls 2>/dev/null | tail -5
  ```

### Segment serves but ffprobe fails

- The segment may still be in progress (partial write). Wait for the segment to
  finalize (next segment starts, or recording stops).
- Verify with: `ffprobe -v error "$RUN_DIR/test_seg.mp4"`

### RecordingPlayer shows 0 segments in browser

- The player fetches from `/recordings/{stream}/segments`. If this returns `[]`,
  segments haven't been finalized yet.
- The player refreshes every 5 seconds for active recordings. Wait and reload.
- Check browser console for fetch errors (CORS, 404).

### Firefox can't play MP4 segments

- Firefox ESR may lack H.264 support on some Linux distros. Install:
  ```bash
  sudo apt-get install -y libavcodec-extra
  ```
- Check browser console for "media resource could not be decoded" errors.

## Cleanup

```bash
REPO=$(git rev-parse --show-toplevel) || exit 1
STATE_FILE="$REPO/target/recording-e2e.state"
[ -f "$STATE_FILE" ] && [ ! -L "$STATE_FILE" ] && [ -O "$STATE_FILE" ] ||
  { echo "Missing or unsafe state file: $STATE_FILE" >&2; exit 1; }
. "$STATE_FILE" || exit 1

cleanup_ok=1
recording_e2e_stop_pid Firefox "${FIREFOX_PID:-}" "${FIREFOX_START:-}" \
  "${FIREFOX_MATCH:-}" || cleanup_ok=0
recording_e2e_stop_pid Intendant "${INTENDANT_PID:-}" "${INTENDANT_START:-}" \
  "${INTENDANT_MATCH:-}" || cleanup_ok=0
recording_e2e_stop_pid x11vnc "${VNC_PID:-}" "${VNC_START:-}" \
  "${VNC_MATCH:-}" || cleanup_ok=0
recording_e2e_stop_pid Xvfb "${XVFB_PID:-}" "${XVFB_START:-}" \
  "${XVFB_MATCH:-}" || cleanup_ok=0
[ "$cleanup_ok" = 1 ] ||
  { echo "Leaving state and directories for manual inspection" >&2; exit 1; }

# The sourced state already required real, non-symlink directories with these
# exact prefixes. Recheck immediately before the destructive operation.
case "$TESTDIR" in
  /tmp/intendant-rec-test-?*) [ -d "$TESTDIR" ] && [ ! -L "$TESTDIR" ] ||
    exit 1; rm -rf -- "$TESTDIR" ;;
  *) exit 1 ;;
esac
case "$RUN_DIR" in
  /tmp/intendant-recording-e2e.?*) [ -d "$RUN_DIR" ] && [ ! -L "$RUN_DIR" ] ||
    exit 1; rm -rf -- "$RUN_DIR" ;;
  *) exit 1 ;;
esac
rm -f -- "$STATE_FILE"
```
