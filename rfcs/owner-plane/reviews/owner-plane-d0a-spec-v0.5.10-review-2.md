# Review 2: D0-A Core + Memory specification v0.5.10

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.10 (3,651 lines, 133 decision rows D-1..D-134), against the
v0.5.9 synthesized review's eight-step sequence, six required owner
rulings, and the v0.5.9 archive (540 changed lines diffed). Written
without reading `owner-plane-d0a-spec-v0.5.9-review.md`. Method: the
raised floor from my overturned v0.5.9 verdict, applied as four
mandatory questions per ruling — three-run convergence, **and**
constructibility (mint the bytes acyclically), **and** authority (who
may cause the convergent transition), **and** lifecycle totality
(missing/removal/revival/permanent/collision). Every discharge claim
below was byte-verified; both findings came from the new floor.*

## Executive verdict

**Both criticals and all five highs from the v0.5.9 synthesis are
discharged, each by the structural repair rather than the patch** —
and then the raised floor catches the same genre one layer down:
D-134's fully-derived import content never reached the `mimport`
CDDL, whose **required** `evidence` key now contradicts the rule's
**absent**, making every v1 import unencodable. One blocking, one
medium, everything else clean.

The eight steps, verified: **D-127** breaks the self-hash cycle the
right way (the bundle preimage is `{v, export_id, recs}` — verified
in the CDDL; the signed release travels alongside; `release_op` is
derived post-signing everywhere it appears; the construct-and-rederive
vector is REQUIRED in family 11). **D-128** replaces signature
possession with a portable `cap_eligible` predicate (admissible under
its own signed anchors ignoring only budget consumption and
deadline/lease receipts — every input a fold of held bytes + control
history; unheld inputs leave the cap `ref-unresolved`; compromise
cutoffs and C3′ dissolve it with re-derivation; escaped finality a
stated residual; cap-bearing bytes GC-exempt, bounded by the
64-open-gap cap). **D-129** totalizes the equation — the boundary
domain `Absent < "none" < Head < Top` with fold-identity inits that
are never wire values (E4 honored), requester entry/snapshot
coherence tightened to **equality** (killing the ignorable-entry and
`"none"`-vs-head contradictions), requesterless `"none"` as the
neutral element, empty `cutoffs` legal iff a requester is present
(the pure-snapshot form — CDDL is `[* ratifycutoff]`, verified), and
my P1 answered: `ratified_through` feeds coverage predicates and
D-111 promotion. **D-130** makes referenced-coordinate forks resolve
by *selection* (first committed boundary in control order — the D-124
pattern extended to tenant forks) and gives promotion the exact
no-widening reduction (per-generation min-fold into `immutable_cap`;
the scalar lex-max rejected for the right reason). **D-131** finishes
the registry state machine: missing ancestry pends, verified
divergence rejects, a pending carrier reserves its scope, fork point
= last common ancestor, plane-wide-forever key freshness (A→B→A
dies), and **symmetric removal** — a C3′ cut dissolving any selector
re-derives exactly what its presence derived. **D-132** totalizes
recovery omission (an omitted cut-branch lineage quarantines entirely
past base — a fail-closed blanket *revivable by ratify growth*, which
is why 256 entries need no continuation against 4,096 heads: entries
are refinements), enrolls `c.lineage_reauth` as a widening event
(revival of held bytes, never mandatory reproposal), and makes
`c.space_retire` position-relative via the D-69 pattern. **D-133**
splits membership (current: latest-accepted-epoch wraps — exclusion
frees the zone and its cap slot; renewal can no longer re-add a
deliberately excluded device) from custody (actual locally-held
old-KEM wraps; absence never obliges a wrap, so Kold drains).
**D-134** resolves import collisions by lowest `op_hash`
(`import-collision` in the outcome enum and disposition map,
verified; both orders + fresh fold converge — an argument that
*depends* on content being fully derived), publishes the per-effect
idempotency keys, pins mirror equality, and keys transfer terminality
on **permanent non-revivability**, never a disposition's name.

---

## Part 1 — Findings

### Blocking

**B1. D-134's fully-derived import content never reached the
`mimport` CDDL — every v1 import is unencodable.** The registry row
and the CDDL's own comment both rule: *"sensitivity = class_floor
exactly, the optional temporal fields absent, `provenance` exactly
the import triple, `labels` and `evidence` ABSENT (annotate
post-import — two valid imports of one record can then differ only in
header identity)."* The structural shape above that comment still
reads `provenance: { ? session, ? project, ? model,
evidence: [* evref], import: {…} }` — **`evidence` is a required
key**. A producer omitting it (per the rule) fails E9 strict decoding
against the closed shape; one including it (even empty) violates
"exactly the import triple" (and E4 makes absent ≠ empty explicit).
This is the v0.5.3 `cenroll.grants` genre precisely — a MUST-be-absent
rule over a required key — and it is load-bearing beyond encodability:
D-134's lowest-op_hash collision rule justifies itself by "derived
content makes the choice semantically invisible," which is only true
if the narrowing is structural. Fix (one CDDL edit): give `mimport`
its own narrowed shape — `provenance: { import: { from_plane,
export_id, release_op, digest } }`, and drop (not comment-forbid) the
temporal/`labels` optionals — so the schema says what the rule means;
the collision and mirror-equality vectors then write themselves.

### Medium

**M1. `cspaceretire` is the only epoch-advancing operation that
cannot carry its own closure coverage.** Its row now says "under
`strict`, closure `cutoffs` per the D-78/D-93 union-coverage rule,
exactly as every epoch-advancing operation," but the body is still
`{ space_id }` — no `? cutoffs` field, unlike `czonepolicy` and
`cepochbump`. A strict-zone retirement (the genesis default posture
is strict) is constructible only by fully pre-staging standalone
cutoffs so the union covers — legal under D-93, but the mandatory
pre-staging is stated nowhere and the "exactly as every" wording
implies parity the wire lacks. Either add the optional `cutoffs`
field (one line, parity) or state the pre-staging-only lane
explicitly and vector it.

### Pins

1. **`mimport`'s comment-forbidden optionals elsewhere**: after B1's
   narrowing, sweep §11.8's prose for the old shape (the "no
   duplicated top-level copies" comment survives from when the shape
   was wide) — one-line hygiene riding the same edit.
2. **`import-collision` displacement telemetry**: the row says an
   accepted loser "re-derives exactly like budget displacement" —
   point it at the standing quarantine review lane the way D-94's
   displacement does, so surfaces don't fork.

---

## Part 2 — Gate-A readiness

The six owner rulings are made and their machinery is real; the two
criticals are dead by construction (I walked the release mint
end-to-end from sources + keys, and the cap-authority negatives all
have portable predicates); the five highs each got the total version
(the domain lattice, selection-not-rejection, scope reservation with
symmetric removal, the revivable recovery blanket, the
three-predicate membership split). The residue is one CDDL shape that
lags its own ruling by an edit and one parity field — the same
closing pattern as every recent round, now at the last wire object
the transfer chain touches.

**Recommendation.** Fold B1 (one shape) + M1 (one field or one
sentence) + the two pins as **v0.5.11 or as the freeze edit itself**;
no owner decision is required — B1's fix is the mechanical
consequence of D-134 as ratified. Then the artifact sequence stands
as the synthesis ordered it: `d0a-vector-cases.v1.json` first — its
opening cases should be the construct-and-rederive trace, the
cap-eligibility negatives, the `Absent`/`"none"`/`Top` lattice table,
the D-130 selection orders, the exclusion/renewal/Kold triple, and
the import-collision pair — then the independent core and harness,
the corpus, family 14, every required surface, and the
prose↔CDDL↔companion↔vector discrepancy audit as the Gate-A decider.
Durable P1 writes remain prohibited until Gate B plus the umbrella's
P0.5/tombed-cutover prerequisites, unchanged.
