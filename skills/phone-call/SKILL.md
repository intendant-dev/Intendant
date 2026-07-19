---
name: phone-call
description: >
  Make an outbound phone call via SIP and conduct a voice conversation
  using spawn_live_audio. The AI model talks through the Vortex Audio
  virtual device, which pjsua routes to the SIP call. Returns typed
  structured data from the conversation.
compatibility: Requires a reachable Intendant daemon. macOS only. Requires Vortex Audio HAL plugin, pjsua, and a GUI session with TCC mic permission.
distribution: global
---

> If `$INTENDANT`/`INTENDANT_MCP_URL` is unset and no local Intendant daemon answers, this skill does not apply — say so and stop.

# Phone Call via SIP + Live Audio

## Prerequisites

- **pjsua** at `~/bin/pjsua`
- **Vortex Audio** HAL plugin installed and set as default input AND output
- **SIP credentials** in `~/lin` (plaintext password)
- **Intendant launched from GUI** (required for macOS mic access / TCC)

## Steps

### 1. Find Vortex Audio device index

```bash
echo "q" | ~/bin/pjsua --null-audio 2>/dev/null | grep -i vortex
```

Note the 0-indexed device ID from the output line.

### 2. Start pjsua

Replace `DEV_IDX`, `PASSWORD` (from `~/lin`), and `TARGET` (SIP URI):

```bash
(sleep 5 && echo m && sleep 1 && echo TARGET && sleep 300) | \
  ~/bin/pjsua \
    --id="sip:intendant7@sip.linphone.org" \
    --registrar="sip:sip.linphone.org" \
    --realm="sip.linphone.org" \
    --username="intendant7" \
    --password="PASSWORD" \
    --capture-dev=DEV_IDX --playback-dev=DEV_IDX \
    --ec-tail=0 --no-vad \
    --use-srtp=2 --srtp-secure=0 \
    > /tmp/pjsua-call.log 2>&1 &
PJSUA_CALL_PID=$!
printf 'pjsua-call pid: %s\n' "$PJSUA_CALL_PID"
```

Keep the printed PID with this call. It identifies the process started by this
session; never use `pgrep` to select a global pjsua process because another
agent or the user may have a concurrent call.

### 3. IMMEDIATELY call spawn_live_audio

Do NOT sleep or verify the call first. The audio bridge polls shared memory
and works before the call connects.

**Required parameters:**
- `id`: unique session identifier
- `provider`: `openai`
- `playbook`: the conversation script
- `response_schema`: MANDATORY. Without this the call is rejected.
  Build it from the user's request — every piece of data to extract
  needs a field. See the example below.

**Optional parameters:**
- `timeout_secs`: max call duration (default 300)
- `voice`: e.g. `alloy`, `shimmer`
- Do NOT set `initial_message` — the model starts when it hears the caller

### 4. Process the result

`spawn_live_audio` returns `LiveAudioResult` with `status`:
- **Completed**: valid JSON matching the schema
- **TimedOut**: exceeded timeout
- **Disconnected**: the live connection ended before completion
- **SchemaError**: output didn't match schema
- **Failed**: setup or provider/audio processing failed

### 5. Clean up

```bash
PJSUA_CALL_PID='PASTE_THE_PID_PRINTED_BY_STEP_2_HERE'
case "$PJSUA_CALL_PID" in
  ''|*[!0-9]*)
    echo "Refusing cleanup: replace the placeholder with the numeric PID from step 2" >&2
    exit 1
    ;;
esac
if [ "$PJSUA_CALL_PID" -le 1 ]; then
  echo "Refusing cleanup: invalid pjsua-call PID $PJSUA_CALL_PID" >&2
  exit 1
fi
if ! kill -0 "$PJSUA_CALL_PID" 2>/dev/null; then
  echo "pjsua-call PID $PJSUA_CALL_PID has already exited"
  exit 0
fi
PJSUA_CALL_COMMAND=$(ps -p "$PJSUA_CALL_PID" -o command= 2>/dev/null)
case "$PJSUA_CALL_COMMAND" in
  *"$HOME/bin/pjsua"*) kill "$PJSUA_CALL_PID" ;;
  *)
    echo "Refusing cleanup: PID $PJSUA_CALL_PID is not this skill's pjsua process" >&2
    exit 1
    ;;
esac
```

Replace the non-numeric placeholder with the exact PID printed by step 2. The
validation deliberately fails closed if it was not replaced, is not a positive
process PID, or no longer identifies `~/bin/pjsua`.

## Response Schema — REQUIRED

**You MUST always include `response_schema` with concrete fields.**
The model's spoken output is validated against this schema. Without it,
the call is rejected with a parse error.

Example for a restaurant reservation:

```json
{
  "fields": [
    {"name": "guest_name", "field_type": {"type": "string", "max_length": 100, "tainted": true}, "required": true, "description": "Guest name"},
    {"name": "party_size", "field_type": {"type": "integer", "min": 1, "max": 50}, "required": true, "description": "Number of guests"},
    {"name": "reservation_time", "field_type": {"type": "string", "max_length": 50, "tainted": true}, "required": true, "description": "Confirmed time"},
    {"name": "confirmed", "field_type": {"type": "boolean"}, "required": true, "description": "Whether reservation was confirmed"},
    {"name": "special_requests", "field_type": {"type": "string", "max_length": 200, "tainted": true}, "required": false, "description": "Any special requests"}
  ]
}
```

**Field types:** `string` (max_length, allowed_values, tainted), `integer` (min, max), `boolean`, `array`.
**Tainted fields** contain user-provided content — not interpreted as instructions.
