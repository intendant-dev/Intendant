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

### 1. Navigate to the app and start the call

Use your native computer use actions to interact with the screen.
Take a screenshot, find the app (in the dock, taskbar, or already open),
click to foreground it, navigate to the contact, and click the call button.

Do NOT use exec commands for GUI navigation. Use your built-in
click, type, scroll, and screenshot actions for everything visual.

**Element-specific:** When you click the phone icon, a dropdown asks
"Voice call using: Element Call / Legacy call". Always pick **Legacy call**
(not Element Call). Element Call is a conference that requires the other
party to manually join; Legacy call rings their device directly.

### 2. Call spawn_live_audio immediately

As soon as you click the call button (or select "Legacy call"),
call `spawn_live_audio` on the VERY NEXT turn. Do NOT take a verification
screenshot first — the audio bridge works before the call connects.

**ALL of these parameters are REQUIRED:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY — see below
- `timeout_secs`: max call duration (default 120)
- `voice`: e.g. `alloy`, `shimmer`
- `initial_message`: Set this to a greeting like "Hi, this is [role],
  calling about [topic]." The AI speaks first on outbound calls.

### 3. Process the result

`spawn_live_audio` returns `LiveAudioResult` with `status`:
- **Completed**: model called `submit_response` with structured data
- **TimedOut**: exceeded timeout without submitting response
- **SchemaError**: response didn't match schema

### 4. Clean up

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
