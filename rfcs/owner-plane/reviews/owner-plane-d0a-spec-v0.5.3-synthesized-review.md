# Synthesized review: D0-A Core + Memory normative specification v0.5.3

*2026-07-12. Adjudicated synthesis of
[`owner-plane-d0a-spec-v0.5.3-review.md`](/Users/vm/owner-plane-d0a-spec-v0.5.3-review.md)
and
[`owner-plane-d0a-spec-v0.5.3-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.5.3-review-2.md),
verified against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.3.
This document resolves disagreements between the reports; it is not a union
or vote-count of their findings.*

## Executive verdict

**Both reviews correctly conclude that Gate A is unavailable and that a
v0.5.4 is required. The peer is right about the fatal renewal-CDDL mismatch,
stale provenance, and several useful pins. Its “one token, then freeze the
prose” conclusion is not supported by the full protocol.**

The peer reruns the original D-69 counterexample correctly. That particular
defect is fixed: an epoch-1 operation and epoch-2 policy opening no longer
produce different currency decisions merely because they arrive in opposite
orders. It also correctly verifies D-83's audit wire and catches the immediate
D-84 encoding failure.

The report overgeneralizes from those successes. Passing the old
epoch-selector trace does not establish canonical replay for later
certificate/grant cutoffs, budgets shared across concurrent generation
chains, or receipt feeds delivered around a compromise cutoff. Likewise,
individual checkpoint array caps do not prove that an embedded 4096-head
Frontier fits a 64-KiB control operation; Fence ordering does not serialize
staged wrap authority or tombstone completion; and adding a free-text
`case_kind` property does not define a typed vector contract.

The adjudicated disposition is:

- **Resolved:** D-83 audit trigger/scope/principals/partition; the original
  fold-current epoch-currency comparison; the policy-vs-budget-axis decision;
  ordinary same-branch reauthorization replay; checkpoint carriage; the
  source import-policy selector; 0x14/0x16 mirrors; E8 element arithmetic.
- **Partially resolved:** D-78 historical closure, D-79 budget replay, D-80
  checkpoint finality, D-81 queued rotation, D-82 cutoff/recovery freshness,
  D-84 renewal, and D-85 dense feeds/transfer/control/vector closure.
- **V0.5.4:** a focused protocol-and-schema patch, not an architectural
  redesign and not a one-token edit.
- **Schema freeze:** no.
- **Gate A:** no; the core, corpus and harness do not exist, and family 14 is
  still open.
- **Durable P1 writes:** remain prohibited under the unchanged later-gate
  requirements.

## Assessment of the peer review

### Findings to adopt

1. **Renewal CDDL blocker B1 is exact.** The registry requires renewal
   `grants[]` and `lineage` to be absent, while `cenroll` still requires the
   `grants` key. Omission fails CDDL; presence, even empty, fails the row.
   ([registry](/Users/vm/owner-plane-d0a-spec.md:948),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2184))
2. **D-83 is materially closed.** The three triggers, bounded multi-space
   one-zone scope, mediated peer/session variants, zero-result chunk, physical
   partition, CDDL and family-11 requirements agree.
   ([audit registry](/Users/vm/owner-plane-d0a-spec.md:1420),
   [audit CDDL](/Users/vm/owner-plane-d0a-spec.md:2369))
3. **The repeated header-provenance defect is real.** It still claims to fold
   the v0.4 synthesis and archives only v0.1–v0.4. Repair it as a freeze item,
   not optional polish.
4. **The peer's cutoff equality pin is valid.** A revoked grant's cutoff must
   equal its zone and lineage. Extend that exactness to policy/bump closure
   sets, whose entries must all name the advancing operation's zone.
5. **Accepted versus active KEK terminology needs pinning.** Portable control
   admission can refer to a control-accepted epoch; serving refers to the
   last-Fenced active epoch. The peer correctly identifies the ambiguity,
   although choosing “accepted” alone exposes the queue deadlock below.
6. **The dropped-witness posture should be named.** Under the safer reading,
   a pending epoch-1 operation that still qualifies witness A cannot be
   hardened by an epoch-2 checkpoint listing only current witness B. It stays
   pending until A proves it, A is re-listed, or an applicable feed/epoch
   cutoff closes it. This is deterministic and conservative, but it is a
   forever-pending lane that needs explicit product/protocol disclosure and a
   vector.
7. **Add the hosted negative vector** in which a requester-attested cutoff set
   contains another device's lineage.

### Where the peer is too optimistic

| Subject | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-78 ordering | Fully portable everywhere | Original epoch comparison fixed; historical cert/grant replay, missing cutoff heads, multi-head meaning and closure capacity remain open |
| D-79 budgets | Cleanly resolved | Axis choice resolved; concurrent live generations still choose budget/raise-quota winners by arrival order |
| Dense feed cutoffs | Complete with `issuer-gap` | `{key_id,through}` proves no prefix; an honestly delayed statement freezes only the replica that received it after the cutoff |
| D-80 checkpoint | Replayable and bounded | Carriage fixed; full object cannot fit advertised caps, deltas cannot recover a missed page, and pending/cutoff semantics conflict |
| D-81 rotation | Activation and serialization complete | Activation fixed; later control acceptance can strand prior wrap-adds, and serialization stops before tombstones |
| D-82 freshness | Banked requests can never revalidate | Ordinary reauth fixed; cutoff assent survives writes, and recovery branch-cut can restore an earlier count |
| D-84 renewal | Only one missing CDDL token | Direct wire bug plus active/queued KEK coverage, wrap replacement, caps and certificate-delta authority |
| D-85 transfer | Signed charge and collect-all recovery complete | Charge number persists, but a flat non-persisted bundle cannot continue after one source is erased |
| Vector contract | Typed | `case_kind`, `inputs`, and `expected.result` remain unconstrained; no per-case schemas exist |
| Control precedence | Complete | Tenant cert stage cannot resolve genesis/admin/recovery signers; a separate arm-indexed pipeline is absent |

## Consolidated v0.5.4 blockers

### 1. Historical authorization, closure cutoffs and budget ordering

D-78 removes the old current-epoch test, but the admission pipeline still
checks current certificate/grant status before cutoff processing.
([renewal semantics](/Users/vm/owner-plane-d0a-spec.md:314),
[pipeline](/Users/vm/owner-plane-d0a-spec.md:1290))

Counterexample: O is admitted under grant G; later
`c.revoke_grant(G, cutoff-through-O)` says O stands. An incremental replica
keeps O. A fresh replica folding control first sees revoked G and returns
`no-grant` before the chain/cutoff stage. Renewal and device revocation have
the same cert-stage problem. Make signer/grant resolution historical and
cutoff-aware, or define a canonical cross-log interleaving.

The closure mechanism also needs:

- a continuation or accumulator, because strict advancement requires every
  live lineage while one operation carries at most 64 cutoffs;
- a missing-cutoff-head pending lifecycle when control arrives before the
  referenced tenant Head;
- one causal formula for a named Head in a lineage with several incomparable
  generation heads;
- exact zone/lineage equality for policy, bump and grant cutoffs;
- `op.capability_epoch >= grant.capability_epoch` before subtracting slack,
  preventing backdated use of newly issued authority.

([closure rule](/Users/vm/owner-plane-d0a-spec.md:1246),
[`zonecutoff`](/Users/vm/owner-plane-d0a-spec.md:2248))

D-79 chooses the correct accounting axis but not a canonical winner order.
With a two-operation budget, O1 can extend live generation 1 while W2 opens
generation 2 with `last_known="unknown"` and O2 follows W2. Arrival order
`O1,W2,O2` rejects O2; `W2,O2,O1` rejects O1. Both chain histories are valid
and eventually identical. Define a total cross-generation budget/raise-quota
order with deterministic re-evaluation, or signed reservations. Identify an
operation's budget window as the most recent bump at or before the control op
that opened its signed epoch—not the fold-current “last bump.”
([budget rule](/Users/vm/owner-plane-d0a-spec.md:364),
[unknown generation](/Users/vm/owner-plane-d0a-spec.md:1195))

### 2. Dense-feed finality and requester freshness

`through=N` states that the cutoff author observed `1..N`, but the bytes do
not prove that prefix. A has statements 1–100 before cutoff 100. B has 1–50,
folds the same cutoff, and later receives honest 51. The arrival-relative
`issuer-gap` rule freezes B's feed but not A's.
([dense rule](/Users/vm/owner-plane-d0a-spec.md:583),
[cutoff body](/Users/vm/owner-plane-d0a-spec.md:2197))

Make cutoff effect pending until the exact prefix is available, with a
missing-prefix outcome, or commit a signed/hash-chained feed head or
accumulator. Network delay cannot be the backfill predicate.

D-82 fixes two banked reauthorizations on an ordinary branch. It does not
stale a cutoff request after later writes or earlier cutoffs; decide whether a
signed cutoff is deliberately durable rollback authority or bind it to a
live-head/cutoff commitment. C3′ recovery can also cut a later reauth branch,
returning the derived count from N+1 to N and resurrecting an unconsumed
attestation. Bind `repoch` or a recovery-advanced nonce, and pin version zero
plus pre-state comparison.
([request rows](/Users/vm/owner-plane-d0a-spec.md:960),
[recovery](/Users/vm/owner-plane-d0a-spec.md:1003))

### 3. Checkpoint bytes and finality

Embedding the object fixes carriage. It does not close the object:

- `covers` may contain the 4096 heads allowed by Frontier inside a 64-KiB
  control operation, in addition to 256 retired heads and 64 proof positions;
  no full-versus-partial rule or joint cap exists;
- if 257 heads retire before the next checkpoint, the exact
  since-predecessor delta cannot fit, and a later page cannot repair an
  omission predating its immediate predecessor;
- one `zonecutoff` per `(zone,lineage)` still names one Head without saying
  how it treats several unknown-gap generation heads;
- live Frontiers are accepted-head-only, while a pending tail is said to be
  covered at its unaccepted signed coordinate;
- §4.6 implies checkpoint coverage retires incorporated heads, while D-80
  says exactly `retired` does;
- E6 forbids `v` on operation bodies, while embedded `checkpointobj` begins
  with `v:1`.

([caps](/Users/vm/owner-plane-d0a-spec.md:99),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2104),
[Frontier](/Users/vm/owner-plane-d0a-spec.md:482))

Define a bounded/paged checkpoint and position-fence representation with one
retirement formula. For historical removed witnesses, adopt the peer's
conservative forever-pending reading explicitly; do not let current policy B
stand in for operation-anchored policy A.

### 4. Rotation completion and renewal custody

Calling `c.wrap_add`'s “current epoch” the accepted control epoch is portable,
but creates this deadlock:

1. rotation N is accepted with a bounded recipient set;
2. N still needs N-epoch wrap-adds before RewrapComplete;
3. rotation N+1 is control-accepted;
4. N-epoch wrap-adds are no longer current, so N cannot complete, while N+1
   cannot Fence.

Require portable recipient closure before N+1 acceptance or let a wrap-add
name an accepted, queued, non-retired rotation. Define the signed intended
recipient set that RewrapComplete waits for.
([rotation queue](/Users/vm/owner-plane-d0a-spec.md:770),
[`c.wrap_add`](/Users/vm/owner-plane-d0a-spec.md:951))

Fence serialization is also one state early: N+1 may Fence after N's
`KekDestroyed`, while N's tombstones are state 6. A prior-manifest item can
therefore be non-tombstoned and wrapperless at N+1's frozen frontier. Serialize
on durable tombstones or a post-state-6 `RotationDone`.
([rotation states](/Users/vm/owner-plane-d0a-spec.md:718))

For renewal, first encode a real new-device/renewal union. Then pin:

- which active, authorized and queued epochs receive replacement wraps;
- how a renewed wrap supersedes the predecessor wrap with the same
  `(zone,epoch,device_id)` logical key;
- equality with the renewed certificate KEM key and whether KEM rotation is
  mandatory;
- safe joint wrap/history-cutoff caps and a continuation—the present maximum
  shapes can exceed 64 KiB and history is unbounded;
- allowed class/deadline changes, which can activate persistent device-bound
  grants even though renewal mints no new grant.

D-84 also does not make re-root the only hosted remedy at plane scope: a
genuinely new hosted-browser enrollment can receive a fresh audit grant and
budget. Narrow the claim to an exhausted existing device, accept enrollment
as a remedy, or use a plane-wide limit.

### 5. Transfer terminality after source erasure

Signed `bundle_size` preserves a number. It does not preserve the canonical
flat bundle. If release `{A,B}` is pending, A is cryptographically erased and
B remains unimported, the source cannot reconstruct A's statement/class floor
and therefore cannot reproduce the `H_bundle` preimage proving B's membership.
D-85's “continue every completable record” rule is unconstructible.
([bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:1591),
[recovery](/Users/vm/owner-plane-d0a-spec.md:851))

Choose one: abort all remaining imports on source erasure; block erasure until
referencing transfers are terminal; retain an encrypted bundle until
terminal; or use a Merkle bundle with per-record proofs. The same missing
preimage prevents a late reducer from independently checking that signed
`bundle_size` was truthful. A mixed erased-source/destination-rejection result
also needs per-record reasons, a `mixed` reason, or a scalar defined only as
the terminal trigger.

### 6. Control precedence and executable vector typing

The peer confirms an explicit tenant signature stage, but control operations
cannot literally run its device-certificate stage. Genesis, admin and recovery
keys resolve from different authority state, and recovery's validity must be
known before applying its C2 precedence exception. Define a separate ordered,
arm-indexed signer-resolution pipeline and root/recovery
`signer_alg`/`signer_key_id` checks.
([pipeline](/Users/vm/owner-plane-d0a-spec.md:1290),
[arms](/Users/vm/owner-plane-d0a-spec.md:969))

The vector schema adds `case_kind` as an arbitrary string while leaving
`inputs` as any object and `expected.result` as any JSON. Section 13.3 names
scenarios, not case-kind tokens or field schemas. D-85 promises the contract;
it does not encode it.
([schema](/Users/vm/owner-plane-d0a-spec.md:1682),
[claim](/Users/vm/owner-plane-d0a-spec.md:1739))

Add a real conditional `$defs` union or a normative companion schema before
fixtures are authored. Also move `H_genesis`/`H_cert(renews)` relations into
the main reference rules, require the active predecessor, and standardize the
hash notation.

## Exactness pins for the same patch

- State that epoch 1 is active after genesis/zone creation despite no Fence.
- Pin `KekDestroyed.epoch = new_epoch - 1` and
  `RewrapDone.count == |survivorset.pairs|`.
- Reject `c.grant` issuance to a revoked device and define conflicting
  repeated/continued cutoff entries.
- Add the peer's hosted cross-lineage cutoff negative.
- Fix the header/archive/synthesis provenance.

## Required decisions and closure order

V0.5.4 needs real choices despite the peer's “no owner decision” conclusion:

1. historical cutoff-aware authorization and a canonical budget allocation
   order;
2. prefix commitment/pending semantics and whether cutoff assent is durable
   rollback authority;
3. checkpoint paging/position fencing and the historical-witness liveness
   posture;
4. portable rotation recipient closure, post-tombstone completion and
   renewal/rotation interlock;
5. source-erasure policy for in-flight transfers;
6. the control signer pipeline and actual vector case schemas.

Recommended sequence:

1. Fix the renewal union immediately, but do not mistake it for the whole
   patch.
2. Close canonical replay: historical authorization, closure dependencies,
   budget order, dense feeds and requester recovery nonce.
3. Close checkpoint and rotation/renewal crash semantics.
4. Choose transfer-erasure behavior.
5. Encode the control pipeline and vector schema, then correct pins and
   provenance.
6. Build `owner-plane-core`, the corpus and harness; run family 14 and the
   prose↔vector discrepancy audit. Further changes should then be driven by
   executable failures rather than another confidence-only prose pass.

## Bottom line

The peer adds a real blocker and useful precision, and it is right that the
old D-69 counterexample, D-83 audit wire, and ordinary reauthorization replay
are fixed. Its review is too narrow to support freezing after one token: it
checks the newly added mechanisms mostly in isolation, while the surviving
defects arise where those mechanisms interact across logs, generations,
crash states and late replicas.

Cut v0.5.4 as a focused protocol-exactness patch. If it closes the items above,
freeze the prose as input to executable conformance—and let the core/corpus
audit, not another prose verdict, determine Gate A.
