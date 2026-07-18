---
name: voice-e2e
description: >
  E2E test the live audio pipeline with real audio. Uses espeak-ng TTS +
  PulseAudio virtual mic to feed synthesized speech to Gemini Live or
  OpenAI Realtime through the browser's getUserMedia on /app.
  Asserts via /debug endpoint JSON.
  Human monitors via VNC on port 5950.
  Tests the full audio path:
  TTS -> virtual mic -> Firefox -> AudioWorklet -> WASM -> live model -> tool calls -> agent.
compatibility: Requires Xvfb, Firefox, x11vnc, espeak-ng, ffmpeg, PulseAudio, xdotool
allowed-tools: Bash Read
disable-model-invocation: true
---

# Live Audio E2E Testing with Real Audio

## Overview

This skill tests the **full audio pipeline** end-to-end: synthesized speech flows
through a PulseAudio virtual microphone into Firefox's `getUserMedia`, through the
AudioWorklet and WASM layer, to the live model (Gemini Live or OpenAI Realtime),
which processes the audio and emits tool calls that drive the agent.

Unlike `web-e2e` (which uses `app.send_text()` to bypass audio), this tests the
actual audio capture, resampling, PCM conversion, and WebSocket audio streaming.

All assertions use the `/debug` JSON endpoint — no screenshots needed.
The graphical stack (Firefox on Xvfb) runs for human VNC observation.

The browser opens `/app` which has full live mode built into the WASM layer.

## Architecture

```
espeak-ng "text"
    |
    v
ffmpeg (resample to 48kHz mono s16le)      <-- match browser's native AudioContext rate
    |
    v
paplay --device=$VIRTUAL_MIC (PulseAudio)
    |
    v
$VIRTUAL_MIC.monitor (PulseAudio source)   <-- Firefox sees this as default mic
    |
    v
Firefox getUserMedia({audio: true})
    |
    v
AudioWorklet (audio-processor.js)
    |
    v
WASM (presence-web PresenceWeb) -- resample to 16kHz/24kHz, PCM16, base64
    |
    v
WebSocket to live model (Gemini Live / OpenAI Realtime)
    |
    v
Live model processes audio, emits tool calls + audio responses
    |
    v
WASM callbacks -> browser UI -> WebSocket -> intendant agent
```

## Prerequisites

```bash
# Install if missing
sudo apt-get install -y espeak-ng ffmpeg pulseaudio pulseaudio-utils pipewire-alsa
```

**`pipewire-alsa` is critical** — Firefox-ESR uses ALSA for audio. Without the
PipeWire-ALSA bridge, Firefox cannot see PipeWire virtual audio devices at all
(`enumerateDevices()` returns 0 devices, `getUserMedia` fails with NotFoundError).

PulseAudio/PipeWire must be running (`pactl info` should succeed).

**Only one browser can be active.** If you have `/app` open on another device
(e.g. your host browser), close it or disconnect its voice first — otherwise
the VM Firefox is passive and its audio is ignored by the presence layer.

## Setup

### 1. Reserve isolated test resources

Never stop another agent's Intendant, browser, Xvfb, or VNC process. This
example uses display `:50`, VNC port `5950`, debugger port `6000`, and
dashboard port `18767`; if any is occupied, choose unused values and update
the variables.

```bash
set -euo pipefail
REPO=$(git rev-parse --show-toplevel)
(
  cd "$REPO"
  cargo build --release --bin intendant --bin intendant-runtime
)
BIN="$REPO/target/release/intendant"
PORT=18767
BASE="http://127.0.0.1:$PORT"
DISPLAY_NUM=50
DISPLAY_NAME=":$DISPLAY_NUM"
VNC_PORT=5950
DEBUG_PORT=6000
STATE_ROOT="$REPO/target/voice-e2e"
ACTIVE_DIR="$STATE_ROOT/active"
STATE_FILE="$ACTIVE_DIR/run.env"
RUN_DIR="$ACTIVE_DIR/run"
PROFILE="$RUN_DIR/firefox-profile"
SAY_HELPER="$RUN_DIR/say"

if pgrep -fa "^Xvfb ${DISPLAY_NAME}([[:space:]]|$)|^x11vnc .*${DISPLAY_NAME}"; then
  echo "Display $DISPLAY_NAME is already owned; choose an unused display"; exit 1
fi
ss -ltn 2>/dev/null | grep -E ":(${VNC_PORT}|${DEBUG_PORT}|${PORT})\\b" && {
  echo "Choose unused display/VNC/debug/dashboard values before continuing"; exit 1;
}

# mkdir is the per-worktree run lock. Never overwrite a stale run: inspect it
# and use Cleanup below, or choose a different worktree.
mkdir -p "$STATE_ROOT"
if ! mkdir "$ACTIVE_DIR" 2>/dev/null; then
  echo "Refusing to overwrite stale voice-E2E state: $STATE_FILE" >&2
  exit 1
fi
mkdir "$RUN_DIR"
umask 077
{
  printf 'REPO=%q\n' "$REPO"
  printf 'BIN=%q\n' "$BIN"
  printf 'PORT=%q\n' "$PORT"
  printf 'BASE=%q\n' "$BASE"
  printf 'DISPLAY_NUM=%q\n' "$DISPLAY_NUM"
  printf 'DISPLAY_NAME=%q\n' "$DISPLAY_NAME"
  printf 'VNC_PORT=%q\n' "$VNC_PORT"
  printf 'DEBUG_PORT=%q\n' "$DEBUG_PORT"
  printf 'STATE_ROOT=%q\n' "$STATE_ROOT"
  printf 'ACTIVE_DIR=%q\n' "$ACTIVE_DIR"
  printf 'STATE_FILE=%q\n' "$STATE_FILE"
  printf 'RUN_DIR=%q\n' "$RUN_DIR"
  printf 'PROFILE=%q\n' "$PROFILE"
  printf 'SAY_HELPER=%q\n' "$SAY_HELPER"
} >"$STATE_FILE"
chmod 600 "$STATE_FILE"
```

Every later block reloads this file. A missing, stale, or incomplete file is a
hard failure; do not reconstruct PIDs or PulseAudio IDs with `pgrep`.

### 2. Create PulseAudio virtual microphone

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || {
  echo "Voice-E2E state belongs to another checkout" >&2; exit 1;
}
[[ -z "${VIRTUAL_MIC:-}" && -z "${PULSE_MODULE_ID:-}" ]] || {
  echo "PulseAudio ownership is already recorded; clean up before retrying" >&2; exit 1;
}

# Create a run-unique null sink — its .monitor becomes the virtual mic source.
VIRTUAL_MIC="intendant_voice_$$"
PREV_SOURCE=$(pactl get-default-source)
printf 'VIRTUAL_MIC=%q\nPREV_SOURCE=%q\n' "$VIRTUAL_MIC" "$PREV_SOURCE" >>"$STATE_FILE"
PULSE_MODULE_ID=$(pactl load-module module-null-sink sink_name="$VIRTUAL_MIC" \
  sink_properties=device.description="VirtualMic" \
  rate=48000 channels=1 format=s16le)
printf 'PULSE_MODULE_ID=%q\n' "$PULSE_MODULE_ID" >>"$STATE_FILE"
[[ "$PULSE_MODULE_ID" =~ ^[0-9]+$ ]] || {
  echo "PulseAudio returned a nonnumeric module ID" >&2; exit 1;
}

# Set the virtual mic monitor as the default recording source
pactl set-default-source "$VIRTUAL_MIC.monitor"

# Verify
pactl list short sources | grep "$VIRTUAL_MIC"
# Should show: $VIRTUAL_MIC.monitor

# Persist the speech helper itself; shell functions do not survive later
# agent/runtime invocations.
cat >"$SAY_HELPER" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
ACTIVE_DIR=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
STATE_FILE="$ACTIVE_DIR/run.env"
[[ -r "$STATE_FILE" ]] || { echo "Missing voice-E2E state: $STATE_FILE" >&2; exit 1; }
source "$STATE_FILE"
[[ "$SAY_HELPER" == "$ACTIVE_DIR/run/say" && "$VIRTUAL_MIC" =~ ^intendant_voice_[0-9]+$ ]] || {
  echo "Invalid voice-E2E helper state" >&2; exit 1;
}
[[ $# -ge 1 && $# -le 2 ]] || { echo "usage: say TEXT [WPM]" >&2; exit 2; }
speed=${2:-140}
[[ "$speed" =~ ^[0-9]+$ && "$speed" -ge 80 && "$speed" -le 450 ]] || {
  echo "WPM must be an integer from 80 through 450" >&2; exit 2;
}
pactl list short sinks | awk -v sink="$VIRTUAL_MIC" '$2 == sink { found=1 } END { exit !found }' || {
  echo "Owned virtual microphone is not loaded: $VIRTUAL_MIC" >&2; exit 1;
}
espeak-ng "$1" -s "$speed" --stdout |
  ffmpeg -loglevel error -i pipe:0 -f s16le -ar 48000 -ac 1 pipe:1 |
  paplay --device="$VIRTUAL_MIC" --format=s16le --rate=48000 --channels=1 --raw
EOF
chmod 700 "$SAY_HELPER"
```

**Why 48kHz?** The browser's `AudioContext` uses the system's native sample rate
(typically 48kHz). The WASM layer handles downsampling to the live model's
target rate (16kHz for Gemini, 24kHz for OpenAI). We match the browser rate
at the PulseAudio level so no extra resampling happens in PulseAudio.

### 3. Start Xvfb + x11vnc

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
[[ -z "${XVFB_PID:-}" && -z "${VNC_PID:-}" ]] || {
  echo "Display PIDs are already recorded; clean up before retrying" >&2; exit 1;
}

nohup Xvfb "$DISPLAY_NAME" -screen 0 1280x720x24 >"$RUN_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
printf 'XVFB_PID=%q\n' "$XVFB_PID" >>"$STATE_FILE"
sleep 0.5
kill -0 "$XVFB_PID"
nohup x11vnc -display "$DISPLAY_NAME" -rfbport "$VNC_PORT" -passwd intendant \
  -forever -quiet >"$RUN_DIR/x11vnc.log" 2>&1 &
VNC_PID=$!
printf 'VNC_PID=%q\n' "$VNC_PID" >>"$STATE_FILE"
sleep 0.5
kill -0 "$VNC_PID"
```

### 4. Launch intendant --web

Prefer running `intendant --web` in a long-lived session/PTY. In this environment,
short-lived shell wrappers around `nohup ... &` let the web server disappear
between steps.

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
[[ -z "${INTENDANT_PID:-}" ]] || {
  echo "An Intendant PID is already recorded; clean up before retrying" >&2; exit 1;
}

(
  cd "$REPO"
  [ ! -f .env ] || source .env
  source "$EXPECTED_STATE"
  exec "$BIN" --direct --autonomy low --web "$PORT" --no-tls \
    --bind 127.0.0.1 "your task here"
) >"$RUN_DIR/intendant.log" 2>&1 &
INTENDANT_PID=$!
printf 'INTENDANT_PID=%q\n' "$INTENDANT_PID" >>"$STATE_FILE"
sleep 3
kill -0 "$INTENDANT_PID"
cat "$RUN_DIR/intendant.log"
```

### 5. Check the target live provider

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
curl -s "$BASE/config"
# Returns: {"provider":"gemini","model":"...","input_sample_rate":16000,"output_sample_rate":24000}
# or:      {"provider":"openai","model":"...","input_sample_rate":24000,"output_sample_rate":24000}
```

Use the `provider` field to decide the Gemini vs OpenAI path below.

### 6. Launch Firefox with debugger

Configure a run-scoped Firefox profile:
```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
mkdir -p "$PROFILE"
grep -q 'devtools.debugger.remote-enabled' "$PROFILE/user.js" 2>/dev/null || \
cat >> "$PROFILE/user.js" << 'EOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
user_pref("media.navigator.permission.disabled", true);
EOF
```

Launch the isolated profile on `/app`:
```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
[[ -z "${FIREFOX_PID:-}" ]] || {
  echo "A Firefox PID is already recorded; clean up before retrying" >&2; exit 1;
}
DISPLAY="$DISPLAY_NAME" nohup firefox --no-remote --profile "$PROFILE" \
  --start-debugger-server "$DEBUG_PORT" --new-window "$BASE/app" \
  >"$RUN_DIR/firefox.log" 2>&1 &
FIREFOX_PID=$!
printf 'FIREFOX_PID=%q\n' "$FIREFOX_PID" >>"$STATE_FILE"
sleep 5
kill -0 "$FIREFOX_PID"
```

### 7. Set API key in browser localStorage (Gemini)

For Gemini Live with tool calling, an API key must be in localStorage:
```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
cd "$REPO"
set -a && source "$REPO/.env" && set +a
JS=$(python3 - <<'PY'
import json, os
print("localStorage.setItem('gemini_api_key', %s); 'stored'" % json.dumps(os.environ['GEMINI_API_KEY']))
PY
)
python3 scripts/ff-eval.py "$JS"
python3 scripts/ff-eval.py "!!localStorage.getItem('gemini_api_key')"
python3 scripts/ff-eval.py "location.reload(); 'reloading'"
sleep 3
```

**Important**: if the first-run voice dialog is already open, storing the key in
`localStorage` does not dismiss it. Reload after storing, or click the mic button
again once the key exists.

For OpenAI Realtime, do **not** place the long-lived key in Firefox storage.
`OPENAI_API_KEY` must be available to the daemon when it starts (step 4);
the browser obtains a short-lived Realtime client secret from the daemon's
voice-session endpoint. If the key was absent at startup, stop only
the resources through **Cleanup**, then start a fresh run after adding the key
to `$REPO/.env`. Do not reconstruct or rediscover an Intendant PID.

## Sending Audio

### The persisted `say` helper

Step 2 writes an executable helper into this run's owned directory. It
synthesizes speech with espeak-ng, converts to the format PulseAudio expects,
and plays it into the persisted virtual mic sink. Reload state before each use:

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
[[ -x "$SAY_HELPER" ]] || { echo "Missing speech helper: $SAY_HELPER" >&2; exit 1; }
"$SAY_HELPER" "Hello, what is happening with the agent?"
```

### Usage

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
[[ -x "$SAY_HELPER" ]] || exit 1
"$SAY_HELPER" "Please submit a task to list files in /tmp" 120
"$SAY_HELPER" "approve"
"$SAY_HELPER" "check status"
"$SAY_HELPER" "yes"
```

### Sending silence (keeps the connection alive)

```bash
set -euo pipefail
CURRENT_REPO=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$CURRENT_REPO/target/voice-e2e/active/run.env"
[[ -r "$EXPECTED_STATE" ]] || { echo "Missing voice-E2E state: $EXPECTED_STATE" >&2; exit 1; }
source "$EXPECTED_STATE"
[[ "$REPO" == "$CURRENT_REPO" && "$STATE_FILE" == "$EXPECTED_STATE" ]] || exit 1
# 2 seconds of silence at 48kHz mono s16le = 192000 bytes of zeros
dd if=/dev/zero bs=192000 count=1 2>/dev/null | \
  paplay --device="$VIRTUAL_MIC" --format=s16le --rate=48000 --channels=1 --raw
```

## Connecting Live Mode from Browser

The live model must be connected from the browser before audio will be processed.
Click the mic button or use the debugger:

```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" ]] || exit 1
cd "$REPO"
# Click the mic button programmatically
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3
```

Or via xdotool (find mic button position via VNC):
```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" ]] || exit 1
DISPLAY="$DISPLAY_NAME" xdotool mousemove 640 680 click 1
sleep 1
```

Then verify connection via `/debug` (see Assertions below).

**Important**: `voice.connected=true` only proves the live model connection is up.
It does **not** prove Firefox is actively capturing microphone audio. Verify both.

## Asserting on State (primary method — no screenshots)

### Verify live connection
```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" ]] || exit 1
curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('voice', {})
connected = v.get('connected', False)
print(f'Live connected: {connected}')
assert connected, 'Live model not connected'
"
```

### Wait for live connection
```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" ]] || exit 1
for i in $(seq 1 15); do
  CONNECTED=$(curl -s "$BASE/debug" | python3 -c "
import sys, json; print(json.load(sys.stdin).get('voice', {}).get('connected', False))" 2>/dev/null)
  [ "$CONNECTED" = "True" ] && break
  sleep 1
done
echo "Live connected: $CONNECTED"
```

### Verify Firefox is actively capturing from the virtual mic
After activating the mic, verify Firefox has a recording stream:

```bash
pactl list short source-outputs
```

Expected: at least one `source-output` owned by Firefox. If empty, the browser is
connected to live mode but is not currently capturing audio.

If the mic button state looks wrong after earlier setup failures, force a fresh
`getUserMedia` start by clicking the visible mic button directly:

```bash
# Example coordinates from a 1280x720 Xvfb display; adjust via VNC if needed
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" ]] || exit 1
DISPLAY="$DISPLAY_NAME" xdotool mousemove --sync 1112 608 click 1
sleep 2
pactl list short source-outputs
```

### Check live activity after speaking
```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" && -x "$SAY_HELPER" ]] || exit 1
"$SAY_HELPER" "What is the current status?" 130
sleep 5

curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('voice', {})
print(f'Voice logs: {v.get(\"voice_log_count\", 0)}')
print(f'Last voice log: {v.get(\"last_voice_log\", \"(none)\")}')
assert v.get('voice_log_count', 0) > 0, 'No voice logs — model may not have received audio'
"
```

### Verify browser audio is being sent to the live model
If `/debug` stays quiet, inspect the session log for `voice:audio_send` diagnostics:

```bash
IHOME="${INTENDANT_HOME:-$HOME/.intendant}"
tail -n 200 "$IHOME"/logs/*/session.jsonl | grep 'voice:audio_send'
```

This confirms the path up to the browser send loop is active:
`virtual mic -> Firefox -> getUserMedia -> AudioWorklet/WASM -> live audio send`.

### Verify task was submitted via live
```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" && -x "$SAY_HELPER" ]] || exit 1
"$SAY_HELPER" "Please list the files in /tmp" 130
sleep 8

curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
state = d.get('agent_state', d)
phase = state.get('phase', 'idle')
print(f'Phase: {phase}')
assert phase != 'idle', f'Task not started — phase still idle'
"
```

### Verify approval pending and approve via live
```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" && -x "$SAY_HELPER" ]] || exit 1
# Wait for approval
for i in $(seq 1 30); do
  PENDING=$(curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
print('yes' if pa and str(pa) != 'null' else 'no')
" 2>/dev/null)
  [ "$PENDING" = "yes" ] && break
  sleep 1
done
echo "Approval pending: $PENDING"

# Approve via live
"$SAY_HELPER" "Yes, approve that" 130
sleep 5

# Verify approval cleared
curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
print(f'Pending approval after live approve: {pa}')
assert pa is None or str(pa) == 'null', f'Approval not cleared: {pa}'
"
```

## Example Test Scenarios

### Scenario 1: Live submits task, live approves

```bash
set -euo pipefail
REPO_NOW=$(git rev-parse --show-toplevel)
STATE="$REPO_NOW/target/voice-e2e/active/run.env"
[[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
source "$STATE"
[[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" && -x "$SAY_HELPER" ]] || exit 1
cd "$REPO"
# 1. Connect live
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3

# 2. Verify connected
curl -s "$BASE/debug" | python3 -c "
import sys, json; d = json.load(sys.stdin)
assert d.get('voice',{}).get('connected'), 'Live not connected'
print('Live connected OK')
"

# 3. Submit task via live
"$SAY_HELPER" "Please list the files in /tmp" 130
sleep 8

# 4. Assert task started
curl -s "$BASE/debug" | python3 -c "
import sys, json; d = json.load(sys.stdin)
phase = d.get('agent_state', d).get('phase', 'idle')
assert phase != 'idle', f'Task not started: {phase}'
print(f'Task started — phase: {phase}')
"

# 5. Wait for and verify approval
for i in $(seq 1 30); do
  PENDING=$(curl -s "$BASE/debug" | python3 -c "
import sys,json; pa=json.load(sys.stdin).get('agent_state',{}).get('pending_approval')
print('yes' if pa and str(pa)!='null' else 'no')" 2>/dev/null)
  [ "$PENDING" = "yes" ] && break; sleep 1
done

# 6. Approve via live and verify
"$SAY_HELPER" "Yes, approve that" 130
sleep 5
curl -s "$BASE/debug" | python3 -c "
import sys, json; d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
assert pa is None or str(pa) == 'null', f'Approval not cleared: {pa}'
print('Approved OK')
"
```

## Troubleshooting

### No audio reaching the live model

1. **Check virtual mic exists**: `pactl list short sources | grep "$VIRTUAL_MIC"`
2. **Check default source**: `pactl get-default-source` — should be
   `$VIRTUAL_MIC.monitor`
3. **Check Firefox is actually recording**: `pactl list short source-outputs`
   - If empty, live may be connected but `getUserMedia` is not active.
4. **Test audio flow**: Play a tone and check PulseAudio levels:
   ```bash
   set -euo pipefail
   REPO_NOW=$(git rev-parse --show-toplevel)
   STATE="$REPO_NOW/target/voice-e2e/active/run.env"
   [[ -r "$STATE" ]] || { echo "Missing voice-E2E state: $STATE" >&2; exit 1; }
   source "$STATE"
   [[ "$REPO" == "$REPO_NOW" && "$STATE_FILE" == "$STATE" ]] || exit 1
   ffmpeg -f lavfi -i "sine=frequency=440:duration=2" -f s16le -ar 48000 -ac 1 pipe:1 2>/dev/null | \
     paplay --device="$VIRTUAL_MIC" --format=s16le --rate=48000 --channels=1 --raw &
   pactl list short sources | grep RUNNING  # Should show $VIRTUAL_MIC.monitor as RUNNING
   ```
5. **Check browser mic permission**: this run's isolated profile sets
   `media.navigator.permission.disabled=true` in `user.js` before Firefox
   starts. Verify the line is present. If you remove that test-only bypass,
   grant the prompt interactively over VNC; do not edit `permissions.sqlite`
   while Firefox owns the profile.

### Live model not responding to speech

- **espeak-ng quality**: espeak-ng is robotic. Live models may struggle with it.
  Try speaking slower (`"$SAY_HELPER" "text" 100`, after reloading state) or
  using shorter, clearer phrases.
- **Ambient noise**: The virtual mic is clean (no noise), which is actually ideal.

## Provider-Specific Notes

### Gemini Live

- **API key mode** (`BidiGenerateContent`): Supports tool calling. Set
  `gemini_api_key` in localStorage.
- Input sample rate: **16kHz** (WASM downsamples from browser's 48kHz)
- `response_modalities: ["AUDIO"]` only (adding `"TEXT"` causes close code 1007)
- espeak-ng speech recognition quality is decent — Gemini handles robotic speech
  reasonably well.

### OpenAI Realtime

- Browser gets a short-lived client secret from the daemon (dashboard-control
  `api_voice_session`, with `POST /session` as the direct-dashboard fallback).
- The long-lived OpenAI key stays daemon-side; browser `localStorage` is not an
  OpenAI authentication path.
- Input sample rate: **24kHz** (WASM downsamples from 48kHz)
- `modalities: ["audio", "text"]`
- OpenAI Realtime tends to be more sensitive to audio quality — speak slower
  and clearer with espeak-ng.

## Cleanup

```bash
set -u
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_ACTIVE="$REPO_NOW/target/voice-e2e/active"
EXPECTED_STATE="$EXPECTED_ACTIVE/run.env"
[[ -r "$EXPECTED_STATE" ]] || {
  echo "Missing state; refusing PID discovery or broad cleanup: $EXPECTED_STATE" >&2
  exit 1
}
source "$EXPECTED_STATE"
[[ "$REPO" == "$REPO_NOW" &&
   "$ACTIVE_DIR" == "$EXPECTED_ACTIVE" &&
   "$STATE_FILE" == "$EXPECTED_STATE" &&
   "$RUN_DIR" == "$EXPECTED_ACTIVE/run" &&
   "$PROFILE" == "$RUN_DIR/firefox-profile" &&
   "$SAY_HELPER" == "$RUN_DIR/say" ]] || {
  echo "State paths failed validation; refusing cleanup" >&2
  exit 1
}

cleanup_failed=0
process_matches() {
  local pid=$1; shift
  local args needle
  args=$(ps -p "$pid" -o args= 2>/dev/null) || return 1
  for needle in "$@"; do
    [[ "$args" == *"$needle"* ]] || return 1
  done
}
stop_owned() {
  local label=$1 pid=$2; shift 2
  [[ -z "$pid" ]] && return 0
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || {
    echo "$label PID is not numeric; refusing: $pid" >&2; return 1;
  }
  [[ "$pid" -gt 1 ]] || {
    echo "$label PID is unsafe; refusing: $pid" >&2; return 1;
  }
  kill -0 "$pid" 2>/dev/null || return 0
  process_matches "$pid" "$@" || {
    echo "$label PID $pid no longer matches its recorded process; refusing" >&2
    return 1
  }
  kill "$pid" 2>/dev/null || true
  for _ in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
  process_matches "$pid" "$@" || {
    echo "$label PID $pid changed identity before SIGKILL; refusing" >&2
    return 1
  }
  kill -9 "$pid" 2>/dev/null || true
}

stop_owned firefox "${FIREFOX_PID:-}" firefox "--profile $PROFILE" \
  "--start-debugger-server $DEBUG_PORT" || cleanup_failed=1
stop_owned intendant "${INTENDANT_PID:-}" "$BIN" "--web $PORT" || cleanup_failed=1
stop_owned x11vnc "${VNC_PID:-}" x11vnc "-display $DISPLAY_NAME" \
  "-rfbport $VNC_PORT" || cleanup_failed=1
stop_owned Xvfb "${XVFB_PID:-}" Xvfb "$DISPLAY_NAME" || cleanup_failed=1

# Restore PulseAudio only when this run still owns the relevant state.
if [[ -n "${PULSE_MODULE_ID:-}" || -n "${VIRTUAL_MIC:-}" ]]; then
  if [[ ! "${PULSE_MODULE_ID:-}" =~ ^[0-9]+$ ||
        ! "${VIRTUAL_MIC:-}" =~ ^intendant_voice_[0-9]+$ ]]; then
    echo "Invalid PulseAudio ownership state; refusing module cleanup" >&2
    cleanup_failed=1
  else
    module_line=$(pactl list short modules |
      awk -v id="$PULSE_MODULE_ID" '$1 == id { print; exit }')
    if [[ -z "$module_line" ]]; then
      echo "Owned PulseAudio module is already absent"
    elif [[ "$module_line" != *"module-null-sink"* ||
          "$module_line" != *"sink_name=$VIRTUAL_MIC"* ]]; then
      echo "PulseAudio module $PULSE_MODULE_ID is not this run's sink; refusing" >&2
      cleanup_failed=1
    else
      current_source=$(pactl get-default-source 2>/dev/null || true)
      if [[ "$current_source" == "$VIRTUAL_MIC.monitor" ]]; then
        if pactl list short sources |
          awk -v source="${PREV_SOURCE:-}" '$2 == source { found=1 } END { exit !found }'; then
          pactl set-default-source "$PREV_SOURCE" || cleanup_failed=1
        else
          echo "Previous PulseAudio source no longer exists; cannot restore it" >&2
          cleanup_failed=1
        fi
      elif [[ "$current_source" != "${PREV_SOURCE:-}" ]]; then
        echo "Default source changed after setup; leaving it unchanged" >&2
      fi
      pactl unload-module "$PULSE_MODULE_ID" || cleanup_failed=1
    fi
  fi
fi

# Preserve the state file whenever scoped cleanup could not be completed, so a
# later attempt still has the exact ownership record.
if (( cleanup_failed )); then
  echo "Cleanup incomplete; preserving $EXPECTED_STATE" >&2
  exit 1
fi
[[ ! -L "$RUN_DIR" && -d "$RUN_DIR" ]] || {
  echo "Owned run directory failed validation; preserving state" >&2; exit 1;
}
rm -f -- "$SAY_HELPER"
rm -rf -- "$RUN_DIR"
rm -f -- "$STATE_FILE"
rmdir "$ACTIVE_DIR"
rmdir "$STATE_ROOT" 2>/dev/null || true
```

## Notes from verified runs

- On this environment, reload `run.env`, then verify VNC with
  `ss -ltnp | grep ":$VNC_PORT\\b"` after startup.
  `x11vnc` can appear to start and then exit.
- A stable VNC launch here was one that remained attached to the persisted
  `$DISPLAY_NAME` and was verified by checking the actual listener.
