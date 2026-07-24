# Review 2: D0-A Core + Memory Specification v0.4 (freeze candidate)

*2026-07-12. Independent conformance audit of `~/owner-plane-d0a-spec.md`
v0.4 against the frozen umbrella (v3.1/D0), the v0.3 synthesized review's
eight blockers and go-ahead checklist, the decision record (D-1…D-47),
and my v0.3 findings (F1–F6 + pins). Method unchanged: walk the spec's
own invariants (§7.3 bootability, D-40's hosted-diary flow, the Gate-A
checklist) through every other rule, plus a fresh audit of the
v0.4-new machinery. Finding IDs are G1… .*

## Verdict

v0.4 is the freeze candidate. Every v0.3-synthesis blocker is discharged
at the wire/reducer level this time, not just architecturally: literal
policy bytes with pinned hashes and a relational rule model (D-45),
tagged device/service receipt issuers with plane/zone binding, per-item
derived wrap keys that remove the GCM birthday question outright (D-22
amended), renewal `history_cutoffs` preserving receipt-free solo
history, revocation by rotation-reference with pending-dependency
atomicity, the `Txn` union + `XferDone` completion journal, `nlen`
length guarding, a valid vector schema with a per-family surface
matrix, and honest hard cuts where verification doesn't exist yet
(cross-plane import fails closed until D0-B, D-44; root envelope bytes
to Gate B, D-46).

My fresh audit found **one blocking defect (one line, but it breaks
genesis encoding), two high-priority gaps, and a dozen pins** — all
editorial-scale. Nothing touches design. Recommendation: fold these as
a v0.4.1 patch, then **go** — implement `owner-plane-core` and the
vector corpus, and let the prose↔vector discrepancy audit be the next
review, as §16 intends.

## Resolution audit (compressed)

My v0.3 findings: F1 → D-39/D-40 + B.2's safe-human accept/retire rows
for `{personal, workflow}` and the corrected §11.2 prose (resolved —
verified against the literal B.2 table). F2 → genesis spacedefs pinned
in the `c.genesis` row and `cgenesis` CDDL (`home` personal/private/
workflow-v1; `audit` audit/private/owner-v1) plus the audit grant
(resolved, modulo G1/G3 below). F3 → D-45 `relation: self/author/any`,
required on every rule; B.2 expresses the session-self and
author-retract rules as real rows (resolved). F4 →
`require_cert_deadlines` = enrollment-time rejection, grandfathered
(resolved). F5 → `ctrl_frontier` explicitly declared diagnostic
(resolved). F6 → `audit.write` verb + genesis audit grant + any-class
service writer + fail-closed reads + terminating recursion (resolved in
design; see G1/G3/G4 for the residue). All fourteen v0.3 pins landed
except one triviality (`recovery_pk` still unconstrained in CDDL —
carried below), including the two I flagged as security-relevant:
`crevokedev` rotation-by-reference kills the 64 KiB revocation
overflow, and the E8 table now caps every array I listed.

Synthesis-only items, spot-checked: Connect inhabits the issuer union
(`src: "service"`, bound to `connect_service_key`); T3's issuer-seq
scope restarts on key renewal — which doubles as the snapshot-rollback
recovery path (clean); T4's revocation modes (`exclude` vs
`compromise`) give retro-disqualification exactly one meaning; the
solo-budgets posture is bounded by construction (finite budget required
in budgets zones); `deadline-unreceipted` correctly moved to
pending-dependency with GC-fence hardening; the declassification fold
now selects causally-maximal judgments first (sequential lowering
works) and preserves floors; `valid_from`/erased-status/revival
interactions are pinned with `as_of` consumed in exactly two named
uses; the JSON Schema is valid JSON with exact ChaCha20 RNG semantics.

## Blocking

### G1. `audit.write` is not in the closed verb vocabulary — genesis is unencodable

The genesis audit grant requires `ops = ["audit.write"]` (§7.1 row,
`cgenesis.audit_grant` comment), and §11.1's `m.audit` row consumes the
verb — but `audit.write` appears in neither §11.1's closed grant-verb
vocabulary nor Appendix A's `verb` enum. Under E9 strict decoding,
every `c.genesis` operation **fails parse** (`ops: [+ verb]` cannot
carry it), so the §7.3 bootability walkthrough fails at sequence 1 on
the spec's own rules. One-line fix (add the verb to both lists) plus a
decision: is `audit.write` grantable only to `service` actors
(recommended: state it in the verb table like `curate.instruction`'s
constraint), and is it excluded from §7.5(b)/§7.6's never-grantable
sets by construction?

## High

### G2. `w.gen` fails the scope-space check for every space-scoped grant — eviction bricks ordinary writers

§10.2's dev-arm admission checks `tenant∧zone∧space∧op∧kind`; `w.gen`
carries `space_id = SYS_SPACE` (N1); but a typical grant lists explicit
real space ULIDs (`spaces: [+ ulid]`), and SYS_SPACE — reserved-range
by definition — can never appear in that list. Result: only
`spaces: "*"` grants can open generation g ≥ 2; every space-scoped
writer hits `scope-space` on its `w.gen` after eviction/restore and is
permanently stuck. This defeats the exact browser-eviction continuity
§9.3 was built for. Fix: one sentence in §9.3/§10.2 — the spaces axis
is not evaluated for `w.gen` (SYS_SPACE is deemed in-scope for every
write-capable grant) — plus a family-10 vector: space-scoped grant,
eviction, `w.gen` accepted.

### G3. "The device's service lineage" — one lineage or two?

`c.genesis` (§7.1) issues the audit grant against "the device's
**service lineage**," but `cgenesis` carries exactly **one**
`lineagedef`, and `c.enroll` enforces "at most one live lineage per
device — a second while one lives rejects." Either the audit grant
rides the device's single lineage (then "service lineage" is a
misnomer to fix, and audit writes share the device's chain, budgets,
and generation window — acceptable but should be said), or genesis
mints a second, service-only lineage (then `cgenesis` needs a second
`lineagedef` and the one-live-lineage rule needs a service-lineage
exemption). The CDDL supports only the first reading; the prose
implies the second. Pin one.

## Medium

### G4. The audit-append-in-read-transaction rule has two unstated consequences

§11.1: a sensitive-space read whose audit row cannot durably commit
fails closed. Two follow-ons the spec should own explicitly:

- **Read-only contexts cannot read sensitive spaces.** A browser tab
  without the Web Locks writer lease and a process that lost the L3
  advisory lock are read-only by construction (§6.2/§6.3) — they cannot
  append the audit row, so every sensitive read fails closed there.
  Plausibly the intended posture (sensitive reads require the writer
  seat) — but it's a product-visible behavior that should be stated,
  with an edge outcome (`quota`? a new `audit-unavailable`?) named for
  it.
- **Audit-budget exhaustion darkens sensitive reads.** In budgets-
  posture zones every write-capable grant must carry a finite budget
  (§4.3) — including the audit grant — and on a hosted plane
  `c.cap_epoch_bump` is not ceiling-admissible, so an exhausted audit
  budget fail-closes all sensitive reads until a re-enroll compound
  ships a fresh audit grant. Either exempt `audit.write` from the
  finite-budget requirement (it is its own grant, charged 1 op/row and
  256 IDs/row — bounded by read quotas anyway) or document the
  re-enroll refresh path as the hosted remedy.

### G5. Initial capability epoch is unstated — and 0 is now reserved

§9.4 pins epochs as "per zone; consecutive; advanced by
`c.cap_epoch_bump`," and §4.3 reserves `capability_epoch = 0` for
read-only wildcard grants ("write grants MUST NOT use 0") — but no
sentence says a zone's capability epoch **starts at 1** at
`c.zone_create`/genesis. Without it, a zone starting at 0 makes every
first write grant illegal by the reserved-value rule. One sentence +
the genesis vector asserting epoch 1.

## Pins

1. **O4 vs §10.1 vs D-47 (daemon human evidence).** D-47 ratifies
   "daemon-class direct-human evidence," §10.1 shape 1 includes the
   daemon, but O4's parenthetical still reads "browser/native/mobile
   classes." Apply the ratified ruling to O4's class list — as it
   stands, actor-class derivation (`safe-human` needs "human evidence")
   is contradictory for daemon-class devices, which decides whether a
   daemon-seated owner counts under B.2's safe-human rows.
2. **T3 issuer-seq scope for service issuers.** "(device_id, signing
   key)" covers device issuers only; state the service scope
   (`key_id`) and what a Connect key rotation does to the counter
   (presumably: new key = new scope at 1, same as devices).
3. **O5 references "§11.9"** — no such section (audit lives in
   §11.1/§11.7). Fix the xref.
4. **`recovery_pk`**: constrain to `.size 32` in CDDL (§2.2 already
   pins Ed25519; recovery is always Ed25519 per R1) — carried from
   v0.3.
5. **Set/dedup markings for control arrays**: `cenroll.grants`,
   `wraps`, `rotation_refs`, `tenant_cutoffs`, `history_cutoffs`,
   `revoke_grants` are not marked as E7 sets — duplicate wraps for one
   recipient or duplicate cutoffs for one lineage are currently legal
   encodings with undefined semantics. Mark them sets (or define
   duplicate handling).
6. **Vector schema: `outcome`/`disposition` are unconstrained
   strings.** Constrain to the §10.4/§10.5 enums (or state the harness
   cross-validates them) — the synthesis asked for pinned enums and
   the schema is where they bind.
7. **Policy-hash precedence must flip at freeze.** B.2's "a mismatch
   fails the corpus, never the protocol" is right for the draft, but
   after Gate A the pinned hash *is* the protocol (M3). Add the flip
   to the Gate-A checklist so the sentence doesn't survive freeze.
8. **`c.lineage_reauth` requester attestation is replayable** — the
   signed statement `{lineage, max_generations}` has no nonce/epoch, so
   an old attestation can authorize a later window extension the
   device never requested. Low stakes (the admin is the beneficiary's
   owner in both lanes), but a `reauth_nonce` or the current window
   counter in the signed statement closes it for two bytes.
9. **Hosted unknown-eviction heads accumulate until re-root.** With
   `last_known = "unknown"`, prior heads persist pending an admin
   `c.cutoff` — which is not hosted-admissible. Bounded in practice
   (E8's 4096-head cap ≈ decades of monthly evictions) but worth one
   honesty sentence in §7.5(c) alongside the erasure one.
10. **`crevokedev.rotation_refs ≤ 16`**: a device wrapped into more
    than 16 zones cannot be revoked in one op; state the multi-op
    procedure (sequenced revocations) or assert the v1 zone-count
    assumption.
11. **B.2 display order vs canonical order**: the listing claims
    "canonical set order," but the row order shown is semantic. Since
    family 11 re-derives the bytes, either sort the display or drop
    the claim — an implementer transcribing the display should not
    inherit an E7 violation.
12. **`m.import.claim` + `expiry_deadline_ms`**: release expiry rides
    §9.1 for the *import* op — fine, but on a solo/budgets destination
    zone no receipt can exist; confirm the intended interaction
    (deadline field present on the release ⇒ import needs a receipt
    even in a budgets zone?) or scope the deadline check to the
    release's zone policy. One sentence.

## Umbrella and gate conformance

§15.1 D0-A duties: all discharged (re-checked against v0.4's moved
pieces — capability-epoch scope now §9.4+D-32, receipt/lease proof
types §4.7, flow admission §11.8 with typed egress profiles, expiry
§9.1 with the D-28 posture). Decision record: D-1…D-47 all trace to
normative text; the D-22 amendment and D-35's Txn generalization are
correctly annotated as amendments rather than silent rewrites; no
drift found. Gate B correctly remains necessary-not-sufficient with
the P0.5 and tombed-cutover dependencies named.

## What improved (credit)

- **Per-item derived wrap keys with a fixed nonce (D-22 amended)** —
  the correct fix, not a mitigation: key/nonce uniqueness holds by
  construction and rewrap idempotence survives as a hard invariant
  (I2's "a differing duplicate is corruption evidence" is now a
  theorem, not a probability).
- **Rotation-by-reference revocation with pending-dependency
  atomicity** — solves the overflow without weakening the compound's
  all-or-nothing semantics.
- **`history_cutoffs` on renewal** — the one mechanism that
  simultaneously preserves receipt-free solo history and keeps the
  superseded key dead for new authorship.
- **D-44's cross-plane fail-closed** — deleting the mediator lane
  rather than shipping an unverifiable attestation is exactly the
  right call for a gate document; same for D-46's honest Gate-B
  deferral of envelope bytes.
- **The B.2/B.3 literal objects** close the loop my v0.3 review asked
  for: the hosted diary's authority story is now decided by hashable
  bytes, not prose — which is what makes G1-class defects *findable*
  by the discrepancy audit at all.

## Bottom line

Patch G1–G5 and the pin list (a day of editing, one new vector
family-10 case, no owner decisions beyond ratifying the G3 reading and
G4's posture), then stop writing prose: build `owner-plane-core`,
generate the corpus, run the families on their named surfaces, and let
the discrepancy audit decide Gate A. The document has reached the
state where its remaining defects are exactly the kind that harness
will catch mechanically — which is the definition of done for a
specification round.
