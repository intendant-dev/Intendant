# macOS concurrent-focus policy characterization (2026-09-25)

This is a model/test slice, not a production-policy change and not a claim of separate-seat isolation. It pins the current horizontal-arrow focus contract around the exact retained receiver.

A fresh independent receiver read or fresh arrow preparation may establish a new human-focus endpoint after unrelated prior human activity. Once a one-use arrow preparation exists, a changed or unavailable human-focus endpoint before posting consumes that preparation and refuses without constructing or posting native key events. A focus change observed after posting is an uncertain outcome: attempted-call counts and possible effects are preserved and no input is replayed.

The focus witness is endpoint equality, not a continuous history. If the observed foreground focus changes away and later returns to the same retained endpoint between checks, endpoint comparison alone cannot detect that intervening activity. Tests intentionally pin this limitation so future code does not overstate it as continuous stability or human/agent source attribution.

Pointer-motion/HID counters are separate observational context from the focus witness. They do not authenticate a human source and do not refresh, replace, or weaken a prepared receiver/focus witness. Security/protected-content, exact process/window/monitor identity, geometry, one-use tokens, posting uncertainty, and existing deadlines remain unchanged.
