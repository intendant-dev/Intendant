# Synthesized review: D0-A Core + Memory normative specification v0.5

*2026-07-12. Adjudicated synthesis of
[`owner-plane-d0a-spec-v0.5-review.md`](/Users/vm/owner-plane-d0a-spec-v0.5-review.md)
and
[`owner-plane-d0a-spec-v0.5-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.5-review-2.md),
verified against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5 and the
v0.4 synthesized closure criteria. This asks whether independent fixtures can
derive one authority and state transition from the canonical bytes; it does
not reopen the umbrella architecture.*

## Executive verdict

**Both reviews reach the same release decision: do not declare Gate A yet;
cut one focused v0.5.1 closure patch, then build the core and corpus.**

The peer report materially improves the review. Its strongest new findings
are:

- `deadline_fallback` has no operative acceptance branch even though the
  families and D-54 describe two postures;
- “write verb” means claim-authoring in one place and any replicated mutation
  elsewhere, leaving judgment/pin/audit-only grant structure ambiguous;
- `actor.id` now affects principal identity but lacks a portable minting rule;
- `admin` is a frozen grant verb with no consumer;
- audit time provenance and several epoch/key interval pins remain unstated.

Its confirmations of hosted `audit.write`, hosted exclusion rotations,
self-cutoff, service compromise and failed-transfer completion are also sound.

The peer's headline claim that all six v0.4 blockers are already discharged at
the wire/reducer level should not be adopted. It verifies that the intended
mechanisms were added, but several mechanisms still have no bytes or complete
state transition:

- non-retroactive receipt policy has no policy/control field in a receipt;
- `c.cutoff` has no requester proof;
- Frontier/checkpoint zone scope is undefined;
- bundle bytes are unversioned, absent from Appendix A and lack a per-record
  floor/control snapshot;
- erase completion has no frozen complete-survivor membership;
- no core, corpus or harness exists.

The accurate combined disposition is:

- **Architecture and D-48…D-55 direction:** accepted.
- **V0.5 as implementation foundation:** yes.
- **Stable encoding/crypto scaffolding:** may start.
- **Canonical reducer/vector freeze:** not yet.
- **Next document:** a compact v0.5.1, broader than only Q1–Q9 but still an
  exactness patch rather than a new design round.
- **Durable P1 Memory writes:** remain prohibited.

## Assessment of the peer review

### Findings to adopt

1. **Q1 identifies a real semantic hole, but not a predetermined fix.** The
   normative deadline rule always requires a qualified receipt when a deadline
   field exists. `deadline_fallback` currently changes budget requirements but
   has no explicit admission branch, while family 9 and D-54 speak as though it
   does. That contradiction must be resolved before fixtures.
2. **Q2, Q3, Q6 and the rotation half of Q4 are confirmed.** Hosted
   self-cutoff has no wire proof; hosted audit authority and revocation-bound
   exclusion rotation contradict the exhaustive ceiling lists; failed
   transfer has no terminal record. The peer overstates the immediate hosted
   need for `c.revoke_zones`: a plane that has never lifted its ceiling cannot
   create enough zones to need that continuation, though the general
   continuation format still needs closure.
3. **Q5 is a significant schema catch.** The document needs two names:
   replicated/op-authoring verbs (the registry operations that append tenant
   chain state) and claim-authoring verbs (`propose`, `assert`). The former
   drives lineage, one-zone, budget and `w.gen`; the latter drives author
   retract/supersede.
4. **Q7–Q9 are valid.** Service-key compromise needs a remedy or explicit
   residual; `actor.id` minting affects portable relations; §9.1 must include
   the proof-feed requirements for GC hardening.
5. **Most pins are useful:** descriptor intervals, exact genesis grant scope,
   dead `admin`, audit-budget remedy, assert phrasing, diagnostic
   `maudit.at_ms`, and witnessless online-lease behavior. Repeating
   `connect_service_key = H_key({alg,pk})` and `new_epoch = current + 1` near
   their consumers would improve fixtures, but both already follow from
   global normative rules and are not independent defects.

### Where the peer is too optimistic

| Subject | Peer disposition | Adjudicated disposition |
|---|---|---|
| Receipt-policy binding | Discharged | Open blocker: no receipt carries the policy/control position, and T2 conflicts with T4's fold-current rule |
| Checkpoint proof finality | Discharged | Partial: no zone, historical policy or uniquely typed issuer scope; lease finality is unstated |
| Revocation/cutoffs | Discharged except hosted cutoff | Partial: empty history, continuation sizing, current-wrap coverage, complete grant revocation and requester replay remain open |
| Genesis | Discharged | Partial: several nested zone/epoch/key/scope equalities and first-cert class constraints remain unpinned |
| Memory identity | Discharged | Relation fold is fixed, but write-verb classes and actor-ID minting still decide authorization |
| Transfer | Discharged except abort | Partial: bundle schema/floors/control frontier, endpoint coordinates, expiry proof and export-ID uniqueness remain open |
| Erase | Discharged | Partial: target mapping and complete survivor membership/frontier are not frozen |
| Conformance | Discharged | Schema shell improved, but writer arithmetic, keyed-set uniqueness, RNG byte consumption and closed failure outcomes remain open; artifacts are absent |

### Q1 needs an owner choice, not an automatic advisory-deadline rule

The peer recommends making every deadline advisory in a budgets zone. That is
one possible ruling, but it conflicts with D-12's normative acceptance-deadline
semantics and makes a present expiry field ineffective. A more conservative
and internally cleaner resolution is:

- if a deadline is present, it always requires qualified proof;
- `budgets` authorizes **deadline absence** only when the write grant has a
  finite budget;
- `fail-closed` requires deadlines for new authority while retaining the
  explicitly grandfathered residual;
- witnessless solo planes cannot use online leases or deadline-bearing flows
  until they install a qualified witness (for example Connect), and this is
  stated and vector-pinned;
- family 9 says “deadline-bearing missing proof versus deadline-free budget
  lane,” not that the same deadline becomes advisory.

If product requirements instead demand solo deadline-bearing flow, amend D-12
explicitly and adopt the peer's advisory rule. Silence or fixture choice is the
only unacceptable option.

## What v0.5 genuinely resolves

Retain:

- service-key descriptors as the resolver behind `key_id`;
- one Connect predicate and witness-qualified lease issuers;
- fresh signing keys on renewal and the decision not to ratify pending proofs;
- early exclusion and all-zone target-exclusion direction;
- capability epoch 1, precise generation-window accounting and `w.gen` axis
  bypass;
- principal-level Memory relations consumed by `authorized(j)`;
- independently admissible assert/diary halves and explicit advisory
  `supersedes[]`;
- one locator hash, shallow nonrecursive evidence and cross-plane sensitive
  fallback;
- record-level import identity and completed source-op sets;
- typed erase entries, survivor pairs and fixed wrapper/key lengths;
- valid vector JSON, explicit draw ordering and the unchanged correct policy
  constants.

The following patch completes those choices.

## Consolidated v0.5.1 freeze blockers

### 1. Time posture, proof-policy identity and finality

First, resolve Q1 using one of the two explicit postures above. Then fix the
larger byte-level issue: T2's non-retroactive policy cannot be reconstructed
from `ReceiptStmt`. Receipts have no policy op/hash, admin epoch or control
frontier, and their storage frame supplies no cross-log control position. T4
instead evaluates against the fold's control frontier. Bind every receipt and
lease to one immutable accepted witness-policy/control event, or ratify one
retroactive rule.
([T2/T4](/Users/vm/owner-plane-d0a-spec.md:496),
[receipt CDDL](/Users/vm/owner-plane-d0a-spec.md:1703))

Close the associated history model:

- require one installed descriptor to resolve the named
  `connect_service_key`, and restate/pin the already-global
  `key_id = H_key({alg,pk})` rule at that consumer;
- define descriptor intervals, same-epoch duplicates and service compromise/
  `issuer_seq` cutoffs, or state compromised-service history as an explicit
  adversary residual;
- pin fresh-scope sequence 1 and whether receipts/leases share one counter;
- make checkpoint proof cutoffs select one unique tagged device/service key
  scope, zone and historical policy;
- apply GC finality to `lease-missing` as well as missing accept receipts, and
  update §9.1 with the full proof-feed condition;
- make “already admitted before renewal” preserve only operations whose normal
  proof predicates passed below the named Head—history cutoffs are not time
  proof;
- require a finite budget for every effective deadline-free write path,
  including a grandfathered certificate after the zone changes to
  `fail-closed`.

**Closure test:** receipt/lease qualification, service rotation and proof
hardening are pure functions of signed objects and canonical control history;
no lane lacks both time proof and a finite bound.

### 2. Hosted self-service, revocation and grant vocabulary

Add a real requester-attested `c.cutoff` body and a new signature domain. It
must bind plane, zone, lineage, accepted-through value, request ID, current
control/window state and requester certificate. For both cutoff and
`c.lineage_reauth`, make request IDs/nonces portable single-use state; current
reauth signatures can be replayed because request-ID reuse is not consumed.

Reconcile the exhaustive hosted ceiling:

- add the system-only `audit.write` exception;
- admit only `c.kek_rotate` operations tightly bound to an in-progress hosted
  exclusion ceremony, rather than allowing arbitrary empty-manifest rotation;
- keep the general continuation semantics coherent for a lifted, multi-zone
  plane; if the hosted ceiling is claimed to require a continuation, state the
  reachable bounded case explicitly;
- explicitly permit the recovery succession whose acceptance lifts the
  ceiling;
- label `c.drill`'s trusted authoring lane as product custody guidance only.

Define an explicit **replicated/op-authoring verb set** from the registry rows
that actually authorize tenant-chain mutations. Do not define it merely as
“everything except the three read verbs”: `admin` currently has no consumer,
and `curate.instruction` is a co-authorizer rather than a standalone operation.
Members require lineage, one zone and the applicable finite budget and receive
implicit `w.gen`. Define `{propose, assert}` separately as
**claim-authoring verbs** for author judgments. Decide whether `admin` is
reserved or identify its edge consumer.

Finish revocation/cutoff exactness:

- `revoke_grants` equals all active target grants;
- referenced rotations establish no current-epoch target wrap and occur after
  its last wrap-add;
- continuation handles both rotation references and zone cutoffs within the
  64-KiB cap;
- pending-continuation behavior on the single control chain is explicit;
- zero-history renewal/revocation has a typed sentinel;
- outer cutoff lineage equals Head lineage and the Head belongs to its zone;
- renewal's mandatory `lineage` is absent or validation-equal to the existing
  lineage;
- consume or mark audit-only `issued_admin_epoch` and constrain service
  `valid_from_admin_epoch`.

**Closure test:** a hosted replica can prove self, audit and revoke entirely
from closed bodies; every mutating grant has a lineage/budget shape; unused and
maximum-scope devices remain revocable.

### 3. Frontier, writer arithmetic and bootable genesis

State whether Frontier is zone-local or add `zone_id` to Head. Chains and
generation accounting are per zone, but Head's uniqueness key is only
`(lineage,gen)`; `c.checkpoint` also carries no zone. Bind Frontier,
`covers`, GC fence and witness policy to one explicit zone.
([Frontier](/Users/vm/owner-plane-d0a-spec.md:438),
[`c.checkpoint`](/Users/vm/owner-plane-d0a-spec.md:1799))

Pin tenant chain arithmetic: first sequence, exact successor increment, and
`w.gen(g) = previous maximum generation + 1`. “Next” and “chain intact” are not
enough for canonical negative vectors.

Complete genesis cross-field constraints: provenance-compatible first device
class; issued/capability epoch 1; exact ordinary/audit grant tenant, zone,
lineage, spaces and verbs; distinct home/audit objects; matching policy zone;
and wrap plane/zone/epoch/recipient/KEM key equality. Pin whether the ordinary
grant reads audit space.

**Closure test:** every valid genesis boots; one lineage can write every legal
zone without Frontier collision; sequence/generation jumps reject identically.

### 4. Memory/audit principal identity and physical audit commits

Pin `actor.id` for every tenant actor kind. Sessions may use session ID;
browser/daemon actors can derive from device ID; peers can use peer ID; human
identity needs an explicit per-plane value or a deliberate reduction to
lineage + direct-human evidence. Without this, `self` and `author` vary by
client convention.

For audit:

- choose the trigger: sensitive space minimum, any returned item's effective
  sensitive class, or both;
- replace free-text `maudit.principal` with a typed authenticated edge
  principal and bind scope/result IDs to a read ID;
- add chunk index/count or another grouping identity;
- map “same read transaction” to one physical Txn with a result cap, or define
  crash semantics for a logical multi-frame transaction; results must never
  precede required durable rows;
- pin `maudit.at_ms` as diagnostic local time (or another named source);
- provide a bounded hosted audit-budget refresh ceremony or name re-root as
  the remedy;
- restate assert equality as `P(claim) == P(judgment)`;
- add a distinct projection field for verdict `retract` versus `retire`, not
  only author versus curator.

**Closure test:** two clients derive the same principal/relation; every audited
read is attributable, physically recoverable and bounded.

### 5. Versioned bundle, expiry proof and terminal transfer

Add a versioned `bundle`/`bundlerec` to Appendix A. Require duplicate-free,
export-eligible claim sources; define each record's `class_floor`; and bind the
bundle one-to-one to `mexportrel.sources`. Add the control frontier used with
`data_frontier` and `as_of_ms` to derive classification.

Require complete plane destinations `(plane,zone,space)` and equality with all
three import-header coordinates. Relate release expiry to flow expiry, select
whether the qualified operation is release or import, and define that proof's
receipt zone/subject. Egress has no import, so its deadline must be consumed at
release acceptance. State witnessless-solo behavior.

Add a terminal XferAbort/Failed state—or a reason-bearing partial completion—
for erased/reject-permanent missing records. Require `completed` to equal the
exact bundle source set. Make `export_id` plane-wide single-use or include
`release_op` in every replay/journal key.

**Closure test:** one versioned snapshot produces one digest; a writer cannot
extend flow authority; every transfer reaches Done or Failed after any crash.

### 6. Erase target mapping and complete survivor membership

Pin `m.erase_request.targets` as operation hashes or item addresses. Define how
rotation admission verifies each `{item_addr, erase_op}` against a multi-target
request and how recovery retains that mapping after index deletion; adding
`target_op` is the simplest closed solution.

Add a versioned survivor-set preimage and a frozen tenant frontier. The
expected membership must be every old-epoch item at the fence minus the exact
erase set, with post-fence new-epoch writes treated explicitly. A self-
consistent hash over only the wrappers an implementation happened to include
does not prove completeness.

Define one winner for multiple erase requests targeting one item, logical-key
duplicate rejection, and the effective manifest limit under the enclosing
64-KiB control cap.

**Closure test:** every tombstone maps to one erased target and omission of any
survivor prevents old-KEK destruction.

### 7. Conformance closure and normative drift

Before corpus freeze:

- key sets by logical identity, not merely full member bytes, for grants,
  wraps, cutoffs, proof cutoffs, sources and erase targets;
- change RNG draws to ordered `{name,nbytes}` entries or provide equally exact
  family schemas;
- give control fork, recovery competition and differing request-ID reuse one
  enumerated outcome/disposition each;
- reiterate and vector-pin the already-normative
  `c.cap_epoch_bump.new_epoch = current + 1` rule at body validation;
- define or reserve the dead `admin` grant verb;
- align stale zone-less recovery/cutoff prose, `c.enroll` reauthorization,
  `c.drill` lane wording, daemon-only audit wording, T4 cutoff wording, the
  evidence-section cross-reference and `CheckpointCommit` name;
- state the effective typed-manifest cap under 64 KiB.

The JSON schema and policy hashes are sound. The mechanical Gate-A artifacts
are not present: no `owner-plane-core`, corpus or harness exists, and the
offline family-14 result remains open.

## Recommended v0.5.1 order

1. **Owner rulings:** exact `deadline_fallback` meaning and service-key
   compromise posture.
2. **Proof bytes:** receipt/lease policy ref, service intervals/cutoffs,
   checkpoint zone/scope and proof finality.
3. **Hosted/control:** cutoff attestation, consumed requester nonce, exhaustive
   ceiling, verb sets, revocation/empty-cutoff continuation.
4. **Scope/identity:** Frontier zone, writer arithmetic, genesis, actor IDs.
5. **Audit:** trigger, typed principal, chunks, time and budget remedy.
6. **Transfer/erase:** versioned bundle/expiry/abort and frozen target/survivor
   sets.
7. **Corpus:** remove drift, close keyed sets/RNG/outcomes, then implement and
   run every surface.

## Gate-A go-ahead

Give the go-ahead only when:

- deadlines, budgets and witnessless lanes have one explicit authority rule;
- receipts/leases bind immutable policy and proof history;
- hosted self-service and revocation are enforceable from closed bodies;
- Frontier/checkpoint, writer arithmetic and genesis have one zone/epoch
  meaning;
- actor identity and audited reads are portable and physically recoverable;
- bundles/transfers and erase/survivor sets have versioned complete identities;
- the core/corpus/harness exist, every named surface passes, the offline result
  is recorded, and the discrepancy audit finds only editorial drift.

## Bottom line

V0.5 is a successful convergence draft, and the peer usefully finds the
deadline-fallback, write-verb and actor-ID gaps. Its “all six discharged”
assessment is premature: it equates added mechanisms with completed byte-level
semantics and misses the unsigned receipt-policy position, zone/checkpoint
scope, partial genesis constraints, unversioned bundle, and incomplete
survivor proof.

Cut v0.5.1 with the consolidated patch above. After that, stop design review,
build `owner-plane-core` and let executable vectors decide Gate A. Durable P1
writes remain gated on Gate B plus the umbrella prerequisites.
