# Review: D0-A Core + Memory normative specification v0.5.15

*2026-07-13. Independent review of
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.15,
4,566 lines / 50,034 words / 378,085 bytes (SHA-256
`94299df5e470bdd7878ef9866fbaa3e2f7cc67031bd5160d0ed3061dfe648e73`).
The reviewed predecessor is the archived v0.5.14 source (SHA-256
`e87bdbdee1406e33d6bc1c604fedc54c876509935d526e982e9bce24b9a833ed`);
the delta is 250 insertions and 92 deletions. I used the v0.5.14 synthesized
review (SHA-256
`960f40f5fc44439c659d5a29d32c541c96cde1dcf8afef74476214515d9fa3ba`)
as the incoming disposition ledger. No v0.5.15 peer report was consulted.
Findings below were re-derived against the normative prose, CDDL, decision
records, and required-vector inventory.*

## Executive verdict

**Cut v0.5.16. Do not freeze v0.5.15 or begin the normative companion and
independent reducer from this text.**

v0.5.15 is a useful revision. Several of its decisions are exactly the right
ones:

- late acceptance of a staged frontier now expressly causes a suffix re-fold;
- retirement coverage no longer depends on strict-zone policy;
- every still-capable earlier import claimant reserves ownership freeze;
- source-derived import equality is retained rather than silently delegating
  content choice to the export signer;
- the journal now attempts to carry an evaluation coordinate and a compound
  collision cause;
- adopted-renewal chains include signing-only links and name the 64-link
  residual;
- the four closure axes appear together in the main authority equation and
  validation pipeline; and
- direct P-256 reuse across signing and KEM roles, plus the held-head Frontier
  transition, are addressed in the main rules.

Keep those rulings. The remaining problem is that their current
representations do not implement them. Eight hard protocol clusters remain:

1. staged-frontier consumption omits renewal from the pending-consumer rule
   and lets an unrelated device renewal consume another lineage's stage;
2. `(winner, freeze_basis)` cannot encode immediate effect finality or a
   multi-boundary finality proof;
3. the source-derived import binding has no durable, portable authenticated
   carrier;
4. `XferAbort.at` orders only control facts, while terminal causes also live
   in tenant and proof feeds, and reopen cannot cite every invalidator;
5. the new revocation domain subtracts an excluding rotation before asking
   `rotation_refs` to cover it, making coverage vacuous;
6. certificate/grant compatibility rejects the ordinary current-renewal plus
   inherited-grant case it must preserve;
7. no rule relates recovery's opaque, role-tagged `retired_keys` to the new
   role-neutral `mat_id`; and
8. the core Frontier definition still mandates the exact-head transition
   D-175 replaced.

E10 is also still false across these machines: several new pending,
displacement, binding, continuation, and journal states have no exact closed
outcome and disposition.

These are not demands for more examples around otherwise complete law. They
change which control operation accepts, which certificate may author, whether
a revocation ever completes, whether an erased import can be validated, and
whether a journal can be rebuilt from its bytes. The source itself correctly
keeps Gate A false
([status](/Users/vm/owner-plane-d0a-spec.md:3616)). The companion and reducer
should not be asked to invent the missing answers.

## Disposition ledger

| Topic | What v0.5.15 genuinely closes | Remaining disposition |
|---|---|---|
| D-168 staged frontiers | Retirement strictness is repaired; late carrier acceptance expressly re-folds the intervening suffix | **Hard reducer blocker.** Renewal is omitted from dependency reservation, and generic “next renewal” consumption is selector-ill-typed |
| D-169 import reservation | Pending and revivably quarantined earlier claimants both reserve freeze | **Hard schema/reducer blocker.** Freeze is a predicate, not one op hash; immediate and multi-boundary finality are unencodable |
| D-170 source binding | The stronger source-derived-equality posture is chosen | **Hard security/portability blocker.** No frame, operation, or journal record carries the stored binding |
| D-171 journal coordinate | `XferAbort` gains `at`; several missing cause-map cases are added | **Hard journal blocker.** A control head cannot snapshot tenant/proof facts; invalidators and causes are still not total |
| D-172 adoption | Chain membership, terminal overlap intent, and the deep-chain residual are substantially repaired | **Mostly closed.** Clean up KEM-only mirror text and define overlap through the final role-neutral identity rule |
| D-173 revocation | The intended evaluation point and re-admission behavior are named | **Hard semantic/security blocker.** Accepted excluding rotations remove their own zones from the set their references are advertised to prove |
| D-174 authority | Recovery is restored to the main equation | **Hard functional/security blocker.** The compatibility direction strands inherited grants and the known-failure lifecycle is wrong |
| D-175 material identity | Direct same-point cross-role reuse and intra-certificate reuse are rejected | **Hard portability blocker.** Recovery carries only role-tagged hashes and defines no cross-role retired-key match |
| D-175 Frontier | §9.3 gives the right accepted-predecessor transition | **Hard normative contradiction.** §4.6 and older decision text still require exact named/terminal-head retirement |
| E10 / canonical shapes | Several vector names were added | **Gate blocker.** Outcomes, fact-reference types, set order, and caps remain incomplete |

## Freeze blockers

### B1. D-168 still has no well-typed staged-frontier consumer relation

The automatic rule says the zone's next epoch advance, renewal, or retirement
consumes every accepted stage and materializes it under that consumer's
selector
([consumption](/Users/vm/owner-plane-d0a-spec.md:1731)). D-168's dependency
rule then enumerates only a strict-zone advance or any space retirement
([stage machine](/Users/vm/owner-plane-d0a-spec.md:1749)); the CDDL repeats
that set
([stage CDDL](/Users/vm/owner-plane-d0a-spec.md:4065)).

Renewal is missing. Its history coverage is mandatory over the renewed
device's authorship domain, and a device with more than 64 covered zones is
explicitly expected to rely on staged frontiers
([renewal registry](/Users/vm/owner-plane-d0a-spec.md:1395)). A direct trace is:

1. D has authored in 65 zones.
2. Earlier carrier P stages the 65th renewal frontier but remains
   `ref-unresolved`.
3. Renewal R carries 64 inline frontiers.
4. Under D-168's exhaustive-looking pending set, R does not wait and fails
   coverage.
5. P resolves. A cold fold sees P before R and accepts R using its stage.

The late-carrier re-fold implies R should later re-derive, but the registry's
reject-permanent disposition and D-168's exhaustive-looking pending set do not
authorize that revival or name its lifecycle. A fresh reducer accepts R; an
incremental reducer has two conflicting instructions for getting there.

There is a deeper type error even when every carrier is already accepted.
Let lineages A and B both be live in zone Z. Stage P carries A's frontier.
If B's certificate renewal is the next generic “renewal” in Z, the automatic
rule consumes P and materializes it with B's predecessor-certificate selector.
That selector does not govern A's operations, so the resulting closure is
inert or invalid; either way P has been burned before the strict zone advance
that needed it.

Repair the relation, not just the enumeration. A stage needs a deterministic
**next applicable consumer** derived from `(zone, lineage, boundary purpose,
consumer selector)`:

- a zone advance or space retirement may consume all applicable live-lineage
  stages in that zone;
- a renewal may consume only stages in the renewed predecessor's authorship
  domain and selector scope; and
- every required-coverage consumer, including renewal, waits behind an
  earlier held carrier that could supply its coverage.

Then name the exact pending outcome—most naturally `ref-unresolved`—and pin
accepted, pending, rejected, vacuous, unrelated-renewal, and late-resolution
traces. “Bounded suffix” should mean the control interval through the first
applicable consumer, not an implementation-dependent search horizon.

### B2. D-169 turns a derived freeze predicate into one nonexistent hash

Widening the reservation set is correct. The new terminal representation is
not. A collision cause is now the “typed conjunction”
`(winner, freeze_basis)`
([cause map](/Users/vm/owner-plane-d0a-spec.md:1271)), encoded as an untagged
`[+ bytes32]`
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4268)). No normative function defines
`freeze_basis`.

The common case is already unencodable. A plane with no open unknown gap is
effect-final immediately
([effect finality](/Users/vm/owner-plane-d0a-spec.md:437)). An initial-generation
import A therefore freezes without any distinct boundary operation. A later
claim B is an `import-collision`, but there is no second hash to place beside
A. Repeating A would collapse the two logical roles and violate set duplicate
freedom.

The nontrivial case needs more than one hash. Effect finality requires every
lower generation to be immutably closed. With several lower generations,
different incorporations or seals can jointly establish that predicate.
Removing any necessary support can unfreeze the winner. The intended
two-member set has room for the winner and only one support, so an uncited
support can die without invalidating the terminal. Alternative sufficient
support sets make “the” basis even less well defined.

There is an even more basic reachability question. A frozen import winner is
already effect-final, so the replay record appears complete; rejection of a
duplicate loser should not put that source record in `XferAbort.missing`.
Nevertheless, the required-vector inventory expressly demands a collision
Abort and reopen
([vector](/Users/vm/owner-plane-d0a-spec.md:3290)). Before adding more wire,
the specification must supply a reachable trace in which a frozen winner does
not satisfy transfer completion, or remove `import-collision` from terminal
causes and delete that vector.

If collision remains terminal-relevant, use a versioned typed freeze
certificate rather than raw hashes. At minimum it needs distinct arms for:

- immediate/structural finality;
- effect finality with a canonical sufficient support proof; and
- a matching authority-ending frontier.

The collision cause can then be `{ winner, freeze_cert }`, and invalidation can
ask whether that represented predicate still holds. The shape also needs an
E7 ordering rule and an E8 cap. D-169 says “≤ 2” only in the decision record;
the CDDL array is unbounded and the E8 inventory contains no cap.

Two adjacent exactness issues should be closed in the same pass. “Only
reject-permanent or frozen-out claimants release the reservation” omits the
specification's permanently non-revivable quarantine class, and `frozen-out`
is not a closed outcome. Ordinary order displacement and judgment/pin/erase
waiting on a provisional owner still name only dispositions, not E10 outcomes
([ownership](/Users/vm/owner-plane-d0a-spec.md:2760)).

### B3. D-170's source binding exists only in prose

The selected security posture is sound: while the source exists, the intended
destination-registration step derives a leaf hash from the real source record;
after erasure, validation should use that same binding rather than trust a
substituted export leaf
([validator](/Users/vm/owner-plane-d0a-spec.md:2606)).

No normative durable object carries it:

- the frame and `txnrec` unions are closed and have no attempt-registration
  member
  ([storage CDDL](/Users/vm/owner-plane-d0a-spec.md:4256));
- `pendingxfer` carries only transfer-level identity, destination, digest, and
  count
  ([pending transfer](/Users/vm/owner-plane-d0a-spec.md:4259));
- `mimport` carries importer-controlled `rec` bytes and its Merkle proof, but
  no independently verified source binding
  ([import CDDL](/Users/vm/owner-plane-d0a-spec.md:4404));
- bundles are expressly not persisted
  ([bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:2644)); and
- the local index is rebuildable, so it cannot be the sole carrier of
  acceptance history.

The v0.5.14 counterexample therefore survives a cold rebuild—and would survive
later replica transfer unless D0-B carries additional proof:

1. malicious B′ is durable but waits on time proof; replica R has observed
   the real source B and records its hash only in unspecified local state;
2. B is erased;
3. R crashes, or a new replica rebuilds from protocol bytes; and
4. the new reducer holds B′ and its valid Merkle path, but neither B nor R's
   hidden binding.

Trusting the signed leaf restores the substitution vulnerability. Waiting
forever diverges from R and can also prevent `XferAbort`; rejecting needs a
rule and outcome not present in the text. A compromised writer may also
replay already-carried `mimport` bytes without ever passing the claimed local
registration ritual.

Add a versioned, durable, portable attempt-registration record whose binding is
authenticated by the verifier/service that performed source equality, under a
named authority the importer cannot mint through `m.import` alone. Its bytes
must let a fresh reducer verify that authority. Merely persisting a local
plane-writer assertion—or adding an importer-asserted hash to `mimport`—does
not prove the check occurred. Define crash atomicity, replay identity,
ingest handling for already-carried `mimport` bytes, source-erasure precedence,
and exact outcomes for missing and mismatching bindings. D0-A must define the
bytes and cold-rebuild law; D0-B must later distribute those same bytes rather
than invent the proof. The simpler alternative is erasure-wins: every
not-yet-accepted import resolves negative once its source disappears.

### B4. D-171's control head cannot reconstruct cross-feed history

`XferAbort.at` is defined as a control-operation hash
([terminal CDDL](/Users/vm/owner-plane-d0a-spec.md:4282)). Terminal causes,
however, include tenant operations and proof-dependent retroactivity as well
as control boundaries
([cause universe](/Users/vm/owner-plane-d0a-spec.md:1252)). There is no total
ordering from those domains into a control head:

- ordinary tenant operations carry no authoritative control frontier;
- their HLC is chronology-only; and
- receipt and lease feeds have independent issuer sequences; receipts carry
  no control coordinate, and the lease's `ctrl_frontier` is explicitly
  diagnostic
  ([proof shape](/Users/vm/owner-plane-d0a-spec.md:679)).

Suppose Abort T is written at control head C with sufficient control cause S.
Later, a held `cap_eligible w.gen` W arrives and independently makes the same
record negative, with `H(W) < H(S)`. W was signed under an already-open epoch;
nothing in its bytes says whether it was held before or after C. A cold rebuild
with C, S, and W cannot apply “facts at or before C” consistently. Counting W
changes the mandated minimal basis; excluding it has no carried justification.

The journal has two further wire problems:

- `XferReopen.invalidation` is specified as an **operation hash**. Direct
  issuer-fork discovery can retro-quarantine a winner, but the killing fact is
  a receipt/lease statement identified by `stmt_id`, not an operation.
- The claimed total cause map still has no representation for static scope
  mismatch, a never-issued grant, request fork, class exclusion, or the
  immutable fact that turns a budget/lineage outcome permanently negative.

The main prose and CDDL also disagree on the new field: the terminal literal
still defines `XferAbort { export_id, release_op, reason, missing }`
([main literal](/Users/vm/owner-plane-d0a-spec.md:1235)), while CDDL makes `at`
mandatory. Mandatory `at` also gives basis-free `source-erased` and intrinsic
Aborts a removable control-branch dependency they do not otherwise need. The
text never says what happens when C3′ cuts that `at` position.

The smallest robust repair is to abandon historical minimum selection. Permit
one typed, currently verifiable sufficient cause certificate carried by the
single writer; if that chosen certificate later invalidates, reopen and
re-derive even if another sufficient fact has since appeared. If historical
canonicality is essential, `at` must commit the relevant **held fact set**—not
only accepted Frontiers—including held eligible caps, revivably quarantined
claimants, tenant-feed positions, and proof-feed branch/fork state. A control
hash alone cannot do it.

In either design, use a tagged `factref` union (`op`, `statement`, and any
derived structural arm), totalize the cause relation by rejecting context
rather than outcome label alone, and specify terminal-first delivery. An
unheld `at`, basis, or invalidation must have an exact outcome and must reserve
the interval so imports cannot continue behind a pending terminal. Wrong
incarnation, double terminal, and reopen-after-Done should explicitly map to
`log-corrupt`/storage-quarantine rather than naming only the disposition.
Vector terminal-first arrival, a losing-branch/C3′ removal of `at`, and every
basis-free terminal arm.

### B5. D-173 subtracts the evidence before testing coverage

The new decryptable-wrap domain contains a zone only while the target has an
effective wrap **not already followed by an accepted rotation excluding it**
([revocation registry](/Users/vm/owner-plane-d0a-spec.md:1396)). Yet
`rotation_refs` can cite only separately accepted excluding rotations, and the
compound completes when those references cover the domain.

The reducer can deterministically treat the shrinking domain itself as the
completion test, but then the references do not prove the advertised
coverage:

1. D holds an effective wrap in Z.
2. Rotation R excluding D is accepted, as rotation-first requires.
3. At the later revoke V, R has already followed D's wrap, so Z is absent from
   D-173's domain.
4. Under the main registry rule, V can therefore satisfy wrap coverage
   without R contributing to that coverage.

Conversely, if Z remains in the domain, no already-accepted excluding rotation
exists to reference—accepting one removes Z before the coverage test. Thus
`rotation_refs` are non-operative for rotation-first coverage. The
0-author/65-wrap vector degenerates to an already-empty domain rather than
exercising its advertised continuation proof.

The hosted ceiling separately requires a `c.revoke_device` **naming R** before
that rotation's freeze lifts
([hosted rule](/Users/vm/owner-plane-d0a-spec.md:1943)). On that lane R remains
mandatory but redundant to D-173's coverage test. The trusted-lane registry
does not say equally clearly whether target-level completion consumes every
accepted exclusion or only named ones. The same fields therefore look like
proof on one surface, audit linkage on another, and potentially no-op
coverage on the main equation.

Choose one law explicitly. Coherent options include removing or repurposing
`rotation_refs` and defining target-level completion entirely from the
shrinking state; requiring the completing revoke to consume every accepted
target exclusion; or modeling an outstanding exclusion obligation keyed by
`(revocation_id, zone, rotation)` that references actually discharge. Under
the latter two choices, a later `wrap_add` creates a new obligation rather
than rewriting the history used to validate an earlier reference. Define the
exact snapshot for main plus continuations and the outcome while the finite
ceremony is incomplete.

Finally, “decryptable” currently conflates control and local storage state.
Rotation acceptance only authorizes; a durable local Fence activates the new
epoch, and the old epoch remains served until then
([I3](/Users/vm/owner-plane-d0a-spec.md:972);
[rotation machine](/Users/vm/owner-plane-d0a-spec.md:1003)). Keep early
control exclusion if desired, but name two product/protocol states:
authority revocation complete versus per-replica cryptographic exclusion
complete. Add an accepted-but-unFenced trace and avoid promising the latter
from control bytes alone.

### B6. D-174 blocks the normal post-renewal write

The new compatibility rule admits `(certificate C, grant G)` only if G was
issued during C's effective span **or** C holds a supersede frontier covering
the operation
([authority rule](/Users/vm/owner-plane-d0a-spec.md:1654)).

Apply it to the intended ordinary case:

1. Enroll C0 with live grant G0.
2. Renew C0 to current certificate C1; G0 deliberately survives because
   grants bind `device_id`.
3. C1 authors under inherited G0.

G0 was issued before C1's effective span. Current C1 is not the certificate
selected by a supersede frontier, and a new C1 operation lies beyond C0's
history boundary in any event. Both arms fail, yielding `cert-superseded`.
This directly contradicts “renewal never orphans [grants]”
([renewal rule](/Users/vm/owner-plane-d0a-spec.md:374)).

The relation needs an upper bound, not a lower bound: reject when G was issued
strictly **after C ceased being effective**. That admits grants issued before
or during the current certificate's span—including same-enrollment grants and
grants inherited across renewal—while rejecting the motivating old-C0/new-G1
attack. The independent closure equation continues to constrain the
operation's tenant position; the compatibility `OR` need not duplicate it.

Two mirrors/lifecycles also remain wrong:

- §4.2 still labels certificate ∩ grant ∩ epoch as the authority predicate,
  omitting recovery
  ([mirror](/Users/vm/owner-plane-d0a-spec.md:351)), although §7.1 and §10.2
  correctly show four axes.
- `cert-superseded` is only pending-dependency “awaiting renewal chain”
  ([disposition](/Users/vm/owner-plane-d0a-spec.md:2338)). Missing chain
  evidence may pend; a fully proven old-cert/post-supersession-grant relation
  is permanent within the branch and should not wait forever under the same
  lifecycle.

Vector C0/G0 → C1/G0 admission, co-issued C/G admission, and old C0/new G1
rejection in both covered and legitimately omitted zones. Give missing
renewal evidence and proven incompatibility distinct exact outcomes or
contextual dispositions.

### B7. D-175 does not make role-neutral retirement portable

`mat_id = H_mat(SEC1 point bytes)` is the right direct comparison for the two
P-256 roles
([freshness rule](/Users/vm/owner-plane-d0a-spec.md:845)). Surviving and adopted
certificates carry public material, so a reducer can compute it there.
Recovery's portable burn set still carries only:

```
retired_keys: [* bytes32]
```

and defines those bytes as role-tagged `key_id` values
([recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:4179)). A hash does not reveal
the SEC1 point from which a replica could derive `mat_id`.

Trace:

1. only replica A saw cut-branch KEM point P;
2. recovery carries `H_key({"hpke-p256-v1", P})` in `retired_keys`;
3. a fresh replica later evaluates a certificate proposing P as a `p256`
   signing key.

Its candidate `key_id` differs from the retired value. A v1 reducer *could*
compute both closed P-256 role-tagged IDs from candidate point P, but the
specification never mandates that alternate-tag match. A literal implementation
therefore needs the cut certificate—exactly the arrival-history dependency
`retired_keys` was introduced to remove.

Either carry typed public material from which both IDs are derived, explicitly
define alternate-tag enumeration for v1 P-256 candidates, or carry both hashes
as recovery-authority assertions while acknowledging that their relationship
cannot be checked without P. Extend the D-172 retired/terminal-adopted overlap
through that same relation and vector both replicas, not only direct same-role
overlap.

One crypto posture deserves an explicit sentence. Exact SEC1 bytes distinguish
P from −P, although a holder of the scalar can derive its negation. If the
identity is intentionally point-byte equality, state that limit. If it is
intended to mean equivalent P-256 secret control, canonicalize the equivalence
class for this specific negation relation—otherwise “same material” overclaims
what `H_mat` detects. No public identifier can detect arbitrary related-key
derivation, so state that boundary too.

### B8. D-175's Frontier transition still has two normative definitions

The §9.3 repair is directionally correct: when `last_known` names a held but
budget-displaced head, acceptance retires the effective accepted head at or
below that position, with a defined no-op if none exists
([new transition](/Users/vm/owner-plane-d0a-spec.md:2092)).

The core Frontier definition still says `last_known` names a terminal head and
the Frontier drops exactly the incorporated/named head
([Frontier](/Users/vm/owner-plane-d0a-spec.md:628)). D-76 in the decision ledger
retains the same accepted-and-terminal exact-head rule
([D-76](/Users/vm/owner-plane-d0a-spec.md:3484)). An implementer following
§4.6 therefore reproduces v0.5.14's no-op on a displaced named head, while one
following §9.3 removes its accepted predecessor. D-76 is an amended historical
row rather than a third coequal rule, but its stale summary makes the live
§4.6 contradiction easier to implement accidentally.

Sweep the old rule from §4.6, effect-finality terminology, the decision
amendment trail, and every CDDL comment that claims exact-name retirement.
The vector should cover the full derived lifecycle: W accepts and retires the
predecessor; a later order-earlier budget consumer displaces W and restores
the predecessor; retro-disqualification of that displacing consumer releases
its charge, revives W, and retires the predecessor again. The
held-bytes incorporation cap may remain while the accepted Frontier effect
reverses—those are deliberately different strata and should be tested as such.

## D-172 and other repairs to retain

D-172 is no longer a composed blocker on its own. Main recovery prose now
requires all renewal links, including signing-only intermediates; the CDDL and
vector name terminal-key/retired-key overlap; and the >64-link orphaning
residual is explicit. Keep those decisions.

Two mirror cleanups remain. The CDDL introduction still calls every entry a
“KEM RENEWAL” before later requiring signing-only members
([adoption CDDL](/Users/vm/owner-plane-d0a-spec.md:4149)), and the overlap rule
is absent from the main §7.4 transition prose. Rewrite the introduction as a
selected renewal chain whose KEM-rotating members additionally preserve wraps,
then define overlap with the repaired role-neutral retirement relation from
B7.

Also retain:

- D-168's explicit late-carrier suffix re-fold and strictness-independent
  retirement;
- D-169's “every still-capable earlier claimant reserves” rule;
- D-170's refusal to trust substituted export-leaf content;
- D-171's totalization intent and citable-reopen direction;
- D-174's four-axis equation on the main reducer surfaces; and
- D-175's direct cross-role and intra-certificate checks plus the §9.3
  accepted-predecessor transition.

## E10 and schema closure pass

The v0.5.14 synthesis asked for an E10 sweep. It has not happened. Before the
next review, explicitly map at least:

1. a staged consumer waiting on a carrier;
2. ordinary import displacement;
3. a judgment/pin/erase waiting on provisional ownership;
4. missing versus mismatching stored source binding;
5. an incomplete main/continuation revocation compound;
6. unheld `XferAbort.at`, cause members, and reopen invalidation;
7. known cert/grant incompatibility versus missing renewal evidence;
8. wrong journal incarnation, double terminal, and reopen-after-Done;
9. permanently non-revivable quarantine in the freeze-reservation rule.

Reuse existing outcomes where their lifecycle truly matches; new names are
not the goal. The goal is one exact result on every surface. At the same time,
replace untyped journal hashes with tagged fact references, give every logical
set its E7 key/sort/duplicate rule, and put every semantic bound—including
collision-cause cardinality—in E8 and CDDL rather than only a decision row.

## v0.5.16 closure checklist

Before another freeze review, prose, CDDL, outcomes, decisions, and vector
inventory should agree on:

1. the next **applicable** staged-frontier consumer, including renewal and
   selector scope;
2. a reachable collision-terminal trace and typed freeze certificate for
   immediate, multi-boundary, and authority-frontier finality—or removal of
   the dead terminal cause;
3. a durable authenticated source-binding carrier, its cold-rebuild law, and
   its D0-B distribution boundary;
4. either a complete multi-feed journal snapshot or a nonhistorical citable
   cause design, plus tagged invalidators;
5. one coherent role for `rotation_refs`—state-derived audit linkage or an
   exclusion obligation they actually discharge—with control-versus-Fence
   completion named;
6. upper-bound certificate/grant temporal compatibility and split known versus
   missing-evidence lifecycles;
7. portable role-neutral retirement material; and
8. one Frontier transition on every normative surface.

Run the E10/E7/E8 sweep as part of those edits, not as a later artifact pass.
Then add the counterexamples above to the companion's opening tranche.

## Artifact sequence

The artifact order remains:

1. ratify and propagate the eight protocol repairs;
2. write their counterexample fixtures into the companion schema;
3. build the independent reducer and differential harness;
4. generate and execute families 1–13;
5. perform family 14 offline confirmation; and
6. run the final prose↔CDDL↔vector discrepancy audit.

Non-normative fixtures can be drafted now. The schema and reducer should not
silently choose the missing protocol law.

## Final assessment

v0.5.15 continues to improve the design's intellectual shape. In particular,
it recognizes that late facts need explicit re-folds, ownership freeze must
respect every revivable claimant, source equality must survive erasure, and
cross-role key identity cannot be algorithm-tagged. Those are meaningful
advances.

The revision is nevertheless not a freeze candidate. Its largest regressions
are crisp: a current renewed certificate loses its inherited grants, and an
excluding rotation removes itself from the very coverage proof meant to bind
it. The source-binding and journal repairs still rely on state that their wire
formats do not carry. A reducer built now would necessarily make incompatible
choices.

**Final decision: no-go for freeze, normative companion, or independent core;
cut v0.5.16 and review again.**
