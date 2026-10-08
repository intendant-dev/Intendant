# Review 2: D0-A Core + Memory Specification v0.2

*2026-07-11. Independent conformance audit of `~/owner-plane-d0a-spec.md`
v0.2 — the narrow schema/state-transition/test-coverage round the
synthesized review called for, not an architecture round. Checked against
the frozen umbrella (v3.1/D0), the synthesized v0.1 review's seven
blockers, the §15 decision record (D-1…D-27), and my v0.1 findings
(D1–D10). Finding IDs are V1… to avoid collision.*

## Verdict

Not Gate A yet, but close — one more revision, not another program. All
seven v0.1 blockers are materially discharged, and the discharge quality
is high (lineages, the edge/portable IAM split, the erase state machine,
and T2's self-receipt exclusion are better than what either review asked
for). The remaining problems are three **new internal contradictions
introduced by the v0.2 machinery itself** — each discoverable by walking
the spec's own §7.3 bootability invariant and D-14 ruling against its own
rules — plus a normative-shape deferral that would make Gate A's audit
impossible to complete, and a batch of pins. Every fix is local; nothing
touches the umbrella or the §15 rulings (two rulings need their *intent*
restored by V1/V2, not changed).

## v0.1 blocker resolution audit (one line each)

| v0.1 finding | Status in v0.2 |
|---|---|
| D1 control vocabulary can't bootstrap | **Resolved** — compound GenesisBundle/Enroll/Revoke, WrapAdd (D-16), Space/ZonePolicy ops, §7.3 bootability invariant + vector |
| D2 hosted ceiling bricks the lane | **Resolved in design** (D-14/D-15, compound ops) — but see V1: a different mechanism re-bricks curation |
| D3 op→IAM mapping, judgment authority | **Resolved** — §11.1 table, D-25 owner-only dispute counting, policy tables as pinned constants; two cells still mix actor-constraints into the permission column (pin 4) |
| D4 capability-epoch scope | **Resolved** — §9.4 per-zone, `CapabilityEpochBump`, lenient window, budget reset |
| D5 writer identity unbound | **Resolved** — lineages bind device_id, budgets/cutoffs lineage-summed, evasion vector required; strongest section of the revision |
| D6 IAM bricks zero-daemon | **Resolved** — §10.1 four shapes / §10.2 portable admission split (better than "vacuously satisfied") |
| D7 device-class ceilings | **Resolved as a table** (§7.6) — but its interaction with §11.4 creates V1 |
| D8 per-item erase ceremony | **Resolved** — D-17 request + batched rotation, §5.5 crash-safe machine |
| D9 receipt backdating | **Resolved** — T2/D-23 signer exclusion, AcceptanceReceipt/LeaseProof, fallback lanes; one key-vs-device pin (V8) |
| D10 missing bounds | **Resolved** — E8 caps + E9 strict decoding + boundary vectors; one cap pair is self-inconsistent (pin 2) |

My v0.1 pin-list: all eleven items landed (O3 wording, key_id domain,
S6 exactness, sentinels, renewal, evidence opacity, §11.6 per-item read
authz, erase test coverage, retract/retire, S7 custody, audit domain).

## New blocking findings

### V1. The owner-class derivation × §7.6 bricks curation on hosted planes — contradicting three of the spec's own commitments

§11.4: owner-class requires "a human actor on a **no-exclusions device
class** (§7.6) with `memory.curate`." §7.6 gives `hosted-browser` a
non-empty exclusion list (export flows, declassify, instruction-grade
curate, effects, admin). Therefore **no device on a pure hosted-genesis
plane can ever be owner-class**. Consequences, each contradicting the
spec itself:

- Curatorial `accept` (§11.1) is owner-class — so on the flagship hosted
  diary, nothing a human writes can ever leave `candidate`. Auto-context
  (`accepted ∧ pinned`) is permanently empty. This guts ruling **D-14**
  ("hosted planes store private/sensitive with accurate labels" — as a
  usable diary), which the ceiling revision exists to serve.
- `m.pin`, `retire`, and counting `dispute` (D-25) are owner-class —
  all dead on hosted planes.
- `m.erase_request` is owner-class `memory.curate` — yet §7.5(c)
  explicitly promises hosted planes "retrieval-exclusion erasure." The
  spec's own consequence sentence is unsatisfiable as written
  (contradicts ruling **D-17**'s hosted half).

The same construction bricks a mobile-unattested/other-only plane.

Fix (restores D-14/D-15/D-17 intent without widening anything): make the
class check **per-verb**, not blanket — owner-class = human actor with
`memory.curate` on a device whose class **does not exclude the specific
verb being exercised**. A hosted-browser owner then counts for
accept/retire/pin/dispute/erase_request (none of which are in its §7.6
exclusion column) while declassify, instruction-grade curation, export
approval, effects, and admin stay dead until re-root — exactly the
ratified ceiling. Add vectors: hosted owner accept/pin/erase-request
count; hosted declassify/graduation still rejected.

### V2. §9.1 acceptance deadlines make a solo plane — including every P1 deployment — unable to accept its own writes

The chain: `cert.expiry_deadline_ms` is **required** (Appendix A —
no optional marker), so every operation cites a deadline-bearing cert;
§9.1 accepts such an op **iff** a qualified `AcceptanceReceipt` or
covering `CheckpointWitness` exists; T2 disqualifies the op's own signer;
a single-device plane has no other enrolled device; `accept_connect_time`
and `deadline_fallback = "budgets"` are opt-in with the default
**fail-closed**. Net: on a fresh plane, **every operation quarantines**.
This directly contradicts the §7.3 bootability invariant ("write a
Memory claim … read it back" after `GenesisBundle` alone — a required
family-7 vector that would fail against the spec's own admission rules),
and it describes **P1 exactly** (one controller-backed plane, no sync, no
second device).

Fix needs two pieces:

1. **Name the genesis-default `ZonePolicy` object exactly.** §7.1 says
   `ZonePolicyInstall(defaults)` but the defaults are unstated — they are
   control state and determinism demands their exact bytes. The default
   for a newly created plane must be viable solo: either
   `deadline_fallback = "budgets"` (and the genesis first-grant then
   needs a budget or explicitly no deadline reliance), or
   deadline-bearing acceptance deferred until a qualified witness class
   exists.
2. **Decide the witness-transition rule**: what happens when the second
   device enrolls — automatic flip to fail-closed, or explicit
   `ZonePolicyInstall`? Either is fine; unstated is not. Add the
   solo→multi transition as a family-9 vector.

(Also worth one honesty line: deterministic deadline enforcement is
*definitionally* unavailable to a solo plane — no independent time
exists. That's why the fallback lane is the correct solo posture, not a
degradation of it.)

### V3. The most authority-critical bytes are deferred to fixtures — Gate A can't audit shapes that don't exist

Appendix A covers tenant bodies, receipts, `ZonePolicy`, and the policy
object, but the **control-operation bodies** — `GenesisBundle` (the most
complex compound object in the system), `EnrollDevice` with its
grants/lineage/wrap-adds, `KekRotation` with `erase_manifest`,
`RecoverySuccession` (C3′) — "ride the vector files one-to-one." That
inverts the normative relationship: fixtures should implement the spec,
not define it. Three concrete holes this currently hides:

- **The KEK recipient-wrap object has no shape anywhere** — the umbrella
  requires every wrap to carry its KEM algorithm ID (§4.9/S3), and v0.2
  names `wrap_adds[]` in three ops without ever defining the wrap struct
  (recipient key/device, `kem_alg`, HPKE `enc`, wrapped bytes, epoch).
- **`SpaceCreate` has no `space_class` field** — §11.1's assert admission
  ("space is workflow-class") and §11.4's policy rows
  (`space_classes`) both match on a space property that no control op
  declares. The designation vocabulary (workflow/personal/audit/…) needs
  a closed enum and a home in `SpaceCreate`/`SpacePolicySet`.
- The genesis-default `ZonePolicy` bytes (V2).

Fix: extend Appendix A to the control bodies (they fit in a page) before
Gate A review; keep the fixture files as *derived* pins.

## Medium

### V4. Hosted-lane control lifecycle: three liveness gaps

- §7.5(c) omits `LineageReauthorize` and `CapabilityEpochBump`, and
  `max_generations` defaults to 8 per epoch — a hosted browser that
  suffers nine evictions/restores (§9.3 requires a `w.gen` advance per
  restore) is **permanently denied** (`lineage-gen`) with no admissible
  remedy. Browser eviction is common enough that the umbrella's §4.4
  floor is built around it. `EnrollDevice`'s compound "WriterLineage
  create-or-successor" may be intended as the remedy via re-enrollment —
  if so, state explicitly that a renewal-`EnrollDevice` reauthorizes the
  lineage window; if not, admit `LineageReauthorize` under §7.5(c).
- Standalone `RevokeGrant` is not hosted-admissible, so the only hosted
  grant-lifecycle operation is whole-device revocation. Narrowing-only
  operations are safe by construction; recommend admitting `RevokeGrant`
  (and note that grant *addition* rides renewal-`EnrollDevice`).
- `DrillProof` is hosted-admissible, which reads as contradicting
  "drills run on the trusted lane only" — it's actually coherent (the
  recovery arm needs no device cert, so the drill is authored on
  un-enrolled trusted hardware and merely *accepted* by the hosted
  plane), but that reasoning must be stated or the next reviewer flags
  it as an owner-decision violation.

### V5. The classification fold's declassify arm is mathematically dead and drops the space floor

§11.5: `effective(c) = max(base(c) capped by d.new_class, d.new_class)`.
"Capped by" = min, and `max(min(base, nc), nc) ≡ nc` — the first arm is
dead code, so the formula reduces to `effective = d.new_class`,
**allowing declassification below `space_minimum`** (a control-set
floor that raise-side `base()` respects). Two defects, one fix:
`effective = max(space_minimum, d.new_class)` when a dominating
declassify exists. Also unpinned: **two authorized declassifies that
both dominate all raises** with different `new_class` — pick the
deterministic tie-break (recommend: causally-maximal wins; concurrent
maxima → the higher class, conservative). Add both as family-11
vectors.

### V6. `raise_class` is unbounded — classification-spam DoS

§11.1 grants `raise_class` to "any authorized reader-writer of the
claim's space," and §11.5 makes any raise defeat any non-descending
declassify. A hostile-but-enrolled session can raise everything to
`sensitive`, blocking every export flow (`class_floor ≤ flow ceiling`)
and forcing owner-ceremony declassifies item by item. Conservative
direction, but a real single-writer DoS on the export/journal pipeline.
Bound it structurally: a raise may not exceed **the actor's own grant
`class_ceiling`** (you cannot name a class you cannot hold) — hosted
and unattested classes then cap at their §7.6 ceilings. Add the §14 row
and a vector.

### V7. O6's "first operation MUST be `w.gen`" contradicts genesis and every gen-1 chain

As written, O6 applies to *every* generation including gen 1 — but the
control chain's first operation is `GenesisBundle` at seq 1 (§7.1), and
a fresh tenant lineage's gen 1 starting with a `w.gen` carrying
`last_known: "unknown"` is meaningless ceremony. Qualify: `w.gen` is
required as the first op of **generation g ≥ 2**; gen 1 chains open
directly on the genstart sentinel. (And state the control chain's
generation is fixed at 1 — `w.gen` never applies to `CTRL_LINEAGE`.)

### V8. T2 excludes the signer's *key*; renewal lets the same device self-receipt

Cert renewal (§4.2) changes the signing key while keeping `device_id`.
A device that authored an op under its old key can issue an
`AcceptanceReceipt` under its renewed key — same device, different
`issuer_key_id`, and T2's exclusion ("never the operation's signer")
no longer bites if read as key identity. Pin the exclusion at
**device_id** (resolve issuer key → cert → device; exclude the op
signer's device). The colluding-second-device residual stays in §14 as
already written.

## Pins (batch)

1. **E4 vs `max_generations` default**: E4 declares "no default values
   anywhere in this protocol"; §4.3 defines `max_generations` "absent ⇒
   8". Make the field required on write-capable grants (cleanest), or
   reword E4 to distinguish absent-semantics from encodable defaults
   (`grant_epoch_slack` absent = unbounded is fine as absence-semantics;
   an absent field that means a *number* is not).
2. **E8 self-inconsistency**: 1024 KEK wraps × ~150 B/wrap ≈ 150 KiB
   cannot fit the 64 KiB control-object cap. Lower the wrap cap (v1
   plane sizes are tens of devices) or raise the control cap; make the
   pair consistent and add the boundary vector.
3. **C3′ vs C2**: the recovery op necessarily duplicates a control
   sequence number with the branches it cuts (its `previous_writer_hash`
   presumably = `base.op`). State that a valid `RecoverySuccession` is
   exempt from C2 fork detection, and pin `previous_writer_hash =
   base.op` explicitly.
4. **§11.1 permission column hygiene**: the `retract` and `supersede`
   rows contain actor constraints ("author of target, or owner-class")
   where every other row names an edge permission. Name the permission
   per branch (author-retract under `memory.propose`/`assert`;
   owner-retract/supersede under `memory.curate`).
5. **`w.gen` needs an admission-table row** (edge permission = any write
   grant on the zone; charge = 1 op against the lineage budget — gen
   churn should cost budget).
6. **Frontier head retention**: heads are per `(lineage, gen)`; state
   when a superseded generation's head leaves the frontier (e.g., once
   gen g+1's `w.gen.last_known` references it, or at checkpoint) — else
   the frontier grows monotonically with generations.
7. **Audit records are shapeless**: §11.6 places read-audits in the
   genesis `audit` space as "system-actor operations," but `actor.kind`
   has no `system`, and no `m.*` op type is designated for audit
   entries. Name the op type and the writing actor (the daemon's own
   principal?), or defer the audit *writer* mechanics to D0-B
   explicitly (the space existing is enough for D0-A).
8. **`mimport` CDDL** says "+ mclaim fields" in a comment — merge the
   groups properly in the appendix.
9. **Cross-plane release verification input**: `m.import.claim` requires
   the destination to have "verified" the matching release — name the
   verification inputs for a *foreign* release (release op bytes +
   source-plane genesis descriptor + cert/grant material), even if their
   transport is D0-B.
10. **Genstart sentinel domain**: the O6 sentinel uses a bespoke
    `"intendant/genstart/v1"` prefix outside the §1 tag table — add it
    to the closed tag list (or compute it as `H_type` with a `genstart`
    tag) so the domain inventory stays complete.
11. **Vector additions riding the findings**: hosted owner-curation
    (V1), solo-plane bootability under the genesis-default policy +
    solo→multi transition (V2), declassify floor and two-declassify
    tie-break (V5), raise-cap (V6), gen-1 chain open (V7),
    renewed-key self-receipt rejection (V8), wrap-cap boundary (pin 2).

## Decision-record check (§15)

D-13 through D-27 are all reflected in the text they claim to govern,
with two caveats already covered above: **D-14 and D-17's hosted halves
are currently unsatisfiable via §11.4's owner-class derivation (V1)** —
the fix restores the rulings rather than revisits them — and D-11's
fallback lane needs the genesis default named (V2) to be real. The
D-1..D-12 carry-forward row correctly annotates the D-10 refinement
(random item nonce, deterministic wrapper nonce); no silent drift found
between the record and the normative text otherwise.

## What improved (credit where due)

- **Lineages (§9.3)** close the budget-evasion hole *by construction*
  and simultaneously solve browser-eviction recovery — the two-sided
  problem both reviews struggled to reconcile — in eight lines.
- **The edge/portable admission split (§10)** is cleaner than either
  review's proposal: portable admission uses only portable inputs, so
  the impossible-to-check components simply don't exist at fold level.
- **Ciphertext-only tenant logs with the single-record commit (0x11
  carrying core+wrap+sequencing)** kill both the plaintext-persistence
  defect and the allocate-vs-durable race in one shape.
- **Deterministic wrapper nonces (I2)** make rewrap replays
  byte-idempotent — which is what makes the §5.5 crash matrix tractable
  at all.
- **C3′'s ancestors∪op∪descendants cut** replaces v0.1's
  replica-dependent `max(observed)+1` with a rule computable from the
  operation's own bytes — the right kind of determinism.
- The **§7.3 bootability invariant as a required vector** is exactly the
  device that catches V1/V2-class regressions forever after; it already
  earns its keep in this review.
