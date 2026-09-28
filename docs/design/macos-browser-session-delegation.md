# Explicit managed-browser delegation to a supervised session

Status: implementation in progress; no installed-daemon grant or behavior change.
Base: #967 at cc9c3a60de973323deef807f4b70d286e3ac3e5b. Do not modify the queued keyboard PR.

## Product contract

An owner assigns one already-created managed browser workspace to one authenticated
supervised session for a bounded time. That session can discover a redacted handle,
observe that exact monitor in memory, and use the existing bounded browser-page
keyboard actions. Assignment is explicit and revocable, not an owner-role upgrade.
It grants no ability to create or move windows, change permissions, read another
workspace, inspect CDP endpoints, control the personal desktop, use a clipboard,
activate an app, mint further grants, or send global native input.

The owner's grant must bind the actual gate-resolved session principal and live
session incarnation, not a caller-supplied owner_session_id, bearer-like workspace
label, shared lease holder, or ancestor session. Children/forks/resumes do not
inherit it. The original workspace, monitor generation, native window and browser
page are frozen into the grant; stale/replaced resources refuse. Daemon restart,
session termination, expiry, explicit revocation and workspace teardown invalidate
use. Unknown identities, expired or revoked IAM bindings, and unavailable session
liveness fail closed. Existing per-tool IAM permission checks remain mandatory.

## Authority and concurrency

Expose owner grant/revoke commands through the authorize facade lane, not act.
Delegate authorization must use an unforgeable internal exact-workspace permit;
never set owner_surface=true or enable the global user-display grant for an agent.
Broker exceptions are limited to the already-authorized exact window validation,
monitor resolution and memory-only capture. All native mutation and inventory
operations retain their existing owner checks. Existing owner keyboard behavior
and protected-receiver checks remain unchanged.

Revocation must serialize with the first input dispatch. An accepted key pair may
finish its one release, but revocation completion cannot allow another key-down.
Expiry is rechecked immediately before input. Transport cancellation does not
replay input or orphan the key pair. Use daemon-local bounded state and single-use
request IDs; no silent eviction revives stale authority. Document the bounded
in-flight operation behavior rather than promising an OS-level isolation lock.

## Privacy and discovery

A delegated session receives only its grant ID, workspace ID, safe display/label
metadata, expiry and allowed operations. No profile path, PID, native binding, CDP
URL or browser debug port is delivered. General workspace inventory and dashboard
snapshots/events retain #970 filtering. Observation is an explicit exact-workspace
operation with memory-only pixels and no fallback or artifact path override.

## Acceptance

Use real gate-bound session identities and temporary state in hermetic tests.
Prove no-grant refusal; one-session/one-workspace success; wrong principal,
wrong session and forged labels refused; expiry/revocation/reassignment/restart
refused; no child inheritance; no authority widening; private metadata absent;
raw/facade HTTP and trusted-owner behavior agree; revoke/dispatch race is bounded;
scoped callers cannot grant/revoke, capture the desktop, enumerate other windows,
or submit new endpoints. Missing IAM display permissions must still deny.

Before claiming usable agent CU, run the normal supervised-session request path
against a disposable managed browser, independently observe page keyboard effects,
then revoke and verify no further action occurs. A generic unit-test success is
not this live acceptance. Simultaneous human typing remains a separately reported
measurement, and automatic Agent View reveal remains deferred.
