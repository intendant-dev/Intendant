//! The grid session window's operational-envelope extension: derived
//! agenda linkage (source item / occurrence / sealed inputs), boot era,
//! and — for dead rows — lineage truth (terminal facts + the successor
//! pointer), attached to intendant catalog rows at serve time.
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
//! instead of baking the blocks into cached rows. The one exception rides
//! the other way: the row's `terminal` block IS dir-local (summary.json +
//! the transcript's last error), so it bakes into the cached row and this
//! join merely lifts it into the boot block — one writer-stamped unit for
//! the SPA's alias-fold resolver to arbitrate.
//!
//! The successor pointer derives from the ONE shared lineage walker,
//! [`crate::session_supervisor::resume_lineage::resolve_resume_lineage`]
//! (never a private re-implementation), memoized on the seed dir's
//! transcript fingerprint plus the wrapper index's LINEAGE EPOCH
//! ([`crate::external_wrapper_index::lineage_epoch`]) — every new
//! wrapper generation writes an index row, so a memo cannot outlive the
//! chain it summarizes. The epoch, not the raw file fingerprint: the
//! list pass's own backfill restamps live rows' recency on every serve,
//! and keying on the file fingerprint invalidated every memo per serve —
//! each poll re-walked every ghost row's lineage over the whole store
//! (the 2026-07-30 boot-storm hot-spin, 871% CPU starving the accept
//! lane). The epoch moves only when walk-relevant content moves.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    /// Home for the resume-lineage walk behind dead rows' successor
    /// pointers; `None` skips the lineage join (tests that exercise only
    /// the boot matrix).
    home: Option<PathBuf>,
    /// Wrapper-index lineage epoch, resolved ONCE per build (one stat on
    /// an unchanged index) and handed to every row's tip memo check —
    /// never re-derived per row.
    lineage_epoch: Option<u64>,
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
            lineage_epoch: Some(crate::external_wrapper_index::lineage_epoch(home_path)),
            home: Some(home_path.to_path_buf()),
        }
    }

    /// A joins set that attaches nothing (non-daemon shapes and tests).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            boot: None,
            live_wrappers: None,
            agenda: None,
            home: None,
            lineage_epoch: None,
        }
    }

    /// Test constructor with every join explicit, shared with the
    /// session-supervisor tests that pin the registry→envelope chain
    /// (the readopt-successor false-ghost class).
    #[cfg(test)]
    pub(crate) fn for_tests(
        boot_start_secs: Option<u64>,
        live_wrappers: Option<HashSet<String>>,
        agenda: Option<HashMap<String, crate::agenda::SessionAgendaEnvelope>>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            boot: boot_start_secs.map(|start_secs| CurrentBoot { start_secs }),
            live_wrappers,
            agenda,
            lineage_epoch: home
                .as_deref()
                .map(crate::external_wrapper_index::lineage_epoch),
            home,
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

        // Worktree state — the stranded-work axis (dirty / unpushed /
        // ahead), probed from the daemon's worktree knowledge
        // (worktree_inventory's serve-time memo). Attached post
        // row-cache like the other blocks: tree state moves without
        // touching the session dir. The path comes from the row's own
        // recorded linkage; without linkage, a cwd or project root
        // under a `.worktrees` directory (the external-seat convention)
        // qualifies — never a main checkout, whose dirtiness says
        // nothing about THIS session's arc. Missing/unknown serve
        // state-only: honest ignorance, never a guessed clean.
        if let Some(path) = row_worktree_path(row) {
            let state = crate::worktree_inventory::cached_worktree_git_state(&path);
            row["worktree_state"] = worktree_state_json(&state);
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
        let ghost = !current && !live_wrapper;
        let mut boot_block = serde_json::json!({
            "era": if current { "current" } else { "preboot" },
            "live_wrapper": live_wrapper,
            // Served, not SPA-derived: pre-boot with no live wrapper is
            // the safe-to-close state.
            "ghost": ghost,
        });
        // Lift the row's dir-local terminal facts into the boot block so
        // terminal + era + lineage reach the SPA as ONE writer-stamped
        // unit — the alias-fold resolver then arbitrates whole claims and
        // the winning row's terminal is the one the card states.
        if let Some(terminal) = row.get("terminal").filter(|value| value.is_object()) {
            boot_block["terminal"] = terminal.clone();
        }
        if ghost {
            match dead_row_lineage_tip(self.home.as_deref(), session_id, dir, self.lineage_epoch) {
                LineageTip::NoWrapperHistory => {}
                LineageTip::SelfTip => {
                    boot_block["lineage_tip"] = serde_json::Value::Bool(true);
                }
                LineageTip::ContinuedAs {
                    source,
                    wrapper_session_id,
                    backend_session_id,
                } => {
                    boot_block["lineage_tip"] = serde_json::Value::Bool(false);
                    // Live bit resolved per build, never memoized: the
                    // registry keys post-identity entries by backend id
                    // and aliases the wrapper dir id, so check both.
                    let successor_live =
                        live.contains(&wrapper_session_id) || live.contains(&backend_session_id);
                    boot_block["continued_as"] = serde_json::json!({
                        "source": source,
                        "session_id": wrapper_session_id,
                        "backend_session_id": backend_session_id,
                        "live": successor_live,
                    });
                }
            }
        }
        row["boot"] = boot_block;
    }
}

/// The checkout path whose git state speaks for this row's arc:
/// recorded worktree linkage first (session_meta.json, native worktree
/// launches), else a cwd/project-root under a `.worktrees` directory —
/// the convention external seats create their own checkouts under. A
/// main checkout never qualifies: its dirtiness is the merge target's,
/// not this session's.
fn row_worktree_path(row: &serde_json::Value) -> Option<PathBuf> {
    if let Some(path) = row["worktree"]["path"].as_str().filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(path));
    }
    for key in ["cwd", "project_root"] {
        if let Some(path) = row[key].as_str().filter(|s| !s.is_empty()) {
            let path = Path::new(path);
            if path
                .components()
                .any(|component| component.as_os_str() == ".worktrees")
            {
                return Some(path.to_path_buf());
            }
        }
    }
    None
}

/// The `worktree_state` wire block: `state` always; the probed facts
/// (`dirty`/`unpushed`/`ahead`, `branch`) only when the probe answered
/// (`clean`/`dirty`) — `missing`/`unknown` carry no fact fields.
fn worktree_state_json(state: &crate::worktree_inventory::WorktreeGitState) -> serde_json::Value {
    use crate::worktree_inventory::WorktreeStateKind;
    let mut block = serde_json::json!({
        "state": state.kind.as_str(),
        "checked_ms": state.checked_ms,
    });
    if matches!(state.kind, WorktreeStateKind::Clean | WorktreeStateKind::Dirty) {
        block["dirty"] = serde_json::Value::Bool(state.dirty);
        block["unpushed"] = serde_json::Value::Bool(state.unpushed);
        block["ahead"] = serde_json::json!(state.ahead);
        if let Some(branch) = state.branch.as_ref() {
            block["branch"] = serde_json::Value::String(branch.clone());
        }
    }
    block
}

/// Where a dead row's lineage stands: no recorded wrapper history (native
/// sessions), this row IS the chain's current incarnation, or the chain
/// continued in another wrapper.
#[derive(Clone)]
enum LineageTip {
    NoWrapperHistory,
    SelfTip,
    ContinuedAs {
        source: String,
        wrapper_session_id: String,
        backend_session_id: String,
    },
}

struct LineageTipMemoEntry {
    transcript_fingerprint: (u64, u128),
    lineage_epoch: u64,
    tip: LineageTip,
}

/// Comfortably above the session-store dir count (~4k on the machine the
/// hot-spin hit), so clear-on-full stays a corruption backstop instead of
/// wiping the memo mid-pass right at today's scale.
const LINEAGE_TIP_MEMO_LIMIT: usize = 16_384;

fn lineage_tip_memo() -> &'static Mutex<HashMap<String, LineageTipMemoEntry>> {
    static MEMO: OnceLock<Mutex<HashMap<String, LineageTipMemoEntry>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn transcript_fingerprint(dir: &Path) -> (u64, u128) {
    match std::fs::metadata(dir.join("session.jsonl")) {
        Ok(meta) => (
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ),
        Err(_) => (0, 0),
    }
}

/// The lineage tip behind one dead row, via the shared resume-lineage
/// walker AND its own tip reduction (`successor_tip` past this row —
/// only `Active` records qualify; `Superseded` rows are dead
/// incarnations, never what a card should point at). Memoized on (own
/// transcript fingerprint, wrapper-index lineage epoch): a dead dir
/// never changes and every new wrapper generation writes an index row,
/// so hits are exact and misses are one bounded walk — seeded with the
/// row's own dir so the walk never pays the id→dir directory probe for
/// its seed. The epoch (not the raw index fingerprint — see the module
/// header) is resolved once per build by the caller.
fn dead_row_lineage_tip(
    home: Option<&Path>,
    session_id: &str,
    dir: &Path,
    lineage_epoch: Option<u64>,
) -> LineageTip {
    let Some(home) = home else {
        return LineageTip::NoWrapperHistory;
    };
    let transcript = transcript_fingerprint(dir);
    let epoch = lineage_epoch.unwrap_or_else(|| crate::external_wrapper_index::lineage_epoch(home));
    if let Ok(memo) = lineage_tip_memo().lock() {
        if let Some(entry) = memo.get(session_id) {
            if entry.transcript_fingerprint == transcript && entry.lineage_epoch == epoch {
                return entry.tip.clone();
            }
        }
    }
    let lineage = crate::session_supervisor::resume_lineage::resolve_resume_lineage_with_dir_hints(
        home,
        &[session_id],
        &[(session_id, dir)],
    );
    let tip = if let Some(record) = lineage.successor_tip(&[session_id]) {
        LineageTip::ContinuedAs {
            source: record.source.clone(),
            wrapper_session_id: record.intendant_session_id.clone(),
            backend_session_id: record.backend_session_id.clone(),
        }
    } else if lineage.has_wrapper_history() {
        LineageTip::SelfTip
    } else {
        LineageTip::NoWrapperHistory
    };
    if let Ok(mut memo) = lineage_tip_memo().lock() {
        if memo.len() >= LINEAGE_TIP_MEMO_LIMIT && !memo.contains_key(session_id) {
            memo.clear();
        }
        memo.insert(
            session_id.to_string(),
            LineageTipMemoEntry {
                transcript_fingerprint: transcript,
                lineage_epoch: epoch,
                tip: tip.clone(),
            },
        );
    }
    tip
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
        GridEnvelopeJoins::for_tests(
            boot_start_secs,
            live.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
            agenda,
            None,
        )
    }

    fn joins_with_home(
        boot_start_secs: Option<u64>,
        live: Option<&[&str]>,
        home: &Path,
    ) -> GridEnvelopeJoins {
        GridEnvelopeJoins::for_tests(
            boot_start_secs,
            live.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
            None,
            Some(home.to_path_buf()),
        )
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
            "lineage_tip",
            "continued_as",
            "raw.terminal",
            // The stranded-work axis: served worktree state consumed by
            // the normalize + chip surfaces.
            "worktree_state",
            "normalizeSessionWorktreeState(",
            "'worktree-state'",
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

    /// The dominance law, byte-pinned as one unit: a writer's own
    /// lifecycle update always lands; otherwise claims rank live wrapper
    /// > current era > lineage tip > the rest, and only an equal-or-higher
    /// rank replaces the standing claim — so a dead twin can never
    /// overwrite a live wrapper's card, and between two DEAD generations
    /// the chain's newest incarnation wins whatever order the rows folded
    /// in. A behavior change must move this pin and the fragment together.
    #[test]
    fn live_wrapper_dominates_the_merge() {
        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        let resolver = "function resolveSessionWindowBootMeta(previous, incoming) {\n  if (!previous || !incoming) return incoming || previous || null;\n  const sameWriter = !!incoming.sourceSessionId\n    && incoming.sourceSessionId === previous.sourceSessionId;\n  if (sameWriter) return incoming;\n  const rank = claim => (claim.liveWrapper ? 3 : (claim.era === 'current' ? 2 : (claim.lineageTip ? 1 : 0)));\n  return rank(incoming) >= rank(previous) ? incoming : previous;\n}";
        assert!(
            fragment.contains(resolver),
            "resolveSessionWindowBootMeta drifted from the pinned dominance ladder"
        );
    }

    fn write_jsonl(dir: &Path, lines: &[serde_json::Value]) {
        let body = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("session.jsonl"), format!("{body}\n")).unwrap();
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A wrapper log dir announcing its backend conversation(s) — the
    /// same durable trace live wrappers leave (the identity event also
    /// writes the wrapper-index row), mirroring the resume_lineage test
    /// helper.
    fn announce(home: &Path, wrapper: &str, backend_ids: &[&str]) -> std::path::PathBuf {
        let dir = crate::platform::intendant_home_in(home)
            .join("logs")
            .join(wrapper);
        let mut log = crate::session_log::SessionLog::open(dir.clone()).unwrap();
        log.write_meta(None, None);
        for backend_id in backend_ids {
            log.session_identity(wrapper, "claude-code", backend_id);
        }
        dir
    }

    /// (a) of the ghost-card terminal-honesty card (01KYR84M4PB8QVBR3Y…):
    /// a dead session's window states its terminal fact plainly. Daemon
    /// half: the row serves summary.json's outcome/ended_at verbatim, the
    /// status stops flattening a backend death to "completed", and attach
    /// lifts the facts into the boot block for the alias fold. SPA half:
    /// the statement composer renders "Ended <when>: <outcome>" and the
    /// note strip exists.
    #[test]
    fn dead_session_window_states_terminal_fact() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("gl1-wrapper");
        std::fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            &[
                serde_json::json!({"event": "session_start", "ts": "2026-07-29 19:40:00"}),
                serde_json::json!({"event": "turn_start", "turn": 22}),
                serde_json::json!({"event": "session_end", "level": "info",
                    "message": "Session ended: Claude Code process closed stdout (22 turns)"}),
            ],
        );
        std::fs::write(
            dir.join("summary.json"),
            serde_json::json!({
                "outcome": "Claude Code process closed stdout",
                "ended_at": "2026-07-29 19:59:01",
                "total_turns": 22,
            })
            .to_string(),
        )
        .unwrap();

        let mut row = super::super::intendant_session_list_row_from_dir(&dir, "gl1-wrapper")
            .expect("row builds");
        assert_eq!(
            row["terminal"]["outcome"], "Claude Code process closed stdout",
            "summary outcome must ride the row verbatim"
        );
        assert_eq!(row["terminal"]["ended_at"], "2026-07-29 19:59:01");
        assert_eq!(
            row["status"], "failed",
            "a backend death must not flatten to completed (the summary consult was unreachable)"
        );

        joins(Some(now_secs() + 3_600), Some(&[]), None).attach(&mut row, "gl1-wrapper", &dir);
        assert_eq!(row["boot"]["ghost"], true);
        assert_eq!(
            row["boot"]["terminal"]["outcome"], "Claude Code process closed stdout",
            "attach must lift the row's terminal facts into the boot block"
        );

        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        for needle in [
            "function sessionWindowTerminalStatement(",
            "Ended${when ? ` ${when}` : ''}: ${terminal.outcome}",
            "session-window-terminal-note",
        ] {
            assert!(
                fragment.contains(needle),
                "session-windows fragment lost the terminal statement: {needle}"
            );
        }
        let styles = include_str!("../../../../../static/app/12-styles-tasks-log.css");
        assert!(
            styles.contains(".session-window .session-window-terminal-note"),
            "the stylesheet lost the terminal-note strip"
        );
    }

    /// The refresh-rebuild lane (second specimen on 01KYMFPC) and the
    /// f7a7ccba shape: a crash-frozen dir — mid-turn death on an API
    /// error streak, no summary ever written — serves its frozen-at facts
    /// (status still in_progress, the freshest error, ghost bit), and the
    /// SPA states "Died mid-turn …" from METADATA (the updateSessionWindow
    /// hook), so a rebuilt card is honest before any click hydrates it.
    /// Liveness inference stays untouched: hydration still never flips
    /// `ended` (the #637 stop-hide law).
    #[test]
    fn rebuilt_card_for_ended_session_renders_terminal_not_started_only() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("gl2-wrapper");
        std::fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            &[
                serde_json::json!({"event": "session_start", "ts": "2026-07-29 15:00:00"}),
                serde_json::json!({"event": "turn_start", "turn": 76}),
                serde_json::json!({"event": "error", "level": "error", "ts": "2026-07-29 15:49:12",
                    "message": "provider error: 500 upstream_error"}),
                serde_json::json!({"event": "error", "level": "error", "ts": "2026-07-29 15:50:41",
                    "message": "claude-code backend error (error_during_execution): API error"}),
            ],
        );

        let mut row = super::super::intendant_session_list_row_from_dir(&dir, "gl2-wrapper")
            .expect("row builds");
        assert_eq!(
            row["status"], "in_progress",
            "the frozen mid-turn status is itself a fact — liveness comes from the boot join"
        );
        assert_eq!(
            row["terminal"]["last_error"]["message"],
            "claude-code backend error (error_during_execution): API error",
            "the freshest error must ride the row"
        );
        assert_eq!(row["terminal"]["last_error"]["ts"], "2026-07-29 15:50:41");
        assert!(
            row["terminal"].get("outcome").is_none(),
            "no summary was ever written — the row must not invent one"
        );

        joins(Some(now_secs() + 3_600), Some(&[]), None).attach(&mut row, "gl2-wrapper", &dir);
        assert_eq!(row["boot"]["ghost"], true);
        assert_eq!(
            row["boot"]["terminal"]["last_error"]["ts"],
            "2026-07-29 15:50:41"
        );

        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        for needle in [
            // the mid-turn death statement branch
            "Died mid-turn${when ? ` — last activity ${when}` : ''}",
            // hydration still never flips ended (phase is not liveness)
            "ended: false,",
        ] {
            assert!(
                fragment.contains(needle),
                "session-windows fragment lost the rebuild-lane terminal statement: {needle}"
            );
        }
        let actions = include_str!("../../../../../static/app/41-session-window-actions.js");
        for needle in [
            // the note renders from the metadata path — before any click
            "renderSessionWindowTerminalNote(win, sid);",
            // and the pill stops advertising activity on a corpse
            "return ghostTerminal?.outcome ? 'Ended' : 'Died';",
        ] {
            assert!(
                actions.contains(needle),
                "the actions fragment lost the pre-click terminal render: {needle}"
            );
        }
    }

    /// (b): a ghost card whose lineage continued serves a successor
    /// pointer — `continued_as` with the tip's ids and live bit — while
    /// the tip's own row says `lineage_tip: true` and points nowhere; the
    /// SPA renders it as the clickable "continued as …" affordance.
    #[test]
    fn ghost_card_with_continued_lineage_shows_successor_pointer() {
        let home = tempfile::tempdir().unwrap();
        let old_dir = announce(home.path(), "gl3-wrapper-old", &["gl3-b1"]);
        let new_dir = announce(home.path(), "gl3-wrapper-new", &["gl3-b1", "gl3-b2"]);
        let watershed = now_secs() + 3_600;

        let mut old_row = serde_json::json!({});
        joins_with_home(Some(watershed), Some(&[]), home.path()).attach(
            &mut old_row,
            "gl3-wrapper-old",
            &old_dir,
        );
        assert_eq!(old_row["boot"]["ghost"], true);
        assert_eq!(old_row["boot"]["lineage_tip"], false);
        assert_eq!(
            old_row["boot"]["continued_as"]["session_id"], "gl3-wrapper-new",
            "the dead original must point at the chain's current incarnation"
        );
        assert_eq!(old_row["boot"]["continued_as"]["live"], false);
        assert!(old_row["boot"]["continued_as"]["backend_session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));

        let mut new_row = serde_json::json!({});
        joins_with_home(Some(watershed), Some(&[]), home.path()).attach(
            &mut new_row,
            "gl3-wrapper-new",
            &new_dir,
        );
        assert_eq!(new_row["boot"]["ghost"], true);
        assert_eq!(
            new_row["boot"]["lineage_tip"], true,
            "the tip's own row is the terminal authority, not a pointer"
        );
        assert!(new_row["boot"].get("continued_as").is_none());

        // The live bit resolves per build against the (alias-closed)
        // registry set — the successor counts as live under its backend
        // id, exactly how post-identity entries are keyed.
        let mut relive = serde_json::json!({});
        joins_with_home(Some(watershed), Some(&["gl3-b2"]), home.path()).attach(
            &mut relive,
            "gl3-wrapper-old",
            &old_dir,
        );
        assert_eq!(relive["boot"]["continued_as"]["live"], true);

        let fragment = include_str!("../../../../../static/app/39-session-windows.js");
        for needle in [
            "continued as ${continuationLabel}",
            "session-window-terminal-continued",
            "openSessionWindowForContinuation(",
        ] {
            assert!(
                fragment.contains(needle),
                "session-windows fragment lost the successor pointer: {needle}"
            );
        }
    }

    /// The pointer is DERIVED from the shared resume-lineage resolver,
    /// never a private re-implementation: an edit-branch retirement edge
    /// (`record_lineage_retired`) is only reachable through
    /// `resolve_resume_lineage`'s conversation chain — a naive
    /// wrappers_for lookup on the parent's conversation has no path to
    /// the child, so this test fails against any re-implementation.
    #[test]
    fn successor_pointer_derived_from_lineage_resolver() {
        let home = tempfile::tempdir().unwrap();
        let parent_dir = announce(home.path(), "gl4-wrapper-parent", &["gl4-b1"]);
        announce(home.path(), "gl4-wrapper-child", &["gl4-b2"]);
        crate::external_wrapper_index::record_lineage_retired(
            home.path(),
            "claude-code",
            "gl4-b1",
            "gl4-b2",
        )
        .unwrap();

        let mut row = serde_json::json!({});
        joins_with_home(Some(now_secs() + 3_600), Some(&[]), home.path()).attach(
            &mut row,
            "gl4-wrapper-parent",
            &parent_dir,
        );
        assert_eq!(
            row["boot"]["continued_as"]["session_id"], "gl4-wrapper-child",
            "the retirement edge must reach the edit-branch child — only the shared resolver walks it"
        );
    }

    /// The tip memo's freshness law under its epoch key: a tip memoized
    /// in one build (here: SelfTip — the chain's only wrapper) must NOT
    /// be served by a later build once the index gained a walk-relevant
    /// change (a successor generation demoting the seed). The memo keys
    /// on the lineage EPOCH — recency restamps hold it steady (pinned in
    /// `external_wrapper_index::tests::lineage_epoch_tracks_walk_relevant_content_only`),
    /// so this test pins the other direction: real changes must break
    /// the memo hit.
    #[test]
    fn stale_tip_never_served_across_epoch_change() {
        let home = tempfile::tempdir().unwrap();
        let solo_dir = announce(home.path(), "gl5-wrapper-solo", &["gl5-b1"]);
        let watershed = now_secs() + 3_600;

        let mut row = serde_json::json!({});
        joins_with_home(Some(watershed), Some(&[]), home.path()).attach(
            &mut row,
            "gl5-wrapper-solo",
            &solo_dir,
        );
        assert_eq!(
            row["boot"]["lineage_tip"], true,
            "the sole wrapper is its own tip (memoized this build)"
        );

        // A successor generation announces the same conversation: new
        // index row + the solo row's demotion — a walk-relevant change.
        announce(home.path(), "gl5-wrapper-heir", &["gl5-b1"]);
        let mut row = serde_json::json!({});
        joins_with_home(Some(watershed), Some(&[]), home.path()).attach(
            &mut row,
            "gl5-wrapper-solo",
            &solo_dir,
        );
        assert_eq!(
            row["boot"]["lineage_tip"], false,
            "the memoized SelfTip must not survive the epoch change"
        );
        assert_eq!(
            row["boot"]["continued_as"]["session_id"], "gl5-wrapper-heir",
            "the re-walk must reach the successor generation"
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

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The interrupted-arc closability amendment (2026-08-01, lex
    /// posterior over the ruled conjunction's CLOSABLE application —
    /// stop-safety untouched), byte-pinned where it bites: the claim
    /// function's interrupted guard, the arc-evidence helper, the
    /// mid-work card copy, the extracted tooltip matrix, and the QA
    /// vector facade. A behavior change must move these pins and the
    /// fragment together.
    #[test]
    fn interrupted_arc_closable_amendment_pinned() {
        let actions = include_str!("../../../../../static/app/41-session-window-actions.js");
        let guard = "  if (claim.interrupted) {\n    if (claim.attested !== 'achieved' || claim.worktreeStranded) return false;\n  }";
        assert!(
            actions.contains(guard),
            "the closable claim's interrupted guard drifted from the amendment"
        );
        for needle in [
            "function sessionWindowInterruptedArcState(",
            "function sessionWindowCloseTitle(",
            "Interrupted mid-work — resumable",
            "CLOSABLE_CLAIM_QA_VECTORS",
            "closableClaim: {",
            "worktreeStranded: !!(ws && (ws.dirty || ws.unpushed))",
        ] {
            assert!(
                actions.contains(needle),
                "the actions fragment lost the interrupted-arc amendment surface: {needle}"
            );
        }
        // Stop-safety semantics unchanged: interrupted remains HARD done
        // evidence for the sweeps and the stop flow.
        assert!(
            actions.contains(
                "return !!(win.ended || meta.ended) || phase === 'done' || phase === 'interrupted';"
            ),
            "hard done evidence must keep interrupted — only the CLOSABLE application was amended"
        );
    }

    /// The stranded-work axis rides the row: a dirty, never-pushed
    /// checkout named by the row's recorded worktree linkage serves the
    /// probed facts — independent of the boot/agenda joins.
    #[test]
    fn worktree_state_rides_the_row() {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init"]);
        git(&checkout, &["checkout", "-b", "seat-branch"]);
        git(&checkout, &["config", "user.email", "t@example.com"]);
        git(&checkout, &["config", "user.name", "T"]);
        std::fs::write(checkout.join("README.md"), "hi\n").unwrap();
        git(&checkout, &["add", "README.md"]);
        git(&checkout, &["commit", "-m", "initial"]);
        std::fs::write(checkout.join("scratch.txt"), "local\n").unwrap();
        crate::worktree_inventory::probe_and_memoize_worktree_git_state(&checkout);

        let dir = session_dir_with_transcript(root.path(), "s1");
        let mut row = serde_json::json!({
            "worktree": {"branch": "seat-branch", "path": checkout.to_string_lossy()},
        });
        joins(None, None, None).attach(&mut row, "s1", &dir);
        assert_eq!(row["worktree_state"]["state"], "dirty");
        assert_eq!(row["worktree_state"]["dirty"], true);
        assert_eq!(
            row["worktree_state"]["unpushed"], true,
            "a remoteless checkout's commits exist nowhere else"
        );
        assert_eq!(row["worktree_state"]["branch"], "seat-branch");
        assert!(row["worktree_state"]["checked_ms"].as_u64().unwrap() > 0);
    }

    /// Honest degradation + scope: a gone linkage path serves state-only
    /// `missing`; a linkage-less cwd qualifies only under a `.worktrees`
    /// directory (honest `unknown` for a non-git dir); a main-checkout
    /// cwd serves NO block — its dirtiness is the merge target's, not
    /// this session's.
    #[test]
    fn worktree_state_degrades_and_scopes_honestly() {
        let root = tempfile::tempdir().unwrap();
        let dir = session_dir_with_transcript(root.path(), "s1");

        let mut row = serde_json::json!({
            "worktree": {"branch": "b", "path": root.path().join("reclaimed").to_string_lossy()},
        });
        joins(None, None, None).attach(&mut row, "s1", &dir);
        assert_eq!(row["worktree_state"]["state"], "missing");
        assert!(
            row["worktree_state"].get("dirty").is_none(),
            "missing serves no fact fields"
        );

        let seat = root.path().join(".worktrees").join("seat");
        std::fs::create_dir_all(&seat).unwrap();
        let mut row = serde_json::json!({ "cwd": seat.to_string_lossy() });
        joins(None, None, None).attach(&mut row, "s2", &dir);
        assert_eq!(
            row["worktree_state"]["state"], "unknown",
            "first sight of a non-git seat dir is honest ignorance"
        );

        let repo_root = root.path().join("repo-root");
        std::fs::create_dir_all(&repo_root).unwrap();
        let mut row = serde_json::json!({ "cwd": repo_root.to_string_lossy() });
        joins(None, None, None).attach(&mut row, "s3", &dir);
        assert!(
            row.get("worktree_state").is_none(),
            "a main checkout never speaks for a session's arc"
        );
    }
}
