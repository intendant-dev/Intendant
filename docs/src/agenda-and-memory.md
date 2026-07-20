# Agenda and Memory

Intendant has two daemon-owned systems for state that must outlive a
conversation. They solve different problems and deliberately do not inject
ambient instructions into an agent:

| System | Holds | Authority model | Current storage |
|---|---|---|---|
| **Agenda** | Parked intent: tasks, notes, non-blocking questions, reminders, and proposed scheduled sessions | Any authorized writer can park or propose; only an owner surface can approve or revoke scheduled work | Daemon-wide append-only files under the Intendant state root |
| **Memory** | Machine-wide claims with provenance, sensitivity, and reducer-derived status | Authorized writers can propose candidates; the current product exposes no judgment or curation command | Durable on macOS by default; honestly labeled ephemeral mode elsewhere and on fallback |

In both systems, stored text is **data, never instructions**. Reading an
agenda item or a Memory claim cannot authorize an action, widen autonomy, or
override the current prompt. A future agent must weigh the content and act
through its normal sandbox, IAM, autonomy, and approval gates.

## Agenda

### Scope and files

There is one Agenda per daemon home, shared across projects. It is not a
per-repository todo list and it does not involve Connect, federation, or
owner-plane replication.

`src/bin/caller/agenda/mod.rs` resolves its home through
`intendant_core::state_paths::intendant_home()`:

```text
<INTENDANT_HOME or ~/.intendant>/agenda/
├── agenda.jsonl              append-only item operation log
├── reminder-policy.json      owner-controlled delivery policy
└── occurrences.jsonl         reminder and scheduled-session occurrence journal
```

`agenda.jsonl` is folded into the current item view. Unknown newer operations,
newer record versions, and a torn final line are preserved but skipped so an
older binary does not destroy history it cannot interpret. Multiple daemon
processes sharing one home detect growth and refold before reads and writes.

The item log writes and flushes one complete JSON line per operation, but does
not `fsync` each item operation. It survives ordinary process and daemon
restarts; the v1 contract is not a guarantee against sudden power loss. The
delivery-critical occurrence journal has a stronger rule: it is synced before
a notification or session launch is attempted.

### Items and transitions

An item is a `note`, `task`, or `question`. Its lifecycle is derived from the
operation history:

```text
add ──► open ──complete/answer──► done
          ▲                         │
          └──────── reopen ─────────┘
          ▲
          └──────── reopen ◄── retired

open or done ──retire──► retired
```

The supported commands are:

- `add`, `patch`, `complete`, `reopen`, and `retire`;
- `answer` for an open question (answering also resolves it);
- `propose_effect`, `approve_effect`, and `revoke_effect` for a scheduled
  session.

Items use monotonic ULIDs, so lexicographic order is creation order. Titles,
bodies, tags, and due times have bounded intake. There is no destructive
delete operation: retirement hides an item from the normal open view while
preserving its history.

A question is the durable, non-blocking counterpart to `ask_user`. Parking it
does not stop a session. The owner can answer later, and a future session can
read the reply from the item. Reopening an answered question clears the
current reply view but not the historical operation.

### Due reminders

`due_ms` schedules a notification, not work. The owner controls delivery with
the reminder policy:

- reminders are enabled by default;
- the default urgency is `attention`;
- quiet hours defer all reminder deliveries, including urgent ones;
- a per-item override can select `mute`, `info`, `attention`, or `urgent`;
- an occurrence more than the staleness window past due is summarized in a
  digest instead of delivered as a separate old reminder.

Completing or retiring an item cancels an outstanding reminder. Reopening
does not replay a reminder occurrence that already reached a terminal state;
patching the due time creates a new occurrence.

Notification delivery is at-least-once. The journal records `prepared` before
delivery and a terminal result after it. A crash between those records can
redeliver once. Two live daemons sharing the same home refold each other's
journal writes, but there remains a narrow double-delivery window.

### Scheduled sessions

Scheduled work is a separate effect object that references an agenda item.
This is intentionally stronger than setting a due date:

1. An authorized Agenda writer proposes a manifest containing the goal,
   fire time, and direct/orchestrated execution shape.
2. The daemon computes a digest over the item, effect identity, and complete
   manifest.
3. An owner surface—an authenticated dashboard or owner-local
   `intendant ctl` process—reviews and approves that exact digest.
4. At the approved instant, the scheduler journals the occurrence and asks
   normal task dispatch to create a supervised session.

Agent sessions and peer daemons may propose manifests but cannot approve or
revoke them. Revising a manifest changes the digest and voids the previous
approval. The spawned session gets ordinary session authority; the approval
does not bypass its sandbox, IAM, autonomy policy, or action approvals.

> **Current execution-shape defect:** the scheduler forwards the manifest's
> `orchestrate` value but also sets `direct=true`; session launch gives
> `direct` precedence. Approved scheduled sessions therefore run Direct today,
> even when their manifest requested orchestration. Treat that field as
> recorded intent until dispatch stops forcing Direct.

Quiet hours do not delay scheduled sessions: approving a 03:00 run is an
explicit decision distinct from reminder loudness. A launch that misses its
window while the daemon is down, or is interrupted before launch
confirmation, fails closed and is not automatically retried. The outcome is
written back to `effects[].last_run`.

The scheduler observes dispatch receipts and completion events through the
bounded broadcast EventBus. A lagged receiver is logged but not reconciled
in-process; under extreme event pressure an occurrence can remain
`awaiting_receipt` or `running` until daemon restart resolves it fail-closed
(normally to `unknown`).

### Attribution, provenance display, and `--source`

Every operation records the actor **as the daemon's gates resolved it**
(principal, session id, actor class), mapped from the shared `ActorBinding`
seam at the authenticated edge — never parsed from the request. Coverage:

- **Supervised sessions — external and native — attribute automatically.**
  The daemon injects a session-scoped `INTENDANT_MCP_URL` (a loopback
  capability token derived per session; never a provider key) into external
  backends' env and, since the follow-through slice, into the native
  runtime's command env at spawn (`agent_runner`), so `intendant ctl agenda …`
  run by any shell command inside a supervised session — sub-agents included —
  records `agent_session` with that session's id.
- **Dashboard writes** attribute as the owner surface; **bare local ctl**
  outside any session records `local_process`.
- **`--source LABEL`** (on `add` and the other non-owner verbs) is a
  self-described, explicitly **unverified** label for unsupervised callers —
  cron jobs, git hooks. It is stored beside the actor on the operation
  envelope (and folded into `provenance.source` for `add`), rendered visibly
  as "self-described", and never becomes a principal, session attribution, or
  trust input. Owner-surface verbs (`approve_effect`, `revoke_effect`) accept
  no label.

For display, the ledger snapshot response carries a `sessions` join map
beside the items (never fields on them — the item DTO stays the pure fold
product): each recorded session id resolves through the external wrapper
index to its backend **conversation** (superseded wrapper incarnations
included) or to the native session's log dir, with the session's human name
and the Sessions-tab row key. The dashboard renders the resolved name as a
jump link to that conversation row, keeps raw ids/principal/kind in the
tooltip, and degrades to the raw truncated id whenever nothing resolves
(index pruned, log dir gone) — a dangling recorded id is never an error.

### Surfaces and permissions

Agenda is available in the dashboard, through `intendant ctl agenda`, through
the `agenda_list` / `agenda_op` MCP tools, and through tunnel-twinned HTTP
routes:

| Route | Permission | Purpose |
|---|---|---|
| `GET /api/agenda` | `agenda.read` | Items, status counts, skipped-line count, and the session-resolution join map |
| `POST /api/agenda/op` | `agenda.write` | Apply one validated Agenda command |
| `POST /api/agenda/reminders/policy` | `settings.manage` | Change owner reminder policy |

The reminder policy is settings authority, not Agenda authorship. An
`agenda.write` grant cannot make its own item notify more loudly.

## Memory

### Stamped kernel and product surface

Memory is the first Intendant consumer of the owner-plane D0-A kernel. The
workspace vendors two independently implemented sides:

- `crates/owner-plane-core` constructs canonical, signed operations;
- `crates/owner-plane-reducer` parses, admits, and folds them without sharing
  the writer implementation.

The normative specification, companion schema, and vector corpus live under
`crates/owner-plane-reducer/corpus/`. They are the Gate-A-stamped v0.5.24 cut
vendored from `owner-plane-d0a` at `583f421a`; a test pins the exact
specification bytes. Kernel semantic changes are owner acts made on the asset
branch, not ordinary edits in this repository. Product documentation should
describe the integration without quietly amending that stamped specification.

The D0-A specification is much broader than the current product surface. This
build exposes only:

- `propose` one claim;
- bounded lexical `search`;
- `read` one claim by an unambiguous hexadecimal operation-hash prefix.

It does **not** expose judgment, pinning, erase, export/import, or curation
commands. Proposed claims therefore enter as `candidate`; the presence of
other reducer status names does not mean their product workflows are shipped.

### Claims and retrieval

Claim kinds are a closed vocabulary:

```text
observation | decision | episode | procedure | preference
```

Sensitivity is also closed:

```text
public | internal | private | sensitive
```

The service defaults an omitted sensitivity to `private`. That value is the
writer's classification claim, not export authority.

Every view includes:

- the operation-hash claim id;
- statement, kind, sensitivity, labels, and optional project/model/session
  context;
- gate-derived authorship (`proposed_by`), separate from writer-stated
  context;
- reducer-derived status;
- effective durability (`durable` or `ephemeral`).

Search is capped at 50 results. Candidates are excluded by default; callers
must opt in with `include_candidates=true` or `--candidates`. The dashboard
opts in because all claims in the current product slice begin as candidates.
Nothing performs ambient recall or injects stored claims into a fresh model
conversation. An agent receives only the bounded results it explicitly
searches for.

### Storage and custody

Daemon startup selects a storage mode:

| Condition | Mode |
|---|---|
| macOS, normal startup | Durable store |
| `INTENDANT_MEMORY_EPHEMERAL=1` | Ephemeral |
| Linux or Windows | Ephemeral |
| Durable open/create failure | Logged soft fallback to ephemeral |

Every API and claim view reports the mode actually in use. Operators and
agents should trust that label rather than assuming that platform implies
durability.

The current durable directory is:

```text
~/.intendant/memory-plane/
├── ctrl.iplog          plaintext genesis/control log
├── tenant.iplog        encrypted tenant item commits
├── custody.v1.json     0600 custody seeds and plane identifiers
└── plane.lock          exclusive store lock
```

An acknowledged durable claim is flushed before success is returned. Recovery
truncates a torn tail, while an ambiguous complete-frame CRC failure or
mid-log corruption refuses the durable store with the named read-only
quarantine outcome instead of silently repairing it; daemon startup then
follows the logged ephemeral fallback above. Plaintext tenant operations do
not go to disk, but the current custody sidecar is protected by filesystem
mode rather than the macOS Keychain. Full multi-platform and OS-keystore
custody remain outside this product slice. Memory is local to one daemon; no
replica, backup, or cross-machine synchronization guarantee is shipped.

> **Current path exception:** unlike other daemon state, durable Memory
> currently resolves `~/.intendant/memory-plane` directly and does not honor
> `INTENDANT_HOME`. This is a source/configuration inconsistency, not a
> supported second state-root policy.

### Surfaces and permissions

Memory is available in the dashboard, through `intendant ctl memory`, through
the `memory_search` / `memory_read` / `memory_propose` MCP tools, and through
tunnel-twinned HTTP routes:

| Route | Permission | Purpose |
|---|---|---|
| `GET /api/memory/search` | `memory.read` | Bounded claim search |
| `GET /api/memory/claim` | `memory.read` | Read by id prefix |
| `POST /api/memory/propose` | `memory.write` | Author one candidate |

All write paths funnel through one `MemoryHandle`, which maps the
gate-resolved actor into the claim provenance and signed owner-plane envelope.
Callers cannot supply their own principal through the request body.

### Legacy project memory

The former per-project `.intendant/memory.json` store and its runtime tools
were removed at the owner-plane cutover. Leftover files are inert: Intendant
does not read, ingest, migrate, or delete them. Fresh sessions receive no
unrequested Memory content. Project files or a backend's own private
auto-memory remain separate systems and must not be bulk-copied into the
machine-wide plane.

## Validation gates

Relevant keyless checks are:

```bash
cargo test -p intendant --bins
cargo test -p owner-plane-core -p owner-plane-reducer
cargo clippy --workspace -- -D warnings
```

The controller tests cover Agenda folding, reminder and scheduled-session
crash behavior, Memory provenance/IAM, durable recovery, and the parity
between declared gateway routes and the dashboard chapter. The owner-plane
crate tests enforce the stamped corpus, differential reducer behavior, and
vendored specification hash.
