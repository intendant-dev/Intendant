//! The self-update lane — the PRODUCE half of the update surface
//! (commission 01KYSWZC7BBPJGT48G9WYFF7J3). Track HS shipped the SWAP
//! half completely: the on-disk watch ([`super::update_watch`]), the
//! update chip + one-per-sha notification, and the packaged app's
//! one-click drain-swap of whatever binary is ON DISK. This module adds
//! the missing first phase: getting a NEWER binary onto disk, on the
//! owner's explicit click, then handing off to that shipped lane.
//!
//! Two lanes, selected by install flavor:
//!
//! - **Source** (the running binary sits in a git checkout's
//!   `target/release`, or the packaged app carries a valid
//!   source-checkout stamp): a bounded behind-`origin/main` compare;
//!   on click, `git pull --ff-only` + `cargo build` (or
//!   `scripts/bundle-macos.sh` for the app shape) as supervised child
//!   processes. The build rides the machine's rustc governor untouched
//!   (the child env never sets `RUSTC_WRAPPER`, so the box-wide cargo
//!   config wrapper stays engaged) and is HEADROOM-GATED: under memory
//!   pressure the job refuses to start rather than joining an OOM
//!   spiral.
//! - **Consumer** (an installed release app with no source checkout):
//!   the latest release manifest via the transparency-log ritual
//!   (`hosted_verify::verify_hosted_release` — inclusion proof, signed
//!   tree head, append-only pin, PGP identity/coverage), then download
//!   with sha256 checked against the LOG, `gpg --verify` against the
//!   compiled-in release signing key in a throwaway GNUPGHOME, then
//!   install beside the running app. FAIL CLOSED on every verify step:
//!   unverified bytes are deleted, never installed.
//!
//! Boundary (the commission's first-class gate line): the daemon never
//! self-execs the build or fetch, and this module never execs a
//! successor daemon — the work runs as supervised, timeout-bounded,
//! env-curated child processes (`git`, `cargo`,
//! `bash scripts/bundle-macos.sh`, `gpg`, `ditto`) plus the
//! already-ruled hosted-verify fetch machinery, with progress and
//! failure rendered honestly on the handover status payload. The update
//! stays two phases: this module PRODUCES the artifact on disk; the
//! shipped watch/chip/one-click lane performs the swap (the app
//! supervisor's spawn when one is attached, or — ruled 2026-07-31, its
//! own explicit click, NEVER a side effect of produce — the
//! [`super::successor_exec`] lane on CLI-launched daemons). The owner's
//! click is the consent surface — nothing here runs without it except
//! the bounded behind-ness check.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Cap on the behind-main count (`git rev-list --count --max-count`):
/// past this the panel says "500+" — a bounded compare, literally.
const BEHIND_COUNT_CAP: u32 = 500;

/// Bounds on the dev-channel shortlog (newest-first `%h %s` lines the
/// panel renders as data beside the behind-count).
const SHORTLOG_MAX_LINES: usize = 20;
const SHORTLOG_LINE_CAP: usize = 160;

/// Bounded log tail kept per job for the status payload (each line also
/// goes to the daemon log as it happens).
const JOB_LOG_TAIL_LINES: usize = 60;
const JOB_LOG_LINE_CAP: usize = 400;

/// Per-phase child timeouts. Builds get an hour — a cold release build
/// on a loaded box is slow, and the governor may queue the link.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);
const GPG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const UNPACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Linux headroom floor: a debug `intendant` link alone peaks ~2 GiB of
/// linker RSS; starting a build with less than this available joins the
/// swap-storm class the governor exists to prevent.
const LINUX_MIN_AVAILABLE_KB: u64 = 3 * 1024 * 1024;

/// The `.asc` detached signatures are small armored text.
const ASC_BYTE_CAP: usize = 64 * 1024;

// ── Install flavor ──────────────────────────────────────────────────

/// How this binary got onto disk — decides which lane the panel offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallFlavor {
    /// A git checkout this daemon can rebuild. `app_bundle` selects the
    /// produce step: plain `cargo build` (artifact lands AT the watched
    /// path) vs `scripts/bundle-macos.sh` (builds, signs, installs to
    /// /Applications — the script's only install target).
    Source {
        repo_root: PathBuf,
        app_bundle: bool,
    },
    /// An installed app bundle with no reachable source checkout: the
    /// release download lane.
    ConsumerApp { app_root: PathBuf },
    /// Neither lane can honestly produce an artifact at the watched
    /// path; the panel says why instead of offering a button.
    Unmanaged { reason: String },
}

impl InstallFlavor {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            InstallFlavor::Source { .. } => "source",
            InstallFlavor::ConsumerApp { .. } => "consumer-app",
            InstallFlavor::Unmanaged { .. } => "unmanaged",
        }
    }

    /// The channel a request without one means: a source checkout
    /// updates from main, everything else from releases.
    pub(crate) fn native_channel(&self) -> UpdateChannel {
        match self {
            InstallFlavor::Source { .. } => UpdateChannel::Dev,
            _ => UpdateChannel::Releases,
        }
    }
}

// ── Channels (the front door's vocabulary) ──────────────────────────

/// The update panel's two-channel vocabulary: **Releases** is the
/// default lane for everyone — logged, PGP-verified builds, fail closed
/// on every verify step; **Dev — build from main** sits behind the
/// panel's Advanced fold and pulls + rebuilds a source checkout.
/// Exactly two: there is no nightly lane and none is invented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateChannel {
    Releases,
    Dev,
}

/// Parse the optional `{"channel": "releases"|"dev"}` request body.
/// An absent body/field means the install's native channel; an unknown
/// channel name is refused by name (the vocabulary is exactly two).
pub(crate) fn parse_channel_arg(body_text: &str) -> Result<Option<UpdateChannel>, String> {
    let Ok(body) = serde_json::from_str::<serde_json::Value>(body_text) else {
        return Ok(None);
    };
    let Some(raw) = body.get("channel") else {
        return Ok(None);
    };
    match raw.as_str() {
        Some("releases") => Ok(Some(UpdateChannel::Releases)),
        Some("dev") => Ok(Some(UpdateChannel::Dev)),
        _ => Err(format!(
            "unknown update channel {} — this daemon has exactly two: \"releases\" and \"dev\"",
            raw.to_string().chars().take(60).collect::<String>()
        )),
    }
}

/// Why a check on `channel` cannot run for this install (`None` = it
/// can). The releases check is data about the latest logged release —
/// honest on every install shape; the dev compare needs a checkout.
pub(crate) fn check_refusal(flavor: &InstallFlavor, channel: UpdateChannel) -> Option<String> {
    match (channel, flavor) {
        (UpdateChannel::Releases, _) => None,
        (UpdateChannel::Dev, InstallFlavor::Source { .. }) => None,
        (UpdateChannel::Dev, InstallFlavor::ConsumerApp { .. }) => Some(
            "no source checkout around this install — the Dev channel compares against and \
             rebuilds a git checkout"
                .to_string(),
        ),
        (UpdateChannel::Dev, InstallFlavor::Unmanaged { reason }) => Some(reason.clone()),
    }
}

/// Why a produce click on `channel` cannot run for this install
/// (`None` = it can). `macos` is a parameter so the whole matrix stays
/// hermetic under test on every host.
pub(crate) fn produce_refusal(
    flavor: &InstallFlavor,
    channel: UpdateChannel,
    macos: bool,
) -> Option<String> {
    match (channel, flavor) {
        (_, InstallFlavor::Unmanaged { reason }) => {
            Some(format!("no update lane for this install: {reason}"))
        }
        (UpdateChannel::Dev, InstallFlavor::Source { .. }) => None,
        (UpdateChannel::Dev, InstallFlavor::ConsumerApp { .. }) => {
            check_refusal(flavor, UpdateChannel::Dev)
        }
        (UpdateChannel::Releases, InstallFlavor::ConsumerApp { .. }) if !macos => Some(
            "releases currently ship only the macOS app — rebuild from source on this \
             platform"
                .to_string(),
        ),
        (UpdateChannel::Releases, InstallFlavor::ConsumerApp { .. }) => None,
        (
            UpdateChannel::Releases,
            InstallFlavor::Source {
                repo_root,
                app_bundle,
            },
        ) => Some(if *app_bundle {
            format!(
                "this app is a source build from {} — its update path rebuilds from main \
                 (the Dev channel behind Advanced), not the release download",
                repo_root.display()
            )
        } else {
            format!(
                "this daemon runs from the checkout at {} — a release would install the \
                 packaged macOS app, not this binary; updates for this install build from \
                 main (the Dev channel behind Advanced)",
                repo_root.display()
            )
        }),
    }
}

/// The per-channel availability catalog the panel derives its sections
/// and buttons from — declared here once, never mirrored client-side.
pub(crate) fn channel_catalog(flavor: &InstallFlavor, macos: bool) -> serde_json::Value {
    let channel_block = |channel: UpdateChannel| {
        let check = check_refusal(flavor, channel);
        let produce = produce_refusal(flavor, channel, macos);
        let mut block = serde_json::json!({
            "check": check.is_none(),
            "produce": produce.is_none(),
        });
        let obj = block.as_object_mut().expect("literal object");
        if let Some(reason) = produce.or(check) {
            obj.insert("reason".into(), reason.into());
        }
        block
    };
    serde_json::json!({
        "releases": channel_block(UpdateChannel::Releases),
        "dev": channel_block(UpdateChannel::Dev),
    })
}

/// The stamp `scripts/bundle-macos.sh` writes into the bundle
/// (`Contents/Resources/<file>`): the absolute checkout path the app was
/// built from, so an INSTALLED source-built app can still offer the
/// source lane. A release app carries its CI runner's path, which does
/// not exist on a consumer machine — the probe fails down to the
/// consumer lane, which is exactly right.
pub(crate) const SOURCE_STAMP_RESOURCE: &str = "source-checkout";

/// The app bundle containing `exe`, when `exe` sits at
/// `<name>.app/Contents/MacOS/<binary>`.
fn containing_app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos_dir.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    if app.extension()? != "app" {
        return None;
    }
    Some(app.to_path_buf())
}

/// A directory is a usable intendant checkout when it has a git
/// dir/file (worktrees carry a `.git` FILE) and the build entry points
/// the produce step needs. Deliberately shallow — the git/cargo
/// children fail honestly on anything subtler.
fn is_buildable_checkout(dir: &Path) -> bool {
    dir.join(".git").exists()
        && dir.join("Cargo.toml").is_file()
        && dir.join("scripts").join("bundle-macos.sh").is_file()
}

/// Classify the watched binary path. Pure over the filesystem shape —
/// hermetic under a tempdir.
pub(crate) fn detect_install_flavor(exe: &Path) -> InstallFlavor {
    if let Some(app_root) = containing_app_bundle(exe) {
        // Source-built app? The bundle stamp names its checkout.
        let stamp = app_root
            .join("Contents")
            .join("Resources")
            .join(SOURCE_STAMP_RESOURCE);
        if let Ok(text) = std::fs::read_to_string(&stamp) {
            let recorded = PathBuf::from(text.trim());
            if !recorded.as_os_str().is_empty() && is_buildable_checkout(&recorded) {
                return InstallFlavor::Source {
                    repo_root: recorded,
                    app_bundle: true,
                };
            }
        }
        return InstallFlavor::ConsumerApp { app_root };
    }
    // Plain binary: offer the source lane only when the binary IS the
    // checkout's release output — that is the path the update watch
    // watches, so a rebuild lands exactly where the chip looks.
    let mut ancestor = exe.parent();
    while let Some(dir) = ancestor {
        if dir.join(".git").exists() {
            if !is_buildable_checkout(dir) {
                return InstallFlavor::Unmanaged {
                    reason: "the running binary sits in a git repository that is not a \
                             buildable intendant checkout"
                        .to_string(),
                };
            }
            let release_binary = dir
                .join("target")
                .join("release")
                .join(format!("intendant{}", std::env::consts::EXE_SUFFIX));
            if exe == release_binary.as_path() {
                return InstallFlavor::Source {
                    repo_root: dir.to_path_buf(),
                    app_bundle: false,
                };
            }
            return InstallFlavor::Unmanaged {
                reason: format!(
                    "the running binary is inside the checkout at {} but is not its \
                     target/release output — a rebuild would not land at the watched path",
                    dir.display()
                ),
            };
        }
        ancestor = dir.parent();
    }
    InstallFlavor::Unmanaged {
        reason: "no source checkout or app bundle found around the running binary".to_string(),
    }
}

// ── The bounded behind-main compare (source lane) ───────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceCheck {
    /// `origin/main` tip after the bounded fetch.
    pub(crate) tip_sha: String,
    /// Commits in `origin/main` not reachable from the RUNNING build's
    /// commit — capped at [`BEHIND_COUNT_CAP`].
    pub(crate) behind: u32,
    pub(crate) behind_capped: bool,
    /// Newest-first `%h %s` lines for the commits behind — bounded
    /// data for the panel, never markup.
    pub(crate) shortlog: Vec<String>,
    /// Tracked files modified in the checkout: the produce step refuses
    /// to touch a dirty tree.
    pub(crate) dirty: bool,
}

/// Fold the bounded git observations into the compare verdict.
/// Pure — the child outputs come in as text; the pinned compare-seam
/// tests live on this function.
pub(crate) fn fold_source_check(
    rev_parse_tip: &str,
    rev_list_count: &str,
    shortlog: &str,
    status_porcelain: &str,
) -> Result<SourceCheck, String> {
    let tip_sha = rev_parse_tip.trim().to_string();
    if tip_sha.len() < 7 || !tip_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "origin/main did not resolve to a commit (git said: {})",
            rev_parse_tip.trim().chars().take(120).collect::<String>()
        ));
    }
    let behind: u32 = rev_list_count.trim().parse().map_err(|_| {
        format!(
            "unparseable behind-count from git: {:?}",
            rev_list_count.trim()
        )
    })?;
    Ok(SourceCheck {
        tip_sha,
        behind,
        behind_capped: behind >= BEHIND_COUNT_CAP,
        shortlog: fold_shortlog(shortlog),
        dirty: tracked_dirty(status_porcelain),
    })
}

/// Bound the shortlog into displayable data lines: trimmed, width-
/// capped, at most [`SHORTLOG_MAX_LINES`] (defense in depth beside the
/// git child's own `--max-count`).
pub(crate) fn fold_shortlog(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(SHORTLOG_MAX_LINES)
        .map(|line| {
            let mut line = line.to_string();
            truncate_on_boundary(&mut line, SHORTLOG_LINE_CAP);
            line
        })
        .collect()
}

/// Truncate to at most `cap` bytes on a char boundary
/// (`String::truncate` panics mid-codepoint, and commit subjects and
/// child output are arbitrary unicode).
fn truncate_on_boundary(line: &mut String, cap: usize) {
    if line.len() <= cap {
        return;
    }
    let mut cut = cap;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    line.truncate(cut);
}

/// TRACKED modifications only (`git status --porcelain` lines whose
/// status is not `??`): untracked files — a checkout's `target/`,
/// scratch notes — never block a fast-forward pull, and a genuine
/// path collision fails the pull child itself with git's own words.
pub(crate) fn tracked_dirty(status_porcelain: &str) -> bool {
    status_porcelain
        .lines()
        .any(|line| !line.trim().is_empty() && !line.starts_with("??"))
}

/// Is the latest logged release newer than the running package version?
/// Plain semver-triple compare (`v` prefix and any pre-release/build
/// suffix on the release tag tolerated); `None` = not comparable, which
/// the panel reports instead of guessing.
pub(crate) fn release_version_newer(latest: &str, running: &str) -> Option<bool> {
    fn triple(raw: &str) -> Option<(u64, u64, u64)> {
        let raw = raw.trim().trim_start_matches('v');
        let core = raw
            .split_once(['-', '+'])
            .map(|(core, _)| core)
            .unwrap_or(raw);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some((major, minor, patch))
    }
    Some(triple(latest)? > triple(running)?)
}

// ── Headroom (the capacity linkage) ─────────────────────────────────

/// What the memory-headroom probe observed. `Low` refuses the build;
/// `Unknown` proceeds with the note carried into the job log — the
/// probe that CAN run fails closed on pressure, a platform without one
/// degrades honestly rather than blocking updates forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Headroom {
    Ok(String),
    Low(String),
    Unknown(String),
}

/// The pure gate the pinned test covers: never build under pressure.
pub(crate) fn headroom_gate(headroom: &Headroom) -> Result<Option<String>, String> {
    match headroom {
        Headroom::Ok(detail) => Ok(Some(detail.clone())),
        Headroom::Unknown(note) => Ok(Some(format!("headroom unknown: {note} — proceeding"))),
        Headroom::Low(reason) => Err(format!(
            "not building under memory pressure ({reason}) — retry when the box has headroom"
        )),
    }
}

/// Interpret macOS `sysctl -n kern.memorystatus_vm_pressure_level`
/// output: 1 = normal, 2 = warn, 4 = critical.
pub(crate) fn fold_macos_pressure(raw: &str) -> Headroom {
    match raw.trim().parse::<u32>() {
        Ok(1) => Headroom::Ok("memory pressure level 1 (normal)".to_string()),
        Ok(level) => Headroom::Low(format!("macOS memory pressure level {level}")),
        Err(_) => Headroom::Unknown(format!(
            "unparseable pressure level {:?}",
            raw.trim().chars().take(40).collect::<String>()
        )),
    }
}

/// Interpret `/proc/meminfo` (Linux): `MemAvailable` under the floor is
/// pressure.
pub(crate) fn fold_linux_meminfo(meminfo: &str) -> Headroom {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = match rest.trim().trim_end_matches(" kB").trim().parse() {
                Ok(kb) => kb,
                Err(_) => return Headroom::Unknown("unparseable MemAvailable".to_string()),
            };
            return if kb < LINUX_MIN_AVAILABLE_KB {
                Headroom::Low(format!(
                    "{} MiB available, floor {} MiB",
                    kb / 1024,
                    LINUX_MIN_AVAILABLE_KB / 1024
                ))
            } else {
                Headroom::Ok(format!("{} MiB available", kb / 1024))
            };
        }
    }
    Headroom::Unknown("no MemAvailable in /proc/meminfo".to_string())
}

/// Transport edge: probe the platform's memory headroom. The
/// `INTENDANT_UPDATE_LANE_HEADROOM` override (`ok` / `low`) is honored
/// only under `PROVIDER=mock` — the fail-closed rig-knob pattern of
/// `INTENDANT_UPDATE_WATCH_PATH`.
async fn probe_headroom() -> Headroom {
    if let Ok(forced) = std::env::var("INTENDANT_UPDATE_LANE_HEADROOM") {
        if std::env::var("PROVIDER").as_deref() == Ok("mock") {
            return match forced.as_str() {
                "low" => Headroom::Low("forced by INTENDANT_UPDATE_LANE_HEADROOM".to_string()),
                _ => Headroom::Ok("forced by INTENDANT_UPDATE_LANE_HEADROOM".to_string()),
            };
        }
        eprintln!(
            "[update-lane] INTENDANT_UPDATE_LANE_HEADROOM ignored: PROVIDER=mock is not set \
             (the override is a mock-rig knob, never a production bypass)"
        );
    }
    if cfg!(target_os = "macos") {
        let out = tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::process::Command::new("sysctl")
                .args(["-n", "kern.memorystatus_vm_pressure_level"])
                .env_clear()
                .envs(curated_env(&[]))
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await;
        return match out {
            Ok(Ok(output)) if output.status.success() => {
                fold_macos_pressure(&String::from_utf8_lossy(&output.stdout))
            }
            _ => Headroom::Unknown("sysctl pressure probe failed".to_string()),
        };
    }
    if cfg!(target_os = "linux") {
        return match std::fs::read_to_string("/proc/meminfo") {
            Ok(meminfo) => fold_linux_meminfo(&meminfo),
            Err(err) => Headroom::Unknown(format!("/proc/meminfo unreadable: {err}")),
        };
    }
    Headroom::Unknown("no headroom probe on this platform".to_string())
}

// ── Verify seams (consumer lane) ────────────────────────────────────

/// Map the running binary's arch vocabulary to the bundle script's
/// (`uname -m`): release zips are named `Intendant-<ver>-macos-<arch>.zip`.
pub(crate) fn bundle_arch(rust_arch: &str) -> &'static str {
    match rust_arch {
        "aarch64" => "arm64",
        _ => "x86_64",
    }
}

/// Pick the installable app artifact for this machine from a VERIFIED
/// release plan, requiring its detached signature beside it. Refuses
/// `-unsigned-dev` / `-signed-unnotarized` artifacts by construction:
/// the accepted name shape is exactly `Intendant-…-macos-<arch>.zip`.
pub(crate) fn select_release_asset<'a>(
    artifacts: &'a [crate::hosted_verify::ReleaseArtifactPlan],
    arch: &str,
) -> Result<
    (
        &'a crate::hosted_verify::ReleaseArtifactPlan,
        &'a crate::hosted_verify::ReleaseArtifactPlan,
    ),
    String,
> {
    let suffix = format!("-macos-{arch}.zip");
    let zip = artifacts
        .iter()
        .find(|artifact| {
            artifact.name.starts_with("Intendant-") && artifact.name.ends_with(&suffix)
        })
        .ok_or_else(|| {
            format!(
                "this release carries no installable macOS app for {arch} \
                 (wanted Intendant-…{suffix}; dev/unnotarized artifacts are refused)"
            )
        })?;
    let asc_name = format!("{}.asc", zip.name);
    let asc = artifacts
        .iter()
        .find(|artifact| artifact.name == asc_name)
        .ok_or_else(|| format!("{asc_name}: missing from the logged manifest"))?;
    Ok((zip, asc))
}

/// Fail-closed download verdict: the bytes on disk must hash to exactly
/// what the transparency log committed.
pub(crate) fn download_verdict(
    name: &str,
    logged_sha: &str,
    actual_sha: &str,
) -> Result<(), String> {
    if logged_sha == actual_sha {
        Ok(())
    } else {
        Err(format!(
            "{name}: downloaded bytes hash {} but the transparency log committed {} — \
             refusing to install",
            &actual_sha[..12.min(actual_sha.len())],
            &logged_sha[..12.min(logged_sha.len())],
        ))
    }
}

/// Fail-closed `gpg --verify` verdict over the `--status-fd` output: the
/// run must exit 0 AND report a `VALIDSIG` whose PRIMARY key fingerprint
/// (the line's last field — signatures come from a signing SUBKEY) is
/// exactly the compiled-in release key pin. GOODSIG alone is not enough:
/// it names a key id, not the full primary fingerprint.
pub(crate) fn gpg_verify_verdict(
    exit_ok: bool,
    status_out: &str,
    pinned_primary_fingerprint: &str,
) -> Result<(), String> {
    if !exit_ok {
        return Err(
            "gpg --verify failed — the artifact's signature does not check out".to_string(),
        );
    }
    let valid = status_out.lines().any(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("[GNUPG:]") || fields.next() != Some("VALIDSIG") {
            return false;
        }
        line.split_whitespace().last() == Some(pinned_primary_fingerprint)
    });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "gpg reported no VALIDSIG by the pinned release key {pinned_primary_fingerprint} — \
             refusing to install"
        ))
    }
}

// ── Curated child environment ───────────────────────────────────────

/// Base allowlist every supervised child gets: enough to run the tool,
/// none of the daemon's provider or ambient credential authority. The
/// governor stays engaged because `RUSTC_WRAPPER` is simply never set —
/// the box-wide cargo config supplies it (never override, per the
/// repo's governor doctrine).
const CHILD_ENV_BASE: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TEMP", "TMP", "LANG", "LC_ALL",
    "LC_CTYPE", "TERM",
];

/// Extra names for build/bundle children (toolchain discovery). No
/// `CARGO_TARGET_DIR`: the artifact must land at the checkout's own
/// `target/release` — the path the update watch watches.
const CHILD_ENV_BUILD: &[&str] = &[
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "DEVELOPER_DIR",
];

/// Extra names for git children: the owner's click authorizes exactly a
/// pull from origin, so the agent socket (ssh remotes) and HOME-based
/// credential helpers work as they would by hand.
const CHILD_ENV_GIT: &[&str] = &["SSH_AUTH_SOCK", "GIT_SSH_COMMAND"];

/// Resolve the curated env for a child: base allowlist + the given
/// extras, values from THIS process's environment.
fn curated_env(extra: &[&str]) -> Vec<(String, String)> {
    CHILD_ENV_BASE
        .iter()
        .chain(extra.iter())
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

// ── Job + check state (rendered onto the handover status payload) ───

#[derive(Debug, Clone)]
enum CheckOutcome {
    Source(SourceCheck),
    Consumer {
        tag: String,
        version: String,
        newer: Option<bool>,
    },
}

#[derive(Debug, Clone, Default)]
struct CheckState {
    in_flight: bool,
    checked_at_ms: Option<u64>,
    outcome: Option<Result<CheckOutcome, String>>,
}

/// One slot per channel: the panel renders both lanes' last verdicts
/// side by side, so a dev compare never overwrites the release story.
#[derive(Debug, Default)]
struct CheckSlots {
    releases: CheckState,
    dev: CheckState,
}

impl CheckSlots {
    fn slot_mut(&mut self, channel: UpdateChannel) -> &mut CheckState {
        match channel {
            UpdateChannel::Releases => &mut self.releases,
            UpdateChannel::Dev => &mut self.dev,
        }
    }
}

#[derive(Debug, Clone)]
struct JobState {
    lane: &'static str,
    phase: String,
    started_ms: u64,
    log: std::collections::VecDeque<String>,
    /// `Some` = finished; `Ok` carries the success detail.
    outcome: Option<Result<String, String>>,
}

/// The lane singleton, installed on the [`super::HandoverRuntime`] at
/// spawn. Route handlers reach it through the runtime; the status block
/// rides `status_json()` beside the update watch's chip block.
pub(crate) struct UpdateLane {
    runtime: Weak<super::HandoverRuntime>,
    exe_path: PathBuf,
    flavor: InstallFlavor,
    check: Mutex<CheckSlots>,
    job: Mutex<Option<JobState>>,
    job_running: AtomicBool,
}

impl UpdateLane {
    fn new(runtime: &Arc<super::HandoverRuntime>, exe_path: PathBuf) -> Self {
        let flavor = detect_install_flavor(&exe_path);
        UpdateLane {
            runtime: Arc::downgrade(runtime),
            exe_path,
            flavor,
            check: Mutex::new(CheckSlots::default()),
            job: Mutex::new(None),
            job_running: AtomicBool::new(false),
        }
    }

    /// The `update_lane` block on the handover status payload: flavor,
    /// running provenance, the last compare, and the job (with its
    /// bounded log tail) — the panel's whole truth.
    pub(crate) fn status_block(&self) -> serde_json::Value {
        let mut block = serde_json::json!({
            "flavor": self.flavor.kind(),
            "running": {
                "version": crate::build_info::pkg_version(),
                "git_sha": crate::build_info::git_sha(),
                "built_at": crate::build_info::build_timestamp(),
            },
        });
        let obj = block.as_object_mut().expect("literal object");
        match &self.flavor {
            InstallFlavor::Source {
                repo_root,
                app_bundle,
            } => {
                obj.insert("repo_root".into(), repo_root.display().to_string().into());
                obj.insert("app_bundle".into(), (*app_bundle).into());
            }
            InstallFlavor::ConsumerApp { app_root } => {
                obj.insert("app_root".into(), app_root.display().to_string().into());
                if !cfg!(target_os = "macos") {
                    obj.insert(
                        "unavailable".into(),
                        "releases currently ship only the macOS app".into(),
                    );
                }
            }
            InstallFlavor::Unmanaged { reason } => {
                obj.insert("unavailable".into(), reason.clone().into());
            }
        }
        if let Ok(slots) = self.check.lock() {
            obj.insert(
                "checks".into(),
                serde_json::json!({
                    "releases": check_state_block(&slots.releases),
                    "dev": check_state_block(&slots.dev),
                }),
            );
        }
        obj.insert(
            "channels".into(),
            channel_catalog(&self.flavor, cfg!(target_os = "macos")),
        );
        obj.insert("default_channel".into(), "releases".into());
        if let Ok(job) = self.job.lock() {
            if let Some(job) = job.as_ref() {
                let mut job_block = serde_json::json!({
                    "lane": job.lane,
                    "phase": job.phase,
                    "started_ms": job.started_ms,
                    "log_tail": job.log.iter().cloned().collect::<Vec<_>>(),
                });
                let job_obj = job_block.as_object_mut().expect("literal object");
                match &job.outcome {
                    Some(Ok(detail)) => {
                        job_obj.insert("ok".into(), true.into());
                        job_obj.insert("detail".into(), detail.clone().into());
                    }
                    Some(Err(error)) => {
                        job_obj.insert("ok".into(), false.into());
                        job_obj.insert("error".into(), error.clone().into());
                    }
                    None => {}
                }
                obj.insert("job".into(), job_block);
            }
        }
        block
    }

    // ── Job bookkeeping ──

    fn job_log(&self, line: impl Into<String>) {
        let mut line = line.into();
        truncate_on_boundary(&mut line, JOB_LOG_LINE_CAP);
        eprintln!("[update-lane] {line}");
        if let Ok(mut job) = self.job.lock() {
            if let Some(job) = job.as_mut() {
                if job.log.len() >= JOB_LOG_TAIL_LINES {
                    job.log.pop_front();
                }
                job.log.push_back(line);
            }
        }
    }

    fn set_phase(&self, phase: &str) {
        if let Ok(mut job) = self.job.lock() {
            if let Some(job) = job.as_mut() {
                job.phase = phase.to_string();
            }
        }
        self.job_log(format!("phase: {phase}"));
    }

    fn finish_job(&self, outcome: Result<String, String>) {
        let (id_suffix, title, text, urgency) = match &outcome {
            Ok(detail) => (
                "done",
                "Update produced",
                detail.clone(),
                crate::types::NotificationUrgency::Info,
            ),
            Err(error) => (
                "failed",
                "Update failed",
                error.clone(),
                crate::types::NotificationUrgency::Attention,
            ),
        };
        match &outcome {
            Ok(detail) => self.job_log(format!("done: {detail}")),
            Err(error) => self.job_log(format!("failed: {error}")),
        }
        if let Ok(mut job) = self.job.lock() {
            if let Some(job) = job.as_mut() {
                job.outcome = Some(outcome);
            }
        }
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.notify_user(
                &format!("update-lane-{id_suffix}-{}", super::now_ms()),
                Some(title),
                &text,
                urgency,
            );
        }
        self.job_running.store(false, Ordering::Release);
    }

    // ── Supervised children ──

    /// Run one supervised child: curated env, bounded runtime, stdout+
    /// stderr streamed line-by-line into the job log as they happen
    /// (ordinary visibility), kill-on-drop. Returns the captured stdout
    /// on success.
    async fn run_child(
        self: &Arc<Self>,
        label: &str,
        program: &str,
        args: Vec<String>,
        cwd: Option<&Path>,
        extra_env: &[&str],
        timeout: std::time::Duration,
    ) -> Result<String, String> {
        self.job_log(format!("$ {program} {}", args.join(" ")));
        let mut command = tokio::process::Command::new(program);
        command
            .args(&args)
            .env_clear()
            .envs(curated_env(extra_env))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("{label}: could not spawn {program}: {err}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(Self::pump_lines(Arc::clone(self), stdout, false));
        let stderr_task = tokio::spawn(Self::pump_lines(Arc::clone(self), stderr, true));
        let status = tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| {
                format!(
                    "{label}: timed out after {}s (child killed)",
                    timeout.as_secs()
                )
            })?
            .map_err(|err| format!("{label}: {err}"))?;
        let captured = stdout_task.await.ok().flatten().unwrap_or_default();
        let _ = stderr_task.await;
        if !status.success() {
            return Err(format!("{label}: {program} exited {status}"));
        }
        Ok(captured)
    }

    /// Stream a child pipe into the job log; returns the captured text
    /// (stdout only — callers parse it).
    async fn pump_lines(
        lane: Arc<UpdateLane>,
        pipe: Option<impl tokio::io::AsyncRead + Unpin>,
        is_stderr: bool,
    ) -> Option<String> {
        use tokio::io::AsyncBufReadExt as _;
        let pipe = pipe?;
        let mut lines = tokio::io::BufReader::new(pipe).lines();
        let mut captured = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if !is_stderr {
                captured.push_str(&line);
                captured.push('\n');
            }
            if !line.trim().is_empty() {
                lane.job_log(format!("  {}", line.trim_end()));
            }
        }
        Some(captured)
    }
}

// ── Public entry points (routes + wiring) ───────────────────────────

impl UpdateLane {
    /// Start a bounded check on `channel` (the install's native lane
    /// when unnamed) unless that channel's check is already running.
    /// Refuses (honestly) a channel this install cannot check.
    pub(crate) fn request_check(
        self: &Arc<Self>,
        channel: Option<UpdateChannel>,
    ) -> Result<serde_json::Value, String> {
        let channel = channel.unwrap_or_else(|| self.flavor.native_channel());
        if let Some(refusal) = check_refusal(&self.flavor, channel) {
            return Err(refusal);
        }
        {
            let Ok(mut slots) = self.check.lock() else {
                return Ok(self.status_block());
            };
            let slot = slots.slot_mut(channel);
            if slot.in_flight {
                return Ok(self.status_block());
            }
            slot.in_flight = true;
        }
        let lane = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = lane.run_check(channel).await;
            if let Ok(mut slots) = lane.check.lock() {
                let slot = slots.slot_mut(channel);
                slot.in_flight = false;
                slot.checked_at_ms = Some(super::now_ms());
                slot.outcome = Some(outcome);
            }
        });
        Ok(self.status_block())
    }

    /// Is a produce job running right now? The successor-exec lane
    /// refuses to exec an artifact a job may be mid-writing.
    pub(super) fn job_in_flight(&self) -> bool {
        self.job_running.load(Ordering::Acquire)
    }

    /// The owner's click: start the produce job on `channel` (the
    /// install's native lane when unnamed). Refuses (honestly) while one
    /// runs and on any channel/install mismatch — the guard runs before
    /// anything touches the network or the checkout.
    pub(crate) fn request_produce(
        self: &Arc<Self>,
        channel: Option<UpdateChannel>,
    ) -> Result<serde_json::Value, String> {
        let channel = channel.unwrap_or_else(|| self.flavor.native_channel());
        if let Some(refusal) = produce_refusal(&self.flavor, channel, cfg!(target_os = "macos")) {
            return Err(refusal);
        }
        if self
            .job_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("an update job is already running".to_string());
        }
        // The guard above pins channel↔flavor: Dev only passes on a
        // Source install, Releases only on a consumer app.
        let lane_kind = match channel {
            UpdateChannel::Dev => "source",
            UpdateChannel::Releases => "consumer",
        };
        if let Ok(mut job) = self.job.lock() {
            *job = Some(JobState {
                lane: lane_kind,
                phase: "starting".to_string(),
                started_ms: super::now_ms(),
                log: std::collections::VecDeque::new(),
                outcome: None,
            });
        }
        let lane = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = match lane.flavor.clone() {
                InstallFlavor::Source {
                    repo_root,
                    app_bundle,
                } => lane.produce_source(&repo_root, app_bundle).await,
                InstallFlavor::ConsumerApp { app_root } => lane.produce_consumer(&app_root).await,
                InstallFlavor::Unmanaged { .. } => unreachable!("refused above"),
            };
            lane.finish_job(outcome);
        });
        Ok(self.status_block())
    }

    // ── The check ──

    async fn run_check(self: &Arc<Self>, channel: UpdateChannel) -> Result<CheckOutcome, String> {
        match channel {
            UpdateChannel::Dev => match self.flavor.clone() {
                InstallFlavor::Source { repo_root, .. } => self
                    .source_check(&repo_root)
                    .await
                    .map(CheckOutcome::Source),
                // request_check refused already; kept for direct callers.
                other => Err(check_refusal(&other, UpdateChannel::Dev)
                    .unwrap_or_else(|| "no source checkout for the Dev channel".to_string())),
            },
            UpdateChannel::Releases => self.consumer_check().await,
        }
    }

    async fn source_check(self: &Arc<Self>, repo_root: &Path) -> Result<SourceCheck, String> {
        self.git(repo_root, &["fetch", "--quiet", "origin", "main"])
            .await?;
        let tip = self
            .git(repo_root, &["rev-parse", "--verify", "origin/main"])
            .await?;
        let running_sha = running_sha_for_compare();
        let base_sha = compare_base_sha(&running_sha).to_string();
        let range = format!("{base_sha}..origin/main");
        let max_count = format!("--max-count={BEHIND_COUNT_CAP}");
        let count = self
            .git(
                repo_root,
                &["rev-list", "--count", max_count.as_str(), range.as_str()],
            )
            .await
            .map_err(|err| {
                format!(
                    "could not count commits behind origin/main (is the running build's commit \
                     {base_sha} in this checkout's history?): {err}"
                )
            })?;
        // `--format=%h %s` sidesteps decoration/color config entirely —
        // the lines are data for the panel, newest first.
        let shortlog_max = format!("--max-count={SHORTLOG_MAX_LINES}");
        let shortlog = self
            .git(
                repo_root,
                &[
                    "log",
                    "--format=%h %s",
                    shortlog_max.as_str(),
                    range.as_str(),
                ],
            )
            .await?;
        let status = self.git(repo_root, &["status", "--porcelain"]).await?;
        fold_source_check(&tip, &count, &shortlog, &status)
    }

    /// A bounded git child — check-lane runs skip the job log (no job
    /// exists), produce-lane runs stream into it.
    async fn git(self: &Arc<Self>, repo_root: &Path, args: &[&str]) -> Result<String, String> {
        let mut argv = vec!["-C".to_string(), repo_root.display().to_string()];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        self.run_child(
            &format!("git {}", args.first().copied().unwrap_or_default()),
            "git",
            argv,
            None,
            CHILD_ENV_GIT,
            GIT_TIMEOUT,
        )
        .await
    }

    async fn consumer_check(self: &Arc<Self>) -> Result<CheckOutcome, String> {
        let report = self.verified_release(None).await?;
        let newer = release_version_newer(&report.version, crate::build_info::pkg_version());
        Ok(CheckOutcome::Consumer {
            tag: report.tag,
            version: report.version,
            newer,
        })
    }

    /// The consumer lane's verify seam: the shipped transparency-log
    /// release ritual (inclusion proof + signed tree head + append-only
    /// pin + PGP identity/coverage + GitHub metadata). Fail closed —
    /// any divergence refuses the whole lane.
    async fn verified_release(
        &self,
        tag: Option<&str>,
    ) -> Result<crate::hosted_verify::ReleaseVerifyReport, String> {
        let base = update_rendezvous_url()?;
        let github_api = url::Url::parse(crate::hosted_verify::GITHUB_API_BASE)
            .map_err(|err| format!("GitHub API base: {err}"))?;
        crate::hosted_verify::verify_hosted_release(
            &base,
            &github_api,
            crate::hosted_verify::DEFAULT_RELEASE_REPO,
            tag,
            false,
            &crate::platform::intendant_home(),
        )
        .await
        .map_err(|failure| match failure {
            crate::hosted_verify::VerifyFailure::Unavailable(detail) => {
                format!("release check unavailable: {detail}")
            }
            crate::hosted_verify::VerifyFailure::Verification {
                summary,
                mismatches,
            } => {
                format!(
                    "release verification FAILED: {summary}{}",
                    if mismatches.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", mismatches.join("; "))
                    }
                )
            }
        })
    }

    // ── Produce: source lane ──

    async fn produce_source(
        self: &Arc<Self>,
        repo_root: &Path,
        app_bundle: bool,
    ) -> Result<String, String> {
        self.set_phase("headroom");
        match headroom_gate(&probe_headroom().await) {
            Ok(Some(note)) => self.job_log(note),
            Ok(None) => {}
            Err(refusal) => return Err(refusal),
        }

        self.set_phase("pull");
        let status = self.git(repo_root, &["status", "--porcelain"]).await?;
        if tracked_dirty(&status) {
            return Err(
                "the checkout has local changes to tracked files — not pulling over them; \
                 commit or stash by hand, then retry"
                    .to_string(),
            );
        }
        self.git(repo_root, &["pull", "--ff-only", "origin", "main"])
            .await
            .map_err(|err| {
                format!(
                    "fast-forward pull of origin/main failed (diverged branch or detached \
                     checkout — reconcile by hand): {err}"
                )
            })?;

        self.set_phase("build");
        let produced = if app_bundle {
            if !cfg!(target_os = "macos") {
                return Err("app-bundle rebuilds only exist on macOS".to_string());
            }
            // Builds, signs with the stable local identity, and installs
            // to /Applications — the script's only install target, which
            // is also the watched path for a stamped installed app.
            self.run_child(
                "bundle",
                "bash",
                vec!["scripts/bundle-macos.sh".to_string()],
                Some(repo_root),
                CHILD_ENV_BUILD,
                BUILD_TIMEOUT,
            )
            .await?;
            self.exe_path.clone()
        } else {
            if let Some(mock_build) = mock_build_override() {
                // Rig lane (PROVIDER=mock only): the e2e legs cannot pay
                // for a real cargo build; the override script stands in
                // for it and must still land the artifact at the watched
                // path for the flow to count.
                self.run_child(
                    "build (mock)",
                    "bash",
                    vec![mock_build],
                    Some(repo_root),
                    CHILD_ENV_BUILD,
                    BUILD_TIMEOUT,
                )
                .await?;
            } else {
                self.run_child(
                    "build",
                    "cargo",
                    [
                        "build",
                        "--release",
                        "--bin",
                        "intendant",
                        "--bin",
                        "intendant-runtime",
                    ]
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect(),
                    Some(repo_root),
                    CHILD_ENV_BUILD,
                    BUILD_TIMEOUT,
                )
                .await?;
            }
            repo_root
                .join("target")
                .join("release")
                .join(format!("intendant{}", std::env::consts::EXE_SUFFIX))
        };

        self.set_phase("verify");
        let build = super::update_watch::run_version_probe(&produced)
            .await
            .map_err(|err| format!("the produced binary failed its --version probe: {err}"))?;
        Ok(format!(
            "built commit {} ({}) at {} — the update chip offers the swap once the watch \
             confirms it on disk",
            build.git_sha,
            build.version,
            produced.display(),
        ))
    }

    // ── Produce: consumer lane ──

    async fn produce_consumer(self: &Arc<Self>, app_root: &Path) -> Result<String, String> {
        self.set_phase("verify-manifest");
        let report = self.verified_release(None).await?;
        let (zip, asc) =
            select_release_asset(&report.artifacts, bundle_arch(std::env::consts::ARCH))
                .map(|(zip, asc)| (zip.clone(), asc.clone()))?;
        self.job_log(format!(
            "release {} verified against the transparency log ({} artifacts; signing key {})",
            report.tag, report.artifact_count, report.pgp_fingerprint
        ));

        let staging = crate::platform::intendant_home()
            .join("update-lane")
            .join(format!("staging-{}", report.tag.replace(['/', '\\'], "_")));
        let cleanup = StagingCleanup(staging.clone());
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|err| format!("staging dir: {err}"))?;

        self.set_phase("download");
        let zip_path = staging.join(&zip.name);
        let zip_sha = self.download_asset(&zip, &zip_path).await?;
        download_verdict(&zip.name, &zip.sha256, &zip_sha)?;
        let asc_path = staging.join(&asc.name);
        let asc_sha = self.download_asset(&asc, &asc_path).await?;
        download_verdict(&asc.name, &asc.sha256, &asc_sha)?;
        self.job_log(format!(
            "downloaded {} ({} bytes) + detached signature; both hash-match the log",
            zip.name, zip.size
        ));

        self.set_phase("pgp-verify");
        self.gpg_verify(&staging, &zip_path, &asc_path).await?;
        self.job_log(format!(
            "gpg VALIDSIG by the pinned release key {}",
            crate::pgp_identity::RELEASE_SIGNING_KEY_FINGERPRINT
        ));

        self.set_phase("install");
        let unpack_dir = staging.join("unpacked");
        std::fs::create_dir_all(&unpack_dir).map_err(|err| format!("unpack dir: {err}"))?;
        self.run_child(
            "unpack",
            "ditto",
            vec![
                "-x".to_string(),
                "-k".to_string(),
                zip_path.display().to_string(),
                unpack_dir.display().to_string(),
            ],
            None,
            &[],
            UNPACK_TIMEOUT,
        )
        .await?;
        let new_app = unpack_dir.join("Intendant.app");
        if !new_app.is_dir() {
            return Err("the release zip did not contain Intendant.app at its root".to_string());
        }
        let new_binary = new_app.join("Contents").join("MacOS").join("intendant-bin");
        let build = super::update_watch::run_version_probe(&new_binary)
            .await
            .map_err(|err| format!("the downloaded app failed its --version probe: {err}"))?;
        {
            // The swap is filesystem renames (a ditto copy on the rare
            // cross-volume staging) — blocking work off the executor.
            let new_app = new_app.clone();
            let app_root = app_root.to_path_buf();
            tokio::task::spawn_blocking(move || install_app_swap(&new_app, &app_root))
                .await
                .map_err(|err| format!("install task: {err}"))??;
        }
        drop(cleanup);
        Ok(format!(
            "release {} (commit {}, {}) installed at {} — the update chip offers the swap once \
             the watch confirms it on disk",
            report.tag,
            build.git_sha,
            build.version,
            app_root.display(),
        ))
    }

    async fn download_asset(
        &self,
        artifact: &crate::hosted_verify::ReleaseArtifactPlan,
        dest: &Path,
    ) -> Result<String, String> {
        let url = artifact
            .download_url
            .as_deref()
            .ok_or_else(|| format!("{}: GitHub exposes no download URL", artifact.name))?;
        let cap = if artifact.name.ends_with(".asc") {
            ASC_BYTE_CAP
        } else {
            // The log commits the exact size; pad for safety margin only.
            usize::try_from(artifact.size)
                .unwrap_or(usize::MAX)
                .saturating_add(1024)
        };
        crate::hosted_verify::download_release_asset_to_file(url, dest, cap).await
    }

    /// The PGP leg: throwaway GNUPGHOME, import ONLY the compiled-in
    /// release key, verify the detached signature. gpg missing = fail
    /// closed (this lane's whole point is the independent signature
    /// check).
    async fn gpg_verify(
        self: &Arc<Self>,
        staging: &Path,
        artifact: &Path,
        signature: &Path,
    ) -> Result<(), String> {
        let gnupg_home = staging.join("gnupg");
        std::fs::create_dir_all(&gnupg_home).map_err(|err| format!("gnupg home: {err}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&gnupg_home, std::fs::Permissions::from_mode(0o700));
        }
        let key_path = staging.join("release-signing-key.asc");
        std::fs::write(&key_path, crate::pgp_identity::RELEASE_SIGNING_PUBKEY_ASC)
            .map_err(|err| format!("write pinned key: {err}"))?;
        let home = gnupg_home.display().to_string();
        self.run_child(
            "gpg import",
            "gpg",
            vec![
                "--batch".to_string(),
                "--homedir".to_string(),
                home.clone(),
                "--import".to_string(),
                key_path.display().to_string(),
            ],
            None,
            &[],
            GPG_TIMEOUT,
        )
        .await
        .map_err(|err| {
            format!(
                "{err} — the consumer lane needs gnupg for the signature check \
                 (macOS: brew install gnupg); refusing to install without it"
            )
        })?;
        let status_out = self
            .run_child(
                "gpg verify",
                "gpg",
                vec![
                    "--batch".to_string(),
                    "--homedir".to_string(),
                    home,
                    "--status-fd".to_string(),
                    "1".to_string(),
                    "--verify".to_string(),
                    signature.display().to_string(),
                    artifact.display().to_string(),
                ],
                None,
                &[],
                GPG_TIMEOUT,
            )
            .await;
        match status_out {
            Ok(out) => gpg_verify_verdict(
                true,
                &out,
                crate::pgp_identity::RELEASE_SIGNING_KEY_FINGERPRINT,
            ),
            Err(err) => {
                // Exit != 0 IS the fail-closed verdict; keep the child's
                // words for the log.
                self.job_log(err);
                gpg_verify_verdict(
                    false,
                    "",
                    crate::pgp_identity::RELEASE_SIGNING_KEY_FINGERPRINT,
                )
            }
        }
    }
}

/// Render one channel's check slot for the status payload.
fn check_state_block(check: &CheckState) -> serde_json::Value {
    let mut block = serde_json::json!({
        "in_flight": check.in_flight,
    });
    let obj = block.as_object_mut().expect("literal object");
    if let Some(at) = check.checked_at_ms {
        obj.insert("checked_at_ms".into(), at.into());
    }
    match &check.outcome {
        Some(Ok(CheckOutcome::Source(source))) => {
            obj.insert("tip_sha".into(), source.tip_sha.clone().into());
            obj.insert("behind".into(), source.behind.into());
            obj.insert("behind_capped".into(), source.behind_capped.into());
            obj.insert("shortlog".into(), source.shortlog.clone().into());
            obj.insert("dirty".into(), source.dirty.into());
        }
        Some(Ok(CheckOutcome::Consumer {
            tag,
            version,
            newer,
        })) => {
            obj.insert("latest_tag".into(), tag.clone().into());
            obj.insert("latest_version".into(), version.clone().into());
            match newer {
                Some(newer) => {
                    obj.insert("behind".into(), u32::from(*newer).into());
                }
                None => {
                    obj.insert(
                        "compare_error".into(),
                        "release and running versions are not comparable".into(),
                    );
                }
            }
        }
        Some(Err(error)) => {
            obj.insert("error".into(), error.clone().into());
        }
        None => {}
    }
    block
}

/// Delete the staging tree on drop — a failed job never leaves
/// unverified bytes behind. Success paths drop it too: the artifact has
/// been installed; the staging copy is done.
struct StagingCleanup(PathBuf);
impl Drop for StagingCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Swap the verified new app into place: stage it beside the target
/// (same volume, so renames are atomic), keep the old bundle as
/// `<name>.app.previous`, roll back on a failed final rename. The
/// running processes keep their open inodes; the update watch sees the
/// path flip and takes it from there.
fn install_app_swap(new_app: &Path, app_root: &Path) -> Result<(), String> {
    let parent = app_root
        .parent()
        .ok_or_else(|| "the installed app has no parent directory".to_string())?;
    let file_name = app_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the installed app has no usable name".to_string())?;
    let staged = parent.join(format!(".{file_name}.update-{}", std::process::id()));
    let previous = parent.join(format!("{file_name}.previous"));
    let _ = std::fs::remove_dir_all(&staged);
    if std::fs::rename(new_app, &staged).is_err() {
        // Cross-volume staging: fall back to a bundle-faithful copy.
        let status = std::process::Command::new("ditto")
            .arg(new_app)
            .arg(&staged)
            .status()
            .map_err(|err| format!("ditto copy into {}: {err}", parent.display()))?;
        if !status.success() {
            return Err(format!(
                "ditto copy into {} exited {status}",
                parent.display()
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::rename(app_root, &previous)
        .map_err(|err| format!("could not set the running app aside: {err}"))?;
    if let Err(err) = std::fs::rename(&staged, app_root) {
        // Roll the old app back so the machine is never left app-less.
        let _ = std::fs::rename(&previous, app_root);
        let _ = std::fs::remove_dir_all(&staged);
        return Err(format!("could not move the new app into place: {err}"));
    }
    Ok(())
}

/// Rig knob (PROVIDER=mock only): a script standing in for the cargo
/// build. Mirrors the `INTENDANT_UPDATE_WATCH_PATH` fail-closed shape.
fn mock_build_override() -> Option<String> {
    let script = std::env::var("INTENDANT_UPDATE_LANE_BUILD_CMD").ok()?;
    if std::env::var("PROVIDER").as_deref() == Ok("mock") {
        return Some(script);
    }
    eprintln!(
        "[update-lane] INTENDANT_UPDATE_LANE_BUILD_CMD ignored: PROVIDER=mock is not set \
         (the override is a mock-rig knob, never a production redirect)"
    );
    None
}

/// The running build's commit as every update surface compares it: the
/// compiled-in provenance, except under the PROVIDER=mock rig knob
/// below. Shared by the source-lane behind compare, the update watch's
/// same-build suppression, and the successor-exec lane's build-neutral
/// refusal — one answer to "what is running", never three.
pub(super) fn running_sha_for_compare() -> String {
    mock_running_sha_override().unwrap_or_else(|| crate::build_info::git_sha().to_string())
}

/// The git revision the behind compare runs against: a dirty-tree build
/// stamps `<sha>-dirty` (build.rs provenance), which is not a revision
/// `git rev-list` can resolve — the compare uses the underlying commit,
/// and the `-dirty` marker stays a display fact on the running
/// provenance everywhere it renders.
pub(crate) fn compare_base_sha(running_sha: &str) -> &str {
    running_sha.strip_suffix("-dirty").unwrap_or(running_sha)
}

/// Rig knob (PROVIDER=mock only): the "running" commit for the compare.
/// The e2e fixture repos cannot contain the test binary's compiled-in
/// sha, so the rig injects one that is; production always compares the
/// real `build_info` provenance.
fn mock_running_sha_override() -> Option<String> {
    let sha = std::env::var("INTENDANT_UPDATE_LANE_RUNNING_SHA").ok()?;
    if std::env::var("PROVIDER").as_deref() == Ok("mock") {
        return Some(sha);
    }
    eprintln!(
        "[update-lane] INTENDANT_UPDATE_LANE_RUNNING_SHA ignored: PROVIDER=mock is not set \
         (the override is a mock-rig knob, never a production redirect)"
    );
    None
}

/// The rendezvous the consumer lane verifies against: the live Connect
/// configuration when one is set, else the env override, else the
/// hosted default — the same ladder as `intendant hosted-verify`.
fn update_rendezvous_url() -> Result<url::Url, String> {
    let configured = crate::connect_rendezvous::status_snapshot()
        .rendezvous_url
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty());
    let raw = configured
        .or_else(|| {
            std::env::var("INTENDANT_CONNECT_RENDEZVOUS_URL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| crate::project::DEFAULT_CONNECT_RENDEZVOUS_URL.to_string());
    url::Url::parse(&raw).map_err(|err| format!("rendezvous URL {raw:?}: {err}"))
}

/// Delay before the boot check, and the standing cadence after it. The
/// check never produces anything — behind-ness only; producing takes
/// the owner's click.
const BOOT_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(180);
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Wire the lane onto the runtime and start the gentle check cadence.
/// Spawned beside the update watch; shares its watched-path resolution
/// (and therefore its mock-rig override).
pub(crate) fn spawn_update_lane(runtime: &Arc<super::HandoverRuntime>) {
    let Some(exe_path) = super::update_watch::watched_binary_path() else {
        eprintln!("[update-lane] current_exe unresolvable — update lane off for this boot");
        return;
    };
    let lane = Arc::new(UpdateLane::new(runtime, exe_path));
    runtime.set_update_lane(lane.clone());
    tokio::spawn(async move {
        tokio::time::sleep(BOOT_CHECK_DELAY).await;
        loop {
            // The AUTOMATIC cadence checks the install's NATIVE channel,
            // following the hosted-verify tripwire's posture: a releases
            // check reaches the rendezvous + GitHub, so it only runs
            // unprompted when the owner has Connect configured; a source
            // install fetches the owner's own origin. An unmanaged
            // install has nothing to check unprompted — the panel says
            // why instead. The panel's check click always may.
            let auto = match &lane.flavor {
                InstallFlavor::Source { .. } => true,
                InstallFlavor::ConsumerApp { .. } => {
                    crate::connect_rendezvous::status_snapshot().configured
                }
                InstallFlavor::Unmanaged { .. } => false,
            };
            if auto {
                let _ = lane.request_check(None);
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The pinned compare seam ──

    /// Commission pin: the behind-main compare folds the bounded git
    /// observations — tip, capped count, shortlog, dirtiness — and
    /// refuses unparseable output instead of guessing. Dirty means
    /// TRACKED modifications: an untracked `target/` or scratch file
    /// never blocks the lane.
    #[test]
    fn source_compare_folds_bounded_git_observations() {
        let check = fold_source_check(
            "3e4c79f8aa",
            "3\n",
            "abc1234 fix: one thing\nabc2222 feat: another\n",
            "?? target/\n?? notes.txt\n",
        )
        .expect("clean fold");
        assert_eq!(check.tip_sha, "3e4c79f8aa");
        assert_eq!(check.behind, 3);
        assert!(!check.behind_capped);
        assert_eq!(
            check.shortlog,
            vec!["abc1234 fix: one thing", "abc2222 feat: another"],
            "the shortlog rides the verdict as data lines"
        );
        assert!(!check.dirty, "untracked files are not dirtiness");

        let capped =
            fold_source_check("abcdef012345", "500\n", "", "?? target/\n M src/main.rs\n").unwrap();
        assert_eq!(capped.behind, BEHIND_COUNT_CAP);
        assert!(capped.behind_capped, "cap reached reads as 500+");
        assert!(capped.dirty, "a tracked modification is");
        assert!(tracked_dirty("A  staged.rs\n"), "staged changes count too");

        assert!(fold_source_check("fatal: bad revision", "0", "", "").is_err());
        assert!(fold_source_check("abcdef012345", "many", "", "").is_err());
    }

    /// The dirty-build auto-check fix (release-availability card): a
    /// `<sha>-dirty` provenance stamp folds to its underlying commit
    /// for the rev-list range — `<sha>-dirty..origin/main` is not a git
    /// revision, and it used to fail every auto compare on a
    /// dirty-tree build.
    #[test]
    fn dirty_build_stamp_folds_to_a_real_revision_for_the_compare() {
        assert_eq!(compare_base_sha("3e4c79f8-dirty"), "3e4c79f8");
        assert_eq!(compare_base_sha("3e4c79f8"), "3e4c79f8");
        assert_eq!(compare_base_sha("unknown"), "unknown");
    }

    /// The shortlog fold is bounded in count and width, and the width
    /// cap lands on a char boundary (commit subjects are unicode).
    #[test]
    fn shortlog_fold_is_bounded() {
        let many = (0..40)
            .map(|n| format!("sha{n} subject {n}\n"))
            .collect::<String>();
        assert_eq!(fold_shortlog(&many).len(), SHORTLOG_MAX_LINES);

        let wide = format!("abc1234 {}", "é".repeat(SHORTLOG_LINE_CAP));
        let folded = fold_shortlog(&wide);
        assert_eq!(folded.len(), 1);
        assert!(folded[0].len() <= SHORTLOG_LINE_CAP);
        assert!(folded[0].starts_with("abc1234 "), "truncated, not mangled");

        assert!(fold_shortlog("\n  \n").is_empty(), "blank lines drop out");
    }

    // ── The channel vocabulary (the front door) ──

    /// Commission pin: exactly two channels, and the catalog derives
    /// availability + honest reasons from the install flavor — the
    /// panel never hardcodes which lane an install can use.
    #[test]
    fn channel_catalog_derives_availability_and_reasons() {
        let source = InstallFlavor::Source {
            repo_root: PathBuf::from("/checkout"),
            app_bundle: false,
        };
        let catalog = channel_catalog(&source, true);
        assert_eq!(catalog["dev"]["check"], true);
        assert_eq!(catalog["dev"]["produce"], true);
        assert_eq!(
            catalog["releases"]["check"], true,
            "release data is honest anywhere"
        );
        assert_eq!(catalog["releases"]["produce"], false);
        assert!(
            catalog["releases"]["reason"]
                .as_str()
                .unwrap()
                .contains("packaged macOS app"),
            "{catalog}"
        );

        let consumer = InstallFlavor::ConsumerApp {
            app_root: PathBuf::from("/Applications/Intendant.app"),
        };
        let catalog = channel_catalog(&consumer, true);
        assert_eq!(catalog["releases"]["produce"], true);
        assert_eq!(catalog["dev"]["check"], false);
        assert_eq!(catalog["dev"]["produce"], false);
        assert!(
            catalog["dev"]["reason"]
                .as_str()
                .unwrap()
                .contains("no source checkout"),
            "{catalog}"
        );
        let off_macos = channel_catalog(&consumer, false);
        assert_eq!(off_macos["releases"]["produce"], false);
        assert!(
            off_macos["releases"]["reason"]
                .as_str()
                .unwrap()
                .contains("only the macOS app"),
            "{off_macos}"
        );

        let unmanaged = InstallFlavor::Unmanaged {
            reason: "stray binary".to_string(),
        };
        let catalog = channel_catalog(&unmanaged, true);
        assert_eq!(catalog["releases"]["produce"], false);
        assert_eq!(catalog["dev"]["produce"], false);
        assert!(
            catalog["dev"]["reason"]
                .as_str()
                .unwrap()
                .contains("stray binary"),
            "{catalog}"
        );
    }

    /// The channel argument parser: absent means the native lane,
    /// the two names parse, anything else is refused by name.
    #[test]
    fn channel_arg_parses_the_two_channel_vocabulary() {
        assert_eq!(parse_channel_arg(""), Ok(None));
        assert_eq!(parse_channel_arg("{}"), Ok(None));
        assert_eq!(parse_channel_arg("not json"), Ok(None));
        assert_eq!(
            parse_channel_arg("{\"channel\":\"releases\"}"),
            Ok(Some(UpdateChannel::Releases))
        );
        assert_eq!(
            parse_channel_arg("{\"channel\":\"dev\"}"),
            Ok(Some(UpdateChannel::Dev))
        );
        let refusal = parse_channel_arg("{\"channel\":\"nightly\"}").unwrap_err();
        assert!(refusal.contains("nightly"), "{refusal}");
        assert!(refusal.contains("exactly two"), "{refusal}");
    }

    /// A produce click on the wrong channel for the install refuses by
    /// name BEFORE any network or checkout touch, and the native
    /// channel resolves per flavor.
    #[test]
    fn produce_refusals_pin_channel_to_flavor() {
        let source = InstallFlavor::Source {
            repo_root: PathBuf::from("/checkout"),
            app_bundle: false,
        };
        assert_eq!(source.native_channel(), UpdateChannel::Dev);
        assert!(produce_refusal(&source, UpdateChannel::Dev, true).is_none());
        let cross = produce_refusal(&source, UpdateChannel::Releases, true).unwrap();
        assert!(cross.contains("/checkout"), "{cross}");

        let bundle = InstallFlavor::Source {
            repo_root: PathBuf::from("/checkout"),
            app_bundle: true,
        };
        let cross = produce_refusal(&bundle, UpdateChannel::Releases, true).unwrap();
        assert!(cross.contains("source build"), "{cross}");

        let consumer = InstallFlavor::ConsumerApp {
            app_root: PathBuf::from("/Applications/Intendant.app"),
        };
        assert_eq!(consumer.native_channel(), UpdateChannel::Releases);
        assert!(produce_refusal(&consumer, UpdateChannel::Releases, true).is_none());
        assert!(produce_refusal(&consumer, UpdateChannel::Dev, true)
            .unwrap()
            .contains("no source checkout"));

        let unmanaged = InstallFlavor::Unmanaged {
            reason: "stray".to_string(),
        };
        for channel in [UpdateChannel::Releases, UpdateChannel::Dev] {
            assert!(produce_refusal(&unmanaged, channel, true)
                .unwrap()
                .contains("no update lane"));
        }
    }

    /// Commission pin: release-vs-running is a semver compare that
    /// refuses to guess on unparseable versions.
    #[test]
    fn consumer_compare_is_honest_semver() {
        assert_eq!(release_version_newer("v0.2.0", "0.1.0"), Some(true));
        assert_eq!(release_version_newer("v0.1.0", "0.1.0"), Some(false));
        assert_eq!(release_version_newer("0.1.0", "0.2.0"), Some(false));
        assert_eq!(release_version_newer("v1.0.0-rc.1", "0.9.9"), Some(true));
        assert_eq!(release_version_newer("nightly", "0.1.0"), None);
        assert_eq!(release_version_newer("v0.1.0", "unknown"), None);
    }

    // ── The headroom gate (the capacity linkage) ──

    /// Commission pin: the produce job never builds under memory
    /// pressure — `Low` refuses, `Unknown` proceeds with the note.
    #[test]
    fn headroom_gate_refuses_pressure() {
        assert!(headroom_gate(&Headroom::Ok("fine".into())).is_ok());
        let unknown = headroom_gate(&Headroom::Unknown("no probe".into())).unwrap();
        assert!(unknown.unwrap().contains("no probe"));
        let refusal = headroom_gate(&Headroom::Low("level 4".into())).unwrap_err();
        assert!(refusal.contains("memory pressure"));
        assert!(refusal.contains("level 4"));
    }

    #[test]
    fn headroom_probes_fold_platform_output() {
        assert!(matches!(fold_macos_pressure("1\n"), Headroom::Ok(_)));
        assert!(matches!(fold_macos_pressure("2"), Headroom::Low(_)));
        assert!(matches!(fold_macos_pressure("4"), Headroom::Low(_)));
        assert!(matches!(fold_macos_pressure("??"), Headroom::Unknown(_)));

        let low = fold_linux_meminfo("MemTotal: 16 kB\nMemAvailable: 1048576 kB\n");
        assert!(matches!(low, Headroom::Low(_)), "1 GiB is under the floor");
        let ok = fold_linux_meminfo("MemAvailable: 8388608 kB\n");
        assert!(matches!(ok, Headroom::Ok(_)));
        assert!(matches!(
            fold_linux_meminfo("MemTotal: 1 kB\n"),
            Headroom::Unknown(_)
        ));
    }

    // ── The pinned verify seams (fail closed) ──

    fn plan(name: &str, sha: &str) -> crate::hosted_verify::ReleaseArtifactPlan {
        crate::hosted_verify::ReleaseArtifactPlan {
            name: name.to_string(),
            sha256: sha.to_string(),
            size: 10,
            download_url: Some(format!("https://example.invalid/{name}")),
        }
    }

    /// Commission pin: the consumer lane installs only a release-shaped,
    /// signed macOS artifact for THIS machine's arch — dev/unnotarized
    /// suffixes and signature-less artifacts are refused.
    #[test]
    fn release_asset_selection_fails_closed() {
        let artifacts = vec![
            plan("Intendant-v0.1.0-macos-arm64.zip", "aa"),
            plan("Intendant-v0.1.0-macos-arm64.zip.asc", "bb"),
            plan("install.sh", "cc"),
        ];
        let (zip, asc) = select_release_asset(&artifacts, "arm64").expect("selects the app zip");
        assert_eq!(zip.name, "Intendant-v0.1.0-macos-arm64.zip");
        assert_eq!(asc.name, "Intendant-v0.1.0-macos-arm64.zip.asc");

        assert!(
            select_release_asset(&artifacts, "x86_64").is_err(),
            "no artifact for another arch"
        );
        let unsigned = vec![plan("Intendant-v0.1.0-macos-arm64-unsigned-dev.zip", "aa")];
        assert!(
            select_release_asset(&unsigned, "arm64").is_err(),
            "dev-suffixed artifacts never install"
        );
        let missing_asc = vec![plan("Intendant-v0.1.0-macos-arm64.zip", "aa")];
        assert!(
            select_release_asset(&missing_asc, "arm64").is_err(),
            "an artifact without its .asc is refused"
        );
        assert_eq!(bundle_arch("aarch64"), "arm64");
        assert_eq!(bundle_arch("x86_64"), "x86_64");
    }

    /// Commission pin (verify seam): downloaded bytes must hash to the
    /// LOG's committed sha — anything else refuses the install.
    #[test]
    fn download_verdict_fails_closed_on_hash_mismatch() {
        assert!(download_verdict("a.zip", "aabbcc", "aabbcc").is_ok());
        let err =
            download_verdict("a.zip", "aabbccddeeff00112233", "deadbeefdeadbeefdead").unwrap_err();
        assert!(err.contains("refusing to install"));
        assert!(err.contains("a.zip"));
    }

    /// Commission pin (verify seam): the PGP verdict requires a gpg
    /// VALIDSIG whose PRIMARY fingerprint is the compiled-in pin —
    /// success exit alone, GOODSIG alone, or a foreign key all refuse.
    #[test]
    fn gpg_verdict_requires_validsig_by_the_pinned_primary() {
        const PIN: &str = "A9B389C058DD177B3303A13522FC08F0A26D3D18";
        let status = format!(
            "[GNUPG:] NEWSIG\n[GNUPG:] GOODSIG 22FC08F0A26D3D18 Intendant Release Signing\n\
             [GNUPG:] VALIDSIG 1111222233334444555566667777888899990000 2026-07-30 1785000000 0 4 0 1 8 00 {PIN}\n"
        );
        assert!(gpg_verify_verdict(true, &status, PIN).is_ok());

        assert!(
            gpg_verify_verdict(false, &status, PIN).is_err(),
            "a failing gpg exit refuses regardless of output"
        );
        assert!(
            gpg_verify_verdict(true, "[GNUPG:] GOODSIG 22FC08F0A26D3D18 x\n", PIN).is_err(),
            "GOODSIG without VALIDSIG refuses"
        );
        let foreign = "[GNUPG:] VALIDSIG 1111 2026-07-30 1 0 4 0 1 8 00 \
                       BBBB389C058DD177B3303A13522FC08F0A26D3D1\n";
        assert!(
            gpg_verify_verdict(true, foreign, PIN).is_err(),
            "a VALIDSIG by another primary key refuses"
        );
    }

    // ── Flavor detection (hermetic) ──

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn flavor_detects_source_checkout_release_binary() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        touch(&repo.join("Cargo.toml"));
        touch(&repo.join("scripts").join("bundle-macos.sh"));
        let exe = repo
            .join("target")
            .join("release")
            .join(format!("intendant{}", std::env::consts::EXE_SUFFIX));
        touch(&exe);
        assert_eq!(
            detect_install_flavor(&exe),
            InstallFlavor::Source {
                repo_root: repo.clone(),
                app_bundle: false
            }
        );
        // A debug binary is inside the checkout but NOT the watched
        // release output: unmanaged, with the reason said out loud.
        let debug_exe = repo.join("target").join("debug").join("intendant");
        touch(&debug_exe);
        match detect_install_flavor(&debug_exe) {
            InstallFlavor::Unmanaged { reason } => {
                assert!(reason.contains("target/release"))
            }
            other => panic!("expected unmanaged, got {other:?}"),
        }
    }

    #[test]
    fn flavor_detects_app_bundles_by_source_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("checkout");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        touch(&repo.join("Cargo.toml"));
        touch(&repo.join("scripts").join("bundle-macos.sh"));

        let app = dir.path().join("Applications").join("Intendant.app");
        let exe = app.join("Contents").join("MacOS").join("intendant-bin");
        touch(&exe);

        // No stamp: a consumer install.
        assert_eq!(
            detect_install_flavor(&exe),
            InstallFlavor::ConsumerApp {
                app_root: app.clone()
            }
        );

        // A valid stamp names the checkout: the source lane, app shape.
        touch(
            &app.join("Contents")
                .join("Resources")
                .join(SOURCE_STAMP_RESOURCE),
        );
        std::fs::write(
            app.join("Contents")
                .join("Resources")
                .join(SOURCE_STAMP_RESOURCE),
            format!("{}\n", repo.display()),
        )
        .unwrap();
        assert_eq!(
            detect_install_flavor(&exe),
            InstallFlavor::Source {
                repo_root: repo.clone(),
                app_bundle: true
            }
        );

        // A stamp whose path is gone (a release app carrying its CI
        // runner's checkout) falls down to the consumer lane.
        std::fs::write(
            app.join("Contents")
                .join("Resources")
                .join(SOURCE_STAMP_RESOURCE),
            "/nonexistent/ci/checkout\n",
        )
        .unwrap();
        assert_eq!(
            detect_install_flavor(&exe),
            InstallFlavor::ConsumerApp {
                app_root: app.clone()
            }
        );

        let stray = dir.path().join("bin").join("intendant");
        touch(&stray);
        assert!(matches!(
            detect_install_flavor(&stray),
            InstallFlavor::Unmanaged { .. }
        ));
    }

    /// Commission pin: the shipped dashboard bundle carries the Daemon
    /// update panel — the Access→Daemons mount, the two-channel
    /// vocabulary (Releases default, Dev behind Advanced), both consent
    /// buttons, the action POSTs, and the swap-step composition hook —
    /// so the front door cannot be silently gutted from the SPA (the
    /// HS6 artifact-scan pattern).
    #[test]
    fn spa_carries_the_update_lane_panel() {
        let app = include_str!("../../../../static/app.html");
        for needle in [
            "update-lane-card",
            "Releases — verified, signed builds (default)",
            "Dev — build from main",
            "Pull & build from main",
            "Download & install release",
            "Fetch & compare",
            "update-lane-advanced",
            "update-lane-shortlog",
            "/api/daemon/update-lane/produce",
            "/api/daemon/update-lane/check",
            "update_lane",
            "intendantHandoverUpdate",
            "renderSwapSection",
            "Open the update panel",
            "nothing installs or restarts automatically",
        ] {
            assert!(
                app.contains(needle),
                "the dashboard bundle lost the update-channels panel wiring: {needle}"
            );
        }
    }

    /// An unmanaged install refuses the click with the reason on the
    /// block — the consent surface never offers a button it cannot
    /// honor, and the refusal releases the single-flight latch.
    #[tokio::test]
    async fn unmanaged_flavor_refuses_produce_honestly() {
        let home = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            0,
            0,
        ));
        let exe = home.path().join("bin").join("intendant");
        touch(&exe);
        let lane = Arc::new(UpdateLane::new(&runtime, exe));
        let block = lane.status_block();
        assert_eq!(block["flavor"], "unmanaged");
        assert!(block["unavailable"].is_string(), "{block}");
        assert!(block["running"]["git_sha"].is_string());
        assert_eq!(block["default_channel"], "releases");
        assert_eq!(block["channels"]["dev"]["produce"], false, "{block}");
        assert_eq!(block["checks"]["releases"]["in_flight"], false, "{block}");
        let refusal = lane.request_produce(None).unwrap_err();
        assert!(refusal.contains("no update lane"), "{refusal}");
        assert!(
            !lane.job_running.load(Ordering::Acquire),
            "a refused click leaves the single-flight latch free"
        );
        let refusal = lane.request_check(Some(UpdateChannel::Dev)).unwrap_err();
        assert!(
            refusal.contains("no source checkout or app bundle"),
            "a dev check on an unmanaged install refuses with the flavor's reason: {refusal}"
        );
    }

    // ── The install swap ──

    #[test]
    fn app_swap_keeps_previous_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("Applications");
        let app = parent.join("Intendant.app");
        touch(&app.join("Contents").join("MacOS").join("old-marker"));
        let new_app = dir.path().join("staging").join("Intendant.app");
        touch(&new_app.join("Contents").join("MacOS").join("new-marker"));

        install_app_swap(&new_app, &app).expect("swap succeeds");
        assert!(app
            .join("Contents")
            .join("MacOS")
            .join("new-marker")
            .is_file());
        assert!(parent
            .join("Intendant.app.previous")
            .join("Contents")
            .join("MacOS")
            .join("old-marker")
            .is_file());
    }
}
