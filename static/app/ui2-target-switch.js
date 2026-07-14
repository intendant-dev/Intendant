// ── composer target hot-swap (seam stub — filled by the switch track) ──
// Contract: reveal + wire #task-target-switch-btn (next to the target
// chip) and fill #ui2-target-switch with the targetable-session listbox.
// Selecting a session retargets the composer IN PLACE (focusSessionWindow
// — it does not switch tabs); listing is gated exactly like the resolver
// (isPromptTargetSessionUsable). Boot follows the ui2-chrome single-boot
// idiom; every entry point is a no-op when the mounts are missing.
