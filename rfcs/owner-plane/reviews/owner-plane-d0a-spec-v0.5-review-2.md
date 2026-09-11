# Review 2: D0-A Core + Memory specification v0.5

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5 (1,975 lines), against the frozen umbrella RFC (v3.1/D0), the v0.4
synthesized review's six consolidated freeze blockers and seven owner
rulings (D-48..D-55, approved verbatim), and the v0.4 archive
(`~/agenda-rfc-archive/2026-07-12-d0a-v0.4-as-reviewed.md`). Written
without reading `owner-plane-d0a-spec-v0.5-review.md`. Method: full
three-pass read; mechanical sweep greps re-run independently; §9.1
diffed against the v0.4 archive; B.2/B.3 canonical ordering re-derived
by hand (byte-level, incl. the length-prefix property of text-string
encodings).*

## Executive verdict

**The authority/reducer closure pass succeeded on its own terms: all
six v0.4-synthesis blockers are discharged at the wire/reducer level,
and this time the closed shapes carry one executable meaning each.**
Service receipts resolve to verifiable keys through `c.service_key`
descriptors with non-retroactive policy binding; every cutoff body is
zone-qualified; revocation has executable compound semantics (early
exclusion, target-excluding wraps, all-zone coverage, cap-64 with
continuation); `authorized(j)` consumes `relation` against a defined
portable principal; transfer is record-level with a completed-set
journal; the erase manifest derives tombstones; the schema closes
`draw_order`, `recovery_pk`, `wrapped_dek`, survivor pairs, and the
set annotations. The decision record is complete through D-55 with no
silent drift, and the B.2/B.3 policy constants are untouched and
correctly canonically ordered (verified independently this pass).

**It is not yet freezable.** A fresh walk against the spec's own
closure tests finds **two blocking defects — both residues of the
D-54 edits — plus four high consistency defects concentrated in the
hosted ceiling, three mediums, and twelve pins.** The blockers are
narrow but not editorial: both require small wire/normative additions
(a stated acceptance rule; a requester field plus one signing-domain
tag), which is precisely why they must land before canonical vector
bytes exist. One more focused patch (v0.5.1), two one-sentence owner
ratifications, then implement.

---

## Part 1 — Discharge audit of the v0.4 synthesized blockers

| # | v0.4 blocker | v0.5 disposition | Residue |
|---|---|---|---|
| 1 | Bootable genesis/audit/generation path | **Discharged.** `audit.write` in both vocabularies + CDDL, system-only actor rule (O5 rewritten), single genesis lineage carries the audit grant, pinned budgets (1e6/256 MiB), capability epoch 1 at genesis + zone create, cross-field validity clause, `w.gen` axis bypass in §9.3 + §10.2 + family 10, E4 stale statement fixed, `max_generations = 8` pinned at genesis | Q3 (hosted grantability of `audit.write` contradicts §7.5(b)); Q5 ("write verb" definition) |
| 2 | Verifiable service/proof-history model | **Discharged.** `c.service_key` descriptors; one witness predicate (`"connect" ∈ time_witnesses`; flag removed); non-retroactive binding; lease issuers = qualified witness set; per-scope `issuer_seq` with fresh-key renewal; compromise `receipt_cutoffs`; renewal never ratifies pendings (D-49); `c.checkpoint proof_cutoffs` convergent hardening; grandfathered lane named (D-54) | **Q1 (blocking):** the budgets acceptance lane is still unstated in §9.1; Q7 (service-key compromise has no retro-disqualification arm); Q9 (§9.1 hardening sentence lags §7.1) |
| 3 | Zone-qualified cutoffs, executable revocation, lineage recovery | **Discharged.** `zonecutoff` everywhere (`c.cutoff`, revocation, recovery, abandon); D-50 early exclusion + invariants + `c.revoke_zones`; one reauthorization path; single-use requester attestation on `c.lineage_reauth`; hosted self-cutoff ruled (D-54); `c.drill` portable admission; O7 reserved header values + signer/actor pins; grant-issuance epoch equality | **Q2 (blocking):** the hosted self-`c.cutoff` attestation has no wire shape; Q4 (hosted exclusion rotations inadmissible as listed) |
| 4 | Relational/compound Memory authority | **Discharged.** P(op) defined; principal-level `self`/`author` (D-51) consumed by `authorized(j)`; author retract/supersede via any claim-authoring verb; assert halves portable with the Txn demoted to a local commit rule; diary compound linkage + idempotent completion; `supersedes[]` advisory; `retired_by` projection; O4/§10.1/§11.4 daemon evidence aligned | Q8 (`actor.id` minting unpinned outside control ops and sessions) |
| 5 | Evidence/transfer/erase identities | **Discharged.** One locator-hash domain (`H_evrec`) — the raw-SHA-256 sentence is gone (grep 0); `effective_shallow` named and exact; cross-plane refs sensitive until D0-B; record-level replay key + `content_digest`/`record_count`/completed-set XferDone; equality-bound imports; `mexportrel` carries the evaluation point; source-zone witness policy; egress journals nothing; typed `erasemref` → derivable tombstones; `survivorpair`; `.size 48`/`.size 32` | Q6 (the transfer journal has no terminal failure state) |
| 6 | Conformance artifacts | **Discharged.** `draw_order` required with `rng`; outcome/disposition deliberately plain strings with harness cross-validation (stated); set annotations swept; families 9/10/11/12/13 extended exactly per the synthesis; §15 rows D-48..D-55 complete and matching the body | pins only |

Also verified: every finding of my v0.4 review (G1–G5) resolves in
v0.5; of my twelve v0.4 pins, ten are applied, and the two the
synthesis rejected (B.2 display order; policy-hash precedence flip) I
now confirm were correctly rejected — I re-derived B.2's rule ordering
this pass and it is exactly E7 canonical-encoding order (the
length-prefix byte of the text-string encoding sorts before content
bytes, which is why `[peer, session, external, safe-human]` is
canonical). The two remaining `accept_connect_time` greps are both
removal-notices (T2 and the D-48 row), not live uses.

---

## Part 2 — Findings

### Blocking

**Q1. The budgets acceptance lane exists in the vectors and the
ruling, but not in the normative rule.** §9.1's operative sentence is
byte-identical to v0.4: *"the operation is accepted iff a qualified
signed accept receipt exists with `seen_ms ≤ D`… Otherwise:
`deadline-unreceipted`, pending-dependency"* — unconditional on
`deadline_fallback`. Yet family 9 requires *"missing-witness →
quarantine **vs budgets lane**"* (two lanes), and D-54's new scoping
sentence — *"`deadline_fallback` is scoped to deadline-bearing
items"* — presupposes the fallback does something **for**
deadline-bearing items. As written it does nothing for them: its only
consumed effect anywhere in v0.5 is §4.3's mandatory finite budget.
Concrete failure: `flow.expiry_deadline_ms` is REQUIRED (CDDL) and
`mexportrel.expiry_deadline_ms` is required, so on a solo plane —
which by D-28 can never hold a qualified receipt — **every same-plane
cross-zone transfer's import pends forever** under the literal rule,
contradicting the bounded solo posture and §7.3's spirit. Fixtures
would have to invent the lane, which is the exact Gate-A disqualifier.
Fix (one paragraph): in `deadline_fallback = "fail-closed"` zones the
current rule stands; in `"budgets"` zones deadline fields are
**advisory-only** — acceptance is unconditional on receipts, the
posture's bound being the mandatory finite budgets — and the family-9
budgets-lane vector pins it. The advisory-only sentence is new
normative surface (it decides the past-deadline-receipt edge too):
**one-sentence owner ratification recommended.**

**Q2. The hosted self-`c.cutoff` attestation (D-54) is unencodable.**
§7.5(c) admits `c.lineage_reauth` **and** `c.cutoff` for the
requesting device's own lineage, *"both requester-attested"*; §9.3's
hosted unknown-head retirement and the D-54 row depend on it. But
`ccutoff = zonecutoff = { zone_id, lineage, accepted_through }` — no
requester group — and the closed tag inventory has `reauth` but no
cutoff-attestation domain. The spec's own argument for
`c.lineage_reauth` (*"the admin-arm header alone cannot prove 'own
lineage'"*) applies verbatim: on a hosted plane the root key lives in
the admitted TCB, so without a device co-signature the service can
retire any lineage's unknown heads unilaterally — the exact authority
D-54 withheld. Fix: add
`? requester: { device_cert, ctrl_frontier, sig }` to `ccutoff`
(REQUIRED under the hosted ceiling; absent on plain admin cutoffs),
sig over `msg("cutoff", { plane_id, zone_id, lineage,
accepted_through, request_id, ctrl_frontier })`, and add `cutoff` to
the closed tag inventory — a version event that is free now and
expensive after corpus freeze. Family-7 vector: hosted self-cutoff
accepted; hosted cutoff naming another device's lineage rejected;
replayed attestation rejected.

### High

**Q3. `audit.write` is hosted-grantable in §11.1 and hosted-forbidden
in §7.5(b).** §11.1: *"grantable on any device class (hosted included
— every device audits its own sensitive reads)"*. §7.5(b): *"grantable
verbs are the hosted-safe set (search, read, evidence.read, propose,
assert, judge.safe, pin.safe, erase.request, raise)"* — no
`audit.write` (and §7.6's hosted-browser never-grantable column
doesn't exclude it, so the three tables disagree two-to-one). The
first hosted device escapes via the genesis audit grant, but every
**later** hosted browser's grants are bounded by (b), so it can never
hold `audit.write` → per D-52 its sensitive-space reads are
permanently `audit-unavailable` → D-14's ratified hosted
private/sensitive storage is half-bricked on enrolled devices. Fix:
add `audit.write` to the (b) list — safe, because the system-only
actor rule confines it to the device's own service writer and the
registry pins `space == audit`.

**Q4. Hosted revocation's exclusion rotations are inadmissible as
listed.** v0.5 moved revocation rotations to by-reference
(*"separately committed `c.kek_rotate` ops"*, D-50), but §7.5(c)'s
exhaustive admissible list has no `c.kek_rotate` entry — only the
prohibition *"no `c.kek_rotate` with an erase manifest"*, which
implies plain rotations are admissible while the list says they are
not — and the parenthetical *"(compound — exclusion rotation
included)"* is a stale v0.4 embedded-rotation artifact. `c.revoke_zones`
is missing from the list too. Strict reading: a hosted
`c.revoke_device` can never satisfy its all-zone coverage invariant →
pends forever → **hosted planes cannot revoke a browser**, a ratified
hosted capability. Fix: admit `c.kek_rotate` **without**
`erase_manifest` (and `c.revoke_zones`) under the ceiling; delete the
stale parenthetical.

**Q5. "Write verb" has two incompatible normative readings.** §11.1's
rows parenthetically define *"any write verb ({propose, assert})"*;
the grant CDDL requires `lineage` *"when ops ∩ write verbs ≠ ∅"*, and
§4.3 hangs the one-zone rule and the mandatory finite budget on
"write-capable". Under the parenthetical reading, a `judge.safe`-only,
`pin.safe`-only, or `audit.write`-only grant requires no lineage, no
single zone, no budget — yet its operations are chain writes, and
admission's *"lineage binding"* step then has nothing to bind. Two
implementations will disagree on the schema validity of a judge-only
reviewer grant. Fix: define once — **op-authoring verbs** = every verb
except `search`/`read`/`evidence.read` (drives grant `lineage`/zone/
budget requirements and §4.3's "write-capable"), **claim-authoring
verbs** = `{propose, assert}` (drives the author-relation rows and
`w.gen`'s "implicit in any write verb").

**Q6. The transfer journal cannot terminate on failure.** §6.1:
XferDone requires `|completed| == record_count`; *"a source erased
mid-transfer fails the remaining imports closed"*; recovery
*"re-derives the bundle… re-runs only the missing imports, then
writes XferDone"* — unconditionally. With an erased source (a path the
spec itself names) or any reject-permanent import at the destination,
ALL-imported never occurs, the bundle is no longer derivable, and the
recovery instruction is unexecutable: the PendingXfer dangles forever
with divergent implementation behavior (retry loop vs silent
abandonment). Local-only, no authority impact — but an unclosable
state machine in the crash-recovery story fails the synthesis's own
closure test ("identical … completion after any crash point"). Fix:
one terminal record (either `XferAbort { export_id, completed,
reason }` as a fourth `txnrec`, or permit XferDone with
`|completed| < record_count` plus a closed `reason`), a rule for when
it may be written (every missing import individually terminal:
erased source or reject-permanent), and a family-13 vector.

### Medium

**Q7. Service-key compromise has no retro-disqualification arm.**
Device issuers get `receipt_cutoffs` + T4 retro-disqualification on
`mode = "compromise"`; `c.service_key` rotation only appends, and
*"the old one validates receipts admitted under its epochs"* — so
forged receipts admitted during a Connect key compromise window stand
forever with no remedy. Either add an optional
`revoked_through: issuer_seq` to the rotation (mirroring
`receipt_cutoffs`) or state the residual explicitly in §14 (zone
policy chooses witnesses; a compromised service witness is a chosen-
witness failure). Owner's call which; silence is the only wrong
option.

**Q8. `actor.id` minting is unpinned for most kinds, so P(op) is not
yet fully portable.** §11.2's principal `P = (writer.lineage,
actor.kind, actor.id)` is only as portable as `actor.id`'s vocabulary.
O7 pins control ops (`"owner"`); sessions *"carry the session id"*;
but human/daemon/browser/peer **tenant**-operation ids have no minting
rule — two clients of one plane can disagree on whether the same human
is the same principal, which moves `self`/`author` outcomes. Pin one
line per kind (e.g. human = stable per-plane user id assigned at
enrollment, daemon/browser = device_id hex, peer = peer_id), or
explicitly reduce P for human actors to lineage + direct-human
evidence.

**Q9. §9.1's hardening sentence lags the D-55 rule.** §9.1: hardens
*"when a `c.checkpoint` GC fence passes the operation's position
without one"*; §7.1's `c.checkpoint` row requires the fence to also
cover **every qualified witness feed** via `proof_cutoffs`. "Only
when" makes §9.1 technically necessary-not-sufficient, but an
implementer reading §9.1 alone hardens early and diverges. Align the
sentence (and state that witness-feed qualification for the fence is
evaluated at the fence's admission position, per D-48
non-retroactivity).

### Pins (exactness, one line each)

1. **`connect_service_key` equality** — state it is `H_key({alg, pk})`
   of the installed descriptor's key (T2 + §4.7 force this reading;
   say it once).
2. **Descriptor validity interval** — pin
   `[valid_from_admin_epoch, successor's valid_from)` and the rule for
   two descriptors claiming the same epoch (reject as differing
   duplicate, or newest-in-chain wins — pick one).
3. **Genesis grant `spaces` value** — the row pins verbs and budget
   but not `spaces`; pin it (`"*"` or `[home]`, and whether the audit
   space is inside the device grant's read scope) so family-7 fixtures
   don't invent it.
4. **Dead verb `admin`** — no registry row consumes it; name its
   consumer (edge-side control gating?) or mark it reserved-frozen in
   M3.
5. **Hosted audit-budget exhaustion remedy** — D-52 says
   "owner-remediable", but `c.cap_epoch_bump` and standalone `c.grant`
   are outside the ceiling; state the in-ceiling remedy (fresh audit
   grant riding a `c.enroll` compound — and whether compound grants
   may name an existing device — or name re-root as the remedy).
6. **Assert row wording** — "same actor+session" predates §11.2;
   restate as "same authoring principal P".
7. **`c.cap_epoch_bump`** — pin `new_epoch = current + 1` (the row
   says "+1"; the body field admits any value).
8. **`maudit.at_ms`** — pin its source and status (local clock,
   diagnostic, never authority — the one body timestamp with no
   stated role).
9. **§7.3 walkthrough causality** — after the Q1 fix, restate which
   clause carries acceptance (budgets posture and field-absence each
   suffice; say so, or the "because A and B" reads as conjunctive).
10. **`accept_connect_time` residue** — two removal-notice mentions
    remain; if the freeze sweep is "0 hits", reword to "the v0.4
    flag" (cosmetic).
11. **Ceiling scope vs genesis** — §7.5(c)'s exhaustive list omits
    `c.genesis`; add "the ceiling binds from genesis acceptance
    onward" so the list's exhaustiveness survives pedantry.
12. **Solo `online_lease` grants** — in a witness-less budgets zone a
    lease can never exist (`lease-missing`, pending forever); one
    sentence in §9.1 or B.1 stating this is intended (leases are
    opt-in per grant) closes the last time-lane ambiguity.

---

## Part 3 — Gate-A readiness

The spec now passes five of the six v0.4 closure tests outright;
the failures are localized: **every valid genesis boots** (test 1
passes; Q3/Q5 gnaw at enrolled-device parity, not boot), **proofs
verify and converge** (test 2 passes except the unstated budgets lane
— Q1), **cutoffs name their domains and revocation covers every zone**
(test 3 passes on trusted planes; Q2/Q4 break exactly the hosted
lane), **relations compare the intended principal** (test 4 passes;
Q8 is a vocabulary pin), **transfer/erase reconstruct identically
after any crash** (test 5 passes except the failure arm — Q6), and
the conformance artifacts are clean.

**Recommendation.** Cut v0.5.1 with Q1–Q9 and the pins — roughly a
day: two owner ratifications (Q1's advisory-only sentence; Q7's
fix-or-state choice), two small wire additions that must precede any
corpus bytes (`ccutoff.requester` + the `cutoff` tag; the transfer
terminal record), and the rest sentence-level. Do not reopen anything
else — the D-48..D-55 architecture held up under this walk everywhere
except where a ruling's mechanism didn't get its bytes (Q1, Q2) or a
pass-A/C edit didn't propagate into the hosted ceiling's exhaustive
list (Q3, Q4). Then freeze prose edits, build `owner-plane-core` and
the corpus, run the families on their named surfaces, and let the
prose↔vector discrepancy audit decide Gate A. Durable P1 writes stay
prohibited until Gate B plus the umbrella's P0.5/tombed-cutover
prerequisites, unchanged.
