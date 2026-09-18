# Handover lock release (2026-09-18)

## Observed failures

PR #936 did not change handover. Linux PR run 35342979472 failed
`successor_port_never_names_a_draining_or_dead_target` at `b.poll_acquire(0)`.
Its retry passed. Merge-group run 35343852948 then failed
`dropped_runtime_releases_lease_and_liveness` at `!boot_id_is_live(...)`
on macbook-b. The Mac failure was a completed assertion, not a lost runner,
OOM report, or compiler failure. Linux and Windows passed that merge group.

## Reproduced mechanism

Both SchedulerLease and DaemonPresence relied on File close for release.
Unix flock remains held while any duplicate of its open file description
survives. Concurrent fork-to-exec activity can extend that lifetime even with
CLOEXEC set. Closing the Rust owner's descriptor alone is insufficient.

An isolated fork/pipe experiment on the plugin Mac reproduced parent-close
leaving the lock held by the child; explicit unlock released it immediately.
Two timing-free Rust tests retain a File::try_clone while dropping the actual
production owners. Both failed before the production patch. A duplicate tests
the shared open-description mechanism without unsafe fork in the test suite.

The original CI log does not identify the particular child or capture its
descriptor table. The mechanism and production defect are reproduced; the
exact original CI interleaving is inferred, not traced.

Primary contracts:
- Rust: https://doc.rust-lang.org/std/fs/struct.File.html#method.unlock
- Apple: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/flock.2.html
- Linux: https://man7.org/linux/man-pages/man2/flock.2.html

## Correction

Both non-cloneable guards explicitly unlock in Drop in their creating process.
Their in-memory PID is minted locally, never read back from a sidecar.
A copied guard in a fork child must not unlock its parent's live ownership.
Unlock errors are logged without panicking; File close remains the fallback.
Scheduler acquisition installs its guard before the fallible sidecar write.

Abrupt termination still uses OS descriptor cleanup and can remain locked
until an inherited copy closes. This patch does not replace lock authority
with PID or time heuristics; uncertain liveness probes remain conservative.

Four regressions cover release with duplicates alive, live-owner exclusion,
successor generation, stale-duplicate close, and copied-guard PID protection.
The original failing tests are unchanged. No CI concurrency, test timeout,
retry policy, ignore list, runner service, cache, or compiler governor changes.

## Validation

Pre-fix: the two new release tests failed (0 passed, 2 failed).
Post-fix results and landing status are recorded in PR #937.
Land this independent correction before re-queueing the unchanged #936.
