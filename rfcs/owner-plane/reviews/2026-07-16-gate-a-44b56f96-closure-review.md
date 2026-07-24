# Gate-A closure review at pin `44b56f96` — fresh independent re-execution of the full battery

*2026-07-16. Reviewer: a fresh session with no prior authorship in the
owner-plane program, commissioned by the owner
(`~/owner-plane-inspection-commission.md`); reporting to the owner
only. Adjudicated under **D-206** (spec §15, owner-ratified
2026-07-16): the verdict is PASS unless a blocker — (a) any suite or
lane red at the pinned head, including the discrimination and negative
controls, or a demonstrated divergence between normative text and
committed artifact behavior that changes an admitted, pended,
rejected, frozen, or derived outcome; (b) a contradiction within or
between the spec and companion under which two conforming
implementations would disagree on a covered behavior's outcome — is
found. Everything lesser is a residual, filed below with severity
labels. In-repo filing name, per the program convention:
`reviews/2026-07-16-gate-a-44b56f96-closure-review.md` (this file is
delivered to the owner only; nothing was committed to the program
branch or the live worktree).*

---

## 1. Verdict

**PASS (zero blockers).**

Every suite and lane is green at the pin from a clean, isolated
re-execution; every commissioned discrimination and negative control
goes red exactly where it must; no normative-text ↔ artifact
divergence changing any outcome was found; no spec↔companion
contradiction bearing on a covered behavior was found. Eight
residuals are filed in §7 — all documentation/annotation drift, none
affecting any outcome. Per D-206 this is a PASS report; the §16 stamp
remains the owner's act.

## 2. Environment

| | |
|---|---|
| Host | macOS 26.4, arm64 (Darwin 25.4.0) |
| Toolchain | rustc 1.96.1 (2026-06-26), cargo 1.96.1 |
| Node | v25.8.2 |
| wasm-pack | 0.14.0 — equals the repo pin `.wasm-pack-version` |
| Browser | Playwright `chromium_headless_shell-1228` → HeadlessChrome/149.0.7827.55, driven over raw CDP |
| Worktree | `git worktree add /Users/vm/projects/gate-a-inspection-44b56f96 44b56f96 --detach` — a fresh detached checkout; all builds and runs happened there; the live `owner-plane-d0a` worktree was never touched |
| Cleanliness | after the entire battery **including a mint re-run**, `git status --porcelain` shows zero tracked-file changes and zero untracked paths |

## 3. Pin verification

- Branch state: `owner-plane-d0a` tip = `44b56f96ac56bfa1414e97abff321606116d4715`,
  exactly **one commit atop** `4b00e3f71779bb437b4f7ac1261bc64c8bc31513` —
  the owner-act filing commit the commission names.
- `git diff 4b00e3f7..44b56f96 --stat` touches exactly five paths, all
  markdown: `README.md`, `gate-a-audit.md`, `residuals.md` (new, the
  D-206 ledger), `owner-plane-d0a-spec.md`, and the byte-exact archive
  snapshot `archive/2026-07-16-d0a-v0.5.22-as-reviewed.md`. **No
  executable artifact (crates, vectors, harness, driver, coverage,
  CI) is touched.**
- Reading of the commission's "docs-only (register, audit, decisions
  files)" requirement: the spec file appears in the diff because the
  §15 decisions register lives inside the spec. I read every spec
  hunk: they are exactly the ratified closure-batch filing — the
  v0.5.22→v0.5.23 header bump with the D-205 explicit-keep and archive
  note, the new D-206 register row, the owner-ratified D4 sentence
  (C2: an op arriving while frozen classifies `(ctrl-fork,
  freeze-control)`, earlier pipeline stages keep their own outcomes),
  the owner-ratified D9 sentence (`m.audit` row: a partition-
  contradicting audit row is `(body-invariant, reject-permanent)` on
  every arrival order), and the D-151 row correction with its
  `c.enroll` row mirror. No behavior-bearing change rides along. I
  therefore did **not** stop-and-report; the commission's "v0.5.22
  lineage" identifies this same document line — v0.5.23 is the
  owner-act filing directly atop the reviewed v0.5.22.
- **Hash pins, all verified at the pin** (`shasum -a 256`):
  - spec v0.5.23 = `22f09e43ee59284e1c0903fd638dc8563b655443904b40e439582395159bb240` — equals README + audit header;
  - companion `d0a-vector-cases.v1.json` = `8d2f880006502563b528b64f70eb2f0fd3ccdb721b894df93596dc6bfab8d859` — equals audit header (companion untouched by the filing);
  - archived v0.5.22 snapshot = `30c91f941da7ba3458ed4886a5fab5a6be991703b7802668be56a4b8d531f5ef` — equals the prior recorded spec hash, byte-exact.
- Corpus: **170 vector files**, family histogram from my own harness
  run = f01×17, f02×7, f03×6, f04×4, f05×4, f06×6, f07×31, f08×4,
  f09×13, f10×7, f11×36, f12×15, f13×16, f14×4 — identical to the
  audit header's stated corpus.

## 4. Battery — per-suite transcripts (all executed in the isolated worktree at the pin)

| # | Suite / lane | Invocation | Result | Expected |
|---|---|---|---|---|
| 1 | core | `cargo test` (core crate) | **141 passed, 0 failed**, incl. `committed_vectors_match_builders` (vector drift gate), `committed_lane_manifests_match_generator`, `committed_outcomes_map_matches_generator`, `uncovered_outcomes_pinned_shrink_only`, `obligations_ledger_is_sound`, `every_vector_within_its_family_surfaces` | 141 ✓ |
| 2 | reducer | `cargo test` (reducer crate) | **37 passed, 0 failed**, incl. `convergence_standard_fails_under_arrival_order_restoration`, `d202_two_worlds_derive_ruled_states`, `semantic_red_fails_the_gate`, `harness_rejects_bad_vectors`, the journal reopen-kill pair | 37 ✓ |
| 3 | strict gate | `cargo run --release --bin harness` | **170/170 rows**, every row `container=ok companion=ok pairs=ok decode=ok convergence=ok semantics=PASS`; **exit 0** | 170, exit 0 ✓ |
| 4 | storage lane (this host) | `cargo run --release --bin storage_lane` | **19/19 PASS**; `sync_all=14 rename=14` = the corpus-derived stream count (counter **equality** gate); lock matrix `locks=REAL(1 denial(s))` across two real processes; internal flush-probe control self-reported green-plain/red-under-failpoint; **exit 0** | 19/19, 14/14 ✓ |
| 5 | browser lane | `node driver.cjs --browser <playwright headless shell>` | wasm package built fresh under the pinned wasm-pack; **56/56 green** in HeadlessChrome/149 over CDP; f13 substrate: 16 vectors over real IndexedDB transactions + Web Locks (records=45 bytes=40053 frames=72 cuts=11); **exit 0** | 56 ✓ |

Supplementary hygiene claims from the audit header, all reproduced:
`cargo fmt --check` clean (core, reducer); `cargo clippy -D warnings`
clean on core, reducer, reducer `--no-default-features --all-targets`,
and browser-lane on `wasm32-unknown-unknown`; reducer no-default tests
31/31; **mint byte-idempotent** — `cargo run --bin mint` followed by
`git diff --exit-code -- vectors coverage` leaves zero changes.

Advisory CI (consulted, not relied on): the most recent
`owner-plane.yml` run is **green at head SHA `44b56f96…`** — all five
legs concur with my local runs. (An earlier same-day `failure` run on
the branch predates the pin push sequence and is superseded by three
successes including the pin's.)

## 5. Discrimination and negative controls — every one red exactly where required

- **Arrival-order restoration control** (criterion 3):
  `convergence_standard_fails_under_arrival_order_restoration` — reads
  a real committed vector, runs the RETAINED pre-repair
  arrival-ordered engine on the review's r2 order and `assert_ne!`s it
  against the fresh fold (the restored engine **must diverge**, else
  the metamorphic suite tests nothing), then `assert_eq!`s the
  canonical engine on the same order (it **must converge**). Green
  within the 37 — the discrimination is real, verified in source and
  by execution.
- **Empty-corpus control** (criterion 10): `harness <empty dir>` →
  `harness setup failed: no vectors in …`, **exit 2**. Red as
  required.
- **Non-permutation control** (criterion 10): two tampered copies of a
  committed fold vector in a scratch corpus — one delivery with a
  dropped item, one with a duplicated item. Both fail the convergence
  layer with `delivery 1 is not a permutation of the item set (each
  item exactly once)` → `GATE RED`, **exit 1**.
- **Storage failpoint control** (criterion 8, commission-mandated both
  ways): plain run **green** (exit 0, counters 14/14); full lane under
  `STORAGE_LANE_FAIL_SYNC=1` → 14 failing vectors, `sync_all=0
  rename=0`, `STORAGE LANE RED`, **exit 1**; probe sub-mode exit 0
  plain / exit 1 under the failpoint. Not a green-only run.
- **Browser-lane negative control** (driver's own `LANE_VECTORS_DIR`
  mechanism): flipping `expected.result.valid` on
  `f03-p256-verify-high-s-rejected` in a corpus copy → `RED
  f03-p256-verify-high-s-rejected.json: … semantics=FAIL:
  valid=false, expected true`, 55/56, **exit 1**. The failure message
  is itself the live proof the lane's §3 low-S policy rides real
  WebCrypto (bare WebCrypto accepts high-S and would have matched the
  flipped expectation).

## 6. Criteria 1–11 of `gate-a-audit.md` §5 — per-criterion findings at the pin

1. **All suites green at one pinned commit** — VERIFIED by my own
   runs (§4), all at `44b56f96`, from a clean detached worktree;
   fmt/clippy/mint claims reproduced; advisory CI concurs at the same
   SHA.
2. **Eight orders + generated convergence suite** — VERIFIED: the
   metamorphic sweep (`metamorphic_orders`: ≤ 5 items exhaustive,
   above that reversal + every rotation + every adjacent
   transposition) is wired into the fold, journal, status-derive, and
   audit-partition lanes and runs on every harness invocation (my
   170-green run exercised it); the harness structurally requires ≥ 2
   byte-distinct listed orders on convergence-bearing vectors.
3. **The suite discriminates** — VERIFIED (restoration control, §5).
4. **The D-99 pipeline holds** — VERIFIED in source and by execution:
   `classify` orders sticky-memo → prevalidate (pins→arm→signature) →
   **complete body stage** (body-hash → registry row keyed by all
   three coordinates, `operation_version != 1` → `unknown-version` at
   the row consult → arm-indexed intrinsic CDDL with closed key sets)
   → replay consult → placement/freeze gate. All five committed
   regressions green: `f07-c2-post-freeze-cddl-invalid-kept`,
   `f07-consumed-request-id-cddl-invalid`,
   `f07-c2-post-freeze-extra-field-kept`,
   `f07-operation-version-unknown-rejects`,
   `f07-header-unknown-version-rejects`; the D4 trio
   (`…valid-op-frozen` / `…sig-invalid-kept` / `…cddl-invalid-kept`)
   matches the newly filed D4 sentence — first-failing-stage outcomes
   preserved under freeze.
5. **Forged/unadmitted recoveries cannot verify a kill** — VERIFIED:
   `f11-reopen-forged-recovery-log-corrupt` and
   `f11-reopen-unadmitted-recovery-pends` green in the gate; the
   reducer journal tests (`reopen_recovery_invalidation_unheld_pends`,
   `reopen_recovery_keeping_basis_rejects`) give the predicate both
   arms.
6. **Incomplete partitions cannot release** — VERIFIED: the five
   refusal negatives (`f11-audit-release-extra-result-refused`,
   `…-missing-last-refused`, `…-missing-middle-refused`,
   `…-omitted-result-refused`, `…-split-txn-refused`) all green;
   companion amendment #5's read-release input present.
7. **Annotation loss reddens the lane** — VERIFIED: core surfaces
   suite (exact §13.2 R-set equality) green; `storage_lane` enforces
   executed-set equality with `coverage/lane-manifests.json` in both
   directions (source + green run); `driver.cjs` pins the served set
   to the same manifest both directions and requires every §13.2
   browser family present.
8. **Storage flush/replacement proven, strength stated** — VERIFIED:
   counter **equality** (14 = corpus-derived stream count, not merely
   nonzero) gates the run; every destination pre-seeded with a stale
   sentinel, read back before the durable write, sentinel must be gone
   after (F4-C); the failpoint control proves the sync seam is invoked
   with its error propagating (§5); the seam's stated
   source-inspection limit (F4-A) is recorded honestly in the audit
   and on the seam.
9. **D-202 lifecycle in both ruled worlds, every relative order** —
   VERIFIED: `f09-lease-lifecycle-sticky-reproposal`
   (`evidence_class: late-first`, **three** listed deliveries, the
   third delivering the re-proposal `i2` before the original `i` — the
   D-205/ff23f1cd-F1 regression order) and
   `f09-lease-lifecycle-timely-first-forks`
   (`evidence_class: timely-first`) both green; the cross-world
   relationship pinned from one byte source by
   `d202_two_worlds_derive_ruled_states` (late-first: the original is
   terminally `(lease-stale, quarantine-reproposal)`); endpoints and
   boundary negatives (`f09-lease-stale-quarantines`,
   `f09-lease-late-then-timely-receipt-admits`,
   `f09-lease-present-no-receipt-pends`,
   `f09-lease-overlong-window-invalid`) all green; the reducer's
   sticky-memo runs before any pipeline stage, matching D-202/D-204/
   D-205 as written.
10. **Empty-corpus and non-permutation controls red** — VERIFIED
    (§5): exit 2 and exit 1 respectively.
11. **Ledgers, comments, counts, prose match** — VERIFIED **with
    residuals** (§7): the execution-lanes-plan header records the
    delivered six-surface state with dates; the P1 profile's
    `Unimplemented` count is stated as grep-derived and dated; the
    audit's corpus histogram and suite counts match my measurements
    exactly. The drift I found is filed as R1–R7 — all editorial,
    none count-bearing on any suite claim.

**Criterion 12** is this review, adjudicated under D-206: the battery
was re-executed in full at the single owner-named pin by a reviewer
with no prior authorship, and **zero blockers** were found.

### Normative-consistency sweep (blocker class (b))

Checked specifically, none contradictory:

- **D-206 rule text** — spec §15 row, audit §5 criterion 12, and the
  `residuals.md` preamble state the same rule; the commission's copy
  matches.
- **D4 sentence ↔ artifacts** — the filed prose matches the vectored
  trio's outcomes and the classify stage order (first-failing-stage
  preserved; only the resolving recovery admits — `c.recovery_succession`
  carries its own placement rules in code).
- **D9 sentence ↔ artifacts** — all five conflict vectors expect
  exactly `(body-invariant, reject-permanent)` on the contradicting
  row; the deliveries mechanism plus metamorphic sweep carries "on
  every arrival order".
- **D-151 correction ↔ corpus ↔ profile** — no renewal-after-revocation
  vectors exist in the corpus (as the corrected row now states); the
  ratified P1 v1 profile indeed fail-closes renewal
  (`cenrollrenew` → reject `op-unknown`) with a named workaround. The
  spec's full renewal semantics vs the profile's fail-closed posture
  is the deliberate, owner-ratified D-203 structure (named-outcome
  deferral), not a spec↔companion contradiction.
- **D-205 ↔ code ↔ vector** — the self-evidence exception is
  implemented as the pre-pipeline sticky memo; the regression order
  rides the sticky vector's third delivery; §13.1's
  evidence-lifecycle exception sits beside the universal-convergence
  sentence as D-205 says.
- **Spec §13.1 ↔ companion** — machine-checked on every harness run:
  the container schema is extracted from the spec's own fenced block
  and compiled by a real Draft 2020-12 engine
  (`container_schema_extracts_and_compiles`, `companion_compiles`
  both green), and every vector must satisfy both. The companion hash
  is unchanged by the filing.

## 7. Residuals ledger — filed per D-206, none affecting the verdict

Rows are ready to paste into `residuals.md` (status `open`, source
review = this document).

| # | Severity | Description |
|---|---|---|
| R1 | low | `README.md` layout table, `reducer/` row: "The full 168-vector corpus is reproduced" — the corpus at the pin is 170. (The Status section's "168" bullet is dated 2026-07-14 narration and acceptable under the dated-counts rule; the layout row is present-tense.) |
| R2 | low | `README.md` layout table, `archive/` row: "(v0.1 → v0.5.19)" — the archive at the pin holds byte-exact drafts through v0.5.22, as the spec header itself states. |
| R3 | low | `gate-a-audit.md` §2 D4 entry ends "The prose sentence remains worth adding at freeze." — the sentence was added by the pin commit itself; stale at the pin. |
| R4 | low | `gate-a-audit.md` §2 D9 entry ends "The prose row remains worth adding at freeze." — likewise satisfied by the pin commit; stale. |
| R5 | low | `gate-a-audit.md` §4 documentation-correction record, R8.11: "the spec's D-151 decision row still says 'two renewal-after-revocation vectors' … RECORDED here rather than edited unilaterally" — the owner's filing executed exactly this correction in the same commit; the deferral paragraph is now discharged and reads as open. |
| R6 | medium | Spec §16 Gate-A status line's reason clause: "The companion, the corpus, the surface runs, and the discrepancy audit do not yet exist" — all four exist at the pin; only the closure review (this document) and the owner's stamp were outstanding. The operative verdict "pending — currently false" remains correct. Worth updating at the stamp so §16 states the true reason. |
| R7 | low | Spec §16 checklist's final clause still carries the pre-D-206 closure phrasing ("the final prose↔vector discrepancy audit finds only editorial drift"); D-206 explicitly supersedes the "finds nothing" family. A cross-reference to D-206 at the stamp would kill the last echo. |
| R8 | low | `evidence_class` (companion amendment #7) is builder-emitted and schema-required, but no artifact consumes it as validation input: shared-structure enforcement is behavioral (all listed deliveries must agree on final verdicts) plus the committed pair's pinned cross-world test. The two committed vectors are correctly declared; a future mis-declared class on a new vector would not automatically red. A one-line harness check (declared class ↔ derived structure) would close it. |

## 8. Boundaries kept

No repairs were made; no file outside this review document and the
inspection worktree was created or modified; the live
`owner-plane-d0a` worktree and the program branch were not touched
(the worktree metadata entry for my detached checkout is the one
commissioned side effect); no contact with the head session or the
steward. The inspection worktree
(`/Users/vm/projects/gate-a-inspection-44b56f96`) is left in place as
evidence and is disposable (`git worktree remove`) at the owner's
convenience.

---

## Verdict line

**PASS (zero blockers)** — Gate-A closure review at pin
`44b56f96ac56bfa1414e97abff321606116d4715`, adjudicated under D-206;
eight residuals filed (§7), none gate-reopening. The §16 stamp remains
the owner's act.
