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
    pub(crate) fn attach(&self, row: &mut serde_json::Value, session_id: &str, dir: &Path) {
        if let Some(envelope) = self
            .agenda
            .as_ref()
            .and_then(|envelopes| envelopes.get(session_id))
        {
            // Safe-to-stop, the ruled Track AO conjunction, fail-closed
            // from the durable journal facts alone (process state can
            // never talk the debt away): the lineage TIP of a
            // started-without-terminal occurrence is a live firing —
            // stopping it kills the run (the owner-stop decree records
            // it failed); a superseded member of one is still owed
            // work; a terminaled occurrence is settled — and "settled"
            // is a claim about AGENDA debt only, displayed beside the
            // attestation so "safe" and "done well" never merge.
            let stop = match (envelope.occurrence_state, envelope.lineage_role) {
                (
                    crate::agenda::OccurrenceState::Started,
                    crate::agenda::SessionLineageRole::Tip,
                ) => "kills_live_run",
                (
                    crate::agenda::OccurrenceState::Started,
                    crate::agenda::SessionLineageRole::Superseded,
                ) => "owed_work",
                _ => "settled",
            };
            let mut block = serde_json::json!({
                "item_id": envelope.item_id,
                // The occurrence rides as an object so Track AO's
                // attestation lands beside `state` without reshaping
                // the wire.
                "occurrence": {
                    "id": envelope.occurrence_id,
                    "state": serde_json::to_value(envelope.occurrence_state)
                        .unwrap_or(serde_json::Value::Null),
                    "lineage_role": envelope.lineage_role.as_str(),
                    "stop": stop,
                },
            });
            if let Some(attempt) = envelope.attempt {
                block["occurrence"]["attempt"] = serde_json::json!(attempt);
            }
            if let Some(attestation) = envelope.attestation.as_ref() {
                // SELF-REPORT (the Q8 labeling law rides the SPA copy):
                // outcome + note + when — refs stay on the agenda
                // surfaces; the chip is a pointer to them.
                let mut att = serde_json::json!({
                    "outcome": serde_json::to_value(attestation.outcome)
                        .unwrap_or(serde_json::Value::Null),
                    "at_ms": attestation.at_ms,
                });
                if let Some(note) = attestation.note.as_ref() {
                    att["note"] = serde_json::Value::String(note.clone());
                }
                block["occurrence"]["attestation"] = att;
            }
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

        let (Some(boot), Some(live)) = (self.boot.as_ref(), self.live_wrappers.as_ref()) else {
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
        let current =
            live_wrapper || super::caches::session_activity_mtime_secs(dir) >= boot.start_secs;
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
            live_wrappers: live.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
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

    fn envelope(
        state: crate::agenda::OccurrenceState,
        lineage_role: crate::agenda::SessionLineageRole,
    ) -> crate::agenda::SessionAgendaEnvelope {
        crate::agenda::SessionAgendaEnvelope {
            item_id: "01ITEM".into(),
            item_title: Some("the source".into()),
            occurrence_id: "occ-1".into(),
            occurrence_state: state,
            lineage_role,
            attempt: None,
            attestation: None,
            sealed_inputs: Vec::new(),
        }
    }

    /// The agenda block's wire shape: id chip + extensible occurrence
    /// object + title + sealed inputs; linkless sessions get no block.
    #[test]
    fn agenda_block_rides_the_row() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir_with_transcript(root.path(), "s1");
        let mut envelopes = HashMap::new();
        envelopes.insert(
            "s1".to_string(),
            envelope(
                crate::agenda::OccurrenceState::Started,
                crate::agenda::SessionLineageRole::Tip,
            ),
        );
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

    /// Track AO pin `grid_agenda_block_is_serving_seam_derived`: the
    /// block is rebuilt from the per-read joins set on every attach —
    /// nothing is stored on the row between builds, a session absent
    /// from the derivation gets NO block (absence claims nothing), and
    /// the Track AO fields (lineage role, the served stop derivation,
    /// the regeneration ordinal, the self-report) ride the occurrence
    /// object exactly as derived.
    #[test]
    fn grid_agenda_block_is_serving_seam_derived() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir_with_transcript(root.path(), "s1");
        let mut env = envelope(
            crate::agenda::OccurrenceState::Started,
            crate::agenda::SessionLineageRole::Tip,
        );
        env.attempt = Some(2);
        env.attestation = Some(crate::agenda::AgendaAttestation {
            outcome: crate::agenda::AttestationOutcome::Partial,
            note: Some("halfway".into()),
            refs: Vec::new(),
            at_ms: 9_000,
            session_id: Some("s1".into()),
        });
        let mut envelopes = HashMap::new();
        envelopes.insert("s1".to_string(), env);
        let joins_set = joins(None, None, Some(envelopes));
        let mut row = serde_json::json!({});
        joins_set.attach(&mut row, "s1", &dir);
        assert_eq!(row["agenda"]["occurrence"]["lineage_role"], "tip");
        assert_eq!(row["agenda"]["occurrence"]["attempt"], 2);
        assert_eq!(
            row["agenda"]["occurrence"]["attestation"]["outcome"],
            "partial"
        );
        assert_eq!(
            row["agenda"]["occurrence"]["attestation"]["note"],
            "halfway"
        );
        assert_eq!(row["agenda"]["occurrence"]["attestation"]["at_ms"], 9_000);

        // The same joins set claims nothing for an underived session —
        // and a fresh row starts from nothing (per-read derivation; no
        // row state survives outside the attach call).
        let mut other = serde_json::json!({});
        joins_set.attach(&mut other, "s-unlinked", &dir);
        assert!(
            other.get("agenda").is_none(),
            "unlinked sessions get no block"
        );

        // A next read deriving a changed state serves the change — the
        // block follows the derivation, never a stored copy.
        let mut settled = HashMap::new();
        settled.insert(
            "s1".to_string(),
            envelope(
                crate::agenda::OccurrenceState::Completed,
                crate::agenda::SessionLineageRole::Tip,
            ),
        );
        let mut row2 = serde_json::json!({});
        joins(None, None, Some(settled)).attach(&mut row2, "s1", &dir);
        assert_eq!(row2["agenda"]["occurrence"]["state"], "completed");
        assert_eq!(row2["agenda"]["occurrence"]["stop"], "settled");
        assert!(
            row2["agenda"]["occurrence"].get("attestation").is_none(),
            "no attestation derived, none served"
        );
    }

    /// Track AO pin `safe_to_stop_is_the_ruled_conjunction`, machine
    /// side: the lineage TIP of a started-without-terminal occurrence
    /// serves `kills_live_run` (the live firing warns loudly — the
    /// fragment pins hold the copy); a SUPERSEDED member of one serves
    /// `owed_work` regardless of process state (the durable journal
    /// debt no liveness can talk away); every terminal serves `settled`
    /// with the attestation beside it, so "safe" and "done well" stay
    /// different claims; and a session with no linkage serves NO block
    /// — the busy-no-linkage card claims nothing.
    #[test]
    fn safe_to_stop_is_the_ruled_conjunction() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir_with_transcript(root.path(), "s1");
        let case = |state, role| {
            let mut envelopes = HashMap::new();
            envelopes.insert("s1".to_string(), envelope(state, role));
            let mut row = serde_json::json!({});
            joins(None, None, Some(envelopes)).attach(&mut row, "s1", &dir);
            row["agenda"]["occurrence"]["stop"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            case(
                crate::agenda::OccurrenceState::Started,
                crate::agenda::SessionLineageRole::Tip,
            ),
            "kills_live_run"
        );
        assert_eq!(
            case(
                crate::agenda::OccurrenceState::Started,
                crate::agenda::SessionLineageRole::Superseded,
            ),
            "owed_work"
        );
        for terminal in [
            crate::agenda::OccurrenceState::Completed,
            crate::agenda::OccurrenceState::Failed,
            crate::agenda::OccurrenceState::Unknown,
            crate::agenda::OccurrenceState::Missed,
        ] {
            assert_eq!(
                case(terminal, crate::agenda::SessionLineageRole::Tip),
                "settled",
                "every terminal settles the agenda debt"
            );
        }
        // No linkage: no block, nothing claimed (the SPA's idle-only
        // "safe" copy is the other half of the conjunction, pinned in
        // the fragment needles).
        let mut row = serde_json::json!({});
        joins(None, None, Some(HashMap::new())).attach(&mut row, "s-busy", &dir);
        assert!(row.get("agenda").is_none());
    }

    /// Daemon↔SPA drift guard for the envelope blocks: the session
    /// windows fragment consumes exactly these wire names, renders the
    /// ghost class, and its sealed-inputs chip shares the one
    /// short-digest formatter instead of minting a truncation.
    #[test]
    fn grid_envelope_wire_reaches_the_fragment() {
        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        for needle in [
            "item_id",
            "item_title",
            "occurrence",
            "sealed_inputs",
            "live_wrapper",
            "ghost",
            "preboot",
            "agendaShortDigest(",
            // Track AO: the safe-to-stop derivation + lineage + retry +
            // self-report wire names, consumed not re-derived.
            "lineage_role",
            "kills_live_run",
            "owed_work",
            "attestation",
        ] {
            assert!(
                fragment.contains(needle),
                "session-windows fragment stopped consuming {needle} — the grid envelope drifted"
            );
        }
        // The window-level ghost treatment lives in the actions fragment
        // (class toggle) and the stylesheet (the dashed-frame rule).
        let actions = include_str!("../../../../../static/app/41-session-window-actions.js");
        assert!(
            actions.contains("session-window-ghost"),
            "the actions fragment stopped toggling the ghost window class"
        );
        let styles = include_str!("../../../../../static/app/12-styles-tasks-log.css");
        assert!(
            styles.contains(".session-window.session-window-ghost"),
            "the stylesheet lost the ghost window treatment"
        );
    }

    /// The join the original matrix missed (the 2026-07-28 ghost-flag
    /// incident): a backend resumed across a daemon restart is TWO
    /// wrapper rows — the dead pre-restart wrapper serves ghost:true
    /// while the live resume-attached wrapper serves ghost:false, same
    /// backend_session_id. Per-row truth is correct by construction;
    /// the fragment pins below keep the SPA's alias fold from letting
    /// the dead twin overwrite the live twin's card.
    #[test]
    fn resumed_across_restart_fixture_pinned() {
        let root = tempfile::tempdir().unwrap();
        let dead_dir = session_dir_with_transcript(root.path(), "wrapper-dead");
        let live_dir = session_dir_with_transcript(root.path(), "wrapper-live");
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Both transcripts predate the restart watershed; live-set
        // membership alone separates the twins.
        let envelope = joins(Some(now_secs + 3_600), Some(&["wrapper-live"]), None);

        let mut dead = serde_json::json!({ "backend_session_id": "backend-a" });
        let mut live = serde_json::json!({ "backend_session_id": "backend-a" });
        envelope.attach(&mut dead, "wrapper-dead", &dead_dir);
        envelope.attach(&mut live, "wrapper-live", &live_dir);

        assert_eq!(
            dead["boot"]["ghost"], true,
            "the pre-restart twin is a ghost"
        );
        assert_eq!(dead["boot"]["era"], "preboot");
        assert_eq!(
            live["boot"]["ghost"], false,
            "the resume-attached twin never is"
        );
        assert_eq!(live["boot"]["live_wrapper"], true);
        assert_eq!(live["boot"]["era"], "current");
        assert_eq!(
            dead["backend_session_id"], live["backend_session_id"],
            "both rows alias one backend-keyed card — the fold fight the SPA resolver settles"
        );
    }

    /// SPA half of the fixture above: the served boot block is stamped
    /// with its writing row's identity and the metadata merge routes
    /// collisions through the resolver, so a dead twin's ghost bit never
    /// folds across alias ids onto a card a live wrapper backs.
    #[test]
    fn ghost_bit_never_folds_across_aliases() {
        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        for needle in [
            // meta build: the writer stamp on the served block
            "boot: session.boot && typeof session.boot === 'object'\n      ? { ...session.boot, source_session_id: session.session_id }\n      : session.boot,",
            // normalize: the stamp survives both wire spellings
            "compactSessionText(raw.source_session_id || raw.sourceSessionId)",
            // merge: boot collisions resolve before the last-write spread
            "normalized.boot = resolveSessionWindowBootMeta(previous.boot, normalized.boot);",
        ] {
            assert!(
                fragment.contains(needle),
                "session-windows fragment lost the alias-fold ghost guard: {needle}"
            );
        }
    }

    /// The dominance law, byte-pinned as one unit: a live-wrapper claim
    /// on a shared card id is replaced only by the same writer's own
    /// next state or by another live wrapper — never by a dead twin.
    /// A behavior change must move this pin and the fragment together.
    #[test]
    fn live_wrapper_dominates_the_merge() {
        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        let resolver = "function resolveSessionWindowBootMeta(previous, incoming) {\n  if (!previous || !incoming) return incoming || previous || null;\n  const sameWriter = !!incoming.sourceSessionId\n    && incoming.sourceSessionId === previous.sourceSessionId;\n  if (previous.liveWrapper && !incoming.liveWrapper && !sameWriter) return previous;\n  return incoming;\n}";
        assert!(
            fragment.contains(resolver),
            "resolveSessionWindowBootMeta drifted from the pinned dominance law"
        );
    }

    /// The card's operative state is read from the RESOLVED metadata
    /// store — the actions fragment's class toggle plus the signature
    /// segment that lets a writer handoff with identical bits still
    /// reach that store — so a resumed backend's card shows the live
    /// wrapper's era and affordances, never the dead lineage's.
    #[test]
    fn resumed_backend_card_shows_live_state() {
        let actions = include_str!("../../../../../static/app/41-session-window-actions.js");
        for needle in [
            "const bootEra = (sessionMetadataById.get(sid) || {}).boot;",
            "win.el.classList.toggle('session-window-ghost', !!(bootEra && bootEra.ghost));",
        ] {
            assert!(
                actions.contains(needle),
                "the actions fragment stopped reading the resolved boot store: {needle}"
            );
        }
        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        assert!(
            fragment.contains("${meta.boot.sourceSessionId || ''}"),
            "the metadata signature lost the boot writer segment — same-bit writer handoffs would never land in the store"
        );
    }
}
