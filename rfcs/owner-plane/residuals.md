# Residuals ledger (D-206)

Non-blocker findings from Gate-A reviews land here instead of
reopening the gate. Per D-206 (§15): a **blocker** is exactly an
executable red / normative-divergence finding or a spec↔companion
contradiction two conforming implementations would disagree under;
everything else — editorial drift, stale comments or counts,
documentation mismatches, coverage-annotation gaps, style — is a
**residual**. The reviewer files residuals with severity labels; the
program records them here and repairs them in ordinary follow-up
commits. Residuals do not reopen Gate A, do not reset a review, and
do not require a further fresh review.

Severity labels: **high** (worth a prompt follow-up commit),
**medium** (batch with the next ordinary commit touching the area),
**low** (opportunistic).

| # | Filed | Source review | Severity | Description | Status | Repaired by |
|---|---|---|---|---|---|---|
| R1 | 2026-07-16 | `2026-07-16-gate-a-44b56f96-closure-review.md` | low | README layout `reducer/` row said "168-vector corpus"; the corpus at the pin is 170 | repaired | the §16 stamp filing (2026-07-16) |
| R2 | 2026-07-16 | same | low | README layout `archive/` row said "(v0.1 → v0.5.19)"; the archive holds drafts through v0.5.22 at the pin (v0.5.23 added by this filing) | repaired | the §16 stamp filing (2026-07-16) |
| R3 | 2026-07-16 | same | low | Audit §2 D4 entry ended "remains worth adding at freeze" — the sentence was added by the pin commit itself | repaired | the §16 stamp filing (2026-07-16) |
| R4 | 2026-07-16 | same | low | Audit §2 D9 entry — same stale "remains worth adding at freeze" clause | repaired | the §16 stamp filing (2026-07-16) |
| R5 | 2026-07-16 | same | low | Audit §4 R8.11 deferral paragraph read as open; the pin commit executed exactly that D-151 correction | repaired | the §16 stamp filing (2026-07-16) |
| R6 | 2026-07-16 | same | medium | Spec §16 status line's reason clause ("companion, corpus, surface runs, audit do not yet exist") was stale at the pin | repaired | the §16 stamp itself (2026-07-16) |
| R7 | 2026-07-16 | same | low | Spec §16 checklist's final clause carried the pre-D-206 "finds only editorial drift" phrasing | repaired | the §16 stamp itself (2026-07-16) |
| R8 | 2026-07-16 | same | low | `evidence_class` (companion #7) is builder-emitted and schema-required but no artifact consumes it as validation input; a mis-declared class on a FUTURE vector would not automatically red. Suggested: a one-line harness check (declared class ↔ derived structure). The two committed vectors are correctly declared | **open** | — |
