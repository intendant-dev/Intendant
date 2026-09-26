# Actual observer calibration (2026-09-26)

## Purpose

Read-only diagnostic mode of the same browser-supervisor executable used by the background-key studies. The user confirmed local typing throughout a prior interval whose HID counters stayed unchanged. That discrepancy remains unexplained; silence in either counter table never establishes user inactivity. This slice does not relax any production input guard or replace HID evidence with session evidence.

## Native mode

`browser-supervisor --calibrate-input-observer SECONDS` accepts a canonical integer from 1 through 30. It validates arguments before observation, emits a bounded JSON ready record, and waits at most 30 seconds for a single `s` byte on its private stdin pipe. It then emits started, raw samples nominally every 100 ms, and a finished record. EOF or `q` cancels collection. Each row is capped at 16 KiB; at most 301 samples are produced. The reader also bounds total bytes, records and runtime and reaps only its owned child.

Both `hid_system` and `combined_session` tables retain raw counters for any input, key-down, key-up, mouse movement and scrolling. Samples carry monotonic start/end timestamps plus process-local listen/post preflight, Accessibility trust, nullable Secure Event Input, and nullable console-user match. Only the calibration process PID is recorded. No key identities, typed contents or foreground-application identity are read. No application or virtual display is launched, no event tap is installed, no input is posted and no permission prompt is requested.

The entry returns before the normal browser path. It uses exactly the existing `activity_sample()` for HID and an explicit second table for comparison. The production witness still samples HID only. Calibration does not create a control capability or prove that every initialization path of the browser mode has identical observation behavior.

## Interpretation and persistence

Collection completion, validation, context availability, per-table regression and per-table keyboard progress are distinct. A complete zero-progress report is not a user-inactivity result or a keyboard-delivery pass. Unknown access stays unknown; numeric 0/1 is not accepted as JSON false/true. Event counters and endpoint samples do not authenticate a human or establish continuous input isolation. User confirmation is separate from machine evidence.

The driver requires macOS, an explicit read-only-observation opt-in, an exact executable SHA-256 and a fresh private report path. It checkpoints bounded evidence during collection. This is not atomic/durable storage across power loss, process death or disk failure. Invalid rows are rejected, not reinterpreted as inactivity.

## Validation checkpoint

Initial native compilation of the supervisor, pure calibration tests and existing activity tests passed with warnings denied. The initial pure native tests passed 59 assertions; existing activity tests passed 28 cases without counter sampling or input. The original 18 Python tests passed on the Mac. A one-second development smoke then correctly failed validation with `preflight types` and reaped the observer.

That smoke exposed a NEW calibration serialization defect: Carbon Boolean and C comparison expressions boxed with `@()` became JSON numbers. The calibration now uses an explicit true/false boxing helper, with a pure native regression for BOOL, Carbon Boolean and comparison inputs. This is not a diagnosis of the earlier #963 zero-counter discrepancy.

The corrected native build and short smoke have now passed on `008fcda0`: 72 calibration assertions, 28 existing activity cases, 20 calibration Python tests, and 206 Python tests across all 13 macOS suites. The full local Rust battery passed (6,691 binary tests, 1,022 library tests, 57 E2E tests, Clippy/fmt/whitespace). Existing ignores remain unchanged. The one-second native run collected 11 samples with four key-downs and five key-ups observed in each table, without counter regression or changing access context. No user typing was requested or confirmed, so this is an operational-protocol result, not a diagnosis of the historical discrepancy. The observer was reaped.

## Verified execution snapshot

An available review found that hashing the original build pathname before launching it allowed a concurrent rebuild to substitute different bytes. The runner now reads a bounded regular executable, hashes those bytes, creates a private verified executable copy, and launches that copy. The private directory remains owned by the runner until collection and observer cleanup finish; report metadata explicitly records `supervisor_execution: private_verified_copy`. No pathname from a mutable build cache is reopened for execution after verification.

This is protection against accidental build replacement, not a sandbox against a same-user or root attacker. The executable copy is a new path; the observer always records its own access checks rather than assuming the source path's permissions carry over. Calibration still uses the actual supervisor code and unchanged HID sampling function.

Seven new hermetic tests cover path replacement, in-place rebuild, private modes and cleanup, wrong hashes/non-executables, bounded size, actual main-to-collector wiring and provenance, and preservation of an existing report. The calibration Python suite now has 27 tests. A development smoke of the corrected launcher completed with 11 samples and unchanged access context; both tables recorded zero progress. That is not evidence of user inactivity. Its private executable and observer were cleaned up.

## Remaining experiment

Coordinate a longer interval of confirmed typing only after the exact published runner is validated. Collection success does not by itself prove observation of a particular person's input. Positive background-key acceptance remains in #963. Optional review availability is not a gate. Production Rust, application input code, permissions, and the installed daemon remain unchanged by this calibration slice.

## API sources

Apple documents the hardware and combined-session state tables separately: https://developer.apple.com/documentation/coregraphics/cgeventsourcestateid . The installed SDK declares `CGEventSourceCounterForEventType` and the nonprompting preflight calls. Neither table by itself establishes authenticated human origin.
