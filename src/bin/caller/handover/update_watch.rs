//! HS6: the update-available surface — detection.
//!
//! At boot the daemon stamps its own binary image on disk (a cheap
//! `current_exe` identity stamp beside the compiled-in provenance); a
//! 60 s stat poll notices the image changing, and a bounded
//! `<binary> --version` probe reads the NEW build's provenance so the
//! chip can say "update on disk: <sha>, built <ts> — running <sha>,
//! booted <when>". ONE info notification per distinct on-disk sha
//! (in-memory dedup — a restart re-notifies once, honestly: the fact is
//! new to the new boot). On macOS an unsigned / non-Developer-ID image
//! carries the keychain/TCC honesty line: its first custody or
//! capture access after a takeover may re-prompt (item ACLs and TCC
//! grants key on the signing identity). The old daemon never execs a
//! successor daemon (Q8) — detection and the `--version` probe only;
//! takeover stays an explicit gesture on the surfaces that carry one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Poll cadence for the on-disk stat. `INTENDANT_UPDATE_POLL_MS`
/// overrides for rigs (clamped to ≥100 ms so a typo cannot spin).
fn poll_interval() -> std::time::Duration {
    std::env::var("INTENDANT_UPDATE_POLL_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|ms| std::time::Duration::from_millis(ms.max(100)))
        .unwrap_or(std::time::Duration::from_secs(60))
}

/// Probe give-up bound per image: a mid-link partial file fails to exec
/// and the next tick retries; a genuinely broken file stops probing
/// after this many failures and surfaces the honest `probe_error`.
const PROBE_FAILURE_LIMIT: u32 = 3;

/// Bound on the `--version` (and macOS `codesign`) subprocess.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Cheap identity stamp of the binary image at a path. Unix adds
/// (dev, ino) so an in-place relink of identical length still flips
/// identity; len + mtime carry it everywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BinaryStamp {
    len: u64,
    modified_ms: Option<u64>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl BinaryStamp {
    pub(crate) fn read(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(BinaryStamp {
                len: meta.len(),
                modified_ms,
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        Ok(BinaryStamp {
            len: meta.len(),
            modified_ms,
        })
    }
}

/// A successful `--version` probe of the on-disk image.
#[derive(Debug, Clone)]
pub(crate) struct ProbedBuild {
    pub(crate) version: String,
    pub(crate) git_sha: String,
    pub(crate) built_at: String,
    /// macOS only: whether the image carries a Developer ID authority
    /// (`None` = not checked / not macOS). `Some(false)` raises the
    /// keychain/TCC honesty line.
    pub(crate) developer_id_signed: Option<bool>,
}

/// Parse `build_info::version_line` output:
/// `<name> <version> (commit <sha>, built <ts>, <triple>)`.
pub(crate) fn parse_version_line(output: &str) -> Option<ProbedBuild> {
    let line = output.lines().next()?.trim();
    let open = line.find("(commit ")?;
    let version = line[..open].trim().rsplit(' ').next()?.to_string();
    let rest = &line[open + "(commit ".len()..];
    let (sha, rest) = rest.split_once(", built ")?;
    let (built_at, _) = rest.split_once(", ")?;
    if sha.trim().is_empty() || built_at.trim().is_empty() {
        return None;
    }
    Some(ProbedBuild {
        version,
        git_sha: sha.trim().to_string(),
        built_at: built_at.trim().to_string(),
        developer_id_signed: None,
    })
}

/// The keychain/TCC honesty line (2.7 / Q8): rendered wherever the
/// update block renders, whenever the observed image is not
/// Developer ID-signed.
pub(crate) const KEYCHAIN_HONESTY_LINE: &str = "This build is not Developer ID-signed — after a \
     takeover its first Keychain (credential custody) or screen/mic access may re-prompt.";

/// A notification the watch wants delivered (one per distinct sha).
pub(crate) struct UpdateNotice {
    /// Stable notification id (`update-available-<sha>`); doubles as
    /// the client-side toast identity.
    pub(crate) id: String,
    pub(crate) text: String,
}

/// What the watch currently observes on disk, rendered into the
/// `update` status block.
struct Observation {
    build: Option<ProbedBuild>,
    probe_error: Option<String>,
    first_seen_ms: u64,
}

/// The detection state machine — pure (no I/O): the task feeds it
/// stat results and probe outcomes, it answers with notices and the
/// status block. Hermetically unit-testable.
pub(crate) struct UpdateWatch {
    exe_path: PathBuf,
    /// The image this process booted from (`None` = the boot stat
    /// failed; the first successful stat then becomes the baseline).
    boot_stamp: Option<BinaryStamp>,
    running_version: String,
    running_sha: String,
    running_built_at: String,
    booted_at_ms: u64,
    /// The stamp the last probe ran against (success or give-up), so an
    /// unchanged image never re-probes.
    probed_stamp: Option<BinaryStamp>,
    /// Consecutive probe failures for `failed_stamp`.
    failed: Option<(BinaryStamp, u32)>,
    observed: Option<Observation>,
    notified_shas: HashSet<String>,
}

impl UpdateWatch {
    pub(crate) fn new(
        exe_path: PathBuf,
        boot_stamp: Option<BinaryStamp>,
        booted_at_ms: u64,
    ) -> Self {
        UpdateWatch {
            exe_path,
            boot_stamp,
            running_version: crate::build_info::pkg_version().to_string(),
            running_sha: crate::build_info::git_sha().to_string(),
            running_built_at: crate::build_info::build_timestamp().to_string(),
            booted_at_ms,
            probed_stamp: None,
            failed: None,
            observed: None,
            notified_shas: HashSet::new(),
        }
    }

    pub(crate) fn exe_path(&self) -> &Path {
        &self.exe_path
    }

    /// Should this tick run the `--version` probe? Only for a stamp
    /// that differs from the boot image AND from the last probed one,
    /// and whose failure budget is not exhausted.
    pub(crate) fn probe_wanted(&self, stamp: &Option<BinaryStamp>) -> bool {
        let Some(stamp) = stamp else { return false };
        if self.boot_stamp.as_ref() == Some(stamp) {
            return false;
        }
        if self.probed_stamp.as_ref() == Some(stamp) {
            return false;
        }
        match &self.failed {
            Some((failed_stamp, count)) => failed_stamp != stamp || *count < PROBE_FAILURE_LIMIT,
            None => true,
        }
    }

    /// Fold one tick's observations. `probe` is `Some` exactly when
    /// [`Self::probe_wanted`] asked for one. Returns the notification
    /// to deliver, at most once per distinct on-disk sha.
    pub(crate) fn record(
        &mut self,
        stamp: Option<BinaryStamp>,
        probe: Option<Result<ProbedBuild, String>>,
        now_ms: u64,
    ) -> Option<UpdateNotice> {
        let Some(stamp) = stamp else {
            // Stat failure (mid-swap unlink, transient FS error): keep
            // the last honest observation rather than flapping.
            return None;
        };
        if self.boot_stamp.is_none() {
            // Boot stat failed: the first image we CAN see becomes the
            // baseline — we cannot claim it changed from an unseen one.
            self.boot_stamp = Some(stamp);
            return None;
        }
        if self.boot_stamp.as_ref() == Some(&stamp) {
            // The running image is (back) on disk — nothing to update to.
            self.observed = None;
            self.probed_stamp = None;
            self.failed = None;
            return None;
        }
        match probe {
            Some(Ok(build)) => {
                self.probed_stamp = Some(stamp);
                self.failed = None;
                if build.git_sha == self.running_sha {
                    // Same build re-materialized (a fresh copy of the
                    // running commit): not an update.
                    self.observed = None;
                    return None;
                }
                let first_seen_ms = self
                    .observed
                    .as_ref()
                    .map(|observation| observation.first_seen_ms)
                    .unwrap_or(now_ms);
                let notice = self.notified_shas.insert(build.git_sha.clone()).then(|| {
                    let honesty = if build.developer_id_signed == Some(false) {
                        format!(" {KEYCHAIN_HONESTY_LINE}")
                    } else {
                        String::new()
                    };
                    UpdateNotice {
                        id: format!("update-available-{}", build.git_sha),
                        text: format!(
                            "The intendant binary on disk changed: commit {} ({}), built {} — \
                             this daemon runs commit {}, booted {}. The dashboard's daemon \
                             update chip has the details.{}",
                            build.git_sha,
                            build.version,
                            build.built_at,
                            self.running_sha,
                            format_instant_ms(self.booted_at_ms),
                            honesty,
                        ),
                    }
                });
                self.observed = Some(Observation {
                    build: Some(build),
                    probe_error: None,
                    first_seen_ms,
                });
                notice
            }
            Some(Err(error)) => {
                let count = match &self.failed {
                    Some((failed_stamp, count)) if failed_stamp == &stamp => count + 1,
                    _ => 1,
                };
                if count >= PROBE_FAILURE_LIMIT {
                    // Give up on this image: surface the change honestly
                    // (no sha, so no notification to dedup) and stop
                    // probing until the image changes again.
                    self.probed_stamp = Some(stamp.clone());
                    self.observed = Some(Observation {
                        build: None,
                        probe_error: Some(error),
                        first_seen_ms: self
                            .observed
                            .as_ref()
                            .map(|observation| observation.first_seen_ms)
                            .unwrap_or(now_ms),
                    });
                }
                self.failed = Some((stamp, count));
                None
            }
            // No probe this tick (unchanged or already probed): keep
            // the current observation.
            None => None,
        }
    }

    /// The `update` block for the handover status payload — `None`
    /// while the on-disk image is the running one (no chip).
    pub(crate) fn status_json(&self) -> Option<serde_json::Value> {
        let observation = self.observed.as_ref()?;
        let mut block = serde_json::json!({
            "running": {
                "version": self.running_version,
                "git_sha": self.running_sha,
                "built_at": self.running_built_at,
                "booted_at_ms": self.booted_at_ms,
            },
            "first_seen_ms": observation.first_seen_ms,
        });
        let obj = block.as_object_mut().expect("literal object");
        match &observation.build {
            Some(build) => {
                obj.insert(
                    "on_disk".into(),
                    serde_json::json!({
                        "version": build.version,
                        "git_sha": build.git_sha,
                        "built_at": build.built_at,
                        "developer_id_signed": build.developer_id_signed,
                    }),
                );
                if build.developer_id_signed == Some(false) {
                    obj.insert("honesty".into(), KEYCHAIN_HONESTY_LINE.into());
                }
            }
            None => {
                if let Some(error) = &observation.probe_error {
                    obj.insert("probe_error".into(), error.clone().into());
                }
            }
        }
        Some(block)
    }
}

/// Render an epoch-ms instant for notification text (UTC, minute
/// precision — display currency only).
fn format_instant_ms(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .map(|instant| instant.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| format!("{ms} ms"))
}

/// The `--version` probe (transport edge): bounded subprocess with a
/// scrubbed environment — the probe target needs nothing, and the
/// daemon's provider authority must never leak into an on-disk image we
/// merely observe.
async fn run_version_probe(path: &Path) -> Result<ProbedBuild, String> {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(path)
            .arg("--version")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("--version probe timed out ({}s)", PROBE_TIMEOUT.as_secs()))?
    .map_err(|e| format!("--version probe failed to spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!("--version probe exited {}", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut build = parse_version_line(&text).ok_or_else(|| {
        format!(
            "unparseable --version output: {:.120}",
            text.lines().next().unwrap_or_default()
        )
    })?;
    if cfg!(target_os = "macos") {
        build.developer_id_signed = Some(developer_id_signed(path).await);
    }
    Ok(build)
}

/// macOS: does the image carry a Developer ID authority? `codesign -dv`
/// prints the authority chain on stderr; ad-hoc and unsigned images
/// don't carry one (and unsigned exits non-zero) — both read `false`,
/// which is the honest default for the re-prompt warning.
async fn developer_id_signed(path: &Path) -> bool {
    let probe = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new("codesign")
            .arg("-dv")
            .arg("--")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match probe {
        Ok(Ok(output)) => {
            String::from_utf8_lossy(&output.stderr).contains("Authority=Developer ID")
        }
        _ => false,
    }
}

/// The watched path: `current_exe`, captured once at spawn (on Linux a
/// replaced image turns `/proc/self/exe` into a `(deleted)` alias, so
/// the boot-time capture is the honest path to keep statting).
/// `INTENDANT_UPDATE_WATCH_PATH` overrides for rigs, honored only
/// alongside `PROVIDER=mock` (fail-closed otherwise, mirroring
/// `INTENDANT_MOCK_DISPLAY`) — the probe execs this path, so a plain
/// env var must never redirect a real daemon's exec target.
fn watched_binary_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("INTENDANT_UPDATE_WATCH_PATH") {
        if std::env::var("PROVIDER").as_deref() == Ok("mock") {
            return Some(PathBuf::from(override_path));
        }
        eprintln!(
            "[update] INTENDANT_UPDATE_WATCH_PATH ignored: PROVIDER=mock is not set \
             (the override is a mock-rig knob, never a production redirect)"
        );
    }
    std::env::current_exe().ok()
}

/// Spawn the detection task: stat cadence, probe on change, chip block
/// onto the handover runtime, one deduped notification per distinct
/// on-disk sha. Detached like its sibling daemon tasks.
pub(crate) fn spawn_update_watch(runtime: std::sync::Arc<super::HandoverRuntime>) {
    let Some(exe_path) = watched_binary_path() else {
        eprintln!("[update] current_exe unresolvable — update watch off for this boot");
        return;
    };
    let boot_stamp = BinaryStamp::read(&exe_path).ok();
    let booted_at_ms = super::now_ms();
    tokio::spawn(async move {
        let mut watch = UpdateWatch::new(exe_path, boot_stamp, booted_at_ms);
        let mut ticks = tokio::time::interval(poll_interval());
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let stamp = BinaryStamp::read(watch.exe_path()).ok();
            let probe = if watch.probe_wanted(&stamp) {
                Some(run_version_probe(watch.exe_path()).await)
            } else {
                None
            };
            if let Some(notice) = watch.record(stamp, probe, super::now_ms()) {
                eprintln!("[update] {}", notice.text);
                runtime.notify_user(
                    &notice.id,
                    Some("Update on disk"),
                    &notice.text,
                    crate::types::NotificationUrgency::Info,
                );
            }
            runtime.set_update_status(watch.status_json());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(len: u64, modified_ms: u64, ino: u64) -> BinaryStamp {
        #[cfg(unix)]
        {
            BinaryStamp {
                len,
                modified_ms: Some(modified_ms),
                dev: 1,
                ino,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ino;
            BinaryStamp {
                len,
                modified_ms: Some(modified_ms),
            }
        }
    }

    fn probed(sha: &str, signed: Option<bool>) -> ProbedBuild {
        ProbedBuild {
            version: "9.9.9".to_string(),
            git_sha: sha.to_string(),
            built_at: "2026-07-29T12:41:00Z".to_string(),
            developer_id_signed: signed,
        }
    }

    fn fresh_watch() -> UpdateWatch {
        let mut watch = UpdateWatch::new(
            PathBuf::from("/tmp/intendant-test-binary"),
            Some(stamp(100, 1_000, 1)),
            1_000,
        );
        // Pin the running provenance independent of the test binary's
        // real build stamps.
        watch.running_sha = "runningsha".to_string();
        watch.running_version = "0.0.0".to_string();
        watch.running_built_at = "2026-07-01T00:00:00Z".to_string();
        watch
    }

    /// HS6 conformance pin `update_chip_from_stat_change_and_version_probe`:
    /// an unchanged image never probes and shows no chip; a changed image
    /// probes once, and the chip block carries the on-disk build beside
    /// the running provenance; the image reverting to the boot stamp
    /// clears the chip.
    #[test]
    fn update_chip_from_stat_change_and_version_probe() {
        let mut watch = fresh_watch();

        // Unchanged image: no probe wanted, no chip.
        let boot = Some(stamp(100, 1_000, 1));
        assert!(!watch.probe_wanted(&boot));
        assert!(watch.record(boot, None, 2_000).is_none());
        assert!(watch.status_json().is_none(), "no chip without a change");

        // Changed image: probe wanted; the probed build renders the chip.
        let changed = Some(stamp(200, 5_000, 2));
        assert!(watch.probe_wanted(&changed));
        let notice = watch.record(changed.clone(), Some(Ok(probed("newsha", None))), 6_000);
        assert!(notice.is_some(), "first sighting notifies");
        let block = watch.status_json().expect("chip block");
        assert_eq!(block["on_disk"]["git_sha"], "newsha");
        assert_eq!(block["running"]["git_sha"], "runningsha");
        assert_eq!(block["first_seen_ms"], 6_000);

        // Same image next tick: no re-probe, chip stays.
        assert!(!watch.probe_wanted(&changed));
        assert!(watch.record(changed, None, 7_000).is_none());
        assert!(watch.status_json().is_some());

        // The boot image returns (revert): chip clears.
        assert!(watch
            .record(Some(stamp(100, 1_000, 1)), None, 8_000)
            .is_none());
        assert!(watch.status_json().is_none(), "revert clears the chip");
    }

    /// HS6 conformance pin `one_notification_per_distinct_on_disk_sha`:
    /// repeated observations of one sha (across distinct image stamps)
    /// notify exactly once; a new sha notifies again; the running sha
    /// re-materializing never notifies.
    #[test]
    fn one_notification_per_distinct_on_disk_sha() {
        let mut watch = fresh_watch();

        let first = Some(stamp(200, 5_000, 2));
        assert!(watch
            .record(first, Some(Ok(probed("sha-a", None))), 5_000)
            .is_some());

        // The same sha lands under a NEW stamp (rebuilt identical
        // commit): probed again, notified NOT again.
        let rebuilt = Some(stamp(201, 6_000, 3));
        assert!(watch.probe_wanted(&rebuilt));
        assert!(watch
            .record(rebuilt, Some(Ok(probed("sha-a", None))), 6_000)
            .is_none());

        // A different sha: second notification.
        let second = Some(stamp(300, 7_000, 4));
        let notice = watch
            .record(second, Some(Ok(probed("sha-b", None))), 7_000)
            .expect("new sha notifies");
        assert_eq!(notice.id, "update-available-sha-b");

        // The RUNNING sha re-materializing is not an update.
        let same_as_running = Some(stamp(400, 8_000, 5));
        assert!(watch
            .record(same_as_running, Some(Ok(probed("runningsha", None))), 8_000)
            .is_none());
        assert!(watch.status_json().is_none());
    }

    /// The keychain/TCC honesty line (Q8, §2.7): a non-Developer-ID
    /// image carries the line on the chip block AND in the
    /// notification; a signed image carries neither.
    #[test]
    fn keychain_honesty_line_rides_unsigned_observations() {
        let mut watch = fresh_watch();
        let notice = watch
            .record(
                Some(stamp(200, 5_000, 2)),
                Some(Ok(probed("unsigned-sha", Some(false)))),
                5_000,
            )
            .expect("notifies");
        assert!(notice.text.contains("not Developer ID-signed"));
        let block = watch.status_json().expect("chip block");
        assert_eq!(block["honesty"], KEYCHAIN_HONESTY_LINE);

        let mut watch = fresh_watch();
        let notice = watch
            .record(
                Some(stamp(200, 5_000, 2)),
                Some(Ok(probed("signed-sha", Some(true)))),
                5_000,
            )
            .expect("notifies");
        assert!(!notice.text.contains("Developer ID"));
        assert!(watch.status_json().expect("chip")["honesty"].is_null());
    }

    /// Probe failures are bounded per image: retries stop at the limit,
    /// the change still surfaces honestly (`probe_error`, no sha, no
    /// notification), and a NEW image gets a fresh budget.
    #[test]
    fn probe_failures_bound_then_surface_honestly() {
        let mut watch = fresh_watch();
        let broken = Some(stamp(200, 5_000, 2));
        for attempt in 1..=PROBE_FAILURE_LIMIT {
            assert!(
                watch.probe_wanted(&broken),
                "attempt {attempt} still probes"
            );
            assert!(watch
                .record(broken.clone(), Some(Err("exec failed".to_string())), 5_000)
                .is_none());
        }
        assert!(!watch.probe_wanted(&broken), "budget exhausted");
        let block = watch.status_json().expect("honest change block");
        assert_eq!(block["probe_error"], "exec failed");
        assert!(block.get("on_disk").is_none());

        let replaced = Some(stamp(300, 6_000, 3));
        assert!(watch.probe_wanted(&replaced), "new image, fresh budget");
    }

    /// The probe parser reads exactly `build_info::version_line`'s shape
    /// and refuses everything else.
    #[test]
    fn version_line_parses_and_refuses() {
        let build = parse_version_line(
            "intendant 0.1.0 (commit 3e4c79f8, built 2026-07-29T01:02:03Z, aarch64-apple-darwin)\n",
        )
        .expect("canonical line parses");
        assert_eq!(build.version, "0.1.0");
        assert_eq!(build.git_sha, "3e4c79f8");
        assert_eq!(build.built_at, "2026-07-29T01:02:03Z");
        // The bundle binary name differs (intendant-bin) — the parser
        // keys on the provenance parenthetical, not the name.
        assert!(parse_version_line(
            "intendant-bin 0.1.0 (commit abc, built 2026-01-01, x86_64-pc-windows-msvc)"
        )
        .is_some());
        assert!(parse_version_line("").is_none());
        assert!(parse_version_line("bash: no such file").is_none());
        assert!(parse_version_line("intendant 0.1.0").is_none());
    }
}
