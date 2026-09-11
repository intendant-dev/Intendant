# Review: D0-A Core + Memory normative specification v0.5.18

*2026-07-13. Independent convergence review of
[`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md), v0.5.18,
4,843 lines / 54,851 words / 414,609 bytes, SHA-256
`26c3a294dbac1e17775b815d5fab06c41a80cf160ed5a9babd331f8df97274aa`.
Compared with the archived v0.5.17 bytes
([archive](/Users/vm/agenda-rfc-archive/2026-07-13-d0a-v0.5.17-as-reviewed.md),
SHA-256
`92c9b43ff9e1b651486d1a638f5d57e3c8fc8fa650e2cce6c2fd5ece9a5fcd7d`)
and the
[`v0.5.17 synthesized review`](/Users/vm/owner-plane-d0a-spec-v0.5.17-synthesized-review.md)
(SHA-256
`7062d2fc565db27b6755a6e49485f877230bb18672b7edb7fdac24f1211e119c`).
The cut is 201 insertions and 94 deletions. This is a composed semantic,
schema, crash-replay, and negative-mirror audit; no executable companion or
reference reducer exists yet, so the traces below are the first required red
fixtures, not claims that a harness has already run them.*

## Executive verdict

**Approve the transition to executable artifacts. Do not freeze v0.5.18, do
not treat its transfer reducer as an implementation-ready oracle, and do not
answer the findings with another prose-only v0.5.19.**

This is the right point to stop the prose-review loop, but it is not a clean
protocol freeze. The narrow cut makes real progress:

- the active revocation registry and CDDL now express one state-derived law;
- delayed revocation cessation is assigned to the completing control
  position;
- an authority-ending frontier now consumes dead staged closures;
- `import-collision` is correctly a derived, revisitable fold state;
- terminal incarnations and an explicit local journal carrier are the right
  shape; and
- the substantive P/−P vector is now in family 7.

Two new repairs do not survive byte-level replay:

1. `ImportCommitted` orders a marker against a tombstone, but does not retain
   the destination import bytes it promises to re-commit after a crash. The
   marker is the beginning of a cross-zone prepare/WAL protocol, not yet a
   complete one.
2. D-194 makes an absent certificate or grant permanently nonexistent using
   a “dense prefix covering the operation's signed anchor,” but a device
   operation signs no control-log position after the cited object was issued.
   Two delivery orders therefore reach different permanent states.

There are also direct schema mirrors left over: `XferReopen.basis` still
accepts statement references, terminal constructors omit the new
`incarnation`, and the Appendix still names `ItemCommit` rather than the new
marker as the erasure boundary.

That yields a deliberately split decision:

| Decision | Verdict |
|---|---|
| Archive v0.5.18 as the end of prose-only convergence | **Yes** |
| Begin the companion schema, counterexample corpus, and reducers | **Yes — with the failures below red first** |
| Gate A / specification freeze | **No** — the document itself correctly says the gate is currently false ([status](/Users/vm/owner-plane-d0a-spec.md:3830)) |
| Implement transfer/journal from these bytes as settled | **No** |
| Cut a broad prose-only v0.5.19 | **No** |
| If transfer needs a prepared-payload/WAL family to pass | **Reduce v1 scope or split D0-Transfer** |

## Disposition of the five known clusters

| Cluster | v0.5.18 disposition |
|---|---|
| Erasure serialization | **Direction right, mechanism incomplete.** Same-log marker ordering is good; crash recovery lacks its payload, marker identity is open, and erase does not take the named exclusion |
| Journal carrier and cause exactness | **Partially closed.** Frame carrier and incarnations land; CDDL and constructor mirrors still disagree, and D-194 introduces a new divergence |
| Revocation replacement and cessation | **Substantively closed.** Remaining work is outcome/vector mirroring, not another revocation design |
| Dead-stage consumption | **Substantively closed.** The authority-ending frontier is now the consuming event; put the existing trace in the executable corpus |
| Collision lifecycle | **Substantively closed.** Collision is derived and former losers re-enter when the freeze basis dies; one misleading “NEVER” remains |

## 1. D-191 carries an order, but not the import it promises to recover

The core idea is sound. v0.5.18 puts an `ImportCommitted` marker in the source
zone log and defines survival by marker-before-Tombstone order in that same
stream ([main rule](/Users/vm/owner-plane-d0a-spec.md:2717),
[CDDL](/Users/vm/owner-plane-d0a-spec.md:4506)). It also changes the exclusion
key from a release to `(source_zone, source_op)`, with source-before-release
nesting. That directly addresses the v0.5.17 problems: erasure is per item,
one source can participate in several releases, and two unrelated zone logs
cannot prove a common order.

The new crash promise is not reconstructible, however. The specification
requires recovery after marker durability but before destination
`ItemCommit` to “re-commit the destination from the held `mimport` bytes”
([rule](/Users/vm/owner-plane-d0a-spec.md:2737),
[vector](/Users/vm/owner-plane-d0a-spec.md:3494)). No durable object holds
those bytes:

- `ImportCommitted` carries only `source_op`, `release_op`, and `leaf_hash`
  ([shape](/Users/vm/owner-plane-d0a-spec.md:4508));
- destination sequence allocation, encrypted item, and outbox state become
  durable only with the destination commit
  ([commit rule](/Users/vm/owner-plane-d0a-spec.md:1174));
- bundles are explicitly never persisted
  ([bundle rule](/Users/vm/owner-plane-d0a-spec.md:2773)); and
- the browser mapping keeps plaintext tenant operations only in memory
  ([browser store](/Users/vm/owner-plane-d0a-spec.md:1442)).

`leaf_hash` is not enough to recreate the signed import header, request ID,
writer generation/sequence, predecessor hash, authorization proof,
signature, proof path, or destination ciphertext. It also does not identify
which of two otherwise valid signed import attempts with the same released
leaf was prepared. The recovery rule therefore asks a fresh reducer to invert
a hash or invent new authority-bearing bytes.

The first executable red fixture should be:

```text
durable source: PendingXfer(R), source item S
volatile only:  signed destination import I

verify source_equality(I, S)
fsync ImportCommitted(source_op=S, release_op=R, leaf_hash=L)
append + fsync Tombstone(S)
crash before destination ItemCommit(I)
discard all volatile state
replay durable bytes

required by D-191: I is re-committed and survives erasure
available to reducer: R, S's now-unreadable history, L, Tombstone
actual result: no unique I or destination ItemCommit can be produced
```

The exclusion is also one-sided. The import path is told to hold
`(source_zone, source_op)` through marker append
([import side](/Users/vm/owner-plane-d0a-spec.md:2721)), but state 6 of the
erase machine merely appends tombstones and never acquires that exclusion
([erase side](/Users/vm/owner-plane-d0a-spec.md:1067)). The vector's claim
that `verify → pause → erase → flush` is impossible
([claim](/Users/vm/owner-plane-d0a-spec.md:3502)) is therefore not entailed
by the machine. If exclusion is normative, both participants and the durable
flush extent must be named.

Marker retry semantics are absent as well. The logical key should at least be
`(release_op, source_op)`, with byte-identical retry idempotence and an exact
outcome for a differing duplicate. As written, one marker may precede the
tombstone and a retry may follow it, while “its marker precedes” does not say
which occurrence governs.

Finally, the live `mimport.proof` CDDL comment still says “the committed
ItemCommit” is the erasure boundary
([stale mirror](/Users/vm/owner-plane-d0a-spec.md:4697)), contradicting the
new marker rule.

Completing this design in prose would require a durable encrypted prepared
import, its identity, destination binding, append order, retry and mismatch
law, cleanup, native/browser mappings, and crash states. That is precisely the
new mechanism family the stopping rule warned about. The preferred v1 choices
are therefore:

1. keep a source cryptographically unerased while any referencing transfer is
   nonterminal; or
2. move same-plane transfer completion and its journal to a separate
   D0-Transfer gate.

If the marker design is retained, first make the trace above executable and
red. Do not design a prepared-import WAL until that concrete failure is the
admission ticket.

## 2. D-194 proves more than the signed bytes can know

D-194 says a certificate or grant absent from the held dense control prefix
covering a tenant operation's signed anchor is proven never issued and becomes
`no-cert`/`no-grant`, reject-permanent
([admission rule](/Users/vm/owner-plane-d0a-spec.md:2338)). This does not work
with the current envelope:

- the device authorization proof carries only certificate and grant hashes
  ([shape](/Users/vm/owner-plane-d0a-spec.md:516));
- the tenant header carries `capability_epoch`, not a control sequence/hash
  bounding issuance of those objects
  ([header](/Users/vm/owner-plane-d0a-spec.md:534));
- `ctrl_frontier` exists on the admin arm and is diagnostic, not a freshness
  proof ([proof arms](/Users/vm/owner-plane-d0a-spec.md:519)); and
- `issued_admin_epoch` is explicitly audit-only
  ([rule](/Users/vm/owner-plane-d0a-spec.md:383)).

A certificate or grant can be issued later in the same capability epoch. The
epoch opener is therefore a lower bound on policy selection, not an upper
bound proving reference nonexistence.

The second executable red fixture is:

```text
C1: open capability epoch 1
C2: issue valid certificate C and grant G, still in epoch 1
I:  tenant import signed at epoch 1, citing H_cert(C), H_grant(G)

delivery A: C1, I, C2
  after C1+I: dense prefix covers the only signed epoch anchor
  D-194 result: no-cert/no-grant, basis-free reject-permanent
  after C2: permanent result cannot re-enter

delivery B: C1, C2, I
  result: I admits

fresh fold of all bytes: agrees with B, disagrees with A
```

The exact same issue exists if genesis already supplies `C` and only `G` is
delayed. The current family text vectors the grant half but cannot make the
claimed prefix proof true ([family text](/Users/vm/owner-plane-d0a-spec.md:3215)).

Adding a signed control upper-bound coordinate to every device operation
would be a new wire and validation mechanism. The narrow, scope-reducing v1
answer is simpler: an unheld certificate or grant remains `ref-unresolved`.
It may pend indefinitely until D0-B supplies a portable completeness proof.
That is honest and convergent.

The cause table also mentions basis-free “proven-never-issued grants” but not
the corresponding `no-cert` case
([cause table](/Users/vm/owner-plane-d0a-spec.md:1330)), even though `no-cert`
is a closed fold outcome ([enum](/Users/vm/owner-plane-d0a-spec.md:2420)).
Removing D-194 for v1 removes both unsupported terminal cases.

## 3. The journal/cause schema still contradicts its stated split

The intended type split is correct: terminal causes are operation hashes;
invalidation evidence may be an operation or issuer statement. The main text
states this twice ([role rule](/Users/vm/owner-plane-d0a-spec.md:1285),
[closed cause rule](/Users/vm/owner-plane-d0a-spec.md:1319)), and
`XferAbort.missing[].basis` now uses `opfactref`
([CDDL](/Users/vm/owner-plane-d0a-spec.md:4535)).

`XferReopen.basis` still uses the broad `factref`, exactly like
`invalidation` ([CDDL](/Users/vm/owner-plane-d0a-spec.md:4583)). A canonical
statement-kind terminal basis therefore parses, while the vector says it must
fail at parse ([vector](/Users/vm/owner-plane-d0a-spec.md:3484)). The wire must
be `basis: opfactref, invalidation: factref`.

The Appendix's cause comment also retains the old rule that only intrinsic
bytes and `source-erased` omit a basis
([comment](/Users/vm/owner-plane-d0a-spec.md:4570)). The main one-table rule
also makes static scope/flow/class mismatches and the proposed never-issued
case basis-free ([main table](/Users/vm/owner-plane-d0a-spec.md:1330)). Thus
the contradiction D-193 claims to remove is still live in the normative
Appendix.

D-192 improves the carrier materially: source-log frames, one append
authority, explicit terminal incarnations, and replay in physical order are
all appropriate ([journal rule](/Users/vm/owner-plane-d0a-spec.md:1362)).
Three exactness details remain:

- The main `XferDone` constructor omits `incarnation`
  ([constructor](/Users/vm/owner-plane-d0a-spec.md:1187)), while CDDL requires
  it ([shape](/Users/vm/owner-plane-d0a-spec.md:4525)).
- The main terminal-state `XferAbort` constructor also omits it
  ([constructor](/Users/vm/owner-plane-d0a-spec.md:1267)), while CDDL requires
  it ([shape](/Users/vm/owner-plane-d0a-spec.md:4535)).
- A Txn contains up to 16 records in one atomic frame
  ([frame](/Users/vm/owner-plane-d0a-spec.md:1151)), but “frame order” does
  not state the order of two journal records inside one Txn. Freeze order as
  `(frame ordinal, records array index)`, validate sequentially against
  transaction-local state, then commit the frame all-or-nothing.

These should become schema/lint assertions and tiny executable fixtures, not
another conceptual essay.

## 4. Revocation and delayed cessation are substantively closed

The old revocation contradiction is gone from the active law. Appendix A now
requires linkage validity, total authorship cutoffs, and an empty
state-derived decryptable-wrap domain
([completion](/Users/vm/owner-plane-d0a-spec.md:4117)); `rotation_refs` are
linkage, never coverage ([references](/Users/vm/owner-plane-d0a-spec.md:4137));
and `c.revoke_zones` adds linkage and/or authorship coverage without reviving
the withdrawn references-cover-zones law
([continuation](/Users/vm/owner-plane-d0a-spec.md:4163)).

The registry assigns exact reference outcomes and makes a pending revocation
cease the certificate at the completing control position
([registry](/Users/vm/owner-plane-d0a-spec.md:1475)). This is the right answer
to D-187: a grant issued while the compound was pending was paired with an
effective certificate; it is not retroactively a post-cessation grant.

Remaining items are mirrors:

- the `ref-unresolved` disposition row does not list incomplete authorship or
  a nonempty decryptable-wrap domain
  ([map](/Users/vm/owner-plane-d0a-spec.md:2445));
- the family-7 constructibility cases say an incomplete compound “must fail”
  without pinning `(ref-unresolved, pending-dependency)`
  ([vectors](/Users/vm/owner-plane-d0a-spec.md:3123));
- §4.2 and §10.2 still say only “ending operation's control position” rather
  than explicitly naming the completing carrier for an initially pending
  revocation ([certificate](/Users/vm/owner-plane-d0a-spec.md:362),
  [admission](/Users/vm/owner-plane-d0a-spec.md:2334)); and
- the wrong-incarnation vector says only `storage-quarantine`, omitting the
  `log-corrupt` outcome ([vector](/Users/vm/owner-plane-d0a-spec.md:3507)).

The D-159 decision-history row still literally says `rotation_refs` cover wrap
zones ([history](/Users/vm/owner-plane-d0a-spec.md:3759)). D-180/D-195 clearly
supersede it, so this is history hygiene rather than a second active law; add
the supersession pointer so a textual audit does not report a false live hit.

No new revocation mechanism is needed. Put the existing pending-revoke →
window-grant → completing-continuation case into the corpus and let the
reducer prove the mirrors.

## 5. D-176 and D-177 now compose

The dead-stage conflict is substantively repaired. Applicable-stage
consumption remains scoped to the consumer's coverage domain
([relation](/Users/vm/owner-plane-d0a-spec.md:1816)), while an immutable
authority-ending frontier is now explicitly the vacuous consuming event for a
lineage that leaves that domain
([transition](/Users/vm/owner-plane-d0a-spec.md:1861)). Appendix A mirrors the
rule ([CDDL comment](/Users/vm/owner-plane-d0a-spec.md:4319)), and the
revoke→regrant trace is named ([vector](/Users/vm/owner-plane-d0a-spec.md:3206)).
The executable corpus should pin which completing control position performs
that consumption, especially for a revocation that initially pends, but the
state-machine direction is now coherent.

The collision repair is also coherent. `import-collision` is in the derived
quarantine lane ([map](/Users/vm/owner-plane-d0a-spec.md:2446)); when the
standing freeze basis dies, the claimant fold re-runs including former
collision losers ([fold](/Users/vm/owner-plane-d0a-spec.md:2902)); and the
incremental/fresh replay vector states the same transition
([vector](/Users/vm/owner-plane-d0a-spec.md:3456)).

One sentence still calls collision losers claimants that can “NEVER” win and
then immediately says their reservation status re-derives if the basis dies
([sentence](/Users/vm/owner-plane-d0a-spec.md:2882)). Replace “NEVER” with
“cannot win while the standing freeze basis remains.” This is clarity, not a
new lifecycle.

## 6. Mechanical sweep

The substantive P/−P acceptance setup is correctly in family 7
([vector](/Users/vm/owner-plane-d0a-spec.md:3276)), and D-190/D-197 now say
family 7 ([decision](/Users/vm/owner-plane-d0a-spec.md:3790)). D-182 still says
the residual was pinned in family 13
([stale label](/Users/vm/owner-plane-d0a-spec.md:3782)); family 13 is storage.

These direct schema/comment/label defects can be repaired with assertions in
the artifact-bearing change:

1. `xferreopen.basis: opfactref`;
2. add `incarnation` to both prose terminal constructors;
3. replace the stale ItemCommit erasure-boundary comment;
4. unify the Appendix cause-absence list with the main table;
5. add revocation contexts to the outcome/disposition fixture;
6. pair wrong incarnation as `(log-corrupt, storage-quarantine)`; and
7. correct the D-182 family label and annotate D-159's supersession.

## Recommended artifact-first handoff

Do not use this review as the seed for another free-form rewrite. Use it to
create failing, canonical fixtures.

1. Archive the exact v0.5.18 bytes and SHA above as the red baseline.
2. Author the closed companion case schema before authoring fixtures.
3. Add, at minimum, these cases:
   - marker → tombstone → crash-before-destination-commit;
   - `C1 → I → C2` versus `C1 → C2 → I` for delayed cert/grant;
   - statement-kind `XferReopen.basis` rejected, statement-kind
     `invalidation` accepted;
   - Abort/Reopen and competing terminals inside one Txn;
   - pending revoke → grant in the window → completing continuation;
   - stage → revoke → regrant; and
   - frozen owner → collision loser → freeze-basis death.
4. For each fixture, store canonical input bytes, every delivery order, crash
   cuts where applicable, expected intermediate dispositions, and expected
   final state.
5. Make the reference reducer pass, then run an independently implemented
   reducer or differential checker. The reference implementation must not be
   its own sole oracle.
6. Admit future behavioral findings only with a failing trace. Direct schema
   contradictions may instead use the smallest failing schema fixture or lint
   assertion.

For the two hard failures, prefer scope reduction over another mechanism
family:

- **References:** unheld cert/grant remains `ref-unresolved` in v1; defer a
  portable absence proof.
- **Transfer:** either forbid cryptographic erasure while a referencing
  transfer is nonterminal, or move the completion journal to D0-Transfer.

This is not a rejection of the design direction. It is the convergence move
the project now needs: v0.5.18 has made the remaining disagreements concrete
enough to execute. The next useful evidence is bytes and reducer state, not a
nineteenth prose interpretation.
