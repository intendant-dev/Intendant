---
name: web-e2e
description: >
  E2E test the --web live mode. Launches Xvfb, runs intendant --web as a
  background process (no xterm needed), opens Firefox on the web TUI,
  and takes screenshots. User monitors via VNC.
compatibility: Requires Xvfb, Firefox, ImageMagick (import), x11vnc, xdotool
allowed-tools: Bash Read
disable-model-invocation: true
---

# Test --web Live Mode E2E

## Prerequisites

```bash
sudo apt-get install -y x11vnc firefox-esr xdotool
```

## Key Differences from TUI E2E

- **No xterm needed**: `--web` uses `WebTui` (buffer-backed ratatui backend).
  Intendant runs as a plain background process, not inside a terminal emulator.
- **Firefox is the UI**: Open Firefox on the Xvfb display pointing to
  `http://localhost:8765`. The browser renders the TUI via xterm.js and
  provides voice model controls.
- **Voice model connects from browser**: Gemini Live / OpenAI Realtime API
  keys are stored in browser localStorage. The live model is a direct
  browser-to-API WebSocket connection (low latency).
- **Control is via browser**: No `--control-socket` or socat needed. The
  browser IS the control interface (approval buttons, voice commands, etc.)

## Launch

**IMPORTANT:** Always use display **:50** (intendant reserves :99+ for its own Xvfb).
Always start `x11vnc` so the human can follow along via VNC on port 5950.

```bash
# 1. Kill stale processes from prior runs
pkill -f 'Xvfb :50' 2>/dev/null; pkill -f 'x11vnc.*:50' 2>/dev/null
pkill -f 'intendant.*web' 2>/dev/null; pkill -f firefox 2>/dev/null
sleep 0.5

# 2. Start Xvfb + x11vnc (MANDATORY — human needs VNC to observe)
nohup Xvfb :50 -screen 0 1280x720x24 > /dev/null 2>&1 &
sleep 0.5
nohup x11vnc -display :50 -rfbport 5950 -nopw -forever -quiet > /dev/null 2>&1 &
sleep 0.5

# 3. Launch intendant --web as background process (no xterm needed)
> /tmp/intendant-web-stderr.log
nohup bash -c 'cd /home/user/projects/intendant-codex-fork && source .env && \
  ./target/release/intendant --direct --autonomy low --web \
  "your task here" 2>/tmp/intendant-web-stderr.log' > /dev/null 2>&1 &

# 4. Wait for web gateway to start
sleep 3
cat /tmp/intendant-web-stderr.log  # Should show "Web TUI: http://0.0.0.0:8765"

# 5. Launch Firefox on display :50 pointing to the web TUI
DISPLAY=:50 nohup firefox --new-window http://localhost:8765 > /dev/null 2>&1 &
```

## Inject API Keys (Voice Model)

Voice model API keys are stored in browser localStorage. If an API key was
previously saved in Firefox on this display, the live model **auto-connects**
on page load — no manual setup needed.

For first-time setup, three options:

**Option A — Click Settings gear in browser (simplest):**
The web TUI has a settings gear icon (top-right). Click it, paste the
API key, click Save. The voice model will auto-connect.

**Option B — Set via Web Console:**
Open Firefox DevTools with **F12** (NOT Ctrl+Shift+K — that opens just the
console but F12 is more reliable for toggling), then click the Console tab:
```js
// For Gemini Live:
localStorage.setItem('intendant_gemini_key', 'YOUR_KEY');
// For OpenAI Realtime:
localStorage.setItem('intendant_openai_key', 'YOUR_KEY');
```
Refresh the page to trigger auto-connect.

**Option C — Set via xdotool (programmatic):**
```bash
DISPLAY=:50 xdotool search --name "Intendant Live" windowactivate --sync
DISPLAY=:50 xdotool key F12
sleep 2
DISPLAY=:50 xdotool mousemove 400 658 click 1
sleep 0.3
DISPLAY=:50 xdotool type --clearmodifiers "localStorage.setItem('intendant_gemini_key', 'YOUR_KEY')"
DISPLAY=:50 xdotool key Return
sleep 0.3
DISPLAY=:50 xdotool key F12
```

## Simulating Voice Input

Since there's no real microphone on a headless display, simulate voice input
by sending text directly to the live model via the browser console.

**IMPORTANT**: The voice model needs *context* to call tools. For approvals,
include the system event format so the model knows what to approve:

```bash
# Open console
DISPLAY=:50 xdotool search --name "Intendant Live" windowactivate --sync
DISPLAY=:50 xdotool key F12
sleep 2
DISPLAY=:50 xdotool mousemove 400 658 click 1
sleep 0.3

# Send text to live model (simulates voice)
DISPLAY=:50 xdotool type --clearmodifiers 'modelProvider.sendText("[System: approval needed] Agent wants to run: ls -la /tmp (id: 1). User says yes. Call approve_action with id 1.")'
DISPLAY=:50 xdotool key Return
```

### What works as sendText input:
- **Task submission**: `modelProvider.sendText("Create a hello world file in /tmp")`
  — model calls `submit_task`
- **Approval with context**: `modelProvider.sendText("[System: approval needed] Agent wants to run: COMMAND (id: N). User says yes. Call approve_action with id N.")`
  — model calls `approve_action`
- **Status queries**: `modelProvider.sendText("What are you working on?")`
  — model calls `check_status`
- **Vague requests** (e.g. "yes go ahead" without context): model responds
  with **audio only** (blocked by AudioContext on headless displays). This is
  NOT a bug — it works fine with real voice because audio playback is allowed
  after a user gesture.

### AudioContext warning
On headless displays, you'll see:
```
An AudioContext was prevented from starting automatically. It must be created
or resumed after a user gesture on the page.
```
This is expected — the browser blocks audio playback without user interaction.
Audio works fine when a real user clicks the mic button. For E2E testing,
only text-based tool calls matter.

## Keyboard Input via xdotool

For TUI keyboard shortcuts (approve, quit, etc.), click inside the xterm.js
terminal first to give it focus, then send keys:

```bash
# Click inside the terminal area, then send 'y' to approve
DISPLAY=:50 xdotool mousemove 500 300 click 1
sleep 0.2
DISPLAY=:50 xdotool key y
```

**Gotcha**: If the follow-up text input panel is active, keyboard shortcuts
(v for verbosity, q for quit, etc.) go into the text input instead. Press
Escape first to dismiss the follow-up panel, then send the shortcut.

## Screenshot

```bash
DISPLAY=:50 import -window root /tmp/web-e2e-screenshot.png
```

## What to Verify

### Core functionality
1. **Web TUI renders in Firefox**: xterm.js shows the TUI with status bar,
   log panel, action panel. Dark Catppuccin theme, colored text.

2. **Server connection**: Green "Server" dot in the top-right connection bar.

3. **Server-side presence**: Log entries like "[presence] Thinking..." and
   narration of the task before live model connects.

4. **Voice model auto-connect**: If API key exists in localStorage, the
   live model auto-connects on page load. Green second dot (Gemini/OpenAI).

### Mutual exclusion
5. **Live model connects**: Log shows "Browser live model connected — server
   presence paused". Server presence stops narrating.

6. **Browser disconnect**: Close Firefox (Ctrl+Q) or close tab. Log shows
   "Browser live model disconnected — server presence resumed". Server
   presence resumes. This tests `beforeunload` + server-side auto-cleanup
   on WebSocket drop.

7. **Browser reconnect**: Open Firefox again. Bootstrap `state_snapshot` is
   sent (TUI renders immediately). If live model auto-reconnects, log shows
   "Browser live model connected — server presence paused" again.

### Voice model tool calls
8. **approve_action**: Send text with approval context → model calls
   `approve_action` → log shows "Approved via control socket (turn N)".

9. **submit_task**: Send text requesting work → model calls `submit_task`
   → new task starts in the agent loop.

10. **check_status**: Send "what are you working on?" → model calls
    `check_status` → tool_request/tool_response roundtrip via WebSocket.

## Config Endpoint

```bash
curl http://localhost:8765/config
# Returns: {"provider":"gemini","model":"gemini-2.5-flash-native-audio-preview-12-2025",...}
```

## Verified Results (March 2026)

All tests passed on the first E2E run:

| Test | Result | Notes |
|------|--------|-------|
| Web TUI renders in Firefox | PASS | Full Catppuccin theme, all panels visible |
| Server-side presence narrates | PASS | "[presence] I need your approval to write..." |
| Gemini Live auto-connects | PASS | Saved API key in localStorage triggers auto-connect |
| Mutual exclusion (connect) | PASS | "Browser live model connected — server presence paused" |
| Approval via keyboard (y key) | PASS | Approval panel accepts browser keyboard input |
| Approval via voice model tool | PASS | `approve_action` called via tool_request protocol |
| Task submission via voice model | PASS | `submit_task` called, new round started |
| Disconnect (Ctrl+Q Firefox) | PASS | "Browser live model disconnected — server presence resumed" |
| Reconnect (reopen Firefox) | PASS | Bootstrap state_snapshot + live_connected re-sent |
| Follow-up round via voice | PASS | Round 2 completed with voice-controlled approval |

## Cleanup

```bash
pkill -f 'intendant.*web' 2>/dev/null
pkill -f firefox 2>/dev/null
pkill -f 'Xvfb :50' 2>/dev/null
pkill -f 'x11vnc.*:50' 2>/dev/null
```
