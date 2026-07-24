# D0-A v0.5 — remaining work (pass D handoff)

> **CONSUMED 2026-07-12.** Pass D (items 1–20 below) is fully applied to
> `~/owner-plane-d0a-spec.md`; the final sweep ran green (1,975 lines,
> §15 through D-55, §13.1 re-parses, pinned hashes intact). Item 7's
> check found O4's class list still excluded daemon — fixed per D-47.
> This file is retained as the pass-D record only.

*2026-07-12. State: v0.4 archived (`~/agenda-rfc-archive/2026-07-12-d0a-v0.4-as-reviewed.md`).
v0.5 passes A (boot authority), B (proof authority), C (control
authority) are APPLIED to `~/owner-plane-d0a-spec.md` via assertive
scripted replaces (each anchor count==1, atomic aborts). Header already
says v0.5. **Passes A–C cite rulings D-48..D-54 in the body, but the
§15 decision-record rows for D-48..D-55 are NOT yet written — pass D
adds them.** All seven owner rulings below were approved verbatim
("Go ahead with v0.5 using your recommendations"). The v0.4 synthesized
review (`~/owner-plane-d0a-spec-v0.4-synthesized-review.md`) is the
source; it also independently CONFIRMED the B.2/B.3 policy hashes
(workflow-v1 219b9bac…/1133 B, owner-v1 d7d5559a…/571 B) — do not
reorder those.*

## Pass D — Memory authority (§11)

1. **`authorized(j)` consumes relation** (§11.2 — anchor: "`authorized(j)` =
   admission passed"): tuple becomes (verdict, target.kind, space_class,
   actor_class, **relation satisfied**). Define the portable
   **authoring principal** `P(op) = (writer.lineage, actor.kind,
   actor.id)` from signed fields. `relation self` = full P equality
   (session actors carry the session id in actor.id, so same-session is
   automatic). `relation author` = P equality **∨ (same lineage ∧ the
   judging actor has direct-human evidence)** — D-51: principal-level;
   device-level was rejected (it would let one session retract
   another's work on a shared controller). Add: shared device custody
   never silently becomes shared authorship (vector).
2. **Author verbs** (§11.1 rows): author retract requires ANY write verb
   ({propose, assert}) — the current "propose (author)" excludes
   assert-only authors; author supersession likewise any write verb
   (workflow-v1 row 9 counts session/author in workflow).
3. **Assert halves portable**: the claim half admits alone as an
   ordinary propose; the judgment half admits iff its claim is present
   (causal). Reconcile with §6.1 Txn all-or-nothing: that is a LOCAL
   storage-commit rule; replicas receive ops individually — both hold.
4. **D-40 diary compound**: hosted personal propose + safe-accept uses
   the same linkage as assert (judgment request_id =
   assert_req(claim.request_id)), one Txn, idempotent completion.
5. **`mclaim.supersedes[]`**: advisory — views MAY render lineage
   links; zero fold effect (one sentence).
6. **Retract vs retire projection field**: derived views carry
   `retired_by: "author" | "curator"`.
7. Verify O4/§10.1/§11.4 daemon-human consistency (believed already
   aligned by pass A of v0.4; just check).

## Pass D — derived state (§11.5, §11.8, §5.4/5.5, CDDL)

8. **locator_hash single domain**: DELETE the older sentence
   "`locator_hash = SHA-256(canonical locator string)` binds the
   resolver input" (§11.5 ~line 1149); keep
   `locator_hash = H_evrec(canonical locator text)`. Pin: external
   `digest` = SHA-256 of the referenced content bytes.
9. **Depth-1 truncation**: verified plane-evidence floor =
   `effective_shallow(source)` = effective computed WITHOUT the
   source's own evidence term (max of its space minimum, import floor,
   declared/mutable) — else taint recurses. Name and pin the formula.
10. **Cross-plane evidence refs** (plane_id present): always
    unresolved → `sensitive` until D0-B transport exists.
11. **Record-level transfer (D-53)**: replay key = `(from_plane,
    export_id, source_op)`. `pendingxfer` gains `content_digest` +
    `record_count`; `xferdone` gains `completed: [* bytes32]` (sorted
    source_ops, set). One XferDone when ALL intended records imported;
    recovery re-runs missing imports then writes it. Bundle bytes are
    RE-DERIVED deterministically from live sources (redaction is
    deterministic); a source erased mid-transfer fails the remaining
    imports closed. Pin: `mimport.class_floor` == bound
    bundlerec.class_floor (equality); import op's header zone/space
    must equal release.to for plane endpoints; `mexportrel` gains
    `{data_frontier: bytes32, as_of_ms}` (the release's classification
    evaluation point); flow expiry evidence uses the SOURCE zone's
    witness policy; egress endpoints (model/embedding/reflection) get
    NO PendingXfer — they complete at release acceptance.
12. **Typed erase manifest**: `ckekrotate.erase_manifest` becomes
    `[* { item_addr: bytes32, erase_op: bytes32 }]` (set, sorted by
    item_addr) — recovery can now derive tombstones ({item_addr,
    erase_op, retired_epoch = new_epoch − 1}). Duplicates across
    rotations = idempotent skip; entries must reference accepted
    m.erase_request ops. Update §5.4/§5.5 + E8 cap row wording.
13. **Survivor pair CDDL** (A.4): `survivorpair = { item_addr: bytes32,
    wrap_hash: bytes32 }`; H_survivors over the canonical sorted array.
14. **`itemwrap.wrapped_dek`**: constrain `.size 48` (A.4).

## Pass D — conformance (§13) + record (§15)

15. **§13.1**: add required `draw_order: [text]` (ordered array) when
    `rng` present — JSON object property order is not portable.
    Outcome/disposition stay strings in the schema DELIBERATELY; the
    harness cross-validates against §10.4/§10.5 (derive-don't-mirror —
    no duplicated enum to drift). One sentence.
16. **`recovery_pk`**: constrain `bstr .size 32` in authproof CDDL.
17. **Set annotations**: sweep `; set (E7)` onto cgenesis/cenroll
    grants+wraps, ckekrotate wraps (revocation arrays done in pass C).
18. **Family additions** (§13.3): 10 = w.gen space-scope-exception;
    12/13 = audit-lock-loser + audit-budget-exhaustion; 9 =
    service-key descriptor resolution + rotation; 11 =
    relation-principal vectors (self/author, cross-session deny on one
    device) + record-level transfer + typed-erase recovery.
19. **§15 rows D-48..D-55** (all "Ratified, owner, 2026-07-12"):
    - D-48 service-key descriptors via `c.service_key`; Connect
      qualifies iff `"connect" ∈ time_witnesses` (accept_connect_time
      REMOVED); receipt qualification binds non-retroactively to the
      policy at the receipt's admission position.
    - D-49 renewal preserves only operations admitted at the
      pre-renewal frontier; pendings never ratified.
    - D-50 early exclusion accepted (monotone-safe); invariants:
      target-excluding wraps + all-decryptable-zone coverage; refs cap
      64 + `c.revoke_zones` continuation.
    - D-51 author relation principal-level (P = lineage, actor.kind,
      actor.id; self = equality; author = equality ∨ same-lineage
      direct-human); device-level rejected.
    - D-52 audit fail-closed via `audit-unavailable`; lock losers
      cannot serve sensitive reads; >256 IDs chunk; results release
      after audit durable; genesis budgets pinned 1e6 ops / 256 MiB.
    - D-53 record-level transfer identity + completed-set XferDone.
    - D-54 hosted self-`c.cutoff` (requester-attested, own lineage)
      in the ceiling; `deadline_fallback` scoped to deadline-bearing
      items, grandfathered lane = named tested residual.
    - D-55 mechanical batch: `audit.write` system-only verb; w.gen
      space/kind-axis bypass; capability epoch 1 at genesis/zone
      creation; zone-qualified cutoffs (zonecutoff); single-use
      requester attestation (request_id + ctrl_frontier bound); one
      locator-hash domain (H_evrec); typed erase manifest + survivor
      pairs; wrapped_dek 48 B; E4 stale-statement fix; `c.drill`
      portable admission (recovery-arm only); fresh-signing-key
      renewal; `c.checkpoint` proof_cutoffs; rotation_refs cap 64;
      draw_order array; O7 control-header pins.
20. **Final sweep**: grep `accept_connect_time` (must be 0 hits),
    `SHA-256(canonical locator` (0), `[* bytes32] }   ; op hashes`
    (erase manifest replaced), dangling `§11.9` (must be gone — O5 was
    fixed in pass A), `D-48|D-49|…` citations all resolve to §15 rows;
    re-validate §13.1 JSON parses; wc; then memory update + report.

## After v0.5

Next deliverables (review-sanctioned): `owner-plane-core` scaffolding
(encoding/crypto/storage lanes — may start in parallel; reducer
fixtures must NOT invent answers), the vector corpus per §13, the
offline confirmation fixture, then the prose↔vector discrepancy audit
that decides Gate A. Durable P1 writes stay prohibited until Gate B +
umbrella P0.5/tombed-cutover prerequisites.
