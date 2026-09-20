# Bounded owned-window scrolling (2026-09-20)

Follow-up to merged #943. Prove a bounded native vertical wheel event on a disposable background window, then integrate owner-only prepare/consume scrolling through the retained-window helper. No global posting, activation, keyboard, drag, momentum, retry or live-daemon upgrade. Preserve current authorization, identity, geometry, focus, cancellation and uncertain-effect semantics.
