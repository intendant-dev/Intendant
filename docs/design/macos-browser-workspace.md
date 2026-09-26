# Managed browser workspace on an owned macOS monitor

This implementation connects the existing owner-only monitor and retained-window
controls to `create_browser_workspace`. It is not general macOS computer-use
completion, a separate seat, or a claim that concurrent human/agent typing is solved.
The input studies in #963 and #967 remain separate. Agent View automatic reveal
is an explicitly deferred agenda item and is not implemented here.

## Contract

An owner calls `browser create URL --display-target EXACT_MACOS_GENERATION` through
the existing MCP/facade/ctl path. HTTP calls require the original RuntimeControl
permission plus DisplayView and DisplayInput; owner-surface trust is independently
required. Scoped user-display grants are insufficient. Legacy control-bus messages
carry no caller trust and cannot acquire owner authority from a selector.

The first slice admits only managed Chrome for Testing, `about:blank` or HTTP(S),
a fresh daemon-created profile, one background 720x530 window inset at (40,40), and a monitor of
at least 800x600. The inset avoids the observed macOS top-edge clamping. It refuses system-browser providers, supplied profiles, extensions,
external URL handlers, malformed selectors and numeric monitor aliases. Linux
Xvfb workspace behavior remains separate.

The same-binary private main-thread supervisor uses NSWorkspace without activation
or running-app substitution. Browser credentials/admission environment variables
are not forwarded. It owns the exact NSRunningApplication returned by its original
launch completion. EOF, foreground observation, cancellation and launch timeout
request cleanup; a deadline never turns an outstanding callback into proof that
no application exists. Terminate/force-terminate are one-shot lifecycle requests.
A final explicit cleanup receipt, not PID disappearance alone, verifies teardown.

A bounded loopback CDP connection creates exactly one background target. It performs
no tested clicks, typing, script mutation or bring-to-front operation. The retained
browser PID/birth is matched against the bounded AX candidate. Existing bind/place
operations validate the exact window, monitor, geometry and observed focus. Their
authority, input guard, timeouts and replay behavior are not relaxed.

The Ready record returns `macos_window_binding`; existing element, pointer, scroll
and fixed-arrow tools operate on that token under their own permissions. The generic
monitor still advertises `input_supported:false` and `streaming_supported:false`.
Owner-only native workspace endpoints are omitted from scoped workspace inventories.

## Lifecycle and uncertainty

A broker-local workspace lane serializes creation, explicit close and tool-driven
monitor destruction. Owned asynchronous workers retain cleanup if a caller stops
waiting. Destruction validates the exact live display ID/generation pair before
closing any browser. Browser cleanup must complete before that monitor is removed.
Unknown cleanup is retained as an Error workspace rather than represented as Closed.
A generation-specific background check closes the owned browser when the monitor
or supervisor becomes unavailable. It never recreates a monitor or falls back.

These are bounded observations, not continuous focus isolation. WindowServer
hotplug can still rearrange windows. Browser launch cancellation and native OS
callbacks can remain pending beyond frontend deadlines; report that state and
inspect the original resource rather than retrying input or adopting a replacement.

## Validation and native reproduction

Hermetic tests cover exact selector parsing, fresh-profile nonreuse, actual
HTTP/raw/facade permission checks, scoped creation/Starting-row cleanup refusal,
stale destruction without browser effects, strict duplicate-safe supervisor PID
receipts, and a mock CDP peer pinning the one background creation request. The
shared C lifecycle state has a non-GUI assertion executable covering cancellation,
late completion, one-shot termination, escalation and verified completion.

```sh
cargo test --locked -p intendant --bin intendant browser_workspace
clang -x c -std=c11 -Wall -Wextra -Werror \
  -DINTENDANT_BROWSER_LIFECYCLE_TEST \
  crates/intendant-platform/src/macos_browser_lifecycle.h \
  -o /tmp/browser-lifecycle-tests
/tmp/browser-lifecycle-tests
python3 scripts/verify-macos-browser-workspace.py \
  --bin /absolute/built/intendant \
  --browser-app '/absolute/Google Chrome for Testing.app' \
  --report /absolute/fresh-report.json --allow-shared-session-monitor
```

Native acceptance is explicit opt-in, not a default CI test. The harness boots
a separate temporary-HOME mock-provider daemon, serves the repository's synthetic
form on loopback, creates a monitor and browser via the actual HTTP tools, performs
one text action and one button action, independently observes fixture state,
checks memory-only capture, then destroys the monitor with its browser still bound.
It records the binary hash/version, failures and cleanup separately. It never
updates the installed app/daemon or uses existing user documents, tabs or profiles.
Passing compilation or a safe refusal is not positive native effect acceptance.

## Native checkpoint: 2026-09-26

**Draft: native form-effect acceptance has not passed.** Four fresh isolated
invocations are summarized in `evidence/macos-browser-workspace-20260926.json`.
Every invocation restored the original display inventory and reaped its temporary
daemon. No form text or button action was sent.

The first two exposed a startup geometry bug: requesting the top edge at y=0
produced y=30 in both AX and CG observations. The workspace now requests a
720x530 rectangle at (40,40) on a monitor at least 800x600. Placement diagnostics
retain the underlying geometry, attempted writes, focus state and failure detail.

With that correction, live-03 returned a Ready workspace, the exact bound window
and the independently observed pristine form. Its control-tree read then refused
on a 50 ms kAXErrorCannotComplete protection-metadata read. Destroying its monitor
closed the bound browser, with verified native cleanup. Live-04 instead refused
during placement re-observation on the same AX timeout class. The existing safety
checks and messaging timeout were not relaxed.

The harness now observes the exact monitor before attempting input and permits at
most three fresh read-only control snapshots on that transient AX error, retaining
each failed observation. It never retries text, button, pointer or keyboard input.
The updated read-only path was not reached in live-04. Four hermetic tests pin
success, transient readiness, exhaustion and non-transient refusal.

Fresh validation: 57 focused Rust tests, 19 pure native lifecycle assertions,
4 Python readiness tests, formatting and whitespace checks passed. Full
cross-platform validation is delegated to PR CI. The remaining native gate is
reliable startup AX readiness followed by independently verified form effects,
capture and cleanup, not concurrent typing or automatic Agent View reveal.
