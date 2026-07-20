//! `<permit_dir>/governor.log`: one compile-permit acquisition line per
//! governed invocation, plus linker acquisition/completion lines for each
//! heavyweight final artifact. Bypasses, probes, and fail-open runs are
//! deliberately silent — the log answers which phase waited, for how long,
//! and on which resource. The probe fast path must not pay even a log write.
//!
//! Best-effort end to end: logging must never fail the build, so every I/O
//! error here is swallowed. Rotation is truncate-in-place at 1MB keeping
//! the last 256KB — the scripts/ci hooks/watchdog doctrine: governed
//! accounts can write the pre-created 0666 file but cannot create siblings
//! in the root-owned permit dir, so tmp+rename is off the table. Racing
//! rotators are serialized by a non-blocking flock on the log itself
//! (losers skip; worst case a line lands mid-rotation and is lost — a
//! tolerated cost, same as the hooks log).

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::flock;

pub(crate) const LOG_NAME: &str = "governor.log";
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const KEEP_BYTES: u64 = 256 * 1024;

/// How the link gate treated a heavyweight invocation (ordinary compiles
/// carry `None` and log `kind=compile`).
pub(crate) enum LinkDisposition<'a> {
    /// Serialized through a held slot.
    Gated {
        slot: &'a str,
        link_wait_ms: u64,
        queue: &'a str,
        scope: &'a str,
    },
    /// `link_slots = 0`: gated off by configuration.
    Off,
    /// No usable slot file: gating degraded, ordinary governance kept.
    Degraded,
}

pub(crate) fn log_compile(
    permit_dir: &Path,
    class: &str,
    crate_name: Option<&str>,
    link_deferred: bool,
    permit_name: &str,
    wait_ms: u64,
) {
    let deferred = if link_deferred { " link=deferred" } else { "" };
    append_line(
        permit_dir,
        &format!(
            "class={class} crate={} kind=compile{deferred} permit={permit_name} wait_ms={wait_ms}",
            printable_crate(crate_name),
        ),
    );
}

pub(crate) fn log_link(
    permit_dir: &Path,
    class: &str,
    crate_name: Option<&str>,
    disposition: &LinkDisposition<'_>,
    permit_name: &str,
    permit_wait_ms: u64,
) {
    let kind = match disposition {
        LinkDisposition::Gated {
            slot,
            link_wait_ms,
            queue,
            scope,
        } => format!(
            "kind=link link_slot={slot} link_wait_ms={link_wait_ms} queue={queue} scope={scope}"
        ),
        LinkDisposition::Off => "kind=link-ungated reason=off".to_string(),
        LinkDisposition::Degraded => "kind=link-ungated reason=degraded".to_string(),
    };
    append_line(
        permit_dir,
        &format!(
            "class={class} crate={} {kind} permit={permit_name} wait_ms={permit_wait_ms}",
            printable_crate(crate_name),
        ),
    );
}

/// Completion line for every heavyweight link (gated or not). In normal
/// shim operation this is actual linker wall time, not rustc compile time.
pub(crate) fn log_link_done(
    permit_dir: &Path,
    crate_name: Option<&str>,
    runtime_ms: u64,
    scope: &str,
) {
    append_line(
        permit_dir,
        &format!(
            "crate={} kind=link-done runtime_ms={runtime_ms} scope={scope}",
            printable_crate(crate_name),
        ),
    );
}

/// Crate names come from argv: keep the log single-line and greppable
/// whatever a hand-run invocation carries (`-` when absent or unusable).
fn printable_crate(crate_name: Option<&str>) -> String {
    let cleaned: String = crate_name
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "-".to_string()
    } else {
        cleaned
    }
}

fn append_line(permit_dir: &Path, rest: &str) {
    let path = permit_dir.join(LOG_NAME);
    rotate_if_oversized(&path);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{} pid={} {rest}\n", iso8601_utc(secs), std::process::id());
    if let Ok(mut f) = OpenOptions::new().append(true).create(true).open(&path) {
        // One short O_APPEND write per invocation: atomic per line.
        let _ = f.write_all(line.as_bytes());
    }
}

fn rotate_if_oversized(path: &Path) {
    let Ok(mut f) = OpenOptions::new().read(true).write(true).open(path) else {
        return;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return;
    };
    if len <= MAX_LOG_BYTES {
        return;
    }
    // Whoever wins the probe rotates; everyone else skips this round.
    if !flock::try_lock_exclusive(&f) {
        return;
    }
    let mut tail = vec![0_u8; KEEP_BYTES as usize];
    let read_ok =
        f.seek(SeekFrom::End(-(KEEP_BYTES as i64))).is_ok() && f.read_exact(&mut tail).is_ok();
    if read_ok {
        // Cut to the first line boundary inside the kept tail.
        let cut = tail
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        // Concurrent O_APPEND writers may interleave lines while the tail
        // is being written back; bounded and tolerated.
        if f.set_len(0).is_ok() && f.seek(SeekFrom::Start(0)).is_ok() {
            let _ = f.write_all(&tail[cut..]);
        }
    }
    flock::unlock(&f);
}

/// Seconds-since-epoch → `2026-07-10T21:15:04Z`, no chrono/time dependency.
/// Days→civil conversion is the classic Euclidean-affine algorithm
/// (Howard Hinnant's `civil_from_days`).
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_known_timestamps() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn log_line_appends_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        log_compile(
            dir.path(),
            "local",
            Some("serde"),
            false,
            "permit-ci-1",
            230,
        );
        let text = std::fs::read_to_string(dir.path().join(LOG_NAME)).unwrap();
        let line = text.trim_end();
        assert!(
            line.ends_with("class=local crate=serde kind=compile permit=permit-ci-1 wait_ms=230"),
            "{line}"
        );
        assert!(line.contains(&format!("pid={}", std::process::id())));
        assert!(line.starts_with("20"), "timestamp first: {line}");
    }

    #[test]
    fn link_lines_carry_the_soak_fields() {
        let dir = tempfile::tempdir().unwrap();
        log_compile(
            dir.path(),
            "local",
            Some("intendant"),
            true,
            "permit-local-0",
            5,
        );
        log_link(
            dir.path(),
            "local",
            Some("intendant"),
            &LinkDisposition::Gated {
                slot: "link-0",
                link_wait_ms: 1200,
                queue: "fifo",
                scope: "linker",
            },
            "permit-local-0",
            5,
        );
        log_link(
            dir.path(),
            "ci",
            Some("intendant_connect"),
            &LinkDisposition::Off,
            "permit-ci-0",
            0,
        );
        log_link(
            dir.path(),
            "ci",
            None,
            &LinkDisposition::Degraded,
            "permit-ci-1",
            0,
        );
        log_link_done(dir.path(), Some("intendant"), 48_000, "linker");
        let text = std::fs::read_to_string(dir.path().join(LOG_NAME)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].contains("crate=intendant kind=compile link=deferred"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains(
                "crate=intendant kind=link link_slot=link-0 link_wait_ms=1200 queue=fifo scope=linker permit=permit-local-0 wait_ms=5"
            ),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("kind=link-ungated reason=off permit=permit-ci-0"),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].contains("crate=- kind=link-ungated reason=degraded"),
            "{}",
            lines[3]
        );
        assert!(
            lines[4].contains("crate=intendant kind=link-done runtime_ms=48000 scope=linker"),
            "{}",
            lines[4]
        );
    }

    #[test]
    fn crate_names_stay_single_line_and_bounded() {
        assert_eq!(printable_crate(None), "-");
        assert_eq!(printable_crate(Some("")), "-");
        assert_eq!(printable_crate(Some("intendant")), "intendant");
        assert_eq!(printable_crate(Some("a b\nc")), "abc");
        assert_eq!(printable_crate(Some(&"x".repeat(100))).len(), 64);
    }

    #[test]
    fn rotation_truncates_in_place_keeping_a_line_aligned_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_NAME);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            for i in 0..40_000 {
                writeln!(f, "line {i:07} padding-padding-padding-padding").unwrap();
            }
        }
        assert!(std::fs::metadata(&path).unwrap().len() > MAX_LOG_BYTES);
        log_compile(dir.path(), "ci", None, false, "permit-ci-0", 7);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            (text.len() as u64) <= KEEP_BYTES + 128,
            "not truncated: {}",
            text.len()
        );
        assert!(
            text.starts_with("line "),
            "must cut at a line boundary: {:?}",
            &text[..24]
        );
        assert!(text.trim_end().ends_with("wait_ms=7"));
    }

    #[test]
    fn undersized_log_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_NAME);
        std::fs::write(&path, "existing line\n").unwrap();
        log_compile(dir.path(), "local", None, false, "permit-local-0", 0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("existing line\n"));
        assert_eq!(text.lines().count(), 2);
    }
}
