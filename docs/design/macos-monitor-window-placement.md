# Owned monitor window binding and placement

Next slice after #931: explicit owner-authorized selection of a window bound to its process generation and an exact owned-monitor generation; verify requested window placement without activating or raising applications, injecting input, changing clipboard state, granting permissions, or adopting native display IDs.

Native AX side effects must stay in the documented AX island. Missing private window mapping, missing permissions, app refusal, off-monitor placement, stale identity and ambiguous mapping fail closed. Bound resource retention and serialize against monitor destruction. Use inline hermetic tests and an opt-in disposable native fixture; never target user documents or restart the installed daemon.

This is a design checkpoint, not a completed feature or test claim.
