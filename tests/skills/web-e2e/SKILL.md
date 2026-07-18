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

The example uses display **:50**, VNC port **5950**, Firefox debugger port
**6000**, and dashboard port **18765**. Check all four first; if any is
occupied, choose another isolated set (the checked-in `scripts/ff-eval.py`
currently requires debugger port 6000). Never stop another agent's processes
or use the shared default dashboard port 8765.

Shell state is not assumed to survive between agent/runtime invocations. The
launch block creates a deterministic, owner-only state file at
`target/web-e2e.state`; every dependent block below reloads and validates it.
An existing state file is a hard stop, even if its processes appear dead: run
the cleanup block or inspect and remove the stale run deliberately before
starting another one.

```bash
# 1. Reserve an owner-only state file before doing any work.
set -eu
REPO=$(git rev-parse --show-toplevel)
STATE_FILE="$REPO/target/web-e2e.state"
mkdir -p "$REPO/target"
if [ -e "$STATE_FILE" ] || [ -L "$STATE_FILE" ]; then
  echo "Refusing to overwrite existing state: $STATE_FILE" >&2
  echo "Run the cleanup block or inspect the stale run manually." >&2
  exit 1
fi
umask 077
( set -o noclobber; : > "$STATE_FILE" ) || {
  echo "Could not reserve $STATE_FILE" >&2
  exit 1
}
chmod 600 "$STATE_FILE"

BIN="$REPO/target/release/intendant"
DISPLAY_NUM=50
DISPLAY_ID=":$DISPLAY_NUM"
VNC_PORT=5950
DEBUG_PORT=6000
PORT=18765
BASE="http://127.0.0.1:$PORT"
RUN_DIR=$(mktemp -d "$REPO/target/web-e2e-run.XXXXXX")
OWNER_MARKER="$RUN_DIR/.web-e2e-owner"
printf '%s\n' "$STATE_FILE" > "$OWNER_MARKER"

# Store shell-escaped values. Empty PID slots make partial-launch cleanup safe.
{
  printf 'WEB_E2E_STATE_VERSION=%q\n' 1
  printf 'REPO=%q\n' "$REPO"
  printf 'STATE_FILE=%q\n' "$STATE_FILE"
  printf 'BIN=%q\n' "$BIN"
  printf 'DISPLAY_NUM=%q\n' "$DISPLAY_NUM"
  printf 'DISPLAY_ID=%q\n' "$DISPLAY_ID"
  printf 'VNC_PORT=%q\n' "$VNC_PORT"
  printf 'DEBUG_PORT=%q\n' "$DEBUG_PORT"
  printf 'PORT=%q\n' "$PORT"
  printf 'BASE=%q\n' "$BASE"
  printf 'RUN_DIR=%q\n' "$RUN_DIR"
  printf 'OWNER_MARKER=%q\n' "$OWNER_MARKER"
  for name in XVFB VNC INTENDANT FIREFOX; do
    printf '%s_PID=%q\n' "$name" ""
    printf '%s_START=%q\n' "$name" ""
  done
} >> "$STATE_FILE"

# Append each captured PID before doing anything else with it. /proc start ticks
# let cleanup distinguish the owned process from later PID reuse.
record_pid() {
  name=$1
  pid=$2
  case "$name" in XVFB|VNC|INTENDANT|FIREFOX) ;; *)
    echo "Invalid process label: $name" >&2; exit 1;;
  esac
  case "$pid" in ''|*[!0-9]*)
    echo "Invalid captured PID for $name: $pid" >&2; exit 1;;
  esac
  [ "$pid" -gt 1 ] || { echo "Unsafe captured PID: $pid" >&2; exit 1; }
  printf '%s_PID=%q\n' "$name" "$pid" >> "$STATE_FILE"
  start=$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)
  case "$start" in
    ''|*[!0-9]*) echo "Could not record /proc start time for $name PID $pid" >&2; exit 1 ;;
  esac
  printf '%s_START=%q\n' "$name" "$start" >> "$STATE_FILE"
}

# 2. Build this worktree and reserve its isolated endpoints.
(
  cd "$REPO"
  cargo build --release --bin intendant --bin intendant-runtime
)
if pgrep -fa "^Xvfb ${DISPLAY_ID}([[:space:]]|$)|^x11vnc .* -display ${DISPLAY_ID}([[:space:]]|$)"; then
  echo "Display $DISPLAY_ID is already owned; choose an unused display" >&2
  exit 1
fi
for candidate_port in "$VNC_PORT" "$DEBUG_PORT" "$PORT"; do
  if ss -ltnH 2>/dev/null | awk -v suffix=":$candidate_port" \
      '$4 ~ (suffix "$") { found=1 } END { exit !found }'; then
    echo "TCP port $candidate_port is occupied; choose an unused endpoint" >&2
    exit 1
  fi
done

# 3. Start Xvfb + x11vnc (human observation only); persist owned PIDs.
nohup Xvfb "$DISPLAY_ID" -screen 0 1280x720x24 > /dev/null 2>&1 &
record_pid XVFB "$!"
sleep 0.5
nohup x11vnc -display "$DISPLAY_ID" -rfbport "$VNC_PORT" \
  -passwd intendant -forever -quiet > /dev/null 2>&1 &
record_pid VNC "$!"
sleep 0.5

# 4. Launch this worktree's controller on loopback plaintext for the test.
(
  cd "$REPO"
  [ ! -f .env ] || source .env
  exec "$BIN" --direct --autonomy low --web "$PORT" --no-tls \
    --bind 127.0.0.1 "your task here"
) \
  >"$RUN_DIR/intendant.log" 2>&1 &
record_pid INTENDANT "$!"

# 5. Wait for web gateway to start.
sleep 3
cat "$RUN_DIR/intendant.log"

# 6. Launch an isolated Firefox profile on the persisted display.
mkdir -p "$RUN_DIR/firefox-profile"
cat > "$RUN_DIR/firefox-profile/user.js" << 'EOF'
user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
EOF
DISPLAY="$DISPLAY_ID" nohup firefox --no-remote \
  --profile "$RUN_DIR/firefox-profile" \
  --start-debugger-server "$DEBUG_PORT" --new-window "$BASE/app" \
  > /dev/null 2>&1 &
record_pid FIREFOX "$!"
sleep 8
```

## Asserting on State (primary method — no screenshots)

### /debug endpoint

The `/debug` endpoint returns the full agent state as JSON. Use it for all assertions.

```bash
# Reload and validate launch state; do not rely on a previous shell.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${PORT:-}" in ''|*[!0-9]*) echo "Invalid persisted dashboard port" >&2; exit 1;; esac
[ "${BASE:-}" = "http://127.0.0.1:$PORT" ] || {
  echo "Invalid persisted dashboard URL" >&2; exit 1;
}

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
# Reload and validate launch state.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${PORT:-}" in ''|*[!0-9]*) echo "Invalid persisted dashboard port" >&2; exit 1;; esac
[ "${BASE:-}" = "http://127.0.0.1:$PORT" ] || {
  echo "Invalid persisted dashboard URL" >&2; exit 1;
}

# Poll until approval is pending (up to 30s)
PENDING=no
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
# Reload and validate launch state.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${PORT:-}" in ''|*[!0-9]*) echo "Invalid persisted dashboard port" >&2; exit 1;; esac
[ "${BASE:-}" = "http://127.0.0.1:$PORT" ] || {
  echo "Invalid persisted dashboard URL" >&2; exit 1;
}

PHASE=
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
# Reload and validate launch state.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${PORT:-}" in ''|*[!0-9]*) echo "Invalid persisted dashboard port" >&2; exit 1;; esac

PORT="$PORT" python3 -c "
import asyncio, json, websockets
import os
async def approve():
    port = int(os.environ['PORT'])
    async with websockets.connect(f'ws://127.0.0.1:{port}') as ws:
        await asyncio.wait_for(ws.recv(), timeout=3)  # bootstrap
        await ws.send(json.dumps({'action': 'approve', 'id': 1}))
        print('Approved')
asyncio.run(approve())
"
```

### Check voice/live connection
```bash
# Reload and validate launch state.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${PORT:-}" in ''|*[!0-9]*) echo "Invalid persisted dashboard port" >&2; exit 1;; esac
[ "${BASE:-}" = "http://127.0.0.1:$PORT" ] || {
  echo "Invalid persisted dashboard URL" >&2; exit 1;
}

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
# Reload and validate launch state.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${PORT:-}" in ''|*[!0-9]*) echo "Invalid persisted dashboard port" >&2; exit 1;; esac
[ "${BASE:-}" = "http://127.0.0.1:$PORT" ] || {
  echo "Invalid persisted dashboard URL" >&2; exit 1;
}

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
# Reload state; ff-eval.py is currently pinned to debugger port 6000.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${DEBUG_PORT:-}" in ''|*[!0-9]*) echo "Invalid debugger port" >&2; exit 1;; esac
[ "$DEBUG_PORT" -eq 6000 ] || {
  echo "scripts/ff-eval.py requires debugger port 6000" >&2; exit 1;
}

# The WASM instance in /app is exposed as window.app (PresenceWeb)
python3 "$REPO/scripts/ff-eval.py" "app.send_text('Hello, what is happening?')"
```

**Setting API key in localStorage** (required for tool calling):
```bash
# Reload state independently of the previous debugger command.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${DEBUG_PORT:-}" in ''|*[!0-9]*) echo "Invalid debugger port" >&2; exit 1;; esac
[ "$DEBUG_PORT" -eq 6000 ] || {
  echo "scripts/ff-eval.py requires debugger port 6000" >&2; exit 1;
}
[ -f "$REPO/.env" ] || { echo "Missing $REPO/.env" >&2; exit 1; }
source "$REPO/.env"
: "${GEMINI_API_KEY:?GEMINI_API_KEY is not set}"
python3 "$REPO/scripts/ff-eval.py" \
  "localStorage.setItem('gemini_api_key', '$GEMINI_API_KEY'); 'stored'"
# Then reload the page:
python3 "$REPO/scripts/ff-eval.py" "location.reload(); 'reloading'"
```

**Clicking the mic button**:
```bash
# Reload state independently of the previous debugger commands.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${DEBUG_PORT:-}" in ''|*[!0-9]*) echo "Invalid debugger port" >&2; exit 1;; esac
[ "$DEBUG_PORT" -eq 6000 ] || {
  echo "scripts/ff-eval.py requires debugger port 6000" >&2; exit 1;
}

python3 "$REPO/scripts/ff-eval.py" \
  "document.querySelector('.mic-btn')?.click(); 'clicked'"
sleep 3
```

## Approval via Browser UI

The `/app` Activity tab has approval buttons (Approve/Skip/Approve All/Deny).
You can also use keyboard shortcuts when the page has focus:

```bash
# Reload and validate the persisted display.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${DISPLAY_NUM:-}" in ''|*[!0-9]*) echo "Invalid display number" >&2; exit 1;; esac
[ "${DISPLAY_ID:-}" = ":$DISPLAY_NUM" ] || {
  echo "Invalid persisted display" >&2; exit 1;
}

# Press 'y' to approve (the page must have focus)
DISPLAY="$DISPLAY_ID" xdotool key y
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
# Reload the run directory and display; keep the image inside this owned run.
set -eu
REPO_NOW=$(git rev-parse --show-toplevel)
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2; exit 1;
}
. "$EXPECTED_STATE"
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2; exit 1;
  }
case "${DISPLAY_NUM:-}" in ''|*[!0-9]*) echo "Invalid display number" >&2; exit 1;; esac
[ "${DISPLAY_ID:-}" = ":$DISPLAY_NUM" ] || {
  echo "Invalid persisted display" >&2; exit 1;
}
case "${RUN_DIR:-}" in "$REPO"/target/web-e2e-run.*) ;; *)
  echo "Unsafe persisted run directory: ${RUN_DIR:-<unset>}" >&2; exit 1;;
esac

DISPLAY="$DISPLAY_ID" import -window root "$RUN_DIR/web-e2e-screenshot.png"
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
# Reload state from this worktree; never accept caller-provided PID variables.
set -u
REPO_NOW=$(git rev-parse --show-toplevel) || exit 1
EXPECTED_STATE="$REPO_NOW/target/web-e2e.state"
[ -f "$EXPECTED_STATE" ] && [ ! -L "$EXPECTED_STATE" ] && [ -O "$EXPECTED_STATE" ] || {
  echo "Missing or untrusted web E2E state: $EXPECTED_STATE" >&2
  exit 1
}
. "$EXPECTED_STATE" || exit 1
[ "${WEB_E2E_STATE_VERSION:-}" = 1 ] &&
  [ "${REPO:-}" = "$REPO_NOW" ] &&
  [ "${STATE_FILE:-}" = "$EXPECTED_STATE" ] || {
    echo "Web E2E state does not belong to this worktree" >&2
    exit 1
  }
case "${DISPLAY_NUM:-}" in ''|*[!0-9]*) echo "Invalid display number" >&2; exit 1;; esac
[ "${DISPLAY_ID:-}" = ":$DISPLAY_NUM" ] || {
  echo "Invalid persisted display" >&2; exit 1;
}
for persisted_port in "${VNC_PORT:-}" "${DEBUG_PORT:-}" "${PORT:-}"; do
  case "$persisted_port" in
    ''|*[!0-9]*) echo "Invalid persisted port" >&2; exit 1 ;;
  esac
done
[ "${BIN:-}" = "$REPO/target/release/intendant" ] || {
  echo "Unexpected controller path in state" >&2; exit 1;
}
case "${RUN_DIR:-}" in "$REPO"/target/web-e2e-run.*) ;; *)
  echo "Unsafe persisted run directory: ${RUN_DIR:-<unset>}" >&2; exit 1;;
esac
[ "${OWNER_MARKER:-}" = "$RUN_DIR/.web-e2e-owner" ] || {
  echo "Unexpected run-directory marker path" >&2; exit 1;
}

process_identity_matches() {
  label=$1
  pid=$2
  args=$(ps -ww -p "$pid" -o args= 2>/dev/null) || return 1
  case "$label" in
    XVFB)
      case "$args" in *"Xvfb $DISPLAY_ID "*) return 0;; esac
      ;;
    VNC)
      case "$args" in
        *x11vnc*"-display $DISPLAY_ID"*"-rfbport $VNC_PORT"*) return 0 ;;
      esac
      ;;
    INTENDANT)
      case "$args" in *"$BIN"*"--web $PORT"*) return 0;; esac
      ;;
    FIREFOX)
      case "$args" in *firefox*"--profile $RUN_DIR/firefox-profile"*) return 0;; esac
      ;;
  esac
  return 1
}

CLEANUP_OK=1
stop_owned_pid() {
  label=$1
  pid=$2
  saved_start=$3

  # Empty means launch never captured this process.
  if [ -z "$pid" ]; then
    if [ -n "$saved_start" ]; then
      echo "Inconsistent $label state; refusing cleanup" >&2
      CLEANUP_OK=0
    fi
    return
  fi
  case "$pid" in *[!0-9]*) echo "Non-numeric $label PID: $pid" >&2; CLEANUP_OK=0; return;; esac
  if [ "$pid" -le 1 ]; then
    echo "Unsafe $label PID: $pid" >&2
    CLEANUP_OK=0
    return
  fi

  # A vanished PID is already clean. A live PID must match both its immutable
  # Linux start tick and the command line expected for this exact run.
  kill -0 "$pid" 2>/dev/null || return
  case "$saved_start" in
    ''|*[!0-9]*) echo "Missing/invalid $label start time" >&2; CLEANUP_OK=0; return;;
  esac
  actual_start=$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)
  if [ "$actual_start" != "$saved_start" ] ||
      ! process_identity_matches "$label" "$pid"; then
    echo "PID $pid no longer matches owned $label; refusing to kill it" >&2
    CLEANUP_OK=0
    return
  fi

  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || return
    sleep 0.1
  done

  # Revalidate before escalation; never signal a PID that was reused.
  actual_start=$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)
  if [ "$actual_start" = "$saved_start" ] &&
      process_identity_matches "$label" "$pid"; then
    kill -KILL "$pid" 2>/dev/null || true
  else
    echo "PID $pid changed before escalation; refusing SIGKILL" >&2
    CLEANUP_OK=0
  fi
}

stop_owned_pid FIREFOX "${FIREFOX_PID:-}" "${FIREFOX_START:-}"
stop_owned_pid INTENDANT "${INTENDANT_PID:-}" "${INTENDANT_START:-}"
stop_owned_pid VNC "${VNC_PID:-}" "${VNC_START:-}"
stop_owned_pid XVFB "${XVFB_PID:-}" "${XVFB_START:-}"

[ "$CLEANUP_OK" -eq 1 ] || {
  echo "Cleanup was incomplete; preserving $STATE_FILE and the run directory" >&2
  exit 1
}

# Remove only the run directory named by this worktree's state and marker.
if [ -e "$RUN_DIR" ] || [ -L "$RUN_DIR" ]; then
  [ -d "$RUN_DIR" ] && [ ! -L "$RUN_DIR" ] &&
    [ "${OWNER_MARKER:-}" = "$RUN_DIR/.web-e2e-owner" ] &&
    [ -f "$OWNER_MARKER" ] && [ ! -L "$OWNER_MARKER" ] &&
    [ "$(cat "$OWNER_MARKER")" = "$STATE_FILE" ] || {
      echo "Run-directory ownership check failed; refusing removal" >&2
      exit 1
    }
  rm -rf -- "$RUN_DIR"
fi
rm -f -- "$STATE_FILE"
```
