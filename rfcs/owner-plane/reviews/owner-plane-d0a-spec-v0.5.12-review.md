# Review: D0-A Core + Memory normative specification v0.5.12

*Independent review, 2026-07-13. Reviewed source:
[`~/owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md), v0.5.12
draft, 4,051 lines / 328,005 bytes, SHA-256
`01cd50f8fba3df806be0e73bd19b6ad886e57146beb94dc728e2f2af788436c9`.
The archived v0.5.11 source is
`e304a5d9e8669a6fcfbdd2a73534d939b2bb5dff7ca838516281cef5d28815f1`;
the v0.5.12 delta is 476 insertions and 233 deletions. No peer v0.5.12
review was consulted.*

## Executive verdict

**Do not freeze v0.5.12. Cut v0.5.13.**

This is another substantial improvement. D-143 is the right repair to the
scalar-closure failure: a closure now carries the whole per-generation
frontier and is qualified by the authority it ends, instead of poisoning an
authority-blind `immutable_cap`. D-144 makes `last_known` a deliberate
truncation assertion and removes the terminality cycle from incorporation-cap
eligibility. D-146 recognizes that import ownership is provisional until no
canonically earlier claimant can arrive. D-147 moves transfer validation in
the right direction with durable per-record material. D-148 adds journal
incarnations and per-record causes. D-149 finally distinguishes intrinsic
failure from branch-relative rejection. D-150 prohibits the cross-device KEM
reuse and K0→K1→K0 cases. D-151 closes several genuine hygiene gaps and,
importantly, now states honestly that Gate A is false.

The remaining failures are mostly interactions among those good decisions,
not reasons to reopen the Owner Plane or Memory architecture. Seven
freeze-blocking traces remain:

1. the legal unknown-gap maximum can produce 65 live heads, while
   `frontierclose` can carry only 64;
2. staged frontiers have neither a consumer binding nor one-use semantics,
   while already materialized advance entries are declared reusable;
3. generation-scoped selector reservation misses a later generation whose
   `w.gen(last_known=…)` depends on the unresolved branch;
4. effect-final import ownership is neither complete at grant handoff nor
   portable across C3′ recovery;
5. the per-record Merkle path does not generally prove the header/source
   commitment that makes the root a valid release root;
6. the transfer journal cannot encode all of D-149's branch-relative causes,
   so an incremental journal and a fresh post-recovery fold can disagree; and
7. adopted KEM renewals are omitted from recovery precedence resolution and
   from the portable key-freshness domain.

There are also several high-priority exactness gaps: device revocation covers
wrapped zones rather than every authored zone; renewal-after-grant-revocation
is asserted but has no executable certificate/grant formula; the recovery
omission universe is still unstated; and D-144's new cap stratum has not been
reconciled with the ordinary `w.gen` terminality rule.

Recommended disposition:

- **Architecture and direction:** accept and preserve.
- **D-143/D-144 core choices:** accept; repair the bounds, staging, dependency,
  and evaluation-order details below.
- **D-146 through D-150:** accept the direction, but do not freeze their current
  state machines or wire contracts.
- **D-151 hygiene fixes:** accept.
- **Protocol/schema freeze:** no.
- **Gate A:** false by the specification's own correct status line, in addition
  to the protocol findings in this review.

## Disposition ledger

| v0.5.11 issue / v0.5.12 decision | v0.5.12 assessment |
|---|---|
| Scalar close could not preserve several live heads | **Core fixed by D-143**, but the 64/65 bound makes the maximum legal frontier unencodable |
| Closure was authority-blind and bricked successor writers | **Fixed by D-143's selector-qualified closure map** |
| Pending selector reserved only one coordinate | **Same-generation case fixed by D-145**; cross-generation `last_known` dependency still escapes |
| Incorporation-cap eligibility was circular | **Substantive decision fixed by D-144**; ordinary `w.gen` evaluation order remains contradictory |
| Import identity was unstable | **Direction improved by D-146**; handoff and C3′ portability remain blockers |
| Flat bundle lost partial-import proof after erasure | **Direction improved by D-147**; the record proof does not yet authenticate the release header/layout |
| Transfer terminal lacked per-record cause and reincarnation | **Direction improved by D-148**; cause vocabulary and transition validation remain incomplete |
| Total C3′ re-fold contradicted unconditional rejection permanence | **Direction fixed by D-149**; the lifecycle map and journal integration remain incomplete |
| KEM reuse and recovery rollback | Cross-device and A→B→A reuse **fixed**; adopted-renewal precedence, dependency closure, and key history remain open |
| Renewal history over revoked grants | D-151 asserts composition and names vectors, but **the cert/grant rule is still undefined** |
| Recovery omission domain | Pair key selected; **the universe and future-pair behavior remain undefined** |
| Dead `mclaim.provenance.import` arm | **Fixed structurally** |
| Empty requester no-op, mirror negatives, Gate status | **Fixed** |

## Blocker 1: `frontierclose` cannot encode the maximum legal live frontier

`frontierclose.heads` is capped at 64
([E8](/Users/vm/owner-plane-d0a-spec.md:133),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3579)). The rationale says the
unknown-gap cap bounds the live heads. The generation rule, however, permits
64 **unknown openings** and rejects the 65th
([§9.3](/Users/vm/owner-plane-d0a-spec.md:1914)). An unknown opening preserves
the prior head and creates a new current head.

Concrete legal trace:

1. g1 has live head H1.
2. Open g2 through g65 with `last_known = "unknown"`: 64 accepted unknown
   openings.
3. None of H1…H64 is retired, and g65 has its current head.
4. The lineage therefore has 65 live heads: 64 gap heads plus the current
   generation head.
5. A strict epoch advance, renewal, device revocation, or space retirement
   must carry the full frontier, but the wire permits only 64 entries.

Reauthorization makes this reachable despite the default eight-generation
window; it does not reset or close old gaps. This is a constructibility
failure at a normative maximum, not merely an encoder inconvenience.

Choose one invariant and pin the boundary vector:

- cap **total live heads** at 64, so the 64th unknown opening rejects when a
  current head would make 65; or
- permit 65 heads in `frontierclose` and every other purported full-frontier
  carrier.

The first choice is conceptually cleaner because the bound then says exactly
what every consumer assumes.

## Blocker 2: staged frontiers have no exact consumption semantics

Strict coverage counts inline entries, accepted `ccutoff.closes`, and even
“prior advances' entries”
([registry](/Users/vm/owner-plane-d0a-spec.md:1310),
[coverage rule](/Users/vm/owner-plane-d0a-spec.md:1587)). A staged frontier is
inert until a consumer materializes it with that consumer's selector
([promotion](/Users/vm/owner-plane-d0a-spec.md:1602),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3648)). But the consuming operation
does not identify a stage operation, and neither the stage nor the consumer
records a consumed-by relation.

Trace:

1. A stage carries H5 for lineage L.
2. The epoch-1→2 advance consumes H5, materializing selector `epoch < 2`.
3. Valid epoch-2 writes extend L through H9.
4. The epoch-2→3 advance has no fresh L entry. The text nevertheless permits
   the prior H5 entry to satisfy coverage.

There are two incompatible readings:

- rematerialize H5 with the new selector `epoch < 3`, which retro-truncates
  legitimate epoch-2 H6…H9; or
- retain H5's original `epoch < 2` selector, in which case epoch 2 is not
  closed even though the strict-coverage check passed.

The same ambiguity exists when several historical staged frontiers share a
`(zone,lineage)` key. “The union covers” says that coverage exists, but not
which bytes become the new closure.

Make staging an explicit transition:

- give each stage a stable identity (its control op hash is sufficient);
- have the consumer carry the selected stage references, or copy the exact
  frontier bytes it is consuming;
- define whether a stage is one-shot; and
- stop treating already materialized prior-advance entries as unused staging
  material.

If deliberate reuse is desired, it still needs an explicit reference in the
new consumer so that the new selector and intended truncation are signed.

## Blocker 3: D-145 reserves a generation, not its cross-generation dependency cone

D-145 correctly widens a pending Head reservation to the whole
`(zone,lineage,gen)` scope
([selector lifecycle](/Users/vm/owner-plane-d0a-spec.md:1569)). That prevents a
same-generation descendant on the other branch from committing first. A later
generation can nevertheless depend on the unresolved generation through
`w.gen.last_known`, whose chain membership is itself re-derived when fork
selection changes
([cap eligibility](/Users/vm/owner-plane-d0a-spec.md:1408),
[`w.gen`](/Users/vm/owner-plane-d0a-spec.md:1909)).

Trace:

1. g1 has conflicting A5 and B5.
2. The replica currently holds B5. It accepts W2 =
   `w.gen(g2, last_known=B5)` and a g2 head H2.
3. Earlier control operation C1 names absent A5 and pends, reserving g1.
4. Later control operation C2 names H2. Because C1 reserves only g1, C2
   commits a boundary in g2.
5. A5 arrives. C1, earlier in control order, selects A. B5 loses; W2 and H2
   lose their `last_known` dependency.

An incremental replica has already committed C2. A fresh control-order fold
processes C1 first and cannot validate C2's H2. D-145's same-generation vector
does not cover this trace.

While the earlier path is unresolved, reserve either:

- the whole `(zone,lineage)` selection scope (simple and bounded); or
- the transitive `w.gen(last_known)` dependency cone, requiring every later
  selector to prove compatibility and to pend when that proof is unavailable.

The lineage-wide reservation is conservative but much easier to specify and
test.

## Blocker 4: D-146 import ownership is not yet a portable state machine

### 4.1 Grant handoff leaves an accepted old claimant unresolved

D-146 says replay-key ownership is provisional until effect finality, and that
grant handoff serializes on the old grant's revoke frontier
([import rule](/Users/vm/owner-plane-d0a-spec.md:2471)). The effect-finality
predicate names eligible `last_known` incorporation, **close-purpose**
closures, and abandonment seals
([§4.3](/Users/vm/owner-plane-d0a-spec.md:423)). It does not name a revoke-
purpose frontier. The handoff prose kills old provisional claims *beyond* the
frontier and preserves effect-final claims, but says nothing about a
provisional claim at or below it.

Trace:

1. Import A claims replay key K under grant G1 while a lower generation is
   open. A is accepted but provisional.
2. `c.revoke_grant(G1)` carries a full frontier preserving A.
3. G2 is issued to a new lineage.
4. Import B claims K under G2.

The full revoke frontier contains enough information to prove that no earlier
G1 claimant can arrive, but the normative finality rule does not consume it.
A remains provisional; B is on an incomparable lineage; “canonically earlier”
has no cross-lineage comparator.

Either every matching immutable authority-ending frontier must freeze replay
ownership for the prefix it preserves, or handoff needs a distinct ownership-
freeze ceremony. State the rule in §4.3 and §11.8, not only in a vector name.

### 4.2 “Frozen at effect finality” is not portable across C3′

The owner carrier is an operation hash, and D-146 says it freezes once
effect-final. There is no signed freeze marker, journal field, or recovery
adoption field for that fact. C3′, meanwhile, requires a fresh fold over the
surviving control chain and re-derives all control-derived state
([recovery](/Users/vm/owner-plane-d0a-spec.md:1653)).

Trace:

1. Replica A observes import I become effect-final because an immutable seal
   closes a lower gap; it freezes I as K's owner.
2. Recovery bases after I's grant but before the seal, cutting the seal. It
   omits the pair under the revivable blanket, then a later ratify ceremony
   re-admits I.
3. Replica A can read “frozen” as historical and retain I.
4. Replica B receives only the surviving control chain and I. It sees no seal,
   no portable freeze fact, and therefore sees a provisional owner that a
   canonically earlier claimant may displace.

The local effect-deduplication key is execution state, not portable validation
state. Choose explicitly:

- materialize frozen replay ownership as a durable protocol fact and define
  its recovery adoption/cut behavior; or
- make ownership wholly derived, allow C3′ to unfreeze it, and specify what
  happens to references and already escaped effects.

A logical imported-item identity derived from the replay key remains the
simplest escape hatch, but carrier-op identity can work if its freeze is
actually represented.

## Blocker 5: the Merkle record proof does not authenticate the release header/layout

D-147 builds one tree whose leaf 0 is
`H_bhdr({v, export_id, record_count})`, followed by record leaves
([root construction](/Users/vm/owner-plane-d0a-spec.md:2361)). An `mimport`
carries only its record index and the siblings needed to fold
`H_brec(record)` to the signed root
([proof rule](/Users/vm/owner-plane-d0a-spec.md:2368),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3907)).

That proves membership in *some* tree with that root. It does not generally
prove that the expected header was leaf 0. For 129 leaves, for example, the
last record leaf promotes repeatedly and eventually receives one opaque
sibling hash representing leaves 0…127. The verifier cannot open that sibling
to establish that it contains the expected header, the expected ordered source
layout, or even a header leaf at all.

The full bundle used to establish those body invariants. The advertised
per-record rebuild path is supposed to replace the full bundle after a sibling
is erased. A compromised release signer cannot be trusted to have built the
right tree merely because it signed the root; source-side body validation is
what must reject that malformed root, and after erasure a fresh replica can no
longer reproduce it.

Use a structure in which every record proof independently binds the release
context. Options include:

- an outer header/root wrapper over a records root, binding at least
  `{v, export_id, record_count, ordered_sources_hash, records_root}`;
- release-context and absolute source index inside every record leaf; or
- durable non-secret leaf/manifest material sufficient to reconstruct and
  validate the canonical root after content erasure.

Then specify the verifier exactly. Today `rec_index` does not say whether it is
zero-based among records or the absolute leaf position (where the first record
would be 1); its range and equality with the sorted `sources` position are not
stated; proof order and exact sibling consumption under odd promotion are not
defined; and extra or missing siblings have no named failure rule. The depth-8
cap itself is correct.

## Blocker 6: D-148 cannot carry D-149's branch-relative rejection causes

The journal gives each missing record an optional scalar `basis`, described as
the seal, void, or eligible cap on which permanent quarantine depends. Absence
means an intrinsic rejection or source erasure
([journal rule](/Users/vm/owner-plane-d0a-spec.md:1199),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3816)). D-149 simultaneously makes
`no-grant`, `scope-*`, certificate rejection, ceilings,
`import-collision`, and control-relative `body-invariant` failures permanent
only within the branch and re-evaluable when their rejecting fact dissolves
([disposition map](/Users/vm/owner-plane-d0a-spec.md:2121)).

Those two contracts do not compose.

Concrete trace:

1. A destination import rejects with `scope-space` because of a space-
   retirement control operation.
2. Source recovery classifies it resolved-negative and writes XferAbort.
3. It has no intrinsic byte failure. Yet D-148 does not normatively map the
   retirement op into `basis`; the prose says basis-free reject-permanent is
   intrinsic.
4. C3′ cuts the retirement. D-149 requires the import to re-evaluate and admit.
5. An implementation that wrote a basis-free abort keeps the transfer closed;
   a fresh fold sees an admissible import.

`import-collision` is harder still: its rejecting fact may be another
**tenant** claimant, not a control operation, and provisional-owner and frozen-
owner collisions have different lifecycles. One record can also be negative
under several independent removable causes; a single unspecified scalar leaves
implementations free to choose different bases and therefore different reopen
histories.

Define a closed, typed per-record cause representation, for example:

- intrinsic/stable;
- control boundary or policy fact, naming the exact control op;
- tenant incorporation/fork fact, naming the exact tenant op;
- provisional replay owner, naming the claimant and its freeze state; and
- source erasure/escaped-effect stable state.

Either carry the complete cause set or define one canonical sufficient cause.
Then define exactly when it invalidates and when `XferReopen` is appended.

The transition fold also needs normative validation. Several older absolutes
remain: recovery looks for “no terminal record,” an import “can never commit
after an XferAbort,” and PendingXfer “clears” on any terminal
([journal prose](/Users/vm/owner-plane-d0a-spec.md:1122)). All are false after a
valid reopen. Pin: Pending opens incarnation 0; terminal closes incarnation n;
`Reopen(n)` is legal only for the current Abort(n) after a cause invalidates;
it opens n+1; it is illegal after Done; duplicate/wrong-incarnation records
have named outcomes.

## Blocker 7: adopted KEM renewals are outside recovery precedence and key history

`crecovsucc` now carries `adopted_renewals`, described in a CDDL comment as
preserving a cut renewal's certificate and replacement wraps
([wire shape](/Users/vm/owner-plane-d0a-spec.md:3716)). The executable recovery
pipeline's precedence stage, however, enumerates only adopted rotations and
cutoff Heads as state-dependent references
([`admit_ctrl`](/Users/vm/owner-plane-d0a-spec.md:2069)).

If recovery arrives before the referenced renewal bytes, one implementation
can give it precedence and cut the branch while another pends waiting for the
renewal. The later adoption cannot be validated after the first
interpretation's cut. Add adopted renewals explicitly to the pre-precedence
resolution contract: unheld target = `ref-unresolved`, no precedence effect;
wrong device, non-renewal, non-KEM renewal where prohibited, target not after
base, and noncontiguous chain need exact outcomes.

The portable freshness domain is independently incomplete. It is defined as
surviving-chain enrollments plus recovery `retired_keys`
([T3](/Users/vm/owner-plane-d0a-spec.md:819),
[enrollment rule](/Users/vm/owner-plane-d0a-spec.md:1302)). An adopted renewal
is cut from the surviving chain and is not required to contribute its signing
or KEM key IDs to `retired_keys`.

Security trace:

1. D1 renews S0/K0→S1/K1 on the cut branch, replaces its wraps, and destroys
   K0 after the custody predicate permits it.
2. Recovery bases before the renewal and adopts it, preserving K1 as D1's
   storage identity.
3. The recovery omits S1 and K1 from `retired_keys`.
4. A new surviving-chain certificate for D2 reuses K1. D1 still possesses the
   K1 secret and can decrypt D2's wraps. Reusing S1 similarly resurrects a
   private key under a new device identity and defeats self-witness exclusion.

An adopted certificate must automatically contribute portable typed key
history: its signing key is globally burned; its KEM key remains bound to its
adopted `device_id`, including an explicit same-device recovery rule. Requiring
the owner to duplicate both IDs in `retired_keys` would close reuse, but a bare
ID loses the device association needed for KEM's different-device/unchanged-
same-device distinction.

Finally, define the renewal adoption graph. `{device_id, renewal_op}` plus
“contiguous chain” does not say what happens in C0/K0→C1/K0 (signing-only)→
C2/K1: C2's `renews` points to a cut C1, even if the list purports to carry
only KEM-rotating renewals. Specify endpoints and automatic dependency closure
for intermediate certificates, just as adopted rotations already have an
exact dependency contract.

## High-priority protocol exactness

### H1. Device-revocation closure coverage follows wraps, not authorship

`c.revoke_device` derives revocation of every active grant, but its completion
coverage is every zone in which the device currently holds wraps
([registry](/Users/vm/owner-plane-d0a-spec.md:1303)). The specification
expressly permits a device to retain an active op-authoring grant in a zone
from whose current epoch it was excluded; D-141 had to make renewal history
coverage independent of KEM membership for exactly this reason.

Such a grant-only/excluded zone may get no certificate-revocation frontier.
That contradicts D-151's premise that every revoked past authoring grant was
already delimited. Separate two domains:

- rotation references cover current wrap membership; and
- `frontierclose`s cover every active op-authoring zone for the target device,
  independent of wraps.

### H2. Renewal-after-grant-revocation is asserted, not defined

§4.2 still says a superseded certificate's operations remain valid only at or
before the renewal's own `history_cutoffs`, and even retains the obsolete
`accepted_through: "none"` wording
([certificate rule](/Users/vm/owner-plane-d0a-spec.md:345)). D-151 allows a
renewal to omit a zone whose authoring grant was revoked, relying on that
grant's frontier instead. But D-143 intentionally gives those records
different selectors: grant-revoke matches the cited grant; supersede matches
the predecessor certificate.

Because certificate resolution precedes grant resolution, an old operation
can pass the revoked grant's position-relative prefix yet fail as
`cert-superseded` for lack of a renewal frontier. The named vectors do not
define the reducer.

Publish the exact formula. Either:

- renewal materializes predecessor-certificate frontiers for every historical
  authored zone, including revoked ones; or
- certificate validity explicitly accepts the immutable frontier of the
  operation's cited, revoked grant as the predecessor-authorship bound.

Then update §4.2 and the admission pipeline to the same rule.

### H3. Recovery omission has a key but no universe or continuation rule

The complement is now keyed by `(zone,lineage)`, a real improvement, but
“a pair without an entry” still lacks a defined universe
([recovery rule](/Users/vm/owner-plane-d0a-spec.md:1676),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:3704)). Possible readings are:

- pairs existing in surviving control state at `base`;
- pairs existing immediately before recovery;
- every held pair, including cut-branch state (replica-relative); or
- a lazy universal default that also applies to pairs created after recovery.

The last reading appears plausible from “universally,” but it means every new
post-recovery lineage's first write starts at `Bounded("none")` and requires a
ratify ceremony. If that is deliberate, state and vector it. If not, freeze the
finite pre-state universe in portable control terms.

Also state the continuation ritual for a pair named by an explicit recover
frontier. Its selector is global and immutable, so later reauthorization does
not permit that lineage to write beyond the preserved frontier. The document
should say whether the device must re-enroll under a new device/lineage or
whether another continuation operation is intended.

### H4. D-144 needs an explicit cap-before-admission stratum

D-144 correctly says cap eligibility asks whether `last_known` is on the held
canonical chain, never whether it is terminal
([cap rule](/Users/vm/owner-plane-d0a-spec.md:1398)). The ordinary `w.gen` rule
still requires `last_known` to be accepted and terminal, with failure
`body-invariant`
([§9.3](/Users/vm/owner-plane-d0a-spec.md:1909)), and the eligibility paragraph
still mentions an unheld “terminal Head”
([stale wording](/Users/vm/owner-plane-d0a-spec.md:1424)).

The intended cap-wins result is well-founded only if the reducer first derives
eligible caps from held bytes, then evaluates ordinary operation admission
under the cap-clamped chain. Say that normatively. Otherwise an implementation
can run the ordinary body check first, reject W because delayed H6 made H5
nonterminal, and never revisit the `body-invariant` rejection; another derives
W's cap first and accepts W after H6 is quarantined.

### H5. Revisit and disposition inventories are no longer exhaustive

T2 still says proofs, budgets, and boundaries are the exhaustive revisit
functions and “nothing else revisits”
([inventory](/Users/vm/owner-plane-d0a-spec.md:700)). D-146 adds a fourth:
replay-key ownership, where a late canonically earlier claimant displaces an
accepted provisional claimant. Add it to the derived-state inventory and name
the displaced outcome.

The closed disposition table also needs a cause-level pass:

- `import-collision` may be tenant-derived, not control-derived;
- provisional-owner and frozen-owner collisions have different lifecycles;
- `cert-revoked-in-fold` is prose, not the enum member `cert-revoked`;
- `request-fork` is listed in the rejection row but its C3′ survival rule is
  not stated; and
- references to a provisional import are called “pending-dependency,” but no
  specific closed outcome is selected.

The `mimport` CDDL comment still says identity is FINAL under D-139
([stale comment](/Users/vm/owner-plane-d0a-spec.md:3934)), directly
contradicting D-146's provisional phase.

### H6. `retired_keys` compares IDs on the wire and raw keys in prose

The wire carries `bytes32` key IDs, while T3 and the enrollment row say
`sig_pk` “equals” or “is in” the freshness domain. State the actual checks as
`H_key({sig_alg,sig_pk})` and `H_key({kem_alg,kem_pk})` membership. For a bare
retired ID, the safest exact meaning is global deny; adopted certificates,
not retired IDs, should retain the typed device association needed by the KEM
rule.

## What is strong and should not be relitigated

- The authority-selector split in D-143 is correct. It fixes both sides of the
  v0.5.11 failure: old authority closes without bricking the successor.
- The frontier shape and total override are the right representation for
  multi-head unknown-gap lineages once the live-head bound is reconciled.
- D-144's “chain membership, not terminality” choice is the right winner for
  the W/H6 race.
- Provisional import ownership and reference deferral are the right direction;
  the missing piece is durable freeze/handoff semantics.
- Durable per-record transfer material is necessary. The remaining D-147 work
  is commitment structure and exact verifier definition, not a return to a
  flat transient bundle.
- Per-record terminal causes, journal incarnations, and incarnation-scoped
  effect keys are the right shape.
- D-149's split by the rejecting fact is the correct model. Finish the closed
  cause mapping rather than reverting to unconditional permanence.
- Cross-device KEM reuse and same-device K0→K1→K0 are correctly prohibited.
  Owner-carried retired key IDs make the deliberate uncarried-cut-key residual
  portable.
- D-151 correctly removes the dead import arm, rejects the empty requester
  no-op, completes the mirror-negative list, defines live lineages, and states
  Gate A's actual status.

## Suggested v0.5.13 repair order

1. **Make closure construction total:** reconcile 64/65, bind staged consumers,
   remove implicit prior-advance reuse, and widen reservation through
   `last_known` dependencies.
2. **Finish import identity:** make revoke frontiers freeze preserved owners,
   decide whether C3′ preserves or re-derives frozen ownership, and represent
   that choice portably.
3. **Finish transfer proof and journal:** redesign the root so one record proof
   binds release context; publish exact proof pseudocode; define typed causes
   and the incarnation transition table.
4. **Finish recovery key state:** put adopted renewals in precedence resolution,
   define their dependency graph, and include adopted certificates in typed
   freshness history.
5. **Close composition exactness:** separate revocation's wrap/authorship
   domains, publish the renewal cert×grant validity formula, define the
   recovery pair universe, and synchronize D-144/D-149 prose and CDDL.
6. **Only then author the companion and corpus.** The new Gate-A status line is
   correct: the companion, fixtures, surface runs, and final discrepancy audit
   still do not exist
   ([§16](/Users/vm/owner-plane-d0a-spec.md:3197)).

The protocol is converging. This round does not call for another redesign; it
calls for making the new frontier, ownership, proof, journal, and recovery
objects reconstruct the same state on an incremental replica and a fresh one.
