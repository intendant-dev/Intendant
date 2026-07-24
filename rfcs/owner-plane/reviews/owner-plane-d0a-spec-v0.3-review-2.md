# Review 2: D0-A Core + Memory Specification v0.3 (freeze candidate)

*2026-07-12. Independent conformance audit of `~/owner-plane-d0a-spec.md`
v0.3 against the frozen umbrella (v3.1/D0), the v0.2 synthesized review's
seven blockers and Gate-A checklist, the §15 decision record (D-1…D-38),
and my v0.2 findings (V1–V8). Method: same as the last round — walk the
spec's own invariants (§7.3 bootability, D-14/D-29 hosted-diary intent,
§16 Gate-A checklist) through every other rule. Finding IDs are F1… .*

## Verdict

v0.3 is the document the synthesis asked for: self-contained, closed
CDDL including every control body, an operation registry with
dispositions, signed time authority with an honest solo posture, and a
clean affirmative decision record. All seven synthesis blockers are
substantively discharged; every one of my v0.2 findings (V1–V8) and pins
landed, most exactly as recommended and some better (the lease-window
binding in T5, framing v2's SYNC + ambiguity quarantine).

It is **one targeted patch round short of freeze-ready**. The remaining
blockers are exactly where the synthesis predicted the last mile would
be — the pinned constants and their coherence with the machinery built
to consume them. The headline: **the built-in policy tables (Appendix
B.2/B.3) do not yet grant the hosted safe-human what D-29 built the
safe verbs to deliver**, so the flagship hosted diary still cannot
accept or retire its own entries — V1's bug class, one level down, now
confined to two appendix tables and one unpinned genesis constant. No
architecture issue found; nothing touches the umbrella or requires a
new owner ruling beyond one clarification (F1's space-class scope).

## Resolution audit

**Synthesis blockers:** 1 (closed schemas — discharged; Appendix A
covers all control bodies, KekWrap, spacedef with `space_class`,
ZonePolicy; no fixture-defined shapes remain). 2 (signed non-circular
time + solo viability — discharged; D-28, B.1, T4's control-frontier
validation replacing the circular at-`seen_ms` rule, T5's
non-backdateable window). 3 (hosted human authority without ceiling
widening — machinery discharged via D-29 safe verbs + `safe-human`
actor class; **counting tables lag it — F1**). 4 (writer/capability/
KEK/recovery/frontier transitions — discharged; O6 gen-1 fix, D-32
one-zone grants, D-33 retirement, D-34 audit-only kek epoch, C3′
placement + precedence exception; the rotate-rewraps-everything
invariant makes WrapAdd-only joins sound). 5 (item wrapping/atomic
commits/corruption-safe storage — discharged; I2 wrapper-mismatch,
survivor digest before old-KEK destruction, 0x18 Batch for assert,
PendingXfer replacing the false one-commit claim, framing v2 per D-35).
6 (executable Memory — discharged in the registry, evidence,
classification fold, export bundle; **modulo F1/F3**). 7 (dispositions +
vector surfaces — discharged; §10.5 map, JSON-Schema vector container,
realistic surface matrix with named browsers and fleet CI lanes).

**My v0.2 findings:** V1 → D-29 machinery (see F1 residual). V2 → D-28 +
B.1, fully closed. V3 → Appendix A complete. V4 → D-30 hosted
`c.lineage_reauth`, compounds-only grant changes kept per D-15, drill
clarified. V5 → §11.6 floor-preserving fold, exact. V6 → D-31
can_raise + quota. V7 → O6 fixed. V8 → T2 by device_id + family-9
vector. All eleven v0.2 pins landed (E4 defaults purged, E8 wrap math
shown, C3′ placement, permission cells, w.gen row, drill sentence,
frontier retirement, audit space, mimport CDDL, interim cross-plane
posture stated, genstart/assertreq in the tag inventory).

**Decision record:** D-4/D-10/D-22 confusion corrected; D-24…D-27
explicitly owner-ratified; D-28…D-38 all match the text they claim to
govern. No silent drift found.

## Blocking findings (freeze-stoppers)

### F1. Appendix B.2/B.3 don't count what the D-29 safe verbs admit — the hosted diary still can't curate itself

The safe-verb machinery (§7.5(b), §11.1, §11.4) exists so a hosted
human can operate their diary without owner-class. The pinned policy
tables don't follow through:

- **B.2 counts safe-human `accept` only in `workflow` spaces**
  ("accept — observation/episode × workflow × {session(self), owner,
  safe-human}"). A hosted owner's diary entries live in `home` —
  presumably `personal`-class (F2) — where no row counts a safe-human
  accept. Result: every hosted diary entry stays `candidate`; pins
  (which require accepted status) never fire; auto-context stays empty.
  Exactly V1's product outcome, now caused by the policy table instead
  of the actor derivation.
- **§11.1's `judge.safe` row admits safe retire, but no B.2 row counts
  it** ("retire/supersede — all × all × {owner}"). A hosted human
  cannot retire their own diary entries — admissible, recorded, and
  status-inert.
- **§11.2 prose vs tables, twice**: the prose says dispute counts for
  hosted safe-humans "on their own plane's workflow/personal spaces
  (**both built-ins**)"; B.2's dispute row is all×all×{owner,
  safe-human} (broader than the prose's space scope), and **B.3
  (`owner-v1`) has no safe-human dispute at all** — "both built-ins" is
  false as written.

Fix (one patch): add safe-human rows to B.2 for
accept/retire — `observation/episode × {workflow, personal}` — align
the dispute row's space scope with whichever the owner intends (needs
a one-line ruling: prose's workflow/personal, or table's all), and
either add the safe-human dispute row to B.3 or correct §11.2's "both
built-ins." Then make family 11's "safe-human accept on hosted plane"
vector target the **genesis `home` space specifically** — as currently
worded a workflow-space vector would pass while the diary case stays
broken.

### F2. The genesis space definitions are not pinned — the constant everything above hinges on

`c.genesis` creates `home` and `audit` spaces (§7.1), and Appendix B.1
pins the genesis ZonePolicy — but **not the two spacedefs**. `home`'s
`space_class` and `status_policy` decide whether F1's fix works, whether
`assert` is admissible there (assert requires workflow-class), and what
the bootability walkthrough actually exercises; `audit`'s decide who can
judge audit rows. Pin both in B.1 (recommend: `home` = `personal` ×
`workflow-v1`, `class_minimum: public`; `audit` = `audit` × `owner-v1`),
and state the genesis grant's verb set (it must include the safe verbs
on a hosted plane for D-29 to hold from the first minute).

### F3. The policy-object schema cannot express the rules the built-ins rely on

B.2's accept row is annotated "session (**self**, same actor+session)"
and its retract row "(+ the **author rule** in §11.1)" — but Appendix
A.6's rule shape is `(verdict, kinds, space_classes, actor_classes)`
with **no relational component**. "Self" and "author-of-target" are
relations between the judgment's actor and the target claim, not actor
classes; the pinned canonical policy bytes therefore *cannot encode two
of the rules the tables claim they contain*. Family 11 pins policy
bytes, so this becomes unfixable-without-version-break the moment
vectors freeze. Two clean options: add `? relation: "self" / "any"`
to the A.6 rule shape (then B.2's annotations become encodable rows),
or normatively move self/author rules into the reducer (§11.2) and
state that policy objects carry class-level counting only. Pick one
before any vector is generated.

## High

### F4. `require_cert_deadlines` has no defined semantics

B.1's recommended multi-device policy sets `require_cert_deadlines:
true`; no rule anywhere says what a validator does when it is true and
a cited certificate lacks `expiry_deadline_ms`. Define the admission
outcome (a new `cert-deadline-required` → quarantine? reject?) and
whether it applies at enrollment (new certs must carry deadlines) or at
op admission (ops citing deadline-less certs fail) — they differ for
certs enrolled before the policy flipped.

### F5. `LeaseStmt.ctrl_frontier` is dead weight — T5 never validates it

The field exists (its v0.2 purpose: prove the lease issuer's revocation
knowledge is fresh — the umbrella's high-impact freshness predicate),
but v0.3's T5 binds only the time window. Either define the check (the
named frontier must cover the grant's zone's control state at some
bound relative to the fold's frontier) or mark the field
diagnostic-only for v1 explicitly — an unvalidated field in a signed
authorization object invites implementers to assume it does something.

### F6. The audit writer's grant and the hosted audit gap

§11.1's `m.audit` row says the daemon writes "under a genesis-issued
audit grant," but `cgenesis` carries exactly **one** grant (the first
device's). Either `cgenesis.grant` becomes `grants: [+ grant]` with the
audit/service grant pinned in B.1, or audit grants are issued
post-genesis (then "genesis-issued" is wrong). And on a hosted plane
there is no daemon and no `service` writer at all — sensitive-space
reads (which D-14 explicitly permits storing) generate **no audit
rows**. State the consequence honestly (hosted planes gain read
auditing with their first daemon) or assign the duty to the reading
device.

## Pins (batch before freeze)

1. **Un-capped arrays**: `maudit.result_ids`, `merasereq.targets`,
   `ckekrotate.erase_manifest`, `crevokedev.revoke_grants`,
   `crecovsucc.tenant_cutoffs`, export bundle length. Add caps or one
   blanket rule ("arrays without named caps are bounded by the
   enclosing object cap") — E8 is deterministic parsing, so say it.
2. **`crevokedev` can exceed the 64 KiB control cap**: `rotations:
   [* ckekrotate]` × N zones × 256 wraps each busts the cap for a
   device enrolled in several populous zones — making the
   security-critical operation (revocation) unencodable exactly when
   it matters. Allow rotations by reference (hashes of
   separately-committed `c.kek_rotate` ops, with the compound's
   atomicity semantics stated) or size the caps for the worst case.
3. **Shape-1 vs O4 human-evidence classes**: §10.1 row 1 says "human
   owner on browser/native/**daemon**"; O4 says human presence is
   admitted by "browser/native/mobile classes." Reconcile (recommend:
   daemon excluded from human evidence; a human at the daemon's
   dashboard is `owner-browser`).
4. **`grant.kinds` absence semantics unstated** — E4 demands the
   absence meaning at the field definition (presumably "all kinds");
   say it.
5. **`mimport` redundancy**: top-level `{export_id, from_plane,
   content_digest, class_floor}` duplicate `provenance.import` — require
   equality as a body invariant or drop one copy.
6. **`connect_service_key` required iff `accept_connect_time`** — add
   the CDDL comment like the other conditional-required fields.
7. **B.1 "exact object" isn't** — `zone_id` varies per plane, so the
   "hash pinned by family 7" can't be a constant. Reword: pinned
   template; the vector pins field values with the plane's zone_id
   substituted.
8. **PendingXfer clearing condition**: §6.1 says it "clears with a
   later OutboxMark" — the actual condition is the destination log
   containing the matching `m.import.claim`. State that; the OutboxMark
   phrasing will mislead implementers.
9. **`w.gen` header `space_id`**: chains are per (zone, lineage, gen);
   the header requires a space. Pin the value (any space in grant
   scope, or a designated one) so canonical vectors are unambiguous.
10. **Judgments/pins whose target was erased**: after rotation the
    target op is unreadable; state the fold's treatment (suggest:
    judgment folds inert; views show the tombstone) so replicas agree.
11. **Hosted planes can never change zone policy** (`c.zone_policy` is
    not §7.5(c)-admissible) — deliberate ceiling consequence, but say
    it in §7.5 like the erasure sentence (a hosted plane stays on the
    solo budgets posture even with several browsers enrolled).
12. **Author-supersede is admissible but never counts** under B.2
    (supersede — owner only): either add the workflow/self row or note
    recorded-vs-counting explicitly, as done for session disputes.
13. **One live lineage per device, or plural cutoffs**: `crevokedev`
    cuts off exactly one lineage; nothing forbids a device accumulating
    several. Pin the invariant (recommended) or make `cutoff` an array.
14. **`recovery_pk` size**: constrain to `.size 32` in CDDL (§2.2
    already fixes Ed25519); cheap parity with the other key fields.

## Umbrella conformance spot-check

All §15.1 D0-A duties discharged: capability-epoch scope (§9.4, D-32),
marker file (N2), provenance evidence matrix (N3 + §7.6 + opaque
evidence docs), hosted-to-trusted ceremony + ceiling (recovery-arm
re-root; §7.5 as reducer invariant), item AEAD/DEK/frontier/receipt
types, typed fail-closed IAM, Memory reducer with policy selection,
flow admission (§11.8 + typed endpoints incl. egress `profile_hash`),
deterministic expiry (§9.1 + D-28's honest solo posture), migration
invariant (M1). Gate B correctly names P0.5 and the tombed-Memory
cutover as additional P1 prerequisites (matching the umbrella's
Appendix A duty).

## What improved (credit)

- **D-28 is the honest solo posture** the synthesis asked for — "a solo
  plane cannot possess independent time evidence" stated as a defined
  authority lane, with the genesis policy pinned and upgrades
  explicit-only.
- **T4's fix of the circular receipt validation** (issuer cert checked
  against the fold's control frontier, not "state at seen_ms") and
  **T5's window binding** (op accepted only with a qualified receipt
  *inside* the lease window; `created_hlc` inert) are exactly right.
- **Framing v2** — CRC covering `len`, SYNC resync markers, and the
  torn-tail vs ambiguous-final-frame distinction with quarantine — is
  a real storage-engineering answer, not a patch.
- **The safe-human/owner split** (D-29) is the correct authority shape
  for the hosted lane; F1 is a table lagging the design, not a design
  fault.
- **The registry format** (§7.1/§11.1: body, arm/verb, invariants,
  transition, charge, replay, disposition per row) is the piece that
  makes the eventual prose↔vector discrepancy audit mechanical.
- **Decision-record hygiene**: the D-4/D-10/D-22 correction and the
  explicit re-ratification of D-24…D-27 close the synthesis's process
  complaint precisely.

## Bottom line

One more patch: fix the two appendix tables and pin the genesis
spacedefs (F1/F2), resolve the policy-schema relation gap (F3) before
any vector bytes exist, define the two dangling semantics (F4/F5),
settle the audit grant (F6), and sweep the pin list. Then implement the
vector corpus and run the discrepancy audit. On this trajectory the
next review should be that audit, not another document round.
