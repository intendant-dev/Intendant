# Synthesized review: D0-A Core + Memory normative specification v0.5.12

*2026-07-13. Adjudicated synthesis of
[`owner-plane-d0a-spec-v0.5.12-review.md`](/Users/vm/owner-plane-d0a-spec-v0.5.12-review.md)
(SHA-256
`8822d23a9f437f713a1325b4dbb372b6b24380dc4870975cad9d251349250316`)
and
[`owner-plane-d0a-spec-v0.5.12-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.5.12-review-2.md)
(SHA-256
`4a4478cea7e88a97decf1993a9ec1af388f4c44902858b2236a6f7507afa3c9f`),
verified against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.12
(SHA-256
`01cd50f8fba3df806be0e73bd19b6ad886e57146beb94dc728e2f2af788436c9`).
This synthesis adjudicates evidence and countertraces; it does not average the
two verdicts.*

## Executive verdict

**Cut v0.5.13. Do not freeze v0.5.12 or use it as the companion/corpus
baseline.**

The peer review is right about the revision's architectural progress. D-143's
frontier-shaped, selector-qualified authority closures fix both halves of the
v0.5.11 scalar failure. D-144 chooses the correct cap anchor. D-146 honestly
withdraws the false one-grant identity proof. D-147 recognizes that a flat,
unretained bundle cannot support partial-import rebuild. D-148 adds the right
journal concepts: per-record cause, reopen intervals, and incarnation-scoped
effects. D-149 correctly makes permanence a property of the rejecting fact.
D-150 closes the direct cross-device and K0→K1→K0 reuse attacks. D-151 lands
the dead-arm cleanup, mirror negatives, requester no-op rejection, live-lineage
definition, and explicit Gate-A-false status.

Those are substantive closures and should be preserved. The peer review's
“findings: none” conclusion
([peer verdict](/Users/vm/owner-plane-d0a-spec-v0.5.12-review-2.md:55)) does not,
however, survive composed-rule checking. Seven freeze-blocking clusters remain:

1. the legal unknown-gap maximum needs 65 live heads while
   `frontierclose.heads` permits 64;
2. staged closure consumption is unbound, and already materialized prior-
   advance entries are incorrectly allowed to satisfy a later advance;
3. D-145's generation reservation does not cover a later generation whose
   `w.gen(last_known=…)` depends on the unresolved branch;
4. D-146's frozen import owner is incomplete at grant handoff and is not a
   portable fact across C3′;
5. D-147's record path does not generally authenticate the expected release
   header/source layout;
6. D-148's journal cause contract cannot represent all of D-149's branch-
   relative rejections; and
7. an adopted renewal's certificate keys are outside the portable freshness
   domain, allowing the exact cross-device KEM reuse D-150 intends to forbid.

The adoption-precedence omission, renewal-adoption graph, device-revocation
coverage, renewal-after-revocation formula, recovery continuation, D-144
evaluation stratum, and disposition/state-machine residues are also
freeze-relevant. Some are small textual or schema repairs, but fixtures must
not be allowed to invent their answers.

Recommended disposition:

- **Architecture:** accept.
- **D-143/D-144 core decisions:** accept, with boundedness, staging,
  dependency, and ordering repairs.
- **D-146–D-150 direction:** accept; current state machines are not
  freeze-ready.
- **D-151 hygiene:** accept.
- **Peer recommendation to begin artifacts now:** reject.
- **Next step after v0.5.13 closes the rules:** the peer's proposed artifact
  order is right—companion first, then independent core/harness, corpus,
  surfaces, and final discrepancy audit.

## Assessment of the peer review

### What it did well

The peer review is independent, concise, and candid about its own scope. It
explicitly says its composed-trace depth is thinner and leaves the freeze
decision to synthesis
([scope note](/Users/vm/owner-plane-d0a-spec-v0.5.12-review-2.md:7)). It
correctly confirms that:

- D-143 removes authority closures from `immutable_cap` and gives them typed
  selectors;
- D-146 and D-147 are honest reversals of recent but insufficient mechanisms;
- the new Merkle tags, `mimport` fields, per-record `basis`, `XferReopen`, and
  `incarnation` exist on the wire;
- D-149's branch-relative permanence split is present in §10.5;
- the dead `mclaim.provenance.import` arm is structurally gone; and
- Gate A is explicitly false pending artifacts.

Its process observation is fair: willingness to reverse D-139 and the flat
bundle is healthy protocol governance. Its proposed artifact order is also the
right order once the text is determinate.

### Why the all-clear does not control the synthesis

The report mainly establishes **representation presence**: “CDDL verified,”
“wire verified,” tag inventory present, vector names present
([peer findings](/Users/vm/owner-plane-d0a-spec-v0.5.12-review-2.md:14)). It does
not show a reducer trace or a normative formula closing the interactions below.
A field can exist while its lifecycle remains non-portable; a hash path can
reach a root without proving the root's required relation to other signed
fields; and a future-vector name is not an executable rule—especially while
the corpus does not exist.

The missed classes map directly to that methodology:

- **bound versus representation:** the full-frontier type exists, but its cap
  is one below the maximum legal live set;
- **carrier versus consumer:** a staged close exists, but no later operation
  says which stage it consumes or under which one-use rule;
- **local scope versus dependency graph:** D-145 covers same-generation
  descendants, not `last_known` dependencies into later generations;
- **derived transition versus portable fact:** “frozen at effect finality” has
  no durable carrier across C3′;
- **membership versus relation proof:** a record path reaches the signed root
  without generally proving the expected header occupied leaf 0;
- **field versus closed cause vocabulary:** `basis: bytes32` exists, but the
  protocol does not say how scope rejection or a provisional tenant claimant
  maps into it; and
- **adoption versus identity history:** a renewed certificate survives as
  storage state but is absent from the key-freshness set.

The peer's positive observations should therefore be retained, while its
empty severity ledger and recommendation to proceed directly to artifacts
should be rejected.

## Adjudicated disposition ledger

| Topic | Peer assessment | Synthesized assessment |
|---|---|---|
| D-143 authority selector and frontier shape | Complete | **Core fixed** |
| Full-frontier bound | No finding | **Blocker:** 64 legal unknown openings can leave 65 live heads |
| Staged close materialization | Complete | **Blocker:** no consumer/one-use relation; prior advances cannot close a newly stale epoch under their original selector |
| D-145 reservation | Complete | **Partial/blocker:** same-generation fixed; cross-generation `last_known` dependency escapes |
| D-144 cap eligibility | Complete | **Core choice fixed; high exactness:** ordinary `w.gen` still requires terminality without a cap-before-admission stratum |
| D-146 import ownership | Complete | **Blocker:** at-frontier handoff unresolved; frozen state non-portable across C3′ |
| D-147 per-record proof | Complete/erasure-proof | **Blocker:** record membership does not generally prove the release header/layout relation |
| D-148 journal | Complete | **Partial/blocker:** D-149 causes are not closed or validated; interval transition prose still contradicts reopen |
| D-149 permanence split | Complete | **Direction fixed; exact cause/outcome map incomplete** |
| D-150 direct KEM freshness | Complete | **Direct cases fixed** |
| Adopted renewal recovery/key history | Complete | **Blocker:** adopted keys absent from freshness; precedence/graph exactness remains high |
| Device-revocation coverage | No finding | **High:** wrap zones and authored zones are different domains |
| Renewal after grant revocation | Complete by composition pins | **High:** assertion/vector names without a cert×grant reducer formula |
| Recovery omission | Complete | **Pair key fixed; future-pair semantics medium, named-frontier continuation high** |
| D-151 hygiene/mirrors/Gate status | Complete | **Fixed** |
| Artifact readiness | Proceed if synthesis concurs | **Do not proceed yet; fixtures would choose unresolved behavior** |

## Freeze blockers

### B1. The maximum legal lineage has 65 live heads, but the closure wire holds 64

`frontierclose.heads` is capped at 64
([E8](/Users/vm/owner-plane-d0a-spec.md:133),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3579)). The generation rule permits 64
accepted unknown openings and rejects the 65th
([§9.3](/Users/vm/owner-plane-d0a-spec.md:1913)). Starting with H1, opening
g2…g65 as unknown preserves 64 prior heads and creates the current g65 head:
65 live heads. Reauthorization makes the trace reachable across generation
windows.

Either cap **total live heads** at 64 or raise every full-frontier carrier to
65. The peer verified the shape but did not reconcile the caps.

### B2. A prior advance's frontier cannot silently become a later advance's closure

Strict coverage counts `ccutoff.closes` and “prior advances' entries”
([registry](/Users/vm/owner-plane-d0a-spec.md:1310),
[coverage](/Users/vm/owner-plane-d0a-spec.md:1587)). Staged promotion says a
consumer materializes a stage with its own selector, but defines no stage
reference or consumption relation
([promotion](/Users/vm/owner-plane-d0a-spec.md:1602)).

Let H5 close epoch 1 during the epoch-1→2 advance; epoch-2 history then reaches
H9. If H5 counts for epoch-2→3, retaining its original `<2` selector leaves
epoch 2 open, while rematerializing it as `<3` retro-truncates H6…H9. D-143
says selectors derive from the carrying operation, so an already materialized
prior entry cannot simply acquire the later carrier's selector.

Remove implicit prior-advance reuse. Have each consumer reference the exact
stages it consumes (or copy their exact frontiers), and state whether
consumption is one-shot.

### B3. Reservation must follow `w.gen.last_known` across generations

D-145 reserves `(zone,lineage,gen)` only
([lifecycle](/Users/vm/owner-plane-d0a-spec.md:1569)); its vector names the
opposite variant or descendant in that same generation
([vector requirement](/Users/vm/owner-plane-d0a-spec.md:2770)).

Countertrace: g1 currently holds B5; W2 opens g2 with `last_known=B5`; g2
reaches H2. Earlier control C1 names absent fork A5 in g1 and pends. Later C2
names H2 in g2 and is not reserved. A5 arrives; C1 selects A, invalidating B5,
W2, and H2. The incremental fold has committed C2; the fresh control-order fold
cannot validate it.

Reserve the transitive `last_known` dependency cone, or conservatively the
whole `(zone,lineage)` scope while the earlier selector is unresolved.

### B4. Import ownership is neither completed at handoff nor represented across C3′

The peer calls D-94 the structurally right no-late-claimant proof
([peer](/Users/vm/owner-plane-d0a-spec-v0.5.12-review-2.md:24)), but §4.3's
effect-finality list includes eligible incorporation, **close-purpose**
closure, and abandon—not a revoke-purpose frontier
([finality](/Users/vm/owner-plane-d0a-spec.md:423)). D-146 kills old provisional
claims beyond a grant-revoke frontier and preserves effect-final claims, but
does not classify an accepted provisional claimant at or below it
([handoff](/Users/vm/owner-plane-d0a-spec.md:2471)). A successor claimant on a
new lineage is incomparable. The small repair is to say exactly which matching
authority-ending frontiers freeze preserved replay ownership.

The deeper problem is portability. “Frozen at effect finality” is not present
in `mimport`, the journal, or recovery. If I freezes under grant G plus seal S,
then recovery bases after G but before S and later re-admits I, an incremental
replica can remember I as frozen while a fresh surviving-chain fold sees it as
provisional. A late earlier claimant is rejected by one and displaces I on the
other. Local execution dedupe is not portable authority state.

Materialize frozen ownership and define its C3′ adoption/cut semantics, or make
ownership wholly derived and define the consequences of unfreezing. A logical
identity derived from the replay key remains the simpler alternative.

### B5. A record proof reaches the root without generally proving the required header

D-147 puts `H_bhdr({v,export_id,record_count})` at leaf 0 and record hashes
after it
([construction](/Users/vm/owner-plane-d0a-spec.md:2361)). A record import
proves only that `H_brec(record)` plus its sibling path reaches the signed root
([proof](/Users/vm/owner-plane-d0a-spec.md:2368)). For many positions, leaf 0
is inside an opaque sibling subtree. With 129 leaves, the final record promotes
repeatedly and eventually sees one aggregate for leaves 0…127; the verifier
cannot establish that the expected header or source layout was inside it.

A release signature authenticates the writer's assertion; it does not enforce
the body relation against a compromised writer. Thus a full-bundle verifier
can reject a root built under E′ while the advertised proof-only rebuild path
accepts a selected record under that same root.

Redesign the commitment so every record proof binds the release context—an
outer header over a records root plus an ordered-source commitment, context
and ordinal in each leaf, or durable manifest material are possible shapes.
Also pin index origin/range, equality to the sorted source position, proof
order, odd promotion, exact sibling consumption, and failure outcomes. The
depth-8 cap is correct.

### B6. The transfer terminal cannot encode every branch-relative rejecting fact

D-148 defines `basis` through seal/void/eligible-cap boundaries and treats a
basis-free reject-permanent result as intrinsic
([journal](/Users/vm/owner-plane-d0a-spec.md:1199),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3816)). D-149 makes scope, grant,
certificate, ceiling, collision, and control-relative body failures
branch-relative
([dispositions](/Users/vm/owner-plane-d0a-spec.md:2121)).

If import Q rejects `scope-space` under retirement T and recovery writes a
basis-free abort, then C3′ cutting T makes a fresh fold admit Q while the
incremental journal stays terminal. More strongly, `import-collision` can be
caused by another provisional **tenant** claim plus its proof state, not a
control boundary at all.

The scalar shape can work if the RFC defines one canonical sufficient cause;
it need not carry every simultaneous cause. But the cause vocabulary,
validation rule, and invalidation predicate must be closed. Then pin the
interval machine: terminal(n), legal Reopen(n) only after that Abort's cause
invalidates, n+1 thereafter, never reopen Done, and named handling for
duplicates/wrong incarnations. Existing “no terminal,” “never after Abort,”
and “clears on terminal” prose must become current-interval language.

### B7. Adopted renewal keys can be reused under another device

The freshness domain is expressly surviving-chain enrollments plus
`retired_keys`
([T3](/Users/vm/owner-plane-d0a-spec.md:819),
[enrollment](/Users/vm/owner-plane-d0a-spec.md:1302)). An adopted renewal is cut
from that chain and survives only as validation/storage state
([wire](/Users/vm/owner-plane-d0a-spec.md:3716)); its signing and KEM keys do
not automatically join freshness.

D1 can therefore adopt cut K1 as its live storage identity while a later
surviving certificate assigns K1 to D2. D1 still holds the private key and can
decrypt D2's wraps—the direct cross-device bypass D-150 says rejects. Reusing
the adopted signing key similarly defeats device-based self-witness exclusion.

Adopted certificates must contribute typed portable key history automatically:
signing key globally burned, KEM key bound to the adopted device, and an exact
same-device recovery rule. Bare retired IDs cannot preserve that association.

The peer is directionally right that all adoption references should resolve
before precedence: generic prose says so
([adoption](/Users/vm/owner-plane-d0a-spec.md:1720)). Therefore the omission of
`adopted_renewals` from the explicit `admit_ctrl.prec` list is best classified
as **high exactness**, not a second independent blocker. Add it explicitly and
pin wrong-device, wrong-op, not-after-base, and noncontiguous outcomes. Also
define dependency closure through signing-only intermediate renewals.

## High-priority composition and exactness

1. **Device revocation has two coverage domains.** Rotation refs cover zones
   where the device holds current wraps; frontiers must cover every active
   op-authoring zone, including grant-only or later-excluded zones
   ([revocation](/Users/vm/owner-plane-d0a-spec.md:1303)). D-151's claim that
   revoked authority is pre-covered otherwise fails.

2. **Renewal-after-grant-revocation needs a cert×grant formula.** Grant-revoke
   selects the cited grant; renewal supersede selects the predecessor cert;
   certificate resolution runs first
   ([admission](/Users/vm/owner-plane-d0a-spec.md:2023)). §4.2 still preserves
   old-cert history only through renewal cutoffs
   ([certificate](/Users/vm/owner-plane-d0a-spec.md:345)). A vector name cannot
   decide whether the revoked-grant frontier also satisfies cert validity.

3. **Recovery must define future pairs and continuation.** “Universally”
   strongly suggests a lazy default, so the bare universe concern is medium if
   future pairs intentionally begin at `Bounded("none")`; say and vector that.
   More importantly, a named recover frontier is global and immutable, so
   ratify cannot continue that lineage beyond it
   ([recover selector](/Users/vm/owner-plane-d0a-spec.md:1379)). State the
   required new-lineage/re-enrollment ritual or add a continuation operation.

4. **D-144 needs cap-before-ordinary-admission ordering.** Cap eligibility now
   asks chain membership, never terminality
   ([cap rule](/Users/vm/owner-plane-d0a-spec.md:1398)); ordinary `w.gen` still
   requires accepted+terminal with `body-invariant` failure
   ([generation rule](/Users/vm/owner-plane-d0a-spec.md:1889)). Without an
   explicit stratum, H6-first can reject W before W's cap quarantines H6.

5. **The derived revisit and lifecycle maps need closure.** D-146 adds replay-
   owner displacement beyond T2's declared proofs/budgets/boundaries inventory
   ([T2](/Users/vm/owner-plane-d0a-spec.md:700)). Select an outcome for a
   displaced provisional claimant and for references waiting on it; classify
   provisional versus frozen collision; fix the stale `mimport` “identity is
   FINAL” comment
   ([CDDL](/Users/vm/owner-plane-d0a-spec.md:3934)); and express
   `retired_keys` comparisons as `H_key({alg,pk})` membership, not raw-key
   equality.

## Confirmed closures

The following should not be reopened in v0.5.13:

- frontier-shaped authority closures and selector qualification;
- separation of authority closures from `immutable_cap`;
- D-144's chain-membership truncation assertion;
- provisional import ownership as the pre-freeze state;
- per-record durable transfer validation as the goal;
- journal intervals and incarnation-scoped effect keys;
- permanence classified by rejecting fact;
- cross-device and K0→K1→K0 KEM-key rejection;
- portable owner-carried retirement of otherwise cut key IDs;
- structural deletion of the dead import arm;
- requester-empty no-op rejection and the completed mirror-negative inventory;
  and
- the explicit statement that Gate A is currently false.

## Required v0.5.13 vectors before artifacts

Add prose requirements now; encode them in the companion only after the text
is fixed:

1. 64 accepted unknown openings with the current head included in the closure
   bound;
2. stage H5 → advance 1→2 → epoch-2 H6…H9 → advance 2→3, proving exact
   one-use/selector behavior;
3. pending g1 selector against a g2 head depending on the losing g1 branch;
4. provisional import at/below grant-revoke frontier, then new-lineage
   claimant;
5. effect-final owner → C3′ cuts finality basis → re-admission, incremental
   versus fresh fold;
6. 1/2/127/128-record Merkle cases, wrong header, wrong source ordinal,
   extra/missing siblings, and odd-promotion paths;
7. `scope-space` abort then retirement cut, plus provisional-owner collision
   then owner dissolution;
8. recovery arriving before an adopted renewal, signing-only intermediate
   renewal, adopted KEM reuse by a different device, and adopted signing-key
   reuse;
9. device revocation of an authored but currently unwrapped zone;
10. both named renewal-after-revocation compositions;
11. first write on a new post-recovery pair and the continuation ritual for a
    named recover frontier; and
12. W/H6 in both arrival orders with the cap-before-admission stratum explicit.

The peer's artifact sequence becomes appropriate after these choices are in
v0.5.13. Starting it now would turn the companion and fixtures into an
unreviewed second specification—the exact outcome Gate A's final discrepancy
audit is meant to prevent.
