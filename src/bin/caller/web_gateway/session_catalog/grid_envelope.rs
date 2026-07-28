//! The grid session window's operational-envelope extension: derived
//! agenda linkage (source item / occurrence / sealed inputs) and boot
//! era, attached to intendant catalog rows at serve time.
//!
//! Derive-don't-mirror: nothing here is persisted or event-plumbed — the
//! agenda block is [`crate::agenda::AgendaHandle::session_agenda_envelopes`]
//! (journal reverse fold joined with the item store at read time), and
//! the boot block joins HS1's presence substrate (`crate::handover`)
//! with live wrapper registry membership
//! ([`crate::session_supervisor::LiveSessionRegistry::live_wrapper_ids`]).
//! Both blocks attach POST row-cache: `intendant_session_list_row_from_dir`
//! is fingerprint-cached on session-dir state, and every input here moves
//! without touching session dirs (a daemon restart, a wrapper's death, a
//! journal terminal), so the two list entry paths attach per build
//! instead of baking the blocks into cached rows.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// This process's boot, per its own HS1 presence record.
pub(crate) struct CurrentBoot {
    /// Presence-registration instant — the era watershed.
    pub(crate) start_secs: u64,
}

/// Our own presence record → boot start. `None` when this daemon runs
/// without presence (registration failed, exotic shapes, pre-HS1 state
/// roots) — the boot block is then omitted rather than guessed. Own =
/// the record naming this pid; freshest wins if debris left more.
pub(crate) fn resolve_current_boot(state_root: &Path) -> Option<CurrentBoot> {
    let pid = std::process::id();
    crate::handover::read_presence_records(state_root)
        .into_iter()
        .filter(|record| record.pid == pid)
        .max_by_key(|record| record.updated_ms)
        .map(|record| CurrentBoot {
            start_secs: record.updated_ms / 1000,
        })
}

/// The serve-time join set for one catalog build, resolved once at the
/// listing edge and applied per intendant row.
pub(crate) struct GridEnvelopeJoins {
    boot: Option<CurrentBoot>,
    /// `None` = unknown (registry unpublished, or its lock was
    /// contended this instant) — the boot block is omitted rather than
    /// computed from a guess.
    live_wrappers: Option<HashSet<String>>,
    agenda: Option<HashMap<String, crate::agenda::SessionAgendaEnvelope>>,
}

impl GridEnvelopeJoins {
    /// Resolve from the process-global seams + disk. The published
    /// agenda handle joins only when it serves the same state root this
    /// listing reads (exotic homes join nothing).
    pub(crate) fn resolve(home_path: &Path) -> Self {
        let state_root = crate::platform::intendant_home_in(home_path);
        let boot = resolve_current_boot(&state_root);
        let live_wrappers = crate::session_supervisor::published_live_session_registry()
            .and_then(|registry| registry.live_wrapper_ids());
        let agenda = crate::agenda::published_agenda_handle()
            .filter(|handle| handle.dir() == crate::agenda::agenda_dir_in(&state_root))
            .and_then(|handle| handle.session_agenda_envelopes());
        Self {
            boot,
            live_wrappers,
            agenda,
        }
    }

    /// A joins set that attaches nothing (non-daemon shapes and tests).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            boot: None,
            live_wrappers: None,
            agenda: None,
        }
    }

    /// Attach the envelope blocks to one intendant wrapper row.
    pub(crate) fn attach(
        &self,
        row: &mut serde_json::Value,
        session_id: &str,
        dir: &Path,
    ) {
        if let Some(envelope) = self
            .agenda
            .as_ref()
            .and_then(|envelopes| envelopes.get(session_id))
        {
            let mut block = serde_json::json!({
                "item_id": envelope.item_id,
                // The occurrence rides as an object so Track AO's
                // attestation can land beside `state` without reshaping
                // the wire.
                "occurrence": {
                    "id": envelope.occurrence_id,
                    "state": serde_json::to_value(envelope.occurrence_state)
                        .unwrap_or(serde_json::Value::Null),
                },
            });
            if let Some(title) = envelope.item_title.as_ref() {
                block["item_title"] = serde_json::Value::String(title.clone());
            }
            if !envelope.sealed_inputs.is_empty() {
                block["sealed_inputs"] = envelope
                    .sealed_inputs
                    .iter()
                    .map(|binding_ref| {
                        serde_json::json!({
                            "locator": binding_ref.locator,
                            "sha256": binding_ref.sha256,
                        })
                    })
                    .collect();
            }
            row["agenda"] = block;
        }

        let (Some(boot), Some(live)) = (self.boot.as_ref(), self.live_wrappers.as_ref())
        else {
            return;
        };
        let live_wrapper = live.contains(session_id);
        // Era: a live wrapper IS current-boot (a resumed pre-outage
        // session belongs to the daemon driving it now); otherwise the
        // transcript decides. `session.jsonl` mtime is rename-immune
        // (meta rewrites don't touch it); the dir-mtime fallback for
        // transcript-less dirs can over-claim currency after any file
        // lands in the dir — the fail direction that never wrongly
        // claims safe-to-close.
        let current = live_wrapper
            || super::caches::session_activity_mtime_secs(dir) >= boot.start_secs;
        row["boot"] = serde_json::json!({
            "era": if current { "current" } else { "preboot" },
            "live_wrapper": live_wrapper,
            // Served, not SPA-derived: pre-boot with no live wrapper is
            // the safe-to-close state.
            "ghost": !current && !live_wrapper,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_presence(state_root: &Path, boot_id: &str, pid: u32, updated_ms: u64) {
        let dir = state_root.join("daemons");
        std::fs::create_dir_all(&dir).unwrap();
        let record = serde_json::json!({
            "v": 1,
            "boot_id": boot_id,
            "pid": pid,
            "port": 8765,
            "version": {"pkg": "0.0.0", "git_sha": "test", "built_at": "test"},
            "state": "running",
            "updated_ms": updated_ms,
        });
        std::fs::write(dir.join(format!("{boot_id}.json")), record.to_string()).unwrap();
    }

    #[test]
    fn resolve_current_boot_matches_own_pid_only() {
        let root = tempfile::tempdir().unwrap();
        assert!(resolve_current_boot(root.path()).is_none(), "no presence");
        write_presence(root.path(), "boot-foreign", std::process::id() + 1, 9_000);
        assert!(
            resolve_current_boot(root.path()).is_none(),
            "a foreign daemon's record never claims this process"
        );
        write_presence(root.path(), "boot-own", std::process::id(), 5_000);
        write_presence(root.path(), "boot-own-fresh", std::process::id(), 7_000);
        let boot = resolve_current_boot(root.path()).expect("own record");
        assert_eq!(boot.start_secs, 7, "freshest own record wins");
    }

    fn joins(
        boot_start_secs: Option<u64>,
        live: Option<&[&str]>,
        agenda: Option<HashMap<String, crate::agenda::SessionAgendaEnvelope>>,
    ) -> GridEnvelopeJoins {
        GridEnvelopeJoins {
            boot: boot_start_secs.map(|start_secs| CurrentBoot { start_secs }),
            live_wrappers: live
                .map(|ids| ids.iter().map(|id| id.to_string()).collect()),
            agenda,
        }
    }

    fn session_dir_with_transcript(root: &Path, session_id: &str) -> std::path::PathBuf {
        let dir = root.join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), b"{}\n").unwrap();
        dir
    }

    /// The ghost predicate matrix: pre-boot + no live wrapper is the one
    /// ghost shape; a live wrapper is current-boot regardless of
    /// transcript age; unknown liveness or unknown boot omits the block
    /// instead of guessing.
    #[test]
    fn boot_block_era_and_ghost_matrix() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir_with_transcript(root.path(), "s1");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let preboot_watershed = now_secs + 3_600; // transcript predates "boot"
        let current_watershed = now_secs.saturating_sub(3_600);

        // Pre-boot transcript, no wrapper → ghost.
        let mut row = serde_json::json!({});
        joins(Some(preboot_watershed), Some(&[]), None).attach(&mut row, "s1", &dir);
        assert_eq!(row["boot"]["era"], "preboot");
        assert_eq!(row["boot"]["live_wrapper"], false);
        assert_eq!(row["boot"]["ghost"], true);

        // Same transcript age, live wrapper → current, never a ghost.
        let mut row = serde_json::json!({});
        joins(Some(preboot_watershed), Some(&["s1"]), None).attach(&mut row, "s1", &dir);
        assert_eq!(row["boot"]["era"], "current");
        assert_eq!(row["boot"]["live_wrapper"], true);
        assert_eq!(row["boot"]["ghost"], false);

        // Transcript written this boot, no wrapper → current (ended
        // this boot), not a ghost.
        let mut row = serde_json::json!({});
        joins(Some(current_watershed), Some(&[]), None).attach(&mut row, "s1", &dir);
        assert_eq!(row["boot"]["era"], "current");
        assert_eq!(row["boot"]["ghost"], false);

        // Unknown liveness or unknown boot → no block, never a guess.
        let mut row = serde_json::json!({});
        joins(Some(preboot_watershed), None, None).attach(&mut row, "s1", &dir);
        assert!(row.get("boot").is_none());
        let mut row = serde_json::json!({});
        joins(None, Some(&[]), None).attach(&mut row, "s1", &dir);
        assert!(row.get("boot").is_none());
    }

    /// The agenda block's wire shape: id chip + extensible occurrence
    /// object + title + sealed inputs; linkless sessions get no block.
    #[test]
    fn agenda_block_rides_the_row() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir_with_transcript(root.path(), "s1");
        let envelope = crate::agenda::SessionAgendaEnvelope {
            item_id: "01ITEM".into(),
            item_title: Some("the source".into()),
            occurrence_id: "occ-1".into(),
            occurrence_state: crate::agenda::OccurrenceState::Started,
            sealed_inputs: Vec::new(),
        };
        let mut envelopes = HashMap::new();
        envelopes.insert("s1".to_string(), envelope);
        let mut row = serde_json::json!({});
        joins(None, None, Some(envelopes)).attach(&mut row, "s1", &dir);
        assert_eq!(row["agenda"]["item_id"], "01ITEM");
        assert_eq!(row["agenda"]["item_title"], "the source");
        assert_eq!(row["agenda"]["occurrence"]["id"], "occ-1");
        assert_eq!(row["agenda"]["occurrence"]["state"], "started");
        assert!(
            row["agenda"].get("sealed_inputs").is_none(),
            "refless manifests serve no empty array"
        );

        let mut row = serde_json::json!({});
        joins(None, None, Some(HashMap::new())).attach(&mut row, "s2", &dir);
        assert!(row.get("agenda").is_none());
    }
}
