# Review: D0-A Core + Memory normative specification v0.5.2

*2026-07-12. Reviewed against
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5.2,
the archived
[`v0.5.1 text`](/Users/vm/agenda-rfc-archive/2026-07-12-d0a-v0.5.1-as-reviewed.md),
and the
[`v0.5.1 synthesized review`](/Users/vm/owner-plane-d0a-spec-v0.5.1-synthesized-review.md).
This audits constructibility, replica convergence, crash recovery and
prose↔CDDL exactness. It does not treat the absent executable Gate-A artifacts
as a defect in the prose draft, but it does distinguish prose readiness from
Gate A itself.*

## Verdict

**Do not freeze the schema or start the canonical corpus yet. Cut a focused
v0.5.3, then build the executable Gate-A artifacts.**

V0.5.2 is materially better. Several previously blocking designs now have a
credible shape:

- D-69 supplies a signed policy selector rather than a local receipt-arrival
  position;
- D-70 makes service descriptors append-only history, permits ordinary key
  rotation and targets compromise cutoffs;
- D-71 removes the hosted revocation hash cycle through rotation-first
  exclusion and preserves `revocation_id` across renewal;
- D-73 closes the multi-epoch survivor omission;
- D-74 and D-75 state the intended audit partition and transfer authority;
- D-76 fixes `w.gen` retirement, trusted verbs, request collision and several
  genesis/outcome fields;
- D-77 adds a checkpoint object, persistent Fence coordinates, reference
  domains, dense proof feeds, safer element caps and registered storage
  shapes.

Keep those decisions. The remaining issues are narrower, but several still
produce different accepted state from the same eventual bytes or leave a
crash/replay path without sufficient data:

1. signed policy selection is deterministic, but **epoch currency is still
   arrival-order dependent**;
2. checkpoint objects have no carriage/storage lifecycle, duplicate authority
   fields, ambiguous pending/retirement semantics and unbounded histories;
3. proof cutoffs do not require an observed dense prefix, and D-72's resetting
   count does not make requester attestations fresh;
4. KEK rotation has no coherent state transition between control acceptance
   and Fence, nor an overlapping-rotation rule;
5. audit prose and normative CDDL still encode different bodies;
6. export bundle-byte charging cannot be replayed after erasure, and import
   proof policy uses the wrong zone's epoch unless further pinned;
7. genesis ceilings, reference hashes, outcome ordering and the vector
   envelope remain partially fixture-defined.

This is a **v0.5.3/no-Gate-A** verdict, not an architectural rejection.

## Closure against the v0.5.1 synthesis

| Prior repair | V0.5.2 disposition |
|---|---|
| Canonical proof policy | **Partial.** `policy(header.capability_epoch)` is a portable selector; stale-epoch admission still diverges by arrival order. |
| Service succession | **Substantially resolved.** Append-only descriptors, same-admin-epoch rotation, explicit policy rebind, historic keys and targeted receipt/lease cutoffs are present. |
| Hosted exclusion | **Substantially resolved.** Rotation-first removes the hash cycle and constrains recipient authority; >128 and last-device cases remain contradictory or unnamed. |
| Requester freshness | **Open.** `window_state` is a resetting/repeating count, so banked requests can become valid again. |
| Checkpoint/finality | **Partial.** Object/hash/tagged feeds exist; carriage, equality, caps, pending coverage and retirement remain undefined. |
| Survivor/erase | **Resolved in the membership formula.** Wrapper-current membership and durable Fence coordinates landed; rotation phase/serialization remains open. |
| Transfer | **Authority mostly resolved.** Flow deadline, whole matching, endpoint equality and egress coordinates landed; signed replayable charging and source-policy anchoring remain open. |
| Audit | **Resolved in prose, not wire.** Registry has the third trigger and partition; Appendix A and §11.7 retain old shapes/rules. |
| Frontier/genesis | **Mostly resolved.** `w.gen` is exact; checkpoint/cutoff retirement and grant class ceilings are not. |
| Stored/hashed schemas | **Improved.** Fence/survivor/bundle/checkpoint types exist; several mirrors, reference relations and object lifecycles remain incomplete. |
| Outcome/vector closure | **Partial.** Tenant request-fork and `cert-expired` are fixed; signature/control precedence and the vector input/result contract remain open. |

## Gate-A blockers

### 1. D-69 still permits two-replica admission divergence

`policy(header.capability_epoch)` now chooses the same proof policy on every
replica. That fixes the original D-57 selection problem. Admission still asks
whether the signed epoch is current relative to the fold's present control
state: an older epoch quarantines under `strict`, while an already admitted
operation is revisited only by compromise cutoffs.
([T2](/Users/vm/owner-plane-d0a-spec.md:552),
[epoch rules](/Users/vm/owner-plane-d0a-spec.md:1173))

Counterexample under the strict genesis policy:

1. O is signed at epoch 1.
2. P2 is a `c.zone_policy` operation advancing the zone to epoch 2.
3. Replica A receives O before P2 and admits O.
4. Replica B folds P2 first, then quarantines O as stale epoch.
5. A does not revisit O, because P2 is not a compromise cutoff.

Both replicas eventually hold identical signed objects. The named
two-replica equivalence vectors therefore cannot pass.

This needs one canonical tenant/control ordering rule. Viable designs include
a signed authoritative control position, deterministic re-evaluation of prior
admissions on every epoch transition, or anchor-only evaluation with an
explicit staleness rule that does not depend on arrival. Apply the same audit
to delayed grant revocation and certificate-status changes.

D-69 also conflates the policy axis with budget/raise-quota epochs.
`c.zone_policy` now advances `capability_epoch`, while §4.3 says budget state
resets **only** on `c.cap_epoch_bump`, and raise quota is per capability epoch.
([budget](/Users/vm/owner-plane-d0a-spec.md:360),
[policy epoch](/Users/vm/owner-plane-d0a-spec.md:1173))

If accounting keys on the operation epoch, every policy edit silently resets
budgets and raise quota. If it keys on the grant epoch, an ordinary capability
bump does not reset an old-but-still-usable grant. Separate policy and budget
epochs or explicitly ratify one reset model and amend every conflicting rule.

### 2. Checkpoint carriage, equality and retirement are not closed

The new `checkpointobj` is useful, but `c.checkpoint` stores only its hash.
There is no checkpoint frame, content-addressed object store, browser store,
resolution rule or missing-object outcome. After restart—or on an independent
replica—the reducer may have the signed control operation but not the object
whose bytes it must validate.
([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:921),
[object CDDL](/Users/vm/owner-plane-d0a-spec.md:1992),
[storage](/Users/vm/owner-plane-d0a-spec.md:827))

The duplicated fields also permit two meanings:

- `checkpointobj.proof_positions` sits under `H_ckpt`;
- `c.checkpoint.proof_cutoffs` is a separate root-signed array;
- only the `covers` equality is pinned.

Require canonical equality between the two proof sets—or remove one—and pin
`body.zone_id == object.zone_id == object.covers.zone_id`.

The state transition remains incomplete:

- `Checkpoint.retired` has no validity rule or exact Frontier transition;
- `c.cutoff` names one Head even though a lineage can have several live
  unknown-gap heads, so which heads it retires is undefined;
- a pending operation is asserted to occupy `covers`, but Frontier never says
  whether pending coordinates may be heads or how such heads validate;
- arbitrary `causal_references[]` carry only hashes, so their absent
  coordinates cannot be “covered.” Only a same-writer predecessor's coordinate
  is derivable without an additional hash→position proof.

Restrict causal hardening to the writer predecessor or add a committed
hash→coordinate index. Define exactly which validated retired heads the
checkpoint removes and how multi-head cutoff works.

Finally, checkpoint state is unbounded. `retired` and `proof_positions` have no
cap, paging, predecessor or accumulator; the in-control proof array must also
fit 64 KiB. On a forever plane, stable witness IDs can accumulate fresh-key
feed scopes on every renewal. Define the feed universe and a bounded
continuation/accumulator scheme before calling the hashed object closed.

### 3. Dense proof feeds and requester freshness remain replayable

T3 now requires dense `issuer_seq`, which is the right anti-backfill rule. A
cutoff still accepts an arbitrary `through: uint` without proving that every
slot through it was observed. If a cutoff of 100 is accepted when the replica
has statements 1–50, a compromised signer can later mint 51 and remain below
the cutoff. Replicas with different prefix availability also lack a common
pending/reject outcome.

Require `through = 0` as the empty-feed sentinel or an observed contiguous
feed head; add an `issuer-gap` lifecycle; and merge repeated compromise
cutoffs monotonically using the minimum effective position so a later cutoff
cannot resurrect authority. State explicitly that **device** compromise
cutoffs cover leases as D-70 now says for service cutoffs.
([T3](/Users/vm/owner-plane-d0a-spec.md:571),
[cutoff bodies](/Users/vm/owner-plane-d0a-spec.md:2028))

D-72's `window_state` is not monotonic. It counts `w.gen` operations since the
last reauthorization and resets on reauthorization. Two distinct requests can
be signed at state 0; after accepting the first, the state is 0 again and the
second banked request remains valid. Two requests signed at exhausted state N
also allow the second to become valid again after the next window consumes N
generations.

Use a monotonic lineage/window version or the last reauthorization operation
hash, incremented on every reauthorization. For `c.cutoff`, also bind the
current lineage-head/cutoff set: ordinary writes within one generation do not
change `window_state`, so a stale cutoff assent can otherwise be delayed
across later writes.

### 4. Revocation and hosted rotation still have finite-size edge failures

D-71 removes the hash cycle and correctly prevents new recipient authority.
Two scale cases remain:

- `revoke_grants` must equal the complete active set, but grants per device are
  unbounded and no continuation carries grant IDs;
- `c.revoke_zones` is keyed only by stable `revocation_id`, not the parent
  revoke operation, and has no explicit one-live-compound/conflicting-entry
  rule.

Derive grant revocation from the target, cap active grants, or continue grants;
bind continuations to the parent revoke hash or freeze exactly one compound.
Receipt cutoffs have the same unbounded-renewal problem and need a continuation
or accumulator.

Hosted exclusion says the rotation recipients equal all current holders minus
targets, while E8 says memberships above 128 stage remaining recipients via
later `c.wrap_add`. Without an explicit target/expected-recipient commitment,
omitted devices are indistinguishable from deferred wraps. A sole-device plane
cannot construct `ckekrotate.wraps: [+]` at all. Either cap hosted membership,
add explicit target/expected-recipient identity for staged rotations, and name
the last-device recovery lane, or define a wrapless terminal exclusion.
([hosted exclusion](/Users/vm/owner-plane-d0a-spec.md:1004),
[rotation CDDL](/Users/vm/owner-plane-d0a-spec.md:2099))

### 5. KEK rotation has two competing activation points

The rotation state machine says:

1. `RotationAccepted` makes the control operation durable;
2. Fence later stops serving the old epoch and starts new-epoch commits.

Elsewhere, accepting `c.kek_rotate` advances the zone's accepted current
epoch, and I3 serves only wrappers matching that current epoch. Between states
1 and 2, one rule serves only the new epoch while another still serves the old
epoch. Items have not yet been rewrapped, so this is an observable availability
and write-path contradiction.
([KEK epochs](/Users/vm/owner-plane-d0a-spec.md:624),
[I3](/Users/vm/owner-plane-d0a-spec.md:673),
[state machine](/Users/vm/owner-plane-d0a-spec.md:700))

Define distinct accepted, active-write and served epochs, with Fence as one
exact transition, or make control acceptance itself the activation point and
rewrite the state machine accordingly. Pin whether `Fence.kek_epoch` and
`RewrapDone.kek_epoch` name the retiring or new epoch.

Also serialize rotations per zone. A second accepted rotation can advance the
“current” epoch before the first finishes its staged `c.wrap_add` and rewrap
work; portable control admission cannot test a local RewrapDone. Introduce a
portable rotation-completion control state/operation or a deterministic local
queue whose epoch rules remain valid across multiple accepted rotations.

### 6. Audit prose and audit bytes still disagree

The registry now has the intended three triggers, multi-space scope, exact
partition and peer/session distinction. Normative Appendix A still defines:

- one `{zone, space}` rather than bounded `scope.spaces`;
- only a peer-flavored shape 4, with no mediated-session variant.

Section 11.7 also repeats only the old two trigger branches. The family-11
vectors cannot instantiate the normative D-74 body.
([audit registry](/Users/vm/owner-plane-d0a-spec.md:1346),
[audit CDDL](/Users/vm/owner-plane-d0a-spec.md:2225),
[view prose](/Users/vm/owner-plane-d0a-spec.md:1499))

Change the CDDL to the bounded canonical scope representation and closed
principal variants, then align §11.7. Pin an audited zero-result read to one
empty chunk (`index=0,count=1`) and either restrict one read to one zone or
represent a bounded zone/space coordinate set.

### 7. Export charging and import proof policy are not replayable

D-75 correctly bounds release authority by one matching flow. It now charges
canonical operation bytes plus canonical bundle bytes, while the global budget
rule still says operation bytes only. Bundles are deliberately never persisted
and are re-derived from live source plaintext.
([bundle/charge](/Users/vm/owner-plane-d0a-spec.md:1517),
[global budget](/Users/vm/owner-plane-d0a-spec.md:360))

After source erasure, a restart or new replica cannot recover the historical
bundle length from `content_digest`; hashes do not encode length. Later budget
admission can diverge. Add signed `bundle_size`/`charged_bundle_bytes` to
`mexportrel`, validate it while the bundle is available, and replay the signed
charge—or use a fixed content-independent formula. Amend the global budget
rule to permit named verb-specific surcharges.

The plane-import receipt uses a destination-zone operation but source-zone
witness policy. D-69 would ordinarily read the import header's
`capability_epoch`, which belongs to the destination zone. Anchor source policy
to the signed release's **source-zone capability epoch** instead.
([import proof](/Users/vm/owner-plane-d0a-spec.md:1566))

For PendingXfer recovery, state that re-derivation uses the frozen release
stamp and exact source records; later status, expiry or classification changes
must not alter the bundle. Specify canonical record order and whether a
permanent failure stops or continues remaining imports before XferAbort.

### 8. Remaining byte-level closure before corpus work

Resolve these together:

- Pin genesis ordinary and audit `class_ceiling`; otherwise a schema-valid
  public/internal grant cannot read the private home/audit spaces.
- Define `authorization_proof.genesis = H_genesis(descriptor)`,
  `cert.renews = H_cert(predecessor)`, every cert-reference field, and
  `header.signer_alg` equality with the resolved signer.
- Split E7 keys: receipt cutoffs by `key_id`; checkpoint proof sets by the full
  tagged issuer; add checkpoint-retired and survivor/bundle logical keys.
- Add `fence_frontier` to inline 0x14 RewrapDone and make 0x16's direct versus
  wrapped payload identical to Appendix A.
- Place signature verification in the ordered admission pipeline and define a
  control-operation precedence pipeline. Rename or split contextual edge/fold
  dispositions instead of claiming one lifecycle per outcome.
- Close the vector harness contract: `inputs` and `expected.result` are still
  arbitrary JSON. Use a `case_kind` union with typed per-family schemas, or
  canonical input/output byte blobs; require unique RNG draw names.
- Fix the stale header/archive/synthesis attribution, D-33's one-head wording,
  and D-64/§11.7's superseded audit trigger.

The lowered 128+128 rotation limits appear to fit 64 KiB, but the published
55-KiB figure is not a true maximum-width proof. Keep the named generated-size
vector and let the encoder establish the exact bound before Gate A.

## Conformance status and recommendation

Gate A remains mechanically unavailable: no `owner-plane-core`, corpus or
harness exists, and family 14 remains open. More importantly, the prose and
CDDL are not yet stable enough for those artifacts—the epoch-arrival,
checkpoint, rotation-phase and audit-wire defects would force the harness to
invent behavior.

Recommended order for v0.5.3:

1. canonical tenant/control ordering and separate policy/budget epoch
   semantics;
2. checkpoint carriage/equality/caps, pending coverage and retirement;
3. dense-prefix cutoffs, monotonic requester version and bounded revocation;
4. rotation activation/serialization and hosted staged-recipient semantics;
5. audit CDDL and export signed charging/source-policy anchor;
6. genesis/reference/frame/E7/outcome/vector/drift sweep;
7. then build the core, corpus and harness and let executable discrepancies
   decide Gate A.

## Bottom line

V0.5.2 successfully repairs the service-key lifecycle, hosted hash cycle,
survivor formula, flow authority and several exact schemas. It still does not
have one replica-independent admission rule or a complete checkpoint/rotation
lifecycle. Cut v0.5.3 with the focused repairs above; only then move from prose
review to the executable Gate-A corpus.
