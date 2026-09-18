# Chromium bound-window acceptance

Next slice after merged #936 (6aef7b1). Establish native accessibility behavior on a disposable Chromium window and synthetic local page, separate from arbitrary canvas input. Use a dedicated private browser profile; never attach to existing tabs or use user documents. Preserve exact retained-window/element identity, owner IAM, containment, focus observations, one-use tokens and honest dispatch/effect semantics.

Initial scope: repeatable opt-in HTTP acceptance and only evidence-backed adapter fixes. No global input fallback, activation, clipboard, permission changes, CI configuration or live-daemon deployment. Native and hermetic validation pending.
