# Automatic use of a session-assigned managed browser

Status: implementation in progress; no installed-daemon grant or behavior change.
Base: #967 merged as ec04fa076043fc3a3aa5662833dd35e9b9cb9b59. Continue in #971.
Product clarification: 2026-09-28; this document specifies the target, not shipped behavior.

## Product contract

The normal workflow is: the owner starts a task with an agent workspace, the
trusted provisioning path creates/assigns its managed browser, and the ordinary
supervised session can immediately discover it, observe pixels and issue the
supported input actions. Workspace assignment includes control of that workspace;
there is no separate per-key, per-click or recurring renewal approval prompt.

Assignments normally last for the active task/session incarnation and its owned
workspace. An explicit expiry may remain an administrative option, but must not
impose an arbitrary short renewal cycle on ordinary tasks. Assign/revoke commands
are administrative controls and the stop switch, not additional steps the owner
must perform after every task launch. Session termination, workspace teardown and
explicit stop/revocation invalidate use. Resuming a task may provision a fresh
workspace under the same trusted task policy; an old grant never silently attaches
to a different process/window. An idle or waiting-for-input turn is not itself the
end of a persistent task session.

This is not an owner-role upgrade. The session may control only its assigned
workspace; it gains no access to the personal desktop, another session's window,
private CDP endpoints, global input, clipboard or further grants. Neither the
workspace ID nor a caller-supplied owner_session_id is authorization. Trusted
provisioning must bind the existing authenticated session identity and authoritative
incarnation before publishing an assignment, with cleanup if assignment fails.

Deliver the observation/keyboard path first without labeling it a complete visual
workstation. The same assigned-workspace boundary must also cover coordinate click,
scroll and ordinary navigation before claiming the full screenshot/action loop.
Do not require the agent to locate semantic Accessibility controls: pixels plus
coordinates are a supported perception/action model. Native/window metadata may
still validate the target independently.

The owner's grant must bind the actual gate-resolved session principal and live
session incarnation, not a caller-supplied owner_session_id, bearer-like workspace
label, shared lease holder, or ancestor session. Children/forks/resumes do not
inherit it. The original workspace, monitor generation, native window and browser
page are frozen into the grant; stale/replaced resources refuse. Daemon restart,
session termination, expiry, explicit revocation and workspace teardown invalidate
use. Unknown identities, expired or revoked IAM bindings, and unavailable session
liveness fail closed. Existing per-tool IAM permission checks remain mandatory.

## Authority and concurrency

Expose administrative owner grant/revoke commands through the authorize facade lane,
not act. The trusted create-and-assign/task-provisioning path performs the same
authority decision once, without requiring a second owner interaction.
Delegate authorization must use an unforgeable internal exact-workspace permit;
never set owner_surface=true or enable the global user-display grant for an agent.
Broker exceptions are limited to the already-authorized exact window validation,
monitor resolution and memory-only capture. All native mutation and inventory
operations retain their existing owner checks. Existing owner keyboard behavior
and protected-receiver checks remain unchanged.

Revocation must serialize with the first input dispatch. An accepted key pair may
finish its one release, but revocation completion cannot allow another key-down.
Any explicit expiry is rechecked immediately before input. Transport cancellation does not
replay input or orphan the key pair. Use daemon-local bounded state and single-use
request IDs; no silent eviction revives stale authority. Document the bounded
in-flight operation behavior rather than promising an OS-level isolation lock.

## Privacy and discovery

A delegated session receives only its grant ID, workspace ID, safe display/label
metadata, lifecycle state, any explicit expiry and allowed operations. No profile path, PID, native binding, CDP
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

## Product acceptance, not only component acceptance

Test the complete trusted provision -> ordinary session discovery -> screenshot ->
keyboard effects path without an intervening manual grant/approval call. Continue
through multiple turns using the same live assignment. A separate administrative
revoke test must stop new input, while finishing an already-admitted key release.
Test task stop/replacement and failed/cancelled provisioning cleanup. Use only the
session credential injected into the real supervised child for its requests.

Document the authentication generation actually used. A lookup of the *current*
supervisor instance does not by itself distinguish an old bearer token that the
MCP gate still accepts for the same session ID. Do not claim old-credential isolation
without testing the actual token/grant invalidation path.

The next major product milestone is arbitrary native-app background CU, not more
browser-specific polish. See [the delivery milestones](macos-background-cu-delivery.md).
A browser-only success is not completion of native keyboard/hover/drag/shortcuts.

## Session credential incarnation binding (implementation checkpoint)

Registered supervised backend tokens now bind to their session-log lifetime.
Re-registering the same log for an in-task backend respawn preserves the token;
replacing that log rotates its epoch even if an old thread retains the old log.
No new user approval or credential-distribution mechanism is introduced.

The matched credential and epoch are one ingress snapshot carried in ToolCaller
through raw and facade dispatch. Stale stamped requests refuse before execution.
Delegation must also pin and recheck this SAME epoch through its admitted action;
looking up the latest epoch for an old request is insufficient. Unregistered
legacy token routes carry no epoch and cannot gain a delegated browser permit
merely by naming a session. Existing IAM gates and owner posture are unchanged.

This is a production prerequisite, not the completed provision/capture/input
workflow. Assignment, exact-resource permits and lifecycle cleanup still apply.
