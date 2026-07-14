# CU type-injection live regression (macOS)

Live proof that `cu type` **delivers every character** and **reports
honestly** on the macOS user-session display. Guards the 2026-07-13
defect class: `CGEventKeyboardSetUnicodeString` chunks silently dropped
by the target app (observed live: the 28-unit phrase below arrived as
only its post-chunk-boundary suffix `ant CU ✓`, and later runs delivered
nothing at all) while the action still returned an unqualified ok.

The hermetic twins live in `computer_use.rs` unit tests (keystroke
planning, chunk boundaries, read-back verdicts, status mapping). This
smoke covers what those can't: real CGEvent delivery into real apps,
the AX read-back against a live field, and Safari's address bar.

Not in CI because it injects input into a real user graphical session
(never run it on a machine someone is actively using) and needs the
Accessibility + Screen Recording permissions.

## Prerequisites

- macOS box with a live GUI session you are allowed to drive.
- A controller built from **your own worktree**, with Accessibility
  and Screen Recording granted to the invoking process tree.
- A daemon from that build (`./target/release/intendant --no-web` in a
  scratch session, or drive a running daemon with `intendant ctl`).
- The user-session display grant (`grant_user_display`, or an owner
  surface).

## The canonical phrase

Every leg types exactly:

```
Typed through Intendant CU ✓
```

28 UTF-16 units: 27 ASCII (keycode path) + `✓` U+2713 (unicode-event
path). Under the old 20-unit chunking the failure signature was
delivery of only `ant CU ✓` — if you ever see that suffix alone, the
first-chunk drop is back.

## The legs

Open TextEdit (or any AX-readable text field) plus Safari. Then, via
MCP `execute_cu_actions` or `intendant ctl cu actions`, run each leg
**three times** — the defect was intermittent:

1. **Already-focused field**: click the TextEdit document once
   manually, then run a lone `type` action with the phrase.
2. **Immediately-clicked field**: one batch of
   `click(field) → type(phrase)` with no wait between them.
3. **Safari address bar**: batch `key(cmd+l) → type(phrase)` with
   Safari frontmost.

## Expected honest statuses

- Every character of the phrase visible at the target, in order,
  including the `✓`, on **every** repeat.
- Legs 1–2 (AX-readable value): `type` reports `ok` with read-back
  detail. Corrupt the run deliberately (e.g. click a non-text area
  first) and the result must be `failed` with expected-vs-observed
  text — never an unqualified ok.
- Leg 3: `ok` when Safari exposes the field value via AX, otherwise
  `injected` with a read-back-unavailable note. Never `ok` without
  qualification when the bar stayed empty.
- Any other input action (`click`, `key`) reports `injected`, not
  `ok` — dispatch confirmed, effect unverified.

## Paste-residue leg (CU-08)

With a known text on the clipboard (`echo -n sentinel | pbcopy`):

1. `paste("residue-test ✓")` into the TextEdit field.
2. The pasted text must appear; the result detail must say the
   previous clipboard text was restored.
3. `pbpaste` afterwards must print `sentinel` again.
4. Repeat with a non-text clipboard (copy an image in Preview): the
   detail must say the previous content could not be captured/restored
   and the clipboard must be left cleared, not holding the paste
   payload.
