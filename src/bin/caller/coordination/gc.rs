//! Liveness GC (Track C, C1): the daemon's periodic sweep over
//! coordination spaces.
//!
//! Scope is exactly the ruled §9 rule-8 amendment: TIME-based removal
//! exists ONLY for the liveness kinds — declarations a day past their
//! last heartbeat, messages past their TTL, and orphaned atomic-write
//! temp files. Workflow checkpoints keep their acknowledgement-driven
//! lifecycle: this sweep never opens, ages, or deletes a checkpoint
//! document, by construction (only dot-prefixed temp orphans are
//! eligible inside `checkpoints/`). Malformed liveness entries are
//! KEPT and reported — GC deletes only what it can positively
//! attribute to an expired lifetime, never what it cannot parse.
#![cfg_attr(not(test), allow(dead_code))] // C1 PR A: consumed by the PR-B daemon wiring; allow dropped as wiring lands.

use std::path::Path;

use super::{declarations, messages, scan, MAX_SCAN_ENTRIES};

/// Atomic-write temp files older than this are orphans (a crashed
/// writer never renamed them).
pub(crate) const TMP_ORPHAN_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct GcReport {
    pub spaces: usize,
    pub declarations_removed: usize,
    pub messages_removed: usize,
    pub tmp_removed: usize,
    /// `space/kind/name` entries GC refused to touch — surfaced, kept.
    pub malformed_kept: Vec<String>,
    pub errors: Vec<String>,
}

impl GcReport {
    pub(crate) fn removed_anything(&self) -> bool {
        self.declarations_removed + self.messages_removed + self.tmp_removed > 0
    }
}

/// Sweep every space under a coordination root. Per-space trouble is
/// recorded and the sweep continues; a missing root is a no-op.
pub(crate) fn sweep_all(coordination_root: &Path, now_ms: u64) -> GcReport {
    let mut report = GcReport::default();
    let entries = match std::fs::read_dir(coordination_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return report,
        Err(e) => {
            report
                .errors
                .push(format!("{}: {e}", coordination_root.display()));
            return report;
        }
    };
    for (n, entry) in entries.enumerate() {
        if n >= MAX_SCAN_ENTRIES {
            report.errors.push(format!(
                "{}: exceeds the {MAX_SCAN_ENTRIES}-space scan bound",
                coordination_root.display()
            ));
            break;
        }
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = std::fs::symlink_metadata(entry.path())
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !is_dir {
            continue;
        }
        report.spaces += 1;
        sweep_space(&entry.path(), &name, now_ms, &mut report);
    }
    report
}

/// Sweep one space dir (shared with the resolved-single-space path).
pub(crate) fn sweep_space(space_dir: &Path, space: &str, now_ms: u64, report: &mut GcReport) {
    // Declarations: a day past the last heartbeat.
    let sessions_dir = space_dir.join("sessions");
    match declarations::scan_dir(&sessions_dir, now_ms) {
        Ok(scan) => {
            for d in scan.entries {
                if now_ms.saturating_sub(d.effective_mtime_ms) > declarations::DECLARATION_GC_MS {
                    remove_counted(
                        &sessions_dir.join(format!("{}.md", d.id)),
                        &mut report.declarations_removed,
                        &mut report.errors,
                    );
                }
            }
            for r in scan.rejected {
                report
                    .malformed_kept
                    .push(format!("{space}/sessions/{} ({})", r.name, r.reason));
            }
        }
        Err(e) => report.errors.push(format!("{space}/sessions: {e}")),
    }

    // Messages: past their TTL; empty writer dirs pruned after.
    let messages_dir = space_dir.join("messages");
    match messages::scan_meta_dir(&messages_dir, now_ms) {
        Ok(scan) => {
            for m in scan.entries {
                if m.expired {
                    remove_counted(
                        &messages_dir.join(&m.writer).join(format!("{}.md", m.id)),
                        &mut report.messages_removed,
                        &mut report.errors,
                    );
                }
            }
            for r in scan.rejected {
                report
                    .malformed_kept
                    .push(format!("{space}/messages/{} ({})", r.name, r.reason));
            }
        }
        Err(e) => report.errors.push(format!("{space}/messages: {e}")),
    }

    // Orphaned atomic-write temps — the ONLY thing GC may touch inside
    // checkpoints/ (checkpoint documents stay ack-driven, §9 rule 8).
    // Writer dirs sweep temps first, then prune if emptied
    // (best-effort; a concurrent writer just wins).
    if let Ok((writers, _)) = messages::writer_dirs(&messages_dir) {
        for w in writers {
            sweep_tmp_orphans(&w.path, now_ms, report);
            let _ = std::fs::remove_dir(&w.path);
        }
    }
    for dir in [sessions_dir, space_dir.join("checkpoints")] {
        sweep_tmp_orphans(&dir, now_ms, report);
    }
}

fn sweep_tmp_orphans(dir: &Path, now_ms: u64, report: &mut GcReport) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return, // absent kind dir — nothing to sweep
    };
    for entry in entries.take(MAX_SCAN_ENTRIES) {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with('.') {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if now_ms.saturating_sub(scan::effective_mtime_ms(&meta, now_ms)) > TMP_ORPHAN_MS {
            remove_counted(&entry.path(), &mut report.tmp_removed, &mut report.errors);
        }
    }
}

fn remove_counted(path: &Path, counter: &mut usize, errors: &mut Vec<String>) {
    match std::fs::remove_file(path) {
        Ok(()) => *counter += 1,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => errors.push(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::declarations::{DeclarationInput, DeclarationSpace};
    use crate::coordination::messages::{MessageInput, MessageSpace};

    const HOUR_MS: u64 = 60 * 60 * 1000;

    fn declare(ds: &DeclarationSpace, id: &str) {
        ds.write_own(&DeclarationInput {
            id,
            session: None,
            backend: None,
            root: None,
            branch: None,
            intent: "test",
            dirty: &[],
        })
        .unwrap();
    }

    #[test]
    fn declarations_age_out_but_malformed_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let ds = DeclarationSpace::open(&space_dir, "s").unwrap();
        declare(&ds, "s-old");
        std::fs::write(space_dir.join("sessions/s-junk.md"), "not a doc").unwrap();

        let now = crate::coordination::now_ms();
        let mut report = GcReport::default();
        sweep_space(&space_dir, "space", now + 25 * HOUR_MS, &mut report);
        assert_eq!(report.declarations_removed, 1);
        assert!(!space_dir.join("sessions/s-old.md").exists());
        assert!(
            space_dir.join("sessions/s-junk.md").exists(),
            "GC never deletes what it cannot parse"
        );
        assert_eq!(
            report.malformed_kept.len(),
            1,
            "{:?}",
            report.malformed_kept
        );

        // A fresh declaration survives a fresh sweep.
        declare(&ds, "s-new");
        let mut report = GcReport::default();
        sweep_space(&space_dir, "space", now + 60_000, &mut report);
        assert_eq!(report.declarations_removed, 0);
        assert!(space_dir.join("sessions/s-new.md").exists());
    }

    #[test]
    fn expired_messages_go_and_writer_dirs_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let ms = MessageSpace::open(&space_dir, "s").unwrap();
        let short = ms
            .write(
                "s-a",
                &MessageInput {
                    to: None,
                    ttl_s: Some(60),
                    body: "expiring",
                },
            )
            .unwrap();
        ms.write(
            "s-b",
            &MessageInput {
                to: None,
                ttl_s: Some(604_800),
                body: "durable",
            },
        )
        .unwrap();

        let now = crate::coordination::now_ms();
        let mut report = GcReport::default();
        sweep_space(&space_dir, "space", now + 2 * 60_000, &mut report);
        assert_eq!(report.messages_removed, 1);
        assert!(!space_dir
            .join("messages/s-a")
            .join(format!("{}.md", short.id))
            .exists());
        assert!(
            !space_dir.join("messages/s-a").exists(),
            "emptied writer dir pruned"
        );
        assert!(
            space_dir.join("messages/s-b").is_dir(),
            "live writer dir kept"
        );
    }

    #[test]
    fn checkpoints_are_never_aged_and_tmp_orphans_are() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let cp_dir = space_dir.join("checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("cp-ancient.md"), "checkpoint bytes").unwrap();
        std::fs::write(cp_dir.join(".cp-orphan.tmp"), "crashed write").unwrap();

        let now = crate::coordination::now_ms();
        let mut report = GcReport::default();
        // A century from now: the checkpoint STILL survives.
        sweep_space(&space_dir, "space", now + 876_000 * HOUR_MS, &mut report);
        assert!(
            cp_dir.join("cp-ancient.md").exists(),
            "GC must never delete checkpoint documents"
        );
        assert!(
            !cp_dir.join(".cp-orphan.tmp").exists(),
            "orphaned temp swept"
        );
        assert_eq!(report.tmp_removed, 1);

        // A fresh in-flight temp is left alone.
        std::fs::write(cp_dir.join(".cp-inflight.tmp"), "active write").unwrap();
        let mut report = GcReport::default();
        sweep_space(&space_dir, "space", now + 60_000, &mut report);
        assert!(cp_dir.join(".cp-inflight.tmp").exists());
        assert_eq!(report.tmp_removed, 0);
    }

    #[test]
    fn sweep_all_walks_spaces_and_tolerates_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("coordination");
        assert!(
            !sweep_all(&root, 0).removed_anything(),
            "missing root is a no-op"
        );

        for name in ["space-a", "space-b"] {
            let ds = DeclarationSpace::open(&root.join(name), name).unwrap();
            declare(&ds, "s-x");
        }
        std::fs::write(root.join("stray-file"), "x").unwrap();
        let now = crate::coordination::now_ms();
        let report = sweep_all(&root, now + 25 * HOUR_MS);
        assert_eq!(report.spaces, 2);
        assert_eq!(report.declarations_removed, 2);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }
}
