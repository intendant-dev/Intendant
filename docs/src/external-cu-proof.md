# Externally driven CU proof sessions

Computer-use execution does not require a model. `cu actions` executes explicit
input; `cu proof` adds an attempt-bound evidence session around explicit input.
Only the optional `cu task` command delegates natural-language instructions to
an internal model. Its provider configuration is not a prerequisite for either
explicit-action interface.

## Authority and ownership

`external_cu_proof` is an owner-surface tool. Its actor comes from the existing
MCP gate, not from request JSON. A session binds one daemon-created virtual
display generation, one local CDP workspace owned by the attempt, and that
workspace's `scout_cdn_capture` lease. Another actor cannot inspect or continue
the session. User-session displays, foreign displays, stale generations and
second active workspaces on the proof display are refused.

The worker retains the native display-exclusion guard across tool calls. It
seals browser-interactive input, uses the existing native execution and
cancellation-cleanup machinery, and does not construct a provider, conversation,
function-tool surface or delegation. An abandoned session expires independently
of the caller. There are at most eight generic sessions per daemon; the Scout
controller additionally holds its existing single-capture host lease.

## Wire protocol

Use `intendant ctl cu proof --request JSON|@file|-`. The request is kept as raw
JSON until duplicate-key validation in the daemon. Unknown fields are rejected.
The MCP equivalent is `external_cu_proof` with one `request` string.

Begin with the actual owned resource identities:

```json
{
  "op": "begin",
  "attempt_id": "cdn-attempt:example",
  "workspace_id": "bw-actual-workspace",
  "display_id": 120,
  "display_target": "display_120",
  "capture_generation": "vdcg-actual-generation",
  "job_sha256": "0000000000000000000000000000000000000000000000000000000000000000"
}
```

The example digest is illustrative, not candidate evidence. Scout supplies the
validated job digest. The response includes `proofId`, `sequence: 0`, the
authenticated `actor`, immutable `binding`, and a private full-display PNG frame.
Every mutation must use the current sequence; accepted mutations advance it
once. A batch contains 1–16 actions and the session permits at most 64 actions.
Actions are prevalidated as a whole before any member is executed. Paste,
split mouse-down/up edges, cropped zoom frames and arbitrary fields are refused.

```json
{"op":"actions","proof_id":"ecup-actual-id","sequence":0,"actions_json":"[{\"type\":\"click\",\"x\":100,\"y\":150}]"}
```

The phase order is:

1. `begin` and `actions`: arrange and inspect the proof, within 180 seconds
   including setup. `status` observes state without advancing the sequence.
2. `freeze`: permanently reject further actions. No unfreeze operation exists.
   This freezes input, not the website's JavaScript or animation.
3. The controller independently records the browser's pre-capture CDP state.
   `observe` receives its `pre_observation_sha256` and captures one fresh PNG.
4. `finish` receives the exact issued `observation_sha256` and duplicate-key-safe
   `claims_json`. Claims are attributed to the external caller. The caller must
   inspect that image; it cannot substitute a later screenshot.
5. The controller checks post-capture CDP state while the guard is still held.
   `close` then releases the proof guard and acknowledges cleanup against the
   exact receipt ID. The controller closes its browser/display/profile.

The frozen phase has a fixed 45-second budget, including observation and claims.
`abort` is always available with the current sequence. A lost action response is
not permission to replay input: inspect `status`. If a terminal acknowledgement
is lost, do not infer success from the earlier receipt. Expired or failed
sessions require honest failure handling and, where appropriate, a new attempt.

## Evidence semantics

Version 2 uses profile `intendant-external-cu-proof-v1`, distinct from the
model-backed version 1 receipt. It records actor/resource/job bindings, bounded
timing and counters, a redacted action transcript, the input-freeze boundary,
exact observation bytes/dimensions/digest, and external claims. It explicitly
records `internalModelCalls: 0`, `claimsAuthority: external-caller`, and
`grantsSubmissionAuthority: false`. It never fabricates provider/model identity
or calls an external assertion an independent policy review.

Transcript and receipt digests use a domain string followed by a NUL byte and
key-sorted JSON. Domains are `intendant-external-cu-stage-v1`,
`intendant-external-cu-transcript-v1`, and `intendant-external-cu-receipt-v1`.
The receipt ID hashes the payload without `receiptId`. These are integrity
bindings, not digital signatures: consumers must obtain the records from the
authenticated control channel and bind them to their admitted job and actor.

Frames, raw claims and host-resource diagnostics are private. The transcript
redacts typed text and key values. It records this interface's execution, not a
claim that the external agent performed no unrelated operations anywhere else.
Sealing, timestamp verification, policy review and owner submission approval
remain separate boundaries.

For Scout, allocate directly from its reserved pool: `intendant ctl display
create --min-display-id 120 --max-display-id 159`. Both bounds are required
when either is supplied, must be an inclusive subset of Intendant’s managed
99–199 pool, and exhaustion fails without spilling into another pool. Ordinary
callers omitting both bounds retain the existing allocator. No filler displays
are created and foreign sockets remain excluded.

## Exact browser viewport

`ctl browser create --viewport 1024x768` creates a Linux display-bound browser with device scale one, then sizes the native browser window until CDP reports the exact CSS viewport. This occurs before workspace readiness; no page script, DOM edits, or device emulation is used. Invalid dimensions, non-owned displays, or failure to settle within ten seconds reject creation and clean up the owned browser. Capture controllers must independently recheck viewport metrics before and after the frozen screenshot.
