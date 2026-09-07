# Criterion-12 fresh review (review-2) — owner-plane-d0a @ 2f66b592

*2026-07-15. The fresh independent re-run the reconciled review's
acceptance criterion 12 requires, over the seven-commit repair
tranche (52b9b8e5..2f66b592, PR #318 draft). Spec v0.5.20 unchanged
(`ec3a9a6d…` verified); companion `11dd8897…` (amendments #5/#6)
verified; corpus 165 (histogram re-verified from files: f09×12,
f11×36 — matches the regenerated audit header). Read-only on the
repo; every experiment on scratchpad tree copies; PR untouched.*

## Verdict

**No executable finding.** All eleven artifact-facing acceptance
criteria verify with evidence I executed myself; criterion 12 is
this report. The reducer survived an adversarial order battery
strictly stronger than both prior review sweeps; every previously
reproduced hole now shows its repaired behavior under my own
re-traces; both lanes' green-wash defenses fire on the committed
path; the gates go red where they must. What remains is a short
documentation-truth list (below) — the largest item is one
unrecorded stale row in a ratified owner document. Whether that
list gates the stamp is the owner's call; nothing in it is
behavioral.

## Reproduced at 2f66b592

core **141/141** · reducer **36/36** (including the metamorphic
corpus test and the arrival-order restoration control) · strict
harness **165/165, exit 0, ~6 s** with the metamorphic sweep aboard
· storage lane **19/19** on real files, **sync_all=10 rename=10**
counters printed (local macOS; CI logs show the same counters on
windows-latest) · browser lane **56/56** in local headless Chrome
150, substrate 16 vectors (records=45, bytes=40053, frames=72,
cuts=11 — grew with the re-minted f13 txn vector; matches the CI
log at 2f66b592) · mint re-run reproduces the committed bytes
exactly (vectors + coverage, `lane-manifests.json` included) · all
five advisory CI jobs green at 2f66b592, job-level.

## The acceptance criteria, each with my own evidence

1. **Suites green at one pinned commit** — reproduced above, plus
   CI. ✓
2. **Eight orders committed + generated suite CI-visible** — all
   eight reconciled-review orders verified present as committed
   deliveries; the metamorphic sweep (exhaustive ≤ 5 items;
   reversal/rotations/adjacent transpositions above, on every listed
   base and the sorted order) runs inside fold, journal,
   status-derive, and audit-partition on every harness invocation
   (export-import inherits via its fold delegate). **My independent
   battery on top: 5,509 adversarial orders across all 79
   convergence-bearing vectors — exhaustive permutations through
   SIX items (the suite stops at five), 40 seeded random shuffles
   per larger vector, walkthroughs included — every one converges,
   exit 0.** The canonical fold (state = pure function of the
   delivered set, content-derived processing order, canonical
   duplicate assignment to the lexicographically first name of a
   byte-group) makes this structural, and the code reads that way. ✓
3. **The suite discriminates** — the committed
   `convergence_standard_fails_under_arrival_order_restoration`
   reconstructs the pre-repair arrival-ordered pending loop
   faithfully (per-arrival classification, arrival-order retry, the
   LIVE `classify`), asserts it diverges on the review's
   `[r2,r1,c1,c2]` while the canonical engine converges on the same
   order. Not a strawman. ✓
4. **Body-hash mismatch exerts no precedence** — my re-run of the
   reconciliation's trace: one body byte of `x2` flipped (header
   signature intact), expectation corrected to
   `body-hash/reject-permanent` → the reducer derives exactly that
   for x2, and the failure line names **e2 deriving None
   (admitted)** where the committed vector expects
   freeze-classification — the plane did not freeze. Second leg: a
   signature-tampered reuse of a consumed request_id now derives
   `sig-invalid`, not `request-fork` (my trace, green under the
   corrected expectation including the sweep) — request-ID
   consumption is transition-last. ✓
5. **Forged/unadmitted recoveries cannot verify a kill** — the
   journal resolves invalidations through fold authority (delivered
   and rejected → dead citation, log-corrupt; cut/frozen overlay →
   dead; not accepted → pends verifiable-when-admitted; accepted →
   shape-bound and the basis must be genuinely dead on the fold).
   Both arms committed (`f11-reopen-forged-recovery-log-corrupt`,
   `f11-reopen-unadmitted-recovery-pends`); flipping the forged
   vector's expectation → the reducer still DERIVES
   `(log-corrupt, storage-quarantine)`, gate red — no echo. ✓
6. **Incomplete partitions cannot release** — `audit_release_check`
   derives completeness (exact `0..count−1`), re-derived
   disjointness, two-directional union against the INDEPENDENT
   `inputs.release` event, and one-Txn membership; five refusal
   vectors committed (missing-middle, missing-last, omitted-result,
   extra-result, split-txn). My previous-round green trace (drop
   chunk a1, fix the expected list) is now **red**. ✓
7. **Annotation loss reddens the lane** — surfaces enforcement is
   now EXACT equality with the §13.2 R-set (two named case_kind
   exceptions); `coverage/lane-manifests.json` (browser 56, storage
   19) is minted + drift-gated; both drivers pin bidirectionally.
   My re-traces on the committed path: dropping `browser` from the
   high-S vector → driver exit 1 naming the missing file; dropping
   `storage-*` from an f13 vector → lane exit 1 ("executed 18 vs
   required 19"). The `LANE_VECTORS_DIR` override skips the browser
   pin BY DESIGN (negative-control hatch, documented in code; CI
   never sets it). ✓
8. **Flush/replacement proven** — every stream materializes via
   write-temp → `sync_all` → `rename`, reads target the FINAL path
   (a bypassed rename fails structurally), and end-of-run invocation
   counters redden a zero count; counters printed nonzero on macOS
   (local) and all three CI OSes. ✓
9. **D-202 lifecycle executable** — `f09-lease-lifecycle-sticky-
   reproposal` (evidence-lifecycle lane, receipts as DELIVERED
   items) walks all four ruled propositions: stale on late-only
   evidence; timely-first wins (the endpoint pair); the original
   stays `lease-stale` after timely evidence arrives — load-bearing,
   because the runner re-evaluates revivable rejections, so without
   the `stale_issued` registry the op would revive and the vector
   would fail; the re-proposal admits in every listed order. The
   lane is exempt from fresh-fold/metamorphic convergence because
   arrival is its SEMANTIC input (the ruled per-replica
   divergence). ✓
10. **Empty-corpus and non-permutation controls red** — empty dir →
    exit 2 ("no vectors"); a delivery with a duplicated item →
    structural red, and the bin now PRINTS the convergence layer's
    per-vector reason. ✓
11. **Ledgers/comments/counts/prose match** — verified: regenerated
    histogram; riders in both coverage JSONs now name all six
    executing surfaces; the fifth saga marked "audit-added … NOT
    among the four sagas the D-203 ruling names"; workflow header
    rewritten; browser-lane clippy step added; storage-lane header
    fixed; companion's family-3 exclusion re-scoped to P-256; the
    D-151 spec-row staleness recorded rather than edited. **One
    residual below (the P1 profile row).** ✓ modulo F-1
12. **Fresh independent review** — this report.

The ninth order-sensitive fixture the tranche itself found is
legitimate: D-93's ledger rider says verbatim "the differing-hash
rejection superseded by D-130 (committed-boundary selection)", and
the re-authored `f07-revoke-cutoff-head-mismatch-selects` implements
exactly D-130 (the committing boundary admits; the losing suffix
quarantines `cutoff`). No fixture-invented semantics.

## Findings (documentation-truth; no executable defects)

- **F-1 — `p1-v1-profile.md` §C.1 row 4 is stale and UNRECORDED.**
  The ratified profile still says "mismatched-hash rejects" and
  names `f07-revoke-cutoff-head-hash-mismatch-rejects`, which no
  longer exists; the spec (D-93 rider, D-130) and the corpus now say
  selection. The tranche recorded the analogous D-151 spec-row
  staleness explicitly (audit §4) but not this one. Same remedy:
  record it in the audit's documentation-correction list for the
  owner's freeze-time pass — the profile is an owner-ratified
  document and should not be edited unilaterally.
- **F-2 — the CI metamorphic sweep exempts the walkthrough lane**
  (4 vectors). My battery ran their exhaustive permutations — all
  converge — so no defect exists today; wiring the sweep into
  `run_walkthrough` is one call and closes the asymmetry.
- **F-3 — the D-202 stickiness disable-control is not committed.**
  The audit says "disabling it reddens the gate" — a development
  probe, not a committed test (criterion 3 got one; this didn't).
  The registry's load-bearing role is structurally evident (the
  revival re-evaluation loop), and the vector pins the behavior;
  a committed control would pin it against refactors.
- **F-4 — audit §5 layering.** The twelve old clause annotations
  ("holds (tranche N)") sit verbatim under a verdict paragraph that
  withdraws them; the acceptance-criteria record then carries the
  real state. Post-tranche the annotations happen to be true again,
  but the layering invites misreading — worth flattening at the
  next amendment.
- Historical figures note: audit §5 criterion 10 cites the browser
  substrate at 94848163 (records=37, bytes=30781) — accurate as a
  citation; current runs print records=45/bytes=40053 after the f13
  re-mint. No action beyond awareness.

## Scope respected

Gate-B production concerns, D11, the §4.7 wire gap (still honestly
`Unimplemented`, unreachable from the corpus), and the P1 write bar
were not relitigated. No repo writes, no PR changes, no history
rewrites; all tampering on scratch copies, restored and re-verified
green afterward.

## For the owner

Criterion 12 is discharged with no executable finding. The four
documentation items above are one short amendment (F-1 is the only
one touching a ratified document's accuracy, and only as a record).
The stamp decision — including whether F-1..F-4 ride the freeze-time
pass or precede it — is yours; P1 durable writes remain barred until
Gate B plus the umbrella's P0.5/tombed-cutover prerequisites
regardless.
