# Fleet CI host kit

Versioned host-side configuration for the self-hosted runner fleet.
Everything a runner box needs beyond the GitHub Actions listener itself
lives here, so host state is auditable and reinstallable instead of
snowflake config. **The repo is public: host-specific values (account
names, LAN addresses, listener labels) belong in `/etc/intendant-ci/`
on the host, never in these files.**

## fleet-watchdog

The in-job disk preflight (windows.yml / smokes.yml) fails a job
*after* the queue assigned it — a full disk burns whole speculative
merge-queue entries (seven macOS validations died at 18G free on
2026-07-10). The watchdog acts *before* assignment, as root, every
5 minutes:

- **Pause/resume with hysteresis.** Below `STOP_GB` (default 50) it
  stops the runner listeners — deferring while a job is mid-flight
  unless free space falls below `HARD_STOP_GB` (25). It resumes them
  only at `RESUME_GB` (75), so it never flaps at a threshold.
- **Honest measurement on macOS.** Local APFS snapshots make `df`
  swing tens of GB with zero real deletions; the watchdog thins them
  (`tmutil thinlocalsnapshots`) before trusting a low reading. This is
  also what keeps the in-job preflight (which stays, as the last-resort
  backstop) from tripping on purgeable-space noise.
- **Owns the external cargo target caches.** The workflows' "External
  cargo target dir" steps build under
  `~/.cache/intendant-ci/target/<listener>-<toolchain>` with a
  `.last-used` marker. The watchdog prunes keys unused for
  `PRUNE_DAYS` (7) and evicts oldest-first over `CAP_GB` per root —
  and, under disk pressure, until free space clears the resume
  ceiling. It deletes **only** inside the configured cache roots.

### Install

```bash
# macOS (LaunchDaemon, tick 300s):
sudo scripts/ci/install-watchdog-macos.sh <runner-account>

# Linux (systemd timer, tick 5min):
sudo scripts/ci/install-watchdog-linux.sh <runner-account>
```

The installer seeds `/etc/intendant-ci/watchdog.conf` from
`watchdog.conf.example`, auto-detecting the account's listener
LaunchAgents / systemd units — review the seeded file. Re-running an
installer upgrades the script and daemon but never overwrites an
existing conf.

Verify: `tail -f /var/log/intendant-ci-watchdog.log` (one line per
action; silent ticks log nothing).

Rollback: each installer prints its one-line rollback command.

### Windows

Deferred: the Windows runner host needs the equivalent (scheduled task,
`LOCALAPPDATA\intendant-ci\target` cache root, 60G budget) once the box
is reachable for verification — tracked in the CI hardening program.

## Interlocks

- The workflow cache steps and this watchdog share the cache layout
  contract: per-listener key dirs with a `.last-used` marker. Change
  one, change both (windows.yml / smokes.yml "External cargo target
  dir" steps ↔ `fleet-watchdog.sh` prune/evict).
- The in-job preflight floor (20G) is deliberately BELOW `STOP_GB`:
  the watchdog should pause listeners long before any job can see a
  sub-floor disk, leaving the preflight as the backstop for the
  watchdog being dead or misconfigured.
