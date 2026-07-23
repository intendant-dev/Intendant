//! The agenda op-log store: an append-only JSONL file plus the folded
//! in-memory state. The daemon's control plane owns exactly one
//! [`AgendaStore`] and is its single writer; everything here takes explicit
//! paths (tests thread tempdirs — never the live state root).

use super::types::{
    apply_op, counts, AgendaActor, AgendaCommand, AgendaCounts, AgendaItem, AgendaOp,
    AgendaOpRecord, AgendaPatch, AgendaRefType, AgendaStatus, AGENDA_LOG_VERSION,
    MAX_ANNOTATIONS_PER_ITEM, MAX_BODY_BYTES, MAX_CHILDREN_PER_HUB, MAX_CRITERION_CHARS,
    MAX_PART_OF_DEPTH, MAX_REFS_PER_ITEM, MAX_REF_FILE_HASH_BYTES, MAX_REF_FILE_LOCATOR_CHARS,
    MAX_REF_ID_LOCATOR_CHARS, MAX_REF_LABEL_CHARS, MAX_REF_URL_LOCATOR_CHARS,
    MAX_RELATES_TO_PER_ITEM, MAX_RELIES_ON_PER_ITEM, MAX_SOURCE_CHARS, MAX_TAGS, MAX_TAG_CHARS,
    MAX_TITLE_CHARS, MAX_UNCLEARED_BLOCKERS_PER_ITEM,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Command intake errors. The gateway maps `NotFound` to 404, the two
/// rejection variants to 400, `NotPermitted` to 403; `Io` is a
/// daemon-side 500.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AgendaError {
    #[error("agenda item not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Transition(String),
    /// The rider's named denial: manifest approval/revocation is an
    /// owner-surface act (mirrors Memory P1.2's `authorize_write`).
    #[error(
        "{verb} is an owner-surface act: {actor} actors may propose scheduled sessions \
         but never approve them — ask the owner to review on the dashboard"
    )]
    NotPermitted { verb: &'static str, actor: String },
    #[error("agenda log I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// The op names this build folds. Lines whose `op.type` is not listed are
/// preserved on disk but skipped at load (forward compatibility: a newer
/// build's vocabulary — effects, journal curation — must not brick an older
/// daemon's ledger).
const KNOWN_OPS: [&str; 24] = [
    "add",
    "patch",
    "complete",
    "reopen",
    "retire",
    "answer",
    "dismiss",
    "annotate",
    "set_blocker",
    "clear_blocker",
    "add_relies_on",
    "remove_relies_on",
    "add_ref",
    "remove_ref",
    "add_part_of",
    "remove_part_of",
    "add_relates_to",
    "remove_relates_to",
    "propose_effect",
    "approve_effect",
    "revoke_effect",
    "request_occurrence",
    "record_occurrence",
    "record_ask_delivery",
];

const LOG_FILE: &str = "agenda.jsonl";

/// Default page size for [`AgendaStore::read_ops`] when the caller names
/// none; the clamp ceiling is [`AGENDA_OPS_MAX_LIMIT`].
pub(crate) const AGENDA_OPS_DEFAULT_LIMIT: usize = 500;
/// Hard page-size ceiling for [`AgendaStore::read_ops`].
pub(crate) const AGENDA_OPS_MAX_LIMIT: usize = 2000;

/// One page of the raw op log, as `GET /api/agenda/ops` serves it.
/// Serializes to exactly the response body:
/// `{"ops":[…],"next_since":…,"log_len":…,"filtered":…}`.
#[derive(Debug, serde::Serialize)]
pub(crate) struct AgendaOpsPage {
    /// Served entries, in log order. Each is
    /// `{"seq":N,"known":bool,"op":<the line's JSON, verbatim>}`, or
    /// `{"seq":N,"known":false,"unparseable":true,"raw":"<line>"}` for a
    /// line that is not JSON at all.
    pub(crate) ops: Vec<serde_json::Value>,
    /// Resume cursor: the first seq this scan did not consume — last
    /// returned seq + 1 when the page filled, otherwise `log_len`.
    pub(crate) next_since: u64,
    /// Total lines in the log right now. A value below a client's cursor
    /// means the append-only contract was broken externally (the same
    /// shrink [`AgendaStore::refresh_if_stale`] refolds through).
    pub(crate) log_len: u64,
    /// True when an `item` filter was applied to this page.
    pub(crate) filtered: bool,
}

/// Start-now's default goal statement: the item quoted as data with its id
/// so the spawned session's own (attributed) `ctl` can act on it. The
/// confirm sheet's editable text replaces exactly this part; the mode coda
/// below is appended either way.
pub(crate) fn start_now_goal_statement(item: &AgendaItem) -> String {
    let mut statement = format!("Agenda follow-through for item {}: {}", item.id, item.title);
    if !item.body.trim().is_empty() {
        statement.push_str("\n\nItem body (quoted):\n");
        statement.push_str(&item.body);
    }
    statement
}

/// Goal-run coda: the autonomous follow-through + write-back protocol.
pub(crate) const START_NOW_GOAL_RUN_CODA: &str =
    "\n\nWork the item, then state the outcome plainly — it is written back to \
     the agenda item, and `intendant ctl agenda` from this session records \
     attributed progress (annotate/complete) on it.";

/// Interactive coda: the session opens like a composer session — take
/// stock, then the owner directs the work.
pub(crate) const START_NOW_INTERACTIVE_CODA: &str =
    "\n\nThe owner opened this session interactively from the agenda: take stock \
     of the item, then follow their direction — they are in the loop, so ask \
     rather than assume.";

pub(crate) struct AgendaStore {
    /// The agenda dir this store lives in (op log, blob store).
    dir: PathBuf,
    log_path: PathBuf,
    log: std::fs::File,
    items: BTreeMap<String, AgendaItem>,
    /// The largest item id this store has ever seen — folded from disk or
    /// minted here. Fresh mints are floored against it (same-millisecond
    /// mints increment it), so id order equals creation order across
    /// restarts and refolds, not just within one process's generator.
    /// (A fresh `ulid::Generator` per open is only monotonic within
    /// itself — CI's warm Linux runner reopened the store fast enough to
    /// mint a smaller id in the same millisecond.)
    last_id: Option<ulid::Ulid>,
    /// Records folded from disk plus records appended this process.
    ops: u64,
    /// Load-time lines preserved but not folded (torn tail, unknown op
    /// vocabulary, newer line version). Surfaced so frontends can show that
    /// history holds more than this build renders.
    skipped_lines: u64,
    /// Log bytes reflected in `items` (including any terminator this
    /// process wrote). Concurrent daemons on one home share the log file —
    /// a length mismatch on disk means another instance appended, and
    /// [`Self::refresh_if_stale`] refolds. Appends are `O_APPEND`, so
    /// interleaved single-line writes stay whole.
    folded_len: u64,
}

/// Facts of one scheduled-session occurrence outcome, written back by the
/// scheduler. Bundled because they always travel together through the
/// handle into the store's daemon-only `record_occurrence`.
pub(crate) struct OccurrenceWriteBack<'a> {
    pub(crate) item_id: &'a str,
    pub(crate) effect_id: &'a str,
    pub(crate) occurrence_id: &'a str,
    /// `started` | `completed` | `failed` | `missed` | `unknown`.
    pub(crate) state: &'a str,
    pub(crate) session_id: Option<String>,
    pub(crate) note: Option<String>,
}

/// Fold raw log bytes into derived state: `(items, ops folded, lines skipped)`.
fn fold_bytes(bytes: &[u8]) -> (BTreeMap<String, AgendaItem>, u64, u64) {
    let text = String::from_utf8_lossy(bytes);
    let mut items = BTreeMap::new();
    let mut ops = 0u64;
    let mut skipped_lines = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_record(line) {
            Ok(record) => {
                if let Some(reason) = apply_op(&mut items, &record) {
                    eprintln!("[agenda] fold: {reason}");
                }
                ops += 1;
            }
            Err(reason) => {
                skipped_lines += 1;
                eprintln!("[agenda] skipping log line ({reason}): {line}");
            }
        }
    }
    (items, ops, skipped_lines)
}

impl AgendaStore {
    /// Open (creating if absent) the agenda under `dir`, replaying the op
    /// log into derived state. A file that does not end in a newline (torn
    /// final line from a crash mid-append) is terminated so subsequent
    /// appends start on a fresh line; the torn line itself is preserved and
    /// skipped. Ops are never destroyed or rewritten here.
    pub(crate) fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let log_path = dir.join(LOG_FILE);
        let bytes = match std::fs::read(&log_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let (items, ops, skipped_lines) = fold_bytes(&bytes);
        let mut folded_len = bytes.len() as u64;

        let mut log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&log_path)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            log.write_all(b"\n")?;
            folded_len += 1;
        }
        let last_id = max_item_id(&items);
        let store = Self {
            dir: dir.to_path_buf(),
            log_path,
            log,
            items,
            last_id,
            ops,
            skipped_lines,
            folded_len,
        };
        store.sync_ask_state();
        Ok(store)
    }

    /// Post-fold bookkeeping for persisted rich asks: floor the process
    /// approval-id allocator above every ask id ever folded (a restarted
    /// daemon's counter must never re-mint a persisted rail id), and
    /// reconcile the open-ask registry the supervisor consults.
    fn sync_ask_state(&self) {
        let max_ask_id = self
            .items
            .values()
            .filter_map(|item| item.ask.as_ref().map(|ask| ask.ask_id))
            .max();
        if let Some(max_ask_id) = max_ask_id {
            crate::event::ensure_approval_id_floor(max_ask_id);
        }
        super::ask::sync_open_asks(self.items.values());
    }

    /// Refold when the on-disk log has bytes this store has not seen.
    /// Multiple daemons on one home (the normal topology on a dev box)
    /// share `~/.intendant/agenda`; this keeps their views convergent
    /// without any cross-process coordination beyond `O_APPEND`. Call
    /// before reads and writes — a stat per call, a refold only on change.
    pub(crate) fn refresh_if_stale(&mut self) -> std::io::Result<()> {
        let disk_len = match std::fs::metadata(&self.log_path) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };
        if disk_len == self.folded_len {
            return Ok(());
        }
        // Shorter than folded means the append-only contract was broken
        // externally; refolding what's there is the honest recovery either way.
        let bytes = std::fs::read(&self.log_path)?;
        let (items, ops, skipped_lines) = fold_bytes(&bytes);
        self.items = items;
        self.ops = ops;
        self.skipped_lines = skipped_lines;
        self.folded_len = bytes.len() as u64;
        // Never lower the mint floor: our own last mint is on disk, so the
        // folded max normally covers it, but a shrunk/tampered file must
        // not let a future mint sort below an id we already handed out.
        self.last_id = self.last_id.max(max_item_id(&self.items));
        // Another instance's torn tail: terminate it (as `open` does) so
        // our next append starts on a fresh line.
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            self.log.write_all(b"\n")?;
            self.folded_len += 1;
        }
        // Another instance may have parked or resolved asks.
        self.sync_ask_state();
        Ok(())
    }

    /// Validate a frontend intent against current state, append the durable
    /// op, fold it, and return the item as it now stands. This is the only
    /// external write path — strictness lives here, not in the tolerant
    /// fold. (`record_occurrence` is the one daemon-internal sibling.)
    pub(crate) fn apply_command(
        &mut self,
        mut cmd: AgendaCommand,
        actor: Option<AgendaActor>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        // Validate against the freshest state another instance may have left.
        self.refresh_if_stale()?;
        let source = validate_source(cmd.take_source())?;
        // Rich-ask park: blob commits interleave with the id mint and need
        // rollback on any later failure, so it has its own arm.
        if let AgendaCommand::Ask { questions } = cmd {
            return self.apply_ask(questions, actor, now_ms);
        }
        // Park (G1: optionally with attached refs): one `add` plus one
        // `add_ref` per spec appended under this same lock — its own arm
        // because one command can map to several ops, all-or-nothing.
        if let AgendaCommand::Add {
            kind,
            title,
            body,
            tags,
            due_ms,
            source: _,
            refs,
        } = cmd
        {
            return self.apply_add(kind, title, body, tags, due_ms, refs, actor, source, now_ms);
        }
        // Re-parent gesture (G2, steward override): the NEW placement is
        // validated in full BEFORE the current one is touched, then the
        // primitive remove+add pair appends under this same lock — a
        // refused target never destroys a live placement.
        if let AgendaCommand::Place {
            id,
            under,
            source: _,
        } = cmd
        {
            return self.apply_place(&id, &under, actor, source, now_ms);
        }
        // Owner "run the standing manifest now" (G3-pre): resolves the
        // item's single effect and validates against the approved digest.
        if let AgendaCommand::RequestOccurrence { id } = cmd {
            let (effect_id, digest) = {
                let item = self.require(&id)?;
                let Some(effect) = item.effects.first() else {
                    return Err(AgendaError::NotFound(format!(
                        "{id} has no scheduled session"
                    )));
                };
                (effect.effect_id.clone(), effect.digest.clone())
            };
            return self.request_occurrence_of(&id, &effect_id, &digest, actor, now_ms);
        }
        // Owner start-now (F3): two ops appended under this same lock —
        // its own arm because one command maps to a propose+approve pair.
        if let AgendaCommand::StartNow {
            id,
            goal,
            project_root,
            interactive,
            agent_config,
        } = cmd
        {
            // The raw Option travels: the standing route (G3-pre) must
            // distinguish "absent" from an explicit mode override before
            // the owner-ratified interactive default applies to the mint.
            return self.start_now(
                &id,
                goal.as_deref(),
                project_root,
                interactive,
                agent_config,
                actor,
                now_ms,
            );
        }
        let deletes_blobs = matches!(&cmd, AgendaCommand::Retire { .. });
        let op = self.command_to_op(cmd, now_ms)?;
        let item = self.append_op(op, actor, source, now_ms)?;
        // Retention is tied to the item lifecycle: preview blobs die with
        // RETIREMENT — not completion, because answered questions remain
        // visible in the archive with their previews.
        if deletes_blobs && item.ask.is_some() {
            if let Err(err) = super::blobs::delete_item_blobs(&self.dir, &item.id) {
                eprintln!("[agenda] deleting blobs of retired {}: {err}", item.id);
            }
        }
        Ok(item)
    }

    /// The owner "start session now" gesture (F3): mint a manifest from
    /// the item — goal = title + body quoted as data, with the item id so
    /// the spawned session's own (attributed) `ctl` can act on it — and
    /// append the propose + approve ops atomically under the caller's
    /// lock, the approve binding the digest of exactly the manifest minted
    /// here. `fire_at_ms = now` makes the ordinary scheduler pass (nudged
    /// right after this apply) journal the occurrence and dispatch through
    /// the standard StartTask lane — start-now IS scheduled firing with a
    /// zero-length wait, never a bypass. Revising semantics are the
    /// standing ones: an existing effect keeps its lineage, gets a fresh
    /// digest, and any prior approval is void.
    ///
    /// `goal_override` is the confirm sheet's reviewed/edited statement —
    /// it replaces the default item statement; the mode coda is appended
    /// either way so the sheet's fixed caption stays the single honest
    /// summary of what runs beyond the editable text. `project_root` is
    /// recorded verbatim on the manifest (the handle resolves and
    /// validates it before the store runs; `None` from a direct caller
    /// falls back to fire-time resolution in the scheduler).
    /// `agent_config` is the sheet's reviewed launch pins, recorded on the
    /// manifest verbatim (an all-inherit block normalizes to the legacy
    /// absent shape so the manifest bytes — and digest — stay identical to
    /// a config-less gesture); the launch path normalizes and
    /// backend-gates the values at spawn.
    ///
    /// The caller (`AgendaHandle::apply`) has already enforced the
    /// owner-surface gate — this command embeds an approval.
    #[allow(clippy::too_many_arguments)] // the confirm sheet's reviewed parameters travel together
    fn start_now(
        &mut self,
        id: &str,
        goal_override: Option<&str>,
        project_root: Option<String>,
        interactive: Option<bool>,
        agent_config: Option<Box<crate::event::AgentLaunchConfig>>,
        actor: Option<AgendaActor>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        let item = self.require(id)?;
        if item.status != AgendaStatus::Open {
            return Err(AgendaError::Transition(format!(
                "{id} is not open — reopen it before starting work on it"
            )));
        }
        // Standing manifests (G3-pre): the button beside an APPROVED
        // recurring manifest fires one extra occurrence of the approved
        // digest instead of revising it — start_now's mint+approve would
        // void the very standing approval it decorates. Overrides mean
        // the owner wants DIFFERENT bytes: that is an explicit revision,
        // named here so the sheet's edit path stays honest (`schedule`
        // revises; approval voids as always).
        if let Some(effect) = item
            .effects
            .first()
            .filter(|e| e.manifest.recurrence.is_some() && e.approval.is_some())
        {
            let overridden = goal_override.is_some()
                || agent_config.as_ref().is_some_and(|c| !c.is_empty())
                || interactive.is_some_and(|mode| mode != effect.manifest.interactive);
            if overridden {
                return Err(AgendaError::Transition(format!(
                    "{id} runs a standing approved manifest — Run now fires it exactly \
                     as approved; to change what runs, revise via schedule (which voids \
                     the standing approval for re-review)"
                )));
            }
            let effect_id = effect.effect_id.clone();
            let digest = effect.digest.clone();
            return self.request_occurrence_of(id, &effect_id, &digest, actor, now_ms);
        }
        // Absent defaults to interactive (owner-ratified): the session
        // opens with the item and waits for the owner.
        let interactive = interactive.unwrap_or(true);
        let mut goal = match goal_override.map(str::trim).filter(|goal| !goal.is_empty()) {
            Some(edited) => {
                if edited.len() > MAX_BODY_BYTES {
                    return Err(AgendaError::Invalid(format!(
                        "goal exceeds {MAX_BODY_BYTES} bytes"
                    )));
                }
                edited.to_string()
            }
            None => start_now_goal_statement(item),
        };
        goal.push_str(if interactive {
            START_NOW_INTERACTIVE_CODA
        } else {
            START_NOW_GOAL_RUN_CODA
        });
        // The item body is already capped, but the wrapper text must never
        // push the goal past the manifest bound the scheduled lane
        // enforces at propose time.
        if goal.len() > MAX_BODY_BYTES {
            let mut cut = MAX_BODY_BYTES;
            while !goal.is_char_boundary(cut) {
                cut -= 1;
            }
            goal.truncate(cut);
        }
        let effect_id = item
            .effects
            .first()
            .map(|effect| effect.effect_id.clone())
            .unwrap_or_else(|| {
                format!("ef-{}", &super::reminders::occurrence_id(id, now_ms)[..12])
            });
        let manifest = super::types::SessionManifest {
            goal,
            fire_at_ms: now_ms,
            orchestrate: false,
            interactive,
            project_root,
            agent_config: agent_config.filter(|config| !config.is_empty()),
            recurrence: None,
        };
        let digest = super::types::manifest_digest(id, &effect_id, &manifest);
        self.append_op(
            AgendaOp::ProposeEffect {
                id: id.to_string(),
                effect_id: effect_id.clone(),
                manifest,
            },
            actor.clone(),
            None,
            now_ms,
        )?;
        self.append_op(
            AgendaOp::ApproveEffect {
                id: id.to_string(),
                effect_id,
                digest,
            },
            actor,
            None,
            now_ms,
        )
    }

    /// One extra occurrence of an approved standing manifest (G3-pre):
    /// validate the request against the approved digest and the run
    /// state, then append the attributed `request_occurrence` op with the
    /// instant minted HERE (replay reads it from the line). Shared by the
    /// `request_occurrence` command and start_now's standing routing —
    /// the owner-surface gate ran at the handle either way.
    fn request_occurrence_of(
        &mut self,
        id: &str,
        effect_id: &str,
        digest: &str,
        actor: Option<AgendaActor>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        let item = self.require(id)?;
        let Some(effect) = item.effects.iter().find(|e| e.effect_id == effect_id) else {
            return Err(AgendaError::NotFound(format!(
                "{id} has no scheduled session"
            )));
        };
        let Some(rec) = &effect.manifest.recurrence else {
            return Err(AgendaError::Transition(format!(
                "{id}'s manifest is one-shot — run it again via start (which re-reviews \
                 and re-approves)"
            )));
        };
        let Some(approval) = &effect.approval else {
            return Err(AgendaError::Transition(format!(
                "{id}'s standing manifest is not approved — nothing may fire"
            )));
        };
        if approval.digest != digest || effect.digest != digest {
            return Err(AgendaError::Invalid(format!(
                "digest mismatch: the manifest was revised since it was reviewed \
                 (current digest {})",
                effect.digest
            )));
        }
        if effect.consecutive_failures >= rec.suspend_threshold() {
            return Err(AgendaError::Transition(format!(
                "{id}'s standing session is suspended after {} consecutive failures — \
                 re-approve the manifest to re-arm it, or revoke it",
                effect.consecutive_failures
            )));
        }
        if effect
            .last_run
            .as_ref()
            .is_some_and(|run| run.state == "started")
        {
            return Err(AgendaError::Transition(format!(
                "a run of {id}'s manifest is in flight — one occurrence at a time"
            )));
        }
        // At most one pending request: pending = no occurrence write-back
        // has landed since the newest request (both facts fold from the
        // log, so the judgment is replay-pure).
        if let Some(newest) = effect.requested.last() {
            let settled = effect
                .last_run
                .as_ref()
                .is_some_and(|run| run.at_ms >= newest.at_ms);
            if !settled {
                return Err(AgendaError::Transition(format!(
                    "{id} already has a requested run pending — it fires on the next \
                     scheduler pass"
                )));
            }
        }
        self.append_op(
            AgendaOp::RequestOccurrence {
                id: id.to_string(),
                effect_id: effect_id.to_string(),
                digest: digest.to_string(),
                at_ms: now_ms,
            },
            actor,
            None,
            now_ms,
        )
    }

    /// Would placing `id` under `parent_id` keep the tree lawful? Checks
    /// self-placement, the ancestry cycle (walking the parent's live
    /// ancestor chain — bounded, so a foreign-log cycle terminates as a
    /// depth rejection), the depth cap counting the moving subtree's own
    /// height, and the parent's live-children rail. Pure read over the
    /// current fold; every rejection is named.
    fn validate_placement(&self, id: &str, parent_id: &str) -> Result<(), AgendaError> {
        if parent_id == id {
            return Err(AgendaError::Invalid(
                "an item cannot be placed under itself".into(),
            ));
        }
        // Ancestor walk from the proposed parent: finding `id` means the
        // placement would close a cycle; running past the bound means the
        // tree would exceed the depth rail either way.
        let mut ancestors = 1usize; // the parent itself
        let mut cursor = parent_id;
        while let Some(placement) = self.items.get(cursor).and_then(|p| p.part_of.as_ref()) {
            if placement.parent_id == id {
                return Err(AgendaError::Invalid(format!(
                    "placement cycle via {cursor} — {id} is already an ancestor of {parent_id}"
                )));
            }
            ancestors += 1;
            if ancestors > MAX_PART_OF_DEPTH {
                return Err(AgendaError::Invalid(format!(
                    "placement exceeds the depth rail ({MAX_PART_OF_DEPTH})"
                )));
            }
            cursor = &placement.parent_id;
        }
        // Depth after placement = the parent's chain + this item + its own
        // subtree height (children move with their parent).
        if ancestors + 1 + self.subtree_height(id) > MAX_PART_OF_DEPTH {
            return Err(AgendaError::Invalid(format!(
                "placement exceeds the depth rail ({MAX_PART_OF_DEPTH})"
            )));
        }
        let children = self
            .items
            .values()
            .filter(|item| {
                item.part_of
                    .as_ref()
                    .is_some_and(|p| p.parent_id == parent_id)
            })
            .count();
        if children >= MAX_CHILDREN_PER_HUB {
            return Err(AgendaError::Invalid(format!(
                "{parent_id} already has {MAX_CHILDREN_PER_HUB} children — a hub \
                 this size is a filing pathology, not an agenda"
            )));
        }
        Ok(())
    }

    /// Height of `id`'s placed subtree (0 = no children), bounded by the
    /// item count. Intake-only arithmetic over the fold.
    fn subtree_height(&self, id: &str) -> usize {
        let mut frontier: Vec<&str> = vec![id];
        let mut height = 0usize;
        while height <= MAX_PART_OF_DEPTH {
            let next: Vec<&str> = self
                .items
                .values()
                .filter(|item| {
                    item.part_of
                        .as_ref()
                        .is_some_and(|p| frontier.iter().any(|f| *f == p.parent_id))
                })
                .map(|item| item.id.as_str())
                .collect();
            if next.is_empty() {
                return height;
            }
            height += 1;
            frontier = next;
        }
        height
    }

    /// The re-parent gesture (G2): validate the new placement first, then
    /// append `remove_part_of` (when placed) + `add_part_of` under the
    /// caller's lock. Op vocabulary unchanged — the log carries the two
    /// primitive lines.
    fn apply_place(
        &mut self,
        id: &str,
        under: &str,
        actor: Option<AgendaActor>,
        source: Option<String>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        let under = under.trim().to_string();
        self.require(id)?;
        self.require(&under)?;
        let current = self
            .require(id)?
            .part_of
            .as_ref()
            .map(|p| p.parent_id.clone());
        if current.as_deref() == Some(under.as_str()) {
            return Err(AgendaError::Transition(format!(
                "{id} is already placed under {under}"
            )));
        }
        self.validate_placement(id, &under)?;
        if let Some(current) = current {
            self.append_op(
                AgendaOp::RemovePartOf {
                    id: id.to_string(),
                    parent_id: current,
                },
                actor.clone(),
                source.clone(),
                now_ms,
            )?;
        }
        self.append_op(
            AgendaOp::AddPartOf {
                id: id.to_string(),
                parent_id: under,
            },
            actor,
            source,
            now_ms,
        )
    }

    /// Park one item, optionally with attached refs (G1's parking-gesture
    /// sugar). Every ref spec is validated — file digests included —
    /// BEFORE anything is appended, so a refused ref refuses the whole
    /// park and strands nothing. The `add` op and one `add_ref` per spec
    /// then append under the caller's lock with identical attribution.
    #[allow(clippy::too_many_arguments)] // the park's reviewed fields travel together
    fn apply_add(
        &mut self,
        kind: super::types::AgendaKind,
        title: String,
        body: String,
        tags: Vec<String>,
        due_ms: Option<u64>,
        refs: Vec<super::types::AgendaRefSpec>,
        actor: Option<AgendaActor>,
        source: Option<String>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        let title = validate_title(&title)?;
        let body = validate_body(body)?;
        let tags = validate_tags(tags)?;
        if refs.len() > MAX_REFS_PER_ITEM {
            return Err(AgendaError::Invalid(format!(
                "more than {MAX_REFS_PER_ITEM} refs"
            )));
        }
        let mut validated: Vec<ValidatedRef> = Vec::with_capacity(refs.len());
        for spec in refs {
            let vref = validate_ref(spec.ref_type, &spec.locator, spec.must_read, spec.label)?;
            if validated
                .iter()
                .any(|v| v.ref_type == vref.ref_type && v.locator == vref.locator)
            {
                return Err(AgendaError::Invalid(format!(
                    "duplicate {} ref {:?} in one park",
                    vref.ref_type.as_str(),
                    vref.locator
                )));
            }
            validated.push(vref);
        }
        let id = self.mint_id()?;
        let mut item = self.append_op(
            AgendaOp::Add {
                id: id.clone(),
                kind,
                title,
                body,
                tags,
                due_ms,
                ask: None,
            },
            actor.clone(),
            source.clone(),
            now_ms,
        )?;
        for vref in validated {
            item = self.append_op(
                AgendaOp::AddRef {
                    id: id.clone(),
                    ref_type: vref.ref_type,
                    locator: vref.locator,
                    digest: vref.digest,
                    must_read: vref.must_read,
                    label: vref.label,
                },
                actor.clone(),
                source.clone(),
                now_ms,
            )?;
        }
        Ok(item)
    }

    /// Park one validated rich ask: build the questions through the same
    /// validator the blocking `ask_user` uses, mint the item id, commit
    /// preview blobs into the agenda blob store (rolling back every blob
    /// of this park on any failure — mirrors `ask_user_inner`'s
    /// cross-question rollback), mint the rail `ask_id`, and append the
    /// `add`.
    fn apply_ask(
        &mut self,
        questions: Vec<crate::mcp::AskUserQuestionParams>,
        actor: Option<AgendaActor>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        if questions.is_empty() {
            return Err(AgendaError::Invalid(
                "ask requires at least one question".into(),
            ));
        }
        // The exact ask_user validation (counts, pick bounds, preview
        // decode, the shared ≤8 MB preview budget) — derive, don't mirror.
        let params = crate::mcp::AskUserParams {
            question: String::new(),
            header: None,
            options: Vec::new(),
            previews: Vec::new(),
            multi_select: None,
            pick_min: None,
            pick_max: None,
            free_text: None,
            questions,
            wait_seconds: None,
            park: false,
            session_id: None,
        };
        let (built, _wait) =
            crate::mcp::build_ask_user_questions(&params).map_err(AgendaError::Invalid)?;

        let item_id = self.mint_id()?;
        // Title = the first question, clipped to the title cap (questions
        // may legally exceed it; the full text lives on the ask payload).
        let title: String = built
            .first()
            .map(|(q, _)| q.question.chars().take(MAX_TITLE_CHARS).collect())
            .unwrap_or_default();
        let title = validate_title(&title)?;

        // Commit preview blobs; on any failure delete everything this park
        // committed so a refused ask strands nothing.
        let mut questions: Vec<crate::types::UserQuestion> = Vec::with_capacity(built.len());
        let mut commit_error: Option<String> = None;
        'questions: for (mut question, previews) in built {
            let mut committed: Vec<crate::types::QuestionPreview> = Vec::new();
            for preview in previews {
                let source = match preview.source {
                    crate::mcp::DecodedPreviewSource::Text(content) => {
                        Ok(crate::types::QuestionPreviewSource::Text { content })
                    }
                    crate::mcp::DecodedPreviewSource::Html(html) => super::blobs::commit_blob(
                        &self.dir,
                        &item_id,
                        &preview.label,
                        "html",
                        "text/html",
                        html.as_bytes(),
                    )
                    .map(|descriptor| crate::types::QuestionPreviewSource::Html {
                        url: super::blobs::agenda_blob_raw_url(&item_id, &descriptor.id),
                        upload_id: descriptor.id,
                    }),
                    crate::mcp::DecodedPreviewSource::Image { mime, bytes } => {
                        super::blobs::commit_blob(
                            &self.dir,
                            &item_id,
                            &preview.label,
                            crate::mcp::note_image_extension(mime),
                            mime,
                            &bytes,
                        )
                        .map(|descriptor| {
                            crate::types::QuestionPreviewSource::Image {
                                url: super::blobs::agenda_blob_raw_url(&item_id, &descriptor.id),
                                upload_id: descriptor.id,
                                mime: mime.to_string(),
                            }
                        })
                    }
                };
                match source {
                    Ok(source) => committed.push(crate::types::QuestionPreview {
                        label: preview.label,
                        source,
                    }),
                    Err(message) => {
                        commit_error = Some(message);
                        break 'questions;
                    }
                }
            }
            question.previews = committed;
            questions.push(question);
        }
        if let Some(message) = commit_error {
            if let Err(err) = super::blobs::delete_item_blobs(&self.dir, &item_id) {
                eprintln!("[agenda] rollback of {item_id} blobs: {err}");
            }
            return Err(AgendaError::Invalid(message));
        }

        let ask_id = crate::event::next_approval_id();
        let op = AgendaOp::Add {
            id: item_id.clone(),
            kind: super::types::AgendaKind::Question,
            title,
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            ask: Some(super::types::AgendaAsk { ask_id, questions }),
        };
        match self.append_op(op, actor, None, now_ms) {
            Ok(item) => Ok(item),
            Err(err) => {
                // The append never made it to disk: the blobs are orphans.
                if let Err(cleanup) = super::blobs::delete_item_blobs(&self.dir, &item_id) {
                    eprintln!("[agenda] rollback of {item_id} blobs: {cleanup}");
                }
                Err(err)
            }
        }
    }

    /// Daemon-internal dismissal of an open rich question (rail
    /// skip/deny/approve — the resolver's lane; no command twin, mirroring
    /// `record_occurrence`). Records the marker; the item stays OPEN.
    pub(crate) fn dismiss_question(
        &mut self,
        item_id: &str,
        action: &str,
        actor: Option<AgendaActor>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        self.refresh_if_stale()?;
        let item = self.require(item_id)?;
        if item.kind != super::types::AgendaKind::Question {
            return Err(AgendaError::Invalid(format!("{item_id} is not a question")));
        }
        if item.status != AgendaStatus::Open {
            return Err(AgendaError::Transition(format!("{item_id} is not open")));
        }
        let op = AgendaOp::Dismiss {
            id: item_id.to_string(),
            action: action.to_string(),
        };
        self.append_op(op, actor, None, now_ms)
    }

    /// The item with `item_id`, whatever its status (freshened first).
    pub(crate) fn item(&mut self, item_id: &str) -> Option<AgendaItem> {
        if let Err(err) = self.refresh_if_stale() {
            eprintln!("[agenda] refresh before item lookup failed: {err}");
        }
        self.items.get(item_id).cloned()
    }

    /// The item currently holding `ask_id` as an OPEN rich ask, if any.
    pub(crate) fn open_ask(&mut self, ask_id: u64) -> Option<AgendaItem> {
        if let Err(err) = self.refresh_if_stale() {
            eprintln!("[agenda] refresh before ask lookup failed: {err}");
        }
        self.items
            .values()
            .find(|item| {
                item.status == AgendaStatus::Open
                    && item.ask.as_ref().is_some_and(|ask| ask.ask_id == ask_id)
            })
            .cloned()
    }

    fn append_op(
        &mut self,
        op: AgendaOp,
        actor: Option<AgendaActor>,
        source: Option<String>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        let item_id = op.item_id().to_string();
        let record = AgendaOpRecord {
            v: AGENDA_LOG_VERSION,
            at_ms: now_ms,
            actor,
            source,
            op,
        };
        let mut line = serde_json::to_string(&record)
            .map_err(|err| AgendaError::Invalid(format!("encoding op: {err}")))?;
        line.push('\n');
        // One write_all per record: a crash tears at most the final line,
        // which `open` terminates and skips. Durability is append + flush
        // by ratified scope (no fsync in v1 — the delivery-critical
        // occurrence journal adds it where it matters).
        self.log.write_all(line.as_bytes())?;
        self.log.flush()?;
        self.folded_len += line.len() as u64;
        if let Some(reason) = apply_op(&mut self.items, &record) {
            // Unreachable by construction: the op was validated against
            // the exact state the fold sees.
            eprintln!("[agenda] fold rejected a validated op: {reason}");
        }
        self.ops += 1;
        self.sync_ask_state();
        self.items
            .get(&item_id)
            .cloned()
            .ok_or_else(|| AgendaError::Invalid("internal: item missing after fold".into()))
    }

    fn command_to_op(&mut self, cmd: AgendaCommand, now_ms: u64) -> Result<AgendaOp, AgendaError> {
        // `source` is detached (and validated) in `apply_command` before the
        // translation — the remaining fields are the op's.
        match cmd {
            // Handled by `apply_command`'s dedicated arms (add: park +
            // trailing ref ops; ask: blob commits + rollback; start_now: an
            // atomic two-op append); reaching here is a daemon bug, not a
            // caller error.
            AgendaCommand::Add { .. } => Err(AgendaError::Invalid(
                "internal: add must route through apply_command".into(),
            )),
            AgendaCommand::Ask { .. } => Err(AgendaError::Invalid(
                "internal: ask must route through apply_command".into(),
            )),
            AgendaCommand::StartNow { .. } => Err(AgendaError::Invalid(
                "internal: start_now must route through apply_command".into(),
            )),
            AgendaCommand::Patch {
                id,
                patch,
                source: _,
            } => {
                self.require(&id)?;
                if patch.is_empty() {
                    return Err(AgendaError::Invalid("patch changes nothing".into()));
                }
                let AgendaPatch {
                    title,
                    body,
                    tags,
                    due_ms,
                } = patch;
                let patch = AgendaPatch {
                    title: title.as_deref().map(validate_title).transpose()?,
                    body: body.map(validate_body).transpose()?,
                    tags: tags.map(validate_tags).transpose()?,
                    due_ms,
                };
                Ok(AgendaOp::Patch { id, patch })
            }
            AgendaCommand::Complete { id, source: _ } => match self.require(&id)?.status {
                AgendaStatus::Open => Ok(AgendaOp::Complete { id }),
                AgendaStatus::Done => Err(AgendaError::Transition(format!("{id} is already done"))),
                AgendaStatus::Retired => Err(AgendaError::Transition(format!(
                    "{id} is retired — reopen it first"
                ))),
            },
            AgendaCommand::Reopen { id, source: _ } => match self.require(&id)?.status {
                AgendaStatus::Done | AgendaStatus::Retired => Ok(AgendaOp::Reopen { id }),
                AgendaStatus::Open => Err(AgendaError::Transition(format!("{id} is already open"))),
            },
            AgendaCommand::Retire { id, source: _ } => match self.require(&id)?.status {
                AgendaStatus::Retired => {
                    Err(AgendaError::Transition(format!("{id} is already retired")))
                }
                AgendaStatus::Open | AgendaStatus::Done => Ok(AgendaOp::Retire { id }),
            },
            AgendaCommand::Answer {
                id,
                text,
                structured,
                source: _,
            } => {
                let item = self.require(&id)?;
                if item.kind != super::types::AgendaKind::Question {
                    return Err(AgendaError::Invalid(format!(
                        "{id} is not a question — only questions accept answers"
                    )));
                }
                match item.status {
                    AgendaStatus::Open => Ok(AgendaOp::Answer {
                        id,
                        text: validate_answer(&text)?,
                        structured: structured.filter(|s| !s.is_empty()),
                    }),
                    AgendaStatus::Done => Err(AgendaError::Transition(format!(
                        "{id} is already answered — reopen it to re-ask"
                    ))),
                    AgendaStatus::Retired => Err(AgendaError::Transition(format!(
                        "{id} is retired — reopen it first"
                    ))),
                }
            }
            AgendaCommand::ProposeEffect {
                id,
                goal,
                fire_at_ms,
                orchestrate,
                recurrence,
                source: _,
            } => {
                let item = self.require(&id)?;
                if item.status != AgendaStatus::Open {
                    return Err(AgendaError::Transition(format!(
                        "{id} is not open — reopen it before scheduling work on it"
                    )));
                }
                if fire_at_ms == 0 {
                    return Err(AgendaError::Invalid("fire_at_ms must be set".into()));
                }
                if let Some(rec) = &recurrence {
                    if rec.every_ms < super::types::RECURRENCE_MIN_EVERY_MS {
                        return Err(AgendaError::Invalid(format!(
                            "recurrence cadence floors at {} minutes",
                            super::types::RECURRENCE_MIN_EVERY_MS / 60_000
                        )));
                    }
                    if rec.until_ms.is_some_and(|until| until <= fire_at_ms) {
                        return Err(AgendaError::Invalid(
                            "recurrence until_ms must be after fire_at_ms".into(),
                        ));
                    }
                    if rec.max_occurrences.is_some_and(|max| max == 0) {
                        return Err(AgendaError::Invalid(
                            "recurrence max_occurrences must be at least 1".into(),
                        ));
                    }
                    if rec.suspend_after_failures.is_some_and(|n| n == 0) {
                        return Err(AgendaError::Invalid(
                            "recurrence suspend_after_failures must be at least 1".into(),
                        ));
                    }
                }
                let goal = {
                    let goal = goal.trim();
                    if goal.is_empty() {
                        return Err(AgendaError::Invalid("goal must not be empty".into()));
                    }
                    if goal.len() > MAX_BODY_BYTES {
                        return Err(AgendaError::Invalid(format!(
                            "goal exceeds {MAX_BODY_BYTES} bytes"
                        )));
                    }
                    goal.to_string()
                };
                // v1: one session effect per item — a re-propose revises it
                // (stable effect_id lineage, fresh digest, approval void).
                let effect_id = item
                    .effects
                    .first()
                    .map(|effect| effect.effect_id.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "ef-{}",
                            &super::reminders::occurrence_id(&id, fire_at_ms)[..12]
                        )
                    });
                Ok(AgendaOp::ProposeEffect {
                    id,
                    effect_id,
                    manifest: super::types::SessionManifest {
                        goal,
                        fire_at_ms,
                        orchestrate,
                        // Proposals keep the legacy autonomous shape; the
                        // project resolves at fire time (provenance →
                        // daemon default → named refusal), and the launch
                        // config inherits the daemon defaults.
                        interactive: false,
                        project_root: None,
                        agent_config: None,
                        recurrence,
                    },
                })
            }
            AgendaCommand::RequestOccurrence { id } => {
                // Validation and the op append live in the dedicated arm
                // (`request_occurrence_of`) so start_now's standing route
                // shares it; reaching here is a daemon bug.
                Err(AgendaError::Invalid(format!(
                    "internal: request_occurrence for {id} must route through apply_command"
                )))
            }
            AgendaCommand::Annotate {
                id,
                text,
                source: _,
            } => {
                let item = self.require(&id)?;
                let text = text.trim();
                if text.is_empty() {
                    return Err(AgendaError::Invalid("annotation must not be empty".into()));
                }
                if text.len() > MAX_BODY_BYTES {
                    return Err(AgendaError::Invalid(format!(
                        "annotation exceeds {MAX_BODY_BYTES} bytes"
                    )));
                }
                // The steward-ruled pathology rail — not a budget (weekly
                // housekeeping ≈ 52/item/year).
                if item.annotations.len() >= MAX_ANNOTATIONS_PER_ITEM {
                    return Err(AgendaError::Invalid(format!(
                        "item has {MAX_ANNOTATIONS_PER_ITEM} annotations — retire it or start \
                         a successor item"
                    )));
                }
                Ok(AgendaOp::Annotate {
                    id,
                    text: text.to_string(),
                })
            }
            AgendaCommand::SetBlocker {
                id,
                criterion,
                source: _,
            } => {
                let item = self.require(&id)?;
                if item.status != AgendaStatus::Open {
                    return Err(AgendaError::Transition(format!(
                        "{id} is not open — blockers describe open work"
                    )));
                }
                let criterion = criterion.trim();
                if criterion.is_empty() {
                    return Err(AgendaError::Invalid("criterion must not be empty".into()));
                }
                if criterion.chars().count() > MAX_CRITERION_CHARS {
                    return Err(AgendaError::Invalid(format!(
                        "criterion exceeds {MAX_CRITERION_CHARS} characters"
                    )));
                }
                let uncleared = item.blockers.iter().filter(|b| b.cleared.is_none());
                if uncleared.clone().any(|b| b.criterion == criterion) {
                    return Err(AgendaError::Transition(format!(
                        "{id} is already blocked on exactly this criterion"
                    )));
                }
                if uncleared.count() >= MAX_UNCLEARED_BLOCKERS_PER_ITEM {
                    return Err(AgendaError::Invalid(format!(
                        "more than {MAX_UNCLEARED_BLOCKERS_PER_ITEM} uncleared blockers"
                    )));
                }
                // Intake-minted, recorded in the op — replay never mints
                // (§7 purity). The at_ms preimage makes same-criterion
                // re-blocks (after a clear) distinct; a same-millisecond
                // duplicate is refused above as an identical criterion.
                let blocker_id = mint_blocker_id(&id, criterion, now_ms);
                if item.blockers.iter().any(|b| b.blocker_id == blocker_id) {
                    return Err(AgendaError::Invalid(
                        "blocker id collision — retry the command".into(),
                    ));
                }
                Ok(AgendaOp::SetBlocker {
                    id,
                    blocker_id,
                    criterion: criterion.to_string(),
                })
            }
            AgendaCommand::ClearBlocker {
                id,
                blocker_id,
                source: _,
            } => {
                let item = self.require(&id)?;
                // Any item status: clearing history on a done item is a
                // legitimate bookkeeping act.
                let needle = blocker_id.trim();
                let mut matches = item
                    .blockers
                    .iter()
                    .filter(|b| b.cleared.is_none() && b.blocker_id.starts_with(needle));
                let Some(blocker) = matches.next() else {
                    return Err(AgendaError::NotFound(format!(
                        "no uncleared blocker matching {needle:?} on {id}"
                    )));
                };
                if matches.next().is_some() {
                    return Err(AgendaError::Invalid(format!(
                        "blocker prefix {needle:?} is ambiguous on {id}"
                    )));
                }
                Ok(AgendaOp::ClearBlocker {
                    id: id.clone(),
                    blocker_id: blocker.blocker_id.clone(),
                })
            }
            AgendaCommand::AddReliesOn {
                id,
                target_id,
                source: _,
            } => {
                let item = self.require(&id)?;
                if item.status != AgendaStatus::Open {
                    return Err(AgendaError::Transition(format!(
                        "{id} is not open — dependencies describe open work"
                    )));
                }
                let target_id = target_id.trim().to_string();
                if target_id == id {
                    return Err(AgendaError::Invalid("an item cannot rely on itself".into()));
                }
                // The target must exist NOW (intake strictness); the fold
                // stays tolerant of dangling edges in foreign logs.
                self.require(&target_id)?;
                if item.relies_on.iter().any(|e| e.target_id == target_id) {
                    return Err(AgendaError::Transition(format!(
                        "{id} already relies on {target_id}"
                    )));
                }
                if item.relies_on.len() >= MAX_RELIES_ON_PER_ITEM {
                    return Err(AgendaError::Invalid(format!(
                        "more than {MAX_RELIES_ON_PER_ITEM} dependencies"
                    )));
                }
                Ok(AgendaOp::AddReliesOn { id, target_id })
            }
            AgendaCommand::RemoveReliesOn {
                id,
                target_id,
                source: _,
            } => {
                let item = self.require(&id)?;
                let target_id = target_id.trim().to_string();
                if !item.relies_on.iter().any(|e| e.target_id == target_id) {
                    return Err(AgendaError::NotFound(format!(
                        "{id} has no live dependency on {target_id}"
                    )));
                }
                Ok(AgendaOp::RemoveReliesOn { id, target_id })
            }
            // Placement/adjacency intake (G2): any item status — organizing
            // done/retired history is bookkeeping (the clear_blocker
            // precedent, ruled call 4). `Place` never reaches here (its
            // apply_command arm owns the remove+add pair).
            AgendaCommand::Place { .. } => Err(AgendaError::Invalid(
                "internal: place must route through apply_command".into(),
            )),
            AgendaCommand::AddPartOf {
                id,
                parent_id,
                source: _,
            } => {
                let parent_id = parent_id.trim().to_string();
                let item = self.require(&id)?;
                if let Some(placement) = &item.part_of {
                    return Err(AgendaError::Transition(format!(
                        "{id} is already placed under {} — re-parent with place, or \
                         remove that placement first",
                        placement.parent_id
                    )));
                }
                self.require(&parent_id)?;
                self.validate_placement(&id, &parent_id)?;
                Ok(AgendaOp::AddPartOf { id, parent_id })
            }
            AgendaCommand::RemovePartOf {
                id,
                parent_id,
                source: _,
            } => {
                let parent_id = parent_id.trim().to_string();
                let item = self.require(&id)?;
                match &item.part_of {
                    Some(placement) if placement.parent_id == parent_id => {
                        Ok(AgendaOp::RemovePartOf { id, parent_id })
                    }
                    Some(placement) => Err(AgendaError::NotFound(format!(
                        "{id} is placed under {}, not {parent_id}",
                        placement.parent_id
                    ))),
                    None => Err(AgendaError::NotFound(format!("{id} is not placed"))),
                }
            }
            AgendaCommand::AddRelatesTo {
                id,
                target_id,
                source: _,
            } => {
                let target_id = target_id.trim().to_string();
                let item = self.require(&id)?;
                if target_id == id {
                    return Err(AgendaError::Invalid(
                        "an item cannot relate to itself".into(),
                    ));
                }
                let target = self.require(&target_id)?;
                // Undirected dedup at intake: either stored direction is
                // the same adjacency.
                if item.relates_to.iter().any(|e| e.target_id == target_id)
                    || target.relates_to.iter().any(|e| e.target_id == id)
                {
                    return Err(AgendaError::Transition(format!(
                        "{id} and {target_id} are already related"
                    )));
                }
                if item.relates_to.len() >= MAX_RELATES_TO_PER_ITEM {
                    return Err(AgendaError::Invalid(format!(
                        "more than {MAX_RELATES_TO_PER_ITEM} relations"
                    )));
                }
                Ok(AgendaOp::AddRelatesTo { id, target_id })
            }
            AgendaCommand::RemoveRelatesTo {
                id,
                target_id,
                source: _,
            } => {
                let target_id = target_id.trim().to_string();
                let item = self.require(&id)?;
                // Resolve which side stores the edge — callers name the
                // pair in either order; the op names the storing item.
                if item.relates_to.iter().any(|e| e.target_id == target_id) {
                    Ok(AgendaOp::RemoveRelatesTo { id, target_id })
                } else if self
                    .require(&target_id)?
                    .relates_to
                    .iter()
                    .any(|e| e.target_id == id)
                {
                    Ok(AgendaOp::RemoveRelatesTo {
                        id: target_id,
                        target_id: id,
                    })
                } else {
                    Err(AgendaError::NotFound(format!(
                        "{id} and {target_id} are not related"
                    )))
                }
            }
            AgendaCommand::AddRef {
                id,
                ref_type,
                locator,
                must_read,
                label,
                source: _,
            } => {
                // Any item status: attaching context to done/retired
                // history is legitimate bookkeeping (ruled, G1 call 4).
                let item = self.require(&id)?;
                let vref = validate_ref(ref_type, &locator, must_read, label)?;
                if item
                    .refs
                    .iter()
                    .any(|r| r.ref_type == vref.ref_type && r.locator == vref.locator)
                {
                    return Err(AgendaError::Transition(format!(
                        "{id} already carries this {} ref",
                        vref.ref_type.as_str()
                    )));
                }
                if item.refs.len() >= MAX_REFS_PER_ITEM {
                    return Err(AgendaError::Invalid(format!(
                        "more than {MAX_REFS_PER_ITEM} refs"
                    )));
                }
                Ok(AgendaOp::AddRef {
                    id,
                    ref_type: vref.ref_type,
                    locator: vref.locator,
                    digest: vref.digest,
                    must_read: vref.must_read,
                    label: vref.label,
                })
            }
            AgendaCommand::RemoveRef {
                id,
                ref_type,
                locator,
                source: _,
            } => {
                let item = self.require(&id)?;
                let locator = locator.trim().to_string();
                if !item
                    .refs
                    .iter()
                    .any(|r| r.ref_type == ref_type && r.locator == locator)
                {
                    return Err(AgendaError::NotFound(format!(
                        "no live {} ref matching {locator:?} on {id}",
                        ref_type.as_str()
                    )));
                }
                Ok(AgendaOp::RemoveRef {
                    id,
                    ref_type,
                    locator,
                })
            }
            AgendaCommand::ApproveEffect { id, digest } => {
                let item = self.require(&id)?;
                if item.status != AgendaStatus::Open {
                    return Err(AgendaError::Transition(format!("{id} is not open")));
                }
                let Some(effect) = item.effects.first() else {
                    return Err(AgendaError::NotFound(format!(
                        "{id} has no proposed scheduled session"
                    )));
                };
                // Plain double-approve stays refused; the ONE exception is
                // the suspended standing effect (G3-pre), where re-approving
                // the unchanged digest is the ratified one-click re-arm
                // (the approve op resets the failure streak in the fold).
                if effect.approval.is_some() && !effect.suspended() {
                    return Err(AgendaError::Transition(format!(
                        "{id}'s scheduled session is already approved — revoke first to re-review"
                    )));
                }
                // The rider's binding rule: approval names exact bytes. A
                // stale digest means the manifest changed since review.
                if effect.digest != digest.trim() {
                    return Err(AgendaError::Invalid(format!(
                        "digest mismatch: the manifest was revised since it was reviewed \
                         (current digest {})",
                        effect.digest
                    )));
                }
                Ok(AgendaOp::ApproveEffect {
                    id,
                    effect_id: effect.effect_id.clone(),
                    digest: effect.digest.clone(),
                })
            }
            AgendaCommand::RevokeEffect { id } => {
                let item = self.require(&id)?;
                let Some(effect) = item.effects.first() else {
                    return Err(AgendaError::NotFound(format!(
                        "{id} has no scheduled session"
                    )));
                };
                if effect.approval.is_none() {
                    return Err(AgendaError::Transition(format!(
                        "{id}'s scheduled session is not approved"
                    )));
                }
                Ok(AgendaOp::RevokeEffect {
                    id,
                    effect_id: effect.effect_id.clone(),
                })
            }
        }
    }

    /// Daemon-internal occurrence write-back (scheduler only — no command
    /// twin exists, so no external surface reaches this). Appends the op
    /// with no actor and folds it.
    pub(crate) fn record_occurrence(
        &mut self,
        write: OccurrenceWriteBack<'_>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        self.refresh_if_stale()?;
        self.require(write.item_id)?;
        let op = AgendaOp::RecordOccurrence {
            id: write.item_id.to_string(),
            effect_id: write.effect_id.to_string(),
            occurrence_id: write.occurrence_id.to_string(),
            state: write.state.to_string(),
            session_id: write.session_id,
            note: write.note.map(|n| n.chars().take(500).collect()),
        };
        self.append_op(op, None, None, now_ms)
    }

    /// Daemon-internal ask-delivery write-back (the session supervisor's
    /// delivery arm only — no command twin, mirroring `record_occurrence`,
    /// so no external surface can forge delivery facts). Requires a
    /// current answer to annotate; the fold keeps the boolean on
    /// `answer.delivered` and the log keeps the receiving session as
    /// history.
    pub(crate) fn record_ask_delivery(
        &mut self,
        item_id: &str,
        delivered: bool,
        session_id: Option<String>,
        now_ms: u64,
    ) -> Result<AgendaItem, AgendaError> {
        self.refresh_if_stale()?;
        if self.require(item_id)?.answer.is_none() {
            return Err(AgendaError::Invalid(format!(
                "{item_id} has no current answer to mark"
            )));
        }
        let op = AgendaOp::RecordAskDelivery {
            id: item_id.to_string(),
            delivered,
            session_id,
        };
        self.append_op(op, None, None, now_ms)
    }

    fn require(&self, id: &str) -> Result<&AgendaItem, AgendaError> {
        self.items
            .get(id)
            .ok_or_else(|| AgendaError::NotFound(id.to_string()))
    }

    /// Mint the next item id: a fresh ULID, floored against the largest id
    /// ever seen so mint order equals id order even when the clock has not
    /// advanced past the previous mint (same-millisecond restarts, refolds
    /// of another instance's appends).
    fn mint_id(&mut self) -> Result<String, AgendaError> {
        let candidate = ulid::Ulid::new();
        let minted = match self.last_id {
            Some(prev) if candidate <= prev => prev
                .increment()
                .ok_or_else(|| AgendaError::Invalid("id space exhausted; retry".into()))?,
            _ => candidate,
        };
        self.last_id = Some(minted);
        Ok(minted.to_string())
    }

    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<&AgendaItem> {
        self.items.get(id)
    }

    /// All items, oldest first (ULID order). Retired items included —
    /// frontends filter; history stays reachable.
    pub(crate) fn snapshot(&self) -> Vec<AgendaItem> {
        self.items.values().cloned().collect()
    }

    pub(crate) fn counts(&self) -> AgendaCounts {
        counts(&self.items)
    }

    /// One page of the raw op log (read-only; `GET /api/agenda/ops`).
    ///
    /// `since` is a 0-based line cursor into `agenda.jsonl`. The log is
    /// append-only — the daemon never truncates or rewrites it — so a
    /// line index is a stable sequence number: line N today is line N
    /// forever, and a cursor survives restarts and refolds. (External
    /// tampering that shrinks the file surfaces as `log_len` dropping
    /// below the cursor.)
    ///
    /// The fold's forward-compatibility rule extends to reads: a line
    /// whose op vocabulary this build does not fold (`op.type` outside
    /// [`KNOWN_OPS`], a newer line version) is still served VERBATIM,
    /// marked `known:false` — an older server never HIDES history it
    /// cannot parse, just as an older binary never destroys history it
    /// cannot read. A line that is not JSON at all (crash-torn tail,
    /// hand edit) is served as
    /// `{"seq":N,"known":false,"unparseable":true,"raw":"<line>"}`.
    ///
    /// `item` filters to lines whose `op.id` equals it; lines carrying
    /// no `op.id` (unparseable lines, foreign envelopes) are excluded
    /// under the filter — they reference no item. `limit` is clamped to
    /// [1, [`AGENDA_OPS_MAX_LIMIT`]]. Whitespace-only lines keep their
    /// seq slot but are never served (fold parity: they are padding,
    /// not history).
    ///
    /// Torn reads: the caller ([`super::AgendaHandle`]) holds the same
    /// store lock every append completes under (`write_all` + flush
    /// before release), so an in-process line can never be observed
    /// half-written; cross-process appends are whole-line `O_APPEND`
    /// writes — exactly the exposure [`Self::refresh_if_stale`]'s own
    /// read path already carries.
    pub(crate) fn read_ops(
        &mut self,
        since: u64,
        item: Option<&str>,
        limit: usize,
    ) -> std::io::Result<AgendaOpsPage> {
        // Converge with disk first (terminates a foreign torn tail and
        // refreshes the fold), like every other read through the handle.
        self.refresh_if_stale()?;
        let limit = limit.clamp(1, AGENDA_OPS_MAX_LIMIT);
        let bytes = match std::fs::read(&self.log_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err),
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut ops: Vec<serde_json::Value> = Vec::new();
        let mut log_len = 0u64;
        // The first seq the scan did not consume; log_len unless the
        // page filled mid-log.
        let mut next_since: Option<u64> = None;
        for (index, raw_line) in text.lines().enumerate() {
            let seq = index as u64;
            log_len = seq + 1;
            if next_since.is_some() || seq < since {
                continue;
            }
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            let entry = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => {
                    if let Some(want) = item {
                        let referenced = value
                            .get("op")
                            .and_then(|op| op.get("id"))
                            .and_then(serde_json::Value::as_str);
                        if referenced != Some(want) {
                            continue;
                        }
                    }
                    let known = value
                        .get("op")
                        .and_then(|op| op.get("type"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|tag| KNOWN_OPS.contains(&tag));
                    serde_json::json!({ "seq": seq, "known": known, "op": value })
                }
                Err(_) => {
                    if item.is_some() {
                        continue; // no op.id — excluded under the filter
                    }
                    serde_json::json!({
                        "seq": seq,
                        "known": false,
                        "unparseable": true,
                        "raw": line,
                    })
                }
            };
            ops.push(entry);
            if ops.len() >= limit {
                next_since = Some(seq + 1);
            }
        }
        Ok(AgendaOpsPage {
            ops,
            next_since: next_since.unwrap_or(log_len),
            log_len,
            filtered: item.is_some(),
        })
    }

    #[cfg(test)]
    pub(crate) fn ops(&self) -> u64 {
        self.ops
    }

    pub(crate) fn skipped_lines(&self) -> u64 {
        self.skipped_lines
    }

    #[cfg(test)]
    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    #[cfg(test)]
    pub(crate) fn force_last_id_for_tests(&mut self, id: &str) {
        self.last_id = ulid::Ulid::from_string(id).ok();
    }
}

/// The largest item id in the fold, as the mint floor. BTreeMap keys are
/// canonical ULID strings, whose lexicographic max is the numeric max.
fn max_item_id(items: &BTreeMap<String, AgendaItem>) -> Option<ulid::Ulid> {
    items
        .last_key_value()
        .and_then(|(id, _)| ulid::Ulid::from_string(id).ok())
}

/// Two-phase line parse: shape-check the envelope before the typed parse so
/// forward-compatible skips (newer version, unknown op vocabulary) are
/// distinguished from corruption.
fn parse_record(line: &str) -> Result<AgendaOpRecord, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("not JSON: {err}"))?;
    let version = value.get("v").and_then(serde_json::Value::as_u64);
    match version {
        Some(v) if v > u64::from(AGENDA_LOG_VERSION) => {
            return Err(format!("line version {v} is newer than this build"));
        }
        Some(_) => {}
        None => return Err("missing version".into()),
    }
    let op_type = value
        .get("op")
        .and_then(|op| op.get("type"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "missing op.type".to_string())?
        .to_string();
    if !KNOWN_OPS.contains(&op_type.as_str()) {
        return Err(format!("unknown op type {op_type:?}"));
    }
    serde_json::from_value(value).map_err(|err| format!("malformed {op_type} op: {err}"))
}

/// The ruled blocker-id shape (F2 vocabulary):
/// `"bk-" + first 12 hex of sha256("agenda-blocker\0" item "\0" criterion
/// "\0" at_ms)`. Minted once at intake and recorded in the op — replay
/// never mints (§7 purity, same discipline as `effect_id`).
fn mint_blocker_id(item_id: &str, criterion: &str, at_ms: u64) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"agenda-blocker\0");
    hasher.update(item_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(criterion.as_bytes());
    hasher.update(b"\0");
    hasher.update(at_ms.to_string().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(15);
    hex.push_str("bk-");
    for byte in digest.iter().take(6) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The self-described `--source` label: trimmed, bounded, present-or-absent
/// — an empty label is a caller mistake, not "no label".
fn validate_source(source: Option<String>) -> Result<Option<String>, AgendaError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let source = source.trim();
    if source.is_empty() {
        return Err(AgendaError::Invalid(
            "source label must not be empty — omit it instead".into(),
        ));
    }
    if source.chars().count() > MAX_SOURCE_CHARS {
        return Err(AgendaError::Invalid(format!(
            "source label exceeds {MAX_SOURCE_CHARS} characters"
        )));
    }
    Ok(Some(source.to_string()))
}

/// One ref spec after intake validation: locator normalized, file digest
/// minted (recorded in the op — replay never hashes).
struct ValidatedRef {
    ref_type: AgendaRefType,
    locator: String,
    digest: Option<String>,
    must_read: bool,
    label: Option<String>,
}

/// Validate one typed-ref spec (G1). Per-type locator rules with named
/// rejections; file refs must exist, be regular files within the digest
/// bound, and are hashed HERE — attach-time truth, recorded in the op.
fn validate_ref(
    ref_type: AgendaRefType,
    locator: &str,
    must_read: bool,
    label: Option<String>,
) -> Result<ValidatedRef, AgendaError> {
    let locator = locator.trim();
    if locator.is_empty() {
        return Err(AgendaError::Invalid("ref locator must not be empty".into()));
    }
    let label = match label {
        None => None,
        Some(label) => {
            let label = label.trim();
            if label.is_empty() {
                return Err(AgendaError::Invalid(
                    "ref label must not be empty — omit it instead".into(),
                ));
            }
            if label.chars().count() > MAX_REF_LABEL_CHARS {
                return Err(AgendaError::Invalid(format!(
                    "ref label exceeds {MAX_REF_LABEL_CHARS} characters"
                )));
            }
            Some(label.to_string())
        }
    };
    let digest = match ref_type {
        AgendaRefType::File => {
            if locator.chars().count() > MAX_REF_FILE_LOCATOR_CHARS {
                return Err(AgendaError::Invalid(format!(
                    "file ref path exceeds {MAX_REF_FILE_LOCATOR_CHARS} characters"
                )));
            }
            let path = Path::new(locator);
            if !path.is_absolute() {
                return Err(AgendaError::Invalid(
                    "file ref path must be absolute".into(),
                ));
            }
            let meta = std::fs::metadata(path).map_err(|err| {
                AgendaError::Invalid(format!(
                    "cannot attach a file ref: {locator} is not readable ({err})"
                ))
            })?;
            if !meta.is_file() {
                return Err(AgendaError::Invalid(format!(
                    "cannot attach a file ref: {locator} is not a regular file"
                )));
            }
            if meta.len() > MAX_REF_FILE_HASH_BYTES {
                return Err(AgendaError::Invalid(format!(
                    "cannot attach a file ref: {locator} exceeds \
                     {MAX_REF_FILE_HASH_BYTES} bytes — refs point at working \
                     artifacts, not archives"
                )));
            }
            Some(digest_file(path).map_err(|err| {
                AgendaError::Invalid(format!("cannot digest file ref {locator}: {err}"))
            })?)
        }
        AgendaRefType::Memory | AgendaRefType::Session => {
            if locator.chars().count() > MAX_REF_ID_LOCATOR_CHARS {
                return Err(AgendaError::Invalid(format!(
                    "{} ref locator exceeds {MAX_REF_ID_LOCATOR_CHARS} characters",
                    ref_type.as_str()
                )));
            }
            None
        }
        AgendaRefType::Url => {
            if locator.chars().count() > MAX_REF_URL_LOCATOR_CHARS {
                return Err(AgendaError::Invalid(format!(
                    "url ref exceeds {MAX_REF_URL_LOCATOR_CHARS} characters"
                )));
            }
            if !locator.starts_with("http://") && !locator.starts_with("https://") {
                return Err(AgendaError::Invalid(
                    "url ref must start with http:// or https://".into(),
                ));
            }
            None
        }
    };
    Ok(ValidatedRef {
        ref_type,
        locator: locator.to_string(),
        digest,
        must_read,
        label,
    })
}

/// Expand-time drift judgment of one file ref against its recorded attach
/// digest (G1): `missing` (unreadable or not a regular file), `changed`,
/// or `unchanged`. A file grown past the digest bound is `changed` by size
/// alone — attach only ever recorded digests of files within the bound.
/// Presentation only: never stored, never a DTO field; callers invoke it
/// on detail expand, never on list render.
pub(crate) fn file_ref_drift(locator: &str, attach_digest: &str) -> &'static str {
    let path = Path::new(locator);
    let Ok(meta) = std::fs::metadata(path) else {
        return "missing";
    };
    if !meta.is_file() {
        return "missing";
    }
    if meta.len() > MAX_REF_FILE_HASH_BYTES {
        return "changed";
    }
    match digest_file(path) {
        Ok(digest) if digest == attach_digest => "unchanged",
        Ok(_) => "changed",
        Err(_) => "missing",
    }
}

/// Full sha256 of a file's bytes as lowercase hex, streamed in bounded
/// chunks. Shared by attach-time intake and the expand-time drift check.
pub(crate) fn digest_file(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

fn validate_title(title: &str) -> Result<String, AgendaError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AgendaError::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(AgendaError::Invalid(format!(
            "title exceeds {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title.to_string())
}

fn validate_body(body: String) -> Result<String, AgendaError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(AgendaError::Invalid(format!(
            "body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(body)
}

fn validate_answer(text: &str) -> Result<String, AgendaError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AgendaError::Invalid("answer must not be empty".into()));
    }
    if text.len() > MAX_BODY_BYTES {
        return Err(AgendaError::Invalid(format!(
            "answer exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(text.to_string())
}

/// Trim, drop duplicates (first occurrence wins), reject empties and
/// oversizes. Case is preserved — tags are the owner's vocabulary.
fn validate_tags(tags: Vec<String>) -> Result<Vec<String>, AgendaError> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(AgendaError::Invalid("tags must not be empty".into()));
        }
        if tag.chars().count() > MAX_TAG_CHARS {
            return Err(AgendaError::Invalid(format!(
                "tag exceeds {MAX_TAG_CHARS} characters"
            )));
        }
        if !out.iter().any(|existing| existing == tag) {
            out.push(tag.to_string());
        }
    }
    if out.len() > MAX_TAGS {
        return Err(AgendaError::Invalid(format!("more than {MAX_TAGS} tags")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::types::AgendaKind;
    use super::*;

    fn add_cmd(title: &str) -> AgendaCommand {
        AgendaCommand::Add {
            refs: Vec::new(),
            kind: AgendaKind::Task,
            title: title.to_string(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            source: None,
        }
    }

    fn owner() -> Option<AgendaActor> {
        Some(AgendaActor {
            principal: Some("owner".into()),
            session_id: None,
            kind: None,
        })
    }

    #[test]
    fn add_persists_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        assert!(store.snapshot().is_empty());

        let item = store
            .apply_command(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Note,
                    title: "  remember the milk  ".into(),
                    body: "whole, not oat".into(),
                    tags: vec![" grocery ".into(), "grocery".into(), "later".into()],
                    due_ms: Some(1_752_000_000_000),
                    source: None,
                },
                owner(),
                1000,
            )
            .unwrap();
        assert_eq!(item.title, "remember the milk");
        assert_eq!(item.tags, vec!["grocery".to_string(), "later".to_string()]);
        assert_eq!(item.status, AgendaStatus::Open);
        assert_eq!(item.provenance.principal.as_deref(), Some("owner"));
        assert_eq!(item.provenance.created_ms, 1000);
        assert_eq!(item.id.len(), 26);

        store
            .apply_command(
                AgendaCommand::Complete {
                    id: item.id.clone(),
                    source: None,
                },
                None,
                2000,
            )
            .unwrap();

        // The A1 acceptance property at unit level: restart ⇒ history intact.
        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        let items = store.snapshot();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, AgendaStatus::Done);
        assert_eq!(items[0].completed_ms, Some(2000));
        assert_eq!(items[0].title, "remember the milk");
        assert_eq!(store.ops(), 2);
        assert_eq!(store.skipped_lines(), 0);
    }

    /// Mint order must equal id order even when the wall clock has not
    /// advanced past the largest id already on disk — the
    /// same-millisecond store reopen a warm CI runner caught live
    /// (fresh `Generator` per open has no cross-instance monotonicity).
    /// Deterministic here: the floor is forced into the far future, so
    /// every fresh ULID sorts below it and must take the increment path;
    /// the floor must also survive a reopen via the folded max.
    #[test]
    fn mint_floor_keeps_ids_ordered_when_clock_stalls() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let future = "7ZZZZZZZZZ0000000000000000";
        store.force_last_id_for_tests(future);

        let first = store.apply_command(add_cmd("first"), None, 1).unwrap().id;
        let second = store.apply_command(add_cmd("second"), None, 2).unwrap().id;
        assert!(first.as_str() > future, "{first} must be above the floor");
        assert!(second > first);

        // Reopen: the floor is re-derived from the folded max, so a mint
        // in the (real-clock) past still sorts after everything on disk.
        drop(store);
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let third = store.apply_command(add_cmd("third"), None, 3).unwrap().id;
        assert!(third > second);
        let titles: Vec<String> = store.snapshot().into_iter().map(|i| i.title).collect();
        assert_eq!(
            titles,
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn ids_are_unique_and_creation_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let ids: Vec<String> = (0..20)
            .map(|i| {
                store
                    .apply_command(add_cmd(&format!("t{i}")), None, 1)
                    .unwrap()
                    .id
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids, sorted, "mint order must equal id order");
        // Snapshot iterates the same order.
        let snap_ids: Vec<String> = store.snapshot().into_iter().map(|i| i.id).collect();
        assert_eq!(snap_ids, ids);
    }

    #[test]
    fn transition_rules_are_strict_at_intake() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store.apply_command(add_cmd("t"), None, 1).unwrap().id;

        let complete = AgendaCommand::Complete {
            id: id.clone(),
            source: None,
        };
        let reopen = AgendaCommand::Reopen {
            id: id.clone(),
            source: None,
        };
        let retire = AgendaCommand::Retire {
            id: id.clone(),
            source: None,
        };

        assert!(matches!(
            store.apply_command(
                AgendaCommand::Complete {
                    id: "01UNKNOWN".into(),
                    source: None,
                },
                None,
                2
            ),
            Err(AgendaError::NotFound(_))
        ));
        assert!(matches!(
            store.apply_command(reopen.clone(), None, 2),
            Err(AgendaError::Transition(_))
        ));
        store.apply_command(complete.clone(), None, 3).unwrap();
        assert!(matches!(
            store.apply_command(complete.clone(), None, 4),
            Err(AgendaError::Transition(_))
        ));
        store.apply_command(retire.clone(), None, 5).unwrap();
        assert!(matches!(
            store.apply_command(complete, None, 6),
            Err(AgendaError::Transition(_))
        ));
        assert!(matches!(
            store.apply_command(retire, None, 7),
            Err(AgendaError::Transition(_))
        ));
        // Reopen resurrects retired.
        store.apply_command(reopen, None, 8).unwrap();
        assert_eq!(store.get(&id).unwrap().status, AgendaStatus::Open);
        // Only the accepted ops reached the log.
        assert_eq!(store.ops(), 4);
    }

    #[test]
    fn validation_rejects_bad_input_without_appending() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        for cmd in [
            add_cmd("   "),
            add_cmd(&"x".repeat(MAX_TITLE_CHARS + 1)),
            AgendaCommand::Add {
                refs: Vec::new(),
                kind: AgendaKind::Note,
                title: "t".into(),
                body: "b".repeat(MAX_BODY_BYTES + 1),
                tags: Vec::new(),
                due_ms: None,
                source: None,
            },
            AgendaCommand::Add {
                refs: Vec::new(),
                kind: AgendaKind::Note,
                title: "t".into(),
                body: String::new(),
                tags: vec!["  ".into()],
                due_ms: None,
                source: None,
            },
            AgendaCommand::Add {
                refs: Vec::new(),
                kind: AgendaKind::Note,
                title: "t".into(),
                body: String::new(),
                tags: (0..=MAX_TAGS).map(|i| format!("t{i}")).collect(),
                due_ms: None,
                source: None,
            },
        ] {
            assert!(matches!(
                store.apply_command(cmd, None, 1),
                Err(AgendaError::Invalid(_))
            ));
        }
        let id = store.apply_command(add_cmd("ok"), None, 2).unwrap().id;
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Patch {
                    id,
                    patch: AgendaPatch::default(),
                    source: None,
                },
                None,
                3,
            ),
            Err(AgendaError::Invalid(_))
        ));
        assert_eq!(store.ops(), 1);
        // Rejected commands left no trace in the log.
        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.ops(), 1);
        assert_eq!(store.snapshot().len(), 1);
    }

    #[test]
    fn patch_edits_presentation_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store.apply_command(add_cmd("before"), None, 1).unwrap().id;
        let item = store
            .apply_command(
                AgendaCommand::Patch {
                    id: id.clone(),
                    patch: AgendaPatch {
                        title: Some("after".into()),
                        due_ms: Some(Some(42)),
                        ..AgendaPatch::default()
                    },
                    source: None,
                },
                None,
                2,
            )
            .unwrap();
        assert_eq!(item.title, "after");
        assert_eq!(item.due_ms, Some(42));
        let item = store
            .apply_command(
                AgendaCommand::Patch {
                    id,
                    patch: AgendaPatch {
                        due_ms: Some(None),
                        ..AgendaPatch::default()
                    },
                    source: None,
                },
                None,
                3,
            )
            .unwrap();
        assert_eq!(item.due_ms, None);
        assert_eq!(item.title, "after");
    }

    #[test]
    fn torn_tail_is_terminated_skipped_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        store.apply_command(add_cmd("first"), None, 1).unwrap();
        let log_path = store.log_path().to_path_buf();
        drop(store);

        // Simulate a crash mid-append: a torn, newline-less final line.
        let mut file = std::fs::File::options()
            .append(true)
            .open(&log_path)
            .unwrap();
        file.write_all(b"{\"v\":1,\"at_ms\":9,\"op\":{\"ty")
            .unwrap();
        drop(file);

        let mut store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.skipped_lines(), 1);
        // Appends after the torn tail land on their own line.
        store.apply_command(add_cmd("second"), None, 10).unwrap();
        drop(store);

        let store = AgendaStore::open(dir.path()).unwrap();
        let titles: Vec<String> = store.snapshot().into_iter().map(|i| i.title).collect();
        assert_eq!(titles, vec!["first".to_string(), "second".to_string()]);
        assert_eq!(store.skipped_lines(), 1);
        // The torn line is preserved on disk, not repaired away.
        let raw = std::fs::read_to_string(&log_path).unwrap();
        assert!(raw.contains("{\"v\":1,\"at_ms\":9,\"op\":{\"ty\n"));
    }

    /// A4 intake strictness + persistence: answers only land on open
    /// questions, and the reply (with attribution) survives a reopen of
    /// the store.
    #[test]
    fn answers_are_strict_at_intake_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let question = store
            .apply_command(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Question,
                    title: "Rotate the fleet certs this week?".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: None,
                },
                None,
                1,
            )
            .unwrap();
        let task = store
            .apply_command(add_cmd("not a question"), None, 2)
            .unwrap();

        // Wrong kind, empty text, then a real answer.
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Answer {
                    id: task.id.clone(),
                    text: "irrelevant".into(),
                    structured: None,
                    source: None,
                },
                None,
                3,
            ),
            Err(AgendaError::Invalid(_))
        ));
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Answer {
                    id: question.id.clone(),
                    text: "   ".into(),
                    structured: None,
                    source: None,
                },
                None,
                4,
            ),
            Err(AgendaError::Invalid(_))
        ));
        let answered = store
            .apply_command(
                AgendaCommand::Answer {
                    id: question.id.clone(),
                    text: "yes, before Friday".into(),
                    structured: None,
                    source: None,
                },
                owner(),
                5,
            )
            .unwrap();
        assert_eq!(answered.status, AgendaStatus::Done);
        assert_eq!(
            answered.answer.as_ref().unwrap().principal.as_deref(),
            Some("owner")
        );

        // Double-answer is a transition error; reopen re-asks.
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Answer {
                    id: question.id.clone(),
                    text: "again".into(),
                    structured: None,
                    source: None,
                },
                None,
                6,
            ),
            Err(AgendaError::Transition(_))
        ));

        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        let reloaded = store.get(&question.id).unwrap();
        assert_eq!(reloaded.answer.as_ref().unwrap().text, "yes, before Friday");
        assert_eq!(reloaded.status, AgendaStatus::Done);
    }

    /// `--source` labels: validated at intake (trimmed, bounded, never
    /// empty), recorded on the envelope, folded into add provenance, and
    /// persistent across reopen. Owner-surface verbs have no such field.
    #[test]
    fn source_labels_validate_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();

        let item = store
            .apply_command(
                AgendaCommand::Add {
                    refs: Vec::new(),
                    kind: AgendaKind::Task,
                    title: "rotate certs".into(),
                    body: String::new(),
                    tags: Vec::new(),
                    due_ms: None,
                    source: Some("  deploy-hook  ".into()),
                },
                None,
                1,
            )
            .unwrap();
        assert_eq!(item.provenance.source.as_deref(), Some("deploy-hook"));

        // Whitespace-only and oversized labels are caller mistakes.
        for bad in [" ".to_string(), "x".repeat(MAX_SOURCE_CHARS + 1)] {
            assert!(matches!(
                store.apply_command(
                    AgendaCommand::Add {
                        refs: Vec::new(),
                        kind: AgendaKind::Task,
                        title: "t".into(),
                        body: String::new(),
                        tags: Vec::new(),
                        due_ms: None,
                        source: Some(bad),
                    },
                    None,
                    2,
                ),
                Err(AgendaError::Invalid(_))
            ));
        }

        // A labeled non-add op records the label on its envelope line.
        store
            .apply_command(
                AgendaCommand::Complete {
                    id: item.id.clone(),
                    source: Some("deploy-hook".into()),
                },
                None,
                3,
            )
            .unwrap();
        let raw = std::fs::read_to_string(store.log_path()).unwrap();
        let completes: Vec<&str> = raw
            .lines()
            .filter(|line| line.contains(r#""type":"complete""#))
            .collect();
        assert_eq!(completes.len(), 1);
        assert!(completes[0].contains(r#""source":"deploy-hook""#));

        // Reopen the store: the folded provenance still carries the label.
        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(
            store.get(&item.id).unwrap().provenance.source.as_deref(),
            Some("deploy-hook")
        );
    }

    /// Two daemons on one home share the log; each converges on the
    /// other's appends via the stat-cheap staleness check.
    #[test]
    fn concurrent_instances_converge_via_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = AgendaStore::open(dir.path()).unwrap();
        let mut b = AgendaStore::open(dir.path()).unwrap();

        let from_a = a.apply_command(add_cmd("from a"), None, 1).unwrap();
        // B validates against fresh state, so it can act on A's item.
        b.apply_command(
            AgendaCommand::Complete {
                id: from_a.id.clone(),
                source: None,
            },
            None,
            2,
        )
        .unwrap();
        assert_eq!(b.snapshot().len(), 1);

        a.refresh_if_stale().unwrap();
        assert_eq!(a.get(&from_a.id).unwrap().status, AgendaStatus::Done);
        assert_eq!(a.ops(), 2);

        // No-op refresh when nothing changed.
        let before = a.snapshot();
        a.refresh_if_stale().unwrap();
        assert_eq!(a.snapshot(), before);
    }

    #[test]
    fn foreign_vocabulary_lines_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        store.apply_command(add_cmd("mine"), None, 1).unwrap();
        let log_path = store.log_path().to_path_buf();
        drop(store);

        let mut file = std::fs::File::options()
            .append(true)
            .open(&log_path)
            .unwrap();
        // A future build's op, a future line version, and junk.
        file.write_all(
            b"{\"v\":1,\"at_ms\":2,\"op\":{\"type\":\"propose_effect\",\"id\":\"x\"}}\n\
              {\"v\":2,\"at_ms\":3,\"op\":{\"type\":\"add\",\"id\":\"y\"}}\n\
              not json at all\n",
        )
        .unwrap();
        drop(file);

        let store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.ops(), 1);
        assert_eq!(store.skipped_lines(), 3);
        // The skipped lines are preserved on disk verbatim, not repaired
        // away — a newer build folds them later.
        let raw = std::fs::read_to_string(store.log_path()).unwrap();
        assert!(raw.contains(r#""type":"propose_effect""#));
    }

    /// F2 intake strictness + persistence: the five thread/gate verbs
    /// validate at intake (empty/oversized text, non-open items for
    /// set_blocker/add_relies_on, duplicates, self-edges, missing targets,
    /// caps), mint blocker ids server-side, and the folded state survives
    /// a store reopen. The forward-compat property (older builds
    /// skip-and-preserve unknown vocabulary) keeps holding for whatever
    /// comes after F2.
    #[test]
    fn f2_verbs_are_strict_at_intake_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let a = store.apply_command(add_cmd("dependent"), None, 1).unwrap();
        let b = store
            .apply_command(add_cmd("prerequisite"), None, 2)
            .unwrap();

        // Annotate: empty and unknown-item rejected; a real one lands with
        // the envelope source.
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Annotate {
                    id: a.id.clone(),
                    text: "   ".into(),
                    source: None,
                },
                None,
                3,
            ),
            Err(AgendaError::Invalid(_))
        ));
        assert!(matches!(
            store.apply_command(
                AgendaCommand::Annotate {
                    id: "01UNKNOWN".into(),
                    text: "x".into(),
                    source: None,
                },
                None,
                3,
            ),
            Err(AgendaError::NotFound(_))
        ));
        let annotated = store
            .apply_command(
                AgendaCommand::Annotate {
                    id: a.id.clone(),
                    text: "waiting on the API rollout".into(),
                    source: Some("deploy-hook".into()),
                },
                None,
                4,
            )
            .unwrap();
        assert_eq!(annotated.annotations.len(), 1);
        assert_eq!(
            annotated.annotations[0].source.as_deref(),
            Some("deploy-hook")
        );

        // SetBlocker: open-only, bounded criterion, duplicate criterion
        // refused, id minted server-side (bk- prefix).
        let blocked = store
            .apply_command(
                AgendaCommand::SetBlocker {
                    id: a.id.clone(),
                    criterion: "gpt-live-1 available on the API".into(),
                    source: None,
                },
                None,
                5,
            )
            .unwrap();
        let blocker_id = blocked.blockers[0].blocker_id.clone();
        assert!(blocker_id.starts_with("bk-") && blocker_id.len() == 15);
        assert!(matches!(
            store.apply_command(
                AgendaCommand::SetBlocker {
                    id: a.id.clone(),
                    criterion: "gpt-live-1 available on the API".into(),
                    source: None,
                },
                None,
                6,
            ),
            Err(AgendaError::Transition(_))
        ));
        assert!(matches!(
            store.apply_command(
                AgendaCommand::SetBlocker {
                    id: a.id.clone(),
                    criterion: "c".repeat(MAX_CRITERION_CHARS + 1),
                    source: None,
                },
                None,
                6,
            ),
            Err(AgendaError::Invalid(_))
        ));

        // AddReliesOn: self-edge, missing target, duplicate all refused.
        assert!(matches!(
            store.apply_command(
                AgendaCommand::AddReliesOn {
                    id: a.id.clone(),
                    target_id: a.id.clone(),
                    source: None,
                },
                None,
                7,
            ),
            Err(AgendaError::Invalid(_))
        ));
        assert!(matches!(
            store.apply_command(
                AgendaCommand::AddReliesOn {
                    id: a.id.clone(),
                    target_id: "01UNKNOWN".into(),
                    source: None,
                },
                None,
                7,
            ),
            Err(AgendaError::NotFound(_))
        ));
        store
            .apply_command(
                AgendaCommand::AddReliesOn {
                    id: a.id.clone(),
                    target_id: b.id.clone(),
                    source: None,
                },
                None,
                8,
            )
            .unwrap();
        assert!(matches!(
            store.apply_command(
                AgendaCommand::AddReliesOn {
                    id: a.id.clone(),
                    target_id: b.id.clone(),
                    source: None,
                },
                None,
                9,
            ),
            Err(AgendaError::Transition(_))
        ));

        // ClearBlocker resolves a unique prefix to the full id at intake
        // (the durable op carries the exact id); RemoveReliesOn needs a
        // live edge.
        let cleared = store
            .apply_command(
                AgendaCommand::ClearBlocker {
                    id: a.id.clone(),
                    blocker_id: blocker_id[..6].to_string(),
                    source: None,
                },
                None,
                10,
            )
            .unwrap();
        assert!(cleared.blockers[0].cleared.is_some());
        assert!(matches!(
            store.apply_command(
                AgendaCommand::ClearBlocker {
                    id: a.id.clone(),
                    blocker_id,
                    source: None,
                },
                None,
                11,
            ),
            Err(AgendaError::NotFound(_)),
        ));
        store
            .apply_command(
                AgendaCommand::RemoveReliesOn {
                    id: a.id.clone(),
                    target_id: b.id.clone(),
                    source: None,
                },
                None,
                12,
            )
            .unwrap();
        assert!(matches!(
            store.apply_command(
                AgendaCommand::RemoveReliesOn {
                    id: a.id.clone(),
                    target_id: b.id.clone(),
                    source: None,
                },
                None,
                13,
            ),
            Err(AgendaError::NotFound(_))
        ));

        // Blocking a completed item is a transition error.
        store
            .apply_command(
                AgendaCommand::Complete {
                    id: b.id.clone(),
                    source: None,
                },
                None,
                14,
            )
            .unwrap();
        assert!(matches!(
            store.apply_command(
                AgendaCommand::SetBlocker {
                    id: b.id.clone(),
                    criterion: "too late".into(),
                    source: None,
                },
                None,
                15,
            ),
            Err(AgendaError::Transition(_))
        ));
        // …but annotating it is fine (housekeeping annotates without
        // disposing).
        store
            .apply_command(
                AgendaCommand::Annotate {
                    id: b.id.clone(),
                    text: "post-completion evidence".into(),
                    source: None,
                },
                None,
                16,
            )
            .unwrap();

        // Reopen the store: thread, blocker history (cleared entry kept),
        // and the removed edge all replay exactly.
        drop(store);
        let store = AgendaStore::open(dir.path()).unwrap();
        let a_re = store.get(&a.id).unwrap();
        assert_eq!(a_re.annotations.len(), 1);
        assert_eq!(a_re.blockers.len(), 1);
        assert!(a_re.blockers[0].cleared.is_some());
        assert!(a_re.relies_on.is_empty());
        assert_eq!(store.get(&b.id).unwrap().annotations.len(), 1);
    }

    fn ask_question(text: &str) -> crate::mcp::AskUserQuestionParams {
        crate::mcp::AskUserQuestionParams {
            question: text.to_string(),
            header: None,
            options: Vec::new(),
            previews: Vec::new(),
            pick_min: None,
            pick_max: None,
            free_text: None,
        }
    }

    fn ask_cmd(questions: Vec<crate::mcp::AskUserQuestionParams>) -> AgendaCommand {
        AgendaCommand::Ask { questions }
    }

    /// Slice 1's park path end to end at store level: validation rides the
    /// ask_user validator, ids are daemon-minted, preview blobs land in the
    /// agenda blob store as references, and the whole thing survives a
    /// store reopen.
    #[test]
    fn park_ask_commits_blobs_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();

        let mut question = ask_question("Which grid layout should the dashboard use?");
        question.header = Some("Grid".into());
        question.options = vec![crate::mcp::AskUserOptionParams {
            label: "A".into(),
            description: Some("dense".into()),
        }];
        question.previews = vec![
            crate::mcp::AskUserPreviewParams {
                label: "A".into(),
                html: Some("<html><body>A</body></html>".into()),
                image: None,
                media_type: None,
                text: None,
            },
            crate::mcp::AskUserPreviewParams {
                label: "notes".into(),
                html: None,
                image: None,
                media_type: None,
                text: Some("inline snippet".into()),
            },
        ];
        let item = store
            .apply_command(
                ask_cmd(vec![question, ask_question("Second question?")]),
                None,
                5,
            )
            .unwrap();
        assert_eq!(item.kind, AgendaKind::Question);
        assert_eq!(item.status, AgendaStatus::Open);
        assert_eq!(item.title, "Which grid layout should the dashboard use?");
        let ask = item.ask.as_ref().unwrap();
        assert!(ask.ask_id >= (1 << 44));
        assert_eq!(ask.questions.len(), 2);
        // The html preview became an agenda blob reference; text stayed inline.
        let previews = &ask.questions[0].previews;
        assert_eq!(previews.len(), 2);
        match &previews[0].source {
            crate::types::QuestionPreviewSource::Html { upload_id, url } => {
                assert_eq!(
                    url,
                    &format!("/api/agenda/blobs/{}/{}/raw", item.id, upload_id)
                );
                let (descriptor, path) =
                    super::super::blobs::find_blob(dir.path(), &item.id, upload_id).unwrap();
                assert_eq!(descriptor.mime, "text/html");
                assert!(std::fs::read_to_string(path).unwrap().contains("A"));
            }
            other => panic!("expected html blob reference, got {other:?}"),
        }
        assert!(matches!(
            previews[1].source,
            crate::types::QuestionPreviewSource::Text { .. }
        ));
        // The registry sees the open ask; the allocator floor is above it.
        assert!(super::super::ask::agenda_ask_pending(ask.ask_id));
        assert!(crate::event::next_approval_id() > ask.ask_id);

        // Reopen the store: the ask payload and blob survive.
        drop(store);
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let reloaded = store.open_ask(ask.ask_id).unwrap();
        assert_eq!(reloaded.id, item.id);
        assert_eq!(reloaded.ask.as_ref().unwrap().questions.len(), 2);
    }

    /// Refused validation strands nothing: no blobs dir, no log line.
    #[test]
    fn refused_ask_leaves_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        // Empty questions list.
        assert!(matches!(
            store.apply_command(ask_cmd(Vec::new()), None, 1),
            Err(AgendaError::Invalid(_))
        ));
        // Bad pick bounds (validator error surfaces verbatim).
        let mut bad = ask_question("Pick?");
        bad.pick_max = Some(3);
        assert!(matches!(
            store.apply_command(ask_cmd(vec![bad]), None, 2),
            Err(AgendaError::Invalid(_))
        ));
        assert_eq!(store.ops(), 0);
        assert!(!super::super::blobs::blobs_root(dir.path()).exists());
    }

    /// The ask-delivery marker round-trip at store level: recorded only on
    /// items with a current answer, superseded by a later write-back,
    /// durable across a store reopen (refold from disk), and cleared with
    /// the answer view when the question reopens.
    #[test]
    fn record_ask_delivery_round_trips_and_requires_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let item = store
            .apply_command(ask_cmd(vec![ask_question("Which grid?")]), None, 1)
            .unwrap();

        // No answer yet: named refusal, nothing appended.
        assert!(matches!(
            store.record_ask_delivery(&item.id, false, None, 2),
            Err(AgendaError::Invalid(_))
        ));
        assert!(matches!(
            store.record_ask_delivery("01UNKNOWN", false, None, 2),
            Err(AgendaError::NotFound(_))
        ));

        store
            .apply_command(
                AgendaCommand::Answer {
                    id: item.id.clone(),
                    text: "A".into(),
                    structured: None,
                    source: None,
                },
                None,
                3,
            )
            .unwrap();
        let marked = store.record_ask_delivery(&item.id, false, None, 4).unwrap();
        assert_eq!(marked.answer.as_ref().unwrap().delivered, Some(false));

        // A later successful successor delivery supersedes the miss.
        let flipped = store
            .record_ask_delivery(&item.id, true, Some("sess-successor".into()), 5)
            .unwrap();
        assert_eq!(flipped.answer.as_ref().unwrap().delivered, Some(true));

        // Refold from disk: the marker is durable history.
        drop(store);
        let mut store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(
            store
                .item(&item.id)
                .unwrap()
                .answer
                .as_ref()
                .unwrap()
                .delivered,
            Some(true)
        );

        // Reopen clears the answer (and its marker) from the view; the
        // write-back is then refused until a fresh answer lands.
        store
            .apply_command(
                AgendaCommand::Reopen {
                    id: item.id.clone(),
                    source: None,
                },
                None,
                6,
            )
            .unwrap();
        assert!(store.item(&item.id).unwrap().answer.is_none());
        assert!(matches!(
            store.record_ask_delivery(&item.id, true, None, 7),
            Err(AgendaError::Invalid(_))
        ));
    }

    /// Answer with structured fields completes the item and records both
    /// forms; dismissal leaves it open with the marker; retire deletes the
    /// blobs (completion does NOT).
    #[test]
    fn ask_answer_dismiss_and_retire_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let mut question = ask_question("Ship it?");
        question.previews = vec![crate::mcp::AskUserPreviewParams {
            label: "diff".into(),
            html: Some("<html>diff</html>".into()),
            image: None,
            media_type: None,
            text: None,
        }];
        let item = store
            .apply_command(ask_cmd(vec![question]), None, 1)
            .unwrap();
        let ask_id = item.ask.as_ref().unwrap().ask_id;
        let blob_id = match &item.ask.as_ref().unwrap().questions[0].previews[0].source {
            crate::types::QuestionPreviewSource::Html { upload_id, .. } => upload_id.clone(),
            other => panic!("unexpected {other:?}"),
        };

        // Dismiss (rail skip): marker recorded, still open, still pending.
        let dismissed = store.dismiss_question(&item.id, "skip", None, 2).unwrap();
        assert_eq!(dismissed.status, AgendaStatus::Open);
        assert_eq!(dismissed.dismissed.as_ref().unwrap().action, "skip");
        assert!(super::super::ask::agenda_ask_pending(ask_id));
        assert!(store.open_ask(ask_id).is_some());

        // Structured answer completes it; blobs SURVIVE completion (the
        // archive shows previews).
        let structured = super::super::types::AgendaAskResolution {
            answers: [("Ship it?".to_string(), "yes".to_string())].into(),
            selections: [("Ship it?".to_string(), vec!["yes".to_string()])].into(),
            followups: BTreeMap::new(),
            annotations: BTreeMap::new(),
        };
        let answered = store
            .apply_command(
                AgendaCommand::Answer {
                    id: item.id.clone(),
                    text: "yes".into(),
                    structured: Some(structured.clone()),
                    source: None,
                },
                owner(),
                3,
            )
            .unwrap();
        assert_eq!(answered.status, AgendaStatus::Done);
        assert!(answered.dismissed.is_none());
        assert_eq!(
            answered
                .answer
                .as_ref()
                .unwrap()
                .structured
                .as_ref()
                .unwrap(),
            &structured
        );
        assert!(!super::super::ask::agenda_ask_pending(ask_id));
        assert!(store.open_ask(ask_id).is_none());
        assert!(super::super::blobs::find_blob(dir.path(), &item.id, &blob_id).is_some());

        // Dismissal of a resolved item is refused at intake.
        assert!(matches!(
            store.dismiss_question(&item.id, "skip", None, 4),
            Err(AgendaError::Transition(_))
        ));

        // Retire: retention ends, blobs are deleted.
        store
            .apply_command(
                AgendaCommand::Retire {
                    id: item.id.clone(),
                    source: None,
                },
                None,
                5,
            )
            .unwrap();
        assert!(super::super::blobs::find_blob(dir.path(), &item.id, &blob_id).is_none());

        // Reopen re-asks: the ask becomes pending again (payload intact,
        // blob gone — the rail shows the missing-preview chip).
        let reopened = store
            .apply_command(
                AgendaCommand::Reopen {
                    id: item.id.clone(),
                    source: None,
                },
                None,
                6,
            )
            .unwrap();
        assert_eq!(reopened.status, AgendaStatus::Open);
        assert!(super::super::ask::agenda_ask_pending(ask_id));
    }

    /// Restart-collision guard: a fresh process whose allocator would
    /// re-mint a persisted ask id is floored above it at fold time.
    #[test]
    fn persisted_ask_ids_floor_the_approval_allocator() {
        let dir = tempfile::tempdir().unwrap();
        // Forge a log with an enormous ask id (far above anything this
        // process has minted).
        let forged_ask_id: u64 = (1 << 50) + 123;
        let line = format!(
            "{{\"v\":1,\"at_ms\":1,\"op\":{{\"type\":\"add\",\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"kind\":\"question\",\"title\":\"forged\",\"ask\":{{\"ask_id\":{forged_ask_id},\"questions\":[{{\"question\":\"forged?\"}}]}}}}}}\n"
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(LOG_FILE), line).unwrap();
        let _store = AgendaStore::open(dir.path()).unwrap();
        assert!(crate::event::next_approval_id() > forged_ask_id);
    }

    fn add_ref_cmd(id: &str, ref_type: AgendaRefType, locator: &str) -> AgendaCommand {
        AgendaCommand::AddRef {
            id: id.to_string(),
            ref_type,
            locator: locator.to_string(),
            must_read: false,
            label: None,
            source: None,
        }
    }

    /// The G1 intake omnibus (the F2 verbs test's sibling): per-type
    /// locator rules with named rejections, the attach-time file digest
    /// recorded in the op, duplicate/cap/remove strictness, and
    /// remove-is-an-op history.
    #[test]
    fn g1_refs_are_strict_at_intake_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store
            .apply_command(add_cmd("carry the brief"), owner(), 1000)
            .unwrap()
            .id;

        // File ref: digest minted at intake, full sha256 hex, recorded.
        let brief = files.path().join("brief.md");
        std::fs::write(&brief, b"typed refs, not blobs").unwrap();
        let brief_loc = brief.to_string_lossy().into_owned();
        let item = store
            .apply_command(
                AgendaCommand::AddRef {
                    id: id.clone(),
                    ref_type: AgendaRefType::File,
                    locator: brief_loc.clone(),
                    must_read: true,
                    label: Some("  kickoff brief  ".into()),
                    source: Some("track-g".into()),
                },
                owner(),
                1001,
            )
            .unwrap();
        assert_eq!(item.refs.len(), 1);
        let r = &item.refs[0];
        assert_eq!(r.ref_type, AgendaRefType::File);
        assert!(r.must_read);
        assert_eq!(r.label.as_deref(), Some("kickoff brief"));
        assert_eq!(r.source.as_deref(), Some("track-g"));
        let digest = r.digest.clone().expect("file refs carry a digest");
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, digest_file(&brief).unwrap());

        // Named rejections, none appending: relative path, missing file,
        // directory, bad url scheme, empty/oversize labels and locators.
        let ops_before = store.ops();
        for (cmd, needle) in [
            (
                add_ref_cmd(&id, AgendaRefType::File, "relative/path.md"),
                "absolute",
            ),
            (
                add_ref_cmd(
                    &id,
                    AgendaRefType::File,
                    &files.path().join("gone.md").to_string_lossy(),
                ),
                "not readable",
            ),
            (
                add_ref_cmd(&id, AgendaRefType::File, &files.path().to_string_lossy()),
                "not a regular file",
            ),
            (
                add_ref_cmd(&id, AgendaRefType::Url, "ftp://example.com/x"),
                "http:// or https://",
            ),
            (add_ref_cmd(&id, AgendaRefType::Memory, "   "), "empty"),
            (
                add_ref_cmd(&id, AgendaRefType::Session, &"s".repeat(201)),
                "exceeds 200",
            ),
        ] {
            let err = store.apply_command(cmd, owner(), 1002).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in {err}"
            );
        }
        let err = store
            .apply_command(
                AgendaCommand::AddRef {
                    id: id.clone(),
                    ref_type: AgendaRefType::Url,
                    locator: "https://example.com/pr/1".into(),
                    must_read: false,
                    label: Some("   ".into()),
                    source: None,
                },
                owner(),
                1002,
            )
            .unwrap_err();
        assert!(err.to_string().contains("label must not be empty"));
        assert_eq!(store.ops(), ops_before, "rejections never append");

        // Oversize file: sparse-extended past the digest bound, named.
        let big = files.path().join("big.bin");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_REF_FILE_HASH_BYTES + 1).unwrap();
        drop(f);
        let err = store
            .apply_command(
                add_ref_cmd(&id, AgendaRefType::File, &big.to_string_lossy()),
                owner(),
                1003,
            )
            .unwrap_err();
        assert!(err.to_string().contains("not archives"));

        // Duplicate live ref is a transition error; a second TYPE with the
        // same locator string is a different address and attaches.
        let err = store
            .apply_command(
                add_ref_cmd(&id, AgendaRefType::File, &brief_loc),
                owner(),
                1004,
            )
            .unwrap_err();
        assert!(err.to_string().contains("already carries"));

        // memory/session/url refs attach digest-less.
        for (rt, loc) in [
            (AgendaRefType::Memory, "mem-claim-abc123"),
            (AgendaRefType::Session, "sess-4242"),
            (AgendaRefType::Url, "https://example.com/pr/7"),
        ] {
            let item = store
                .apply_command(add_ref_cmd(&id, rt, loc), owner(), 1005)
                .unwrap();
            let r = item.refs.iter().find(|r| r.locator == loc).unwrap();
            assert_eq!(r.ref_type, rt);
            assert!(r.digest.is_none());
        }

        // Remove is an op: the view drops the ref, the log keeps history.
        let ops_before = store.ops();
        let item = store
            .apply_command(
                AgendaCommand::RemoveRef {
                    id: id.clone(),
                    ref_type: AgendaRefType::Url,
                    locator: "https://example.com/pr/7".into(),
                    source: None,
                },
                owner(),
                1006,
            )
            .unwrap();
        assert!(!item.refs.iter().any(|r| r.locator.contains("/pr/7")));
        assert_eq!(store.ops(), ops_before + 1);
        let err = store
            .apply_command(
                AgendaCommand::RemoveRef {
                    id: id.clone(),
                    ref_type: AgendaRefType::Url,
                    locator: "https://example.com/pr/7".into(),
                    source: None,
                },
                owner(),
                1007,
            )
            .unwrap_err();
        assert!(matches!(err, AgendaError::NotFound(_)));

        // Reopen replays the log to the same view.
        let before = store.item(&id).unwrap();
        let mut reopened = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(reopened.item(&id).unwrap(), before);

        // The ref cap is enforced against live refs with a named error.
        let small = store
            .apply_command(add_cmd("cap me"), owner(), 2000)
            .unwrap()
            .id;
        for i in 0..MAX_REFS_PER_ITEM {
            store
                .apply_command(
                    add_ref_cmd(
                        &small,
                        AgendaRefType::Url,
                        &format!("https://example.com/{i}"),
                    ),
                    owner(),
                    2001,
                )
                .unwrap();
        }
        let err = store
            .apply_command(
                add_ref_cmd(&small, AgendaRefType::Url, "https://example.com/over"),
                owner(),
                2002,
            )
            .unwrap_err();
        assert!(err.to_string().contains("more than 32 refs"));
    }

    /// Park-with-refs (the `add` sugar) is all-or-nothing: every spec is
    /// validated before anything appends, and a good park lands the `add`
    /// plus one attributed `add_ref` per spec under one lock.
    #[test]
    fn g1_add_with_refs_is_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let count_before = store.snapshot().len();
        let ops_before = store.ops();
        let bad = AgendaCommand::Add {
            refs: vec![
                super::super::types::AgendaRefSpec {
                    ref_type: AgendaRefType::Url,
                    locator: "https://example.com/ok".into(),
                    must_read: false,
                    label: None,
                },
                super::super::types::AgendaRefSpec {
                    ref_type: AgendaRefType::Url,
                    locator: "gopher://nope".into(),
                    must_read: false,
                    label: None,
                },
            ],
            kind: AgendaKind::Task,
            title: "refused park".into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            source: None,
        };
        assert!(store.apply_command(bad, owner(), 1000).is_err());
        assert_eq!(store.snapshot().len(), count_before, "no item strands");
        assert_eq!(store.ops(), ops_before, "no op strands");

        // Duplicate specs within one park are refused whole.
        let dup = AgendaCommand::Add {
            refs: vec![
                super::super::types::AgendaRefSpec {
                    ref_type: AgendaRefType::Memory,
                    locator: "mem-1".into(),
                    must_read: false,
                    label: None,
                },
                super::super::types::AgendaRefSpec {
                    ref_type: AgendaRefType::Memory,
                    locator: "mem-1".into(),
                    must_read: true,
                    label: None,
                },
            ],
            kind: AgendaKind::Task,
            title: "dup park".into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            source: None,
        };
        let err = store.apply_command(dup, owner(), 1000).unwrap_err();
        assert!(err.to_string().contains("duplicate"));

        let good = AgendaCommand::Add {
            refs: vec![
                super::super::types::AgendaRefSpec {
                    ref_type: AgendaRefType::Url,
                    locator: "https://example.com/pr/9".into(),
                    must_read: true,
                    label: Some("the PR".into()),
                },
                super::super::types::AgendaRefSpec {
                    ref_type: AgendaRefType::Session,
                    locator: "sess-parent".into(),
                    must_read: false,
                    label: None,
                },
            ],
            kind: AgendaKind::Task,
            title: "parked with context".into(),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            source: Some("track-g".into()),
        };
        let item = store.apply_command(good, owner(), 2000).unwrap();
        assert_eq!(item.refs.len(), 2);
        assert!(item.refs[0].must_read);
        assert_eq!(item.refs[0].principal.as_deref(), Some("owner"));
        assert_eq!(item.refs[0].source.as_deref(), Some("track-g"));
        assert_eq!(store.ops(), ops_before + 3, "one add + two add_ref ops");
    }

    /// The expand-time drift judgment: unchanged → changed on edit →
    /// missing on delete; a file grown past the digest bound is `changed`
    /// by size alone.
    #[test]
    fn g1_file_ref_drift_judgments() {
        let files = tempfile::tempdir().unwrap();
        let path = files.path().join("artifact.md");
        std::fs::write(&path, b"v1").unwrap();
        let loc = path.to_string_lossy().into_owned();
        let attach = digest_file(&path).unwrap();

        assert_eq!(file_ref_drift(&loc, &attach), "unchanged");
        std::fs::write(&path, b"v2 drifted").unwrap();
        assert_eq!(file_ref_drift(&loc, &attach), "changed");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_REF_FILE_HASH_BYTES + 1).unwrap();
        drop(f);
        assert_eq!(file_ref_drift(&loc, &attach), "changed");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(file_ref_drift(&loc, &attach), "missing");
    }

    /// The G2 intake omnibus: placement strictness (cycle, self, depth,
    /// double-place, children rail), Place's validate-first override (a
    /// refused target never destroys the live placement), adjacency
    /// either-direction dedup + storing-side removal resolution, and
    /// replay round-trips.
    #[test]
    fn g2_graph_intake_is_strict_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let hub = store
            .apply_command(add_cmd("Track G"), owner(), 1000)
            .unwrap()
            .id;
        let hub2 = store
            .apply_command(add_cmd("Track H"), owner(), 1001)
            .unwrap()
            .id;
        let child = store
            .apply_command(add_cmd("G1 slice"), owner(), 1002)
            .unwrap()
            .id;
        let grand = store
            .apply_command(add_cmd("G1 docs"), owner(), 1003)
            .unwrap()
            .id;

        let place = |id: &str, under: &str| AgendaCommand::Place {
            id: id.to_string(),
            under: under.to_string(),
            source: None,
        };

        // Place + nest; the placement carries attribution.
        let placed = store
            .apply_command(place(&child, &hub), owner(), 1100)
            .unwrap();
        assert_eq!(placed.part_of.as_ref().unwrap().parent_id, hub);
        assert_eq!(
            placed.part_of.as_ref().unwrap().principal.as_deref(),
            Some("owner")
        );
        store
            .apply_command(place(&grand, &child), owner(), 1101)
            .unwrap();

        // Named rejections, nothing appended.
        let ops_before = store.ops();
        for (cmd, needle) in [
            (place(&hub, &hub), "under itself"),
            // hub → child would close hub → child → … → hub.
            (place(&hub, &grand), "placement cycle"),
            (
                AgendaCommand::AddPartOf {
                    id: child.clone(),
                    parent_id: hub2.clone(),
                    source: None,
                },
                "already placed",
            ),
            (
                AgendaCommand::RemovePartOf {
                    id: child.clone(),
                    parent_id: hub2.clone(),
                    source: None,
                },
                "not",
            ),
        ] {
            let err = store.apply_command(cmd, owner(), 1200).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in {err}"
            );
        }
        assert_eq!(store.ops(), ops_before, "rejections never append");

        // The override's motivating case: a refused re-parent target
        // leaves the CURRENT placement intact (validate-first).
        assert!(store
            .apply_command(place(&hub, &grand), owner(), 1201)
            .is_err());
        assert!(
            store.item(&hub).unwrap().part_of.is_none(),
            "hub stays a root"
        );
        assert_eq!(
            store
                .item(&child)
                .unwrap()
                .part_of
                .as_ref()
                .unwrap()
                .parent_id,
            hub,
            "failed gesture must not strand or move the child"
        );

        // Re-parent round trip: remove+add pair under one gesture.
        let moved = store
            .apply_command(place(&child, &hub2), owner(), 1300)
            .unwrap();
        assert_eq!(moved.part_of.as_ref().unwrap().parent_id, hub2);
        assert_eq!(store.ops(), ops_before + 2, "one remove + one add");

        // Adjacency: either-direction dedup, storing-side removal.
        store
            .apply_command(
                AgendaCommand::AddRelatesTo {
                    id: child.clone(),
                    target_id: grand.clone(),
                    source: None,
                },
                owner(),
                1400,
            )
            .unwrap();
        let err = store
            .apply_command(
                AgendaCommand::AddRelatesTo {
                    id: grand.clone(),
                    target_id: child.clone(),
                    source: None,
                },
                owner(),
                1401,
            )
            .unwrap_err();
        assert!(err.to_string().contains("already related"));
        // Removal named in the OPPOSITE order still resolves the stored
        // side; the view edge disappears from the storing item.
        store
            .apply_command(
                AgendaCommand::RemoveRelatesTo {
                    id: grand.clone(),
                    target_id: child.clone(),
                    source: None,
                },
                owner(),
                1402,
            )
            .unwrap();
        assert!(store.item(&child).unwrap().relates_to.is_empty());

        // Depth rail: a chain of exactly MAX_PART_OF_DEPTH nodes is legal;
        // the link that would make it deeper refuses, named.
        let mut chain: Vec<String> = vec![hub2.clone()];
        for i in 0..(MAX_PART_OF_DEPTH - 1) {
            let next = store
                .apply_command(add_cmd(&format!("depth {i}")), owner(), 2000)
                .unwrap()
                .id;
            store
                .apply_command(place(&next, chain.last().unwrap()), owner(), 2001)
                .unwrap();
            chain.push(next);
        }
        let too_deep = store
            .apply_command(add_cmd("too deep"), owner(), 2002)
            .unwrap()
            .id;
        let err = store
            .apply_command(place(&too_deep, chain.last().unwrap()), owner(), 2003)
            .unwrap_err();
        assert!(err.to_string().contains("depth rail"));

        // Replay converges.
        let before = store.item(&child).unwrap();
        let mut reopened = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(reopened.item(&child).unwrap(), before);
    }

    fn recurrence(every_ms: u64) -> super::super::types::RecurrenceSpec {
        super::super::types::RecurrenceSpec {
            every_ms,
            until_ms: None,
            max_occurrences: None,
            suspend_after_failures: None,
        }
    }

    fn propose_recurring(id: &str, every_ms: u64) -> AgendaCommand {
        AgendaCommand::ProposeEffect {
            id: id.to_string(),
            goal: "standing sweep".into(),
            fire_at_ms: 1_000_000,
            orchestrate: false,
            recurrence: Some(recurrence(every_ms)),
            source: None,
        }
    }

    fn owner_kind() -> Option<AgendaActor> {
        Some(AgendaActor {
            principal: Some("owner".into()),
            session_id: None,
            kind: Some("dashboard".into()),
        })
    }

    /// The G3-pre intake omnibus: cadence floor + bound validation, the
    /// failure streak deriving from write-backs, suspension refusing new
    /// requests, re-approval of the UNCHANGED digest as the one-click
    /// re-arm (plain double-approve stays refused), and revocation
    /// clearing pending requests.
    #[test]
    fn g3pre_recurrence_intake_streak_and_rearm() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store
            .apply_command(add_cmd("standing"), owner(), 1000)
            .unwrap()
            .id;

        // Cadence floor + bound sanity, named.
        for (cmd, needle) in [
            (propose_recurring(&id, 60_000), "floors at 15 minutes"),
            (
                AgendaCommand::ProposeEffect {
                    id: id.clone(),
                    goal: "g".into(),
                    fire_at_ms: 1_000_000,
                    orchestrate: false,
                    recurrence: Some(super::super::types::RecurrenceSpec {
                        until_ms: Some(999_999),
                        ..recurrence(3_600_000)
                    }),
                    source: None,
                },
                "until_ms must be after",
            ),
            (
                AgendaCommand::ProposeEffect {
                    id: id.clone(),
                    goal: "g".into(),
                    fire_at_ms: 1_000_000,
                    orchestrate: false,
                    recurrence: Some(super::super::types::RecurrenceSpec {
                        max_occurrences: Some(0),
                        ..recurrence(3_600_000)
                    }),
                    source: None,
                },
                "at least 1",
            ),
        ] {
            let err = store.apply_command(cmd, owner(), 1001).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in {err}"
            );
        }

        // Propose + approve a standing hourly manifest.
        let proposed = store
            .apply_command(propose_recurring(&id, 3_600_000), owner(), 1002)
            .unwrap();
        let digest = proposed.effects[0].digest.clone();
        let effect_id = proposed.effects[0].effect_id.clone();
        store
            .apply_command(
                AgendaCommand::ApproveEffect {
                    id: id.clone(),
                    digest: digest.clone(),
                },
                owner_kind(),
                1003,
            )
            .unwrap();

        // A request before any run: accepted once, second refused pending.
        store
            .apply_command(
                AgendaCommand::RequestOccurrence { id: id.clone() },
                owner_kind(),
                1004,
            )
            .unwrap();
        let err = store
            .apply_command(
                AgendaCommand::RequestOccurrence { id: id.clone() },
                owner_kind(),
                1005,
            )
            .unwrap_err();
        assert!(err.to_string().contains("pending"));

        // Streak: failed/unknown accrue, started/missed neutral, the
        // write-back after the request also unblocks the next request.
        let fail = |store: &mut AgendaStore, occ: &str, state: &str, at: u64| {
            store
                .record_occurrence(
                    OccurrenceWriteBack {
                        item_id: &id,
                        effect_id: &effect_id,
                        occurrence_id: occ,
                        state,
                        session_id: None,
                        note: None,
                    },
                    at,
                )
                .unwrap()
        };
        fail(&mut store, "occ-1", "started", 2001);
        fail(&mut store, "occ-1", "failed", 2002);
        fail(&mut store, "occ-2", "missed", 2003);
        let item = fail(&mut store, "occ-3", "unknown", 2004);
        assert_eq!(item.effects[0].consecutive_failures, 2);
        assert!(!item.effects[0].suspended(), "threshold defaults to 3");
        let item = fail(&mut store, "occ-4", "failed", 2005);
        assert_eq!(item.effects[0].consecutive_failures, 3);
        assert!(item.effects[0].suspended());

        // Suspended: new requests refused, named.
        let err = store
            .apply_command(
                AgendaCommand::RequestOccurrence { id: id.clone() },
                owner_kind(),
                2006,
            )
            .unwrap_err();
        assert!(err.to_string().contains("suspended"));

        // Re-approving the UNCHANGED digest is the re-arm: allowed exactly
        // in the suspended state, streak resets to zero.
        let item = store
            .apply_command(
                AgendaCommand::ApproveEffect {
                    id: id.clone(),
                    digest: digest.clone(),
                },
                owner_kind(),
                2007,
            )
            .unwrap();
        assert_eq!(item.effects[0].consecutive_failures, 0);
        assert!(item.effects[0].approval.is_some());
        // Plain double-approve (not suspended) stays refused.
        let err = store
            .apply_command(
                AgendaCommand::ApproveEffect {
                    id: id.clone(),
                    digest: digest.clone(),
                },
                owner_kind(),
                2008,
            )
            .unwrap_err();
        assert!(err.to_string().contains("already approved"));

        // A completed run resets the streak from any depth.
        fail(&mut store, "occ-5", "failed", 2009);
        let item = fail(&mut store, "occ-6", "completed", 2010);
        assert_eq!(item.effects[0].consecutive_failures, 0);

        // Revocation is instant and clears pending requests.
        store
            .apply_command(
                AgendaCommand::RequestOccurrence { id: id.clone() },
                owner_kind(),
                2011,
            )
            .unwrap();
        let item = store
            .apply_command(
                AgendaCommand::RevokeEffect { id: id.clone() },
                owner_kind(),
                2012,
            )
            .unwrap();
        assert!(item.effects[0].approval.is_none());
        assert!(item.effects[0].requested.is_empty());
        let err = store
            .apply_command(
                AgendaCommand::RequestOccurrence { id: id.clone() },
                owner_kind(),
                2013,
            )
            .unwrap_err();
        assert!(err.to_string().contains("not approved"));

        // Replay converges (request/streak state included).
        let before = store.item(&id).unwrap();
        let mut reopened = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(reopened.item(&id).unwrap(), before);
    }

    /// Start-now beside a standing approval fires the approved digest
    /// (request_occurrence) instead of revising it; explicit overrides
    /// are a named refusal pointing at the honest revise path; a one-shot
    /// item keeps the classic mint+approve pair (regression).
    #[test]
    fn g3pre_start_now_routes_standing_manifests_to_requests() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store
            .apply_command(add_cmd("standing"), owner(), 1000)
            .unwrap()
            .id;
        let proposed = store
            .apply_command(propose_recurring(&id, 3_600_000), owner(), 1001)
            .unwrap();
        let digest = proposed.effects[0].digest.clone();
        store
            .apply_command(
                AgendaCommand::ApproveEffect {
                    id: id.clone(),
                    digest: digest.clone(),
                },
                owner_kind(),
                1002,
            )
            .unwrap();

        // Bare start_now (mode absent): one request op, approval intact,
        // digest unchanged — the standing manifest is never revised.
        let ops_before = store.ops();
        let item = store
            .apply_command(
                AgendaCommand::StartNow {
                    id: id.clone(),
                    goal: None,
                    project_root: None,
                    interactive: None,
                    agent_config: None,
                },
                owner_kind(),
                1003,
            )
            .unwrap();
        assert_eq!(
            store.ops(),
            ops_before + 1,
            "one request op, no propose/approve"
        );
        assert_eq!(item.effects[0].digest, digest);
        assert!(item.effects[0].approval.is_some());
        assert_eq!(item.effects[0].requested.len(), 1);
        assert_eq!(item.effects[0].requested[0].at_ms, 1003);

        // An explicit override is a named refusal: edits go through the
        // revise ceremony, never silently voiding the standing approval.
        let err = store
            .apply_command(
                AgendaCommand::StartNow {
                    id: id.clone(),
                    goal: Some("different bytes".into()),
                    project_root: None,
                    interactive: None,
                    agent_config: None,
                },
                owner_kind(),
                1004,
            )
            .unwrap_err();
        assert!(err.to_string().contains("standing approved manifest"));

        // One-shot regression: no recurrence → the classic two-op pair.
        let plain = store
            .apply_command(add_cmd("one shot"), owner(), 2000)
            .unwrap()
            .id;
        let ops_before = store.ops();
        let item = store
            .apply_command(
                AgendaCommand::StartNow {
                    id: plain.clone(),
                    goal: None,
                    project_root: Some(dir.path().to_string_lossy().into_owned()),
                    interactive: None,
                    agent_config: None,
                },
                owner_kind(),
                2001,
            )
            .unwrap();
        assert_eq!(store.ops(), ops_before + 2, "propose + approve, as ever");
        assert!(item.effects[0].approval.is_some());
        assert!(item.effects[0].manifest.recurrence.is_none());
    }

    /// The steward's cross-build rider, pinned: a recurrence-bearing
    /// manifest's digest DIFFERS from the recurrence-stripped
    /// re-serialization an older build would derive — the premise that
    /// makes old daemons fail closed (approval mismatch → never fires) —
    /// while a recurrence-less manifest is byte-identical to the legacy
    /// shape, so every legacy digest and approval is unchanged.
    #[test]
    fn g3pre_cross_build_digest_degrades_fail_closed() {
        let with = super::super::types::SessionManifest {
            goal: "standing".into(),
            fire_at_ms: 1_000_000,
            orchestrate: false,
            interactive: false,
            project_root: None,
            agent_config: None,
            recurrence: Some(recurrence(3_600_000)),
        };
        let json = serde_json::to_value(&with).unwrap();
        assert!(
            json.get("recurrence").is_some(),
            "the field reaches the wire"
        );
        // An older build deserializes-without then re-serializes-without:
        let mut stripped_json = json.clone();
        stripped_json.as_object_mut().unwrap().remove("recurrence");
        let stripped: super::super::types::SessionManifest =
            serde_json::from_value(stripped_json).unwrap();
        assert_ne!(
            super::super::types::manifest_digest("i", "e", &with),
            super::super::types::manifest_digest("i", "e", &stripped),
            "recurrence must be digest-visible or old builds would fire it as a one-shot"
        );
        // And the recurrence-less shape is the legacy bytes exactly.
        let legacy = super::super::types::SessionManifest {
            recurrence: None,
            ..stripped
        };
        assert!(!serde_json::to_string(&legacy)
            .unwrap()
            .contains("recurrence"));
    }

    /// A future ref type inside a known op name degrades exactly like an
    /// unknown op: the line is preserved on disk and skipped at load.
    #[test]
    fn g1_unknown_ref_type_lines_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let lines = concat!(
            "{\"v\":1,\"at_ms\":1,\"op\":{\"type\":\"add\",\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"kind\":\"task\",\"title\":\"host\"}}\n",
            "{\"v\":1,\"at_ms\":2,\"op\":{\"type\":\"add_ref\",\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"ref_type\":\"sigil\",\"locator\":\"x\"}}\n",
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join(LOG_FILE), lines).unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        assert_eq!(store.skipped_lines(), 1);
        let item = store.item("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert!(item.refs.is_empty());
        // The foreign line survives on disk verbatim.
        let text = std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap();
        assert!(text.contains("sigil"));
    }

    /// Seed the read_ops fixture through the real command path (two
    /// parks, one annotate, one complete — lines 0..=3), then append a
    /// newer build's op referencing item A (4), an item-less foreign op
    /// (5), and a non-JSON line (6) directly, as another binary or a
    /// hand edit would. Returns `(store, item_a_id, item_b_id)`.
    fn seeded_ops_store(dir: &Path) -> (AgendaStore, String, String) {
        let mut store = AgendaStore::open(dir).unwrap();
        let a = store
            .apply_command(add_cmd("first"), owner(), 1000)
            .unwrap();
        let b = store.apply_command(add_cmd("second"), None, 2000).unwrap();
        store
            .apply_command(
                AgendaCommand::Annotate {
                    id: a.id.clone(),
                    text: "progress note".into(),
                    source: None,
                },
                None,
                3000,
            )
            .unwrap();
        store
            .apply_command(
                AgendaCommand::Complete {
                    id: b.id.clone(),
                    source: None,
                },
                None,
                4000,
            )
            .unwrap();
        let foreign = format!(
            "{{\"v\":1,\"at_ms\":5000,\"op\":{{\"type\":\"journal_curate\",\"id\":\"{}\",\"note\":\"from a newer build\"}}}}\n\
             {{\"v\":1,\"at_ms\":6000,\"op\":{{\"type\":\"compact_marker\"}}}}\n\
             this line is not JSON at all\n",
            a.id
        );
        let mut log = std::fs::File::options()
            .append(true)
            .open(store.log_path())
            .unwrap();
        log.write_all(foreign.as_bytes()).unwrap();
        (store, a.id, b.id)
    }

    /// `GET /api/agenda/ops` mechanics (a, c, d): the full page serves
    /// every line — known ops as full envelopes, unknown vocabulary
    /// verbatim with `known:false`, non-JSON as `unparseable` — and the
    /// since/limit window keeps `next_since`/`log_len` exact.
    #[test]
    fn read_ops_serves_every_line_and_windows_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, a, _b) = seeded_ops_store(dir.path());

        let page = store.read_ops(0, None, 500).unwrap();
        assert_eq!(page.log_len, 7);
        assert_eq!(page.next_since, 7);
        assert!(!page.filtered);
        assert_eq!(page.ops.len(), 7);
        for (index, entry) in page.ops.iter().enumerate() {
            assert_eq!(entry["seq"].as_u64(), Some(index as u64));
        }
        // The four command-path lines are full envelopes this build
        // folds: they round-trip through the typed record, so nothing
        // partial was served.
        for entry in &page.ops[..4] {
            assert_eq!(entry["known"], serde_json::Value::Bool(true));
            let record: AgendaOpRecord = serde_json::from_value(entry["op"].clone()).unwrap();
            assert!(record.at_ms > 0);
        }
        assert_eq!(page.ops[0]["op"]["op"]["type"], "add");
        assert_eq!(page.ops[2]["op"]["op"]["type"], "annotate");
        assert_eq!(page.ops[3]["op"]["op"]["type"], "complete");
        // (c) Unknown vocabulary: served VERBATIM, marked unknown.
        assert_eq!(page.ops[4]["known"], serde_json::Value::Bool(false));
        assert_eq!(page.ops[4]["op"]["op"]["type"], "journal_curate");
        assert_eq!(page.ops[4]["op"]["op"]["id"], a.as_str());
        assert_eq!(page.ops[4]["op"]["op"]["note"], "from a newer build");
        assert_eq!(page.ops[5]["known"], serde_json::Value::Bool(false));
        assert_eq!(page.ops[5]["op"]["op"]["type"], "compact_marker");
        // (d) Non-JSON: unparseable, raw text preserved string-escaped.
        assert_eq!(page.ops[6]["known"], serde_json::Value::Bool(false));
        assert_eq!(page.ops[6]["unparseable"], serde_json::Value::Bool(true));
        assert_eq!(page.ops[6]["raw"], "this line is not JSON at all");

        // (a) Window math: a mid-log page fills and resumes exactly.
        let page = store.read_ops(2, None, 3).unwrap();
        let seqs: Vec<u64> = page
            .ops
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![2, 3, 4]);
        assert_eq!(page.next_since, 5);
        assert_eq!(page.log_len, 7);
        let page = store.read_ops(page.next_since, None, 500).unwrap();
        let seqs: Vec<u64> = page
            .ops
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![5, 6]);
        assert_eq!(page.next_since, 7);
        // A cursor at (or past) the tail returns an empty page that
        // keeps pointing at the tail.
        let page = store.read_ops(7, None, 500).unwrap();
        assert!(page.ops.is_empty());
        assert_eq!(page.next_since, 7);
        let page = store.read_ops(100, None, 500).unwrap();
        assert!(page.ops.is_empty());
        assert_eq!(page.next_since, 7);
        // The limit clamp floor: 0 is not "unbounded" and not "nothing".
        let page = store.read_ops(0, None, 0).unwrap();
        assert_eq!(page.ops.len(), 1);
        assert_eq!(page.next_since, 1);
    }

    /// (b) The `item` filter serves exactly the lines whose `op.id` is
    /// the requested item — unknown vocabulary included — and excludes
    /// lines carrying no item reference (foreign item-less ops,
    /// unparseable lines). The cursor stays a LINE cursor under the
    /// filter, so a truncated filtered page resumes without re-serving.
    #[test]
    fn read_ops_item_filter_includes_only_that_items_ops() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, a, b) = seeded_ops_store(dir.path());

        let page = store.read_ops(0, Some(&a), 500).unwrap();
        assert!(page.filtered);
        assert_eq!(page.log_len, 7);
        assert_eq!(page.next_since, 7);
        let seqs: Vec<u64> = page
            .ops
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        // A's add (0), A's annotate (2), the newer build's op on A (4);
        // never B's lines, the item-less op (5), or the non-JSON line (6).
        assert_eq!(seqs, vec![0, 2, 4]);
        for entry in &page.ops {
            assert_eq!(entry["op"]["op"]["id"], a.as_str());
        }
        assert_eq!(page.ops[2]["known"], serde_json::Value::Bool(false));

        let page = store.read_ops(0, Some(&b), 500).unwrap();
        let seqs: Vec<u64> = page
            .ops
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![1, 3]);

        // Truncated filtered page: next_since is the line after the last
        // served line, and resuming there serves the rest exactly once.
        let page = store.read_ops(0, Some(&a), 2).unwrap();
        let seqs: Vec<u64> = page
            .ops
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![0, 2]);
        assert_eq!(page.next_since, 3);
        let page = store.read_ops(page.next_since, Some(&a), 500).unwrap();
        let seqs: Vec<u64> = page
            .ops
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![4]);
        assert_eq!(page.next_since, 7);

        // An id nothing references filters to an empty (but honest) page.
        let page = store
            .read_ops(0, Some("01NOSUCHITEMEVERPARKED0000"), 500)
            .unwrap();
        assert!(page.ops.is_empty());
        assert!(page.filtered);
        assert_eq!(page.next_since, 7);
    }

    /// (e) Reads interleaved with appends never serve a torn line: every
    /// page taken between appends holds only complete envelopes, and the
    /// tail advances by exactly one line per op. (The lock discipline
    /// this relies on — reads and appends under one store mutex — is
    /// exercised cross-thread in `handle.rs`.)
    #[test]
    fn read_ops_between_appends_never_serves_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgendaStore::open(dir.path()).unwrap();
        let id = store.apply_command(add_cmd("host"), None, 1).unwrap().id;
        let mut last_len = store.read_ops(0, None, 500).unwrap().log_len;
        assert_eq!(last_len, 1);
        for round in 0..20u64 {
            store
                .apply_command(
                    AgendaCommand::Annotate {
                        id: id.clone(),
                        text: format!("note {round}"),
                        source: None,
                    },
                    None,
                    2 + round,
                )
                .unwrap();
            let page = store.read_ops(0, None, 2000).unwrap();
            assert_eq!(page.log_len, last_len + 1);
            assert_eq!(page.next_since, page.log_len);
            assert_eq!(page.ops.len(), page.log_len as usize);
            for entry in &page.ops {
                // Complete lines only: every one parses as the typed
                // record — a torn/partial line could not.
                assert_eq!(entry["known"], serde_json::Value::Bool(true));
                let record: AgendaOpRecord = serde_json::from_value(entry["op"].clone()).unwrap();
                assert_eq!(record.op.item_id(), id);
            }
            last_len = page.log_len;
        }
    }
}
