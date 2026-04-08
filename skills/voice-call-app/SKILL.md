---
name: voice-call-app
description: >
  Make a voice call through any app (Element, FaceTime, WhatsApp, etc.)
  using computer use to navigate the UI and spawn_live_audio for the
  AI voice conversation. Returns typed structured data.
autonomy: full
---

# Voice Call via App + Live Audio

## Overview

Use computer use to open an app, navigate to a contact, start a voice
call, then spawn_live_audio to conduct the conversation through Vortex
Audio. Works with any app that uses system audio devices.

## Prerequisites

- **Vortex Audio** HAL plugin installed and set as default input AND output
- **Intendant launched from GUI** (required for macOS mic access / TCC)
- **Target app** installed and logged in
- **Autonomy Full** (CU requires display grant + command approval)

## Steps

### 1. Open the app and navigate to the contact

Use `execAsAgent` to open the app, then `captureScreen` + `cliclick`
to navigate the UI. Keep it simple — don't write pixel analysis scripts.
Use captureScreen to see the UI, identify buttons by their position,
and click them with cliclick.

```bash
open -a "Element" && sleep 2
```

Then capture and click. Typical flow:
1. captureScreen → see the app
2. Click the search bar or room list entry
3. Click the call/voice button (usually a phone icon in the header)

### 2. IMMEDIATELY call spawn_live_audio

As soon as you click the call button, call `spawn_live_audio` on the
VERY NEXT command. Do NOT wait, do NOT take another screenshot, do NOT
read source code. The audio bridge works before the call connects.

**ALL of these parameters are REQUIRED:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY — see below
- `timeout_secs`: max call duration (default 120)
- `voice`: e.g. `alloy`, `shimmer`
- Do NOT set `initial_message`

### 3. Process the result

`spawn_live_audio` returns `LiveAudioResult` with `status`:
- **Completed**: model called `submit_response` with structured data
- **TimedOut**: exceeded timeout without submitting response
- **SchemaError**: response didn't match schema

### 4. Clean up

Hang up the call if still connected (captureScreen + click end call).

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
