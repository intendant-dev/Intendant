# Owned macOS monitor recovery and read-only readiness

Follow-up to #930, stacked on its controller branch. This worktree implements two read-only slices: owner-surface inventory for committed handles after a lost response, and exact-generation status through `display_readiness`.

`inspect display monitors` routes to typed `list_macos_monitors` under the existing `DisplayView` IAM operation and actual caller trust. Inventory additionally requires an owner surface, at ingress to the broker and at actor execution/delivery. A scoped caller's user-display grant never permits daemon-wide handle enumeration. `list_displays` keeps its existing shape and behavior.

Before any create, inspection returns `broker_state: "not_started"` and an empty `monitors` array without allocating the actor, helper or native state. Live reads use the same bounded serialized actor, waiting behind receipt commit/rollback and cleanup. Only committed, currently verified owned objects appear, with logical `display_id`, exact `display_target`/`capture_generation`, and geometry. Inspection uses existing helper liveness and `Resolve`; it never adopts, captures or respawns. Verification failure retires the broker through its owned cleanup and returns `retired` with no handles. Queue saturation/deadline reports `unavailable` with no handles; it neither replaces nor bypasses the cleanup worker. Private native IDs, helper handles, PIDs, paths and internal failure diagnostics are absent from snapshots.

`display_readiness` intercepts only exact, canonical `macos_virtual:<owner>:<generation>` selectors before ordinary display resolution. It requires current `DisplayView` at the existing ingress gate plus owner-surface or existing scoped user-display authority, rechecked at actor dequeue/delivery. Malformed selectors and numeric/raw-ID aliases remain refused. Physical/Linux/default paths remain unchanged. Status returns `lifecycle_status: "verified"` only for the matching live owned generation; unknown/stale/foreign handles report `unknown` separately from `retired` or `unavailable` broker state. Unknown or retired results contain no usable monitor handle.

Create, recovery inventory and status derive monitor descriptions centrally. Status never captures: `capture_ready` stays false and `capture_status` unverified, even after an earlier successful screenshot. `input_supported`, `streaming_supported` and overall `ready` remain false when `lifecycle_ready` is true. No grant, GUI permission probe, input, streaming, physical fallback, browser placement or clipboard behavior is added.

## Validation

Inline hermetic tests cover fake broker lifecycle, cancellation/receipts, authority revocation, serialized public fields, routing and HTTP IAM dispatch. Status preserves the common `target`, `summary` and five-layer readiness envelope; native permissions remain explicitly unprobed and input blocked.

The opt-in `scripts/verify-macos-monitor-http.py --check-recovery` fixture passed on 2026-09-16 using a separate temporary-HOME loopback daemon and a disposable pattern window. It discarded the creation handle locally, recovered the same generation in a new HTTP call, captured the expected 800x600 four-quadrant pattern, and destroyed the recovered generation. Inventory was empty before creation and after destruction. Status kept capture unverified and CU/input/streaming false both before and after a successful screenshot. Stale selectors were refused, the artifact was owner-private, and parent EOF cleanup restored the original native display inventory. This simulates lost client state; it does not cut a real tunnel or prove arbitrary app input compatibility.

The installed daemon and user permission settings were not changed. Default tests remain free of native display creation. Full branch validation results are recorded on PR #931. Its stacked base means the repository's main-only cross-platform workflow does not run yet; retargeting after prerequisite merges still requires that CI.

## Subsequent work

Keep #925's existing-window input work separate. The following GUI slice now implements explicit, process-generation-checked app window binding and verified placement on a selected owned monitor; see `macos-monitor-window-placement.md` for its contract and pending validation. It adds no activation or clipboard changes. A later input slice must demonstrate effects in native and Chromium/canvas fixtures while testing human focus/pointer interference; creation of a virtual monitor alone does not establish input isolation.
