# Review: D0-A Core + Memory normative specification v0.5

*2026-07-12. Strict closure audit of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md) v0.5 against
[`owner-plane-d0a-spec-v0.4-synthesized-review.md`](/Users/vm/owner-plane-d0a-spec-v0.4-synthesized-review.md),
with a direct diff against the archived v0.4. Independent passes covered
proof/control authority, Memory/IAM, and schema/storage/conformance. The test is
whether a vector author can derive one result from canonical bytes without
inventing a security transition.*

## Executive verdict

**V0.5 is the strongest draft so far and closes most of the v0.4 review in
substance. It is not yet ready for canonical-vector freeze. Cut one narrow
v0.5.1 protocol-exactness patch, then begin the discrepancy-audit corpus.**

This revision gets many difficult choices right:

- it introduces a resolvable service-key descriptor, one Connect predicate,
  explicit lease issuers, fresh-key renewal and separate receipt cutoffs;
- it chooses and documents early exclusion, all-zone coverage, generation
  accounting, hosted unknown-head retirement and the grandfathered deadline
  residual;
- `audit.write`, capability epoch 1 and the `w.gen` space/kind exception now
  exist;
- Memory relations are principal-level and actually consumed;
- assert and diary compounds, evidence depth, record-level transfer identity,
  typed erase entries and 48-byte wrappers are materially repaired;
- the vector JSON is valid and no longer relies on JSON property order;
- both canonical policy constants independently reproduce exactly.

The remaining defects are no longer architectural. They are concentrated at
six joins where the new prose promises more than the signed or stored objects
can prove:

1. receipts have no immutable policy/control binding despite a non-retroactive
   qualification rule;
2. hosted self-cutoff is called requester-attested, but the operation has no
   requester field;
3. checkpoint/frontier scope and proof-feed identity remain ambiguous;
4. audit chunking, triggering and hosted authority do not form one executable
   transaction;
5. bundle classification/expiry and failed-transfer completion remain open;
6. erase tombstones and survivor commitments do not prove the complete frozen
   target set.

Those joins are Gate-A issues: replicas or fixtures can produce different
outcomes from identical bytes. They are still a bounded repair, not a reason to
reopen the owner-plane design.

Recommended disposition:

- **Direction and owner rulings:** accepted.
- **V0.5 as implementation foundation:** yes.
- **Unaffected encoding/crypto scaffolding:** safe to begin.
- **Canonical reducer vectors:** wait for v0.5.1.
- **Gate A:** no-go today; the core, corpus, harness and offline result also do
  not yet exist.
- **Durable P1 Memory writes:** remain prohibited.

## What genuinely closed

Do not re-litigate these v0.5 decisions:

1. **Service-key descriptor shape.** `{service, alg, pk,
   valid_from_admin_epoch}` is the right resolver behind a service `key_id`.
2. **Time posture choices.** Connect has one enable predicate; lease issuers
   use the witness set; device renewal requires a fresh signing key; renewal
   does not ratify pending operations; the grandfathered no-deadline lane is an
   explicit residual rather than an accidental promise.
3. **Early exclusion.** Separately effectful rotations are now an explicit
   owner choice instead of being mislabeled atomic.
4. **Generation/epoch mechanics.** Capability epoch 1, precise `w.gen` scope
   bypass and per-zone/lineage window accounting are sound.
5. **Memory principals.** `P = (lineage, actor.kind, actor.id)` closes the
   shared-device authorship hole, and `authorized(j)` now evaluates relation.
6. **Memory compounds and evidence.** Assert/diary halves have portable
   linkage; evidence uses one locator domain and a nonrecursive shallow floor;
   cross-plane evidence remains safely unresolved.
7. **Record-level transfer direction.** `(from_plane, export_id, source_op)`
   and completed source-op sets are the right cardinality.
8. **Typed erase direction.** `{item_addr, erase_op}`, a 48-byte wrapped DEK
   and survivor pairs are improvements to retain.
9. **Conformance constants.** The vector JSON parses; `workflow-v1` remains
   1133 bytes with hash
   `219b9baced57e8fdc06f56119e25dd02403cc4076cec04448e4957d0fa91dd1c`;
   `owner-v1` remains 571 bytes with hash
   `d7d5559a6c3462426cb63eed84b05c569f2571da0cc6b5009edc79910f1e4486`.

The findings below finish these mechanisms rather than replace them.

## Freeze blocker 1: non-retroactive proof authority is not present in the bytes

T2 says receipt qualification is evaluated against the witness policy at the
control-frontier position where the receipt is admitted. But every receipt
contains only issuer, plane, zone, subject, time and sequence. It contains no
policy-op hash, policy hash, admin epoch or control frontier. Receipt frames
also have no writer-chain position. Two replicas receiving the same receipt on
opposite sides of a policy change can therefore choose different qualifying
policies.
([T2](/Users/vm/owner-plane-d0a-spec.md:496),
[receipt shapes](/Users/vm/owner-plane-d0a-spec.md:469),
[Receipt frame](/Users/vm/owner-plane-d0a-spec.md:671))

T4 simultaneously says validation uses the fold's control frontier, which is a
different, retroactive rule. Add an immutable accepted policy/control-event
reference to receipts and leases, or choose one explicitly retroactive rule.
`LeaseStmt.ctrl_frontier` cannot solve this while it remains diagnostic.

The same missing binding leaves service history incomplete. `c.service_key`
can install and rotate keys, but has no compromise/revocation cutoff,
`valid_from_admin_epoch` is not constrained to the accepting epoch, and old
keys validate receipts “admitted under” epochs that no receipt identifies.
Define service-key historical validity and counter cutoffs over the same
policy/control reference. Pin sequence 1 for a fresh service key and whether
receipts and leases consume one shared issuer counter.

Checkpoint finality is only partial:

- `proof_cutoffs = {key_id, through}` is unambiguous only if control state
  enforces one unique device/service scope per key ID; no such registration or
  resolution rule exists;
- the checkpoint body has no zone, although witness policy is per zone;
- hardening prose names missing accept receipts but not `lease-missing`;
- “every qualified witness feed” cannot be determined until proof-policy
  binding is exact.

Use a tagged issuer scope, bind the checkpoint to a zone and historical policy,
and apply the rule to every missing proof type.

Likewise, define “already admitted at the pre-renewal frontier” mechanically:
the tenant Head limits candidate operations, but the cutoff itself must never
waive an operation's still-missing receipt or lease. That preserves D-49
without turning an administrator's history cutoff into implicit time proof.

One authority hole remains in the declared grandfathered lane. Finite budgets
are required only while the current policy says `deadline_fallback =
"budgets"`. After a zone switches to `"fail-closed"`, an administrator can
issue a budgetless, deadline-free grant to a grandfathered deadline-free
certificate. That path has neither time evidence nor a finite budget despite
§9.1 saying budgets govern it. Require a finite budget whenever the effective
cert/grant path has no deadline, independently of the current policy label.

**Closure test:** proof qualification is a pure function of signed bytes and
accepted control history; device and service compromise have sequence-domain
cutoffs; checkpoint finality covers receipts and leases in one typed zone; no
write lane lacks both signed time and a finite budget.

## Freeze blocker 2: hosted continuity and revocation claim authority the bodies do not carry

The hosted ceiling says `c.cutoff` is requester-attested and limited to the
requesting device's own lineage. Yet `ccutoff = zonecutoff` contains only zone,
lineage and head—no certificate, signature, nonce or attestation. The reducer
cannot distinguish a self-cutoff from one browser cutting another device's
lineage.
([hosted rule](/Users/vm/owner-plane-d0a-spec.md:895),
[`ccutoff`](/Users/vm/owner-plane-d0a-spec.md:1796))

`c.lineage_reauth` has a requester signature, but “single-use” is not yet true:
no rule makes a control `request_id` unique or consumed, `ctrl_frontier` need
not be current, and `device_cert` is outside the signed statement. The same
attestation can be placed in a later control sequence with the same request ID.
Bind the current window/control sequence and certificate into the signed
statement, or maintain a portable consumed-nonce set. Give `c.cutoff` its own
closed, domain-separated requester proof.

The hosted operation list also contradicts the mechanisms it needs:

- hosted genesis and §11.1 require `audit.write`, but the exhaustive hosted
  safe verb list omits it;
- hosted revocation now references separately committed `c.kek_rotate`
  operations, but the admitted-control list does not affirmatively allow
  empty-manifest exclusion rotations;
- the list omits `c.recovery_succession`, even though the preceding rule says
  any valid recovery succession lifts the ceiling.

Revocation completeness needs exact set constraints too. `revoke_grants` must
equal every active grant for the target, and each referenced rotation must be
the current accepted rotation after the target's last wrap-add—not merely an
old rotation that once excluded it. `c.revoke_zones` extends rotation
references but not per-zone cutoffs; an unbounded-zone device can still exceed
the 64-KiB original revocation operation. Specify continuation semantics for
both, including whether a pending control operation advances the single
control chain.

Cutoff shapes also need a zero-history case. A newly enrolled device can be
renewed or revoked before its first tenant write, but renewal/revocation require
nonempty accepted-through Heads and Head has no genesis/empty sentinel. Define
`none` or a typed generation-start cutoff. Require
`zonecutoff.lineage == accepted_through.lineage` and bind the Head to the named
zone. On renewal, `cenroll.lineage` remains mandatory even though the registry
says renewal leaves the existing lineage untouched; make it absent on renewal
or require exact equality and call it validation-only.

Finally, `issued_admin_epoch` remains an unconsumed field on certificates and
grants. Require equality to current admin epoch at issuance or declare it
audit-only. Apply the same exactness to `c.service_key.valid_from_admin_epoch`.

**Closure test:** hosted admission is one exhaustive set; every self-service
operation proves “self” from its body; attestations are single-use; all active
grants, wraps, zones and cutoffs—including a never-used lineage—fit the
revocation protocol at maximum legal scope.

## Freeze blocker 3: frontier, checkpoint and genesis scope remain ambiguous

Writer chains and generation windows are per `(zone, lineage, generation)`,
but `Frontier.Head` contains only `(lineage, gen, seq, op)` and permits one head
per `(lineage, gen)`. The likely intent is a **zone-local Frontier**—the
`ReplicaAck` separately binds `zone_id`—but the specification never says so.
If Frontier is plane-wide, one device writing the same lineage/generation in
two zones is unrepresentable; if it is zone-local, `c.checkpoint` still lacks
the zone required to select its frontier and witness policy.
([chain scope](/Users/vm/owner-plane-d0a-spec.md:423),
[Frontier](/Users/vm/owner-plane-d0a-spec.md:438),
[`c.checkpoint`](/Users/vm/owner-plane-d0a-spec.md:1799))

Pin Frontier as zone-local everywhere or add `zone_id` to each Head. Then bind
checkpoint `covers`, proof cutoffs and GC-fence position to that same zone and
define `head.lineage == zonecutoff.lineage`.

Close the writer-chain arithmetic at the same time: first sequence, exact
successor increment, and whether `w.gen(g)` must open precisely the next
generation are currently only implied by “chain intact” and “next.” Without an
exact `g = prior_max + 1` rule, one accepted `w.gen` can jump arbitrarily while
consuming only one generation-window opening.

Genesis is much better but still does not make every schema-valid body
bootable. Complete the cross-field table:

- hosted provenance requires a hosted-browser first certificate; trusted
  provenance requires a class capable of the promised boot write;
- cert/grant `issued_admin_epoch = 1`, grant capability epoch = 1;
- the ordinary grant names the genesis zone, lineage, Memory tenant and home
  space; the audit grant names only the audit space and `audit.write`;
- home/audit spaces and grants are distinct;
- every wrap's plane, zone, epoch, recipient device and KEM key match the
  descriptor, zone and first certificate;
- `zone_policy.zone_id` and all nested IDs match their enclosing objects.

**Closure test:** “valid genesis” and “bootable genesis” are the same set, and
every frontier/checkpoint hash has one explicit zone scope and one consecutive
writer/generation interpretation.

## Freeze blocker 4: audit has a closed failure name but not a closed transaction

The audit trigger is still “sensitive-space read.” A space has a content class
minimum, while individual claims have effective classifications. State whether
auditing triggers on the space minimum, any returned item with effective
`sensitive`, or both; otherwise a sensitive claim in the default private home
can be read without a deterministic audit decision.

`maudit.principal` is unconstrained text and the body does not bind an audit row
to the authenticated edge requester, read request, result scope or chunk
position. Define a typed edge-principal descriptor and a read/chunk identity,
then make the service writer derive—not accept—principal, scope and result IDs.

Chunking also exceeds the physical transaction model. Each row holds at most
256 IDs; one Txn holds at most 16 records. The promise that every result set is
chunked inside one “read transaction” does not say whether that means one 0x18
Txn or a logical transaction spanning frames. If it means one physical Txn,
4096 results is the ceiling; if it spans frames, partial-audit crash behavior is
undefined. Pin the cap/batch shape or add a crash-safe multi-Txn
prepare/commit protocol.

Audit-budget exhaustion is called owner-remediable, but the hosted remedy is
not named. Define a bounded hosted audit-budget refresh ceremony and prove it
remains within the hosted ceiling; absent one, the only evident recovery is a
ceiling-lifting re-root. Also replace §11.7's stale daemon-only writer wording
with the new any-device internal service writer.

**Closure test:** an edge read has one typed trigger and principal, all chunks
are durably attributable to that read under explicit partial-crash semantics,
results never precede the required rows, and budget exhaustion has one portable
hosted remedy.

## Freeze blocker 5: transfer identity is record-level, but bundle and expiry authority are not closed

`bundle` and `bundlerec` have an inline normative CDDL-like shape but remain
absent from Appendix A's supposedly complete inventory, and the hashed bundle
has no versioned wrapper despite E6. More importantly,
`bundlerec.class_floor` is not defined: per-source effective class and the
release-wide maximum produce different canonical bytes and content digests.
Define a versioned CDDL object, require every source to be an export-eligible
claim carrying `kind` and `statement`, emit one record per duplicate-free
source, and pin the exact per-record floor.
([bundle prose](/Users/vm/owner-plane-d0a-spec.md:1314),
[Appendix A Memory bodies](/Users/vm/owner-plane-d0a-spec.md:1830))

The release freezes `data_frontier` and `as_of_ms`, but classification also
depends on control state. Add `control_frontier` and define recovery
re-derivation at the exact `{data, control, as_of}` snapshot. Plane endpoints
must carry plane, zone and space for importable releases; optional destination
coordinates cannot satisfy the stated equality rule.

Expiry remains non-executable. The flow and release each carry a deadline, but
no invariant relates them. §9.1 reads deadlines from the cited grant or
certificate, whereas §11.8 newly applies the release body's deadline to the
import under the source zone's policy. Select whether the qualified operation
is the release or import, then define that operation's exact receipt zone and
subject. Require `release.deadline ≤ flow.deadline` and state the intended
no-witness behavior for a solo source zone. Egress has no import operation at
all, so its mandatory deadline must instead be consumed at release acceptance.

The journal also lacks a terminal failed state. If a source is erased during a
partial transfer, remaining imports fail forever, while PendingXfer clears only
through an XferDone containing `record_count` entries. Add XferAbort/Failed or
explicitly define a terminal partial state; require `completed` to equal the
bundle's exact source-op set, not merely have the same cardinality.

Also require `export_id` to be plane-wide single-use, or include `release_op`
in replay and journal identities. Today two releases can reuse one ID and
collide in PendingXfer/XferDone and `(from_plane, export_id, source_op)` replay.

**Closure test:** the bundle digest has one versioned preimage and frozen
classification snapshot; flow expiry cannot be extended by the writer; every
transfer reaches Done or Failed after a crash or source erasure.

## Freeze blocker 6: erase completion still does not prove exactly what survived or died

`m.erase_request.targets` is a bare bytes32 array and never says whether those
bytes are operation hashes or item addresses. The manifest and tombstone carry
`item_addr + erase_op`, but the spec does not pin rotation-admission validation
of that pair or how recovery reconstructs the per-target mapping for a
multi-target request after the private index is removed. Include `target_op` in
each manifest/tombstone, or normatively preserve and verify an equivalent
durable mapping.

The survivor digest proves only the wrappers that the implementation chose to
include. No frozen tenant frontier defines the expected membership set as all
old-epoch items at the fence minus the typed erase set. An implementation can
omit an un-erased item, produce a self-consistent digest, destroy the old KEK
and lose the omitted item. Add a versioned survivor-set preimage and a frozen
membership frontier to the rotation operation/state machine.

Define one manifest entry per item when multiple erase requests target it,
including deterministic winner/duplicate semantics. `mexportrel.sources` and
`m.erase_request.targets` should likewise be explicit E7 sets.

**Closure test:** the rotation commits to a complete expected input set;
recovery maps every tombstone to exactly one target operation; omission of any
survivor prevents old-KEK destruction.

## Conformance and normative residue

The schema shell is valid JSON, draw ordering is explicit, and leaving
outcome/disposition strings open in JSON is acceptable because the harness is
required to cross-check the closed protocol enums. The policy constants remain
correct.

Before vector freeze, close the remaining set/conditional rules and remove
normative drift:

- E7's default full-byte ordering does not reject two different entries with
  the same logical key. Pin key uniqueness for grants, wraps, zone cutoffs,
  receipt/proof cutoffs and transfer/erase source sets;
- `draw_order` removes JSON property-order dependence but carries arbitrary
  names without byte counts or a closed path vocabulary. Use ordered
  `{name, nbytes}` draws (plus family-specific constraints) so HPKE/key sampling
  consumes one reproducible stream;
- the outcome map names `fork`, but the disposition table separately uses the
  unenumerated “control-plane fork” and same-recovery-epoch competition;
  differing request-ID reuse is described as `duplicate→fork` while
  `duplicate` has only the idempotent disposition. Give each negative path one
  closed outcome/disposition pair;
- the advertised 4096-entry typed erase manifest cannot fit the enclosing
  64-KiB control-op cap; state the effective derived limit or add a bounded
  continuation;
- C3′ still displays zone-less tenant cutoffs while Appendix A uses
  `zonecutoff`;
- §9.3 still says `c.enroll` reauthorizes lineages although the registry makes
  it creation-only;
- §8 and the hosted list still say `c.drill` is trusted-lane-authored after the
  registry correctly makes that product guidance only;
- §11.7 still says the audit writer is daemon-only;
- T4's singular “cutoff forward” wording does not match per-key accepted-
  through receipt cutoffs;
- `m.claim` points evidence validation to §11.7 rather than §11.5, and
  `CheckpointCommit` names no closed operation (the registry has
  `c.checkpoint`);
- `retired_by: author/curator` does not itself distinguish a curator's retract
  from a curator's retire, despite D-27's promise that verdicts surface
  distinctly.

`owner-plane-core`, the vector corpus and the harness are absent, and the
offline family-14 result remains open. That is expected before corpus work, but
Gate A itself cannot be declared until they exist and the discrepancy audit is
green.

## Recommended v0.5.1 repair order

1. **Proof identity:** bind receipt/lease policy, service history and typed
   proof cutoffs; close the no-deadline budget invariant.
2. **Hosted/control identity:** encode self-cutoff proof, consume requester
   nonces, reconcile the hosted operation set and finish revocation
   completeness/continuations.
3. **Scope/boot:** pin zone-local Frontier/checkpoint semantics and complete
   genesis/issued-epoch cross-fields.
4. **Audit:** exact trigger/principal/chunks/remedy.
5. **Transfer:** versioned bundle, per-record floor, control snapshot, expiry
   proof and terminal failure.
6. **Erase:** target-op identity, frozen survivor membership and duplicate
   rules.
7. **Vectors:** remove stale prose, close schemas, then build the core/corpus
   and let the discrepancy audit resolve only editorial drift.

## Gate-A go-ahead

Give the go-ahead only when:

- receipt/lease qualification and finality are immutable and replica-
  independent;
- hosted self-service and revocation are enforceable from closed bodies at
  maximum scope;
- every Frontier/checkpoint and genesis field has one zone/epoch meaning;
- every audited read is attributable and physically committable;
- every transfer terminates with a versioned, snapshot-bound bundle;
- every erase proves the complete target and survivor sets;
- all vector artifacts exist, every required surface is green, the offline
  result is recorded, and the final audit finds only editorial drift.

## Bottom line

V0.5 is a successful convergence draft. It resolves the most important Memory
relation, evidence, generation and record-cardinality issues from v0.4. The
remaining problems are narrower but still security-semantic: policy-relative
receipts without a policy field, self-service cutoffs without a requester,
zone-sensitive checkpoints without a zone, plural audit rows without physical
transaction semantics,
flow deadlines without a proof rule, and erase/survivor sets without a frozen
membership identity.

Cut a compact v0.5.1 exactness patch before freezing canonical vectors. After
that, stop prose rounds and let `owner-plane-core` plus the corpus decide Gate
A. Do not enable durable P1 writes before Gate B and the umbrella prerequisites.
