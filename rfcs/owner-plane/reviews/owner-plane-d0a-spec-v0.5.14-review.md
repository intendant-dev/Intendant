# Review: D0-A Core + Memory normative specification v0.5.14

*2026-07-13. Independent review of
[*owner-plane-d0a-spec.md*](/Users/vm/owner-plane-d0a-spec.md) v0.5.14,
4,408 lines / 47,916 words / 361,874 bytes (SHA-256
`e87bdbdee1406e33d6bc1c604fedc54c876509935d526e982e9bce24b9a833ed`).
The reviewed predecessor is the archived v0.5.13 source
(SHA-256
`0d3a316a8082392744f2890a92e2824a4bb287796fe210c1ac07eb710f9c8609`);
the delta is 259 insertions and 99 deletions. I used the v0.5.13
synthesized review (SHA-256
`2588f0a05742129a2c7de3d96483920fb8205adea90790deb9a017ba87e1069a`)
as the disposition ledger. No v0.5.14 peer report was consulted. Findings
below were re-derived against the v0.5.14 normative prose, CDDL, decision
records, and required-vector inventory.*

## Executive verdict

**Cut v0.5.15. Do not freeze v0.5.14 or start the normative companion and
independent core from this text.**

This is a material improvement, not a failed revision. D-160 through D-167
resolve most of the narrow counterexamples that motivated them:

- a reference-pending stage carrier now creates no accepted stage, and the
  named dependent strict consumer waits;
- pre-freeze import losers now receive the same disposition in both simple
  arrival orders;
- `bundleleaf` has a named, versioned preimage and import admission is
  nominally per-record;
- `XferReopen` now cites both a basis and an invalidating operation;
- the adopted-renewal comparison is correctly “strictly after base”;
- device revocation can encode empty authorship and independently requires
  authorship and wrap coverage;
- the selector-intersection rule is published on the main validation
  surfaces; and
- recovery-universe, unadopted-key, and held-chain mirrors are substantially
  better aligned.

Those choices should be retained. The problem is that the new local rules do
not yet compose into one deterministic reducer. Nine freeze-blocking clusters
remain:

1. staged-frontier consumption is contradictory for retirement and
   undefined when a stage resolves behind a later consumer;
2. import freeze ignores revivable earlier claimants, while a collision's
   terminal cause does not represent freeze-basis-only unfreeze;
3. the per-record Merkle validator changes its answer when the source record
   disappears;
4. journal terminals still lack the historical coordinate and total cause
   vocabulary needed to validate a later reopen;
5. adopted-renewal entries are simultaneously restricted to KEM rotations
   and required to include signing-only links, with an unresolved
   `retired_keys` overlap;
6. wide device revocation does not define a stable wrap-coverage domain
   across its rotation-first ceremony;
7. the published authority predicate omits recovery and permits an old
   certificate to use a grant issued after its supersession;
8. algorithm-tagged key IDs do not detect reuse of the same P-256 key
   material across signing and KEM roles; and
9. `w.gen` may name a held-but-unaccepted head, yet its transition retires
   exactly that non-Frontier head and leaves the accepted head live.

The last two are security/invariant failures, not documentation niceties.
Several of the others produce different accepted/quarantined state or
different journal legality from the same eventual byte set. A core built now
would have to invent protocol law.

Recommended disposition:

- **Architecture, trust posture, and the v0.5.14 direction:** accept.
- **D-152 and D-154:** keep closed.
- **D-160 through D-167:** retain their local rulings, but keep each cluster
  open until the composed repairs below land.
- **Protocol/schema freeze:** no.
- **Gate A:** false, consistently with the specification's own status line
  ([§16](/Users/vm/owner-plane-d0a-spec.md:3492)).
- **Next artifact:** v0.5.15, then a fresh discrepancy audit; only then the
  companion and independent reducer.

## Disposition ledger

| Topic | What v0.5.14 genuinely closes | Remaining disposition |
|---|---|---|
| D-160 staged frontiers | A pending carrier has no stage; the specifically named strict dependent waits; a lineage already closed by revocation is consumed vacuously | **Partial / blocker.** Retirement contradicts the strict-only rule, and the incremental downstream re-fold after late stage acceptance is unspecified |
| D-161 import ownership | Simple A-before-B and B-before-A traces now give the unfrozen loser the same quarantine disposition; a proof-pending earlier claimant reserves freeze | **Partial / blocker.** Revivable quarantined claimants do not reserve; a collision terminal's basis misses freeze-basis-only dissolution |
| D-162 Merkle import | `bundleleaf` and rank/path semantics are closed; whole-bundle checking is demoted to transport | **Partial / blocker.** Source-byte equality is availability-dependent, so target-record erasure changes admission |
| D-163 journal citations | Reopen carries `basis` and `invalidation`; an unheld citation is intended to pend | **Partial / blocker.** No evaluation frontier makes the historical claim verifiable; the cause map is neither total nor consistently mirrored |
| D-164 renewal adoption | “Strictly after base” is fixed; terminal/intermediate KEM intent is much clearer | **Partial / blocker.** Main prose rejects the signing-only entries that CDDL and vectors require; terminal-key/retired-key overlap is undefined |
| D-165 device revocation | `cutoffs: [*]` makes zero-authorship revocation encodable; dual coverage is explicit | **Partial / blocker.** The wrap-domain snapshot and evaluation position across rotation-first are undefined |
| D-166 authority predicate | The former certificate/grant mirror contradiction is addressed directionally | **Partial / security blocker.** The equation drops the global recovery selector and lacks certificate/grant temporal compatibility |
| D-167 recovery/freshness/frontier mirrors | The recovery blanket and post-base exception now agree across main prose, E8, and CDDL; unadopted cut keys are explicitly reusable | **Partial / security and reducer blockers.** Role-tagged hashes miss P-256 material reuse; held-but-unaccepted `last_known` cannot be retired by the stated Frontier transition |

## Freeze blockers

### B1. D-160 does not define one staged-frontier reducer

The core staging rule says the next epoch advance, renewal, or retirement
consumes every accepted stage for the zone
([§7.1](/Users/vm/owner-plane-d0a-spec.md:1685)). D-160 then reserves only “a
strict consumer whose coverage would be satisfied” by a held, pending carrier
([stage state](/Users/vm/owner-plane-d0a-spec.md:1707)).

That is already inconsistent with space retirement. The operation registry
([registry](/Users/vm/owner-plane-d0a-spec.md:1375)) and CDDL
([CDDL](/Users/vm/owner-plane-d0a-spec.md:3822)) require retirement coverage
**regardless of strictness**, while §9.4 groups
`c.space_retire` with operations whose coverage is mandatory only in a
strict zone
([epoch hygiene](/Users/vm/owner-plane-d0a-spec.md:2109)). The CDDL stage
comment also says a generic “dependent consumer” pends
([CDDL](/Users/vm/owner-plane-d0a-spec.md:3934)), not only a strict one.

Minimal trace:

1. Z is lenient and lineage L is live.
2. Earlier `c.cutoff` P stages L, but P is `ref-unresolved`, so no stage yet
   exists.
3. Later `c.space_retire` R relies on P for retirement's mandatory coverage.
4. The narrow D-160 rule lets R fail coverage; the CDDL/retirement rule makes
   R wait.
5. The referenced head arrives and P accepts. A fresh fold sees P before R
   and can accept R.

The same bytes therefore lack one prescribed incremental disposition.

There is a second exactness hole even when a later consumer does not need P.
Let P be an earlier pending stage for L1. Let later renewal R carry complete
inline coverage and accept while P has no stage. When P resolves:

- the positional D-153 rule and a fresh control-order fold put P before R, so
  R must consume it;
- an incremental reducer must therefore revisit already-accepted R and
  materialize P under R's selector.

The positional answer is inferable, but the specification never states that
resolving P triggers this downstream re-fold, identifies its affected suffix,
or vectors the transition. An implementation that only applies P's newly
accepted effect would leave it for the next consumer. That implementation
would be nonconforming, but the executable incremental obligation is missing.

Required repair:

- distinguish **stage consumers** (all renewals, all retirements, and every
  epoch advance under the current automatic-total rule) from operations for
  which coverage is mandatory (renewal over its authorship domain,
  retirement regardless of strictness, and strict epoch advance);
- mandate and bound the downstream control re-fold when a positionally
  earlier carrier resolves, unless the protocol instead chooses an explicit
  next-consumer reservation;
- state creation, identity, dependency, acceptance/rejection, one-use
  consumption, dead-lineage disposal, and selector materialization as one
  transition table;
- align §7.1, §9.4, the registry, CDDL, D-160, outcomes, and vectors.

### B2. D-161's import freeze and collision lifecycle are not closed

D-161 fixes the simple arrival-order case, but its freeze guard covers only a
held **unresolved** order-earlier claimant
([freeze rule](/Users/vm/owner-plane-d0a-spec.md:2660)). The derived revisit
inventory also contains held, resolved claimants whose budget or boundary
quarantine can later revive. Those claimants are just as capable of becoming
the canonical owner.

Constructible trace:

1. Claimant P is accepted under import grant G1.
2. G1 is revoked with a frontier preserving P, and later import grant G2 is
   issued.
3. An earlier, already-held budget consumer E under G1 resolves inside the
   preserved prefix and displaces P, leaving P in revivable quarantine.
4. Claimant Q under G2 becomes owner and then effect-final,
   giving it a live freeze basis. P is not “unresolved,” so it does not
   reserve the replay key.
5. E's qualifying proof is retro-disqualified,
   releasing the budget. P revives.
6. The total claimant order says P precedes Q, while “a frozen identity never
   moves while its basis stands” leaves Q frozen.

The guard must include every held order-earlier claimant not permanently
incapable of winning, or revival itself must be an explicit unfreeze trigger.
“Surviving” and “otherwise-admissible” need closed definitions over pending,
revivable, and permanent states.

Collision classification is correctly allowed to re-evaluate under C3′, and
the journal already reopens when winner A itself dissolves, displaces, or
retro-quarantines. The uncovered transition is narrower: D-163 records a
collision's basis as the winning claim
([basis map](/Users/vm/owner-plane-d0a-spec.md:1238)). That is not the fact
that makes the loser permanently unable to win. If recovery removes freeze
basis F while winner A remains the unfrozen owner, loser B changes from
permanent collision to ordinary revivable order-loser. A has not dissolved,
been displaced, or retro-quarantined, so the stated basis-invalidation
predicate does not reopen B's transfer terminal. The actual sufficient cause
is at least the conjunction `(winner A, freeze basis F)`, or a canonical typed
freeze fact; one winner hash is insufficient.

Required repair:

- reserve freeze for pending **and revivable** earlier candidates;
- explicitly re-derive collision classification when a freeze basis dies
  while its winner survives;
- encode a collision cause that includes the live freeze basis;
- name exact outcomes for ordinary import displacement and a provisional
  import target. The closed enum currently has only
  `import-collision` ([§10.4](/Users/vm/owner-plane-d0a-spec.md:2241)), while
  the prose also requires quarantine-reproposal and pending-dependency.

### B3. D-162's validator changes when the source disappears

D-162 says admission is always per-record, but adds byte equality against the
source-derived `bundlerec` only **while the source remains derivable**
([validator](/Users/vm/owner-plane-d0a-spec.md:2536)). That conditional makes
data availability an admission input.

Take a signed two-record root over correct A and malicious B′. B′ carries B's
`source_op` and signed rank but altered record content, and it has a valid
leaf/path to the signed root.

- While source B is readable, source equality rejects B′ as
  `body-invariant`.
- If source B is erased before B′ is first evaluated, equality is skipped and
  the same leaf/path accepts B′.

This directly contradicts the required vector's assertion that the bad record
never imports before **or** after erasure
([family 11](/Users/vm/owner-plane-d0a-spec.md:3211)). It also yields
different durable destination state from the same signed release and import
bytes under different retention/delivery schedules.

The source-erasure abort machinery does not close the hole. Its critical
section guarantees that no import commits after the **current interval's
appended `XferAbort`** lists it missing
([journal recovery](/Users/vm/owner-plane-d0a-spec.md:1174)). It does not give
the erasure path priority over a newly delivered B′ that takes the lock after
source deletion but before the abort is appended. Serialization makes “B′
first” and “Abort first” atomic; it does not prohibit the first schedule.

The preferred repair is durable source-binding evidence that remains
verifiable after source erasure. Making the signed leaf authoritative and
removing source equality is possible only as an explicit security-posture
change: an export-authorized signer could then bind arbitrary statement,
kind, or class-floor bytes to an allowed `source_op`, including substituting
lower-class flow eligibility for unrelated content. That choice would have to
amend the exact source-derived bundle contract
([bundle contract](/Users/vm/owner-plane-d0a-spec.md:2495)) and its CDDL
mirror, not merely delete the equality check.

If source equality remains, source erasure must immediately make new attempts
ineligible and that transition must itself be portable. Refine the existing
target-erasure vector so malicious B′ is first delivered/evaluated only after
B's erasure, including both erase/Abort/import lock schedules; an earlier
permanent rejection must not mask the availability-dependent case.

### B4. D-163 does not make journal history verifiable

Adding `basis` and `invalidation` is useful, but the bytes do not establish
the historical state about which those hashes make claims.

The canonical basis is the minimal op hash among sufficient facts “at the
terminal's fold position”
([main rule](/Users/vm/owner-plane-d0a-spec.md:1234)). `XferAbort` carries no
control frontier, tenant frontier, proof-feed frontier, or equivalent
evaluation coordinate
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4129)). Because the terminal,
control, tenant, and receipt records live in different ordered domains, a
fresh replica cannot reconstruct which sufficient facts existed at that
historical position. A later-arriving, lower-hash fact changes the apparent
minimum. Merely recording the selected hash does not prove it was canonical
then.

The same omission prevents validating historical invalidation. Suppose Q
displaced P, then became the frozen owner so P acquired an
`import-collision` terminal; later Q's qualifying proof is
retro-disqualified. A current rebuild sees only that Q is not a surviving
claimant. Without the terminal's evaluation coordinate, it cannot prove that
Q once made P permanently resolved-negative, or that the cited control/proof
operation later invalidated exactly that state. “Verifiable-when-held” proves
possession of two operations, not their historical relationship.

The cause vocabulary is also not closed:

- the main map sends `scope-*` to a boundary/retirement, but a static
  `scope-op` failure is caused by the cited grant itself;
- D-149 includes control-relative `body-invariant`, but D-163 does not map it;
- deadline/lease/causal attempts can become terminal-eligible through
  checkpoint hardening
  ([checkpoint](/Users/vm/owner-plane-d0a-spec.md:1384)), but no checkpoint
  cause is defined;
- `no-grant` can reflect never-issued/missing authority rather than a
  revocation operation; and
- collision needs the compound mutable cause described in B2.

There is a direct normative mirror conflict too: the main rule selects the
**minimal op hash**, while both D-157
([decision record](/Users/vm/owner-plane-d0a-spec.md:3449)) and the CDDL still
say the **first branch-relative fact in fold order**
([CDDL comment](/Users/vm/owner-plane-d0a-spec.md:4142)).

Required repair:

- bind every terminal to a portable evaluation coordinate covering every
  domain its resolved-negative decision reads, or replace the historical
  scheme with a monotone cause construction;
- make `terminalcause` a typed, total wire union capable of conjunctions where
  needed;
- define one cause and invalidation predicate for every terminal-eligible
  outcome/context;
- assign closed outcomes to an unheld reopen citation, invalid citation,
  wrong incarnation, duplicate terminal, and reopening a Done. The prose
  alternates between “pends,” `log-corrupt`, and the disposition
  “storage-quarantine” without one E10 outcome for every case
  ([interval rule](/Users/vm/owner-plane-d0a-spec.md:1276)).

### B5. D-164's adopted-renewal list has contradictory membership

The main recovery rule says **each entry** adopts a cut KEM-rotating renewal,
then says dependency closure runs through signing-only intermediate renewals
([§7.4](/Users/vm/owner-plane-d0a-spec.md:1833)). The CDDL says the bounded
list carries **all chain renewals**, including signing-only links
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4012)), and the required vector says
those links ride the list
([family 8](/Users/vm/owner-plane-d0a-spec.md:3033)).

For K0→K1 followed by a signing-only renewal that retains K1, one reading
rejects the second entry because it is not KEM-rotating; another requires it
for contiguity. A recovery operation cannot satisfy both validators.

Define each list entry as any `cenrollrenew` in the selected contiguous,
strictly-post-base prefix. Every entry contributes certificate/signing-key
history. Only entries whose KEM key changes contribute replacement-wrap
storage state. The terminal adopted KEM key is the terminal certificate's KEM
material; burn each distinct earlier KEM material only when it differs from
that terminal material.

A separate cross-field conflict remains: `retired_keys` may contain the same
typed ID as the terminal adopted KEM key. The key is then both globally
retired and expressly reusable by the same device
([freshness rule](/Users/vm/owner-plane-d0a-spec.md:841);
[recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:4040)). Reject the overlap
as `body-invariant`, or state explicit retirement precedence and narrow the
same-device exception. Vector the chosen rule, including a signing-only
terminal.

The one-total-64 cap also lacks a boundary consequence for a cut renewal
chain with 65 required links. Adopted rotations name deep-fork
storage-orphaning as a residual; adopted renewals do not. Either define the
65th-link orphaning result and vector it, or add a continuation mechanism.

### B6. D-165 does not freeze the revocation wrap domain

The dual-domain ruling is correct, and `cutoffs: [*]` fixes the zero-author
case. The wrap side still has no unique 65-zone ceremony.

Exclusion rotations are accepted and effectful **before** the completing
`c.revoke_device` references them
([registry](/Users/vm/owner-plane-d0a-spec.md:1369);
[rotation-first ceiling](/Users/vm/owner-plane-d0a-spec.md:1877)).
The row says “zones the target holds wraps for per control state,” while
earlier D-50 language says all decryptable zones. It does not identify a
snapshot or evaluation position. A plausible current-membership reading uses
`held_zones`, where only effective wraps at the latest accepted epoch count
and an exclusion rotation immediately removes the zone
([membership](/Users/vm/owner-plane-d0a-spec.md:488)). A plausible
outstanding-obligation reading instead retains zones under D-71's exclusion
freeze. The text never chooses.

Under the current-membership reading, take a zero-author device holding wraps
in 65 zones:

- If all 65 exclusion rotations precede the main revoke, its current wrap
  domain is empty. A main operation with 0 or 64 references can vacuously
  appear complete; the continuation is unreachable.
- If 64 precede the pending main revoke and the 65th rotation arrives next,
  the last zone leaves current membership before its continuation reference
  arrives. A dynamic-domain implementation again completes early.

Thus the pinned “0-author/65-wrap” constructibility obligation does not test a
uniquely defined state machine
([family 8](/Users/vm/owner-plane-d0a-spec.md:2900)).

Freeze a durable wrap-coverage domain: for example, membership immediately
before the first outstanding exclusion rotation, or current membership union
the target's D-71 outstanding-exclusion obligations. Define the snapshot
identity, the exact control position at which completion is evaluated, how
re-admission/abandonment changes it, and how main plus continuations discharge
it. Do the same audit for authorship coverage so neither domain silently
shrinks while a compound is pending.

### B7. D-166's authority predicate is incomplete

There are two independent failures.

First, the selector algebra defines **global/unqualified recovery** alongside
the certificate/supersede, grant/revoke, and epoch/close axes
([authority closures](/Users/vm/owner-plane-d0a-spec.md:1595)). The published
equation and the validation pipeline enumerate only certificate ∩ grant ∩
epoch
([equation](/Users/vm/owner-plane-d0a-spec.md:1613);
[pipeline](/Users/vm/owner-plane-d0a-spec.md:2157)).

Take a named recovery frontier through H5. H6 is within its certificate,
grant, and epoch bounds. The general “every matching closure” rule rejects H6
as beyond recovery; the literal three-axis equation admits it. Publish

`recovery closure ∩ certificate closure ∩ grant closure ∩ epoch closure`

on every surface. Recovery's omitted-lineage, revivable `admit_bound` blanket
is a separate mechanism and should not be conflated with the named recovery
closure.

Second, “absence on an axis is neutral” allows a superseded signer to acquire
new authority:

1. C0 and G0 authorize zone Z.
2. G0 is revoked with its frontier.
3. C0 renews to C1; Z is legitimately omitted from supersession coverage
   because no grant remains.
4. A later G1 is issued to the same device/lineage in Z.
5. The old C0 key signs H6 citing G1.

There is no C0 supersede closure for Z, G1 has no revoke closure, and the epoch
axis is open. The literal intersection admits H6 even though §4.2 promises
that “the old key authors nothing new”
([renewal rule](/Users/vm/owner-plane-d0a-spec.md:347)). Grants bind a device
ID, not a certificate generation, and `c.grant` adds no compatibility test.

Add a portable certificate/grant temporal relation: a certificate may use a
grant that was effective for it when issued, and descendant renewed
certificates may inherit the intended prefix semantics; an already-superseded
ancestor may not use a later grant. Pin the exact cert/authz stage,
outcome, disposition, and all four combinations of old/new certificate with
old/new grant.

### B8. D-167's “typed identity domain” misses cross-role P-256 reuse

The spec correctly moved comparisons to `key_id = H_key({alg, pk})`, but
`alg` makes the same cryptographic material hash differently by role.
Browser signing uses algorithm ID `p256`; HPKE recipient keys use
`hpke-p256-v1`
([suite](/Users/vm/owner-plane-d0a-spec.md:185)). Both public keys are the same
65-byte SEC1 P-256 point format, while `key_id` includes the algorithm ID
([encoding](/Users/vm/owner-plane-d0a-spec.md:195)).

Therefore the same P-256 private scalar/public point can appear as:

- D1's KEM key and D2's signing key, letting D1 sign as D2 and bypass
  device-ID self-exclusion; or
- D1's signing key and D2's KEM key, letting D1 decrypt wraps intended for D2.

Neither pair has equal typed key IDs. Intra-certificate signing/KEM reuse also
passes unless another rule rejects it. This defeats exactly the cross-device
custody and self-witness attacks that T3 says plane-wide freshness prevents
([T3](/Users/vm/owner-plane-d0a-spec.md:833)). The sentence at line 835 still
says raw `sig_pk` “equals any key” immediately before saying comparisons are
never raw, which exposes the unresolved model.

Keep role-tagged `key_id` for protocol addressing, but add a role-neutral
`key_material_id` for freshness—e.g. a domain-separated hash of
`{curve: "p256", sec1_point}`—and carry that identity through surviving
enrollments, adopted history, and `retired_keys`. Explicitly reject
signing/KEM material equality inside one certificate and across all historical
roles. Add both role-swap directions and same-certificate reuse to the
security vectors.

### B9. D-167 permits a head that the transition cannot retire

The Frontier contains accepted live heads only
([Frontier definition](/Users/vm/owner-plane-d0a-spec.md:624)). D-167 now
allows ordinary `w.gen(last_known=H)` to name a canonical held head even when
H was budget-displaced and never accepted. Its transition still says to
retire **exactly the named head**
([§9.3](/Users/vm/owner-plane-d0a-spec.md:2034)).

Constructible trace:

1. H4 is the accepted Frontier head under grant G1.
2. H5 is H4's valid canonical successor, but budget displacement leaves H5
   unaccepted.
3. W uses a funded grant for the same lineage to open the next generation
   with `last_known = H5`. D-167 makes H5 a valid held-chain anchor.
4. W accepts and “retires exactly H5.” H5 is not in the Frontier, so the
   transition removes nothing. H4 remains live even though W's incorporation
   cap closes that generation at H5.

Repeating this pattern can retain accepted “live” heads for generations
already deliberately closed, undermining Frontier exactness and bounded
live-head/checkpoint constructibility. Stale mirrors still say “terminal
Head” in cap dependency prose
([cap eligibility](/Users/vm/owner-plane-d0a-spec.md:1464)), compounding the
ambiguity.

On accepting W, retire the generation's effective accepted Frontier head at
or below the named canonical position, while leaving unrelated unknown-gap
generations alone. If no accepted head exists at or below that valid held
position, the natural transition is a successful retirement no-op; state that
explicitly, or deliberately reject it with a named outcome. The vector must
assert the resulting Frontier and effect-finality state, not only that W
avoids `body-invariant`.

## Exact outcome, transition, and mirror closure

The nine blockers drive a small but important E10 sweep. Before freeze, the
closed outcome enum, disposition table, and transition prose need explicit
entries or explicit broadenings for:

- a consumer waiting behind a pending stage carrier;
- an unfrozen import order-loser/displacement;
- a judgment, pin, or erase aimed at a provisional import;
- source-equality failure after the source has become unavailable, if that
  validator remains;
- a reopen with either citation unheld;
- an invalid citation, wrong incarnation, duplicate terminal, second reopen,
  and reopen-after-Done;
- a recovery adoption entry conflicting with `retired_keys`;
- the old-certificate/new-grant incompatibility; and
- the explicit successful no-op (or deliberately chosen rejection) when no
  accepted Frontier head exists at/below `last_known`.

Using `ref-unresolved` for several of these is reasonable, but it must be said
normatively and reflected in §10.4/§10.5. “Pending-dependency” or
“storage-quarantine” is a lifecycle/disposition, not the required typed
outcome.

Current-surface fixes must also supersede stale decision-record comments where
they remain normative-looking. The most consequential direct conflict is the
minimal-hash journal rule versus “first in fold order” in D-157 and CDDL. The stale
“terminal Head” wording and the raw-`sig_pk` sentence should be removed rather
than left for readers to reconcile through decision-record chronology.

## Required v0.5.15 vector additions

Every row needs both material delivery orders where applicable, an
incremental fold, and a fresh rebuild over the same eventual bytes.

| Cluster | Required trace |
|---|---|
| D-160 | pending stage → lenient `c.space_retire`; exact pending outcome and later release |
| D-160 | pending stage → independently complete same-zone renewal; on resolution, prove the downstream re-fold makes that renewal consume the stage |
| D-160 | pending carrier permanently rejects; dependent consumer releases with a named outcome |
| D-161 | earlier budget-quarantined claimant revives after a later owner freezes |
| D-161/D-163 | freeze basis disappears while winner remains; collision terminal reopens |
| D-162 | malicious B′ evaluated while B exists versus first delivered only after B's erasure, including the erase/Abort/import lock race |
| D-163 | later lower-hash sufficient fact arrives after terminal; canonical basis remains verifiable |
| D-163 | terminal under temporary claimant/proof state → later invalidation → fresh rebuild validates the historical interval |
| D-163 | one case for each cause arm: static scope, revocation, ceiling, checkpoint hardening, collision conjunction, intrinsic bytes, source erase |
| D-164 | KEM-changing → signing-only → KEM-changing contiguous adoption; exact list and terminal key |
| D-164 | terminal adopted KEM also in `retired_keys`; chosen rejection or precedence |
| D-164 | 64-link versus 65-link cut renewal chain; chosen orphaning consequence or continuation |
| D-165 | 0-author/65-wrap revocation with all rotations before main, and with the 65th while main is pending |
| D-165 | re-admission/abandonment during a pending wide revocation |
| D-166 | named recovery H5 versus otherwise-valid H6 |
| D-166 | C0/G0, C1/G0, C0/G1, C1/G1 after renewal and later grant issuance |
| D-167/T3 | same P-256 material KEM→signing, signing→KEM, and both roles in one certificate |
| D-167/`w.gen` | accepted H4, displaced H5, W cites H5; assert exact post-W Frontier and closure |

## Recommended repair order

1. **Close the authority and key-material security rules first** (B7, B8).
   They are small in concept and protect the two most important invariants:
   superseded keys acquire no new authority, and one private scalar cannot
   impersonate/decrypt across device roles.
2. **Finish the reducer state machines** (B1, B2, B9). Write transition
   tables before prose patches; these are all “same bytes, different event
   order” problems.
3. **Choose the transfer model, then close the journal** (B3, B4). The
   journal's cause vocabulary depends on the final import lifecycle.
4. **Close bounded recovery and revocation ceremonies** (B5, B6), including
   their frozen domains and cross-field exclusions.
5. **Run one generated mirror/outcome sweep** over main prose, registry,
   pipeline, E8, CDDL, decision records, and family inventory.
6. Only after those vectors are expressed in prose and schema should the
   normative companion and independent core begin.

## Final assessment

v0.5.14 demonstrates that the specification is converging. Its author is now
repairing at the right level—portable facts, fold state, explicit
dispositions, and executable mirrors—and most of the v0.5.13 local defects
are genuinely gone. The remaining issues are the harder second-order cases:
a stage resolving behind its consumer, a candidate reviving behind a frozen
owner, evidence disappearing after conditional validation, historical
journal claims without a historical coordinate, and authority/key identity
that changes meaning when two otherwise sensible rules meet.

That is exactly why another cut is warranted. Freezing now would convert
those ambiguities into implementation-specific protocol law. A focused
v0.5.15 addressing B1–B9, followed by the required differential/rebuild
vectors, should be reviewed as the next freeze candidate. Until then,
**Gate A remains false and the correct decision is no-go.**
