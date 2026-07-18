---
name: web-e2e
description: >
  E2E test the --web app UI. Launches Xvfb, runs intendant --web as a
  background process, opens Firefox on /app, and asserts via /debug
  endpoint and WebSocket JSON. Human monitors via VNC on port 5950.
compatibility: Requires Xvfb, Firefox, x11vnc, xdotool, curl
allowed-tools: Bash Read
disable-model-invocation: true
---

# Test --web App UI E2E

## Key Concepts

- **No terminal emulator needed**: `--web` serves the dashboard SPA; sessions render in the browser.
  Intendant runs as a plain background process, not inside a terminal emulator.
- **Firefox renders `/app`**: the ui-v2 dashboard with Activity, Sessions,
  Agenda, Memory, Live display, Station, Terminal, Files, Usage, Access,
  Vault, Settings, and Debug destinations. State handling is split between
  `presence-web` WASM and the generated JavaScript application.
- **Live mode available**: `/app` has a mic button for connecting Gemini Live or
  OpenAI Realtime. Gemini uses a browser-held key; OpenAI uses a short-lived
  client secret minted by the daemon from its server-side credential.
- **Approval via browser**: Approval buttons in the Activity tab, or keyboard
  shortcuts (y/s/a/n). No `--control-socket` or socat needed.
- **Display streaming**: Live display and Station receive the agent's
  display through the WebRTC display pipeline. The observer VNC server in
  this runbook is only for watching the Firefox test desktop on `:50`.

## Launch

The example uses display **:50**, VNC port **5950**, and dashboard port
**18765**. Check all three first; if any is occupied, pick unused values and
update the variables. Never stop another agent's processes or use the shared
default dashboard port 8765.

```bash
# 1. Pin this worktree and reserve isolated endpoints.
REPO=$(git rev-parse --show-toplevel)
(
  cd "$REPO"
  cargo build --release --bin intendant --bin intendant-runtime
)
BIN="$REPO/target/release/intendant"
PORT=18765
BASE="http://127.0.0.1:$PORT"
RUN_DIR=$(mktemp -d /tmp/intendant-web-e2e.XXXXXX)
if pgrep -fa '^Xvfb :50([[:space:]]|$)|^x11vnc .*:50'; then
  echo "Display :50 is already owned; choose an unused display"; exit 1
fi
ss -ltn 2>/dev/null | grep -E ':(5950|6000|18765)\b' && {
  echo "Choose unused display/VNC/debug/dashboard values before continuing"; exit 1;
}

# 2. Start Xvfb + x11vnc (human observation only); retain owned PIDs.
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
XVFB_PID=$!
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -passwd intendant -forever -quiet > /dev/null 2>&1 &
VNC_PID=$!
sleep 0.5

# 3. Launch this worktree's controller on loopback plaintext for the test.
(
  cd "$REPO"
  [ ! -f .env ] || source .env
  exec "$BIN" --direct --autonomy low --web "$PORT" --no-tls \
    --bind 127.0.0.1 "your task here"
) \
  >"$RUN_DIR/intendant.log" 2>&1 &
INTENDANT_PID=$!

# 4. Wait for web gateway to start
sleep 3
cat "$RUN_DIR/intendant.log"

# 5. Launch an isolated Firefox profile on display :50.
mkdir -p "$RUN_DIR/firefox-profile"
cat > "$RUN_DIR/firefox-profile/user.js" << 'EOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
EOF
DISPLAY=:50 nohup firefox --no-remote --profile "$RUN_DIR/firefox-profile" \
  --start-debugger-server 6000 --new-window "$BASE/app" > /dev/null 2>&1 &
FIREFOX_PID=$!
sleep 8
```

## Asserting on State (primary method — no screenshots)

### /debug endpoint

The `/debug` endpoint returns the full agent state as JSON. Use it for all assertions.

```bash
# Full state dump
curl -s "$BASE/debug" | python3 -m json.tool

# Assert on specific fields
curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
state = d.get('agent_state', d)
print(f'Phase: {state.get(\"phase\")}')
print(f'Turn: {state.get(\"turn\")}')
print(f'Pending approval: {state.get(\"pending_approval\")}')
voice = d.get('voice', {})
print(f'Voice connected: {voice.get(\"connected\", False)}')
print(f'Voice logs: {voice.get(\"voice_log_count\", 0)}')
"
```

### Wait for a specific state
```bash
# Poll until approval is pending (up to 30s)
for i in $(seq 1 30); do
  PENDING=$(curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
pa = d.get('agent_state', d).get('pending_approval')
print('yes' if pa and pa != 'null' else 'no')
" 2>/dev/null)
  [ "$PENDING" = "yes" ] && break
  sleep 1
done
echo "Approval pending: $PENDING"
```

### Wait for task completion
```bash
for i in $(seq 1 60); do
  PHASE=$(curl -s "$BASE/debug" | python3 -c "
import sys, json; print(json.load(sys.stdin).get('agent_state', {}).get('phase', ''))" 2>/dev/null)
  [ "$PHASE" = "Done" ] || [ "$PHASE" = "Idle" ] && break
  sleep 1
done
echo "Final phase: $PHASE"
```

### Approve via WebSocket (programmatic, no browser interaction)
```bash
python3 -c "
import asyncio, json, websockets
async def approve():
    async with websockets.connect('ws://127.0.0.1:$PORT') as ws:
        await asyncio.wait_for(ws.recv(), timeout=3)  # bootstrap
        await ws.send(json.dumps({'action': 'approve', 'id': 1}))
        print('Approved')
asyncio.run(approve())
"
```

### Check voice/live connection
```bash
curl -s "$BASE/debug" | python3 -c "
import sys, json
d = json.load(sys.stdin)
v = d.get('voice', {})
assert v.get('connected') == True, f'Voice not connected: {v}'
print('Voice connected OK')
print(f'Last voice log: {v.get(\"last_voice_log\", \"(none)\")}')
"
```

### Other endpoints
```bash
# Config: provider, model, sample rates (no secrets)
curl -s "$BASE/config"

# Session: mint ephemeral token (called by browser on mic click)
curl -s -X POST "$BASE/session"
```

## Simulating Live Input

Since there's no real microphone on a headless display, simulate live input
by sending text directly via the Firefox debugger.

**Via Firefox debugger** (if `--start-debugger-server 6000` is active):
```bash
# The WASM instance in /app is exposed as window.app (PresenceWeb)
python3 scripts/ff-eval.py "app.send_text('Hello, what is happening?')"
```

**Setting API key in localStorage** (required for tool calling):
```bash
source "$REPO/.env" && python3 scripts/ff-eval.py "localStorage.setItem('gemini_api_key', '$GEMINI_API_KEY'); 'stored'"
# Then reload the page:
python3 scripts/ff-eval.py "location.reload(); 'reloading'"
```

**Clicking the mic button**:
```bash
python3 scripts/ff-eval.py "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3
```

## Approval via Browser UI

The `/app` Activity tab has approval buttons (Approve/Skip/Approve All/Deny).
You can also use keyboard shortcuts when the page has focus:

```bash
# Press 'y' to approve (the page must have focus)
DISPLAY=:50 xdotool key y
```

**Gotcha**: If the follow-up text input panel is active, keyboard shortcuts
go into the text input. Press Escape first to dismiss it.

## Live display

When the agent runs commands that trigger a virtual display (commonly
`display_99`), the Live display destination shows the WebRTC stream. Features:

- **View-only** by default — watch what the agent does
- **Take Control** button — switch to interactive mode (mouse/keyboard forwarded)
- **Release** button — return to view-only, optional note for the agent
- Auto-connects when `display_ready` arrives and the display grant permits it

## Screenshot (optional — for human VNC verification only)

```bash
DISPLAY=:50 import -window root /tmp/web-e2e-screenshot.png
```

This is **not needed for assertions** — use `/debug` instead.

## Known Gotchas

- **WASM cache**: Content-hash versioning (`?v=<hash>`) on WASM/JS URLs means
  browsers automatically fetch new assets after rebuilds. No manual cache
  clearing needed.
- **WASM rebuild**: use the pinned canonical builder from the repo root:
  `bash scripts/build-wasm.sh`, then rebuild only the controller if needed
  (`cargo build --release --bin intendant --bin intendant-runtime`).
  Regenerate committed WASM
  artifacts on macOS only.
- **AudioContext warning** on headless displays is expected and harmless.
- **Follow-up panel** captures keystrokes — Escape first before sending shortcuts.
- **Firefox profile lock**: use the run-scoped `--no-remote --profile
  "$RUN_DIR/firefox-profile"` invocation above; do not remove another
  browser's profile lock.
- **Late-connect**: If you reload the browser mid-session, Activity replays
  the session log, Usage gets cached data, and Live display reconnects through
  WebRTC.
- **Two Gemini endpoints, different capabilities**:
  | | `BidiGenerateContent` | `BidiGenerateContentConstrained` |
  |---|---|---|
  | Auth | API key (`?key=`) | Ephemeral token (`?access_token=`) |
  | Frames | Text | Binary (ArrayBuffer) |
  | Tool calling | Yes | No |
  | Setup message | Full (model + config + tools) | Minimal (tools + system_instruction only) |

**For browser-side JS debugging** (only needed for WASM/JS errors):

The launch recipe already configures the run-scoped profile and starts the
debugger on port 6000. Use `scripts/ff-eval.py`; do not modify a user's
normal Firefox profile.

## Cleanup

```bash
# Stop only processes whose PIDs this run captured.
kill "$INTENDANT_PID" "$VNC_PID" "$XVFB_PID" 2>/dev/null || true
kill -9 "$FIREFOX_PID" 2>/dev/null || true
rm -rf "$RUN_DIR"
```
