# Review 2: D0-A Core + Memory Normative Specification (v0.1)

*2026-07-11. Independent second review of `~/owner-plane-d0a-spec.md` v0.1
against the frozen umbrella RFC (`~/agenda-owner-plane-rfc.md`, v3.1/D0)
and the codebase claims underlying both (main @ `b8a51150`). The other
review (`~/owner-plane-d0a-spec-review.md`) was not read past its verdict
paragraph, discovered only when saving this file — findings here are
independent. Finding IDs are D1… to avoid colliding with the earlier
RFC-review F-numbers.*

## Verdict

Strong draft — the right objects, honest adversary boundaries, and several
genuinely elegant mechanisms (the signed `authored_crypto_epoch` /
mutable `key_wrap_epoch` split; low-S as anti-fork-framing; re-root
through the recovery arm so a hosted root cannot self-succeed; the
receipt-based acceptance deadline as a genuinely deterministic answer to
untrusted time). Umbrella conformance is very good: every decision-11/12
erratum I checked is discharged faithfully.

It is not freezable yet. Five gaps are blocking — three are "the
vocabulary doesn't cover the product's own basic flows" (a hosted-genesis
plane and even a trusted plane cannot actually be bootstrapped and
operated end-to-end with the ops as listed), one is an
umbrella-assigned D0-A duty that went undischarged (capability-epoch
scope), and one is a security hole in the budget/writer model. All five
are repairable within the spec's existing shapes; none re-opens the
umbrella.

## Blocking findings

### D1. The control-op vocabulary cannot bootstrap or operate a plane

Walking genesis → first write → second device → revocation exposes
missing ops and missing fields:

- **No zone exists at genesis.** `GenesisBundle = {descriptor,
  first_certs, first_grants}` carries no zones, and the umbrella (§5)
  mandates exactly one default `private` zone. Nothing creates it: on the
  hosted lane §7.3(c) even forbids `ZoneCreate`. A fresh plane has
  nowhere to write.
- **KEK epoch 1 is never distributed.** §5.2 says epoch 1 exists "at zone
  creation," but `ZoneCreate { zone_id, name_hash }` carries no recipient
  wraps — only `KekRotation` has a `wraps` field. No device can ever
  decrypt epoch 1.
- **New devices never receive the current KEK.** `EnrollDevice { cert }`
  carries no wraps, and there is no `KekWrapAdd`-style op. As specified,
  the only way to key a newly enrolled device is a full `KekRotation` —
  which per §5.4 implies rewrapping every item in the zone, O(zone) work
  per enrollment. Either add a current-epoch wrap-add op (cheap join) or
  state rotation-on-join as the deliberate cost model.
- **No `SpaceCreate`.** §3's identifier table says spaces are "minted by
  control ops," but no control op mints one. Spaces also need a home for
  their designation (§11.4's policy rules match on `space_class` /
  "designated workflow spaces" — declared where?).
- **`ZonePolicy` is uninstallable.** X5 says the strict/lenient policy
  object is "installed by a control op"; no listed op installs or updates
  it (`accept_connect_time` — X2 — rides the same object).

Fix shape: `GenesisBundle` gains `first_zones` (each with initial KEK
wraps to the first certs); `ZoneCreate` gains `wraps`; add `SpaceCreate`
(with class/designation) and `SetZonePolicy`; decide join-time key
delivery (wrap-add op vs rotation-on-join). Add vectors: genesis →
write → enroll → second-device-reads-history; zone-policy install.

### D2. The hosted ceiling §7.3(c) bricks the lane it exists to permit

(c) enumerates the admissible control ops on a pre-re-root hosted plane
as *exactly* `EnrollDevice` (hosted-browser), `RevokeDevice`, "and the
tenant data ops those grants allow". That excludes:

- **`IssueGrant`** — directly contradicting (b), which regulates what
  hosted grants may contain, and the umbrella's "low-effect read/write
  capabilities otherwise" (§3.5). Enrolling a second browser
  (explicitly needed for P2a second-browser restore) yields a cert that
  can never be granted anything: it cannot write.
- **`ZoneCreate`/zone bootstrap** — combined with D1, a hosted plane has
  no writable zone at all; the flagship zero-install diary cannot store
  one entry.
- **`KekRotation`** — the umbrella defines revocation *as* epoch rotation
  (§5, §4.5). A hosted plane can `RevokeDevice` but never rotate, so a
  revoked hosted browser keeps receiving readable content forever.
- **Hygiene ops** — `CutoffSet`, `CheckpointCommit`, `EraseAuthorize`:
  without them a hosted plane can never quarantine a revoked writer's
  tail, never compact, and never erase a diary item — erase being a
  headline product property (§4.7).

Recommendation: invert (c) into an **exclusion list** derived from the
threat (what hosted code must not be able to do): enroll any class other
than `hosted-browser`; issue grants above the (b) constraints;
provenance elevation; effect approval; succession (already structurally
recovery-only). Everything else — grants within (b), zone/space
lifecycle, rotation, cutoffs, checkpoints, erase — stays admissible.
Extend vector family 7 accordingly (hosted plane: full
genesis→write→enroll→revoke→rotate→erase walk succeeds; each excluded op
rejects).

**Ratification flag riding the same section:** (b) caps hosted grants at
`class_ceiling ≤ internal`. For the hosted-genesis *diary* persona this
means the owner cannot write (or read back) their own `private`-class
entries from the only device they have — the class ladder's natural
label for personal diary content. If the intent is "don't put secrets in
the degraded lane," that is a product decision the owner should ratify
explicitly (§15 list); if not, the ceiling's write/read cap should move
to `private` while keeping flows/exports/effect-approvals and
`sensitive` trusted-lane-only. The umbrella's ratified sentence ("notes,
tasks, and reminder drafts") speaks to *effects*, not storage class.

### D3. Memory operation→IAM mapping is missing; judgment authority is unscoped

§11.1 lists the IAM operations and §11.2 the wire op types, but nothing
maps them. Concretely undefined:

- Which IAM operation authorizes emitting `m.judge` with each verdict?
  `dispute`, `retract`, `retire`, `supersede`, `declassify` are all
  unmapped (only `assert`'s claim+self-accept and pins-via-`curate` are
  inferable, the latter from the umbrella rather than this spec).
- **Dispute rights are the sharp edge**: fold rule 3 flips any accepted
  claim to `disputed` — and §11.6 fails auto-context closed on an
  unresolved dispute — until an owner-class accept causally answers it.
  If any writer with `memory.propose` can emit counting disputes, an
  enrolled low-trust agent can suppress the owner's entire pinned
  context (retrieval-DoS as poisoning). Dispute needs an explicit
  authority class per policy, and the IAM mapping needs to say which op
  admits it.
- The two built-in policies pin actor classes for *accept* only. The
  full rule table — every verdict × kinds × space_class ×
  actor_classes — must be normative (in the spec text or as the
  normative content of the pinned policy documents), or two
  implementations will fold differently. Open questions the table must
  answer: can a session retract its own asserted observation under
  `workflow-v1` (it could assert it — symmetry says yes)? Whose
  supersede counts? Declassify says "own capability" — which capability?

Add: an op_type × verdict → required-IAM-operation table; the complete
built-in policy tables; vectors for dispute-by-unauthorized-actor
(non-counting) and the self-retract decision either way.

### D4. Capability-epoch scope and lifecycle — an umbrella-assigned D0-A duty, undischarged

Umbrella §4.5 (frozen): "**D0-A also defines capability-epoch scope**
and the minimum control frontier required for high-impact
curation/export." The spec carries `capability_epoch` in grants (§4.3)
and headers (§4.5), keys budgets by it (X4), and… never defines it. No
control op mints or bumps an epoch; scope (plane-wide? per-zone?
per-grant-lineage?) is unstated; the umbrella's two-mechanism revocation
story (encryption epoch + **writer-capability epoch** + cutoffs, §4.5)
has only its encryption half (`KekRotation`) and its cutoff half
(`CutoffSet`) in the vocabulary. Also unstated: what an epoch bump means
for in-flight old-epoch ops (umbrella: quarantine + re-proposal), and
whether/how budgets reset across epochs (X4 implies per-epoch budget
pools — so who advances the epoch controls budget refresh).

Define: scope (recommend per-grant-subject or per-zone), the bump
operation (or fold it into re-issuance: revoking grant G and issuing
G′ at epoch+1 — then say so), old-epoch op handling, and the
budget-reset rule. The X1-style receipt discipline already gives the
acceptance semantics; this is vocabulary + scope, not new machinery.

### D5. Writer identity is unbound — X4 budgets and cutoffs are evadable

`writer_id` is "minted per writer generation" (§3) but nothing
authorizes the minting or binds a writer to a certificate:

- `CapabilityGrant.writer_id` is optional; if a grant omits it, the
  subject device can apparently write under any `writer_id` it invents.
  X4 budgets are keyed `(writer_id, capability_epoch)` — a
  budget-capped device mints a fresh writer per exhaustion and the
  deterministic cut never fires. `CutoffSet`/`AbandonWriter` chase a
  name the adversary abandons for free.
- The umbrella's root-authorized **signing-key continuation record**
  (§4.1: rotation continues a chain under the same `writer_id`) has no
  control op here — yet the envelope semantics it preserves freeze in
  D0-A.
- L4's browser `writer_generation` bump (eviction/restore) mints a new
  `writer_id` locally. If writers require grant binding, every eviction
  needs a fresh admin ceremony (untenable for the browser lane); if they
  don't, see the first bullet.

Resolve with one rule; two workable shapes: (a) **derive writers from
certs** — `writer_id = H(cert_hash ‖ generation)[0..16]`, budgets and
cutoffs keyed by *subject cert* (sum over its generations), generation
self-asserted but budget-irrelevant; or (b) writers are granted
resources (grant binds `writer_id`; a `WriterContinuation` control op
covers key rotation; generation bumps ride a cheap pre-authorized
"successor writer" rule for the browser lane). (a) is simpler and kills
the evasion; (b) matches the umbrella's continuation-record language
more literally. Either way, add vectors: budget-evasion-by-fresh-writer
rejected; continuation across signing-key rotation; L4 bump → writes
accepted without a new ceremony. (Full multiwriter torture stays D0-B —
this is only the binding rule D0-A's own budget/cutoff semantics
already depend on.)

## High

### D6. IAM component applicability per actor kind is undefined — as written it bricks the zero-daemon lane

§10.2: "Any absent, expired, revoked, or non-matching component denies,"
over five components including *daemon-local session grant* and *live
session token*. A zero-daemon browser principal — the plane's founding
persona — has no daemon and no session token; a daemon acting as itself
has no session token either. Read literally, every request from either
is denied (`no-session` / `no-token`). Obviously not intended; the fix
is a small normative table: which components are **required vs
vacuously-satisfied per actor kind** (`agent-session`: all five;
`browser`/`human` direct: device capability ∩ resource constraints ∩
provenance ceiling; `daemon` self: those plus local grant; `peer`:
…). Without it, implementers improvise the most security-relevant
function in the spec. Add one allow vector per actor kind.

### D7. Per-device-class ceilings ("class-based caps") are named but never defined

§10.2's fifth component cites "§7.3 hosted ceiling; class-based caps" —
but §7.3 is the hosted-*genesis-plane* invariant. The umbrella's
per-*device* ceiling (§3.5: a hosted-browser device on **any** plane
gets "no provenance elevation, no cross-plane/high-impact export
approval, low-effect read/write otherwise") exists nowhere in this spec.
Freeze the class → cap table: which grant contents each class may be
the subject of (flows? class_ceiling max? export/graduation approval?),
and which judgment/approval verbs each class's devices may sign.
`mobile` and `other` (new classes this spec adds — fine, but note them
as an umbrella superset) need rows too. Vector: hosted-browser device on
a trusted plane denied flow-grant subjecthood and export approval.

## Medium

### D8. Every erase is an admin ceremony — ratify or restructure

`EraseAuthorize` is admin-arm-only (§7.2). The umbrella's stance is
"root operations are explicit ceremonies, **not per-entry writes**"
(§3.1) — but erase is inherently per-entry, and diary erase is a
headline product property. Consequences as specified: erasing one
regretted entry requires the admin key (unavailable from a non-admin
enrolled phone), and each erase is a control-chain ceremony. Either
ratify that explicitly in §15 (defensible: erase destroys data,
ceremonies are deliberate) or split intent from execution: an erase
*request* as a tenant op under `memory.curate`/`memory.admin`, batched
into the next admin-authorized `EraseAuthorize`+`KekRotation`. The
batching also amortizes D1's O(zone) rewrap cost, which the spec should
state honestly either way (§5.4 is silent on rotation cost and erase
latency; the umbrella promises "UI language must match the implemented
granularity and latency").

### D9. Receipt backdating by a colluding enrolled device — own it in §14

X1 acceptance turns on receipt timestamps; X2 qualifies **any plane
device** as a receipt signer. A compromised-but-enrolled device can sign
a `ReplicaAck`/`StorageReceipt` with a fabricated old `ts_ms`,
laundering an expired-capability op past the deadline (exactly the
stale-authority window §9 exists to close — reopened by any single
colluding device). Minimum: a §14 row — "acceptance-deadline expiry …
does not defend against an enrolled device signing false-timestamped
receipts." Better: let strict-mode `ZonePolicy` name a qualifying
witness class (e.g., checkpoint-witness receipts or ≥2 independent
devices) for expiry-critical acceptance. Vector: backdated receipt from
a revoked-later device — document the chosen outcome.

### D10. Normative size and depth bounds are missing from the encoding profile

E1–E6 admit unbounded sizes: statement text, `causal_references`,
`labels`, `flows`, body bytes. The umbrella's §16 mandates fuzzing
"with hard byte, reference, depth, and time bounds" — but the *bounds
themselves* aren't normative, so implementations will diverge on
acceptance (one folds a 100 MB claim, another rejects it: a
deterministic-fold split). Pin per-object byte caps and per-array count
caps in §1 (or as frozen constants in M3), reject-at-parse, one vector
per cap.

## Pin-list (minor, batch before freeze)

1. **O3's comment is false as written**: "`authored_crypto_epoch` … the
   ONLY epoch in signed bytes" — `capability_epoch` sits two lines below
   in the same signed header, and the admin proof arm carries `epoch`.
   Reword to "the only *KEK/crypto* epoch…".
2. **Unpinned derivations**: `signer_key_id` ("H of signer pubkey" —
   which domain tag? the closed tag set has none for pubkeys; say `raw
   SHA-256(pubkey bytes)` or add a tag); `StorageReceipt.key_id`
   likewise; R1's HKDF salt (state: empty/zero salt); sequence-1
   `previous_writer_hash` sentinel (32 zero bytes?); initial `repoch`
   value; N1's reserved constants are described "-style" — pin exact
   bytes in the text or state explicitly that the vector file is
   normative for them; CRC32C coverage (over `type‖payload`?).
3. `v` vs `protocol_version`: M2 says "`protocol_version` in the header";
   the header field is `v`. Say they're the same or rename.
4. **Cert renewal semantics**: `device_id` is "stable across cert
   renewals" but renewal is undefined — re-`EnrollDevice` with the same
   `device_id`? Does the old cert auto-revoke or coexist?
5. **Evidence document opacity**: `evidence_hash` hashes a per-class
   evidence doc whose format is never given. Either pin the shapes or
   state normatively that evidence docs are opaque bytes validators
   never parse (character-of-evidence lives in `class`); golden vectors
   need *some* canonical bytes.
6. **§11.6 should state** that auto-context assembly still evaluates
   full §10 per-item read authorization for the receiving session (pins
   don't confer read authority — parallel to the umbrella's
   references-confer-no-authority rule).
7. Erase test row should keep the umbrella §16 clause verbatim: "index,
   view, checkpoint, and **backup** copies all unreadable after erase" —
   the spec's test map dropped the index/view enumeration.
8. `retract` and `retire` both fold to `retired`; fine, but note that
   views should surface which (the judgments differ in meaning), and
   that v1 has no un-retract (a retracted claim returns only by
   re-proposal) — say it.
9. Admin-key custody: the umbrella (§3.1) expects vault-style
   passkey-sealed custody for the root/admin key; the spec specifies
   custody only for the recovery phrase (R2/R3). One sentence pointing
   admin-key custody at the platform-appropriate keystore/passkey
   envelope per device class closes the gap.
10. "owner-audit domain" (§10.2) is a new, undefined term — name where
    audit records live (a control-adjacent feed? the source zone?) or
    defer explicitly to D0-B.
11. C2 note worth making explicit for ratification: an *honest* control
    fork (admin daemon restored from snapshot re-running a ceremony)
    freezes the control plane until the owner digs out the recovery
    phrase — deliberate (§15.6), but the phrase-ceremony-for-honest-fork
    UX cost should be said out loud.

## Umbrella-conformance check (v3.1 duties → this spec)

Discharged faithfully: genesis descriptor incl. recovery commitment and
provenance (§4.1 ↔ RFC §3.1/§3.5); tagged `AuthorizationProof` with all
four arms (§4.4 — `admin_key_id` dropped as redundant with
`signer_key_id`: fine, the matrix pins signer=admin(e)); admin epochs +
algorithm migration (C1, `new_admin_alg`); loss ≠ compromise + recovery
precedence + both-secrets-rotate (§8.2, C3); re-root via recovery arm
with hosted-root-cannot-self-succeed (§7.3 — resolves review-F2 exactly
as the umbrella ratified); marker file frozen (N2); provenance evidence
matrix summarized with the custody-not-incorruptibility line (§4.2);
item AEAD/nonce/AAD + DEK wrap + signed-epoch/mutable-epoch split
(§5 — the O3 design itself is exactly right); frontier type (§4.6);
receipt/lease proof types (§4.7); acceptance-deadline expiry + Connect
opt-in time role (X1/X2); typed IAM intersection with closed deny enum
(§10, modulo D6/D7); judgments completed (Supersede{target,replacement},
Declassify, Unpin, `memory.evidence.read`) (§11.2); `assert` =
claim+self-accept (§11.2); advisory `supersedes[]` (§11.3); pins bind
acceptance + disputed-pins-fail-closed (§11.6); composite export
primary + paraphrase residual (§11.5); migration invariant verbatim
(M1); vectors as required keyless CI, crate added by name (§13); KEM =
HPKE DHKEM(P-256) exactly as the umbrella's ratified §4.9.

Not discharged: **capability-epoch scope (D4 — explicitly assigned to
D0-A by §4.5)**. Partially discharged: control vocabulary (D1),
per-class ceilings (D7), writer binding (D5 — the D0-A-relevant slice).

## What's strong

- **The signed/mutable epoch split (O3 + §5.3)** — zone-rotation erase
  without touching signed bytes is the cleanest reconciliation of
  immutable ops with rotating keys in the whole design.
- **Low-S with the fork-framing rationale (S1)** — most specs mandate
  low-S out of habit; this one derives it from its own fork-evidence
  semantics, and pins the rejection vector.
- **Recovery-arm-only re-root (§7.3/C3)** with the explicit
  plane-compromise freeze for competing recoveries — honest about what
  protocol rules cannot adjudicate.
- **X1–X4**: acceptance-deadline expiry over receipts, budgets with a
  deterministic cut, and clocks confined to online leases — a genuinely
  deterministic answer to untrusted time, matching the umbrella's
  HLC-is-never-authority doctrine.
- **N1's one-envelope decision** and the closed deny-reason enum: both
  small, both the kind of choice that prevents a class of drift.
- **The fold's conservatism** (rule 3's causally-answered disputes;
  cycle → disputed) matches the "conflicts visible, never LWW" doctrine
  and is implementable exactly as written — once D3 pins who may judge.
