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
REPO=$(git rev-parse --show-toplevel)
(
  cd "$REPO"
  cargo build --release --bin intendant --bin intendant-runtime
)
BIN="$REPO/target/release/intendant"
PORT=18767
BASE="http://127.0.0.1:$PORT"
RUN_DIR=$(mktemp -d /tmp/intendant-voice-e2e.XXXXXX)
if pgrep -fa '^Xvfb :50([[:space:]]|$)|^x11vnc .*:50'; then
  echo "Display :50 is already owned; choose an unused display"; exit 1
fi
ss -ltn 2>/dev/null | grep -E ':(5950|6000|18767)\b' && {
  echo "Choose unused display/VNC/debug/dashboard values before continuing"; exit 1;
}
```

### 2. Create PulseAudio virtual microphone

```bash
# Create a run-unique null sink — its .monitor becomes the virtual mic source.
VIRTUAL_MIC="intendant_voice_$$"
PREV_SOURCE=$(pactl get-default-source)
PULSE_MODULE_ID=$(pactl load-module module-null-sink sink_name="$VIRTUAL_MIC" \
  sink_properties=device.description="VirtualMic" \
  rate=48000 channels=1 format=s16le)

# Set the virtual mic monitor as the default recording source
pactl set-default-source "$VIRTUAL_MIC.monitor"

# Verify
pactl list short sources | grep "$VIRTUAL_MIC"
# Should show: $VIRTUAL_MIC.monitor
```

**Why 48kHz?** The browser's `AudioContext` uses the system's native sample rate
(typically 48kHz). The WASM layer handles downsampling to the live model's
target rate (16kHz for Gemini, 24kHz for OpenAI). We match the browser rate
at the PulseAudio level so no extra resampling happens in PulseAudio.

### 3. Start Xvfb + x11vnc

```bash
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
XVFB_PID=$!
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &
VNC_PID=$!
sleep 0.5
```

### 4. Launch intendant --web

Prefer running `intendant --web` in a long-lived session/PTY. In this environment,
short-lived shell wrappers around `nohup ... &` let the web server disappear
between steps.

```bash
(
  cd "$REPO"
  [ ! -f .env ] || source .env
  exec "$BIN" --direct --autonomy low --web "$PORT" --no-tls \
    --bind 127.0.0.1 "your task here"
) >"$RUN_DIR/intendant.log" 2>&1 &
INTENDANT_PID=$!
sleep 3
cat "$RUN_DIR/intendant.log"
```

### 5. Check the target live provider

```bash
curl -s "$BASE/config"
# Returns: {"provider":"gemini","model":"...","input_sample_rate":16000,"output_sample_rate":24000}
# or:      {"provider":"openai","model":"...","input_sample_rate":24000,"output_sample_rate":24000}
```

Use the `provider` field to decide the Gemini vs OpenAI path below.

### 6. Launch Firefox with debugger

Configure a run-scoped Firefox profile:
```bash
PROFILE="$RUN_DIR/firefox-profile"
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
DISPLAY=:50 nohup firefox --no-remote --profile "$PROFILE" \
  --start-debugger-server 6000 --new-window "$BASE/app" > /dev/null 2>&1 &
FIREFOX_PID=$!
sleep 5
```

### 7. Set API key in browser localStorage (Gemini)

For Gemini Live with tool calling, an API key must be in localStorage:
```bash
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
`$INTENDANT_PID`, relaunch the same command after sourcing `$REPO/.env`, and
reload the page.

## Sending Audio

### The `say` helper

This is the core function. It synthesizes speech with espeak-ng, converts to the
format PulseAudio expects, and plays it into the virtual mic sink:

```bash
say() {
  local text="$1"
  local speed="${2:-140}"  # words per minute (default 140, slower = clearer for ASR)
  espeak-ng "$text" -s "$speed" --stdout | \
    ffmpeg -loglevel error -i pipe:0 \
      -f s16le -ar 48000 -ac 1 pipe:1 | \
    paplay --device="$VIRTUAL_MIC" --format=s16le --rate=48000 --channels=1 --raw
}
```

### Usage

```bash
# Simple utterance
say "Hello, what is happening with the agent?"

# Slower speech for better recognition
say "Please submit a task to list files in /tmp" 120

# Short commands (live models handle these well)
say "approve"
say "check status"
say "yes"
```

### Sending silence (keeps the connection alive)

```bash
# 2 seconds of silence at 48kHz mono s16le = 192000 bytes of zeros
dd if=/dev/zero bs=192000 count=1 2>/dev/null | \
  paplay --device="$VIRTUAL_MIC" --format=s16le --rate=48000 --channels=1 --raw
```

## Connecting Live Mode from Browser

The live model must be connected from the browser before audio will be processed.
Click the mic button or use the debugger:

```bash
# Click the mic button programmatically
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3
```

Or via xdotool (find mic button position via VNC):
```bash
DISPLAY=:50 xdotool mousemove 640 680 click 1
sleep 1
```

Then verify connection via `/debug` (see Assertions below).

**Important**: `voice.connected=true` only proves the live model connection is up.
It does **not** prove Firefox is actively capturing microphone audio. Verify both.

## Asserting on State (primary method — no screenshots)

### Verify live connection
```bash
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
DISPLAY=:50 xdotool mousemove --sync 1112 608 click 1
sleep 2
pactl list short source-outputs
```

### Check live activity after speaking
```bash
say "What is the current status?" 130
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
say "Please list the files in /tmp" 130
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
say "Yes, approve that" 130
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
say "Please list the files in /tmp" 130
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
say "Yes, approve that" 130
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
  Try speaking slower (`say "text" 100`) or using shorter, clearer phrases.
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
# Restore the previous source and unload only this run's PulseAudio module.
pactl set-default-source "$PREV_SOURCE" 2>/dev/null || true
pactl unload-module "$PULSE_MODULE_ID" 2>/dev/null || true

# Stop only processes whose PIDs this run captured.
kill "$INTENDANT_PID" "$VNC_PID" "$XVFB_PID" 2>/dev/null || true
kill -9 "$FIREFOX_PID" 2>/dev/null || true
rm -rf "$RUN_DIR"
```

## Notes from verified runs

- On this environment, verify VNC with `ss -ltnp | grep 5950` after startup.
  `x11vnc` can appear to start and then exit.
- A stable VNC launch here was one that remained attached to the live `Xvfb :50`
  display and was verified by checking the actual listener.
