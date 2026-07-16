//! The transfer-journal replay machine (§6.1/§6.2, D-140/D-192/
//! D-193/D-198/D-200) — the `journal-replay` harness lane.
//!
//! A delivered item is either a SignedOperation triple (a map with a
//! `header` key — folded through the §10.2 engine) or a zone-log
//! `txn` frame body (a map with a `records` key — replayed here).
//! `aux` entries are HELD facts: operations resolvable by op hash,
//! signed statements by `stmt_id` — context for `opfactref`/`factref`
//! holding checks, never folded.
//!
//! Txn records validate SEQUENTIALLY against transaction-local
//! journal state and commit all-or-nothing (D-200: journal order is
//! `(frame ordinal, record index)`): an invalid shape or transition
//! classifies the WHOLE frame `(log-corrupt, storage-quarantine)` and
//! discards every record in it. A shape-valid record whose cited fact
//! is unheld PENDS `(ref-unresolved, pending-dependency)`, reserving
//! the interval (verifiable-when-held, D-163/D-185).
//!
//! HELD citations are verified, never taken on faith: an aux op must
//! carry a sound `body_hash` binding to register as held; a
//! `source-erased` abort is a basis-FORBIDDEN cause class (D-193) so
//! any basis row in one is malformed; and a reopen's held
//! invalidation must actually KILL its basis — the one
//! wire-expressible killer of a control-chain fact is a
//! `c.recovery_succession` whose branch cut covers it (`base.seq <`
//! the basis's chain position, §7.4/D-163). Statement killers
//! (fork-discovery) have no §4.7 wire shape — a held stmt-kind
//! invalidation is honestly Unimplemented until one exists (an audit
//! finding); full cause SUFFICIENCY (this fact makes THAT record
//! resolved-negative) needs source dereferencing and stays fold
//! territory — the D-193 request-fork cause admits any op kind, so
//! no closed kind check exists for abort bases.

use std::collections::BTreeMap;

use crate::cbor::{decode, Node};
use crate::domains;
use crate::envelope::parse_op;
use crate::fold::{classify, State, Unimplemented, Verdict};

/// One journal interval's terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Terminal {
    /// The abort's recorded causes (`missing[].basis` op refs) — the
    /// reopen's basis must match one (D-193).
    Abort {
        bases: Vec<[u8; 32]>,
    },
    Done,
}

#[derive(Debug, Clone)]
struct Journal {
    export_id: [u8; 16],
    record_count: u64,
    /// Index = incarnation; `None` = open.
    intervals: Vec<Option<Terminal>>,
}

/// Replayed zone-log state.
#[derive(Debug, Clone, Default)]
pub struct JournalState {
    /// (lineage, gen, seq) → item_addr, from replayed ItemCommits.
    commits: BTreeMap<([u8; 16], u64, u64), [u8; 32]>,
    /// release_op → journal (transfer identity is `release_op`,
    /// D-123).
    journals: BTreeMap<[u8; 32], Journal>,
    /// Held facts for reference resolution: aux operations by op hash
    /// (bytes retained — held citations are VERIFIED against them)
    /// and aux statement ids.
    held_ops: BTreeMap<[u8; 32], Vec<u8>>,
    held_stmts: Vec<[u8; 32]>,
}

impl JournalState {
    /// Register one `aux` entry: an operation triple or a signed
    /// statement (`{stmt, sig}` — `stmt_id = H_stmtid(bytes)`). An op
    /// whose carried `body_hash` does not bind its body never
    /// registers as held (O1 — a broken triple resolves nothing).
    fn hold_aux(&mut self, bytes: &[u8]) -> Result<(), Unimplemented> {
        if let Ok(op) = parse_op(bytes) {
            if !op.body_hash_ok() {
                return Err(Unimplemented(
                    "aux op with a broken body_hash binding".into(),
                ));
            }
            self.held_ops
                .insert(domains::h("op", bytes), bytes.to_vec());
            return Ok(());
        }
        let node = decode(bytes).map_err(|e| Unimplemented(format!("aux decode: {e:?}")))?;
        if keys_are(&node, &["stmt", "sig"]) {
            self.held_stmts.push(domains::h("stmtid", bytes));
            return Ok(());
        }
        Err(Unimplemented("aux entry of unknown shape".into()))
    }

    fn op_held(&self, h: &[u8; 32], fold: &State) -> bool {
        self.held_ops.contains_key(h) || fold.holds_op(h)
    }

    /// Replay one txn frame. `Err(inner Verdict)` semantics ride the
    /// outer `Verdict` — this returns the frame's classification.
    fn replay_txn(
        &mut self,
        node: &Node,
        fold: &State,
        op_verdicts: &BTreeMap<[u8; 32], Verdict>,
    ) -> Result<Verdict, Unimplemented> {
        let corrupt = Verdict::Rejected("log-corrupt", "storage-quarantine");
        if !keys_are(node, &["records"]) {
            return Ok(corrupt);
        }
        let Some(records) = node.get("records").and_then(|r| r.as_array()) else {
            return Ok(corrupt);
        };
        if records.is_empty() || records.len() > 16 {
            return Ok(corrupt);
        }
        // Transaction-local state: all-or-nothing.
        let mut local = self.clone();
        for rec in records {
            let keys = rec.map_keys().unwrap_or_default();
            let step = if keys.contains(&"core") {
                local.apply_item_commit(rec)
            } else if keys.contains(&"record_count") {
                local.apply_pending_xfer(rec)
            } else if keys.contains(&"completed") {
                local.apply_done(rec)
            } else if keys.contains(&"missing") {
                local.apply_abort(rec, fold)?
            } else if keys.contains(&"basis") {
                local.apply_reopen(rec, fold, op_verdicts)?
            } else {
                return Ok(corrupt);
            };
            match step {
                Ok(()) => {}
                Err(v @ Verdict::Pending(..)) => {
                    // Sequential validation (D-200) halts at the
                    // first unresolved record: the WHOLE frame pends
                    // — all-or-nothing extends to pendency, nothing
                    // commits, and the reservation holds the frame
                    // until the cited fact lands (D-185).
                    return Ok(v);
                }
                Err(_) => return Ok(corrupt),
            }
        }
        *self = local;
        Ok(Verdict::Admitted)
    }

    /// `itemcommit` — structural: `item_addr = H_item(canonical
    /// itemcore bytes)`; the coordinate is recorded for the erase
    /// machinery (the ciphertext itself is opaque to replay).
    fn apply_item_commit(&mut self, rec: &Node) -> Result<(), Verdict> {
        let bad = Err(Verdict::Rejected("log-corrupt", "storage-quarantine"));
        if !keys_are(rec, &["core", "wrap", "lineage", "gen", "seq"]) {
            return bad;
        }
        let (Some(core), Some(wrap)) = (rec.get("core"), rec.get("wrap")) else {
            return bad;
        };
        if !keys_are(core, &["v", "aead", "nonce", "ct"])
            || core.get("v").and_then(|n| n.as_uint()) != Some(1)
            || core.get("aead").and_then(|n| n.as_text()) != Some("a256gcm")
            || core.get("nonce").and_then(|n| n.bytes_n::<12>()).is_none()
            || core.get("ct").and_then(|n| n.as_bytes()).is_none()
        {
            return bad;
        }
        if !keys_are(wrap, &["v", "item_addr", "key_wrap_epoch", "wrapped_dek"])
            || wrap.get("v").and_then(|n| n.as_uint()) != Some(1)
            || wrap
                .get("key_wrap_epoch")
                .and_then(|n| n.as_uint())
                .is_none()
            || wrap
                .get("wrapped_dek")
                .and_then(|n| n.bytes_n::<48>())
                .is_none()
        {
            return bad;
        }
        let Some(addr) = wrap.get("item_addr").and_then(|n| n.bytes_n::<32>()) else {
            return bad;
        };
        if addr != domains::h("item", core.raw) {
            return bad;
        }
        let (Some(lineage), Some(gen), Some(seq)) = (
            rec.get("lineage").and_then(|n| n.bytes_n::<16>()),
            rec.get("gen").and_then(|n| n.as_uint()),
            rec.get("seq").and_then(|n| n.as_uint()),
        ) else {
            return bad;
        };
        match self.commits.get(&(lineage, gen, seq)) {
            Some(a) if *a != addr => bad,
            _ => {
                self.commits.insert((lineage, gen, seq), addr);
                Ok(())
            }
        }
    }

    /// `pendingxfer` — opens the journal at incarnation 0. Its ids
    /// are opaque to replay (the release triple is sealed inside the
    /// item; never dereferenced here).
    fn apply_pending_xfer(&mut self, rec: &Node) -> Result<(), Verdict> {
        let bad = Err(Verdict::Rejected("log-corrupt", "storage-quarantine"));
        if !keys_are(
            rec,
            &[
                "export_id",
                "release_op",
                "dest_zone",
                "content_digest",
                "record_count",
            ],
        ) {
            return bad;
        }
        let (Some(export_id), Some(release_op), Some(count)) = (
            rec.get("export_id").and_then(|n| n.bytes_n::<16>()),
            rec.get("release_op").and_then(|n| n.bytes_n::<32>()),
            rec.get("record_count").and_then(|n| n.as_uint()),
        ) else {
            return bad;
        };
        if rec
            .get("dest_zone")
            .and_then(|n| n.bytes_n::<16>())
            .is_none()
            || rec
                .get("content_digest")
                .and_then(|n| n.bytes_n::<32>())
                .is_none()
            || count == 0
            || self.journals.contains_key(&release_op)
        {
            return bad;
        }
        self.journals.insert(
            release_op,
            Journal {
                export_id,
                record_count: count,
                intervals: vec![None],
            },
        );
        Ok(())
    }

    /// Resolve a terminal/reopen's journal + open-interval check.
    fn open_interval<'j>(
        journals: &'j mut BTreeMap<[u8; 32], Journal>,
        rec: &Node,
    ) -> Result<(&'j mut Journal, u64), Verdict> {
        let bad = Verdict::Rejected("log-corrupt", "storage-quarantine");
        let (Some(export_id), Some(release_op), Some(inc)) = (
            rec.get("export_id").and_then(|n| n.bytes_n::<16>()),
            rec.get("release_op").and_then(|n| n.bytes_n::<32>()),
            rec.get("incarnation").and_then(|n| n.as_uint()),
        ) else {
            return Err(bad);
        };
        let Some(j) = journals.get_mut(&release_op) else {
            // The journal's opener has not arrived: a missing
            // dependency reserves, never corrupts (D-185).
            return Err(Verdict::Pending("ref-unresolved", "pending-dependency"));
        };
        if j.export_id != export_id {
            return Err(bad);
        }
        Ok((j, inc))
    }

    /// `xferdone` — terminals the current OPEN interval; `completed`
    /// is the bundle's exact record set (`len == record_count`).
    fn apply_done(&mut self, rec: &Node) -> Result<(), Verdict> {
        let bad = Err(Verdict::Rejected("log-corrupt", "storage-quarantine"));
        if !keys_are(
            rec,
            &["export_id", "release_op", "incarnation", "completed"],
        ) {
            return bad;
        }
        let (j, inc) = Self::open_interval(&mut self.journals, rec)?;
        let Some(completed) = rec.get("completed").and_then(|c| c.as_array()) else {
            return bad;
        };
        if completed.len() as u64 != j.record_count
            || completed.iter().any(|c| c.bytes_n::<32>().is_none())
        {
            return bad;
        }
        if inc + 1 > j.intervals.len() as u64 {
            // The cited incarnation has not opened yet (its reopen
            // is in flight): reserve.
            return Err(Verdict::Pending("ref-unresolved", "pending-dependency"));
        }
        if inc + 1 < j.intervals.len() as u64 || j.intervals[inc as usize].is_some() {
            return bad;
        }
        j.intervals[inc as usize] = Some(Terminal::Done);
        Ok(())
    }

    /// `xferabort` — terminals the current OPEN interval; `missing`
    /// is non-empty, each optional `basis` an op-kind fact that must
    /// be HELD.
    fn apply_abort(
        &mut self,
        rec: &Node,
        fold: &State,
    ) -> Result<Result<(), Verdict>, Unimplemented> {
        let bad = Ok(Err(Verdict::Rejected("log-corrupt", "storage-quarantine")));
        if !keys_are(
            rec,
            &[
                "export_id",
                "release_op",
                "reason",
                "incarnation",
                "missing",
            ],
        ) {
            return bad;
        }
        let reason = rec.get("reason").and_then(|r| r.as_text());
        if !matches!(
            reason,
            Some("source-erased" | "reject-permanent" | "release-rejected")
        ) {
            return bad;
        }
        let Some(missing) = rec.get("missing").and_then(|m| m.as_array()) else {
            return bad;
        };
        if missing.is_empty() {
            return bad;
        }
        let mut bases = Vec::new();
        for m in missing {
            if !keys_are(m, &["rec"]) && !keys_are(m, &["rec", "basis"]) {
                return bad;
            }
            if m.get("rec").and_then(|n| n.bytes_n::<32>()).is_none() {
                return bad;
            }
            if let Some(b) = m.get("basis") {
                // Basis sufficiency, the checkable slice: the D-193
                // cause table forbids a basis on `source-erased`
                // rows (an erased source is basis-free by kind); the
                // op-kind universe stays OPEN for the other reasons
                // (request-fork's conflicting operation may be any
                // op), so kind is not narrowed here — full cause
                // sufficiency needs the sources and is fold
                // territory.
                if reason == Some("source-erased") {
                    return bad;
                }
                let Some(op) = opfactref(b) else {
                    return bad;
                };
                if !self.op_held(&op, fold) {
                    return Ok(Err(Verdict::Pending(
                        "ref-unresolved",
                        "pending-dependency",
                    )));
                }
                bases.push(op);
            }
        }
        let (j, inc) = match Self::open_interval(&mut self.journals, rec) {
            Ok(v) => v,
            Err(v) => return Ok(Err(v)),
        };
        if inc + 1 > j.intervals.len() as u64 {
            return Ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        if inc + 1 < j.intervals.len() as u64 || j.intervals[inc as usize].is_some() {
            return bad;
        }
        j.intervals[inc as usize] = Some(Terminal::Abort { bases });
        Ok(Ok(()))
    }

    /// `xferreopen` — closes a TERMINAL interval and opens the next
    /// incarnation. The basis is op-kind ONLY (D-193/D-200) and must
    /// match a recorded cause; basis and invalidation must be held
    /// (an unheld citation pends, reserving the interval).
    fn apply_reopen(
        &mut self,
        rec: &Node,
        fold: &State,
        op_verdicts: &BTreeMap<[u8; 32], Verdict>,
    ) -> Result<Result<(), Verdict>, Unimplemented> {
        let bad = Ok(Err(Verdict::Rejected("log-corrupt", "storage-quarantine")));
        if !keys_are(
            rec,
            &[
                "export_id",
                "release_op",
                "incarnation",
                "basis",
                "invalidation",
            ],
        ) {
            return bad;
        }
        let Some(basis) = rec.get("basis").and_then(opfactref_node) else {
            // A stmt-kind basis is structurally outside `opfactref` —
            // parse-invalid, classified immediately.
            return bad;
        };
        let invalidation = match rec.get("invalidation") {
            Some(f) => match factref(f) {
                Some(f) => f,
                None => return bad,
            },
            None => return bad,
        };
        // Holding checks FIRST (D-185: citation resolution precedes
        // transition legality — an unheld citation pends, reserving).
        if !self.op_held(&basis, fold) {
            return Ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        let held = match &invalidation {
            Fact::Op(h) => self.op_held(h, fold),
            Fact::Stmt(id) => self.held_stmts.contains(id),
        };
        if !held {
            return Ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        // Transition legality against the (transaction-local) state.
        let (j, inc) = match Self::open_interval(&mut self.journals, rec) {
            Ok(v) => v,
            Err(v) => return Ok(Err(v)),
        };
        if inc + 1 > j.intervals.len() as u64 {
            return Ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        if inc + 1 < j.intervals.len() as u64 {
            return bad;
        }
        // Basis-match: the invalidated recorded cause. An interval
        // still awaiting its terminal reserves; reopen-after-Done
        // and a basis outside the recorded causes are held-invalid.
        match &j.intervals[inc as usize] {
            Some(Terminal::Abort { bases }) => {
                if !bases.contains(&basis) {
                    return bad;
                }
            }
            Some(Terminal::Done) => return bad,
            None => {
                return Ok(Err(Verdict::Pending(
                    "ref-unresolved",
                    "pending-dependency",
                )))
            }
        }
        // Kill verification (D-163/D-179, hardened per the 2026-07-15
        // review's R3): a citation is verified against AUTHORITY,
        // never taken on faith — the pre-repair check re-parsed held
        // bytes and predicted the kill from their SHAPE, so a
        // signature-invalid recovery verified a reopen (the review's
        // reproduced trace). Now the invalidation must be an
        // AUTHENTICATED, ADMITTED control fact and the kill must
        // have actually HAPPENED:
        //
        // - the cited op is ACCEPTED on the fold's control chain
        //   (admission is the plane's authentication — a forged
        //   recovery never admits), AND the basis is DEAD there (cut
        //   by the recovery's §7.4 branch cut) → the kill verifies;
        // - the cited op is accepted but the basis still stands →
        //   verified-false, `log-corrupt` (D-163);
        // - the cited op is REJECTED by the engine or lies on a cut
        //   branch (the fold's §7.4 overlay) → it can never be
        //   authority: an unverifiable citation, `log-corrupt`;
        // - otherwise (held bytes not yet admitted, or nothing held)
        //   → pending, verifiable-when-ADMITTED (D-163/D-185's
        //   reservation extended from held to authoritative).
        //
        // The retained shape checks (recovery kind, same writer
        // chain, base strictly below the basis) bind the CITED kill
        // to the cut the fold performed. Statement killers
        // (fork-discovery) have no §4.7 wire shape and stay honestly
        // Unimplemented.
        match &invalidation {
            Fact::Op(h) => {
                if let Some(v) = op_verdicts.get(h) {
                    if matches!(v, Verdict::Rejected(..)) {
                        // Delivered and rejected (a forged signature,
                        // a malformed body): the citation can never
                        // become authority.
                        return bad;
                    }
                }
                if let Some(Verdict::Rejected(..)) = fold.ctrl_overlaid(h) {
                    // Admitted once, then cut or frozen off the
                    // chain: dead authority.
                    return bad;
                }
                if !fold.ctrl_accepted(h) {
                    // Held bytes are not authority until the engine
                    // admits them (the review's unadmitted arm).
                    return Ok(Err(Verdict::Pending(
                        "ref-unresolved",
                        "pending-dependency",
                    )));
                }
                let Some(inv_bytes) = self.held_ops.get(h) else {
                    return Err(Unimplemented(
                        "reopen kill verification needs the invalidation bytes held".into(),
                    ));
                };
                let inv = parse_op(inv_bytes)
                    .map_err(|e| Unimplemented(format!("held invalidation re-parse: {e:?}")))?;
                if inv.header.operation_type != "c.recovery_succession" {
                    return Err(Unimplemented(format!(
                        "reopen kill verification for {:?} invalidations awaits vectors",
                        inv.header.operation_type
                    )));
                }
                let Some(basis_bytes) = self.held_ops.get(&basis) else {
                    return Err(Unimplemented(
                        "reopen kill verification needs the basis bytes held".into(),
                    ));
                };
                let basis_op = parse_op(basis_bytes)
                    .map_err(|e| Unimplemented(format!("held basis re-parse: {e:?}")))?;
                if inv.header.writer_lineage != basis_op.header.writer_lineage {
                    return Err(Unimplemented(
                        "recovery-cut kill for a basis outside the recovery's chain awaits \
                         vectors"
                            .into(),
                    ));
                }
                let base_seq = inv
                    .body
                    .get("base")
                    .and_then(|b| b.get("seq"))
                    .and_then(|n| n.as_uint());
                let Some(base_seq) = base_seq else {
                    return bad;
                };
                if base_seq >= basis_op.header.writer_sequence {
                    // The recovery keeps the basis: verified-false.
                    return bad;
                }
                // The kill must have HAPPENED: the basis is dead on
                // the fold (cut by the accepted recovery), not
                // merely predicted dead from the cited shape.
                let basis_dead = matches!(fold.ctrl_overlaid(&basis), Some(Verdict::Rejected(..)))
                    && !fold.ctrl_accepted(&basis);
                if !basis_dead {
                    return bad;
                }
            }
            Fact::Stmt(_) => {
                return Err(Unimplemented(
                    "stmt-kind invalidation kill verification awaits a fork-discovery statement \
                     wire shape (§4.7 carries none — audit finding)"
                        .into(),
                ));
            }
        }
        j.intervals.push(None);
        Ok(Ok(()))
    }
}

/// Exact key-SET equality (the strict reader already enforced
/// canonical ORDER; shape checks only need membership).
fn keys_are(n: &Node, want: &[&str]) -> bool {
    n.map_keys().is_some_and(|mut k| {
        k.sort_unstable();
        let mut w = want.to_vec();
        w.sort_unstable();
        k == w
    })
}

enum Fact {
    Op([u8; 32]),
    Stmt([u8; 32]),
}

/// `opfactref = { kind: "op", ref: bytes32 }` — exactly.
fn opfactref(n: &Node) -> Option<[u8; 32]> {
    opfactref_node(n)
}

fn opfactref_node(n: &Node) -> Option<[u8; 32]> {
    if !keys_are(n, &["kind", "ref"]) || n.get("kind")?.as_text() != Some("op") {
        return None;
    }
    n.get("ref")?.bytes_n::<32>()
}

/// `factref = { kind: "op" / "stmt", ref: bytes32 }`.
fn factref(n: &Node) -> Option<Fact> {
    if !keys_are(n, &["kind", "ref"]) {
        return None;
    }
    let r = n.get("ref")?.bytes_n::<32>()?;
    match n.get("kind")?.as_text()? {
        "op" => Some(Fact::Op(r)),
        "stmt" => Some(Fact::Stmt(r)),
        _ => None,
    }
}

/// One journal-replay run's results.
pub struct JournalRun {
    pub final_verdicts: BTreeMap<String, Verdict>,
    /// The sole journal's intervals: (incarnation, terminal name) —
    /// "open" / "abort" / "done".
    pub intervals: Vec<(u64, &'static str)>,
    /// Named state probes, canonical CBOR bytes.
    pub probes: BTreeMap<String, Vec<u8>>,
}

/// Replay one delivery order: operations fold, txn frames replay,
/// the pending set re-evaluates to fixpoint after every arrival, and
/// held tenant classifications overlay derived (§10.5).
pub fn run_journal(
    items: &BTreeMap<String, Vec<u8>>,
    aux: &BTreeMap<String, Vec<u8>>,
    order: &[String],
) -> Result<JournalRun, Unimplemented> {
    // Order convergence is structural, mirroring the fold engine
    // (review R1): the replay is recomputed from scratch over the
    // delivered SET in a content-derived canonical order — ops by
    // chain coordinate, frames after them by content hash — with a
    // verdict-stable fixpoint, so the result is a pure function of
    // the delivered set. (The pre-repair loop retried pendings in
    // arrival order; the committed corpus never diverged, but the
    // mechanism was the same one behind the fold lane's R1 class.)
    let mut uniq: Vec<&String> = order.iter().collect();
    uniq.sort();
    uniq.dedup();
    uniq.sort_by_key(|n| crate::fold::canonical_key(&items[*n]));

    let mut fold = State::default();
    let mut journal = JournalState::default();
    for bytes in aux.values() {
        journal.hold_aux(bytes)?;
    }
    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut hashes: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    for name in &uniq {
        if let Ok(op) = parse_op(&items[*name]) {
            hashes.insert((*name).clone(), op.op_hash());
        }
    }

    let rounds_cap = uniq.len() * 2 + 4;
    let mut stabilized = false;
    for _ in 0..rounds_cap {
        let mut changed = false;
        for name in &uniq {
            match verdicts.get(*name) {
                Some(Verdict::Admitted) | Some(Verdict::Rejected(..)) => continue,
                _ => {}
            }
            let bytes = &items[*name];
            let node = decode(bytes).map_err(|e| Unimplemented(format!("item decode: {e:?}")))?;
            let op_verdicts: BTreeMap<[u8; 32], Verdict> = hashes
                .iter()
                .filter_map(|(n, h)| verdicts.get(n).map(|v| (*h, *v)))
                .collect();
            let v = if node.map_keys().is_some_and(|k| k.contains(&"records")) {
                journal.replay_txn(&node, &fold, &op_verdicts)?
            } else {
                classify(&mut fold, bytes)?
            };
            if verdicts.get(*name) != Some(&v) {
                changed = true;
                verdicts.insert((*name).clone(), v);
            }
        }
        if !changed {
            stabilized = true;
            break;
        }
    }
    if !stabilized {
        return Err(Unimplemented(
            "journal replay did not stabilize within the round cap".into(),
        ));
    }
    let derived = fold.derived_tenant_verdicts()?;
    for (n, h) in &hashes {
        if let Some(v) = derived.get(h) {
            verdicts.insert(n.clone(), *v);
        }
        if let Some(v) = fold.ctrl_overlaid(h) {
            verdicts.insert(n.clone(), v);
        }
    }

    // The intervals result: the fixture convention is one journal.
    let intervals = match journal.journals.len() {
        0 => Vec::new(),
        1 => {
            let j = journal.journals.values().next().expect("len 1");
            j.intervals
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    (
                        i as u64,
                        match t {
                            None => "open",
                            Some(Terminal::Abort { .. }) => "abort",
                            Some(Terminal::Done) => "done",
                        },
                    )
                })
                .collect()
        }
        _ => return Err(Unimplemented("multiple journals in one vector".into())),
    };

    // State probes (exact-name registry; §5.4/§11.1/D-198).
    let mut probes = BTreeMap::new();
    let queue = fold.erase_state();
    let addr_of = |target: &[u8; 32]| -> Option<[u8; 32]> {
        let (lineage, gen, seq) = fold.op_coordinate(target)?;
        journal.commits.get(&(lineage, gen, seq)).copied()
    };
    let queue_addrs: Vec<[u8; 32]> = queue.0.iter().filter_map(addr_of).collect();
    probes.insert(
        "erase-queue accepted entries, item_addrs (§5.4)".to_string(),
        encode_id_array(&queue_addrs),
    );
    // D-198: an entry whose item is a source of a NONTERMINAL journal
    // is manifest-ineligible; sources resolve through the HELD
    // release (unresolvable sources cannot defer).
    let eligible: Vec<[u8; 32]> = queue
        .0
        .iter()
        .filter(|t| {
            !journal.journals.iter().any(|(release_op, j)| {
                j.intervals.last().is_some_and(|t_| t_.is_none())
                    && fold
                        .release_sources(release_op)
                        .is_some_and(|srcs| srcs.contains(t))
            })
        })
        .filter_map(addr_of)
        .collect();
    probes.insert(
        "manifest-eligible erase-queue entries, item_addrs (§5.4 D-198 — a nonterminal referencing journal defers)"
            .to_string(),
        encode_id_array(&eligible),
    );
    probes.insert(
        "retrieval-excluded claims, op hashes (§11.1 m.erase_request — immediate on acceptance)"
            .to_string(),
        encode_id_array(queue.1),
    );

    Ok(JournalRun {
        final_verdicts: verdicts,
        intervals,
        probes,
    })
}

/// Canonical CBOR array of 32-byte strings (probe values).
fn encode_id_array(ids: &[[u8; 32]]) -> Vec<u8> {
    let mut out = match ids.len() {
        n if n < 24 => vec![0x80 | n as u8],
        n if n <= 255 => vec![0x98, n as u8],
        _ => panic!("probe arrays stay small"),
    };
    for id in ids {
        out.push(0x58);
        out.push(32);
        out.extend_from_slice(id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type Loaded = (
        BTreeMap<String, Vec<u8>>,
        BTreeMap<String, Vec<u8>>,
        serde_json::Value,
    );

    fn load(name: &str) -> Loaded {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join("vectors")
            .join(name);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let unhex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        let mut items = BTreeMap::new();
        for (k, hv) in v["inputs"]["items"].as_object().unwrap() {
            items.insert(k.clone(), unhex(hv.as_str().unwrap()));
        }
        let mut aux = BTreeMap::new();
        if let Some(m) = v["inputs"]["aux"].as_object() {
            for (k, hv) in m {
                aux.insert(k.clone(), unhex(hv.as_str().unwrap()));
            }
        }
        (items, aux, v)
    }

    fn order(v: &serde_json::Value) -> Vec<String> {
        v["inputs"]["deliveries"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    }

    /// f13: t2's abort+reopen validate sequentially inside one Txn
    /// (interval 0 aborts, interval 1 opens); t3's competing
    /// terminals violate the journal invariant and the whole frame
    /// discards — interval 1 stays open.
    #[test]
    fn txn_internal_order_and_competing_terminals() {
        let (items, aux, v) = load("f13-txn-internal-order-and-competing-terminals.json");
        let run = run_journal(&items, &aux, &order(&v)).unwrap();
        assert_eq!(run.final_verdicts["t1"], Verdict::Admitted);
        assert_eq!(run.final_verdicts["t2"], Verdict::Admitted);
        assert_eq!(
            run.final_verdicts["t3"],
            Verdict::Rejected("log-corrupt", "storage-quarantine")
        );
        assert_eq!(run.intervals, vec![(0, "abort"), (1, "open")]);
    }

    /// f11 pend arm: the reopen cites the REAL killing recovery
    /// (based below its basis), but unheld — the citation pends,
    /// reserving the interval in both orders.
    #[test]
    fn reopen_recovery_invalidation_unheld_pends() {
        let (items, aux, v) = load("f11-reopen-recovery-invalidation-unheld-pends.json");
        let run = run_journal(&items, &aux, &order(&v)).unwrap();
        assert_eq!(run.final_verdicts["t1"], Verdict::Admitted);
        assert_eq!(run.final_verdicts["t2"], Verdict::Admitted);
        assert_eq!(
            run.final_verdicts["t3"],
            Verdict::Pending("ref-unresolved", "pending-dependency")
        );
        assert_eq!(run.intervals, vec![(0, "abort")]);
    }

    /// f11 verified-false arm: the reopen cites a HELD recovery that
    /// bases AT the revocation — the basis survives the cut, so the
    /// citation fails verification: log-corrupt, interval 0 stays
    /// aborted.
    #[test]
    fn reopen_recovery_keeping_basis_rejects() {
        let (items, aux, v) = load("f11-reopen-recovery-keeps-basis-rejects.json");
        let run = run_journal(&items, &aux, &order(&v)).unwrap();
        assert_eq!(run.final_verdicts["t1"], Verdict::Admitted);
        assert_eq!(run.final_verdicts["t2"], Verdict::Admitted);
        assert_eq!(
            run.final_verdicts["t3"],
            Verdict::Rejected("log-corrupt", "storage-quarantine")
        );
        assert_eq!(run.intervals, vec![(0, "abort")]);
    }

    /// f11-reopen: a shape-valid reopen with an unheld stmt-kind
    /// invalidation PENDS (reserving the interval — no incarnation 1
    /// opens); a stmt-kind BASIS is structurally invalid and
    /// classifies log-corrupt immediately.
    #[test]
    fn reopen_basis_typing_and_unheld_invalidation() {
        let (items, aux, v) = load("f11-reopen-basis-op-kind-and-unheld-invalidation.json");
        let run = run_journal(&items, &aux, &order(&v)).unwrap();
        assert_eq!(run.final_verdicts["t1"], Verdict::Admitted);
        assert_eq!(run.final_verdicts["t2"], Verdict::Admitted);
        assert_eq!(
            run.final_verdicts["t3"],
            Verdict::Pending("ref-unresolved", "pending-dependency")
        );
        assert_eq!(
            run.final_verdicts["t4"],
            Verdict::Rejected("log-corrupt", "storage-quarantine")
        );
        assert_eq!(run.intervals, vec![(0, "abort")]);
    }
}
