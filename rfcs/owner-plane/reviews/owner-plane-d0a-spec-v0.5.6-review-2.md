# Review 2: D0-A Core + Memory specification v0.5.6

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.6 (2,962 lines), against the v0.5.5 synthesized review's
eight-step sequence, the new rulings D-100..D-106, and the v0.5.5
archive (317 changed lines diffed). Written without reading
`owner-plane-d0a-spec-v0.5.5-review.md`. Method: composed traces
first, per the last synthesis's adjudication of my method — each of
the eight repairs was walked through the cross-log/crash/order
interactions the synthesis named (requester orders, ratify-vs-escaped-
effect, 257-head lineages, adoption divergence, renewal
satisfiability, pending-precedence), then byte verification of every
ruling against CDDL/E7/E8/outcomes/dispositions/tag inventory.*

## Executive verdict

**All eight steps landed, and the three hard ones landed with the
structurally correct choice rather than the patch I proposed:**
D-100 *removes* `gens_total` outright — the right call, since the
synthesis showed the count is non-monotone under D-94 displacement,
so no carriage could have made it a freshness signal — and declares
reauth assent a durable one-shot with the residual stated, while
promoting the reconstruction invariant into §2.3 with the correct
"immutable carried reference" qualification. D-101 binds effect
finality to **immutable boundaries only** and makes `c.abandon_writer`
the sixth boundary purpose — the sealing instrument the algebra
needed anyway becomes the finality gate, so the
ratify-grows-after-reliance trace is impossible by construction; all
three consumer mirrors are gated (verified verbatim: §6.1 egress
"acceptance **∧ effect finality**", recovery terminals inheriting the
release barrier, the audit row's "durable **and** … effect-final").
D-102 completes the algebra (six scoped purposes; supersede binds the
predecessor certificate; one total comparator; T3's min-merge
reconciled as intersection-of-immutables; `ref-unresolved` closed and
mapped). D-103 makes paging total by capping open unknown-gap heads
at 64 (E8 row present; 65th unknown → `lineage-gen`). D-104 makes
adoption select the actual Fence commitment
(`{zone_id, rotation_op, control_frontier, recipients_hash}` on the
wire, `storage-orphaned` closed) and honestly voids v0.5.5's
unsatisfiable every-epoch renewal rule for latest-accepted-epoch.
D-105 removes precedence effect from pending recovery. D-106's
fourteen mechanical items verified, including T2's **four**-path
revisit enumeration and the pinned `record_count = |sources|`.

**One high finding — a composed consequence of D-101 crossing the
hosted ceiling, which no text addresses:** hosted planes hold *no
finality-conferring closure*, so a hosted browser that reopens with a
genuine `w.gen(unknown)` permanently loses audited reads (and every
effect-bearing execution) until re-root. Two mediums and two pins.
No wire changes needed beyond one optional ceiling row: this is a
one-ruling v0.5.7, and the prose is otherwise done.

---

## Part 1 — Discharge audit

| # | v0.5.5-synthesis step | v0.5.6 disposition | Residue |
|---|---|---|---|
| 1 | Requester snapshot/monotonicity | **D-100**: `gens_total` removed (all seven remaining mentions are historical); freshness = control-plane monotone triple; durable one-shot posture declared with residual; unheld carried heads → `ref-unresolved`, never `sig-invalid`; §2.3 reconstruction invariant normative | none |
| 2 | Immutable effect finality | **D-101**: finality = `last_known` chain ∨ close cutoff ∨ abandon seal; ratify never confers it; abandon = immutable purpose with the ratify-may-never-exceed rule; three mirrors gated; execution ownership named (daemon effect layer, idempotent per `export_id`/`read_id`); escaped-evidence residual in §14 | **H1**: the hosted ceiling has no finality closure |
| 3 | Boundary purposes/comparator/outcomes | **D-102**: supersede purpose (predecessor-scoped, immutable); total comparator (`"none"` minimal, lex `(gen, seq)`, equal-coordinate hash mismatch = unordered fork evidence) reused by fences; T3 intersection reconciliation; `ref-unresolved` in the outcome enum + pending-dependency row | none |
| 4 | Wide-lineage paging | **D-103**: ≤ 64 open gaps per lineage (E8 + §9.3); latest-page-wins now total over legal lineages; both orderings on the D-102 comparator with non-regression; empty pages legal; proof positions ancestor-consistent/nondecreasing; renewed witness = new issuer scope | none |
| 5 | Recovery adoption; renewal satisfiability | **D-104**: adoption entries carry the Fence commitment (CDDL verified — keyed by zone, highest rotation, predecessor chain); `storage-orphaned` in the storage enum + storage-quarantine row; renewal wraps the **latest accepted epoch** per held zone (the unsatisfiable rule voided honestly); predecessor-KEM custody until completion | none |
| 6 | Validity before precedence | **D-105**: recovery resolves adopted rotations and cutoff Heads before C3′; pending = no precedence effect (C2 evaluates as if absent) — deterministic in the eventual fold | none |
| 7 | Transfer/storage integration | **D-106**: `record_count = |sources|` pinned + PendingXfer equality; export row mirrors fixed; 512 B named a record-rate posture; XferDone counts accepted *effect-final* imports; destination replay-index rebuild precedes source recovery; RewrapComplete literal aligned; `recipientset` typed (≤ 256, keyed by device) with `control_frontier` = op-hash identity | pin 1 (`recipientset` lacks `v`) |
| 8 | Feed-fork revisit; record; artifacts | **D-106**: fourth revisit path (feed-fork exposure retro-quarantines at the commitment's fold position) — T2 now enumerates four; frozen release stamp declared final for the source read (+ vector); O7 admin-key wording, D-80 pointer, `zoneheads` sort key; companion schema still first in Open-tracked, D-91 artifact-pending | none |

B.1/B.2/B.3 untouched; the §15 record carries supersession pointers
on D-87/D-95/D-98/D-99 for this round's amendments.

---

## Part 2 — Findings

### High

**H1. Hosted planes hold no finality-conferring closure — a genuine
unknown-gap reopen permanently darkens the device's audited reads,
and the posture is unstated.** Compose three ratified rules: (1)
D-101 — effect finality requires an **immutable** boundary
(`last_known` incorporation, a close-purpose cutoff, or an abandon
seal), and ratify cutoffs "never confer finality"; (2) §7.5 — the
hosted ceiling forbids `c.abandon_writer` (D-15), `c.zone_policy`
(D-43), and `c.cap_epoch_bump`, i.e. **every** carrier of the
immutable closures; the hosted self-service instrument is
requester-attested `c.cutoff` — a **ratify** boundary; (3) §11.1 —
audited results release only when the read's coordinate is
effect-final. A hosted browser that loses its store and reopens with
`w.gen(last_known = "unknown")` — the exact scenario the D-54
self-cutoff machinery was ratified for — can tidy its frontier but
can never again make generation 1 immutably closed: every subsequent
audited read (any sensitive read — the D-14/D-29 hosted diary lane)
blocks forever, until re-root. §9.3's reassurance ("unknown heads
cannot permanently grow a hosted frontier") is now half the story —
frontier hygiene survives, effect flow does not — and D-52/D-64's
"visible, owner-remediable denial" promises a remediability the
ceiling cannot deliver. Deterministic, so not a divergence defect —
but it bricks a ratified capability on the modeled-normal crash path
with the spec's remedy list empty. Fix is one ruling either way: (a)
admit **requester-attested self-abandon of the device's own gap
generations** into the ceiling (the D-54 pattern exactly; D-15's
rationale — destructive maintenance, key-destruction theater — does
not cover sealing one's own abandoned generations, which destroys
nothing and is strictly finality-restoring), or (b) declare re-root
the remedy (consistent with the D-64 budget posture) and require the
product-copy warning. I recommend (a); either needs the family-7/11
composed vector (hosted gap → blocked audited read → remedy →
restored).

### Medium

**M1. The two reassurance sentences H1 exposes need their finality
footnote whichever way it is ruled.** §9.3's hosted-cutoff sentence
(frontier hygiene ≠ effect finality) and the D-52/D-64
"owner-remediable" claims should name the hosted finality remedy once
it exists — otherwise the discrepancy audit will re-derive H1 from
the prose contradiction alone.

**M2. The composed hosted-gap trace is missing from the families.**
Family 7 gained abandon-seal vectors (ratify-may-never-exceed) and
family 11 the effect-finality races, but no vector walks the hosted
composition end-to-end (unknown reopen → ratify cutoff → audited
read still blocked → the H1 remedy). It is precisely the trace two
independent implementations would today resolve differently only in
*product* behavior — encode it.

### Pins

1. **`recipientset` violates E6**: it is a hashed object
   (`H_recips(canonical recipientset)`) with no `v`, while its exact
   sibling `survivorset` carries `v: 1`. checkpointobj's exemption
   reasons from being an operation body (versioned by
   `operation_version`); `recipientset` is not a body. Add `v: 1`
   before the corpus pins the bytes.
2. **The §7.1 `c.cutoff` row's formula comment** still reads
   "`lineage_version, repoch`" mid-row while the prose beside it
   explains the D-100 removal — one clause of comment hygiene so the
   registry row and A.3 stay byte-identical on the formula (the CDDL
   is already correct).

---

## Part 3 — Gate-A readiness

The composed traces the last synthesis demanded now close: the
requester ceremony has a monotone control-plane snapshot; effect
finality cannot be reopened by a growable boundary; every legal
lineage fits the paging; adoption names the exact Fence state;
renewal is satisfiable; pending recovery exerts no precedence; the
revisit enumeration is complete at four. The residue is one composed
product-posture hole (H1) that crosses two subsystems ratified in
different rounds — exactly the kind of finding that argues the prose
has reached its audit-ready state, because finding it required
composing rules that are each individually correct.

**Recommendation.** Cut **v0.5.7**: H1's one ruling (self-abandon in
the ceiling, or the declared re-root posture) with its two
consequence sentences (M1) and one composed vector (M2), plus the two
pins — then **freeze**. Author `d0a-vector-cases.v1.json` as the
first corpus artifact, build `owner-plane-core`, generate the corpus
(the hosted-gap composition, the four revisit paths, the
abandon/ratify ordering, the adoption matrix, and the D-102
comparator table belong in the first tranche), record family 14, and
run the prose↔schema↔vector discrepancy audit as the Gate-A decider.
Durable P1 writes remain prohibited until Gate B plus the umbrella's
P0.5/tombed-cutover prerequisites, unchanged.
