//! Durable resume-lineage resolution: the ONE walker behind every
//! "which session carries this conversation now?" question. A wrapper
//! session's work survives it — owner Restart supersedes the wrapper on
//! the same backend conversation, an edit branch retires the
//! conversation to a successor conversation, and each successor can
//! chain again — so any consumer that classifies or delivers against a
//! session id must resolve the whole chain, from durable state only
//! (session-dir identity facts + the wrapper index): in-memory alias
//! maps die with the daemon and cap out under churn.
//!
//! Consumers today: the agenda-answer delivery arm
//! (`resolve_ask_delivery_entry`, probing candidates against live
//! supervisor state) and the agenda scheduler's occurrence terminal
//! classification (following an ended session to its successor before
//! journaling a terminal). They deliberately share this walk — two
//! independent walkers WILL drift; add consumers here, never a private
//! re-implementation.

use super::launch::{
    recorded_backend_conversations_in_home, recorded_backend_conversations_in_home_with_dir,
};
use crate::external_wrapper_index::{
    lineage_tombstones, wrapper_preference, wrappers_for, ExternalWrapperRecord, WrapperState,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Walk bounds: a lineage is a restart chain (human-gesture scale), so
/// real chains are short; the caps only defend against a corrupted or
/// adversarial index (e.g. a `retired_successor` cycle) spinning the
/// walk.
const MAX_LINEAGE_SESSIONS: usize = 64;
const MAX_LINEAGE_CONVERSATIONS: usize = 64;

/// The transitive resume lineage of a seed session, resolved from
/// durable state. Empty collections mean the seeds have no recorded
/// external-wrapper history at all (a native session, or a wrapper that
/// died before any identity fact landed).
pub(crate) struct ResumeLineage {
    /// Ordered delivery-probe candidates: each conversation id first
    /// (a live successor is aliased or keyed under it once it
    /// announces), then that conversation's indexed wrappers, newest
    /// first — the seed's own conversations lead, transitive hops
    /// follow. `(source, id)` pairs; ids may name conversations or
    /// wrapper sessions, which is exactly what a live-session probe
    /// wants to try.
    pub(crate) candidates: Vec<(String, String)>,
    /// Every wrapper record in the closure, preference-ordered (active
    /// first, then newest).
    pub(crate) wrapper_records: Vec<ExternalWrapperRecord>,
    /// An owner-stop tombstone anywhere in the chain: the owner
    /// declared this lineage done (`record_user_stop`); only a
    /// deliberate revival gesture clears it.
    pub(crate) stopped_by_user: bool,
}

impl ResumeLineage {
    /// True when the walk found any durable wrapper trace — the
    /// external-lineage discriminator: without one there is no
    /// successor chain to wait on.
    pub(crate) fn has_wrapper_history(&self) -> bool {
        !self.wrapper_records.is_empty()
    }

    /// The lineage tip a consumer should follow PAST the given ended
    /// sessions: the preferred (active, newest) wrapper record whose
    /// session is not excluded. `None` means the lineage is quiet — no
    /// admitted successor is visible in durable state (yet). Only
    /// `Active` rows qualify: a superseded row is a dead incarnation,
    /// never a successor to re-attach to.
    pub(crate) fn successor_tip(&self, exclude: &[&str]) -> Option<&ExternalWrapperRecord> {
        self.wrapper_records.iter().find(|record| {
            record.state == WrapperState::Active
                && !exclude.contains(&record.intendant_session_id.as_str())
        })
    }
}

/// Resolve the transitive resume lineage of `seeds` (typically one
/// session id plus its known aliases). Breadth-first over two durable
/// edge kinds, deduplicated and bounded:
///
/// - session → the backend conversations its OWN records tie it to
///   ([`recorded_backend_conversations_in_home`]: id-form, its session
///   dir's identity facts, wrapper-index rows), then conversation → its
///   indexed wrapper sessions (a resume registers the successor under
///   the resumed conversation via the eager identity, which is the edge
///   that chains wrapper generations);
/// - conversation → its `retired_successor` conversation (an edit
///   branch replaces the conversation itself, so no shared wrapper row
///   exists to follow).
///
/// Never an unrelated session: every hop derives from the seed's own
/// recorded identity or from wrapper-index rows of a conversation
/// already in the closure.
pub(crate) fn resolve_resume_lineage(home: &Path, seeds: &[&str]) -> ResumeLineage {
    resolve_resume_lineage_with_dir_hints(home, seeds, &[])
}

/// [`resolve_resume_lineage`] with log dirs the caller already knows
/// (`(session id, dir)` pairs — e.g. the catalog row's own dir behind a
/// serve-time lineage join). Hints are an I/O shortcut, never new
/// authority: an id resolves to the same dir the probe would find (a
/// wrapper dir is named by its id), and ids without a hint still resolve
/// through [`session_log_dir_for_id_in_home`] — but each closure record
/// contributes its `log_path` as a further hint, so a hinted walk never
/// pays the probe's O(store) directory-scan fallback per hop.
pub(crate) fn resolve_resume_lineage_with_dir_hints(
    home: &Path,
    seeds: &[&str],
    dir_hints: &[(&str, &Path)],
) -> ResumeLineage {
    let mut lineage = ResumeLineage {
        candidates: Vec::new(),
        wrapper_records: Vec::new(),
        stopped_by_user: false,
    };
    let mut known_dirs: HashMap<String, PathBuf> = dir_hints
        .iter()
        .filter(|(id, _)| !id.trim().is_empty())
        .map(|(id, dir)| (id.trim().to_string(), dir.to_path_buf()))
        .collect();
    let mut seen_sessions: HashSet<String> = HashSet::new();
    let mut seen_conversations: HashSet<(String, String)> = HashSet::new();
    let mut session_frontier: Vec<String> = seeds
        .iter()
        .map(|seed| seed.trim().to_string())
        .filter(|seed| !seed.is_empty())
        .collect();

    let mut cursor = 0;
    while cursor < session_frontier.len() {
        let session_id = session_frontier[cursor].clone();
        cursor += 1;
        if !seen_sessions.insert(session_id.clone()) {
            continue;
        }
        if seen_sessions.len() > MAX_LINEAGE_SESSIONS {
            break;
        }
        // A known dir (hint or closure record) skips the id→dir probe;
        // hintless ids take the probing composition unchanged.
        let conversations = match known_dirs.get(&session_id) {
            Some(dir) => {
                recorded_backend_conversations_in_home_with_dir(home, &session_id, Some(dir))
            }
            None => recorded_backend_conversations_in_home(home, &session_id),
        };
        for (source, conversation_id) in conversations {
            // Conversation frontier, itself chained by retirement edges.
            let mut conversation_frontier = vec![conversation_id];
            while let Some(conversation_id) = conversation_frontier.pop() {
                if !seen_conversations.insert((source.clone(), conversation_id.clone())) {
                    continue;
                }
                if seen_conversations.len() > MAX_LINEAGE_CONVERSATIONS {
                    break;
                }
                lineage
                    .candidates
                    .push((source.clone(), conversation_id.clone()));
                for record in wrappers_for(home, &source, &conversation_id) {
                    known_dirs
                        .entry(record.intendant_session_id.clone())
                        .or_insert_with(|| PathBuf::from(&record.log_path));
                    lineage
                        .candidates
                        .push((source.clone(), record.intendant_session_id.clone()));
                    if session_frontier.len() < MAX_LINEAGE_SESSIONS {
                        session_frontier.push(record.intendant_session_id.clone());
                    }
                    lineage.wrapper_records.push(record);
                }
                let (stopped_at, successor) = lineage_tombstones(home, &source, &conversation_id);
                if stopped_at.is_some() {
                    lineage.stopped_by_user = true;
                }
                if let Some(successor) = successor {
                    conversation_frontier.push(successor);
                }
            }
        }
    }

    lineage.wrapper_records.sort_by(wrapper_preference);
    lineage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_log::SessionLog;
    use std::path::PathBuf;

    fn logs_root(home: &Path) -> PathBuf {
        crate::platform::intendant_home_in(home).join("logs")
    }

    /// A wrapper log dir announcing its backend conversation(s), which
    /// is exactly how live wrappers persist lineage (the identity event
    /// also writes the wrapper-index row).
    fn announce(home: &Path, wrapper: &str, source: &str, backend_ids: &[&str]) {
        let mut log = SessionLog::open(logs_root(home).join(wrapper)).unwrap();
        log.write_meta(None, None);
        for backend_id in backend_ids {
            log.session_identity(wrapper, source, backend_id);
        }
    }

    /// The shared-resolver pin: ONE walk serves both consumers — the
    /// answer-delivery arm's candidate order (the seed's own
    /// conversation and wrappers lead, successors follow) and the
    /// scheduler's tip reduction (`successor_tip` resolves PAST the
    /// ended session to the newest active wrapper, across BOTH edge
    /// kinds and at depth > 1). Both call sites route through
    /// [`resolve_resume_lineage`]; this test pins the walk's reach so a
    /// private re-implementation of either half has nothing left to
    /// add.
    #[test]
    fn lineage_resolver_shared_with_answer_delivery() {
        let home = tempfile::tempdir().unwrap();

        // Chain: wrapper-old announced b1; wrapper-mid resumed b1 (eager
        // row) and upgraded to its own b2; wrapper-new resumed b2. Two
        // wrapper generations plus an identity upgrade — the resolver
        // must reach wrapper-new from wrapper-old (depth 2, no
        // blindness).
        announce(home.path(), "wrapper-old", "claude-code", &["b1-old"]);
        announce(
            home.path(),
            "wrapper-mid",
            "claude-code",
            &["b1-old", "b2-mid"],
        );
        announce(home.path(), "wrapper-new", "claude-code", &["b2-mid"]);

        let lineage = resolve_resume_lineage(home.path(), &["wrapper-old"]);

        // Delivery-arm shape: the seed's OWN hop leads (its id-form
        // probes, then its recorded conversation and that conversation's
        // wrappers), transitive hops follow — and every chain wrapper is
        // a candidate.
        assert_eq!(
            lineage.candidates.first().map(|(_, id)| id.as_str()),
            Some("wrapper-old"),
            "the seed's own id-form probe must lead the candidate order"
        );
        let position = |needle: &str| {
            lineage
                .candidates
                .iter()
                .position(|(source, id)| source == "claude-code" && id == needle)
                .unwrap_or_else(|| panic!("candidate list must cover {needle}"))
        };
        assert!(
            position("b1-old") < position("wrapper-mid")
                && position("wrapper-mid") < position("b2-mid")
                && position("b2-mid") < position("wrapper-new"),
            "the seed's conversation and wrappers precede transitive hops: {:?}",
            lineage.candidates
        );

        // Scheduler shape: the tip past the ended wrapper is the newest
        // ACTIVE record — wrapper-new, two hops out.
        let tip = lineage
            .successor_tip(&["wrapper-old"])
            .expect("the chain has a live successor");
        assert_eq!(tip.intendant_session_id, "wrapper-new");
        assert!(!lineage.stopped_by_user);

        // The retirement edge (an edit branch replaces the conversation
        // itself; the child shares no wrapper row with the parent) also
        // chains: without it the child is unreachable.
        let edit_home = tempfile::tempdir().unwrap();
        announce(
            edit_home.path(),
            "wrapper-parent",
            "claude-code",
            &["b1-parent"],
        );
        announce(
            edit_home.path(),
            "wrapper-child",
            "claude-code",
            &["b2-child"],
        );
        crate::external_wrapper_index::record_lineage_retired(
            edit_home.path(),
            "claude-code",
            "b1-parent",
            "b2-child",
        )
        .unwrap();
        let lineage = resolve_resume_lineage(edit_home.path(), &["wrapper-parent"]);
        let tip = lineage
            .successor_tip(&["wrapper-parent"])
            .expect("the retirement edge reaches the edit-branch child");
        assert_eq!(tip.intendant_session_id, "wrapper-child");
    }

    /// Dir hints are an I/O shortcut, never a semantic input: a hinted
    /// walk resolves the exact same closure as the probing walk — same
    /// candidate order, same records, same flags.
    #[test]
    fn dir_hints_change_no_outcomes() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "hint-old", "claude-code", &["hb1"]);
        announce(home.path(), "hint-mid", "claude-code", &["hb1", "hb2"]);
        announce(home.path(), "hint-new", "claude-code", &["hb2"]);

        let probed = resolve_resume_lineage(home.path(), &["hint-old"]);
        let hinted_dir = logs_root(home.path()).join("hint-old");
        let hinted = resolve_resume_lineage_with_dir_hints(
            home.path(),
            &["hint-old"],
            &[("hint-old", hinted_dir.as_path())],
        );
        assert_eq!(hinted.candidates, probed.candidates);
        assert_eq!(
            hinted
                .wrapper_records
                .iter()
                .map(|record| record.intendant_session_id.clone())
                .collect::<Vec<_>>(),
            probed
                .wrapper_records
                .iter()
                .map(|record| record.intendant_session_id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(hinted.stopped_by_user, probed.stopped_by_user);
        assert_eq!(
            hinted.successor_tip(&["hint-old"]).map(|r| r.intendant_session_id.clone()),
            probed.successor_tip(&["hint-old"]).map(|r| r.intendant_session_id.clone()),
        );
    }

    /// No recorded wrapper history (a native session id) resolves to an
    /// empty lineage: no candidates beyond the id-form probes, no
    /// wrapper records, no tip — the discriminator consumers classify
    /// immediately on.
    #[test]
    fn native_session_has_no_wrapper_history() {
        let home = tempfile::tempdir().unwrap();
        let lineage = resolve_resume_lineage(home.path(), &["sess-native"]);
        assert!(!lineage.has_wrapper_history());
        assert!(lineage.successor_tip(&[]).is_none());
        assert!(!lineage.stopped_by_user);
    }

    /// An owner stop anywhere in the chain surfaces as
    /// `stopped_by_user` — quiet by decree, whatever rows remain
    /// active.
    #[test]
    fn stop_tombstone_surfaces_from_the_chain() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-one", "claude-code", &["b1-stop"]);
        crate::external_wrapper_index::record_user_stop(home.path(), "claude-code", "b1-stop")
            .unwrap();
        let lineage = resolve_resume_lineage(home.path(), &["wrapper-one"]);
        assert!(lineage.has_wrapper_history());
        assert!(lineage.stopped_by_user);
    }

    /// A `retired_successor` cycle (corrupt index) terminates under the
    /// walk bounds instead of spinning.
    #[test]
    fn cyclic_retirement_edges_terminate() {
        let home = tempfile::tempdir().unwrap();
        announce(home.path(), "wrapper-a", "claude-code", &["b-cycle-1"]);
        announce(home.path(), "wrapper-b", "claude-code", &["b-cycle-2"]);
        crate::external_wrapper_index::record_lineage_retired(
            home.path(),
            "claude-code",
            "b-cycle-1",
            "b-cycle-2",
        )
        .unwrap();
        crate::external_wrapper_index::record_lineage_retired(
            home.path(),
            "claude-code",
            "b-cycle-2",
            "b-cycle-1",
        )
        .unwrap();
        let lineage = resolve_resume_lineage(home.path(), &["wrapper-a"]);
        assert!(lineage.has_wrapper_history());
    }
}
