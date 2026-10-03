# Synthesized review: D0-A Core + Memory normative specification v0.5.6

*2026-07-12. Adjudicated synthesis of
[owner-plane-d0a-spec-v0.5.6-review.md](/Users/vm/owner-plane-d0a-spec-v0.5.6-review.md)
and
[owner-plane-d0a-spec-v0.5.6-review-2.md](/Users/vm/owner-plane-d0a-spec-v0.5.6-review-2.md),
verified against
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.6.
This document resolves disagreements rather than unioning the reports.*

## Executive verdict

**Both reviews correctly recommend v0.5.7 and correctly withhold Gate A. The
peer adds one important composed consequence—the hosted plane has no
finality-restoring operation after a genuine unknown gap—and one exact wire
pin: `recipientset`, as a direct hash preimage, needs `v: 1`. Its conclusion
that all eight prior repair steps are otherwise discharged and v0.5.7 needs
only one ruling is not supported by the composed reducer.**

V0.5.6 is nevertheless a successful convergence cut. Preserve these direct
repairs:

- `gens_total` is removed, the reconstruction invariant is normative, and
  `c.lineage_reauth` has coherent durable one-shot semantics;
- supersede and abandon are named purposes, the scalar Head comparator is
  pinned, receipt minima are reconciled, and `ref-unresolved` is closed;
- growable ratify is excluded from finality and all three effect-consumer
  mirrors now invoke the barrier;
- the 64-open-gap cap closes the raw checkpoint cardinality failure;
- recovery carries a control-intent tuple and has `storage-orphaned`;
- transfer charge/count/restart bookkeeping and the frozen source-read stamp
  are aligned; and
- the D-106 mechanical fixes to O7, wrap-add, RewrapComplete and replay-index
  reconstruction landed.

The adjudicated residue is:

1. the finality instrument itself is not fully defined: `last_known`
   incorporation has no explicit ratify cap, and one scalar abandon Head
   cannot close an old gap while preserving the waiting later branch;
2. hosted `c.cutoff` still compares against arrival-relative tenant state;
3. KEM renewal has an unresolved queued-epoch custody/recovery interlock and
   is unencodable for more than 128 held zones;
4. `recipientset` needs both versioning and a coherent total-cardinality
   posture;
5. checkpoint page replacement and predecessor witness-feed closure remain
   incomplete;
6. recovery references are pre-checked and transition is last, but provisional
   selection/outcome semantics and adopted dependency survival need exact
   rules;
7. transfer needs an explicit live critical section and PendingXfer lifecycle,
   while direct issuer-fork discovery is missing from the claimed exhaustive
   revisit wording; and
8. the normative companion/core/corpus/harness and family-14 result remain
   absent.

Recommended disposition:

- **Direction:** accept and preserve.
- **V0.5.7:** required; focused, but not a one-ruling prose patch.
- **Protocol/schema freeze:** no.
- **Gate A:** no. A repaired v0.5.7 can become the audit baseline; Gate A
  still requires the companion, implementations, corpus, family 14 and final
  discrepancy audit.
- **Durable P1 writes:** remain prohibited under the unchanged later gates.

## Adjudicated decision ledger

| Decision | Peer disposition | Adjudicated disposition |
|---|---|---|
| D-100 | Complete | **Partial:** reauthorization complete; hosted `c.cutoff` remains cross-log-relative |
| D-101 | Complete except hosted availability | **Partial:** mirrors and ratify exclusion complete; incorporation/seal semantics and hosted remedy remain |
| D-102 | Complete | **Mostly direct-complete:** staged ratify→close/supersede promotion and fence projection still need normative pins |
| D-103 | Complete | **Partial:** cardinality fixed; multi-head page transition and historical feed closure open |
| D-104 | Complete | **Partial/open:** control-intent adoption improved; renewal queue custody is unresolved and 129-zone renewal is unconstructible |
| D-105 | Complete | **Mostly closed:** missing references precede precedence and transition is last; clarify provisional selection and exact later-stage outcomes |
| D-106 | Complete plus `recipientset.v` pin | **Mechanical batch landed; protocol partial:** recipient cardinality/versioning, live transfer serialization, direct feed-fork wording remain |
| D-91/Gate A | Artifact-pending, after v0.5.7 | **Artifact-pending:** agrees, but this is independently sufficient to withhold Gate A |

## Assessment of the peer review

### Findings to adopt

1. **Hosted unknown-gap availability is a real high finding.** Ratify cutoffs
   never confer finality; the hosted ceiling forbids abandon, policy change
   and epoch bump; audited results require effect finality. A hosted
   `w.gen(last_known = "unknown")` can clean its Frontier but cannot restore
   effect flow before re-root.
   ([effect finality](/Users/vm/owner-plane-d0a-spec.md:409),
   [hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:1244),
   [audit release](/Users/vm/owner-plane-d0a-spec.md:1687))
2. **`recipientset` needs `v: 1`.** It is the direct canonical input to
   `H_recips`, so it is a top-level hashed object under E6, not an operation
   body inheriting `operation_version`.
   ([E6](/Users/vm/owner-plane-d0a-spec.md:77),
   [recipientset](/Users/vm/owner-plane-d0a-spec.md:2751))
3. **The peer's byte-level discharge of the direct fixes is useful.** The
   effect mirrors, scalar comparator, closed outcomes, raw open-gap cap,
   transfer counts and the D-106 schema mirrors all genuinely landed.
4. **The hosted composition belongs in the first corpus tranche.** Whichever
   remedy is selected, vector hosted unknown gap → ratify cleanup → effect
   still blocked → remedy → effect restored (or explicit re-root requirement).

### Where the peer overcredits v0.5.6

#### Requester repair is complete only for reauthorization

The peer treats the remaining requester state as a monotone control-plane
snapshot. That is true for `c.lineage_reauth`, not `c.cutoff`. The latter still
requires carried `live_heads` to equal the lineage's “current” tenant heads,
says later writing invalidates assent, and simultaneously says unpublished
successors are knowingly retired.
([cutoff row](/Users/vm/owner-plane-d0a-spec.md:1105))

Control-first replica A accepts C over H1 and later quarantines H2. Tenant-
first replica B accepts H2 and rejects identical C as stale. `ref-unresolved`
only covers a missing **carried** head, not an extra head B already holds. D-100
therefore has real residue.

#### D-101 does not make the old finality trace impossible by construction

The peer assumes the word “immutable” is an operational ratify cap. The
algebra explicitly prevents ratify from exceeding **abandon**, but never says
it cannot cross a `last_known` incorporation. Thus H1 → W(last_known=H1) →
effect B → delayed H2 → generic ratify H2 still has two readings: revive H2,
or infer an unstated incorporation cap.
([effect rule](/Users/vm/owner-plane-d0a-spec.md:409),
[ratify/abandon algebra](/Users/vm/owner-plane-d0a-spec.md:1116),
[`w.gen`](/Users/vm/owner-plane-d0a-spec.md:1418))

The current `c.abandon_writer` also does not encode “abandon only the old
gap.” It carries one Head: abandon at H1 kills W/B beyond it; abandon at B
leaves delayed generation-1 H2 before it. Adding that existing operation to
the hosted ceiling grants destructive lineage authority without solving the
gap-preservation problem.

#### Paging is cardinality-total, not transition-complete

The 64-gap cap ensures one lineage fits in a page. It does not say whether a
later `{L/g1:H11}` page replaces all of earlier
`{L/g1:H10, L/g2:H5}` or updates g1 while retaining g2. `covers` is flat,
“page naming L” is not encoded, and omission/removal is undefined. An all-
empty object is explicitly a pure-proof checkpoint and carries no lineage to
clear.
([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1107),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2476))

#### Latest-accepted-epoch renewal is satisfiable only in the simplest queue

Control acceptance and local activation are intentionally distinct. With
epoch 2 active and epochs 3/4 accepted but queued, renewal carries only an
epoch-4 wrap under Knew. Those wraps accept with the renewal, satisfying the
stated predecessor-key custody condition before storage has crossed epochs 2
and 3. Once the predecessor KEM secret is discarded, the renewed recipient
cannot unwrap the still-active or intermediate epoch's KEK; a daemon's local
KEK cache is not a recipient-custody/recovery protocol. Independently, one
wrap per held zone conflicts with the 128-wrap operation cap at zone 129.
([renewal](/Users/vm/owner-plane-d0a-spec.md:1091),
[rotation activation](/Users/vm/owner-plane-d0a-spec.md:788),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2593))

#### Recovery reference validity is not full recovery validity

D-105 correctly moves adopted-rotation and cutoff-Head resolution before
C3′. A recovery with valid references but malformed `new_admin` bytes still
passes the `prec` stage and fails in the later `state` stage. A careful reducer
should keep precedence selection provisional because transition is explicitly
last; that placement is strong evidence against an irreversible divergence.
Clarify the provisional/pure selection rule and its exact outcome mapping,
because `prec` has no arrow and `state` advertises chain/body outcomes while
this failure is `key-malformed`.
([control pipeline](/Users/vm/owner-plane-d0a-spec.md:1560))

#### Four revisit categories are not exhaustive as worded

D-106 defines feed-fork exposure specifically as a **later committed
boundary** revealing a losing branch. T3 separately says a differing duplicate
at one issuer sequence freezes the scope and quarantines newer receipts. An
operation admitted on R10 must be revisited when conflicting R5′ arrives even
before a checkpoint/cutoff selects a branch. Broaden the fourth category to
direct and boundary-revealed issuer-fork discovery, or add another category.
([T2](/Users/vm/owner-plane-d0a-spec.md:647),
[T3](/Users/vm/owner-plane-d0a-spec.md:665))

### Peer pin to reject

The alleged stale `c.cutoff` requester formula is already correct. D-100
removed only `gens_total`; `lineage_version` and `repoch` intentionally remain
in both the registry and Appendix formulas. No edit is required.
([registry](/Users/vm/owner-plane-d0a-spec.md:1105),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:2708))

## Consolidated v0.5.7 findings

### 1. Define one real immutable gap-finality ceremony, including hosted use

This single design decision should close three related gaps:

1. state explicitly whether `last_known` incorporation permanently caps later
   ratification and what outcome a crossing ratify request receives;
2. replace or redefine scalar `c.abandon_writer` with a complete
   per-generation/multi-head surviving frontier, so named abandoned branches
   can never revive while later accepted branches remain live; and
3. choose the hosted posture:
   - allow a requester-attested **self-gap seal** for the device's own lineage;
     or
   - make trusted recovery/re-root the only remedy and say so in product copy.

The peer's self-service direction is attractive, but the current abandon wire
is not that operation. A hosted seal needs the D-54 requester pattern, complete
frontier binding, own-lineage scope pins and control-first/tenant-first
convergence. Also replace §4.3's stale “until its cutoff” with “until an
immutable seal/close.”

Later compromise/feed-fork invalidation of a seal's authorizing proof should
either be included explicitly in the accepted post-escape residual or avoided
by making the finality fact control-authorized and non-revisitable.

### 2. Give hosted `c.cutoff` a portable order

Choose one:

- **snapshot wins:** a valid requester signature over the complete carried
  heads deterministically retires every uncarried successor; or
- **freshness wins:** bind/consume a final tenant frontier or monotone write
  nonce, and make later arrival revisit the control operation against a
  non-circular raw history.

The stated “requester is sole writer; unpublished successors are knowingly
retired” residual points toward snapshot-wins. Whichever wins, vector
control-first, tenant-first, behind, missing-carried-head and extra-head.

### 3. Finish KEM renewal as an atomic cardinality-safe transition

Resolve both dimensions together:

- **epoch position:** interlock renewal on portable queue completion, prepare
  Knew wraps for every still-live active/queued epoch, or retain Kold until
  every affected local store completes and specify that custody protocol;
- **zone count:** cap a device at 128 held zones before granting the 129th, or
  introduce a bounded prepare/commit continuation atomically tied to the new
  certificate.

Generic `c.wrap_add` does not currently bind staged future-key wraps atomically
to certificate activation. Either add current-key equality or define an
explicit future-key prepare/commit ceremony.

### 4. Repair `recipientset` before its bytes enter the corpus

Add `v: 1`, then settle cardinality. D-106 and the Appendix say ≤256, while E8
does not register the cap and the rotation ceremony allows a 257th recipient
through wrap-adds. If 256 is normative, the 257-member Fence is unconstructible;
if not, the declared cap is unenforced.
([D-106](/Users/vm/owner-plane-d0a-spec.md:2351),
[recipientset](/Users/vm/owner-plane-d0a-spec.md:2751))

Choose an enforced zone-recipient cap, a larger/unbounded canonical hash
input, or a paged/Merkle commitment. Add its logical key to E7 and the selected
cap to E8. This is at least one wire change, contrary to the peer's “no wire
changes” conclusion.

### 5. Complete boundary promotion, checkpoint transition and feed closure

These can remain compact:

- when epoch advance or renewal consumes an earlier generic ratify boundary,
  normatively materialize its value at that control position as immutable
  close/predecessor-specific supersede state; no duplicate wire value is
  necessarily required;
- encode or precisely define latest-page replacement per
  `(lineage, generation)`, including omission only after retirement and the
  semantics of fence-only/empty pages;
- define fence non-regression over coordinate projection `(gen,seq)` because
  `fencecoord` has no op hash; and
- require renewal to close the predecessor receipt/lease feed with
  `{key_id, through, head_hash}`, then define whether historical scopes
  participate in checkpoint hardening.

The last item makes “historical scopes closed by their cutoffs” executable and
distinguishes a delayed old-key receipt from a post-renewal mint.

### 6. Clarify recovery selection and make adoption dependency-complete

State explicitly that precedence selection is pure/provisional and commits
only through the final `state` transition, or move every recovery acceptance
predicate before it. Pin exact outcome order, including malformed new-admin
keys. Because transition is already last, this is an exactness pin rather than
an independently demonstrated state-divergence blocker.

One highest tuple is compact for a replica already at that exact Fence, whose
predecessors necessarily reached state 6. The spec must still define a replica
active at a predecessor: predecessor Fence commitments are not carried. Carry
or hash-chain the adopted activation prefix, declare exact-highest-only
adoption and orphan predecessor-active replicas, or define deterministic
normalization.

`control_frontier` locates cut-branch control records such as wrap-adds,
enrollments and descriptors; tenant erase requests are referenced separately
by the adopted manifest. D-97 already says the epoch/wraps survive, but it does
not say whether that includes every effective/superseding wrap-add through the
frontier or how associated certificate/descriptor and tenant erase evidence
remain valid after C3′ cuts the branch. Define that dependency closure.

Call the tuple a control-intent commitment unless `fence_frontier` is added;
the current entry does not identify the complete physical Fence frame.

### 7. Close the remaining effect/transfer lifecycle seams

- Require the single plane writer to hold an `export_id` critical section
  across destination replay-key observation, in-flight import completion and
  source terminal append. Otherwise an import may commit after XferAbort named
  it missing.
- State that a PendingXfer whose release is displaced before finality becomes
  dormant, can revive with the release, and has a terminal/GC rule when revival
  becomes impossible. D-94 already prevents unauthorized execution; this is
  chiefly correctness/liveness.
- Broaden feed-fork exposure to direct duplicate discovery as well as later
  boundary selection, and extend the post-escape residual accordingly.

Transfer counts, charge, frozen source read and startup replay reconstruction
are closed and should not be reopened.

## Mechanical parity sweep

Fold these into v0.5.7:

1. §7.4's C3′ literal still shows only `{zone_id, rotation_op}` while Appendix
   A carries `control_frontier` and `recipients_hash`.
2. §4.6/D-33 still imply every retired head lives in checkpoints, contrary to
   the rule that immediate `w.gen`/cutoff retirements are never re-listed.
3. Clarify D-80's checkpoint as versioned by the operation envelope, not an
   object-local `v`.
4. Update the `zonecutoff` comment with `ref-unresolved` and the full purpose
   inventory.
5. Make `zoneheads.heads` use the Frontier-consistent per-generation logical
   key, and add `cabandon.at` zone/lineage equality if abandon remains.

Do **not** change the already-correct `c.cutoff` requester signature formula.

## Gate-A sequence

After v0.5.7's protocol decisions are clean:

1. author `d0a-vector-cases.v1.json` first;
2. build the independent owner-plane core/harness;
3. create the corpus, including every ordering/cardinality trace above and the
   peer's hosted composition;
4. record the family-14 offline confirmation result;
5. run all required surfaces; and
6. perform the final prose↔schema↔vector discrepancy audit.

Only then is Gate A true. A v0.5.7 prose cut may be the freeze **candidate**;
it is not by itself the protocol/Gate-A freeze.

## Final recommendation

Cut v0.5.7. Preserve the substantial D-100–D-106 progress, adopt the peer's
hosted-gap consequence and `recipientset.v` pin, and reject its one-ruling
scope. The work is now concentrated and tractable, but freezing v0.5.6 would
leave arrival-relative cutoff authority, an under-specified finality seal and
unconstructible key/cardinality transitions for the corpus to invent. That is
exactly the semantic migration the Gate-A process is designed to prevent.
