# Synthesized review: D0-A Core + Memory normative specification v0.5.2

*2026-07-12. Adjudicated synthesis of
[`owner-plane-d0a-spec-v0.5.2-review.md`](/Users/vm/owner-plane-d0a-spec-v0.5.2-review.md)
and
[`owner-plane-d0a-spec-v0.5.2-review-2.md`](/Users/vm/owner-plane-d0a-spec-v0.5.2-review-2.md),
verified against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.2.
This document resolves disagreements between the reviews; it is not an
unranked union of their findings.*

## Executive verdict

**Cut a focused v0.5.3 before schema freeze or corpus construction. Do not
call it a terminal prose patch in advance: several remaining repairs require
protocol choices and wire or stored-byte changes. Gate A remains unavailable.**

The peer review is useful and substantially independent. It confirms that
v0.5.2 is the strongest cut so far, and it correctly identifies three defects
that should lead the next patch:

- D-74's ratified multi-space audit body and peer/session principal split
  never reached Appendix A;
- cross-zone import still invokes the abolished local “admission position”
  and has no signed source-zone policy selector;
- D-69 made `c.zone_policy` an epoch event without reconciling the rules for
  budget and raise-quota resets.

It also adds two useful findings: zone-less lineage reauthorization cannot
unambiguously recompute a per-zone `window_state`, and hosted certificate
renewal can mint a fresh audit grant—contradicting the claim that re-rooting is
the only in-ceiling audit-budget remedy.

The peer's central conclusion is too optimistic. It establishes that D-69
selects the same **policy** from an operation's signed epoch; it does not
establish that two replicas make the same **admission decision** when a policy
epoch operation and a tenant operation arrive in opposite orders. It likewise
treats a resetting `w.gen` count as a freshness nonce, overlooks the missing
checkpoint-object lifecycle and dense-prefix precondition, and declares the
rotation/storage/transfer byte sweep complete despite remaining
constructibility defects.

The adjudicated disposition is:

- **Keep:** D-70 service-key history and succession; D-71's rotation-first,
  non-cyclic hosted exclusion; D-73's wrapper-current survivor formula; the
  central D-75 flow-authority rules; D-76's writer/genesis/outcome
  improvements; D-77's durable Fence coordinates and versioned object shapes.
- **Partial:** D-69 fixes policy selection but not cross-log admission order;
  D-74 is correct in prose but absent from the wire; D-75 lacks replayable
  bundle charging; D-77 introduces the checkpoint object but does not make it
  available, bounded, or deterministic.
- **Not closed:** D-72 freshness, dense cutoff finality, KEK activation and
  rotation serialization, checkpoint retirement, several reference and frame
  identities, reducer precedence, and the conformance-vector contract.
- **Schema freeze:** no.
- **Gate A:** no; the core, corpus and harness do not yet exist, and family 14
  remains open.
- **Durable P1 writes:** still prohibited under the unchanged Gate-B and
  umbrella prerequisites.

## Assessment of the peer review

### Findings to adopt

1. **Audit B1 is fully confirmed.** The operation registry requires bounded
   `scope.spaces`, exact multi-space partitioning, and separate peer/session
   mediated principals, while `maudit` still has one `{zone, space}` and
   `auditprin` still has only the peer-shaped variant.
   ([registry](/Users/vm/owner-plane-d0a-spec.md:1346),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2225))
2. **Import H1 is fully confirmed.** A destination-zone import cannot use its
   own epoch to select source-zone witness policy. Qualification should use
   `policy(release.header.capability_epoch)` in the source zone.
   ([import rule](/Users/vm/owner-plane-d0a-spec.md:1566))
3. **Budget H2 is fully confirmed.** `c.zone_policy` now advances
   `capability_epoch`; §4.3 says only `c.cap_epoch_bump` resets budgets; §9.4
   says accounting resets per epoch; raise quota is also per capability epoch.
   This is a normative contradiction, not merely stale prose.
   ([budget](/Users/vm/owner-plane-d0a-spec.md:360),
   [epochs](/Users/vm/owner-plane-d0a-spec.md:1173))
4. **Peer M1 is valid, but it is secondary to a deeper replay defect.**
   Reauthorization carries no zone while the count is defined per
   `(zone,lineage)`. Sum, map, or one-zone interpretations are all possible.
   ([reauthorization](/Users/vm/owner-plane-d0a-spec.md:917),
   [accounting](/Users/vm/owner-plane-d0a-spec.md:1146))
5. **Peer M2 correctly spots missing checkpoint construction and proof-set
   equality.** Those are part of a larger checkpoint lifecycle blocker below.
6. **Peer M3 is valid.** Hosted `c.enroll` permits renewal with fresh grants
   for the renewed device, including `audit.write`; a fresh `grant_id` has a
   fresh budget. “Hosted remedy = re-root; no in-ceiling refresh exists” is
   therefore false unless renewal grants are newly forbidden.
   ([enrollment](/Users/vm/owner-plane-d0a-spec.md:905),
   [hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:995),
   [audit rule](/Users/vm/owner-plane-d0a-spec.md:1346))
7. **Adopt the peer's cleanup pins:** repair E7's proof-set key, §11.7's
   two-branch audit restatement, the stale document provenance, D-33/D-67
   refinement notes, hosted/trusted exclusion-freeze scope, and
   `grant_epoch_slack` aging on policy edits.

### Where the peer is too optimistic

| Subject | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-69 | Arrival-order divergence is gone | Policy selection is portable; epoch currency still depends on the fold's current control state, producing permanent two-replica divergence under `strict` |
| D-72 | Banked requests go stale | The count resets and later repeats; distinct banked requests can become valid again, while cutoff assent survives ordinary writes |
| Dense proof feeds | Complete anti-backfill | `through=N` is not required to name an observed contiguous prefix, so unused sequence positions below a cutoff remain mintable |
| Checkpoint | Two-sentence medium repair | Protocol blocker: no carriage/store, duplicated authority, undefined retirement/pending coverage, unbounded histories |
| Rotation/storage | Durable-Fence sweep complete | Activation occurs at two different points; rotations can overlap; inline 0x14 still disagrees with Appendix A |
| Transfer | Bundle accounting complete | Historical bundle-byte charge cannot be reconstructed after source erasure because the bundle is deliberately not stored |
| Genesis/references | Exactness discharged | Grant class ceilings, genesis/renewal hashes, signer-algorithm equality, and several reference identities remain open |
| Reducer/vectors | One deterministic path | Signature/control precedence is incomplete, and vector inputs/results remain fixture-defined arbitrary JSON |
| V0.5.3 | One CDDL edit and a few sentences; terminal | Focused, but includes real protocol and wire decisions; only executable discrepancy work can establish terminality |

One earlier concern should be **downgraded** in the synthesis. A hosted
rotation with more than 128 current holders can treat omitted holders as
temporarily excluded and then re-admit them with the expressly permitted
current-epoch `c.wrap_add`. That is a plausible construction, although the
specification should state and vector it. Likewise, last-device exclusion
needs a named precondition or enroll/recovery lane, but is not by itself a
Gate-A blocker.

## Consolidated v0.5.3 blockers

### 1. Canonical tenant/control ordering and epoch semantics

D-69 correctly defines `policy(e)`. Epoch **currency** still evaluates against
the fold's current control frontier: an older epoch quarantines under
`strict`, while accepted operations are revisited only by compromise cutoffs.
([T2](/Users/vm/owner-plane-d0a-spec.md:541),
[currency](/Users/vm/owner-plane-d0a-spec.md:1173))

Minimal counterexample under the pinned strict genesis policy:

1. Tenant operation O is signed at epoch 1.
2. P2, a valid `c.zone_policy`, advances the zone to epoch 2.
3. Replica A folds O before P2 and accepts it.
4. Replica B folds P2 before O and quarantines O as stale.
5. A does not revisit O because P2 is not a compromise cutoff.

Both replicas eventually possess the same signed objects. The dev
authorization arm has no authoritative control position; its
`ctrl_frontier`, where present elsewhere, is diagnostic. Thus the specified
family-7/family-9 order-equivalence vectors cannot pass as written.

Choose one canonical rule: carry an authoritative signed control anchor,
deterministically re-evaluate prior admissions on epoch transitions, or define
anchor-only currency semantics independent of local fold order. Apply the same
model to delayed certificate status and grant revocation.

Separately choose the accounting axis. Prefer distinct policy and
budget/authority epochs unless every policy edit is deliberately intended to
re-arm budgets, raise quotas, and age `grant_epoch_slack`. Whichever ruling is
chosen must align §4.3, §9.4, the grant key, `c.cap_epoch_bump`, and family 10.

### 2. Proof-cutoff finality and requester freshness

Dense issuer numbering helps only if a cutoff names an **observed dense
prefix**. The current schemas accept a bare `through: uint`. If statements
1–50 are known when a cutoff of 100 is accepted, a compromised issuer can
later mint 51: it lies below the cutoff and does not collide with an existing
statement. Replicas that possess different portions of the prefix also have
no shared pending/reject outcome.
([dense feeds](/Users/vm/owner-plane-d0a-spec.md:571),
[cutoff schemas](/Users/vm/owner-plane-d0a-spec.md:2028))

Require `through=0` as the empty-feed sentinel or `through` to identify an
observed contiguous feed head; define an `issuer-gap` lifecycle; merge repeated
cutoffs monotonically using the minimum effective position; and state that
device compromise cutoffs cover leases as service-key cutoffs do.

D-72's `window_state` is a repeating count, not freshness. At exhausted state
N, a device can sign requests A and B with different `request_id`s. A resets
the count; after N more accepted generations, the count is N again and banked
B becomes valid. A cutoff request is weaker still: normal writes in the same
generation do not move the count, so old assent can be delayed across later
state.
([control rows](/Users/vm/owner-plane-d0a-spec.md:917),
[window definition](/Users/vm/owner-plane-d0a-spec.md:1146))

Use a monotonic lineage reauthorization version or last-reauthorization hash.
For cutoff, also bind the named zone's current live-head/cutoff state. This
fixes both replay and the peer's zone-less/per-zone ambiguity.

Revocation has one related boundedness defect: `revoke_grants` must enumerate
the target's complete active grant set, but active grants are unbounded and
continuations carry only rotations and zone cutoffs. This is eventually
unconstructible, especially for hosted planes that forbid standalone grant
narrowing. Derive revocation from the target, cap active grants, or continue
grant IDs. Bind `c.revoke_zones` to one parent ceremony—or explicitly permit
only one live compound per `revocation_id`—and define repeated-cutoff merge
semantics.

### 3. Checkpoint carriage, construction, retirement and bounds

`c.checkpoint` carries `H_ckpt`, not the `checkpointobj`. No control frame,
content-addressed object store, browser store, reducer input, resolution rule,
or missing-object outcome makes the preimage available after restart or on an
independent replica.
([checkpoint operation](/Users/vm/owner-plane-d0a-spec.md:921),
[local stores](/Users/vm/owner-plane-d0a-spec.md:827),
[object CDDL](/Users/vm/owner-plane-d0a-spec.md:1992))

Either embed the object in `c.checkpoint`, or define its durable carriage and
resolution lifecycle. Then close these semantics:

- require an exact relation between `checkpointobj.proof_positions` and
  `c.checkpoint.proof_cutoffs`—normally canonical equality—and pin all three
  zone IDs;
- define how `retired` is constructed and validated, and whether the fold
  removes `retired`, covered live heads, or their intersection;
- define how a `c.cutoff` retires several live unknown-gap heads when its body
  currently names only one head;
- define whether and how pending coordinates enter a valid Frontier;
- restrict checkpoint hardening to the derivable same-writer predecessor, or
  add a committed hash→coordinate proof for arbitrary causal references;
- cap/page/accumulate `retired` and `proof_positions`, and ensure the in-body
  proof set remains encodable under 64 KiB over a forever key history.

Until those choices land, a fixture must invent both checkpoint bytes and the
state transition they authorize.

### 4. KEK rotation activation and serialization

Three rules disagree between control acceptance and Fence:

- accepting `c.kek_rotate` advances the accepted current KEK epoch;
- I3 serves only the wrapper matching that accepted current epoch;
- the state machine says the old epoch stops being served and new-epoch
  commits begin only at Fence.

([I3](/Users/vm/owner-plane-d0a-spec.md:673),
[state machine](/Users/vm/owner-plane-d0a-spec.md:700))

Define accepted, active-write, and served epochs with one exact Fence
transition, or activate at control acceptance and rewrite the machine. Pin
whether Fence/RewrapDone `kek_epoch` is the retiring or new epoch.

Also serialize or deterministically queue rotations per zone. A second
control rotation can be accepted while the first is staging wraps or
rewrapping, and portable control admission cannot observe a local
`RewrapDone`. The rule must remain valid across two accepted rotations and a
crash.

The stored-frame mirror is still inconsistent: inline 0x14 omits
`fence_frontier`, while the state machine and Appendix A require it.
([frame table](/Users/vm/owner-plane-d0a-spec.md:777),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:2150))

For hosted membership above 128, explicitly state the safe staged ceremony:
the first rotation temporarily excludes the complement of its bounded wrap
set, then `c.wrap_add` re-admits non-target holders before completion. Vector
that case and state the last-holder precondition/recovery path.

### 5. Audit bytes and exact read partition

D-74's prose should be retained, but it needs an actual wire representation:

- bounded multi-space scope rather than singular `{zone,space}`;
- distinct closed peer and mediated-session principal variants;
- the third audit-space trigger in §11.7;
- one deterministic zero-result partition, preferably one empty chunk at
  `(index=0,count=1)`.

Do not automatically adopt `{zone, spaces}` without deciding whether one read
is restricted to one zone. If reads may span zones, encode a bounded keyed set
of `{zone_id,space_id}` coordinates instead. Either decision must preserve the
one-Txn/4096-result proof and receive family-11 vectors.

### 6. Replayable transfer accounting and recovery

D-75 says a release charges operation bytes plus canonical bundle bytes.
Bundles are deliberately never persisted and are reconstructed from source
plaintext. After a source is cryptographically erased, `content_digest`
cannot reveal the historical encoded length, so a restart or new replica
cannot replay budget consumption. Later admissions can diverge.
([charge](/Users/vm/owner-plane-d0a-spec.md:1546),
[bundle lifecycle](/Users/vm/owner-plane-d0a-spec.md:1517))

Add signed `bundle_size`/`charged_bundle_bytes` to `mexportrel`, validate it
against the canonical bundle while readable, and replay that signed value; or
adopt a fixed content-independent charge. Amend the global §4.3 rule to name
verb-specific surcharges.

For imports, replace the local admission-position rule with source-zone
`policy(release.header.capability_epoch)`. For PendingXfer recovery, use the
exact records and classification at the frozen release stamp—not later
status, expiry, or classification—and define canonical record order plus
stop/continue behavior after a permanent per-record failure.

### 7. Remaining closed-byte, reducer and conformance work

Complete these in the same exactness sweep:

- Pin ordinary and audit genesis `class_ceiling`; a schema-valid
  public/internal grant cannot operate in the private home/audit spaces.
- Define `authorization_proof.genesis = H_genesis(descriptor)`,
  `cert.renews = H_cert(predecessor)`, every certificate-reference domain,
  and `header.signer_alg` equality with the resolved key.
- Split E7 keys: receipt cutoffs by `key_id`; checkpoint proof sets by full
  tagged issuer; add the omitted checkpoint-retired, survivor, and bundle
  logical keys.
- Make 0x16's direct/wrapped receipt payload wording byte-identical to
  Appendix A.
- Place signature verification in the ordered admission algorithm and define
  equivalent precedence for control operations. Describe §10.5 as contextual
  edge/fold dispositions or split the outcomes instead of claiming one
  lifecycle where the table gives two.
- Close the vector contract. `inputs` accepts any object and
  `expected.result` any JSON, forcing each harness to invent family-specific
  semantics. Use a typed `case_kind` union or canonical input/output bytes,
  and require unique RNG draw names.
- Repair the stale header/archive/synthesis attribution, D-33's historical
  one-head wording, D-64's hosted-remedy claim, and §11.7's trigger drift.

The peer is right that the 128+128 rotation still fits the control cap, but
“306 B exactly” is true only for a small epoch. Under maximum-width legal
CBOR, a `kekwrap` is 314 B and an `erasemref` is 132 B. The full operation is
still about 60 KiB, leaving margin under 64 KiB; publish the encoder-generated
exact value rather than the current approximate 14/55-KiB subfigures.

## Owner decisions required for v0.5.3

Most repairs are mechanical once these choices are explicit:

1. What canonical tenant/control ordering makes old-epoch admission
   replica-independent?
2. Are policy epochs also budget/raise-quota epochs, or are those separate?
3. Is the Checkpoint object embedded in control or stored/resolved by hash,
   and what exact retired-head transition does it authorize?
4. Is Fence the KEK activation point, and how are overlapping rotations
   serialized?
5. Are audited reads one-zone, or does audit scope encode zone/space pairs?
6. Is hosted certificate renewal the supported budget-refresh ceremony, or
   may renewals not mint fresh grants?

## Recommended closure order

1. Canonical epoch ordering, accounting-axis ruling, dense cutoffs, and
   monotonic requester freshness.
2. Checkpoint carriage/equality/retirement/bounds.
3. KEK activation/serialization and exact stored frames.
4. Audit CDDL and transfer signed charging/source-policy anchoring.
5. Revocation bounds, genesis/reference/E7/outcome exactness, and the renewal
   budget ruling.
6. Freeze the family-specific vector contract, then build
   `owner-plane-core`, the corpus, and every required harness surface.
7. Run the prose↔vector discrepancy audit and record family 14; only that
   evidence can support Gate A or the claim that specification prose is
   terminal.

## Bottom line

V0.5.2 should be credited for solving the service-key lifecycle, hosted hash
cycle, survivor definition, much of transfer authority, and several durable
storage shapes. The peer review strengthens the next patch by finding the
audit-wire omission, source-policy anchor, budget-epoch collision,
cross-zone requester ambiguity, and renewal-budget contradiction.

It does not overturn the no-freeze verdict. Policy selection is now
deterministic, but admission order is not; requester state is signed, but not
fresh; checkpoint bytes exist as a type, but not as a replayable object; Fence
is durable, but rotation activation is inconsistent. V0.5.3 should repair
those protocol seams, after which the executable corpus—not another prose
confidence judgment—should decide Gate A.
