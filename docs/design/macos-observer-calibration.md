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

The corrected native compile-and-smoke command was blocked twice before execution by the OpenAI tool layer. No alternate execution route was attempted. The corrected native binary and added native assertions have NOT been run. The expanded 20 Python tests pass in the conversation Linux container; source hashes match the Mac checkout. The full Rust battery has not been repeated for this draft checkpoint. Production Rust, application input code and permissions are unchanged.

## Resume gates

First build the corrected supervisor and native tests with the existing warnings-denied flags, verify their exact source/binary provenance, and run a fresh short read-only smoke. Only then coordinate a longer interval of confirmed typing; do not silently count an idle smoke as calibration of human input. Positive background-key acceptance remains in #963. Optional review quota is not a gate; this PR remains draft for unfinished native validation.

## API sources

Apple documents the hardware and combined-session state tables separately: https://developer.apple.com/documentation/coregraphics/cgeventsourcestateid . The installed SDK declares `CGEventSourceCounterForEventType` and the nonprompting preflight calls. Neither table by itself establishes authenticated human origin.
