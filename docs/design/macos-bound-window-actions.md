# Bound-window controls and semantic actions

Two slices atop #932: bounded inspection of controls within a retained, explicitly bound window on its owned monitor; then one-shot AXPress and text AXValue writes against opaque retained-element tokens.

Owner-only plus existing DisplayView/DisplayInput IAM. No CGEvents, global input, focus activation/restoration, clipboard, implicit grants, external apps or installed daemon changes. Token refresh/replay, exact window/element identity, containment, permission revocation and uncertain effects must fail safely.

Implementation and native acceptance are pending. Raw canvas/keyboard control and Chromium compatibility are separate work.
