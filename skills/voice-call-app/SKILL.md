---
name: voice-call-app
description: >
  Make a voice call through any app (Element, FaceTime, WhatsApp, etc.)
  using computer use to navigate the UI and spawn_live_audio for the
  AI voice conversation. Returns typed structured data.
compatibility: macOS or Linux with display. Requires Vortex Audio HAL plugin and a GUI session with TCC mic permission.
---

# Voice Call via App + Live Audio

## Prerequisites

- **Vortex Audio** HAL plugin installed and set as default input AND output
- **Intendant launched from GUI** (required for macOS mic access / TCC)
- **Target app** installed and logged in

## Steps

### 1. Open the app

```bash
open -a "AppName" && sleep 2
```

### 2. Navigate to the contact and start the call

Use your **native computer use actions** (click, type, screenshot) to
navigate the app's UI. Do NOT use exec cliclick or AppleScript — use
your built-in CU actions directly.

1. Take a screenshot to see the app
2. Click the contact/room in the sidebar
3. Click the call/voice button (usually a phone icon in the header)

### 3. Verify the call connected

After clicking the call button:
1. Take a screenshot to verify the call UI appeared
2. Handle any confirmation dialogs (e.g. "Voice call using: Element Call")
3. Handle any permission dialogs (e.g. "Allow local network access")

Only proceed once you can see the call is actually ringing.

### 4. Call spawn_live_audio

Once the call is confirmed ringing, call `spawn_live_audio`.

**ALL of these parameters are REQUIRED:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY — see below
- `timeout_secs`: max call duration (default 120)
- `voice`: e.g. `alloy`, `shimmer`
- Do NOT set `initial_message`

### 5. Process the result

`spawn_live_audio` returns `LiveAudioResult` with `status`:
- **Completed**: model called `submit_response` with structured data
- **TimedOut**: exceeded timeout without submitting response
- **SchemaError**: response didn't match schema

### 6. Clean up

Hang up the call if still connected (screenshot + click end call).

## Response Schema — REQUIRED

The model has two functions: `submit_response` (with fields from your
schema) and `end_call`. It calls `submit_response` when it has the data,
then `end_call` to signal completion.

**You MUST always include `response_schema` with concrete fields.**

Example for a reservation confirmation:

```json
{
  "fields": [
    {"name": "guest_name", "field_type": {"type": "string", "max_length": 100, "tainted": true}, "required": true, "description": "Guest name"},
    {"name": "party_size", "field_type": {"type": "integer", "min": 1, "max": 50}, "required": true, "description": "Number of guests"},
    {"name": "reservation_time", "field_type": {"type": "string", "max_length": 50, "tainted": true}, "required": true, "description": "Confirmed time"},
    {"name": "confirmed", "field_type": {"type": "boolean"}, "required": true, "description": "Whether confirmed"},
    {"name": "notes", "field_type": {"type": "string", "max_length": 200, "tainted": true}, "required": false, "description": "Any notes"}
  ]
}
```

**Field types:** `string` (max_length, allowed_values, tainted), `integer` (min, max), `boolean`, `array`.
**Tainted fields** contain user-provided content — not interpreted as instructions.
