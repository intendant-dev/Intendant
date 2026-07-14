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
    /// Held facts for reference resolution: aux operation hashes and
    /// aux statement ids.
    held_ops: Vec<[u8; 32]>,
    held_stmts: Vec<[u8; 32]>,
}

impl JournalState {
    /// Register one `aux` entry: an operation triple or a signed
    /// statement (`{stmt, sig}` — `stmt_id = H_stmtid(bytes)`).
    fn hold_aux(&mut self, bytes: &[u8]) -> Result<(), Unimplemented> {
        if parse_op(bytes).is_ok() {
            self.held_ops.push(domains::h("op", bytes));
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
        self.held_ops.contains(h) || fold.holds_op(h)
    }

    /// Replay one txn frame. `Err(inner Verdict)` semantics ride the
    /// outer `Verdict` — this returns the frame's classification.
    fn replay_txn(&mut self, node: &Node, fold: &State) -> Result<Verdict, Unimplemented> {
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
        let mut pend: Option<Verdict> = None;
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
                local.apply_reopen(rec, fold)?
            } else {
                return Ok(corrupt);
            };
            match step {
                Ok(()) => {}
                Err(v @ Verdict::Pending(..)) => {
                    if records.len() > 1 {
                        // A pending record inside a multi-record
                        // commit — ordering vs the discard rule is
                        // unpinned.
                        return Err(Unimplemented("pending record in a multi-record txn".into()));
                    }
                    pend = Some(v);
                }
                Err(_) => return Ok(corrupt),
            }
        }
        if let Some(v) = pend {
            // The reservation: nothing commits; the frame re-evaluates
            // when the cited fact lands.
            return Ok(v);
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
            return Err(bad);
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
        if inc + 1 != j.intervals.len() as u64 || j.intervals[inc as usize].is_some() {
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
        if !matches!(
            rec.get("reason").and_then(|r| r.as_text()),
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
        if inc + 1 != j.intervals.len() as u64 || j.intervals[inc as usize].is_some() {
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
        // Transition legality against the (transaction-local) state.
        {
            let (j, inc) = match Self::open_interval(&mut self.journals, rec) {
                Ok(v) => v,
                Err(v) => return Ok(Err(v)),
            };
            if inc + 1 != j.intervals.len() as u64 {
                return bad;
            }
            // Basis-match: the invalidated recorded cause.
            match &j.intervals[inc as usize] {
                Some(Terminal::Abort { bases }) => {
                    if !bases.contains(&basis) {
                        return bad;
                    }
                }
                Some(Terminal::Done) | None => return bad,
            }
        }
        // Holding checks (verifiable-when-held → pend).
        if !self.op_held(&basis, fold) {
            return Ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        let held = match invalidation {
            Fact::Op(h) => self.op_held(&h, fold),
            Fact::Stmt(id) => self.held_stmts.contains(&id),
        };
        if !held {
            return Ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        let (j, _) = match Self::open_interval(&mut self.journals, rec) {
            Ok(v) => v,
            Err(v) => return Ok(Err(v)),
        };
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
    let mut fold = State::default();
    let mut journal = JournalState::default();
    for bytes in aux.values() {
        journal.hold_aux(bytes)?;
    }

    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut pending: Vec<String> = Vec::new();
    let mut hashes: BTreeMap<String, [u8; 32]> = BTreeMap::new();

    let step = |name: &String,
                fold: &mut State,
                journal: &mut JournalState|
     -> Result<Verdict, Unimplemented> {
        let bytes = &items[name];
        let node = decode(bytes).map_err(|e| Unimplemented(format!("item decode: {e:?}")))?;
        if node.map_keys().is_some_and(|k| k.contains(&"records")) {
            journal.replay_txn(&node, fold)
        } else {
            classify(fold, bytes)
        }
    };

    for name in order {
        if let Ok(op) = parse_op(&items[name]) {
            hashes.insert(name.clone(), op.op_hash());
        }
        let v = step(name, &mut fold, &mut journal)?;
        verdicts.insert(name.clone(), v);
        if matches!(v, Verdict::Pending(..)) {
            pending.push(name.clone());
        }
        loop {
            let mut progressed = false;
            let mut still = Vec::new();
            for pname in pending.drain(..) {
                let v = step(&pname, &mut fold, &mut journal)?;
                verdicts.insert(pname.clone(), v);
                match v {
                    Verdict::Pending(..) => still.push(pname),
                    Verdict::Admitted => progressed = true,
                    Verdict::Rejected(..) => {}
                }
            }
            pending = still;
            if !progressed {
                break;
            }
        }
        let derived = fold.derived_tenant_verdicts()?;
        for (n, h) in &hashes {
            if verdicts.get(n) == Some(&Verdict::Rejected("duplicate", "duplicate-idempotent")) {
                continue;
            }
            if let Some(v) = derived.get(h) {
                verdicts.insert(n.clone(), *v);
            }
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
            .join("..")
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
