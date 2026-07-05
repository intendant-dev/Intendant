---
name: native-goal-smoke
description: Keyless end-to-end smoke for operator goals on a NATIVE session — drives a mock-provider intendant over the control socket and proves set/get/notice-injection/clear/honest-failure
---

# Native-session goal smoke

Verifies the native `/goal` engine (the shared `external_agent::GoalEngine`
running in `run_with_presence`) against real binaries with the scripted mock
provider — no API keys, no network, ~5 seconds.

```bash
cargo build --release
node tests/skills/native-goal-smoke/driver.cjs "$PWD/target/release/intendant"
```

Checks (7):

1. `goal-set` answers success and names the objective.
2. The `session_goal` broadcast carries objective / status / token budget.
3. `goal-get` reports `goal active: …`.
4. **The goal notice reaches the model**: after an idle `goal-set`, the next
   task's transcript must contain `[Operator goal] smoke goal` — asserted by
   the mock provider itself (`expect_transcript_contains`), so a broken
   injection fails loudly.
5. `goal-clear` answers `goal cleared`.
6. The cleared broadcast carries a null goal.
7. Status ops without a goal refuse honestly (`set an objective first`).

Gotchas encoded in the driver:

- An **initial task argument is required**: idle web startups take the daemon
  path, which does not spawn `--control-socket`; with a task the run lands in
  the web-TUI branch whose worker loop is `run_with_presence` — the native
  goal engine's home.
- Thread actions **must carry `session_id`** (the control plane rejects
  untargeted ones); the driver scrapes the id from the `Session ID:` startup
  banner.
- The initial mock task completes before the socket client subscribes, so its
  events are unobservable — all assertions ride request/response pairs or the
  second task.
