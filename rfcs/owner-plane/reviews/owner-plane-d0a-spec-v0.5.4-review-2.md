# Review 2: D0-A Core + Memory specification v0.5.4

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.4 (2,650 lines), against the v0.5.3 synthesized review's six
consolidated blockers and six required decisions, the new rulings
D-86..D-92, and the v0.5.3 archive (434 changed lines diffed).
Written without reading `owner-plane-d0a-spec-v0.5.3-review.md`.
Method: bytes-first — every new mechanism checked against its CDDL
and E8/E7/tag-inventory rows before its prose; the synthesis's
counterexamples re-walked against the new rules (fresh-replica
revoked-grant replay, budget arrival-order, dense-feed backfill,
rotation queue deadlock, renewal encoding); plus a line-item audit of
every repair the synthesis named.*

## Executive verdict

**The six v0.5.3-synthesis blocker areas are discharged with the
strongest protocol machinery of the series, and each fix is the
structural one rather than the patch:** D-86 generalizes
position-relative authorization to certificates and grants (a fresh
replay now equals an incremental one — the revoked-grant
counterexample dissolves) and makes budget consumption a canonical
`(gen, seq)` fold with deterministic displacement; D-87 hash-chains
every proof feed (`prev_stmt`/`stmt_id`, boundary `head_hash` on
cutoffs) so backfill fails the chain identically everywhere and
`issuer-gap` becomes an honest pending-dependency — the
arrival-relative rule is voided, and `repoch` binding defeats the C3′
attestation-resurrection replay; D-88 pages checkpoints (≤ 256-head
`covers` per page, union-of-latest-pages coverage, explicit
`fences` for pending coverage, E6 conformance, the forever-pending
dropped-witness posture named); D-89 fixes the rotation queue
deadlock (wrap-adds valid for any accepted unretired epoch,
intent closing at the Fence, serialization moved to durable state 6)
and lands the real `cenrollnew / cenrollrenew` union — my v0.5.3 B1 —
with KEM-conditional replacement wraps; D-90 resolves transfer
erasure the honest way (erasure wins; immediate `XferAbort`); D-91
adds the arm-indexed control pipeline and commits to the
`d0a-vector-cases.v1.json` companion as the first corpus artifact.
The three-round-stale header provenance is finally fixed.

**Five findings remain — one blocking, two high, two medium — and
for the first time none of them is missing machinery: one is a fresh
contradiction the new ruling created, and four are synthesis line
items the patch skipped.** All are ≤ 2 sentences each; none needs an
owner decision beyond consequences already ratified. This is a
~10-sentence v0.5.5, and it should genuinely be the last prose cut.

---

## Part 1 — Discharge audit

| # | v0.5.3 blocker | v0.5.4 disposition | Residue |
|---|---|---|---|
| 1 | Historical authorization; closure; budget order | **D-86**: cert stage and proof stage both position-relative against explicit revocation/renewal cutoffs (§10.2 rewritten; fresh = incremental — counterexample re-walked); budget window = the bump at-or-before the signed epoch's opening op (never fold-current); canonical `(gen, seq)` consumption with displacement → quarantine (family-10 vectors) | **B1** (T2/D-69 contradict the displacement rule); **H1** (no `op.epoch ≥ grant.epoch` bound); **H2** (no closure continuation > 64 lineages); **M1** (missing-head lifecycle); **M2** (cutoff equality pins) |
| 2 | Dense-feed finality; requester freshness | **D-87**: `prev_stmt` on all five statements (both CDDL mirrors + T3 rewritten; `stmtid` tag added); cutoffs commit `{key_id, through, head_hash}`; backfill = `issuer-fork` via chain, replica-independent; missing link = `issuer-gap` pending (disposition table updated); `repoch` bound into both attestations (C3′ resurrection defeated); min-merge kept | none |
| 3 | Checkpoint bytes and finality | **D-88**: paged `covers` (≤ 256, E8 rows added) with union-of-latest-pages coverage; `fences ≥ covers` per lineage — pending ops compare against fences (v0.5.3's accepted-heads rule could never fire and is fixed); retirement = exactly `retired` (§4.6 aligned); no `v` on the body (E6); `proof_positions` gain `head_hash`; the dropped-witness forever-pending posture stated with its three exits | none |
| 4 | Rotation completion; renewal custody | **D-89**: `c.wrap_add` valid for any accepted unretired epoch + logical-key supersession (queue deadlock dissolved); intent closes at the Fence (`RewrapComplete` waits on the frozen intended set); serialization on durable **state 6** — the KekDestroyed-boundary tombstone gap is closed with the right reason given; `cenroll` a real discriminated union; renewal wraps iff KEM rotation, per accepted-unretired epoch, superseding by `(zone, epoch, device)`; history cutoffs ≤ 64 + standalone-`c.cutoff` union for more | pin 3 (renewal wrap count vs the 128 cap) |
| 5 | Transfer after source erasure | **D-90**: erasure makes the flat bundle underivable → immediate `XferAbort` with all un-imported records (`reason` = first terminal trigger); committed imports stand; destination rejection still completes the rest; `bundle_size`/`content_digest` = admission-time-validated signed facts | none |
| 6 | Control precedence; vector typing | **D-91**: `admit_ctrl` pipeline (parse → per-arm signer → sig → placement with C3′ validity-before-precedence → body); companion schema normative, authored-before-any-fixture, harness-enforced, in discrepancy-audit scope | pin 1 (companion not in the Open-tracked list) |

D-92's pins all verified in place (epoch 1 active with no Fence;
`KekDestroyed.epoch` = destroyed = new − 1; `RewrapDone.count` =
survivor pair count; `c.grant` to a revoked device rejects;
same-`(zone, lineage)` cutoffs compose at the maximum; the hosted
cross-lineage attested-cutoff negative; provenance repaired). My
v0.5.3 findings: B1 ✓ (the union), M1 ✓ (header — after three
rounds), pins: wrap-add epoch ✓, dropped-witness posture ✓, hosted
negative ✓; the cutoff-equality pin was adopted by the synthesis but
**not landed** (→ M2 below). B.1/B.2/B.3 untouched.

---

## Part 2 — Findings

### Blocking

**B1. T2 and the D-69 row still say admitted operations are
revisited "only by explicit compromise cutoffs" — D-86's budget
displacement is a second revisit path, and the texts now
contradict.** §4.7 T2: *"Admitted operations are revisited **only**
by explicit compromise cutoffs (T4, device or service)"*; §15 D-69:
*"compromise cutoffs are the sole revisit of admitted operations"*.
§4.3 (D-86): a late-arriving canonically-earlier operation
*"deterministically displaces later-ordered operations past the
budget line, and displaced operations move to quarantine-reproposal"*
— an admitted operation un-admitted by something that is not a
compromise cutoff. An implementer following T2 literally never
displaces; one following D-86 does; their accepted sets diverge under
unknown-gap concurrency — the exact divergence class Gate A exists to
prevent, in the sentence that anchors the determinism story. Fix
(two sentences): T2 enumerates both revisit paths ("explicit
compromise cutoffs (T4) and canonical budget displacement (D-86) —
both deterministic in fold inputs"); the D-69 row gets a "refined by
D-86" pointer, matching the record's own convention.

### High

**H1. The backdated-anchor hole is still open — the synthesis's
`op.capability_epoch ≥ grant.capability_epoch` bound did not land.**
§9.4's slack rule is unchanged (`op − grant ≤ slack`, meaningless for
negative differences), and no other rule binds an operation's anchor
to its grant's issuance epoch. Concrete: a device enrolled at epoch 5
(its grant pinned to epoch 5 at issuance) opens generation 1 and
signs `capability_epoch = 1` — opening passed (epoch 1 is open),
monotonicity passed (no predecessor bound), no closure cutoff names
this lineage (it wasn't live at the earlier advances) — so its
proofs evaluate under `policy(1)`, the weakest historical witness
policy, in strict zones too. A grant should never authorize anchors
older than itself: one line, plus the family-10 negative.

**H2. Strict-zone closure is unconstructible past 64 live lineages —
the synthesis-named continuation did not land.** `czonepolicy` /
`cepochbump` closure `cutoffs` cap at 64 (E8/CDDL) and under
`strict` MUST cover **every** live lineage — so a strict zone with 65
live lineages can never accept another `c.zone_policy` or
`c.cap_epoch_bump`: the owner's policy authority bricks at a size the
schema otherwise permits. D-89 already built the exact mechanism for
renewal history cutoffs (union with pre-established standalone
`c.cutoff` ops); one sentence extends it to closure sets.

### Medium

**M1. The missing-cutoff-head lifecycle is still unstated** (a
synthesis blocker-1 line item). A `zonecutoff.accepted_through`
names a full `head` including its op hash; when the control operation
arrives before the replica holds that tenant position, nothing says
whether the control op admits with the boundary latent, pends, or
what outcome a later tenant op at those coordinates with a
**different** hash produces (fork evidence against the cutoff's
claim, presumably — say so). One rule sentence covering all cutoff
consumers (closure, revocation, recovery, history).

**M2. The cutoff zone/lineage equality pins did not land** (adopted
in the synthesis from my v0.5.3 pin, extended there to closure
sets). `crevokegrant.cutoff` should equal the grant's `zone` and
`lineage`; every closure-cutoff entry should name the advancing
operation's zone. As written, a mistyped admin cutoff silently closes
an unrelated lineage's chain as a side effect. Two comment lines in
A.3.

### Pins

1. **Track the companion schema as an open artifact**: `d0a-vector-cases.v1.json`
   is normative, external, and unauthored; the Open-tracked list
   (App-C #2, drill cadence) should carry it until it exists —
   Gate A's checklist otherwise claims a closed schema surface while
   a normative schema file is absent.
2. **Renewal wrap volume vs the cap**: `cenrollrenew` requires one
   wrap per accepted-unretired epoch of every held zone on KEM
   rotation — a 65-zone device with two live epochs exceeds the
   128-wrap E8 cap with no stated overflow path; one clause pointing
   at the `c.wrap_add` supersession lane (as rotation staging
   already does) closes it.
3. **Displacement telemetry**: displaced-to-quarantine operations are
   "surfaced" (§4.3) — name the surface (the same owner-review lane
   as quarantine-reproposal generally) so implementations don't
   invent distinct UX; one clause.

---

## Part 3 — Gate-A readiness

The synthesis's six decision areas are all decided and (modulo the
five findings) landed with bytes: authorization and budgets replay
canonically from signed inputs; proof feeds are self-authenticating
chains; checkpoints are bounded, paged, and carried; rotations
serialize through tombstones with closed intent; transfer terminality
survives erasure; control operations have their own pipeline; and the
vector contract has a named, normative home. The core, corpus,
harness, and the companion schema itself still do not exist — Gate A
remains mechanically impossible today, as it should until the
executable work runs.

**Recommendation.** Cut **v0.5.5**: B1 (two sentences), H1 (one rule
+ one vector), H2 (one sentence reusing D-89's union mechanism), M1
(one rule sentence), M2 (two comment lines), three pins — roughly ten
sentences, no new owner decisions. The residue profile has inverted
since v0.5.2: the machinery is now ahead of the prose (four of five
findings are the prose failing to keep up with its own rulings),
which is precisely the state where the prose↔vector discrepancy
audit outperforms another review round. After v0.5.5: author the
companion schema first (it is the corpus's contract), build
`owner-plane-core`, generate the families — lead with displacement,
backdated-anchor, chained-feed backfill, paged-checkpoint coverage,
and the rotation-queue crash matrix — record family 14, and let the
discrepancy audit decide Gate A. Durable P1 writes stay prohibited
until Gate B plus the umbrella's P0.5/tombed-cutover prerequisites,
unchanged.
