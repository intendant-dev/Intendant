# P1 v1 profile — every unimplemented normative mechanism, dispositioned

Status: **RATIFIED as drafted (owner, 2026-07-14; recorded in
D-203).** The five §C.1 mechanisms are implement-before-Gate-A work;
every §C.2 row is the binding fail-closed contract for a P1 v1
build. P1
writes stay barred until Gate B plus the P0.5/tombed-cutover
sequence regardless of this file; the profile exists because the
amended Gate-A predicate requires every normative v1 mechanism to be
either implemented-and-vectored or EXPLICITLY fail-closed — never
half-present. "Fail-closed" here is a hard rule for any P1 v1 build:
the triggering input is REJECTED with the named §10.4 outcome (or
refused at construction on the writing side); silently proceeding,
partially applying, or downgrading is a conformance violation.

The inventory below was derived from the reducer's `Unimplemented`
markers (99 sites; `grep -rn 'Unimplemented(' reducer/src/*.rs`),
which are the reference implementation's honest boundary: the strict
harness treats any reachable one as RED, so none is reachable from
the committed corpus.

## A. Not profile rows — differential-lane contracts

Markers that guard the FIXTURE boundary (malformed aux entries,
unknown `case_kind`/probe names/draw tags, undecodable items,
multiple journals in one vector, re-parse failures of already-held
bytes). These have no production analogue — they exist so a broken
fixture dies loudly instead of skewing the differential — and need
no disposition.

## B. Decision-blocked — resolved by ruling, not by profile

| Mechanism | Marker | Blocked on |
|---|---|---|
| Bare-writer actor class (judge-verb exercise; status counting) | `bare-writer actor class awaits the owner's D2 ruling` ×2 | D2 (decisions-pending.md) |
| Held stmt-kind reopen invalidation | `stmt-kind invalidation kill verification awaits a fork-discovery statement wire shape` | the §4.7 wire gap (Gate-A audit finding) |
| Reopen kill verification beyond recovery-cut (other op kinds; cross-chain bases) | `reopen kill verification for … awaits vectors` ×2 | vectors + the D-163 kill-class enumeration |
| Late-receipt lifecycle | (no marker — behavior matches sticky-rejection mechanically) | D5 (decisions-pending.md) |

## C. Normative v1 mechanisms — the profile rows

Disposition vocabulary: **implement+vector** (close before stamping
Gate A) or **fail-closed** (P1 v1 rejects with the named outcome;
the workaround column says what v1 operators do instead).

### C.1 Recommended implement+vector (security-critical; fail-closing guts a v1 pillar)

| Mechanism | Marker | Why fail-closing is unacceptable |
|---|---|---|
| Erase-manifest admission in the control fold (§5.4: `erase_op` cites an accepted `m.erase_request`, `target_op` ∈ its targets, item_addr binding) | ~~`erase manifest`~~ **DONE** — implemented + vectored (`f07-kek-rotate-manifest-admits` / `-target-outside-rejects`; D-66 first-manifest-wins effects, unheld citation pends) | Erase is a v1 pillar; the storage lane is signed-manifest-bound (D6) and admission now enforces the portable §5.4 face. |
| Compromise-mode device revocation (T4 `receipt_cutoffs`) | `compromise mode (T4 receipt cutoffs)`, `receipt_cutoffs under exclude` | The stolen-device response; v1 cannot ship with exclude-only revocation. |
| `rotation_refs` typed linkage on revocation compounds | `rotation_refs linkage` | Mandatory on hosted planes (§7.5); hosted is a v1 posture. |
| Frontier-close head validation (carried heads vs held ops) | `frontierclose heads` ×4, `cutoff heads below accepted ops`, `carried head mismatches the held op` (partially implemented; the markers are the residue) | Cutoff ceremonies are the revocation backbone; head validation is their integrity. |
| Re-fold classification of cut accepted ops | `re-fold parse of an accepted op` | The C3′ cut path is vectored; this residue arm rides the same invariant. |

### C.2 Recommended fail-closed in P1 v1

| Mechanism | Marker | P1 v1 behavior (outcome) | Workaround |
|---|---|---|---|
| Device certificate renewal (`c.enroll_renew`) | `cenrollrenew` | reject `op-unknown` | exclude + fresh enrollment (more ceremony, same end state) |
| Recovery renewal/freshness carriage + adopted rotations | `recovery renewal/freshness carriage`, `adopted rotations`, `non-ed25519 successor admin key` | recovery ops carrying them reject `body-invariant`; successor admin keys are ed25519-only | base-at-head recoveries with empty carriage (vectored shape) |
| Cross-plane transfer (import/destination) | `cross-plane import`, `cross-plane destination` | reject `body-invariant` at admission | single-plane v1; export stays in-plane |
| Mediated egress release | `egress endpoint release` | reject `scope-op` | in-plane release only |
| Connect service time witnesses (`c.service_key` qualification) | `connect time witness`, `service-issued statements` | statements from service issuers never qualify (ops depending on them pend `lease-missing` / `deadline-unreceipted`) | device-witnessed zones only |
| P-256 tenant writers | `p256 tenant signer` | reject `sig-invalid` at the signer stage | ed25519 writer keys in v1 |
| Classification judgments (`raise_class` / `declassify`) | `classification judgment arms` | reject `body-invariant` | classification set at claim time only |
| Mediated (non-direct-human) erase evidence | `mediated erase evidence` | reject `body-invariant` | direct-human erase requests only (the vectored shape) |
| Non-strict zone closure coverage | `non-strict zone coverage` | strict-only: advances without total coverage reject `body-invariant` (the vectored strict shape) | strict zones only |
| Ratify-carrying cutoffs + requester attestation forms | `ratify cutoffs`, `cutoff requester attestation` | reject `body-invariant` | staging + equation-only ceremonies (vectored) |
| Enrollment wraps beyond (current epoch × known zones) | `enroll wrap at non-current epoch`, `enroll wrap for unknown zone` | reject `body-invariant` | wrap-adds after zone creation/rotation |
| Re-revocation / revoked-citation revival arms | `re-revocation of a revoked grant`, `claim under a revoked grant`, `claim under a revoked certificate`, `compound target not enrolled`, `unfrozen order-loser` | reject `body-invariant` (`no-grant`/`cert-revoked` where the citation itself is dead) | n/a — degenerate shapes |
| Multi-generation writer histories | `w.gen generations` | second generations reject `lineage-gen` | one generation per lineage in v1 (rewind = new lineage) |
| Non-built-in status policies | `status policy {pid}` | judgments under unknown policies pend `policy-missing` (already the vectored shape for hash mismatch) | B.2/B.3 only |
| Non-`memory` tenants | `tenant {}` ×2 | reject `scope-tenant` | the memory tenant is v1 |
| Unknown operation types | `op_type {other}` | reject `op-unknown` | closed §7.1/§11.1 registry |
| Non-exclusive lock actions | `lock action {other}` | reject at the storage edge (`lock-denied`) | exclusive-lock v1 |
| Template-form budget cap | `template-form cap-exceed` | reject `budget` | literal-form budgets |
| Non-ed25519/hpke key-id kinds | `key name {other}`, `key-id kind {other}` | reject `key-malformed` | the two v1 suites |

## D. Enforcement

A P1 v1 build claiming this profile MUST reproduce the reducer's
verdicts on the full committed corpus AND emit the table's named
outcomes for each fail-closed mechanism (negative vectors for the
fail-closed rows are Gate-B work, tracked with the §13.3 pending
obligations in `coverage/obligations-13-3.json`). Ratifying this
profile amends nothing in the spec: every fail-closed row is a
narrowing an owner may lift by implementing the mechanism and
minting its vectors.
