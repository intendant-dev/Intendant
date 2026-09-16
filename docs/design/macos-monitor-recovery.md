# Owned macOS monitor recovery and read-only readiness

Follow-up to #930, stacked on its controller branch. Planned slices: (1) owner-surface-only inventory to recover committed generation handles after a lost tool response; (2) exact-generation read-only status through display_readiness.

Inventory must not start a helper, adopt raw IDs, create a monitor, capture pixels, grant authority or expose native/helper identifiers. Scoped callers, including ones with a user-display grant, cannot enumerate the daemon-wide recovery handles. The existing list_displays result remains unchanged.

Status must distinguish registered lifecycle from unverified current capture and unsupported input/streaming. It must preserve malformed/raw-ID refusal and require the existing display-view and shared-session authority. Reuse the bounded broker and serialized lifecycle; no helper respawn, primary-display fallback, global input, app placement or clipboard changes.

Validation will use inline hermetic lifecycle/authority tests and a separate opt-in temporary-HOME HTTP fixture. This document is a plan, not a claim of completed validation.
