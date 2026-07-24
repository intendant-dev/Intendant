# Review 2: D0-A Core + Memory specification v0.5.5

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.5 (2,827 lines), against the v0.5.4 synthesized review's
eight-step closure sequence, the new rulings D-93..D-99, and the
v0.5.4 archive (439 changed lines diffed). Written without reading
`owner-plane-d0a-spec-v0.5.4-review.md`. Method: bytes-first
verification of all seven rulings (CDDL, E7/E8 rows, tag inventory,
frame mirrors, §15 supersession pointers), then composed-seam probes
targeted at the new machinery — the lesson of the last synthesis
being that fresh seams live where new rulings meet old absolute
sentences and where signed state crosses the control/tenant log
boundary.*

## Executive verdict

**The eight-step program landed whole: every synthesis blocker got
real protocol machinery, and the composed counterexamples now have
answers on the wire.** D-93's cutoff algebra is the round's
centerpiece — four disjoint boundary purposes (ratify max-composing
with cascading revival as a derived fold state; revoke/close/recover
immutable), so the grant-revoked-at-H5-restored-by-H10 widening is
structurally impossible; the grant-epoch lower bound, staged
union-coverage closure, unheld-Head pendency, and equality pins all
land with it. D-94 separates admission from execution — effect-bearing
operations (egress, audited-result release, import execution,
terminal records) run only at *effect-final* coordinates, the eligible
charge set is the accepted set with cascading release and revival,
and T2 now enumerates exactly three deterministic revisit paths
(closing my v0.5.4 B1). D-95 gates qualification on verified ancestry
to committed feed heads (the 51′-wrong-branch window is shut) and
puts the requester's `live_heads` on the wire. D-96 makes checkpoints
a monotone machine (this-checkpoint-causes retirement, latest-page-
wins, fence non-regression, an honest 48 KiB joint budget). D-97
commits rotation intent durably (`recipients_hash` + the new `recips`
tag in Fence/RewrapDone), makes `c.wrap_add` validity a pure
control-log fact, and — the round's best composed fix — gives C3′
`adopted_rotations`, so recovery either adopts activated storage or
the activating replica storage-quarantines, never silently strands.
D-98 removes the self-attested `bundle_size` for a content-independent
`record_count × 512 B` surcharge (the compromised-signer objection
dissolves by construction) and makes terminal precedence
state-derived with non-empty aborts. D-99 reorders the control
pipeline so the body authenticates before C3′ precedence fires. All
five of my v0.5.4 findings and all three pins are resolved; the D-92
and D-84 rows carry their supersession pointers; B.2/B.3 intact.

**One blocking defect — the recurring ruling-versus-bytes genre, and
this time it breaks the protocol's bedrock invariant:** D-95's own
`gens_total` is signed into both requester attestations but carried
in neither, so a control operation's signature validity depends on
non-carried tenant state — the exact defect the same ruling fixed
for `live_heads`, reintroduced one field over, with control-fold
divergence as the blast radius. Two mediums (three stale mirrors of
the D-94 barrier; `c.abandon_writer` orphaned outside the cutoff
algebra) and two pins. Roughly six sentences plus one CDDL field:
v0.5.6 should be the freeze.

---

## Part 1 — Discharge audit

| # | v0.5.4-synthesis step | v0.5.5 disposition | Residue |
|---|---|---|---|
| 1 | Scoped cutoff algebra | **D-93**: purpose table (ratify/revoke/close/recover) derived from the carrying op; ratify-only max-compose; revoke immutability pinned at `c.revoke_grant` (equality with the grant's zone/lineage); closure = union coverage with staged standalone cutoffs; `grant.capability_epoch ≤ op.capability_epoch` before slack (§9.4); unheld Heads pending, differing hash = body-invariant + fork | **M2**: `c.abandon_writer` unclassified |
| 2 | Effect finality; eligible charges | **D-94**: §4.3 effect-final rule (all lower generations closed; solo case immediate; deferral surfaced); charge set = accepted set, releases cascade, revival re-derives; T2's three-path revisit enumeration; telemetry = the standing quarantine lane | **M1**: three consumer-site mirrors lack the barrier |
| 3 | Feed ancestry; requester bytes | **D-95**: T3 ancestry gating (qualification only via a complete verified path to the committed head; min-merge needs ancestor proof); `ccutoff.requester.live_heads` carried (`zoneheads`, E7-keyed, plural under gaps — "assent dies the moment the lineage writes past it"); registry/Appendix formulas byte-identical | **B1**: `gens_total` signed, not carried |
| 4 | Checkpoint monotone machine | **D-96**: retirement = this checkpoint's causes (pre-state-live, never re-listed, omitted-stays-live); latest-page-wins; fences never regress; encoded object ≤ 48 KiB (E8 row; family-1 encoder vector); `time_witnesses` ≤ 64 matching `proof_positions`; E7 carries covers/fences/live_heads/adopted_rotations keys | none |
| 5 | Rotation/renewal durable bytes | **D-97**: Fence + RewrapDone commit `{control_frontier, recipients_hash}` (H_recips; `recips` tag added); intent = wraps ∪ wrap-adds at-or-before the committed control position; `c.wrap_add` epoch validity control-log-pure; **C3′ `adopted_rotations`** (adopt or storage-quarantine — explicit owner choice); KEM-rotating renewal requires ≤ 128 memberships (drain first); empty `history_cutoffs` legal; "mints no grants and no new-zone access" | none |
| 6 | Transfer verifiability | **D-98**: `bundle_size` **removed** — charge = op bytes + `record_count × 512 B` (content-independent; claim bytes were charged at their own admission); state-derived terminal precedence (replay keys → XferDone even post-erasure; then source-erased; then reject-permanent); `xferabort.missing` non-empty (empty residue = XferDone); §11.8 puts release completion and import execution behind the barrier | (M1's mirrors) |
| 7 | Control pipeline order | **D-99**: parse → arm → sig → **body** → C3′ precedence-validity → placement → state invariants — a signed header over malformed body bytes never suppresses C2 | none |
| 8 | Record + artifacts | D-92 rescoped-by-D-93 pointer; D-84 narrowed to the exhausted-existing-device claim; D-90 first-trigger wording superseded by D-98's state precedence; companion schema now **first entry in Open-tracked** with D-91 explicitly artifact-pending | none |

---

## Part 2 — Findings

### Blocking

**B1. `gens_total` is signed into both requester attestations but
carried in neither — a control operation's signature validity now
depends on non-carried tenant state, which breaks control-fold
determinism.** D-95 added `gens_total` (the lineage's lifetime
accepted `w.gen` count — **tenant**-chain content) to the signed
messages of `c.lineage_reauth` and `c.cutoff`, with admission
requiring "equality … at this operation's control position." But the
`clineagereauth.requester` group is still
`{device_cert, ctrl_frontier, sig}`, and `ccutoff.requester` carries
`live_heads` only — so a validator must **reconstruct** the signed
message using its own tenant view. Tenant and control logs have no
total order (the precise lesson D-95's `live_heads` carriage encodes:
*"v0.5.4 signed state it did not carry, and control-first delivery
misread as sig-invalid"*): a replica that folds the control op before
the lineage's latest `w.gen` ops computes a smaller `gens_total`,
reconstructs a different message, and gets `sig-invalid` —
**reject-permanent** — while a tenant-first replica accepts. Two
replicas disagreeing on the validity of the *same control-chain
operation* is control-fold divergence, the worst failure class in the
document, on the hosted plane's mandatory self-service path. And
`live_heads` does not rescue it: it covers only the named zones,
while `gens_total` sums across every zone the lineage writes in. Fix
(one CDDL field + two sentences): carry `gens_total: uint` in both
requester groups; admission compares the carried value against the
validator's derived count with **pending-dependency until the tenant
view reaches it** (the D-93 unheld-Head posture), never
reconstruction into `sig-invalid`; family-7 negatives for a stale
carried value. The general rule worth one sentence of its own: **a
signed message must reconstruct from the body alone** — D-95 states
it as `live_heads` rationale; promote it to a normative invariant so
the next field can't repeat this.

### Medium

**M1. Three consumer-site mirrors still lack the D-94 barrier.**
§4.3 and §11.8 carry the rule, but: (a) §6.1 still says egress
releases *"complete at release acceptance"* — the pre-D-94 sentence,
now contradicting §11.8's *"release completion … sit[s] behind the
effect-finality barrier"* in the very section a journal implementer
reads; (b) §6.1's recovery procedure writes terminal records
unconditionally, though D-94 enumerates "a transfer's terminal
record" as effect-bearing (the release coordinate must be
effect-final); (c) the §11.1 audit row still releases results "only
after [the audit Txn] is durable" — missing the *and effect-final*
half while §4.3 names audited-result release explicitly. One clause
each; all three are the discrepancy-audit genre, but they sit on the
security-relevant path (an escaped effect is the thing D-94 exists to
prevent).

**M2. `c.abandon_writer` is orphaned outside the D-93 cutoff
algebra.** Its `at: head` is a boundary; the algebra's four purposes
are assigned by carrying operation, and abandon is not in the table.
Unclassified, its composition is a fixture choice: can a later ratify
cutoff at a higher head revive operations beyond the abandon point?
(Surely not — abandonment should be close-like immutable, but the
text doesn't say.) Does the unheld-Head pendency rule cover its
`at`? One table entry plus one clause.

### Pins

1. **`ccutoff` live-head equality timing**: "each named zone's
   live-head set … equal the lineage's current one" — state that a
   validator behind those heads holds the op pending-dependency (the
   carried set makes this safe; the check should say so) rather than
   rejecting.
2. **D-95's row** should note the carriage repair when B1 lands
   (the record's own supersession convention).

---

## Part 3 — Gate-A readiness

The blocker arc across seven rounds tells the story: a missing verb
(v0.4), an unencodable genesis lane (v0.5), a missing 15-verb list
(v0.5.1), an un-landed audit wire (v0.5.2), a required-key contradiction
(v0.5.3), a stale absolute sentence (v0.5.4), and now a single
non-carried field (v0.5.5). The defect class has narrowed from whole
subsystems to one field per round — the process is converging, and
what remains is exactly what the executable corpus catches
mechanically. Every structural question the syntheses raised now has
ratified machinery with bytes; the residue is one field, six
sentences, and the artifacts (companion schema, core, corpus,
harness, family 14) that Gate A has always still required.

**Recommendation.** Cut **v0.5.6**: B1 (one CDDL field in each
requester group + the pending-comparison rule + the promoted
reconstruct-from-body invariant), M1 (three clauses), M2 (one table
entry + one clause), two pins — then **freeze the prose as the
audit's input**. Author `d0a-vector-cases.v1.json` first, build
`owner-plane-core`, generate the corpus — lead with the family-7
gens_total staleness negatives, the D-93 purpose-composition
vectors, the D-94 displacement/effect-finality races, the D-95
ancestry gating, and the D-97 recovery-over-Fence adoption matrix —
record family 14, and run the prose↔schema↔vector discrepancy audit
as the Gate-A decider. Durable P1 writes stay prohibited until
Gate B plus the umbrella's P0.5/tombed-cutover prerequisites,
unchanged.
