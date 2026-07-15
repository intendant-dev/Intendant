# Computer Use live E2E findings — 2026-07-13

## Executive result

**Verdict: partial pass with one high-priority core-input defect and one high-priority readiness/reporting defect.**

The live Computer Use path successfully captured and inspected the screen, navigated Safari, pasted text, clicked a native button, scrolled to a verified target, closed the disposable tabs, and restored the user's original tab. Direct text injection through `cu type` was not reliable: it returned `ok` while delivering either no characters or only a suffix. Permission state was also misleading because the Intendant display grant was active while macOS Screen Recording and Accessibility permissions still prevented CU operations.

The run was observed live through Intendant's shared view. No form was submitted, the original page was restored, the shared view was hidden, display-control access was revoked, and the disposable local fixture was deleted.

## Run context

| Field | Value |
|---|---|
| Date/time | 2026-07-13, approximately 19:45–19:52 EDT for the interaction sequence |
| Host | macOS user session |
| Browser | Safari |
| Display | Display 0, Primary Display, 1800 × 975 |
| Authority | Explicit user display-control grant; revoked after cleanup |
| Runtime version | User confirmed they were on the latest binary |
| Exact binary revision | Not captured: `intendant --version` is not supported |
| Design worktree reference | `agent/cu-design-reconcile` at `ebb2fa04`; this is a source reference, not independently proven binary provenance |
| Intendant session | `82bf2292-10f8-46c0-8e65-a79846f5aa81` |

## Coverage and outcome

| Capability | Result | Observation |
|---|---:|---|
| Display grant | Conditional pass | Daemon grant worked, but did not describe missing OS permissions. |
| Screenshot capture | Pass after permission fix | Failed generically before Screen Recording permission was available. |
| Accessibility inspection (`read_screen`) | Pass after permission fix | Correctly reported the missing Accessibility permission initially. |
| App activation | Pass with expected edge | First click on inactive Safari activated it; the second performed the intended action. |
| Safari navigation | Pass with workaround | Reliable through paste; direct `type` into the address bar was unreliable. |
| Text-field focus | Pass | AX confirmed the test field was focused. |
| Direct text injection (`type`) | **Fail** | Returned `ok` despite missing or partial text. |
| Clipboard paste | Pass with residue | Delivered exact text but may leave the supplied text in the system clipboard. |
| Native click | Pass | Button changed to its expected green/disabled state. |
| Relative scroll | Pass with iteration | One amount-8 scroll was partial; a second reached the target. |
| Post-action visual verification | Pass with marker interference | State was visible, but large red click markers obscured controls. |
| Keyboard shortcuts | Pass when Safari was active | `cmd+w` closed the two disposable tabs. |
| Original-page restoration | Pass | The original tab briefly reloaded and then rendered normally. |
| Shared-view cleanup | Pass after user caught stale annotation | Explicit `shared hide` was required. |
| Access cleanup | Pass | Display 0 user access was revoked. |

## Confirmed product defects and design gaps

### CU-01 — `cu type` falsely reports success and is unreliable in Safari

- **Priority:** P1
- **Area:** Computer Use input injection
- **Observed:** With AX-confirmed focus and an 800 ms settle, `type` returned `ok` but entered no characters. An earlier attempt entered only the suffix `ant CU ✓`. Direct typing into Safari's address bar was likewise unreliable.
- **Control:** `paste` worked on the same address bar and input field.
- **Why it matters:** This is both delivery failure and false-positive reporting; downstream automation cannot distinguish success from silent text loss.
- **Acceptance criteria:**
  1. A focused Safari text field and address bar receive an exact mixed ASCII/Unicode test phrase on repeated runs.
  2. Partial or missing delivery does not return an unqualified `ok`.
  3. Tests cover an already-focused field, an immediately clicked field, and the Safari address bar.
- **Evidence:** `cu-showcase-click-type.png`, `cu-showcase-typed-retry.png`, and `cu-showcase-pasted.png`.

### CU-02 — Display grant does not represent actual CU readiness

- **Priority:** P1
- **Area:** Authority and macOS permission readiness
- **Observed:** `display request` returned `already_granted` while macOS Screen Recording and Accessibility permissions still prevented screenshots and AX inspection.
- **Why it matters:** A valid Intendant authority grant and usable OS capture/input permissions are separate states, but the operator receives no unified readiness result.
- **Acceptance criteria:** Readiness/status exposes at least these states independently: Intendant display authority, Screen Recording, Accessibility, target availability, and input backend availability. Missing layers are identified before an action starts.

### CU-03 — Action results conflate event injection with semantic success

- **Priority:** P2
- **Area:** CU result semantics
- **Observed:** Key and click actions returned `ok` even when Activity Monitor still owned focus or when the first click merely activated Safari. `cmd+t` and `cmd+l` therefore had no intended Safari effect despite successful injection.
- **Why it matters:** The result reads as task success even though it only confirms event dispatch.
- **Acceptance criteria:** Results use an explicit state such as `injected`, with optional postcondition verification; UI and documentation consistently distinguish dispatch from observed application effect.

### CU-04 — Screenshot permission failure is not actionable

- **Priority:** P2
- **Area:** macOS permission diagnostics
- **Observed:** Screen Recording denial surfaced only as `screencapture failed: could not create image from display`. The Accessibility failure correctly named its missing permission.
- **Acceptance criteria:** Capture denial identifies Screen Recording as the likely missing permission, names the affected process/binary where possible, and gives the relevant System Settings destination.

### CU-05 — Shared-focus annotation outlives its relevant content

- **Priority:** P2
- **Area:** Shared-view collaboration overlay
- **Observed:** The user still saw “Watch the input and button highlight.” after the demo tab was closed and Safari returned to the original page. The annotation was display-scoped and remained until `shared hide` was explicitly issued.
- **Immediate operator cause:** Cleanup had recorded its final note but had not yet executed `shared hide` when the user noticed it.
- **Product/design gap:** Navigation, tab closure, and disappearance of the annotated content do not invalidate page-specific guidance; there is also no dedicated `shared focus clear` command.
- **Acceptance criteria:** Provide an idempotent explicit clear operation. Focus annotations should also clear when the shared view hides, authority is revoked, the owning run ends, or a target-bound annotation becomes invalid.

### CU-06 — Click visualization obscures the state being verified

- **Priority:** P2
- **Area:** CU visual feedback and screenshots
- **Observed:** Large red crosshairs remained in post-action screenshots, sometimes with multiple markers, and covered button labels or changed state.
- **Acceptance criteria:** Markers are smaller and transient, or verification captures can omit them. The clicked control's resulting visual state must remain legible.

### CU-07 — `read_screen` output can be dominated by long URLs

- **Priority:** P2
- **Area:** AX serialization and context efficiency
- **Observed:** A full data URL appeared repeatedly in the frontmost application/window title and values before output truncation, consuming most of the result.
- **Acceptance criteria:** URL-like titles and values are length-capped once with a stable ellipsis or hash. Full values remain available through an explicit detail request.

### CU-08 — Paste can leave clipboard residue

- **Priority:** P2
- **Area:** CU paste semantics
- **Observed:** Paste supplied demo text and later the disposable local fixture URL through the system clipboard. The pre-run clipboard was not captured, so it could not safely be restored during cleanup.
- **Why it matters:** A successful CU action can unexpectedly mutate unrelated user state.
- **Acceptance criteria:** Preserve and restore clipboard contents where supported, or explicitly document/warn that paste is destructive and provide a deliberate non-restoring mode.

## Evidence and reproducibility gaps

### EV-01 — Session notes are not a discoverable findings deliverable

- **Priority:** P2 for operator/developer workflow
- **Observed:** `intendant ctl session note` returns a note ID and session ID and persists entries for dashboard replay, but `intendant ctl session --help` exposes no list/read/show command. Note IDs are not returned with a direct dashboard deep link.
- **User impact:** The user reasonably could not tell where the E2E findings were or how to retrieve them.
- **Acceptance criteria:** Posting a note returns a direct dashboard URL; the CLI supports listing/showing notes by session and note ID; documentation states the dashboard path.
- **Process correction for this run:** This Markdown file is the primary findings artifact. Session notes are only supporting replay evidence.

### EV-02 — Exact running-binary provenance was not capturable through the CLI

- **Priority:** P3
- **Observed:** `intendant --version` fails with `Unknown CLI flag: --version`.
- **Impact:** “Latest binary” cannot be tied independently to an exact revision in the E2E report.
- **Acceptance criteria:** `intendant --version` or `intendant ctl status` emits a stable version, commit, build timestamp, and target triple without exposing credentials.

## Expected operating edge cases

These were real observations, but they are not classified as product defects by themselves.

| Edge case | Observation | Recommended handling |
|---|---|---|
| Inactive-app activation | The first click on inactive Safari only activated it. | Activate explicitly, wait, then perform and verify the intended action. |
| Focus settling | Immediate typing was worse, although an 800 ms settle did not cure CU-01. | Confirm the frontmost app and editable AX focus, then use a bounded settle. |
| Relative scrolling | One `scroll amount 8` moved only partway through the 1,478 px page. | Treat scroll amount as relative and verify after each step. |
| Asynchronous navigation | The restored original tab was temporarily blank while reloading, then rendered after roughly 2.5 seconds. | Wait for a stable frame or load condition before declaring restoration complete. |
| Injection versus state | Successfully injected input does not prove the application changed as intended. | Pair consequential actions with screenshot or AX verification. |

## Fixture-only issues — not Intendant defects

| Issue | Cause | Evidence/classification |
|---|---|---|
| Broken first data-URL handler and quote loss | JSON/HTML embedded inside shell single quotes lost apostrophe quoting. | Demo construction error. |
| `Â·` rendered instead of `·` | The disposable local HTML omitted `<meta charset="utf-8">`. | Fixture encoding error. |

## Evidence index

The complete local frame sequence produced during the run is listed below. These `/tmp` copies are ephemeral; the selected frames marked “attached” were also stored in Intendant session-note attachments.

| Frame | What it shows | Durable attachment |
|---|---|---:|
| `/tmp/cu-showcase-start.png` | Original pre-demo screen | No |
| `/tmp/cu-showcase-newtab2.png` | New Safari tab after activation/click sequencing | No |
| `/tmp/cu-showcase-demo-loaded.png` | First data-URL fixture | No |
| `/tmp/cu-showcase-click-type.png` | Partial suffix delivered by `type`; broken fixture handler | No |
| `/tmp/cu-showcase-typed-retry.png` | Focused field after `type` reported success but inserted nothing | Yes |
| `/tmp/cu-showcase-pasted.png` | Same field populated successfully through paste | Yes |
| `/tmp/cu-showcase-local-loaded.png` | Correct local fixture loaded | No |
| `/tmp/cu-showcase-local-interacted.png` | Successful paste and green/disabled button state | Yes |
| `/tmp/cu-showcase-scrolled.png` | Intermediate relative-scroll position | No |
| `/tmp/cu-showcase-scroll-target.png` | Verified bottom scroll target | Yes |
| `/tmp/cu-showcase-cleanup.png` | Original tab during asynchronous reload | No |
| `/tmp/cu-showcase-restored.png` | Original tab fully restored | Yes |

### Supporting Intendant replay notes

Open the Intendant dashboard, select **Sessions** in the left rail, open session `82bf2292-10f8-46c0-8e65-a79846f5aa81`, and view its detail/replay log. The entries use source label **CU showcase**. If needed, use **Deep Search** for `CU showcase` or a note ID.

| Note ID | Contents |
|---|---|
| `note-1566113c46db47009db36d5c34c53856` | `type` failure and successful paste, with two screenshots |
| `note-0eb867598979438d8f6249147532663a` | Successful click/scroll plus marker and URL-bloat findings, with two screenshots |
| `note-1c60ba8e7dd64122b016977ea2eea6a2` | Aggregate result and restored-screen evidence |
| `note-1913729809fd42e2b517e6fb3999dc32` | Stale shared-focus annotation reported by the user |
| `note-498d53401d43405eb6cd4f281f5fe53b` | Verified cleanup state |

## Cleanup state

- Original Safari tab restored; no demo form submission occurred.
- Two disposable tabs closed.
- Shared view hidden and stale focus annotation cleared.
- Display 0 user-session authority revoked.
- Disposable `.cu-showcase.html` fixture deleted and absence verified.
- Possible remaining user-state residue: the system clipboard may contain the now-deleted fixture's local URL because the prior clipboard was not captured.

## Recommended triage order

1. Fix and regression-test exact text delivery plus result reporting for `cu type` (CU-01).
2. Add a unified CU readiness preflight that distinguishes Intendant authority from OS permissions (CU-02/CU-04).
3. Make result semantics explicit about injected versus verified effects (CU-03).
4. Make shared annotations lifecycle-safe and explicitly clearable (CU-05).
5. Remove verification interference and context bloat (CU-06/CU-07).
6. Define non-destructive clipboard semantics for paste (CU-08).
7. Improve evidence retrieval and build provenance for future E2E runs (EV-01/EV-02).

## Verification and disposition addendum — 2026-07-15

Every finding above was triaged into the 2026-07-13/14 CU program (PRs #302–#307) and re-verified live against a release build of merged `main` (`b9169990`) on a macOS user session on 2026-07-14, following `tests/skills/cu-type-injection/SKILL.md`.

| Finding | Disposition |
|---|---|
| CU-01 `type` unreliable / false ok | **Fixed and live-verified 9/9** (#303). Root cause: 20-UTF-16-unit `CGEventKeyboardSetUnicodeString` chunks posted back-to-back with mispaired keyUps — the observed `ant CU ✓` suffix was exactly chunk 2 of 2. All three skill legs (already-focused field, zero-wait click→type batch, Safari address bar) passed 3/3 repeats with AX read-back confirmation of the full phrase. |
| CU-02 grant ≠ readiness | **Fixed** (#305): five independent readiness layers via the `display_readiness` tool / `ctl display status`, plus `os_readiness` gap blocks on grant answers. Live-verified against real TCC state. |
| CU-03 injection conflated with effect | **Fixed** (#303): `ok` / `injected` / `failed` result vocabulary, with AX read-back where the platform allows. Proven in live use — misdirected batches surfaced as `injected` rather than false `ok`. |
| CU-04 capture failure not actionable | **Fixed** (#305): preflight-confirmed denials name Screen Recording, the affected binary, and the System Settings destination. Denial path is unit-tested; not exercised live (the test host holds the grant). |
| CU-05 stale shared-focus annotation | **Fixed** (#302): idempotent `shared focus clear`, plus auto-clear on view hide, display revocation, and owning-session end. |
| CU-06 markers obscure verified state | **Fixed** (#304 + #306): model-facing screenshots are clean by default (`annotate` is opt-in); live visual feedback moved to transient dashboard overlays (cursor halo, click ripples). |
| CU-07 URL bloat in `read_screen` | **Fixed** (#305): 80-char value/title cap with a stable `… [N chars total, #hash]` marker and a `full_values` escape hatch. Live-verified on a 532-char URL. |
| CU-08 clipboard residue | **Fixed** (#303): clipboard save/restore around paste on macOS and Windows; X11/Wayland state their no-restore semantics in the result. Live-verified by sentinel round-trip. |
| EV-01 notes not discoverable | **Deferred by design — not built.** Session notes (landed 2026-07-09) are session-log-plane replay evidence anchored to one transcript. The workflow EV-01 asks for — an owner retrieving agent-left findings later, across sessions — belongs to the planned owner-plane attention/agenda surface, and a notes-browsing CLI now would duplicate that design. Durable findings remain repo files (this document is the pattern). |
| EV-02 no version provenance | **Fixed** (#302): `--version`/`-V` on both binaries (version, commit, build timestamp, target triple) and `daemon_version` in `ctl status`. |

The same program also landed the architecture-comparison items that sat behind these findings: `observe: pixels|ax|auto|none` on `execute_cu_actions`, lazy single-encode screenshots, bounded `settle` quiescence reporting, and per-batch CU metrics (#306, #307).
