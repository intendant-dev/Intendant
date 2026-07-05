# Computer Use & Live Audio

This page covers three related capabilities: the provider-agnostic **computer
use** (CU) abstraction that lets a model see and drive a desktop, the
**live audio** system that connects an untrusted voice model to an application
through a virtual audio bridge, and the **phone-call** / **voice-call-app**
skills built on top of live audio.

For the WebRTC capture/encode/stream stack that puts pixels on screen for a
human viewer, see [Display Pipeline](./display-pipeline.md). This page is about
the *model* seeing and acting, not the human-facing video transport.

## Computer Use

The CU abstraction (`src/bin/caller/computer_use.rs`) gives any provider a
common action set and dispatches it through a platform backend. Provider-
specific parsing of CU tool calls (OpenAI computer-use, Anthropic
`computer_20251124`, Gemini) lives in `provider.rs`; the executor here is
provider-neutral. Anthropic `wait`/`hold_key` durations arrive in **seconds**
and are converted to milliseconds (clamped to 30 s) at parse time.

The full `CuAction` vocabulary (the same tagged JSON accepted by the MCP
`execute_cu_actions` tool and `intendant ctl cu actions`):
`click`, `double_click`, `triple_click`, `mouse_down`, `mouse_up`, `type`,
`paste` (clipboard + paste chord — fast for long text; restores the previous
clipboard text on the user's macOS session), `key`, `hold_key`, `scroll`,
`move_mouse`, `drag`, `screenshot`, `zoom` (region capture at the highest
resolution the platform can supply — native 2x pixels on Retina), and `wait`.
`intendant ctl cu actions --help` prints the per-action field shapes with an
example.

### Backends

`DisplayBackend` (in `computer_use.rs`) is detected at runtime by
`DisplayBackend::detect()` — macOS → `MacOS`; Windows → `Windows`; otherwise
`WAYLAND_DISPLAY` set → `Wayland`; else `X11`. It can be forced via the
`backend` config value.

| Backend | Screenshot capture                          | Input injection            | Platform        |
|---------|---------------------------------------------|----------------------------|-----------------|
| X11     | live session frame, else in-process root-window capture via `x11rb` | `x11rb`/XTest | Linux (X11) |
| Wayland | live session frame (PipeWire portal)        | portal `InputEvent`s       | Linux (Wayland) |
| MacOS   | live session frame, else `screencapture`    | in-process CGEvents        | macOS           |
| Windows | live session frame (DXGI)                   | `SendInput` via session    | Windows         |

Wayland and Windows are **session-only** backends: both capture and input run
through the live `DisplaySession` (WebRTC pipeline); without an active session
the executor returns an actionable error naming the recovery path. X11 and
macOS inject input directly — X11 uses a persistent `x11rb`/XTest connection
with in-process root-window capture and clipboard support; macOS posts
CoreGraphics `CGEvent`s in-process (no `cliclick`/`osascript` subprocesses; key
chords use ANSI-US virtual keycodes, unicode typing is layout-independent) —
and prefer the in-memory frame of a live capture session for screenshots,
falling back to their platform capture paths when no session exists.

**Post-action freshness:** capture backends are damage-driven, so a screenshot
taken right after an input action waits (bounded, 300 ms) for a frame captured
*after* the action before falling back to the freshest available frame — the
model never sees pre-click pixels after a click that changed the screen.

Coordinates from the model are in the provider's logical screenshot space and
are scaled to the backend's actual pixel/point space before dispatch (important
on HiDPI and under the Wayland portal, which reports its own stream size).
`zoom` is the deliberate exception: on macOS it captures raw physical pixels
(2x on Retina) and crops the requested logical region, because detail is the
point.

### Element Observation (`read_screen`)

`read_screen` is the cheap textual grounding path: a filtered accessibility
tree of the frontmost application's focused window — roles, labels, values,
and logical-point frames, plus a one-line summary of other visible windows —
typically a few hundred tokens versus ~1.5k for a screenshot. Clicks ground
deterministically: click the center of a reported frame. Depth/node caps are
always announced in the output, never silent.

Implemented for the user session on all supported platforms: macOS AX
(`src/bin/caller/ax.rs`, raw `accessibility-sys` bindings — the documented Unix
`unsafe` exception), Linux AT-SPI (`src/bin/caller/atspi_read.rs`), and Windows
UIA (`src/bin/caller/windows_uia.rs`). It inspects the user's session, not
virtual display pixels, and requires the platform accessibility path to be
available. Exposed as an MCP tool (in the `core` bootstrap profile) and as
`intendant ctl cu elements [--format json]`. Pixels remain the fallback for
visual verification and for apps with sparse accessibility trees (web views
notably).

### Who Can Call CU

Every agent Intendant runs or supervises reaches the same executor:

- **Native agent** — provider-native CU tool calls (`cu_enabled`), executed
  in the controller.
- **Codex / Claude Code (supervised)** — the MCP bootstrap set
  (`tool_profile=core`) carries `read_screen`, `take_screenshot`, and
  `execute_cu_actions`; the injected `$INTENDANT`/`INTENDANT_MCP_URL` env
  makes `"$INTENDANT" ctl cu ...` work from their shells for the long tail.
  See [External Agent Orchestration](./external-agent-orchestration.md).
- **Anything with a shell** — `intendant ctl cu` speaks to the running
  daemon's `/mcp` endpoint; discovery is lazy via `--help`.

All paths execute under the same server-side autonomy/approval model
(`DisplayControl` session grant for the user's display).

### Display Targets

CU actions operate on a `DisplayTarget` (`#[serde(tag = "kind")]`):

- **`Virtual { id }`** — an Xvfb-managed virtual display (`:99`, `:100`, …).
  `display_env_string()` → `":<id>"`.
- **`UserSession`** — the user's real desktop. On Linux X11 it resolves the
  login session's `DISPLAY` (falling back to `:0`); on macOS the primary display
  doesn't use `DISPLAY`. Requires an explicit `DisplayControl` grant via the
  autonomy system.

User-session access uses a **session-grant** model: approve once (the `d` hotkey
the dashboard control, MCP `grant_user_display`, or
`intendant ctl display grant-user`), and the grant holds for the rest of the
session until revoked. On Wayland, granting starts the GNOME portal flow. For
Computer Use, the operator must enable **Allow Remote Interaction** in the
physical portal dialog before clicking **Share**; approving screen sharing alone
can produce screenshots while leaving keyboard/mouse injection unavailable. See
[Autonomy & Approvals](./autonomy.md) for the approval surface.

### CU-First Routing

> **Status note (vaulted 2026-07-04):** the all-tasks CU-first interception —
> where every non-direct task in the agent loop is first offered to a fast CU
> model (`try_cu_first` in `main.rs`) — is **off by default**, behind
> `[experimental] cu_first_routing = true`. In practice it added a model hop
> (latency) to every task and, under subscription-based external agents
> (Codex, Claude Code), reintroduced an API-key model the deployment
> otherwise didn't need. The code stays runnable for a future pickup. What
> remains always-on is the *frame-grounded* dispatch described below: when
> the user issues a task while pointing at a display, that is an explicit
> computer-use request, and the CU task path is the only machinery that can
> act on the referenced frames. The fast paths and observation layer above
> are designed for the heavy agents (native, Codex, Claude Code) as primary
> consumers and do not depend on either router.

Frame-grounded work goes to a fast CU model, with escalation to the heavy
agent for anything that turns out to need code changes. The routing decision
lives in the session supervisor (`session_supervisor.rs`,
`start_new_session` → `spawn_cu_task`):

```
submit_task / StartTask
        │
        ▼
 reference_frame_ids non-empty?
        │
        ├── yes ─▶ spawn_cu_task  (fast CU model, with the referenced frames
        │             │            resolved to images as visual context)
        │             │
        │             └── CU model decides it's not a display task
        │                  → calls escalate_to_agent → heavy agent runs it
        │
        └── no  ─▶ normal agent / orchestrator path
```

The gate is **`reference_frame_ids` being non-empty** (the frames the user was
looking at, supplied by [presence](./presence.md)'s `submit_task`); the
`display_target` hint is carried through to tell the CU pipeline which display
to act on. Earlier docs implied `display_target` alone triggers CU routing — the
actual trigger is the presence of reference frames. The CU provider is given
native CU tools plus a single `escalate_to_agent` function tool; calling it ends
the CU run with `CuTaskResult::Escalate` and hands the task to the main agent.

### Configuration

```toml
[computer_use]
provider = "gemini"          # optional; default = CU_PROVIDER / PROVIDER env, else auto
model = "gemini-3-flash-preview"  # optional; gemini default shown
backend = "auto"             # "x11" | "wayland" | "macos" | "auto" (default)
```

Provider/model resolution (`provider::select_cu_provider`): config → `CU_PROVIDER`
/ `CU_MODEL` env → `PROVIDER` env → auto. Default models when unset: gemini
`gemini-3-flash-preview`, anthropic / openai use their CU-capable defaults.

## Live Audio

`spawn_live_audio` is an **agent tool** (defined in `src/bin/caller/tools.rs`),
not a CLI flag. It spins up an *untrusted* voice sub-agent that talks to Gemini
Live or OpenAI Realtime and exchanges audio with an application through a virtual
audio bridge.

### How It Works

```
voice model ──WebSocket──▶ Intendant ──audio bridge──▶ application
   │  PCM16 mono 24 kHz        │   virtual mic/speaker      │
   │  structured tool calls    │   (Vortex shm / PulseAudio)│
   │◀─────────────────────────│◀──────────────────────────│
```

1. The agent calls `spawn_live_audio` with `id`, `provider`, `playbook`, and a
   mandatory `response_schema` (plus optional `timeout_secs`, `voice`,
   `display_id`, `initial_message`).
2. Intendant opens an audio bridge and connects to the voice model with a
   *whitelisted* tool set generated from the response schema.
3. The model follows the playbook; its turns are bridged as audio to/from the
   app, and inbound audio is also teed to Whisper for a transcript.
4. When the model calls `submit_response`, the data is validated against the
   schema; the tool returns a `LiveAudioResult` with a `status` of `Completed`,
   `TimedOut`, or `SchemaError`.

### Security Model — Untrusted, Schema-Validated, Quarantined

The voice model is treated as hostile input. It has **zero tools** beyond the
two generated from the schema (`submit_response`, `end_call`) and **zero file
access**. Three layers protect the rest of the system:

- **Whitelisted tools + schema validation** (`schema_validator.rs`): the model
  can only call the response tool; submitted data is checked against the
  declared `ResponseSchema`, with oversized fields truncated and off-schema data
  rejected.
- **Quarantine** (`quarantine.rs`): any unexpected content — a tool-call attempt
  for an unknown tool, oversized strings, off-schema payloads — is written to
  `~/.intendant/quarantine/<live_audio_id>/<payload_id>.json` and **only a
  reference is returned**; the raw content is never surfaced to the agent.
- **Sandbox**: when the write sandbox is enabled, live-audio processes can write only to the session
  log and quarantine directories — no project root, no `/tmp`.

### Silence Watchdog

The session loop runs a time-based watchdog (`live_audio.rs`): if there has been
**no model output for 15 seconds**, it sends one nudge ("Are you still there?
Please continue the conversation.") to unstick a frozen model, and resets when
output resumes. A separate turn counter nudges the model toward emitting its JSON
response after enough turns. (Earlier docs described "6 consecutive unresponsive
turns" — the real mechanism is the 15-second silence timer.)

### Audio Bridge — Per Platform

The bridge is selected at spawn time by probing for the Vortex shared-memory
segment (`shm_open("/vortex-audio")`). The preferred path on **both** macOS and
Linux is the **Vortex Audio HAL plugin via a direct POSIX shared-memory bridge**
(`start_vortex_shm_bridge`, `shm_open` + `mmap`, no daemon/socket). If the Vortex
segment isn't present, it falls back to the per-OS device bridge:

| Path                  | Implementation |
|-----------------------|----------------|
| Vortex (preferred)    | Direct POSIX shm ring buffers shared with the Vortex HAL plugin (`start_vortex_shm_bridge`). Converts Vortex Float32 stereo 48 kHz ↔ model PCM16 mono 24 kHz. POSIX-only. |
| Linux fallback        | PulseAudio null sinks via `pactl` (virtual mic/speaker, set as default for the session, restored on drop). |
| macOS fallback        | BlackHole virtual device + `SwitchAudioSource` (legacy). |

Audio routing is only needed for the app-to-model bridge. Browser voice
interaction through the [dashboard](./web-dashboard.md) (Gemini Live / OpenAI
Realtime) needs none of this.

### Configuration

```toml
[live_audio]
enabled = false                # default: false
default_timeout_secs = 300     # default: 300 (5 minutes)
gemini_model = "gemini-2.5-flash-native-audio-preview-12-2025"  # optional
openai_model = "gpt-4o-realtime-preview"                        # optional
sample_rate = 24000            # default: 24000
```

`LiveAudioSpawn` is its own [autonomy category](./autonomy.md#action-classification),
so spawning a voice session can be gated independently of other actions.

## Phone Calls (`phone-call` skill)

`skills/phone-call/SKILL.md` places an outbound SIP call and conducts the
conversation with a voice model. **macOS only**; requires the Vortex Audio HAL
plugin, `pjsua`, and a GUI session with TCC mic permission.

```
voice model ──shm──▶ Vortex Audio device ──▶ pjsua (SIP/SRTP) ──▶ phone
   │                                                                          │
   │◀────────────────────────────────────────────────────────────────────────│
```

How it works:

1. Find the Vortex device index (`pjsua --null-audio | grep vortex`).
2. Start `pjsua` with Vortex as both `--capture-dev` and `--playback-dev`,
   SRTP enabled, dialing the target SIP URI.
3. **Immediately** call `spawn_live_audio` (`provider: openai`) — do not wait
   for the call to connect; the shm bridge polls and works before connect.
4. The model conducts the call per the playbook and returns structured data.
5. Clean up `pjsua`.

`response_schema` is **mandatory** — without it the call is rejected with a parse
error. Do **not** set `initial_message`: the model starts speaking when it hears
the callee.

## Voice Calls Through Any App (`voice-call-app` skill)

`skills/voice-call-app/SKILL.md` makes a voice call through **any** app (Element,
FaceTime, WhatsApp, …) by combining [computer use](#computer-use) to drive the
UI with `spawn_live_audio` for the conversation. **macOS or Linux with a
display**; requires the Vortex Audio HAL plugin and a GUI/TCC mic permission.

How it works:

1. Prepare the `spawn_live_audio` arguments (playbook, schema, voice, id)
   *before* dialing, so they fire the instant the call connects.
2. Use CU actions to foreground the app, navigate to the contact, and click the
   call button. (`take_display_control` is **not** required for
   `execute_cu_actions` — only take it if you need exclusive input.)
3. Call `spawn_live_audio` (ideally in the same turn as the call click to
   minimize dead air).
4. Write the result from `response_data` immediately; hang up on completion.

The voice model has two generated functions here: `submit_response` (the schema
fields) and `end_call`. It submits data, then signals `end_call`.

### Response Schema Format

Both skills use the same `ResponseSchema` shape (`live_audio_types.rs`). Each
field nests its type under `field_type`:

```json
{
  "fields": [
    {"name": "guest_name",       "field_type": {"type": "string",  "max_length": 100, "tainted": true}, "required": true,  "description": "Guest name"},
    {"name": "party_size",       "field_type": {"type": "integer", "min": 1, "max": 50},                "required": true,  "description": "Number of guests"},
    {"name": "reservation_time", "field_type": {"type": "string",  "max_length": 50,  "tainted": true}, "required": true,  "description": "Confirmed time"},
    {"name": "confirmed",        "field_type": {"type": "boolean"},                                     "required": true,  "description": "Whether confirmed"},
    {"name": "special_requests", "field_type": {"type": "string",  "max_length": 200, "tainted": true}, "required": false, "description": "Any special requests"}
  ]
}
```

Field types: `string` (`max_length`, `allowed_values`, `tainted`), `integer`
(`min`, `max`), `boolean`, `array`. The voice model cannot submit until all
`required: true` fields are filled. Fields marked **`tainted: true`** carry
user-/callee-provided content and are treated as untrusted data, never as
instructions.

## Browser Microphone Transcription

Separately from live audio, the server can transcribe the *user's* dashboard
microphone via Whisper (`transcription.rs`). Off by default. The web gateway
buffers `user_audio` WebSocket frames into ~3-second chunks, filters silence by
RMS energy, wraps them as WAV, sends them to the transcription API, and
broadcasts `user_transcript` events.

```toml
[transcription]
enabled = true
provider = "openai"
model = "whisper-1"          # default
language = "en"              # optional ISO-639-1 hint
# endpoint = "http://..."    # optional; custom/self-hosted whisper-compatible endpoint
```

Requires `OPENAI_API_KEY` (or a custom `endpoint`).
