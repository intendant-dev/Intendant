//! Collision radar — the detection half (Track C, C2; ruled §2.1/§2.7).
//!
//! A periodic daemon task maintains per-space radar state from three
//! zero-LLM inputs — detection is deterministic and never waits on a
//! model (§2.1, binding):
//!
//! - **declared working sets**: the bus `sessions/` scan (rule-5
//!   liveness posture, under the §1.6 whole-space read budget). A
//!   stale declaration (mtime > 45 min) is *marked* in presence and no
//!   longer trusted as proof of a live session, but its dirty set
//!   still participates in overlap as `declared` evidence — a claimed
//!   working set outlives a missed heartbeat (C2 brief, pinned);
//! - **observed dirty sets**: `git status` over the sessions the
//!   daemon actually supervises — the published `GitVitalsTargets`
//!   registry — reusing the vitals prober machinery verbatim
//!   ([`GIT_STATUS_ARGS`], [`parse_status_v2`], the 10 s [`run_git`]
//!   anti-wedge ceiling, per-toplevel caching). Sessions are seen by
//!   git-scan detection whether or not they declared (§2.8);
//! - **open-PR file sets**: `gh pr list --json number,files`, cached
//!   at least [`PR_CACHE_MIN_MS`] per space and degraded silently when
//!   `gh` is absent or fails (§2.1). Never invoked by unit tests —
//!   the pure computation takes injected sets.
//!
//! Severity per §2.7: **ALERT** is a path dirty in two working sets
//! (declared∪observed × declared∪observed, cross-session) or in one
//! live set ∩ an open PR's files; everything else — presence, stale
//! peers, unread messages, invalid counts — is ambient. Snapshots are
//! plain serializable values computed by a pure function over injected
//! inputs (hermetic tests) and published per space like the
//! `publish_git_vitals_targets` precedent; `render.rs` turns one
//! snapshot into one session's §2.2 block, and the daemon writes
//! deduplicated radar notes to flagged parties (§2.8, `messages.rs`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use super::declarations::{self, SessionDeclaration};
use super::lifecycle::writer_id_for_session;
use super::messages::{self, MessageMeta, MessageSpace, RadarNoteInput};
use super::scan::{self, ReadBudget};
use super::{sanitize_key, CoordinationError, MAX_SCAN_ENTRIES};
use crate::session_vitals::{
    parse_status_v2, run_git, GitVitalsTargets, GIT_PROBE_TIMEOUT, GIT_STATUS_ARGS,
};

/// Detection cadence. The protocol names no radar cadence, so the task
/// rides the same tick as the vitals producer whose git machinery it
/// reuses (`session_vitals::PROBE_INTERVAL`) — a local ref read per
/// distinct checkout plus two bounded directory scans per space.
pub(crate) const RADAR_TICK: std::time::Duration = std::time::Duration::from_secs(5);
/// §1.6: whole-space read budget per radar pass — 512 files / 8 MiB,
/// shared across both liveness kinds of one space.
pub(crate) const RADAR_PASS_FILE_BUDGET: usize = 512;
pub(crate) const RADAR_PASS_BYTE_BUDGET: u64 = 8 * 1024 * 1024;
/// §2.1: open-PR file sets are cached at least this long (failures
/// included, so a broken `gh` is retried at the same gentle pace).
pub(crate) const PR_CACHE_MIN_MS: u64 = 5 * 60 * 1000;
/// Open PRs / files-per-PR accepted from `gh` (defensive parse bound).
const MAX_PRS: usize = 64;
const MAX_PR_FILES: usize = 1024;

/// One session's presence in a space, from its declaration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RadarSessionPresence {
    pub writer_id: String,
    pub backend: Option<String>,
    pub stale: bool,
}

/// ALERT: `path` is in both `a`'s and `b`'s working set (`a < b`,
/// cross-session). `declared`/`git` say which evidence kinds back the
/// overlap across the two sides.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RadarPairOverlap {
    pub path: String,
    pub a: String,
    pub b: String,
    pub declared: bool,
    pub git: bool,
}

/// ALERT: `path` is in `writer`'s working set and in open PR `pr`'s
/// file set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RadarPrOverlap {
    pub path: String,
    pub writer: String,
    pub pr: u32,
}

/// Existence-only message metadata (writer + id + recipient — NEVER
/// body text; §9 verbatim).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RadarMessageMeta {
    pub writer: String,
    pub id: String,
    pub to: Option<String>,
}

/// One space's radar state: a plain value the renderer and the
/// delivery lanes read. Every collection is sorted — identical inputs
/// produce an identical (byte-identical once rendered) snapshot.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub(crate) struct SpaceRadarSnapshot {
    pub space_key: String,
    /// When the radar computed this snapshot. Never rendered (the
    /// block must hash identically across quiet ticks) — freshness
    /// metadata for diagnostics/dashboard lanes.
    pub computed_ms: u64,
    pub sessions: Vec<RadarSessionPresence>,
    pub pair_overlaps: Vec<RadarPairOverlap>,
    pub pr_overlaps: Vec<RadarPrOverlap>,
    pub messages: Vec<RadarMessageMeta>,
    /// Entries ignored loudly across the pass: scan rejections (§1.7
    /// rule-5 counts, read-budget drops) plus every token that failed
    /// its §2.3 grammar during computation.
    pub invalid: u64,
}

/// One supervised session's observed git-dirty set (toplevel-relative
/// paths, raw — the computation validates the grammar).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedSet {
    pub writer_id: String,
    pub paths: BTreeSet<String>,
}

/// One open PR's changed-file set (raw paths from `gh`; validated at
/// computation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrFileSet {
    pub number: u32,
    pub paths: BTreeSet<String>,
}

/// Injected inputs for one space's computation — everything the pure
/// function needs, nothing read from the environment.
pub(crate) struct RadarSpaceInputs<'a> {
    pub space_key: &'a str,
    pub declarations: &'a [SessionDeclaration],
    pub observed: &'a [ObservedSet],
    pub messages: &'a [MessageMeta],
    pub pr_files: &'a [PrFileSet],
    /// Named rejections the scans already counted (rule-5 liveness
    /// amendment) — folded into the snapshot's `invalid` total.
    pub scan_invalid: usize,
}

/// The §2.7 overlap computation, pure and deterministic: working set =
/// declared ∪ observed per writer; a path in two writers' sets is a
/// pair ALERT; a path in one writer's set ∩ an open PR's files is a PR
/// ALERT. Tokens outside their §2.3 grammar never enter a set — they
/// are counted into `invalid` (loud, never silent).
pub(crate) fn compute_space_snapshot(
    inputs: &RadarSpaceInputs<'_>,
    now_ms: u64,
) -> SpaceRadarSnapshot {
    let mut invalid = inputs.scan_invalid as u64;

    // Presence: declarations only (a registry-only session has no bus
    // presence to describe; its overlaps still name it below).
    let mut sessions: Vec<RadarSessionPresence> = inputs
        .declarations
        .iter()
        .map(|d| RadarSessionPresence {
            writer_id: d.id.clone(),
            backend: d.backend.clone(),
            stale: d.stale,
        })
        .collect();
    sessions.sort_by(|x, y| x.writer_id.cmp(&y.writer_id));
    invalid += inputs
        .declarations
        .iter()
        .map(|d| d.dirty_dropped as u64)
        .sum::<u64>();

    // Working sets: declared ∪ observed per writer, grammar-gated.
    #[derive(Default)]
    struct Sets {
        declared: BTreeSet<String>,
        git: BTreeSet<String>,
    }
    let mut sets: BTreeMap<String, Sets> = BTreeMap::new();
    for d in inputs.declarations {
        let entry = sets.entry(d.id.clone()).or_default();
        for p in &d.dirty {
            if scan::valid_rel_path(p) {
                entry.declared.insert(p.clone());
            } else {
                invalid += 1;
            }
        }
    }
    for o in inputs.observed {
        if o.writer_id.is_empty() || sanitize_key(&o.writer_id) != o.writer_id {
            invalid += 1;
            continue;
        }
        let entry = sets.entry(o.writer_id.clone()).or_default();
        for raw in &o.paths {
            // git spends one line on a whole untracked directory
            // (`dir/`); normalize the trailing slash before the
            // grammar gate.
            let p = raw.trim_end_matches('/');
            if scan::valid_rel_path(p) {
                entry.git.insert(p.to_string());
            } else {
                invalid += 1;
            }
        }
    }
    let alls: BTreeMap<&str, BTreeSet<&str>> = sets
        .iter()
        .map(|(w, s)| {
            let all: BTreeSet<&str> = s
                .declared
                .iter()
                .map(String::as_str)
                .chain(s.git.iter().map(String::as_str))
                .collect();
            (w.as_str(), all)
        })
        .collect();
    let writers: Vec<&str> = alls.keys().copied().collect();

    // Pair overlaps (cross-session — a session is never in conflict
    // with itself).
    let mut pair_overlaps = Vec::new();
    for i in 0..writers.len() {
        for j in i + 1..writers.len() {
            let (wa, wb) = (writers[i], writers[j]);
            for path in alls[wa].intersection(&alls[wb]) {
                let declared =
                    sets[wa].declared.contains(*path) || sets[wb].declared.contains(*path);
                let git = sets[wa].git.contains(*path) || sets[wb].git.contains(*path);
                pair_overlaps.push(RadarPairOverlap {
                    path: (*path).to_string(),
                    a: wa.to_string(),
                    b: wb.to_string(),
                    declared,
                    git,
                });
            }
        }
    }
    pair_overlaps.sort_by(|x, y| (&x.path, &x.a, &x.b).cmp(&(&y.path, &y.a, &y.b)));

    // PR overlaps: one live set ∩ an open PR's files.
    let mut pr_sets: Vec<(u32, BTreeSet<&str>)> = Vec::new();
    for pr in inputs.pr_files {
        let mut valid_paths = BTreeSet::new();
        for p in &pr.paths {
            if scan::valid_rel_path(p) {
                valid_paths.insert(p.as_str());
            } else {
                invalid += 1;
            }
        }
        pr_sets.push((pr.number, valid_paths));
    }
    pr_sets.sort_by_key(|(n, _)| *n);
    let mut pr_overlaps = Vec::new();
    for (writer, all) in &alls {
        for (number, paths) in &pr_sets {
            for path in all.intersection(paths) {
                pr_overlaps.push(RadarPrOverlap {
                    path: (*path).to_string(),
                    writer: writer.to_string(),
                    pr: *number,
                });
            }
        }
    }
    pr_overlaps.sort_by(|x, y| (&x.path, &x.writer, x.pr).cmp(&(&y.path, &y.writer, y.pr)));

    // Messages: existence-only, live entries only (expiry is advisory
    // until GC, but the radar stops surfacing an expired note).
    let mut messages: Vec<RadarMessageMeta> = inputs
        .messages
        .iter()
        .filter(|m| !m.expired)
        .map(|m| RadarMessageMeta {
            writer: m.writer.clone(),
            id: m.id.clone(),
            to: m.to.clone(),
        })
        .collect();
    messages.sort_by(|x, y| (&x.writer, &x.id).cmp(&(&y.writer, &y.id)));

    SpaceRadarSnapshot {
        space_key: inputs.space_key.to_string(),
        computed_ms: now_ms,
        sessions,
        pair_overlaps,
        pr_overlaps,
        messages,
        invalid,
    }
}

/// One space's bus read for a radar pass: both liveness kinds under
/// one §1.6 whole-space budget, rejections counted (rule 5). Missing
/// kind directories are an empty space, never an error; scan-bound
/// overflow and real I/O trouble stay errors the caller surfaces.
pub(crate) struct SpaceBusRead {
    pub declarations: Vec<SessionDeclaration>,
    pub messages: Vec<MessageMeta>,
    pub scan_invalid: usize,
}

pub(crate) fn read_space_bus(
    space_dir: &Path,
    now_ms: u64,
) -> Result<SpaceBusRead, CoordinationError> {
    let mut budget = ReadBudget::new(RADAR_PASS_FILE_BUDGET, RADAR_PASS_BYTE_BUDGET);
    let decls = declarations::scan_dir_budgeted(&space_dir.join("sessions"), now_ms, &mut budget)?;
    let msgs = messages::scan_meta_dir_budgeted(&space_dir.join("messages"), now_ms, &mut budget)?;
    Ok(SpaceBusRead {
        scan_invalid: decls.rejected.len() + msgs.rejected.len(),
        declarations: decls.entries,
        messages: msgs.entries,
    })
}

/// Write the deduplicated radar notes for a snapshot's ALERT overlaps
/// (§2.8): one note per distinct overlap set per flagged pair, to each
/// flagged party, under the reserved daemon writer dir. Dedup and the
/// per-recipient cooldown live in [`MessageSpace::write_radar_note`];
/// this groups overlaps into per-pair path sets. Returns the trouble
/// (if any) for the caller's throttled log — a full bus is loud, not
/// fatal.
pub(crate) fn write_space_radar_notes(
    space_dir: &Path,
    space_key: &str,
    snapshot: &SpaceRadarSnapshot,
) -> Vec<String> {
    if snapshot.pair_overlaps.is_empty() && snapshot.pr_overlaps.is_empty() {
        return Vec::new();
    }
    let mut errors = Vec::new();
    let space = match MessageSpace::open(space_dir, space_key) {
        Ok(space) => space,
        Err(e) => return vec![e.to_string()],
    };

    // Pair notes: group per (a, b) into one path set + source union.
    let mut pairs: BTreeMap<(&str, &str), (BTreeSet<&str>, bool, bool)> = BTreeMap::new();
    for o in &snapshot.pair_overlaps {
        let entry = pairs.entry((o.a.as_str(), o.b.as_str())).or_default();
        entry.0.insert(o.path.as_str());
        entry.1 |= o.declared;
        entry.2 |= o.git;
    }
    for ((a, b), (paths, declared, git)) in &pairs {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        for recipient in [a, b] {
            let result = space.write_radar_note(&RadarNoteInput {
                to: recipient,
                parties: &[a, b],
                declared: *declared,
                git: *git,
                pr: None,
                paths: &paths,
                ttl_s: None,
            });
            if let Err(e) = result {
                errors.push(e.to_string());
            }
        }
    }

    // PR notes: group per (writer, pr).
    let mut prs: BTreeMap<(&str, u32), BTreeSet<&str>> = BTreeMap::new();
    for o in &snapshot.pr_overlaps {
        prs.entry((o.writer.as_str(), o.pr))
            .or_default()
            .insert(o.path.as_str());
    }
    for ((writer, pr), paths) in &prs {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_string()).collect();
        let result = space.write_radar_note(&RadarNoteInput {
            to: writer,
            parties: &[writer],
            declared: false,
            git: false,
            pr: Some(*pr),
            paths: &paths,
            ttl_s: None,
        });
        if let Err(e) = result {
            errors.push(e.to_string());
        }
    }
    errors
}

/// The published per-space radar state (`publish_git_vitals_targets`
/// precedent): the daemon task is the single writer; the injection
/// seam and delivery lanes read.
#[derive(Clone, Default)]
pub(crate) struct RadarState {
    spaces: Arc<RwLock<HashMap<String, Arc<SpaceRadarSnapshot>>>>,
}

impl RadarState {
    pub(crate) fn publish_space(&self, snapshot: SpaceRadarSnapshot) {
        self.spaces
            .write()
            .expect("radar state lock")
            .insert(snapshot.space_key.clone(), Arc::new(snapshot));
    }

    /// Drop spaces that stopped existing (dir GC'd, no supervised
    /// members left) so stale overlap state cannot outlive its space.
    pub(crate) fn retain_spaces(&self, live: &HashSet<String>) {
        self.spaces
            .write()
            .expect("radar state lock")
            .retain(|key, _| live.contains(key));
    }

    /// One space's latest snapshot — the injection seam's read
    /// (`render::render_block` consumes it per session).
    pub(crate) fn space(&self, space_key: &str) -> Option<Arc<SpaceRadarSnapshot>> {
        self.spaces
            .read()
            .expect("radar state lock")
            .get(space_key)
            .cloned()
    }

    /// Delivery-lane read (§2.8): the snapshot of the space whose ALERT
    /// overlaps name this writer. A session lives in exactly one space,
    /// so the first hit wins (iteration over the handful of live
    /// spaces); `None` means nothing is alerting on this writer
    /// anywhere — the external steer lane's cheap no-op.
    pub(crate) fn space_with_alerts_for(&self, writer_id: &str) -> Option<Arc<SpaceRadarSnapshot>> {
        self.spaces
            .read()
            .expect("radar state lock")
            .values()
            .find(|snapshot| session_has_alerts(snapshot, writer_id))
            .cloned()
    }
}

/// Whether any ALERT overlap in the snapshot names this writer — the
/// raise/resolve predicate for the §2.8 rail-badge flag and the
/// external lane's "anything to say at all" gate. Ambient content
/// (presence, messages, invalid counts) is invisible here by
/// construction.
pub(crate) fn session_has_alerts(snapshot: &SpaceRadarSnapshot, writer_id: &str) -> bool {
    snapshot
        .pair_overlaps
        .iter()
        .any(|o| o.a == writer_id || o.b == writer_id)
        || snapshot.pr_overlaps.iter().any(|o| o.writer == writer_id)
}

/// One §2.8 rail-badge transition to broadcast as
/// [`crate::event::AppEvent::CoordinationRadar`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RadarFlagTransition {
    pub session_id: String,
    /// The flag's stable identity: the session's space key at raise
    /// time, so a resolve always retracts the raise it pairs with.
    pub id: String,
    pub state: crate::types::CoordinationRadarState,
}

/// Raise/resolve bookkeeping for the §2.8 rail badge: one retractable
/// flag per supervised session, raised while any ALERT overlap names
/// the session, resolved when none does — including when the session
/// disappears from the tick (ended, registry pruned, space GC'd), so a
/// gone session can never hold a raised flag. A pure state machine the
/// task feeds full-tick observations; it returns the transitions to
/// broadcast (raise once, resolve once — no re-raise chatter while a
/// flag stays up).
#[derive(Default)]
pub(crate) struct RadarFlagTracker {
    /// session id → flag id of the outstanding raise.
    raised: HashMap<String, String>,
}

impl RadarFlagTracker {
    /// Fold one tick's complete observation set — every supervised
    /// session the radar saw, with its space key and whether that
    /// space's snapshot names it in an ALERT — and return the
    /// transitions. A session moving spaces mid-raise resolves the old
    /// flag before raising the new one (ids must pair exactly).
    pub(crate) fn observe_tick(
        &mut self,
        observed: &[(String, String, bool)],
    ) -> Vec<RadarFlagTransition> {
        use crate::types::CoordinationRadarState as State;
        let mut transitions = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for (session_id, space_key, alerted) in observed {
            seen.insert(session_id.as_str());
            let outstanding = self.raised.get(session_id).cloned();
            match (outstanding, alerted) {
                (None, true) => {
                    self.raised.insert(session_id.clone(), space_key.clone());
                    transitions.push(RadarFlagTransition {
                        session_id: session_id.clone(),
                        id: space_key.clone(),
                        state: State::Raised,
                    });
                }
                (Some(id), false) => {
                    self.raised.remove(session_id);
                    transitions.push(RadarFlagTransition {
                        session_id: session_id.clone(),
                        id,
                        state: State::Resolved,
                    });
                }
                (Some(id), true) if id != *space_key => {
                    self.raised.insert(session_id.clone(), space_key.clone());
                    transitions.push(RadarFlagTransition {
                        session_id: session_id.clone(),
                        id,
                        state: State::Resolved,
                    });
                    transitions.push(RadarFlagTransition {
                        session_id: session_id.clone(),
                        id: space_key.clone(),
                        state: State::Raised,
                    });
                }
                _ => {}
            }
        }
        // Sessions that vanished from the tick resolve, in stable order.
        let mut gone: Vec<(String, String)> = self
            .raised
            .iter()
            .filter(|(session_id, _)| !seen.contains(session_id.as_str()))
            .map(|(session_id, id)| (session_id.clone(), id.clone()))
            .collect();
        gone.sort();
        for (session_id, id) in gone {
            self.raised.remove(&session_id);
            transitions.push(RadarFlagTransition {
                session_id,
                id,
                state: State::Resolved,
            });
        }
        transitions
    }
}

/// §2.8: the external ALERT steer lane's cooldown window — at most one
/// steer per recipient session per window, regardless of overlap set.
pub(crate) const EXTERNAL_STEER_COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// Delivered-set entries kept per session / sessions kept in the ledger
/// before the cheap clear (the radar task's own cache-cap shape). A
/// clear's worst case is one repeat steer per pair, still cooldown-paced.
const STEER_LEDGER_MAX_SETS: usize = 256;
const STEER_LEDGER_MAX_SESSIONS: usize = 512;

#[derive(Default)]
struct SessionSteerLedger {
    last_steer_ms: u64,
    delivered_sets: HashSet<u64>,
}

/// Spam discipline for the external ALERT steer lane (§2.8), mirroring
/// the radar-note lane's two layers (`messages::write_radar_note`):
/// one steer per **distinct overlap set** per session pair —
/// `set_hash` is the steer's canonical set identity from
/// [`super::render::render_alert_steers`] — plus at most one steer per
/// recipient session per [`EXTERNAL_STEER_COOLDOWN_MS`] regardless of
/// set. A cooldown-suppressed set stays unrecorded, so the periodic
/// consult retries it once the window clears (the note lane's
/// tick-retry shape — deferred, not dropped). Daemon-side state beside
/// the radar task's published snapshots (the ruled altitude); ambient
/// content never reaches this ledger because only ALERT steers carry a
/// set hash at all.
#[derive(Default)]
pub(crate) struct ExternalSteerLedger {
    sessions: std::sync::Mutex<HashMap<String, SessionSteerLedger>>,
}

impl ExternalSteerLedger {
    /// True ADMITS the steer now and records it (set marked delivered,
    /// cooldown restarted) — the caller must then deliver it, mid-turn
    /// via `steer_turn` or queued as the between-turns
    /// `ContextInjection` fallback. False: this exact set was already
    /// delivered, or the session's cooldown is still running.
    pub(crate) fn admit(&self, session_id: &str, set_hash: u64, now_ms: u64) -> bool {
        let mut sessions = self.sessions.lock().expect("steer ledger lock");
        if sessions.len() >= STEER_LEDGER_MAX_SESSIONS && !sessions.contains_key(session_id) {
            sessions.clear();
        }
        let entry = sessions.entry(session_id.to_string()).or_default();
        if entry.delivered_sets.contains(&set_hash) {
            return false;
        }
        if entry.last_steer_ms != 0
            && now_ms.saturating_sub(entry.last_steer_ms) < EXTERNAL_STEER_COOLDOWN_MS
        {
            return false; // cooling down; the set stays unrecorded and retries later
        }
        if entry.delivered_sets.len() >= STEER_LEDGER_MAX_SETS {
            entry.delivered_sets.clear();
        }
        entry.delivered_sets.insert(set_hash);
        entry.last_steer_ms = now_ms;
        true
    }
}

/// The daemon's steer-cooldown ledger (process-wide like the published
/// radar state: both external drain halves — mid-turn and idle — must
/// share one view of what was already delivered).
static EXTERNAL_STEER_LEDGER: OnceLock<ExternalSteerLedger> = OnceLock::new();

pub(crate) fn external_steer_ledger() -> &'static ExternalSteerLedger {
    EXTERNAL_STEER_LEDGER.get_or_init(ExternalSteerLedger::default)
}

/// The daemon's live radar state, published once at startup. Tests
/// never publish — seams take a [`RadarState`] or a snapshot directly.
static PUBLISHED_RADAR_STATE: OnceLock<RadarState> = OnceLock::new();

pub(crate) fn publish_radar_state(state: &RadarState) {
    let _ = PUBLISHED_RADAR_STATE.set(state.clone());
}

/// The published state, when a daemon startup path wired one — where
/// the native injection seam reads each session's space snapshot from
/// (`agent_loop`), and the external ALERT steer lane its alert view
/// (`external_supervision`). `None` in every non-daemon shape, which is
/// exactly the ruled degrade-to-no-op.
pub(crate) fn published_radar_state() -> Option<&'static RadarState> {
    PUBLISHED_RADAR_STATE.get()
}

/// Parse `gh pr list --json number,files` output into file sets. Pure
/// (unit-tested on fixtures); bounds are defensive parse caps, and the
/// path grammar is enforced later at computation.
pub(crate) fn parse_pr_list_json(bytes: &[u8]) -> Option<Vec<PrFileSet>> {
    #[derive(serde::Deserialize)]
    struct GhFile {
        path: String,
    }
    #[derive(serde::Deserialize)]
    struct GhPr {
        number: u32,
        #[serde(default)]
        files: Vec<GhFile>,
    }
    let prs: Vec<GhPr> = serde_json::from_slice(bytes).ok()?;
    let mut sets: Vec<PrFileSet> = prs
        .into_iter()
        .take(MAX_PRS)
        .map(|pr| PrFileSet {
            number: pr.number,
            paths: pr
                .files
                .into_iter()
                .take(MAX_PR_FILES)
                .map(|f| f.path)
                .collect(),
        })
        .collect();
    sets.sort_by_key(|s| s.number);
    Some(sets)
}

/// Observed dirty sets for one space's supervised members, one
/// `git status` per distinct checkout toplevel (the vitals prober's
/// dedup shape). A member outside any checkout, or a wedged/failed
/// git, simply contributes no observed set — declarations still cover
/// it.
async fn collect_observed(
    git_program: &std::ffi::OsStr,
    members: &[(String, PathBuf)],
    toplevel_cache: &mut HashMap<PathBuf, PathBuf>,
) -> Vec<ObservedSet> {
    let mut status_by_toplevel: HashMap<PathBuf, Option<BTreeSet<String>>> = HashMap::new();
    let mut merged: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (writer_id, root) in members {
        let toplevel = match toplevel_cache.get(root) {
            Some(cached) => cached.clone(),
            None => {
                let out = run_git(
                    git_program,
                    GIT_PROBE_TIMEOUT,
                    root,
                    &["rev-parse", "--show-toplevel"],
                )
                .await;
                let Some(out) = out.filter(|o| o.status.success()) else {
                    continue; // not a checkout (or git trouble): no observed set
                };
                let top = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
                if top.as_os_str().is_empty() {
                    continue;
                }
                if toplevel_cache.len() > 256 {
                    toplevel_cache.clear(); // vitals prober's cheap cap
                }
                toplevel_cache.insert(root.clone(), top.clone());
                top
            }
        };
        let dirty = match status_by_toplevel.get(&toplevel) {
            Some(cached) => cached.clone(),
            None => {
                let out =
                    run_git(git_program, GIT_PROBE_TIMEOUT, &toplevel, &GIT_STATUS_ARGS).await;
                let dirty = out.filter(|o| o.status.success()).map(|o| {
                    parse_status_v2(&String::from_utf8_lossy(&o.stdout))
                        .entries
                        .into_iter()
                        .map(|e| e.path)
                        .collect::<BTreeSet<String>>()
                });
                if dirty.is_none() {
                    // The cached toplevel may be stale (checkout gone):
                    // drop it so the next tick re-resolves.
                    toplevel_cache.remove(root);
                }
                status_by_toplevel.insert(toplevel.clone(), dirty.clone());
                dirty
            }
        };
        if let Some(paths) = dirty {
            merged.entry(writer_id.clone()).or_default().extend(paths);
        }
    }
    merged
        .into_iter()
        .map(|(writer_id, paths)| ObservedSet { writer_id, paths })
        .collect()
}

/// `gh` under the same anti-wedge guards as [`run_git`] (kill_on_drop
/// + timeout); `None` on any trouble — the caller degrades silently.
async fn run_gh(cwd: &Path, args: &[&str]) -> Option<std::process::Output> {
    let output = tokio::process::Command::new("gh")
        .current_dir(cwd)
        .args(args)
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(GIT_PROBE_TIMEOUT, output)
        .await
        .ok()?
        .ok()
}

struct PrCacheEntry {
    fetched_ms: u64,
    sets: Arc<Vec<PrFileSet>>,
}

/// The periodic detection task's state. Constructed once at spawn with
/// real paths; every tick reads the bus + registry and publishes.
struct RadarTask {
    coordination_root: PathBuf,
    state: RadarState,
    targets: GitVitalsTargets,
    /// The daemon event bus, for the §2.8 rail-badge transitions
    /// ([`RadarFlagTracker`] → `AppEvent::CoordinationRadar`).
    bus: crate::event::EventBus,
    flags: RadarFlagTracker,
    git_program: std::ffi::OsString,
    /// `gh` participation — disabled only in tests via [`RadarTask`]
    /// construction (unit tests never spawn the task at all).
    gh_enabled: bool,
    space_key_cache: HashMap<PathBuf, String>,
    toplevel_cache: HashMap<PathBuf, PathBuf>,
    pr_cache: HashMap<String, PrCacheEntry>,
    /// Last logged trouble per space — the tick logs on CHANGE, never
    /// per tick (a broken space must be loud once, not a 5 s drumbeat).
    last_trouble: HashMap<String, String>,
}

impl RadarTask {
    fn space_key_for(&mut self, root: &Path) -> String {
        if let Some(cached) = self.space_key_cache.get(root) {
            return cached.clone();
        }
        let key = super::space_key(root);
        if self.space_key_cache.len() > 256 {
            self.space_key_cache.clear();
        }
        self.space_key_cache.insert(root.to_path_buf(), key.clone());
        key
    }

    async fn pr_sets(
        &mut self,
        space_key: &str,
        roots: &[PathBuf],
        now_ms: u64,
    ) -> Arc<Vec<PrFileSet>> {
        if !self.gh_enabled {
            return Arc::new(Vec::new());
        }
        if let Some(cached) = self.pr_cache.get(space_key) {
            if now_ms.saturating_sub(cached.fetched_ms) < PR_CACHE_MIN_MS {
                return cached.sets.clone();
            }
        }
        let mut sets = Vec::new();
        if let Some(cwd) = roots.first() {
            let fetched = run_gh(
                cwd,
                &["pr", "list", "--json", "number,files", "--limit", "50"],
            )
            .await
            .filter(|out| out.status.success())
            .and_then(|out| parse_pr_list_json(&out.stdout));
            sets = fetched.unwrap_or_default(); // silent degrade (§2.1)
        }
        let sets = Arc::new(sets);
        self.pr_cache.insert(
            space_key.to_string(),
            PrCacheEntry {
                fetched_ms: now_ms,
                sets: sets.clone(),
            },
        );
        sets
    }

    fn note_trouble(&mut self, key: String, trouble: Option<String>) {
        match trouble {
            Some(msg) => {
                if self.last_trouble.get(&key) != Some(&msg) {
                    eprintln!("[coordination] radar {key}: {msg}");
                    self.last_trouble.insert(key, msg);
                }
            }
            None => {
                self.last_trouble.remove(&key);
            }
        }
    }

    /// Space dirs present on disk (the GC walk's shape: bounded, dirs
    /// only, dot entries skipped; names outside the space-key grammar
    /// are foreign and inert).
    fn disk_spaces(&self) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        let Ok(entries) = std::fs::read_dir(&self.coordination_root) else {
            return keys;
        };
        for entry in entries.take(MAX_SCAN_ENTRIES).flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.')
                || name.is_empty()
                || name.len() > 96
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                continue;
            }
            let is_dir = std::fs::symlink_metadata(entry.path())
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir {
                keys.insert(name);
            }
        }
        keys
    }

    async fn tick(&mut self) {
        let now_ms = super::now_ms();
        // Registry members grouped by space: every supervised session
        // participates in git-scan detection, declared or not (§2.8).
        // Session ids ride along for the rail-badge flags — the bus
        // works in writer ids, the dashboard in session ids.
        let mut members_by_space: BTreeMap<String, Vec<(String, String, PathBuf)>> =
            BTreeMap::new();
        for (session_id, root) in self.targets.snapshot() {
            let key = self.space_key_for(&root);
            let writer_id = writer_id_for_session(&session_id);
            members_by_space
                .entry(key)
                .or_default()
                .push((session_id, writer_id, root));
        }
        for members in members_by_space.values_mut() {
            members.sort();
            members.dedup();
        }
        let mut keys: BTreeSet<String> = members_by_space.keys().cloned().collect();
        keys.extend(self.disk_spaces());

        let mut live: HashSet<String> = HashSet::new();
        // (session id, space key, alerted) per supervised session — the
        // full-tick observation set the flag tracker folds at the end.
        let mut flag_observations: Vec<(String, String, bool)> = Vec::new();
        for key in keys {
            let space_dir = self.coordination_root.join(&key);
            let members = members_by_space.remove(&key).unwrap_or_default();
            let bus = match read_space_bus(&space_dir, now_ms) {
                Ok(bus) => bus,
                Err(e) => {
                    // Corruption-grade trouble: keep the last snapshot
                    // (don't blind consumers on a transient) and log on
                    // change. Flags follow the retained view for the
                    // same reason — a transient must not flap them.
                    self.note_trouble(format!("{key}/bus"), Some(e.to_string()));
                    let retained = self.state.space(&key);
                    for (session_id, writer_id, _) in &members {
                        let alerted = retained
                            .as_deref()
                            .is_some_and(|s| session_has_alerts(s, writer_id));
                        flag_observations.push((session_id.clone(), key.clone(), alerted));
                    }
                    live.insert(key);
                    continue;
                }
            };
            self.note_trouble(format!("{key}/bus"), None);
            let roots: Vec<PathBuf> = members.iter().map(|(_, _, root)| root.clone()).collect();
            let writer_members: Vec<(String, PathBuf)> = members
                .iter()
                .map(|(_, writer_id, root)| (writer_id.clone(), root.clone()))
                .collect();
            let observed =
                collect_observed(&self.git_program, &writer_members, &mut self.toplevel_cache)
                    .await;
            let pr_sets = self.pr_sets(&key, &roots, now_ms).await;
            let snapshot = compute_space_snapshot(
                &RadarSpaceInputs {
                    space_key: &key,
                    declarations: &bus.declarations,
                    observed: &observed,
                    messages: &bus.messages,
                    pr_files: &pr_sets,
                    scan_invalid: bus.scan_invalid,
                },
                now_ms,
            );
            let note_errors = write_space_radar_notes(&space_dir, &key, &snapshot);
            self.note_trouble(
                format!("{key}/notes"),
                (!note_errors.is_empty()).then(|| note_errors.join("; ")),
            );
            for (session_id, writer_id, _) in &members {
                flag_observations.push((
                    session_id.clone(),
                    key.clone(),
                    session_has_alerts(&snapshot, writer_id),
                ));
            }
            self.state.publish_space(snapshot);
            live.insert(key);
        }
        self.state.retain_spaces(&live);
        // Rail-badge transitions (§2.8, R8) — after the publishes, so a
        // consumer woken by a raise reads the fresh snapshots.
        for transition in self.flags.observe_tick(&flag_observations) {
            self.bus.send(crate::event::AppEvent::CoordinationRadar {
                session_id: transition.session_id,
                id: transition.id,
                state: transition.state,
            });
        }
    }
}

/// Spawn the periodic radar task (daemon startup). Resolves the real
/// coordination root at this edge — everything below takes explicit
/// paths — publishes the state handle for the read side, and ticks
/// forever on [`RADAR_TICK`]. `bus` carries the §2.8 rail-badge
/// transitions to the normal outbound broadcaster path.
pub(crate) fn spawn_radar_task(
    targets: GitVitalsTargets,
    bus: crate::event::EventBus,
) -> tokio::task::JoinHandle<()> {
    let state = RadarState::default();
    publish_radar_state(&state);
    let mut task = RadarTask {
        coordination_root: super::paths::coordination_root(&crate::platform::intendant_home()),
        state,
        targets,
        bus,
        flags: RadarFlagTracker::default(),
        git_program: "git".into(),
        gh_enabled: true,
        space_key_cache: HashMap::new(),
        toplevel_cache: HashMap::new(),
        pr_cache: HashMap::new(),
        last_trouble: HashMap::new(),
    };
    tokio::spawn(async move {
        loop {
            task.tick().await;
            tokio::time::sleep(RADAR_TICK).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::declarations::{DeclarationInput, DeclarationSpace};
    use crate::coordination::messages::MessageInput;

    fn declaration(id: &str, dirty: &[&str], stale: bool) -> SessionDeclaration {
        SessionDeclaration {
            id: id.to_string(),
            session: None,
            backend: Some("native".to_string()),
            root: None,
            branch: None,
            created_ms: 1,
            intent: "test".to_string(),
            dirty: dirty.iter().map(|s| s.to_string()).collect(),
            dirty_dropped: 0,
            effective_mtime_ms: 1,
            stale,
        }
    }

    fn observed(writer: &str, paths: &[&str]) -> ObservedSet {
        ObservedSet {
            writer_id: writer.to_string(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn pr(number: u32, paths: &[&str]) -> PrFileSet {
        PrFileSet {
            number,
            paths: paths.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn snap(inputs: &RadarSpaceInputs<'_>) -> SpaceRadarSnapshot {
        compute_space_snapshot(inputs, 1_000_000)
    }

    fn inputs<'a>(
        declarations: &'a [SessionDeclaration],
        observed: &'a [ObservedSet],
        messages: &'a [MessageMeta],
        pr_files: &'a [PrFileSet],
    ) -> RadarSpaceInputs<'a> {
        RadarSpaceInputs {
            space_key: "test-space-0000000000000000",
            declarations,
            observed,
            messages,
            pr_files,
            scan_invalid: 0,
        }
    }

    /// The §2.7 severity classification table, pinned case by case.
    #[test]
    fn severity_classification_table() {
        // declared × declared → pair ALERT, sources declared only.
        let decls = [
            declaration("s-a", &["src/x.rs"], false),
            declaration("s-b", &["src/x.rs"], false),
        ];
        let s = snap(&inputs(&decls, &[], &[], &[]));
        assert_eq!(s.pair_overlaps.len(), 1);
        let o = &s.pair_overlaps[0];
        assert_eq!(
            (o.path.as_str(), o.a.as_str(), o.b.as_str()),
            ("src/x.rs", "s-a", "s-b")
        );
        assert!(o.declared && !o.git);

        // declared × observed → pair ALERT, both sources.
        let decls = [declaration("s-a", &["src/x.rs"], false)];
        let obs = [observed("s-b", &["src/x.rs"])];
        let s = snap(&inputs(&decls, &obs, &[], &[]));
        assert_eq!(s.pair_overlaps.len(), 1);
        assert!(s.pair_overlaps[0].declared && s.pair_overlaps[0].git);

        // observed × observed → pair ALERT, git only.
        let obs = [
            observed("s-a", &["src/x.rs"]),
            observed("s-b", &["src/x.rs"]),
        ];
        let s = snap(&inputs(&[], &obs, &[], &[]));
        assert_eq!(s.pair_overlaps.len(), 1);
        assert!(!s.pair_overlaps[0].declared && s.pair_overlaps[0].git);

        // one live set ∩ open PR files → PR ALERT (declared or observed).
        let decls = [declaration("s-a", &["docs/y.md"], false)];
        let s = snap(&inputs(&decls, &[], &[], &[pr(31, &["docs/y.md"])]));
        assert_eq!(s.pair_overlaps.len(), 0);
        assert_eq!(s.pr_overlaps.len(), 1);
        assert_eq!(
            (
                s.pr_overlaps[0].path.as_str(),
                s.pr_overlaps[0].writer.as_str(),
                s.pr_overlaps[0].pr
            ),
            ("docs/y.md", "s-a", 31)
        );
        let obs = [observed("s-a", &["docs/y.md"])];
        let s = snap(&inputs(&[], &obs, &[], &[pr(31, &["docs/y.md"])]));
        assert_eq!(s.pr_overlaps.len(), 1);

        // A path in ONE set only, or in two PRs with no live set:
        // no overlap at all.
        let decls = [declaration("s-a", &["src/solo.rs"], false)];
        let s = snap(&inputs(
            &decls,
            &[],
            &[],
            &[pr(1, &["other/p.rs"]), pr(2, &["other/p.rs"])],
        ));
        assert!(s.pair_overlaps.is_empty() && s.pr_overlaps.is_empty());

        // A session's own declared ∩ its own observed set is NOT an
        // overlap (cross-session only).
        let decls = [declaration("s-a", &["src/x.rs"], false)];
        let obs = [observed("s-a", &["src/x.rs"])];
        let s = snap(&inputs(&decls, &obs, &[], &[]));
        assert!(s.pair_overlaps.is_empty());
    }

    /// The C2 brief's pinned staleness rule: a >45 min-stale
    /// declaration is marked in presence but its dirty set still
    /// participates in overlap as `declared` evidence.
    #[test]
    fn stale_declarations_are_marked_but_still_participate() {
        let decls = [
            declaration("s-live", &["src/x.rs"], false),
            declaration("s-stale", &["src/x.rs"], true),
        ];
        let s = snap(&inputs(&decls, &[], &[], &[]));
        assert_eq!(s.sessions.iter().filter(|p| p.stale).count(), 1);
        assert_eq!(
            s.pair_overlaps.len(),
            1,
            "stale declared set still overlaps"
        );
        assert!(s.pair_overlaps[0].declared);
    }

    #[test]
    fn hostile_tokens_are_counted_never_kept() {
        let mut d = declaration("s-a", &[], false);
        // Inject paths that bypass the parse gate (hostile snapshot
        // construction): the computation re-validates.
        d.dirty = vec![
            "src/ok.rs".to_string(),
            "../escape".to_string(),
            "has space".to_string(),
            "ansi\u{1b}[31m".to_string(),
        ];
        d.dirty_dropped = 2; // parse-side counts fold in too
        let decls = [d];
        let obs = [
            observed("s-b", &["src/ok.rs", "also bad", "x\u{202e}rtl"]),
            observed("Bad Writer!", &["src/ok.rs"]),
        ];
        let prs = [pr(9, &["src/ok.rs", "-leading-dash"])];
        let s = snap(&inputs(&decls, &obs, &[], &prs));
        // Only the grammar-valid path overlaps; every hostile token
        // and the pre-counted parse drops land in `invalid`.
        assert_eq!(s.pair_overlaps.len(), 1);
        assert_eq!(s.pair_overlaps[0].path, "src/ok.rs");
        assert_eq!(s.pr_overlaps.len(), 2, "both writers ∩ pr#9");
        // 3 hostile declared paths + 2 hostile observed paths + 1
        // hostile writer id + 1 hostile PR path + 2 pre-counted parse
        // drops = 9.
        assert_eq!(s.invalid, 9, "{s:?}");
    }

    /// Zero-LLM determinism (binding): identical inputs in any order
    /// produce an identical snapshot, ordering stable.
    #[test]
    fn computation_is_deterministic_and_order_independent() {
        let d1 = declaration("s-b", &["src/x.rs", "src/y.rs"], false);
        let d2 = declaration("s-a", &["src/x.rs"], false);
        let o1 = observed("s-c", &["src/y.rs", "src/x.rs"]);
        let o2 = observed("s-a", &["src/z.rs"]);
        let m1 = MessageMeta {
            id: "m-2".into(),
            writer: "s-b".into(),
            kind: "message".into(),
            to: None,
            created_ms: 1,
            ttl_s: 60,
            expired: false,
        };
        let m2 = MessageMeta {
            id: "m-1".into(),
            writer: "s-a".into(),
            kind: "message".into(),
            to: Some("s-b".into()),
            created_ms: 1,
            ttl_s: 60,
            expired: false,
        };
        let p1 = pr(7, &["src/x.rs"]);
        let p2 = pr(3, &["src/y.rs"]);

        let forward = [d1.clone(), d2.clone()];
        let backward = [d2, d1];
        let a = snap(&inputs(
            &forward,
            &[o1.clone(), o2.clone()],
            &[m1.clone(), m2.clone()],
            &[p1.clone(), p2.clone()],
        ));
        let b = snap(&inputs(&backward, &[o2, o1], &[m2, m1], &[p2, p1]));
        assert_eq!(a, b, "input order must not matter");
        // Ordering is pinned, not incidental.
        let pair_keys: Vec<(&str, &str, &str)> = a
            .pair_overlaps
            .iter()
            .map(|o| (o.path.as_str(), o.a.as_str(), o.b.as_str()))
            .collect();
        let mut sorted = pair_keys.clone();
        sorted.sort();
        assert_eq!(pair_keys, sorted);
        assert!(a
            .messages
            .windows(2)
            .all(|w| (&w[0].writer, &w[0].id) <= (&w[1].writer, &w[1].id)));
    }

    #[test]
    fn expired_messages_never_surface() {
        let m = MessageMeta {
            id: "m-dead".into(),
            writer: "s-a".into(),
            kind: "message".into(),
            to: None,
            created_ms: 1,
            ttl_s: 60,
            expired: true,
        };
        let s = snap(&inputs(&[], &[], &[m], &[]));
        assert!(s.messages.is_empty());
    }

    #[test]
    fn read_space_bus_charges_one_budget_across_both_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let ds = DeclarationSpace::open(&space_dir, "s").unwrap();
        for i in 0..3 {
            ds.write_own(&DeclarationInput {
                id: &format!("s-{i}"),
                session: None,
                backend: None,
                root: None,
                branch: None,
                intent: "x",
                dirty: &[],
            })
            .unwrap();
        }
        let ms = MessageSpace::open(&space_dir, "s").unwrap();
        ms.write(
            "s-0",
            &MessageInput {
                to: None,
                ttl_s: None,
                body: "note",
            },
        )
        .unwrap();

        let bus = read_space_bus(&space_dir, super::super::now_ms()).unwrap();
        assert_eq!(bus.declarations.len(), 3);
        assert_eq!(bus.messages.len(), 1);
        assert_eq!(bus.scan_invalid, 0);

        // A malformed neighbor is counted, never fatal (rule 5).
        std::fs::write(space_dir.join("sessions/s-junk.md"), "not a doc").unwrap();
        let bus = read_space_bus(&space_dir, super::super::now_ms()).unwrap();
        assert_eq!(bus.declarations.len(), 3);
        assert_eq!(bus.scan_invalid, 1);
    }

    #[test]
    fn radar_notes_flow_from_snapshot_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let decls = [
            declaration("s-a", &["src/x.rs"], false),
            declaration("s-b", &["src/x.rs"], false),
        ];
        let obs = [observed("s-a", &["docs/pr-file.md"])];
        let prs = [pr(42, &["docs/pr-file.md"])];
        let s = snap(&inputs(&decls, &obs, &[], &prs));
        let errors = write_space_radar_notes(&space_dir, "test-space", &s);
        assert!(errors.is_empty(), "{errors:?}");

        let ms = MessageSpace::open(&space_dir, "test-space").unwrap();
        let scan = ms.scan_meta(super::super::now_ms()).unwrap();
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
        // Pair note to each flagged party + one PR note to s-a's set
        // holder... but the PR-note recipient (s-a) was just noted by
        // the pair lane: the recipient cooldown holds it. Two notes.
        assert_eq!(scan.entries.len(), 2, "{:?}", scan.entries);
        let recipients: Vec<Option<&str>> = scan.entries.iter().map(|m| m.to.as_deref()).collect();
        assert!(recipients.contains(&Some("s-a")) && recipients.contains(&Some("s-b")));
        assert!(scan
            .entries
            .iter()
            .all(|m| m.writer == "daemon" && m.kind == "radar-note"));

        // Re-running the same snapshot writes nothing new (set dedup +
        // cooldown).
        let errors = write_space_radar_notes(&space_dir, "test-space", &s);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            ms.scan_meta(super::super::now_ms()).unwrap().entries.len(),
            2
        );

        // No overlaps → no bus dirs are ever created.
        let empty_dir = tmp.path().join("empty-space");
        let quiet = snap(&inputs(&[], &[], &[], &[]));
        assert!(write_space_radar_notes(&empty_dir, "empty", &quiet).is_empty());
        assert!(!empty_dir.exists(), "quiet spaces stay untouched");
    }

    #[test]
    fn pr_list_json_parses_the_gh_shape() {
        let json = br#"[
            {"number": 566, "files": [{"path": "src/a.rs", "additions": 1, "deletions": 2}, {"path": "docs/b.md"}]},
            {"number": 12, "files": []},
            {"number": 90}
        ]"#;
        let sets = parse_pr_list_json(json).unwrap();
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0].number, 12, "sorted by number");
        assert_eq!(sets[2].number, 566);
        assert!(sets[2].paths.contains("src/a.rs") && sets[2].paths.contains("docs/b.md"));
        assert!(parse_pr_list_json(b"not json").is_none());
        assert!(parse_pr_list_json(b"{}").is_none(), "object, not array");
    }

    #[tokio::test]
    async fn observed_sets_come_from_git_status_per_toplevel() {
        let git_ok = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !git_ok {
            eprintln!("SKIPPED: no usable `git` on PATH — observed-set collection DID NOT RUN");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git spawns");
            assert!(status.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("tracked.rs"), "a").unwrap();
        git(&["add", "tracked.rs"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        std::fs::write(repo.join("tracked.rs"), "b").unwrap(); // modified
        std::fs::write(repo.join("untracked.rs"), "c").unwrap(); // untracked

        let members = vec![
            ("s-one".to_string(), repo.clone()),
            ("s-two".to_string(), repo.clone()), // same checkout, shared status
            ("s-none".to_string(), tmp.path().join("not-a-repo")),
        ];
        let mut cache = HashMap::new();
        let observed = collect_observed(std::ffi::OsStr::new("git"), &members, &mut cache).await;
        assert_eq!(observed.len(), 2, "{observed:?}");
        for set in &observed {
            assert!(set.paths.contains("tracked.rs"), "{observed:?}");
            assert!(set.paths.contains("untracked.rs"), "{observed:?}");
        }
        assert!(observed.iter().any(|s| s.writer_id == "s-one"));
        assert!(observed.iter().any(|s| s.writer_id == "s-two"));
    }

    #[test]
    fn radar_state_publishes_and_retains() {
        let state = RadarState::default();
        let mut s = SpaceRadarSnapshot {
            space_key: "alpha-1".into(),
            ..Default::default()
        };
        state.publish_space(s.clone());
        s.space_key = "beta-2".into();
        state.publish_space(s);
        assert!(state.space("alpha-1").is_some());
        let live: HashSet<String> = ["beta-2".to_string()].into_iter().collect();
        state.retain_spaces(&live);
        assert!(state.space("alpha-1").is_none());
        assert!(state.space("beta-2").is_some());
        assert!(state.space("never").is_none());
    }

    #[test]
    fn space_with_alerts_finds_only_alerting_writers() {
        let state = RadarState::default();
        let mut s = SpaceRadarSnapshot {
            space_key: "gamma-3".into(),
            ..Default::default()
        };
        s.sessions.push(RadarSessionPresence {
            writer_id: "s-ambient".into(),
            backend: None,
            stale: false,
        });
        s.pair_overlaps.push(RadarPairOverlap {
            path: "src/x.rs".into(),
            a: "s-hot".into(),
            b: "s-warm".into(),
            declared: true,
            git: false,
        });
        s.pr_overlaps.push(RadarPrOverlap {
            path: "docs/y.md".into(),
            writer: "s-pr".into(),
            pr: 4,
        });
        state.publish_space(s);
        for alerted in ["s-hot", "s-warm", "s-pr"] {
            assert!(
                state.space_with_alerts_for(alerted).is_some(),
                "{alerted} alerts"
            );
        }
        // Present but ambient-only, or absent entirely: no alert view —
        // the external lane sees nothing to say.
        assert!(state.space_with_alerts_for("s-ambient").is_none());
        assert!(state.space_with_alerts_for("s-elsewhere").is_none());
    }

    /// RULED (R8): the rail-badge flag raises once while ALERTs name a
    /// session, retracts when they clear, retracts when the session
    /// disappears, and re-pairs correctly across a space move.
    #[test]
    fn flag_tracker_raises_and_resolves_per_session() {
        use crate::types::CoordinationRadarState as State;
        let mut tracker = RadarFlagTracker::default();
        let obs = |alerted: bool| vec![("sess-1".to_string(), "space-a".to_string(), alerted)];

        assert!(tracker.observe_tick(&obs(false)).is_empty(), "quiet start");
        let raised = tracker.observe_tick(&obs(true));
        assert_eq!(raised.len(), 1);
        assert_eq!(
            (
                raised[0].session_id.as_str(),
                raised[0].id.as_str(),
                raised[0].state
            ),
            ("sess-1", "space-a", State::Raised)
        );
        // Still alerting: NO re-raise chatter.
        assert!(tracker.observe_tick(&obs(true)).is_empty());
        // Cleared: exactly one resolve, same flag id.
        let resolved = tracker.observe_tick(&obs(false));
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            (resolved[0].id.as_str(), resolved[0].state),
            ("space-a", State::Resolved)
        );
        assert!(tracker.observe_tick(&obs(false)).is_empty(), "no double");

        // Raise again, then the session VANISHES from the tick — the
        // sweep retracts (a gone session never holds a raised flag).
        tracker.observe_tick(&obs(true));
        let swept = tracker.observe_tick(&[]);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].state, State::Resolved);

        // A space move mid-raise resolves the old id before raising the
        // new one, so raise/resolve always pair by id.
        tracker.observe_tick(&obs(true));
        let moved =
            tracker.observe_tick(&[("sess-1".to_string(), "space-b".to_string(), true)]);
        assert_eq!(
            moved
                .iter()
                .map(|t| (t.id.as_str(), t.state))
                .collect::<Vec<_>>(),
            vec![("space-a", State::Resolved), ("space-b", State::Raised)]
        );
    }

    /// §2.8 spam discipline for the external steer lane: one steer per
    /// distinct set, a 10-minute per-session cooldown that DEFERS (not
    /// drops) newer sets, and per-session isolation.
    #[test]
    fn external_steer_ledger_dedups_and_cools_down() {
        let ledger = ExternalSteerLedger::default();
        let t0: u64 = 1_000_000;
        assert!(ledger.admit("sess-1", 11, t0), "first set admits");
        assert!(!ledger.admit("sess-1", 11, t0 + 1), "same set never repeats");
        assert!(
            !ledger.admit("sess-1", 22, t0 + 1),
            "different set cools down"
        );
        assert!(
            !ledger.admit("sess-1", 22, t0 + EXTERNAL_STEER_COOLDOWN_MS - 1),
            "still cooling"
        );
        assert!(
            ledger.admit("sess-1", 22, t0 + EXTERNAL_STEER_COOLDOWN_MS),
            "deferred set admits once the window clears"
        );
        assert!(
            !ledger.admit("sess-1", 11, t0 + 10 * EXTERNAL_STEER_COOLDOWN_MS),
            "delivered sets stay delivered across windows"
        );
        // Sessions are independent.
        assert!(ledger.admit("sess-2", 11, t0));
    }

    /// The real task tick end to end (hermetic paths, no gh): a bus
    /// overlap raises the session's flag on the daemon bus; removing
    /// the neighbor's declaration resolves it. This is the Rust-side
    /// emission proof for the retractable `radar` attention kind.
    #[tokio::test]
    async fn radar_task_tick_emits_raise_then_resolve_on_the_bus() {
        use crate::types::CoordinationRadarState as State;
        let tmp = tempfile::tempdir().unwrap();
        let coordination_root = tmp.path().join("coordination");
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let key = crate::coordination::space_key(&proj);
        let space_dir = coordination_root.join(&key);
        let writer = writer_id_for_session("sess-radar-a");
        let ds = DeclarationSpace::open(&space_dir, &key).unwrap();
        let dirty = ["src/hot.rs".to_string()];
        for id in [writer.as_str(), "s-nbr"] {
            ds.write_own(&DeclarationInput {
                id,
                session: None,
                backend: Some("native"),
                root: None,
                branch: None,
                intent: "overlap fixture",
                dirty: &dirty,
            })
            .unwrap();
        }

        let targets = GitVitalsTargets::default();
        targets.register("sess-radar-a", proj.clone());
        let bus = crate::event::EventBus::new();
        let mut bus_rx = bus.subscribe();
        let mut task = RadarTask {
            coordination_root,
            state: RadarState::default(),
            targets,
            bus,
            flags: RadarFlagTracker::default(),
            git_program: "git".into(),
            gh_enabled: false,
            space_key_cache: HashMap::new(),
            toplevel_cache: HashMap::new(),
            pr_cache: HashMap::new(),
            last_trouble: HashMap::new(),
        };

        task.tick().await;
        let raised = loop {
            match bus_rx.try_recv() {
                Ok(crate::event::AppEvent::CoordinationRadar {
                    session_id,
                    id,
                    state,
                }) => break (session_id, id, state),
                Ok(_) => continue,
                Err(e) => panic!("no CoordinationRadar after the alerting tick: {e:?}"),
            }
        };
        assert_eq!(raised, ("sess-radar-a".to_string(), key.clone(), State::Raised));
        // The same tick published the snapshot the delivery lanes read.
        assert!(task.state.space_with_alerts_for(&writer).is_some());

        // Quiet ticks: no chatter.
        task.tick().await;
        // Neighbor leaves; the overlap clears; the flag retracts.
        std::fs::remove_file(space_dir.join("sessions/s-nbr.md")).unwrap();
        task.tick().await;
        let mut transitions = Vec::new();
        while let Ok(event) = bus_rx.try_recv() {
            if let crate::event::AppEvent::CoordinationRadar {
                session_id,
                id,
                state,
            } = event
            {
                transitions.push((session_id, id, state));
            }
        }
        assert_eq!(
            transitions,
            vec![("sess-radar-a".to_string(), key, State::Resolved)],
            "exactly one retraction, nothing between"
        );
    }
}
