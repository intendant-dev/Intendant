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
  ceiling. Cache maintenance defers while any runner job is active and
  rechecks immediately before each deletion, so rustc never loses its
  target directory mid-build. It deletes **only** inside the configured
  cache roots.

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

## Cargo parallelism cap (runner accounts)

A box that hosts two listeners plus interactive agents cannot run
uncapped rustc: concurrent test-binary links exceed physical RAM, the
box swaps, macOS's `kernel_task` thermal/IO throttling then slows the
CPU, the links crawl instead of finishing, more jobs stack behind
them, and the kernel finally OOM-kills a compile (`sccache: Compile
terminated by signal 9`) — failing whole speculative merge-queue
entries (2026-07-10: three hours of queue stall on the 24GB Mac).

Cap cargo at the account level in the runner account's
`~/.cargo/config.toml`:

```toml
[build]
jobs = 6   # 2 listeners x 6 jobs fits a 24GB box; scale with RAM
```

This binds every cargo invocation in that account — CI legs and local
agents alike — without touching workflow files or restarting
listeners. Pair it with the merge queue's build concurrency (ruleset;
2 as of 2026-07-10): the queue bounds how many entries validate at
once, the jobs cap bounds what each validation can demand.

The watchdog also gates assignment on **memory pressure** (macOS
`kern.memorystatus_vm_pressure_level`, Linux PSI `some avg10`):
sustained pressure (`MEM_PAUSE_TICKS` consecutive ticks) pauses the
listeners, sustained normal resumes them — with the disk and memory
pauses tracked as separate markers, so listeners return only when BOTH
clear. It never pauses mid-job on memory alone and it cannot constrain
builds already running: bounding running compiles is the governor's
job (next section); this is purely a new-assignment circuit breaker.
Probes fail open — a box whose pressure interface is unreadable keeps
assigning rather than wedging.

The jobs cap is per-*process*; bounding what all cargo processes on the
box can demand **together** is the governor's job — next section.

## Governor: machine-wide rustc concurrency (`rustc-governor`)

The jobs cap cannot stop several concurrent cargo processes (two CI
listeners plus interactive agents) from each spawning their capped six:
concurrent cold builds still oversubscribe RAM, and the link storm →
swap → `kernel_task` throttle spiral follows. `crates/rustc-governor`
adds the missing machine-wide bound: a compile-permit pool shared by
every account on the box.

### The chain

```
cargo ──[build] rustc-wrapper=governor──▶ rustc-governor <real rustc> <args…>
                    │  probe (-vV / pure --print):
                    │    exec(2) real rustc directly — no permit, no sccache
                    ▼  compile: acquire flock(2) permit
              governor (HOLDS the permit, waits)
                    │  spawns; the permit fd keeps FD_CLOEXEC —
                    ▼  no child ever sees it
              sccache <real rustc> <args…>   (the blocking sccache CLIENT)
                    ▼
              sccache server ── hit: answer from cache (client exits in
                    │                ~tens of ms, permit released)
                    ▼  miss / non-cacheable
              real rustc runs; the client — and so the waiting governor,
              and so the permit — blocks until it finishes
```

The governor is cargo's `rustc-wrapper`; sccache is no longer the
wrapper — the governor runs it as its child, prepending the real
compiler path cargo handed it as argv[1] (the `wrap_with` config key
names the sccache binary; unset it and the compiler runs directly,
governed but uncached):

- **The ceiling holds transitively.** The permit is held by the
  governor, whose spawned sccache *client* blocks until the server
  answers: at most N outstanding clients ⇒ at most N server-side
  compiles, no matter which rustc binary the server resolves and runs.
- **Probes never wait and never touch sccache** — `-vV` / `--version` /
  pure `--print` queries (cargo fires them at every startup) exec the
  real compiler directly: snappy under a full pool, and cargo startup no
  longer depends on a healthy sccache server (a real incident class). A
  real compile that merely *carries* `--print` (e.g.
  `--print native-static-libs` with `--emit link`) is still governed.
- **Hits hold a permit briefly.** A cache hit occupies its permit for
  the client round-trip (~tens of ms); under a miss-saturated pool, hits
  queue head-of-line behind running compiles. Accepted trade, decided
  with the operator: ceiling correctness over warm-path latency.
- **Parent-held permits (the 2026-07-12 leak post-mortem).** The
  governor originally exec(2)'d the chain over a FD_CLOEXEC-cleared
  permit fd so the flock rode into the sccache client — but when no
  server is running, the client *daemonizes* one, and that long-lived
  server (ppid 1) inherited the fd: flock belongs to the open file
  description, so the permit stayed held long after the client exited
  (verified live — a 3-permit pool ran as 2 for hours while a local
  server held a borrowed CI permit on fd 9). Now the governor stays
  alive as the chain's parent: the permit fd keeps FD_CLOEXEC and is
  invisible to every child, the child's exit code is propagated, signal
  death is re-raised so cargo observes the same disposition, and
  TERM/INT/HUP are forwarded to the child while the governor waits.
  Crash semantics, accepted: SIGKILL on the governor releases the
  permit in the kernel the instant its fds close; the orphaned child
  finishes its current compile momentarily ungoverned. Regression:
  `daemonized_server_does_not_inherit_the_permit` in
  `crates/rustc-governor/tests/sccache_chain.rs`.

#### Why wrapper-side (the 2026-07 bypass post-mortem)

The governor originally sat on the compiler side of sccache
(`[build] rustc = governor`, `rustc-wrapper = sccache`) so cache hits
never executed it — and that design was silently bypassed for exactly
the work it existed to bound. sccache 0.15 identifies rustup proxies by
probing the compiler with `+stable -vV`; the governor's probe fast path
passed the probe through to `$HOME/.cargo/bin/rustc` — the rustup proxy,
which accepts `+toolchain` selectors where a real rustc errors — so
sccache classified the governor as a rustup proxy, resolved the
underlying toolchain rustc itself, and had its server invoke that binary
directly for every cacheable miss: ungoverned (verified live — five
toolchain rustcs as sccache-server children while all permits were
held). Only non-cacheable invocations (bin links etc., which the client
runs locally) stayed governed. Wrapper-side, nothing reaches sccache
without a permit, so no compiler-identification cleverness can route
around the pool. The regression test that would have caught it —
`crates/rustc-governor/tests/sccache_chain.rs` — drives the real sccache
binary and asserts a *cacheable* rlib miss queues behind a held permit
(bin/link and metadata-emit shapes are non-cacheable and silently test
the wrong path).

### Permits and the demand gate

`permit_dir` holds one flock file per permit, split into per-class
reservations — `permit-local-<i>` for interactive accounts,
`permit-ci-<i>` for the accounts in `ci_users` — plus one demand file
per class (`demand-local` / `demand-ci`). Holding LOCK_EX on a permit
file IS the permit. Waiters hold LOCK_SH on their own class's demand
file for the whole wait and poll every eligible permit at 100ms
(never a blocking flock — it has no timeout and would bypass the gate).
Borrowing: before touching a foreign-class permit, probe that class's
demand file with LOCK_EX|LOCK_NB — success (released immediately)
means no waiters, so idle capacity is never wasted; failure means the
class has waiters and its reservation is honored. Nothing is ever
killed or signalled; borrowed permits return naturally when their
holder exits.

### Fail-open doctrine + live kill switch

A governor must never break a build — and, since the wrapper-chain flip,
must never cost a build its caching either. Missing or unparseable
config, `enabled = false`, `INTENDANT_GOVERNOR=off` in the env, an
unusable permit dir, zero configured permits — every degraded state
means "run ungoverned", never "block", and still execs the `wrap_with`
chain when it is configured (a disabled governor is indistinguishable
from a plain sccache rustc-wrapper; only a missing/unparseable config,
which cannot know `wrap_with`, runs the compiler directly). The config
is re-read by every invocation (and once per poll tick by in-flight
waiters), so `/usr/local/etc/intendant-governor.toml` is live: flipping
`enabled = false` drains the governor within ~100ms, no listener
restarts. Observability: one line per *governed* invocation in
`<permit_dir>/governor.log` (timestamp, pid, class, permit, wait_ms;
truncate-in-place rotation at 1MB keeping the last 256K — the hooks-log
doctrine, because governed accounts can write the pre-created file but
not create siblings in the root-owned dir).

### Sizing, install, rollout

Per box: `local_reserved + ci_reserved` = the machine-wide ceiling of
concurrent rustc processes; the per-account jobs cap still bounds any
single cargo underneath it. The 24GB two-listener Mac runs 1 + 2.
Don't set a class to 0 unless it truly never compiles — its members
would then depend entirely on borrowing.

```bash
cargo build --release -p rustc-governor
sudo scripts/ci/install-governor-macos.sh   # binary + permit dir + conf
```

The installer never edits account cargo configs; it prints the
`[build] rustc-wrapper = ".../rustc-governor"` line to set per account
(replacing sccache as the wrapper — the governor execs sccache itself,
via the conf's `wrap_with` key) and the legacy `rustc = ".../rustc-governor"`
line to REMOVE (cargo passes the real compiler as argv[1] now; a
leftover rustc= line would hand the governor itself, which it refuses
with exit 127 rather than exec-looping). The installer never overwrites
an existing conf, so on upgraded boxes the operator adds
`wrap_with = "/opt/homebrew/bin/sccache"` by hand — the installer
prints that reminder when the key is missing.
**Canary order: the CI account first, soak a day of green runs, then
the operator account.** Cache keys: sccache hashes the compiler it is
asked to run — the real rustc again — so enablement and governor
upgrades no longer invalidate the account's sccache cache (the old
governor-as-compiler wiring paid one cold rebuild per governor change;
that cost is gone). Resizing the reservations
upward needs the installer re-run (or a root `touch` + `chmod 0644`)
to mint the new permit files; permit files the governor cannot open
are simply not part of the pool, and if none are usable it fails open.

## macOS CI service account (`_intendant-ci`)

**Why:** CI jobs on the Mac listeners historically executed as the
operator's own account — so any code that lands in a PR (and every
action it pulls in) could read everything the operator can read
(`~/.ssh`, `.env` API keys, gh tokens, browser profiles, session
stores) and inherited the operator's TCC grants (Screen Recording,
Microphone, Accessibility). The Dell and Windows runners already run
as dedicated non-admin `ci` users; this kit brings the Mac to parity.

The kit (all root, all idempotent, everything host-specific detected
at run time — nothing beyond the generic `_intendant-ci` name is
committed):

- **`setup-ci-account-macos.sh`** — creates the hidden role account
  (`sysadminctl -roleAccount`: UID auto-picked in 450–499 — the range
  sysadminctl itself enforces — own primary group, not `admin`, no
  password material, home `/var/ci`; role accounts conventionally live
  outside `/Users`, and an empty non-template home keeps the
  hermeticity signal clean). Two macOS realities, verified live: staff
  membership is *computed* for every local account (not removable —
  the boundary is 700 operator homes + no admin), and sysadminctl
  mints ShadowHashData even with no password argument (the script
  deletes it; verification fails if it ever reappears). Provisions the
  per-user toolchain **as that account**: rustup pinned to the invoking
  host's current `rustc -V` (printed; the workflows key their cargo
  target caches by it), `~/.cargo/config.toml` with the jobs cap above
  (mirrors the operator's value; adds `rustc-wrapper = <sccache>` plus
  a per-account `SCCACHE_SERVER_PORT`/`SCCACHE_DIR` iff sccache exists
  — the client/server rendezvous is one machine-wide TCP port, and the
  operator's server cannot read the CI account's 0750 toolchain).
  wasm-pack at the repo's `.wasm-pack-version` pin (failure is a loud,
  canary-visible gap, not a blocker). Installs the job hooks. Verifies
  and prints: not in admin, no password material, HOME resolves, and
  that the account **cannot traverse any human `/Users/<home>`**
  (expects 700; reports, never chmods — that fix is the operator's
  call).
- **`migrate-runner-macos.sh <listener-name>`** — one listener per
  invocation. Stops the operator-account LaunchAgent, waits for the
  service tree to exit, moves the runner dir into `/var/ci` (the
  `.runner`/`.credentials` registration travels with it — identity and
  name preserved, **no re-registration**), remaps `.path` onto the CI
  home, wires the hooks into `.env`, renders a LaunchDaemon from the
  runner's own `bin/actions.runner.plist.template` (the exact template
  `svc.sh` renders LaunchAgents from — same load-bearing keys:
  `runsvc.sh` ProgramArguments, WorkingDirectory, RunAtLoad, log paths,
  `ACTIONS_RUNNER_SVC=1`, ProcessType Interactive, SessionCreate) with
  `UserName` swapped to `_intendant-ci` and `HOME`/`USER` injected into
  `EnvironmentVariables` (the one deliberate divergence: gui
  LaunchAgents inherit them from the login session, system
  LaunchDaemons get neither, and rustup/cargo need `HOME`), bootstraps
  it in the system domain, waits for the runner to report online
  (`gh api`, when
  available), and rewires `watchdog.conf` (label moves to
  `RUNNER_DAEMON_LABELS`, CI cache root and account are added; the old
  cache root stays listed so the watchdog prunes its stale keys away).
  Prints the rollback invocation on completion. Post-migration the
  runner's `svc.sh` no longer applies (gui-domain only) — control the
  listener via `launchctl … system …` or the watchdog.
- **`rollback-runner-macos.sh <listener-name>`** — exact inverse, from
  the metadata migrate parked under `/etc/intendant-ci/migration/`:
  bootout the daemon, move the dir back, chown, restore
  `.path`/`.env`/`.service`, restore + bootstrap the original
  LaunchAgent, restore the watchdog entries (the CI account and cache
  root drop out once no daemon listener remains).

### Migration runbook

```bash
sudo scripts/ci/setup-ci-account-macos.sh        # account + toolchain + hooks
sudo scripts/ci/migrate-runner-macos.sh <listener-b>   # secondary listener first
# canary (below), soak ≥ a day
sudo scripts/ci/migrate-runner-macos.sh <listener-a>   # primary listener
# final soak; rollback at any point:
sudo scripts/ci/rollback-runner-macos.sh <name>
```

Canary: after migrating the first listener, force a full required-check
run onto it — pause the un-migrated listener for one run
(`launchctl bootout gui/<uid>/<label>`, resume with the matching
`bootstrap`) or simply watch until the migrated listener has executed
each required workflow at least once (`gh run list`, per-job runner
name in the job log).

### Canary expectations (fresh account: zero TCC grants, no Aqua/WindowServer session)

The LaunchDaemon carries `SessionCreate=true`, so jobs get a security
session (securityd works) but no window server and no TCC grants.
Suite grep, 2026-07-10 — the classes that touch macOS machinery, and
what to expect:

| Test class | Expectation as `_intendant-ci` |
|---|---|
| `access/certs.rs::p12_imports_via_real_macos_keychain`, `::p12_imports_via_security_cli_auto_detection` | **PASS — watch these closest.** Real Keychain machinery, but against throwaway *file-backed* keychains in tempdirs (never the login keychain). Needs securityd + a security session, not WindowServer/TCC. A regression here looks like `errSecInteractionNotAllowed` / `security create-keychain` failing. |
| `sandbox.rs` seatbelt tests (3 spawn real `/usr/bin/sandbox-exec`) | PASS — Seatbelt needs no GUI or TCC. |
| `access/cert_server.rs::mobileconfig_profile_is_valid_plist_on_macos` | PASS — spawns `plutil`, headless-safe. |
| `terminal.rs` PTY suite | PASS — `openpty` needs no controlling terminal or GUI. |
| `platform.rs` (process tree/spawn), `vision.rs` (Linux-display logic), `computer_use.rs` (pure key parsing), `audio_routing.rs` / `transcription.rs` / `recording.rs` (pure command construction), `encode/*` (software VP8 + AVCC byte-munging), `clipboard.rs` (struct-only — tests never start the NSPasteboard poller) | PASS — pure logic; no live OS surface. |
| `ax.rs::live_read_frontmost` (AX TCC), `intendant-display::macos_real_capture_stress_cycles` (Screen Recording TCC + display), `live_audio.rs` live-API tests | Already `#[ignore]`d — not in CI on any account; they stay operator-hardware smokes. |
| `tests/e2e` (mock provider, headless) | PASS — hermetic by design; fixtures inject their roots. A test that fails **only** on the CI account because it resolved the real `$HOME` (now an empty `/var/ci`) is an unhermetic-fixture bug to fix (CLAUDE.md, "Tests are hermetic"), not a migration blocker. |

What *would* regress, and deliberately doesn't exist: any non-ignored
test calling WindowServer (CGDisplay/ScreenCaptureKit/NSPasteboard/
CGEvent-post) — the grep found none; new tests must keep it that way.
Runtime capabilities of the box under the CI account are reduced **by
design**: `screencapture`, ScreenCaptureKit, AX, and live audio would
each need TCC grants the fresh account doesn't have — CI never uses
them.

### Job hooks

`hooks/job-started.sh` / `hooks/job-completed.sh` (shared engine
`hooks/hook-lib.sh`) are wired through each runner root's `.env` —
`ACTIONS_RUNNER_HOOK_JOB_STARTED=…` / `…_JOB_COMPLETED=…`, the
documented runner mechanism; the runner reads `.env` at listener
startup, so re-wiring needs a listener restart. Every invocation is
**account-gated** before anything else: `run_hook` refuses (one log
line, exit 0) unless `id -un` matches `INTENDANT_CI_HOOK_ACCOUNT`
from the same `.env` (the migrate script wires it). The rules below
are only safe inside the dedicated CI account — executed as anyone
else they apply "everything here is CI residue" to a real user's
session and daemon state, which is precisely the 2026-07-10 incident.
Work is bounded by a 60s self-timeout — a foreground poll in the
hook's own shell, deliberately not a detached timer subshell, which
bash 3.2 leaked and later fired at recycled pids (GitHub applies no
timeout of its own, and a wedged started-hook would wedge the job):

- wipe `$RUNNER_TEMP` contents (per-runner, recreated every job);
- reap stale (>24h) temp/test-home residue — `$TMPDIR` and
  `~/.intendant` are per-*account*, shared with the other listener's
  live jobs, hence the age gate;
- kill leftover CI-account processes that no live runner tree owns —
  decided by **ancestry**, not age (both listeners share the account);
  the runner service stacks and the shared sccache server are
  protected (killing sccache mid-compile fails the other listener's
  rustc invocations);
- log exactly one summary line to `/var/log/intendant-ci-hooks.log`
  (rotated at 1MB like the watchdog log, truncate-in-place because the
  account can't create files in `/var/log`);
- **always exit 0** — a non-zero started-hook fails the job (GitHub
  semantics), and janitorial trouble must never take down CI.

Never touched: `~/.cache/intendant-ci` (the watchdog owns the warm
target caches), `~/.cargo`, `~/.rustup`.

### sccache: one supervised server per account

The account's `rustc-wrapper` clients rendezvous with ONE server on
the account's port (4227; the operator's server keeps the 4226
default). Never rely on in-job server spawning: cargo's `[env]` port
does not reach every in-job sccache invocation (2026-07-10: the rustc
version probe missed it), and a client racing a dying or job-reaped
server reads a truncated response header — "failed to fill whole
buffer", cargo exit 101 within seconds, every job on the listener red.
Instead `setup-ci-account-macos.sh` installs
`com.intendant.ci.sccache`, a launchd-supervised **foreground** server
(`SCCACHE_NO_DAEMON`: a forked server dies with its launchd process
group; `KeepAlive` revives crashes; idle timeout disabled), and the
migrate script mirrors `SCCACHE_SERVER_PORT`/`SCCACHE_DIR` into each
listener's `.env` so every job process agrees. The Linux runner
accounts still use on-demand servers (a systemd twin of the daemon is
listed under "Deliberately deferred").

### Watchdog interplay

`fleet-watchdog.sh` understands both listener shapes at once —
`RUNNER_LABELS` (gui-domain LaunchAgents, `RUNNER_UID` +
`RUNNER_PLIST_DIR`) and `RUNNER_DAEMON_LABELS` (system-domain
LaunchDaemons, `RUNNER_DAEMON_PLIST_DIR`) — and `RUNNER_USER` may list
several accounts, so a half-migrated host stays fully supervised. The
migrate/rollback scripts edit `watchdog.conf` themselves; the
before-images land in `/etc/intendant-ci/migration/` for audit.

### Deliberately deferred

- **PF / private-LAN egress deny for the CI account** (packet-filter
  rules keyed to `_intendant-ci`'s uid, so PR code can't probe the
  LAN): needs operator sign-off at apply time — not part of this kit.
- **Windows host equivalent** of the hooks + migration ergonomics (the
  Windows runner already runs as a dedicated non-admin user; see the
  watchdog "Windows" note above).
- **sccache cache custody**: the CI account's sccache cache lives at
  `~/.cache/sccache` (pinned by the supervised server's `SCCACHE_DIR`)
  under sccache's default 10G self-cap; the watchdog does not manage
  it yet.
- **Linux twin of the supervised sccache server**: the Dell runner
  accounts still spawn sccache servers on demand; port a systemd unit
  of `com.intendant.ci.sccache` when the Linux hosts get the hooks
  treatment.

## Interlocks

- The workflow cache steps and this watchdog share the cache layout
  contract: per-listener key dirs with a `.last-used` marker. Change
  one, change both (windows.yml / smokes.yml "External cargo target
  dir" steps ↔ `fleet-watchdog.sh` prune/evict).
- The in-job preflight floor (20G) is deliberately BELOW `STOP_GB`:
  the watchdog should pause listeners long before any job can see a
  sub-floor disk, leaving the preflight as the backstop for the
  watchdog being dead or misconfigured.
- The migrate/rollback scripts and the watchdog share the
  `watchdog.conf` vocabulary (`RUNNER_LABELS` ↔
  `RUNNER_DAEMON_LABELS`, multi-account `RUNNER_USER`). Change the
  conf schema, change `migrate-runner-macos.sh` /
  `rollback-runner-macos.sh` in the same commit.
- The governor's permit/demand file names and permissions are minted
  by `install-governor-macos.sh` but consumed by
  `crates/rustc-governor/src/permits.rs` (non-root accounts cannot
  create files in the root-owned permit dir). Change the naming or the
  file ACLs in one, change both.
- The governor's config keys (`enabled`, `permit_dir`,
  `local_reserved`, `ci_reserved`, `ci_users`, `wrap_with`) are
  written by the installer's here-doc and parsed by
  `crates/rustc-governor/src/config.rs` — a minimal TOML-subset
  reader, so keep the conf flat `key = value` (unknown keys are
  ignored — which is also why pre-flip confs carrying the retired
  `real_rustc` key stay parseable; malformed lines make the whole
  file fail open).
