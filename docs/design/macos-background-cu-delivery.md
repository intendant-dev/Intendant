# Background computer use: product milestones

Owner priority confirmed 2026-09-28. This is the delivery target, not a claim that
all described behavior is implemented. The browser is the first useful milestone;
arbitrary native applications are a core goal, not an optional browser extension.

## User-facing model

Give an agent a workspace as part of starting its task. It sees a screenshot,
chooses an action (including coordinates), performs the action on that workspace,
and checks a new screenshot. It should not move the human's pointer, steal keyboard
focus, or require approvals for every action inside the assigned workspace.
Accessibility can supply target validation or semantic shortcuts; it is not the
only permitted way to identify controls. The screenshot/action interface should
stay consistent as input backends evolve.

## Delivery order and honest exit conditions

1. **Ordinary-agent browser handoff.** Trusted task provisioning assigns an owned
   browser and the real supervised child can discover, capture and use supported
   keyboard operations with its own credential, without a separate grant prompt.
   Stop/termination invalidates the assignment and cleans owned resources. #971
   is this integration; the manual-grant mechanism alone is not its product exit.
2. **Complete assigned visual loop.** The same session can choose display/window
   coordinates from a screenshot and click/scroll/navigate within its assigned
   workspace, then observe the result. Do not report this done from keyboard-only
   or owner-only tests. No fallback to the human's desktop or another workspace.
3. **Native-app background workstation.** Extend the existing retained-window
   input path to useful typing/editing keys, hover/move, right/double click and
   bounded drag, then exercise real multi-step native-app tasks. Start with a
   disposable standard AppKit editor plus a custom-drawn canvas (so semantic
   controls cannot substitute for every coordinate action), then a representative
   real nonbrowser app. Mark each operation's tested compatibility explicitly.
   App-specific failures are data, not permission to activate the human's desktop.

## Native-app decision gate

Prove actual receiver effects and the human's foreground cursor/keyboard/clipboard
behavior separately. A screenshot proves visible state, not input isolation; an
API acknowledgement is not a verified application effect. Coordinate transforms
must include capture scale, display origin and retained window geometry, including
asymmetric nonzero origins so accidental reflections are exposed.

Keep the same screenshot/action surface if a class of native apps ultimately
requires a genuinely isolated guest/session backend. Do not claim that a virtual
monitor creates a separate keyboard or pointer, and do not accumulate silent
foreground fallbacks just to make an app pass. Evaluate that backend choice from
representative native results before widening the compatibility claim.

## Not on the critical input path

Automatic Agent View reveal remains deferred. It should eventually show the exact
active agent workspace without activation and respect an explicit user hide.
Streaming and cosmetic preview work do not block delivering the input workflow.

Existing foundation: #968 managed browser lifecycle, #970 privacy/cleanup, and
#967 managed-page keyboard are merged. That is component evidence, not an automatic
ordinary-agent or arbitrary-native-app completion claim.
