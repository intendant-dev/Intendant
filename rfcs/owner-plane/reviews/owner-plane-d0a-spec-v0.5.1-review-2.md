# Review 2: D0-A Core + Memory specification v0.5.1

*2026-07-12. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.1 (2,200 lines), against the v0.5 synthesized review's seven
consolidated blocker areas and the new rulings D-56..D-68, the frozen
umbrella RFC (v3.1/D0), and the v0.5 archive
(`~/agenda-rfc-archive/2026-07-12-d0a-v0.5-as-reviewed.md`). Written
without reading `owner-plane-d0a-spec-v0.5-review.md`. Method: full
read; diff against the v0.5 archive (563 changed lines, ~80 hunks);
independent sweep greps (case-insensitive this time — it mattered);
walkthrough re-derivation of genesis, rotation-completeness, and
service-key resolution semantics.*

## Executive verdict

**v0.5.1 is one micro-patch away from freeze.** The seven
v0.5-synthesis blocker areas each received real byte-level mechanisms,
and — the change from every prior round — the fixes this time are
*complete state machines*, not added shapes: D-57's citing-position
anchor resolves receipt-policy identity with zero new receipt bytes
and unifies T2/T4; D-56 gives `deadline_fallback` an operative
issuance-time meaning while preserving D-12; the hosted ceiling is now
a coherent exhaustive list (exclusion-bound rotations, admissible
recovery succession, attested self-cutoff with the `cutoffreq` domain,
consumed request IDs); D-60/D-61 close the verb-class ambiguity and
the dead `admin` verb; D-62/D-68 pin frontier zone scope, chain
arithmetic, and the genesis cross-field lattice; D-63 makes principals
portable; D-64 makes audit physically recoverable; D-65/D-66/D-67
close transfer terminality, erase target mapping, and survivor
completeness. All nine of my v0.5 findings (Q1–Q9) and all twelve pins
are resolved or explicitly ruled.

**What remains is one blocking exactness gap, two high
divergence-inducing sentences, three mediums, and ten pins — every one
of them prose-level; none needs a new wire shape or a new owner
ruling beyond mechanical consequences of rulings already made.** That
is a first for this series, and it is the signal to stop reviewing
prose after the next patch: the residual defect class (a stale
sentence contradicting a new ruling elsewhere) is exactly what the
Gate-A prose↔vector discrepancy audit catches mechanically.

---

## Part 1 — Discharge audit of the v0.5 synthesized blockers

| # | v0.5 blocker | v0.5.1 disposition | Residue |
|---|---|---|---|
| 1 | Time posture, proof-policy identity, finality | D-56 (present deadline always binds; postures operative at issuance; witnessless lanes named and vector-pinned), D-57 (citing-position anchor — receipts need no policy bytes; pendings judged at eventual admission; never re-litigated), D-58 (service `receipt_cutoff`), shared issuer counter from 1, history cutoffs delimit authorship not proof, §9.1 hardening covers op position + every witness feed + `lease-missing` | **F3**: the descriptor "validity interval" sentences support a reading under which D-58 is dead weight |
| 2 | Hosted self-service, revocation, verb vocabulary | `ccutoff` requester group + `cutoffreq` tag; D-59 consumed request IDs + `request-fork`; §7.5(b) gains system-only `audit.write`; §7.5(c) admits exclusion-bound empty-manifest rotations (compound-first), recovery succession, and states `c.revoke_zones` unreachability; D-60 op-authoring/claim-authoring classes; D-61 `admin` reserved and rejecting; D-68 revocation exactness (complete grant set, rotation-after-last-wrap, any-order continuations, `"none"` sentinel, chain continues past pendency) | **F1**: "trusted: full verbs" now collides with D-61 and leaves the one unpinned genesis field |
| 3 | Frontier, writer arithmetic, genesis | D-62 zone-scoped Frontier (`zone_id` in hashed bytes) + zone-scoped `c.checkpoint`; D-68 chain arithmetic (seq from 1, +1, `g = max + 1`, per-axis outcomes); the genesis cross-field lattice (provenance-compatible class, epochs 1, exact spaces incl. the audit-read exclusion, wrap↔cert key equality) | F1 again (the ops list is the missing lattice edge) |
| 4 | Principal identity and physical audit | O8/D-63 closed `actor.id` minting (`body-invariant` on deviation); D-64 trigger + typed `auditprin` + `read_id`/chunk + one-Txn rule with the 4096-ID edge cap + diagnostic `at_ms` + hosted re-root remedy; `retired_via`/`retired_by`; assert restated as `P(claim) == P(judgment)` | **F4**: the "audit-space reads are themselves audited" sentence contradicts the D-64 trigger |
| 5 | Versioned bundle, expiry proof, terminal transfer | Versioned `bundle`/`bundlerec` (1:1 keyed to sources, claims only, floors at the stamped point); `control_frontier` added to the eval stamp; complete plane endpoints with import-header equality; the import is the deadline-qualified op (destination-zone receipt, source-zone policy); egress consumes at release; `export_id` plane-wide single-use; `XferAbort {reason, missing}` terminal | **F5**: the shapes live in §11.8, not Appendix A, though D-65's row says otherwise; egress receipt coordinates unpinned (pin) |
| 6 | Erase target mapping and survivor membership | D-66 `target_op` on manifest entries and tombstones, membership-checked, first-accepted-wins, cap 192 with cross-rotation drain; D-67 `fence_frontier` frozen in RewrapComplete, expected membership, destruction gated on membership equality, post-fence commits on the new epoch | **F2**: the expected-membership formula's "under the old epoch" admits a reading that re-opens the hole at the second rotation; **F6**: §6.1's 0x14 listing is stale |
| 7 | Conformance closure and drift | E7 keyed sets with the declared logical-key table; `draw_order` `{name, nbytes}`; `ctrl-fork`/`recovery-competition`/`request-fork` enumerated with dispositions; `c.cap_epoch_bump` body validation; the D-68 drift sweep (all seven named items verified applied) | one `Write-capable` leftover the sweep's grep missed (case) |

B.2/B.3 bytes and hashes are untouched and correct. The §15 record is
complete through D-68 with refinement annotations on D-52/D-53 —
though not on D-54 (pin).

---

## Part 2 — Findings

### Blocking

**F1. "Trusted: full verbs" is self-rejecting under D-61 and is the
one genesis field D-68 left unpinned.** The genesis row (§7.1) pins
the hosted ordinary grant's verbs exactly ("the §7.5 safe verb set")
but says only *"trusted: full verbs"* — while D-61 now makes **a v1
grant carrying `admin` reject at issuance** (`body-invariant`). The
literal reading (full = the 17-verb vocabulary) makes every
trusted-plane genesis reject itself: the §7.3 bootability walkthrough
fails at sequence 1, the exact defect class as v0.4's G1. The
charitable reading (full = vocabulary minus `admin`) still leaves the
exact ops list undetermined: does the ordinary grant carry
`audit.write` (inert off the audit space — the audit grant exists
precisely to hold it)? The v0.5 synthesis explicitly demanded "exact
ordinary/audit grant tenant, zone, lineage, spaces **and verbs**";
D-68 pinned everything except the trusted verbs. A family-7 fixture
would have to invent the list — the Gate-A disqualifier. Fix (one
line): pin the trusted genesis ordinary grant's `ops` = **every verb
except `admin` and `audit.write`** (15 verbs — `audit.write` rides the
audit grant; `admin` is D-61-reserved; `assert` being inert in a
personal-only space is fine — verbs grant capability, spaces scope
it).

### High

**F2. The survivor expected-membership formula re-opens D-67's hole at
the second rotation under one natural reading.** §5.5 state 4:
*"expected set = every item **committed at or before fence_frontier
under the old epoch** MINUS the rotation's manifest item_addrs."*
"Committed under the old epoch" reads as commit-time epoch. At
rotation e→e+1, items committed under epochs < e (rewrapped at earlier
rotations; current wrapper = e) are then **outside the expected set**,
so their e+1 rewraps are never completeness-checked, and KEK-e
destruction can proceed while they are missing — silent data loss for
any item older than one epoch, which is precisely the
"self-consistent but incomplete" failure D-67 exists to close. The
correct set is wrapper-current, not commit-time: fix by rewording to
*"every item committed at or before `fence_frontier` **holding a
current old-epoch wrapper** (equivalently: every non-tombstoned item
at or before the fence) minus the manifest's `item_addrs`"* — which
also makes the exclusion of previously-tombstoned items explicit — and
add a **multi-epoch family-13 vector** (a third rotation whose
expected set must include epoch-1-committed items).

**F3. The service-descriptor "validity interval" sentences support a
reading under which D-58 is dead weight.** §7.1: *"a descriptor's
validity runs from its acceptance **to the next accepted descriptor**
…, selected at the citing operation's position"*; T2: *"**the**
service-key descriptor — in force at the citing operation's admission
position."* Reading (a): only the position-current descriptor
validates → any routine `c.service_key` rotation silently disqualifies
the predecessor's receipts for **all still-pending and future-citing
operations** (a proof-history purge by rotation), and `receipt_cutoff`
becomes pointless — its only marginal effect would be re-litigating
admitted operations, which T2 itself forbids. Reading (b), the only
one under which D-58 does anything: `issuer.key_id` resolves against
**every descriptor accepted at or before the citing operation's
admission position**; a successor descriptor never disqualifies its
predecessor — only an explicit `receipt_cutoff` does. Fix: rewrite
both sentences to state (b), and add family-9 vectors (old-key
receipt qualifying post-rotation without a cutoff; disqualified beyond
one with it).

### Medium

**F4. "Audit-space reads are themselves audited" contradicts the D-64
trigger in the same row.** The trigger is sensitive-minimum space in
scope ∨ effective-sensitive item in results; the audit space is
private-minimum, and `maudit` rows are not claims, so they carry no
effective classification — at genesis the trigger **never fires** on
an audit-space read, yet the row asserts it does. An implementer may
add it as an undeclared third trigger branch; another won't; family
11/12 vectors then disagree. Fix: reword ("audit-space reads are
audited **when the trigger fires** — e.g. after the owner raises the
audit space's `class_minimum` — and recursion terminates because…")
or add the branch deliberately; also state that only claims carry
effective classification for trigger branch 2.

**F5. `bundle`/`bundlerec` are not in Appendix A, though D-65's row
says they are.** The shapes are fully closed in §11.8 (versioned,
keyed, sorted — no semantic gap), but Appendix A claims to hold *every*
shape, the bundle is a hashed object (`H_bundle`), and the decision
record now states a falsehood about the artifact. Move the two rules
into A.5 (or amend the D-65 row; moving is better).

**F6. §6.1's `0x14 RewrapDone` inline listing lacks
`fence_frontier`** — the pre-D-67 shape survives there while §5.5 and
the A.4 `rewrapdone` rule carry the field. Stale mirror; align.

### Pins

1. **Header provenance is stale**: "Folds the v0.4 synthesized review"
   should name the v0.5 synthesis this cut actually folds; the
   archived-drafts list omits v0.5 (the archive file exists).
2. **D-54's row lacks its refinement annotation**: it still says
   "`deadline_fallback` scoped to deadline-bearing items," which D-56
   reversed (present deadlines always bind; the fallback governs
   issuance). D-52/D-53 got "refined by" notes; give D-54 one.
3. **§9.4 "Write-capable grants"** → "op-authoring grants" — the last
   D-60 leftover (capitalized, so the sweep's grep missed it).
4. **`c.cutoff` "own lineage" check**: state it portably —
   `requester.device_cert` resolves to a cert whose `device_id` equals
   the target lineage's `lineagedef.device_id`, and the attestation
   verifies under that cert's key.
5. **Egress release deadline receipt coordinates**: pin `zone_id` =
   the source zone and `subject` = the release operation's own
   `item_addr` (imports got exact coordinates; egress didn't).
6. **Hosted enroll compounds**: may `grants[]` name an existing
   device? This decides whether any hosted device can ever read the
   audit space before re-root; state it either way (either answer is
   consistent with D-64's re-root posture).
7. **Trusted audit-read path**: one explicit clause that the owner
   reads the audit space via a later `c.grant` (D-68's exclusion
   sentence implies it; say it).
8. **O8 residual restated**: two humans sharing one enrolled device
   share `P` (same device-hex `actor.id`) — the O4 residual surfacing
   at the principal layer; one sentence.
9. **`fail-closed` + `require_cert_deadlines = false`**: legal
   combination? If yes, state its meaning (deadlines required on new
   grants only).
10. **`auditprin` shape 4**: pin what `peer: text` carries for a
    low-trust *session* (the shape covers both; the field name fits
    only one).

---

## Part 3 — Gate-A readiness

Against the v0.5 synthesis's own go-ahead criteria: deadlines/budgets/
witnessless lanes now have one explicit authority rule (D-56);
receipts and leases bind proof history without new bytes (D-57/D-58,
modulo F3's sentence); hosted self-service and revocation are
enforceable from closed bodies (modulo F1's ops list); Frontier,
checkpoint, arithmetic, and genesis have one zone/epoch meaning;
actor identity and audited reads are portable and physically
recoverable (modulo F4's stray sentence); bundles, transfers, and
erase/survivor sets have versioned complete identities (modulo F2's
formula wording and F5's location). The core, corpus, and harness
still do not exist — Gate A remains mechanically impossible today,
as expected.

**Recommendation.** Cut **v0.5.2 as a same-day micro-patch**: F1 is
one line, F2/F3 are one paragraph each plus three named vectors,
F4–F6 and the pins are sentence-level. No new wire shapes, no new
signature domains, and no owner decision beyond ratifying F1's
15-verb list as the mechanical consequence of D-61 + the audit-grant
split (and F2/F3's rewordings as the only readings under which D-67
and D-58 do anything). Then **stop reviewing prose**: this round's
entire residue is stale-sentence drift against newer rulings — the
defect class the prose↔vector discrepancy audit finds mechanically —
so the next artifacts should be `owner-plane-core`, the corpus, and
the offline family-14 fixture, with the discrepancy audit as the
final gate. Durable P1 writes stay prohibited until Gate B plus the
umbrella's P0.5/tombed-cutover prerequisites, unchanged.
