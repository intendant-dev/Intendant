# Review: D0-A Core + Memory normative specification v0.5.6

*2026-07-12. Fresh review of
[owner-plane-d0a-spec.md](/Users/vm/owner-plane-d0a-spec.md) v0.5.6,
diffed against the archived v0.5.5 and replayed against the adjudicated
[v0.5.5 synthesis](/Users/vm/owner-plane-d0a-spec-v0.5.5-synthesized-review.md).
The review first retested D-100 through D-106 against the counterexamples that
motivated them, then composed the requester, cutoff, effect, checkpoint,
recovery, renewal, rotation and transfer machines in fresh two-replica and
crash traces.*

## Executive verdict

**V0.5.6 is a substantial improvement, but I would not freeze it or give Gate
A the go-ahead. I recommend one focused v0.5.7 protocol cut, followed by the
normative companion schema and executable corpus.**

This cut closes a large fraction of the v0.5.5 review cleanly:

- `gens_total` is gone and reauthorization now has a coherent durable
  one-shot posture;
- the reconstruction invariant is stated at the right level;
- supersede and abandon are distinct boundary purposes, Head ordering is
  pinned, and `ref-unresolved` is a real outcome;
- growable ratify cutoffs no longer directly claim effect finality, and the
  egress, audit and transfer mirrors now invoke the barrier;
- the 64-open-gap cap removes the raw 257-head checkpoint counterexample;
- recovery entries select a control-intent tuple and storage mismatch has a
  closed outcome;
- transfer charging, record counts, replay-index rebuild order and the frozen
  source-read stamp are aligned; and
- the old admin-key, RewrapComplete and wrap-add mirror drift is repaired.

The remaining findings are narrower than the prior round, but several are
still state-machine blockers rather than editorial cleanup:

1. D-101 calls `last_known` incorporation immutable without defining a rule
   that prevents a later ratify boundary from crossing it; its proposed
   one-Head abandonment seal also cannot unambiguously close an old gap while
   preserving the later branch whose effect is waiting.
2. Hosted `c.cutoff` still compares its carried heads with replica-local
   “current” tenant state, so control-first and tenant-first delivery can
   accept different histories.
3. An epoch advance or renewal may consume staged generic ratify cutoffs, but
   does not explicitly materialize their values as immutable
   close/supersede boundaries.
4. KEM renewal is not constructible for more than 128 held zones and is unsafe
   to complete while the latest accepted epoch is ahead of the locally active
   epoch; the new 256-recipient hash-input cap separately makes a legal
   257-member rotation impossible.
5. D-105 moves missing references before recovery precedence, but other
   recovery-invalidating state checks still occur afterward; adopted cut-
   branch storage dependencies are not defined.
6. Checkpoint cardinality is now bounded, but page replacement/removal and
   renewed witness-feed closure remain underspecified.
7. Cross-zone transfer terminalization still needs a live concurrency barrier
   and a rule for a PendingXfer whose release loses admission before finality.

Recommended disposition:

- **Direction:** accept and preserve.
- **D-100:** reauthorization resolved; hosted cutoff freshness still open.
- **D-101–D-105:** materially advanced, each still partial.
- **D-106:** most bookkeeping fixes resolved; it introduces one fresh
  recipient-cardinality contradiction and leaves terminal concurrency open.
- **Protocol/schema freeze:** no.
- **Gate A:** no. The normative case companion, corpus, harness and family-14
  result still do not exist.
- **Next cut:** v0.5.7, limited to the decisions and traces below.
- **Durable P1 writes:** unchanged; still prohibited until the later gates.

## Closure ledger

| Decision | What v0.5.6 genuinely closes | Remaining disposition |
|---|---|---|
| D-100 | Removes non-carried/non-monotone `gens_total`; adds reconstruction invariant; gives reauth durable one-shot semantics; missing carried heads pend | **Partial:** `c.cutoff` equality remains cross-log/arrival-relative |
| D-101 | Excludes growable ratify from finality; classifies abandon; aligns all three consumers; names execution owner and compromise residual | **Partial:** incorporation has no ratify cap; scalar abandon does not encode a gap-preserving finality frontier |
| D-102 | Separates supersede; pins Head comparator; reconciles receipt minima; closes `ref-unresolved` | **Partial:** staged generic boundaries are not promoted to immutable close/supersede snapshots; fence projection needs a pin |
| D-103 | Caps open unknown heads; makes raw page cardinality total; adds non-regression and proof-position succession | **Partial:** multi-head page replacement/removal and historical feed closure are not fully encoded |
| D-104 | Adds control-intent tuple and `storage-orphaned`; replaces impossible every-historical-epoch renewal rule | **Partial:** queued active epochs, >128 zones, and adopted dependency closure remain |
| D-105 | Resolves adopted rotations/cutoff Heads before C3′ and withholds precedence while unresolved | **Partial:** provisional selection/atomic commit and exact outcomes for later state invalidity are unstated |
| D-106 | Fixes fourth-revisit direction, transfer counts/charge mirrors, replay rebuild, RewrapComplete, frozen read stamp and several schema pins | **Mostly resolved:** direct feed-fork wording, recipient-set constructibility and live terminal serialization remain |

## What should be preserved

1. **Reauthorization is finally coherent.** Its signature inputs are either
   carried or control-derived, and delayed application is explicitly part of
   the authority posture rather than disguised as tenant-log freshness.
   ([reconstruction invariant](/Users/vm/owner-plane-d0a-spec.md:229),
   [reauth registry row](/Users/vm/owner-plane-d0a-spec.md:1103))
2. **The boundary taxonomy is much better.** Supersede, revoke, close, abandon
   and recover no longer silently share the growable ratify pool, and the Head
   comparator is exact.
   ([cutoff algebra](/Users/vm/owner-plane-d0a-spec.md:1112))
3. **The prior effect-consumer drift is fixed.** Egress requires acceptance
   and finality, transfer terminals inherit the barrier, audited results wait
   for durable audit rows and finality, and execution ownership is named.
   ([effect rule](/Users/vm/owner-plane-d0a-spec.md:409),
   [transfer](/Users/vm/owner-plane-d0a-spec.md:968),
   [audit](/Users/vm/owner-plane-d0a-spec.md:1687))
4. **Checkpoint cardinality now has a real invariant.** The open-gap cap makes
   every legal lineage fit comfortably inside a 256-head page.
   ([E8](/Users/vm/owner-plane-d0a-spec.md:101),
   [`w.gen`](/Users/vm/owner-plane-d0a-spec.md:1395))
5. **Transfer accounting is now honest.** `record_count = |sources|`; the
   surcharge is explicitly a record-rate bound, not a claim about egress byte
   volume; the release stamp remains final for the source read.
   ([export rule](/Users/vm/owner-plane-d0a-spec.md:1862))
6. **Recovery now fails visibly at the storage boundary.** An activation that
   does not match the selected control intent has a named
   `storage-orphaned` outcome rather than an implied quarantine.
   ([C3′](/Users/vm/owner-plane-d0a-spec.md:1225),
   [outcomes](/Users/vm/owner-plane-d0a-spec.md:1593))

## Freeze-blocking findings

### 1. The replacement effect-finality seal is not yet an immutable frontier

#### 1.1 `last_known` is called immutable, but ratify is not capped by it

D-101 correctly says a growable ratify cutoff cannot establish finality. It
then treats `w.gen(last_known = H)` incorporation as an immutable boundary.
The cutoff algebra, however, says only an **abandon** boundary caps later
ratification. It never says a ratify boundary may not cross a head previously
incorporated by `last_known`.
([effect finality](/Users/vm/owner-plane-d0a-spec.md:409),
[ratify/abandon algebra](/Users/vm/owner-plane-d0a-spec.md:1116),
[`w.gen` retirement](/Users/vm/owner-plane-d0a-spec.md:1418))

Literal replay:

1. Generation 1 has accepted head H1.
2. W opens generation 2 with `last_known = H1`; H1 is retired.
3. Effect-bearing B follows W. With no open lower gap, B executes.
4. A successor H2 of H1 arrives late and is quarantined as retired-generation
   history.
5. A later generic `c.cutoff(H2)` grows the ratified-history boundary. Under
   the broad revival rule it revives H2; under an inferred incorporation cap
   it must reject. Both readings fit different sentences in the document.
6. On the revival reading W's old “H1 is terminal” predicate no longer holds,
   and W/B may be displaced or invalidated after B escaped.

If the intended answer is that `last_known` permanently caps ratification,
make that a boundary-algebra rule and give it a closed failure outcome. Merely
calling the incorporation immutable does not define its interaction with the
only operation that revives quarantined history.

The nearby common-case sentence also still says an open gap defers effects
“until its cutoff,” immediately after generic ratify cutoffs were excluded
from finality. It should say “until an immutable finality seal/close.”
([stale finality sentence](/Users/vm/owner-plane-d0a-spec.md:423))

#### 1.2 One scalar `c.abandon_writer.at` cannot express “close this old gap,
keep that later branch”

The prescribed remedy for an unknown gap is `c.abandon_writer`. Its wire body
contains one Head and the rule says operations beyond that Head quarantine
permanently.
([registry row](/Users/vm/owner-plane-d0a-spec.md:1106),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:2719))

For live heads H1 in generation 1 and B in generation 2:

- abandon at H1 also places W/B beyond the seal, destroying the branch whose
  effect was waiting; while
- abandon at B leaves a delayed generation-1 H2 lexically before the seal, so
  the current text does not prevent H2 from later being ratified.

A finality ceremony needs to commit a **complete per-generation/multi-head
frontier** (or an equivalent budget reservation) and state that unlisted
history can never revive across it. If the intended semantics are instead
“abandon freezes every not-yet-accepted coordinate anywhere in the lineage,”
the body must carry the accepted frontier against which that assertion is
made, and the control-first/tenant-first trace must converge. Independently,
`cabandon.at.lineage` must equal the body lineage and the Head must belong to
the body zone; those scope pins are absent today.

There is an availability posture to state as well: for its own writer-gap
closure, a hosted plane's available self-service operation is ratify
`c.cutoff`, while `c.abandon_writer` is outside its ceiling. One hosted unknown
gap therefore defers effects until recovery lifts the ceiling unless a
different safe seal is added. That fail-closed result may be intentional, but
it should be product-visible and vector-pinned.
([hosted ceiling](/Users/vm/owner-plane-d0a-spec.md:1283))

The named post-escape residual should also cover every later proof invalidator
(including feed-fork exposure), not only compromise cutoffs. That residual is
acceptable; an arrival-relative finality decision is not. In particular, a
later proof invalidator can quarantine W itself, removing the `last_known`
fact on which B's finality rested even when B's own proofs remain valid. The
specification should say explicitly whether that is included in the accepted
post-escape residual or whether finality seals must be control-authorized and
non-revisitable.

### 2. Hosted cutoff freshness still has no cross-log order

The `c.cutoff` requester signs carried `live_heads`, and a missing carried
Head now correctly pends. Admission nevertheless requires those heads to
equal the lineage's **current** set and says later writing invalidates assent,
while the residual says the requester's unpublished successors are knowingly
retired.
([cutoff registry row](/Users/vm/owner-plane-d0a-spec.md:1105),
[cutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2692))

Two-replica replay:

1. The requester signs cutoff C over H1, then signs successor H2 but withholds
   one of the two objects from each replica.
2. Replica A sees C first. Its current set is H1, so C accepts; H2 later
   quarantines.
3. Replica B sees H2 first. Its current set is H2, so the identical C is stale
   and rejects.
4. Applying C first on B to make the equality true is an unstated
   cutoff-wins priority and makes the predicate circular.

`ref-unresolved` addresses a missing head named by C. It does not address an
extra head already known to the validator. There is no total order between the
tenant log and control log from which “current” can be reconstructed.

Choose one portable posture:

- **durable snapshot authority:** a valid requester signature makes its
  carried complete frontier win over every uncarried successor; or
- **freshness against prior writing:** carry/consume a final tenant frontier or
  monotone write nonce and define how later arrival revisits the control op.

The first is closest to the already stated “knowingly retired” residual. The
second needs more protocol machinery. Local-set equality is neither.

### 3. Promotion of a staged ratify boundary is only implicit

Strict epoch advance validates closure over the union of inline close entries
and earlier accepted ratify/close cutoffs. Renewal similarly permits zones
beyond its inline `history_cutoffs` cap to be covered by pre-established
generic `c.cutoff`s.
([strict closure](/Users/vm/owner-plane-d0a-spec.md:1099),
[scalable closure](/Users/vm/owner-plane-d0a-spec.md:1162),
[renewal](/Users/vm/owner-plane-d0a-spec.md:1091))

Trace:

1. Stage generic ratify boundary L@H5.
2. Accept a strict epoch advance, or renew a predecessor certificate, using
   that staged coverage.
3. Later grow the generic ratify boundary to H10.

One reducer may dynamically widen old-epoch/predecessor authority to H10;
another may infer that the advancing operation snapshotted H5. D-102 says
close and supersede are immutable, but the advancing/renewal body does not
carry the staged value and no normative transition says it materializes a new
scoped boundary. The total control order makes a derived H5 snapshot entirely
workable; the missing piece is saying that this promotion happens, despite
the adjacent rule that purposes are otherwise disjoint and never composed.

At the advancing control position, derive and permanently record an immutable
per-lineage close/supersede boundary equal to the effective inline/staged
value. Later generic ratification must not amend it. Add both H5 → advance →
H10 and H5 → renew → H10 vectors.

### 4. KEM renewal still has two unconstructible transitions

#### 4.1 “Latest accepted epoch” can be ahead of the active storage epoch

A KEM-rotating renewal carries one replacement wrap per held zone at that
zone's latest **accepted** epoch. Local activation deliberately lags control
acceptance because rotations serialize through state 6.
([renewal row](/Users/vm/owner-plane-d0a-spec.md:1091),
[rotation queue](/Users/vm/owner-plane-d0a-spec.md:855),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2593))

Crash/availability trace:

1. Storage is active at epoch 2; rotations for epochs 3 and 4 are accepted but
   queued.
2. Renewal carries only an epoch-4 wrap under Knew.
3. Every latest-epoch wrap is carried by the renewal itself, so acceptance of
   those wraps coincides with renewal acceptance. Appendix A's stated custody
   condition therefore permits Kold to be discarded immediately even though
   local activation still trails.
4. Until R3 and R4 Fence, I3 still serves epoch-2 and then epoch-3 wrappers,
   neither of which is recoverable with Knew.
5. A crash, a second replica at a different queue position, or a stalled R3
   leaves the renewed device unable to decrypt its active store.

A still-running process may temporarily limp along on an already-unwrapped
active KEK; the specification does not define that cache as durable custody.
The hard failure is reconstruction after local KEK loss/crash, when Knew has
only the future epoch-4 wrap and Kold was declared discardable.

“Queued staging re-targets via its own wrap-adds” is not a required transition
and those wrap-adds are not a renewal completion condition. Choose one:

- prohibit KEM renewal while a held zone has an accepted-but-not-completed
  rotation, using a portable completion fact;
- prepare Knew wraps for every still-live active/queued epoch before atomic
  cert activation; or
- retain Kold until every affected local store has crossed the queue and name
  this as a local custody protocol rather than control admission.

#### 4.2 A 129-zone device cannot renew its KEM key

Renewal requires one wrap for **every held zone**, but E8 caps wraps in one
operation at 128. No invariant caps a device at 128 zones and there is no
renewal continuation.
([E8 wrap cap](/Users/vm/owner-plane-d0a-spec.md:101),
[renewal CDDL](/Users/vm/owner-plane-d0a-spec.md:2593))

Either enforce a hard per-device held-zone cap before the 129th authority is
granted, or add a bounded prepare/commit renewal ceremony. Generic
`c.wrap_add` cannot by itself provide atomic activation of a not-yet-enrolled
KEM key: the current protocol neither permits a future-certificate key as a
staging recipient nor binds such staging atomically to the later renewal.

### 5. The new recipient commitment has a 257-member dead end

The rotation ceremony explicitly supports memberships above 128 through
`c.wrap_add`s and defines no total zone-member cap. D-106's decision and the
Appendix declare its new `recipientset` hash input capped at 256, while E8
does not register that cap.
([rotation membership](/Users/vm/owner-plane-d0a-spec.md:882),
[recipientset CDDL](/Users/vm/owner-plane-d0a-spec.md:2751))

With 257 intended recipients, the rotation and 129 wrap-adds can all be
accepted. If the ratified/commented cap is normative, no conforming
`recipientset` preimage exists and the Fence can never activate; if it is not
normative, D-106's cap is unenforced. Either reading is a freeze defect.

Enforce a 256-recipient zone cap at enrollment/wrap-add, raise/remove the hash-
input cap, or define a paged/Merkle commitment. Whichever posture wins, add
`recipientset`'s logical key to E7 and its cap to E8; neither is registered in
those central inventories (the concrete key/cap are in the Appendix comment,
and D-106's decision summary repeats the cap).

### 6. Recovery does not pin full-validity atomicity before precedence

D-105 says recovery validity fully precedes precedence, but the pipeline moves
only state-dependent **references** into `prec`. “Remaining state-dependent
invariants” still run after C3′ placement.
([control pipeline](/Users/vm/owner-plane-d0a-spec.md:1560))

Concrete example:

1. Recovery R has a valid recovery signature, base, epoch, repoch and held
   references.
2. `new_admin = {alg: "p256", pk: h'00'}` passes the broad CDDL `bstr` shape
   but violates the suite's 65-byte/on-curve key rule.
3. `prec` selects R over a competing admin operation before `state` returns
   `key-malformed` for R.

The final `state` stage is also labeled as the transition, so a careful
implementation can keep `prec` provisional and commit nothing until R passes.
The pipeline never says that, however, while D-105 says validity “fully”
precedes precedence. Move **every** recovery acceptance predicate before
precedence, or state that selection is provisional and the transition commits
atomically only after all later checks pass. The former is much easier to
specify and vector.

Outcome precedence is also incomplete: `prec` has no outcome arrow and
`state` advertises only chain/body outcomes, while this example is
`key-malformed` from the cert family. Pin the first-failure mapping along with
the atomicity rule.

#### 6.1 Adoption needs a dependency-closure rule

The highest adopted entry can be a workable compact representation because a
later Fence implies predecessor rotations reached state 6. But its
`recipients_hash` can depend on cut-branch `c.wrap_add`s, enrollments and key
descriptors, while erase manifests can depend on cut-branch erase requests.
The phrase “rotation's predecessor chain” does not say whether those records
survive as evidence/state after recovery cuts the branch.
([storage adoption](/Users/vm/owner-plane-d0a-spec.md:1225),
[recovery CDDL](/Users/vm/owner-plane-d0a-spec.md:2726))

Define the exact adopted dependency closure and whether those records are
retained as verification evidence, copied into recovery-derived effective
state, or carried by reference. `control_frontier` can locate the cut prefix
when its bytes are held, and `recipients_hash` can prove the selected set, but
the specification does not say that those cut records' effective authority
and HPKE ciphertexts survive the branch cut.

Also call the tuple what it is: the entry omits `fence_frontier`, so it selects
a **control-intent commitment**, not the complete physical Fence frame. If the
complete Fence is intended, carry the missing field.

### 7. Checkpoint pages and renewed proof feeds remain incomplete

#### 7.1 Latest-page-wins lacks a set transition

The 64-gap cap fixes size, but `covers` is a flat set keyed by
`(lineage, gen)` while prose says a later page “naming lineage L” replaces L's
earlier coverage entry. It does not define which array names L, how entries for
omitted generations are retained/removed, or how “later covers head ≥ earlier”
compares two multi-head sets.
([checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1107),
[checkpoint CDDL](/Users/vm/owner-plane-d0a-spec.md:2476))

For C1 covering `{L/g1:H10, L/g2:H5}` and C2 carrying only
`{L/g1:H11}`, one implementation replaces the whole L page and drops g2;
another updates `(L,g1)` and retains g2. A fence-only C2 creates the same
question. The new scalar Head comparator does not define this set operation.

Encode explicit page lineage membership and define replacement per
`(lineage,gen)`, including when omission is legal (normally only after a
retirement). If an empty replacement is meaningful, the wire needs a lineage
identifier; an all-empty object currently means only a proof-position
checkpoint. `fencecoord` also lacks an op hash, so say explicitly that fence
non-regression uses coordinate projection `(gen,seq)` and that equal-coordinate
fork evidence is resolved from the chain, rather than claiming the full Head
comparator applies literally.

#### 7.2 Renewal does not close the predecessor witness feed

D-103 says a renewed signing key is a new issuer identity and historical
scopes are “closed by their cutoffs.” Ordinary renewal carries tenant
`history_cutoffs`, not a receipt/lease feed `{through, head_hash}` cutoff.
([certificate renewal](/Users/vm/owner-plane-d0a-spec.md:331),
[checkpoint row](/Users/vm/owner-plane-d0a-spec.md:1107))

The reducer therefore cannot distinguish a delayed pre-renewal receipt from a
post-renewal old-key mint, nor prove where the old scope ended. Repeated
renewals create an unbounded number of historical scopes, and the text does
not say whether/how they participate in checkpoint hardening. If historical
scopes must remain in `proof_positions`, its 64-entry cap is insufficient; if
“currently qualified only” excludes them, their terminal cutoff is the fact
that must make exclusion safe.

Require renewal to commit the predecessor feed's terminal position (the same
dense `{key_id, through, head_hash}` shape already used for compromise), and
define old-scope statements beyond it. Then “historical scopes closed by their
cutoffs” becomes executable rather than aspirational.

### 8. Transfer terminality is restart-safe but not live-concurrency-safe

D-106 correctly rebuilds destination replay indexes before startup recovery.
It does not require the single plane writer to serialize a normal destination
append against a source terminal decision in one `export_id` critical section.
([transfer recovery](/Users/vm/owner-plane-d0a-spec.md:968))

Trace:

1. Destination import A validates and begins its durable append.
2. Source erasure makes the remaining bundle underivable.
3. Terminalization checks replay keys before A becomes visible and commits
   `XferAbort(missing = {A})`.
4. A's destination append completes.

Unless an unstated single-writer critical section excludes the trace, the
terminal permanently says a record is missing when it exists. Require that
per-`export_id` coordinator/barrier: terminalization must fence future
destination commits and wait for in-flight ones, or terminal truth must be
defined as a re-derivable local state instead of a one-time decision.

There is a second, primarily liveness/cleanup gap. PendingXfer is written at
release admission, before effect finality. D-94 already forbids imports while
the release is not accepted and effect-final, but the journal has no specified
dormant/revival or terminal-GC state after displacement. Define that lifecycle
or delay journal activation until finality with a crash-safe discovery path.

## Feed-revisit wording

D-106's fourth revisit path is phrased as a **later committed boundary**
revealing a losing feed branch. T3 also freezes a scope when a differing
duplicate `(scope, issuer_seq)` arrives directly. If an admitted operation
rested on that scope, direct fork discovery must revisit it even before a
cutoff/checkpoint commits the winning head.
([T2](/Users/vm/owner-plane-d0a-spec.md:647),
[T3](/Users/vm/owner-plane-d0a-spec.md:665))

Broaden “feed-fork exposure” to include both direct fork discovery and
boundary-revealed fork selection, and include this case in the post-escape
residual. This is a small semantic pin, but the current “exactly four” wording
makes omission normative.

## Gate A and artifact status

The document is honest that D-91 remains artifact-pending. A filesystem check
found no `d0a-vector-cases.v1.json`, external container schema, owner-plane
core, corpus or harness. Family 14's offline confirmation result is also still
open.
([vector contract](/Users/vm/owner-plane-d0a-spec.md:1955),
[Open list](/Users/vm/owner-plane-d0a-spec.md:2353),
[Gate A](/Users/vm/owner-plane-d0a-spec.md:2363))

Therefore:

- the prose/core protocol may be self-contained, but the normative vector
  contract and Gate-A package are not yet self-contained because §13 delegates
  case semantics to an absent companion;
- the inline generic schema is not enough to author fixtures safely; and
- Gate A is unavailable even if every prose finding above is repaired.

The right order remains: clean v0.5.7 prose/CDDL → author the companion first
→ implement the independent core/harness → create fixtures → run every
surface → perform the prose/vector discrepancy audit.

## Mechanical and consistency pass

These do not independently drive the verdict, but should ride v0.5.7 so the
document remains truly self-contained:

1. §7.4's normative C3′ body literal still has
   `{zone_id, rotation_op}` and omits D-104's `control_frontier` and
   `recipients_hash`; Appendix A has the newer shape.
   ([C3′ literal](/Users/vm/owner-plane-d0a-spec.md:1203),
   [CDDL](/Users/vm/owner-plane-d0a-spec.md:2726))
2. §4.6/D-33 says retired heads live in checkpoints, while the checkpoint row
   says immediate `w.gen`/cutoff retirements are never re-listed.
   ([Frontier](/Users/vm/owner-plane-d0a-spec.md:551),
   [D-33](/Users/vm/owner-plane-d0a-spec.md:2279))
3. Clarify D-80's “versioned checkpoint body” as “versioned by the envelope”
   so it does not imply the object-local `v` field D-88 removed.
4. The `zonecutoff` CDDL comment still names only disposition
   `pending-dependency`, not outcome `ref-unresolved`, and omits supersede and
   abandon from its purpose summary.
   ([zonecutoff CDDL](/Users/vm/owner-plane-d0a-spec.md:2683))
5. `zoneheads.heads` is one lineage's live set and should key by `gen`, like
   Frontier, not `(gen,seq)`; the current parse-level key admits two members
   from one generation even if later live-set validation would reject them.
6. `cabandon.at` should explicitly equal the body's zone/lineage scope.
7. Add `recipientset → device` to E7 and its selected cap/posture to E8.

## Required v0.5.7 vector additions

Before the companion is authored, reserve exact case kinds for:

1. known `last_known` incorporation → late lower-generation successor → later
   ratify; and unknown-gap abandonment that preserves a later branch;
2. hosted cutoff control-first vs tenant-first vs behind vs extra-head;
3. staged H5 → strict advance/renewal → generic ratify H10;
4. renewal with active epoch 2 and accepted queued epochs 3/4, a 129-zone
   holder, and a 257-recipient rotation;
5. recovery with valid references but malformed `new_admin`, plus adopted
   wrap-add/enrollment/erase dependency closure;
6. checkpoint multi-generation replacement, omission after retirement,
   fence-only page, and explicit empty membership;
7. repeated witness-key renewals with predecessor feed closure, including a
   65th historical scope to prove why it is excluded from (or representable
   by) checkpoint hardening;
8. direct feed-fork discovery after an operation has admitted;
9. concurrent destination import vs source abort, and a PendingXfer whose
   release is displaced before effect finality; and
10. every stale prose/CDDL mirror listed above as schema parity assertions.

## Final recommendation

V0.5.6 should be treated as a successful convergence cut, not a failed freeze.
It resolves most of the prior report and leaves a much smaller set of exact
protocol choices. I would cut v0.5.7 around those choices, run one short
adversarial review focused only on the new transitions, and then proceed to
the companion/core/corpus work. Freezing v0.5.6 now would push cross-log order,
key-transition cardinality and recovery atomicity into fixture authorship,
which is precisely where this specification has correctly insisted they do
not belong.
