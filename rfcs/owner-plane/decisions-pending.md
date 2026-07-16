# Owner decisions — rulings record + pending drafts

Status: **D2 and D5 are RULED (owner, 2026-07-14) and recorded in
the spec's decision table** (this file survives as the alternatives
record the rulings chose from). **D6 below is a PENDING DRAFT** —
the owner directed its preparation (2026-07-15, the criterion-12
send-back) but has not ratified the wording; the frozen spec is
untouched until ratification.

---

## D6 (PENDING — DRAFT for owner ratification) — narrow D-202's convergence carrier (proposed D-204)

### Why

The criterion-12 review produced an executable counterexample
against D-202's unqualified sentence "convergence rides the
writer's re-proposed operation": using only the committed
`f09-lease-lifecycle-sticky-reproposal` bytes, a replica that holds
the TIMELY receipt at the original operation's first evaluation
admits the original — so the same-coordinate re-proposal reaching
it is D-130 fork evidence (`fork`, `freeze-writer`), not an
admission. The re-proposal therefore cannot carry convergence
across the two evidence-arrival worlds D-202 itself legitimizes: a
same-lineage sequence-1 retry cannot satisfy both histories, and a
sequence-2 retry cannot follow a rejected sequence-1 without new
chain semantics. The owner chose narrowing the promise over
changing coordinate-consumption semantics or minting a new carrier.

### Proposed T5 prose amendment (§4, T5 — replaces the ruled sentence)

Current (v0.5.20):

> — terminal where issued; a later timely receipt does not re-open
> the verdict, and convergence rides the writer's re-proposed
> operation. Held timely evidence beats held late evidence at first
> evaluation.

Proposed:

> — terminal where issued; a later timely receipt does not re-open
> the verdict. **The re-proposal carrier is structure-relative
> (D-204)**: among replicas sharing the original operation's
> evidence-arrival structure (which qualified evidence — timely or
> late — is held at its first evaluation), convergence rides the
> writer's re-proposed operation at the freed coordinate. A replica
> holding timely evidence at first evaluation admits the ORIGINAL
> operation, and the same-coordinate re-proposal reaching it is
> D-130 fork evidence (`fork`, `freeze-writer`) — the pair is
> reconcilable only by a committed boundary selection (the D-130
> lane). The cross-structure divergence of the pair's verdicts is a
> stated, owner-visible residual of alternative (ii). Held timely
> evidence beats held late evidence at first evaluation.

### Proposed decision row

> | D-204 | D-202's convergence carrier narrowed to shared
> evidence-arrival structure: the re-proposed operation carries
> convergence only among replicas holding the same qualified-evidence
> class (timely vs late) at the original operation's first evaluation
> — late-first replicas issue sticky `lease-stale` and admit the
> re-proposal at the freed coordinate; timely-first replicas admit
> the original, and the arriving re-proposal is D-130 same-coordinate
> fork evidence (`fork`, `freeze-writer`), the pair reconcilable only
> by a committed boundary selection. The cross-structure verdict
> divergence is a stated owner-visible residual of alternative (ii)
> (v0.5.20's unqualified carrier sentence promised a cross-structure
> convergence no same-coordinate retry can honor — the Gate-A
> criterion-12 review's executable trace); harness enforcement: a
> fold vector's listed deliveries MUST share the declared
> evidence-arrival structure, and the structure pair is vector-pinned
> (`f9-lease-lifecycle-sticky-reproposal` late-first /
> `f9-lease-lifecycle-timely-first-forks` timely-first) | Ratified,
> owner, 2026-07-__ |

### Mechanical consequences on ratification (owner's act, not this tranche's)

- Spec version v0.5.20 → v0.5.21; the spec SHA-256 re-pins in
  `gate-a-audit.md`, `README.md`, and the program ledger.
- The harness's listed-deliveries rule ("the listed orders must
  share the declared evidence-arrival structure") becomes the
  normative precondition of the narrowed promise rather than a
  harness convention.
- Both worlds are ALREADY vector-pinned by this tranche (the
  committed late-first vector and the new timely-first sibling, plus
  a reducer cross-world test asserting each world's derived Memory
  state) — ratification changes prose only, no artifact bytes.

---

- **D2 → alternative (c), no class / no vote (D-201).** Bare
  non-human unattested writers never count toward status; their
  judgments are recordable where authoring verbs admit them and
  inert in the §11.2 fold; status influence requires attestation.
  The discriminating drafts below are MINTED as
  `f11-status-bare-daemon-retract-inert` and
  `f11-status-bare-daemon-supersede-inert` (both derive `candidate`
  where the withdrawn session mapping would have derived
  `retired`/`superseded`), and the reference implementation's
  `Unimplemented` boundaries are replaced by the ruled semantics.
- **D5 → alternative (ii), sticky rejection + writer re-proposal
  (D-202).** `lease-stale` is terminal where issued; convergence
  rides the re-proposed op; the original op's verdict is knowingly
  evidence-order-relative. Written into the T5 prose (v0.5.20).
  Draft C was already committed as `f9-lease-stale-quarantines`;
  draft D is MINTED as `f9-lease-late-then-timely-receipt-admits`
  (held timely evidence beats held late evidence).

The analysis below is preserved as written when the decisions were
open; the reference-implementation touchpoint lists describe the
PRE-ruling state and no longer match the code.

Both decisions share one hard constraint: the **B.2/B.3 policy
literals and their pinned hashes never change** (B.2 `workflow-v1`
1133 B / `219b9bac…`, B.3 `owner-v1` 571 B / `d7d5559a…`). Every
alternative below is stated against the pinned bytes; an alternative
that would need new policy vocabulary can only take effect through a
FUTURE policy version, never by editing the pins.

---

## D2 — the bare-writer actor class (§11.4)

### The gap

§11.4 derives four actor classes: `owner` (direct-human evidence +
full judgment rights), `safe-human` (direct-human otherwise),
`session` (an **attested** actor — §10.1 shape 2), and `service`. A
**bare non-human unattested writer** — an autonomous daemon or
browser agent authoring under its own device certificate with no
`attested_by` — matches no row. The withdrawn scaffold silently
derived `session` for it.

### Why the silent mapping granted authority

The B.2 `workflow-v1` rules name `session` in four places:

| verdict | kinds | spaces | actor_classes | relation |
|---|---|---|---|---|
| retract | * | * | peer, session, external, safe-human | author |
| supersede | * | workflow | session | author |
| accept | episode, observation | workflow | session | self |
| raise_class | * | * | owner, session, safe-human | any |

Under the silent mapping, a bare unattested daemon's judgments
**count toward claim status** exactly as an attested session's would
— attestation stops gating status influence. That is an authority
grant the spec never made.

### What is settled regardless of the ruling

- Bare writers **authoring claims** (`m.claim` under `propose` /
  `assert`) are class-independent and stay admitted; the corpus's
  daemon-authored claims are unaffected.
- **Admission** of `judge.safe` / `judge.full` rows requires
  direct-human evidence / the owner class under every alternative
  below except (d)-with-new-rows — a bare writer holding neither
  judge verb rejects `scope-op` under every ruling.
- An **attested** non-human writer is `session` (settled §11.4
  shape 2).

What the ruling decides: (1) whether a bare writer's *admitted*
judgments (the author-relation retract/supersede paths, which admit
through authoring verbs) **count** in the §11.2 status fold; (2)
whether a bare writer holding a judge verb may exercise it at all.

### Alternatives

**(a) `session`** — the withdrawn scaffold default. Bare autonomous
writers inherit attested-session counting rights: a daemon's
self-retract counts (B.2 retract row), its workflow self-supersede
counts (B.2 supersede/session row). Consequence: attestation ceases
to distinguish authority; the weakest-trust reading.

**(b) `external`** — bare writers count only where `external`
counts: the retract/author row (which names external) but NOT the
session-only supersede and accept rows. Consequence: narrower than
(a), but still decides status influence for a class the prose never
placed.

**(c) no class (least authority)** — bare-writer judgments never
count toward status: admissible as records where authoring verbs
admit them, inert in the §11.2 fold. A daemon wanting status
influence must operate attested (becoming a session) or through
service rows. Consequence: the least-authority default, consistent
with the trust posture; the visible cost is that a bare daemon's
retract-of-its-own-claim is recorded but does not move status.

**(d) a new class** (e.g. `autonomous`) — new §11.4 row + future
policy vocabulary. Under the PINNED B.2/B.3 bytes no rule names the
new class, so (d) is byte-identical to (c) today; it differs only in
what a future policy version can express without touching §11.4
again.

### Discriminating vector drafts (unminted)

Both drafts use a human-authored genesis plane; dev2 is a bare
daemon (`actor_kind: daemon`, no `attested_by`) holding `propose` on
a workflow space governed by B.2 `workflow-v1`.

**Draft A — `f10-bare-daemon-self-retract-status` (separates
{a, b} from {c, d}).** dev2 proposes claim `i`, then retracts it
(`m.judge` retract, author relation — admission is settled through
`propose`). Expected status of `i`:

| ruling | status |
|---|---|
| (a) session | `retired` (retract/author row names session) |
| (b) external | `retired` (same row names external) |
| (c) / (d) | `candidate` (the judgment is recorded, counts nowhere) |

**Draft B — `f10-bare-daemon-self-supersede-workflow` (separates
(a) from (b)).** dev2 proposes claim `i`, then supersedes it with
replacement `r` (admission through `propose` + author + workflow);
the human owner accepts `r` (so the replacement reaches `accepted`
and rule 2 can fire). Expected status of `i`:

| ruling | status |
|---|---|
| (a) session | `superseded` (the supersede/session/author row) |
| (b) external | `candidate` (that row names session ONLY) |
| (c) / (d) | `candidate` |

### Reference-implementation touchpoints (now honest)

- `reducer/src/fold.rs` `actor_class` → `None` for bare non-human
  unattested writers (was: `"session"`).
- `admit_judge` — a bare writer HOLDING a judge verb surfaces
  `Unimplemented("bare-writer actor class awaits the owner's D2
  ruling")` instead of a silent row rejection (a rejection would
  encode (a)–(c) against (d)).
- `claim_status` — a standing bare-writer judgment on the target
  surfaces the same `Unimplemented` instead of counting or not
  counting.
- Verified: no committed vector reaches any of the three (every
  corpus `m.judge` is human-actored; daemon writers author claims
  only).

---

## D5 — the late-receipt lifecycle (T2/T5 lease staleness)

### The gap

Lease qualification (T2 anchored at `policy(zone, capability_epoch)`
resolving downward; T5 skew fixed at 300 000 ms) classifies a held
qualified observation OUTSIDE every valid lease window as
`(lease-stale, quarantine-reproposal)`. The spec does not pin the
**lifecycle** when a *later, timely* receipt arrives at a store that
already classified the op stale:

- Replica R1 holds {op, late receipt} → `lease-stale`.
- Replica R2 holds {op, late receipt, timely receipt} → admits.
- R1 then receives the timely receipt. Same final fact set — do the
  replicas converge on the op's verdict, and through which
  mechanism?

The tranche removed the "conclusive staleness" wording
(`reducer/src/fold.rs`, `core/src/corpus_time.rs`): the verdict is
staleness **on the held evidence** — a later timely receipt is not
precluded, and calling the rejection conclusive presumed an answer.

### Why the current corpus cannot see the gap

The time-lane fixtures carry receipts as `aux` (held before any
delivery), so every evaluation — in both convergence orders — sees
the full receipt set. Distinguishing the alternatives needs a
receipt-ARRIVAL lane (receipts as delivered items interleaved with
the op), which is new fixture machinery; the endpoint drafts below
stay within the existing lane.

### Endpoint vector drafts (unminted, paired)

Identical fixture: dev2 holds an `online_lease` grant
(`max_age_ms = 2 d`), a valid lease window `[T0, T0 + 1 d]`, and
authors one claim. The only variable is the held receipt set.

- **Draft C — `f9-lease-late-receipt-stale-aux`**: aux holds ONE
  qualified receipt at `T0 + 1 d + skew + 100 s` (outside every
  window). Expected: `(lease-stale, quarantine-reproposal)`.
- **Draft D — `f9-lease-late-then-timely-augmented-aux`**: the same
  fixture plus a second qualified receipt at `T0 + 12 h`
  (in-window). Expected: admitted.

The pair pins the two endpoint states. The LIFECYCLE between them —
what a store in C's state does when D's extra fact arrives — is the
open question; the drafts deliberately do not encode it.

### Alternatives for the lifecycle

**(i) Re-evaluation on evidence arrival.** A `lease-stale` op
re-enters evaluation when a qualified receipt for its zone arrives;
R1 converges to R2 automatically. Consequences: rejected-time ops
must be retained and re-triggerable (rejection is no longer terminal
for this outcome class); the fixpoint machinery gains a
rejected-set, not just a pending-set.

**(ii) Sticky rejection; the writer re-proposes.** The
`quarantine-reproposal` disposition read literally: the ORIGINAL
op's rejection is terminal wherever it was issued, and convergence
is carried by the writer's re-proposed op (which both replicas
admit). Consequences: simple, rejections stay terminal; the original
op's verdict is knowingly allowed to differ across replicas whose
evidence arrived in different orders — the spec would need to state
that this divergence is accepted and invisible to derived state.

**(iii) Timeboxed pendency instead of rejection.** Absence of
in-window evidence never rejects; the op pends until an explicit
horizon (a new prose/wire mechanism) closes it. Consequences: avoids
the divergence entirely; requires a horizon rule that does not exist
today — under the repair-tranche scope rules, choosing (iii) is a
STOP-and-report item, not an implementable amendment.

The reference implementation's mechanics today match (ii) —
rejections do not re-enter the fixpoint — but the corpus cannot yet
distinguish (i) from (ii), and no committed artifact asserts either.

---

*Related but not a decision:* the reopen invalidation work (tranche
6b) surfaced that D-193's rationale for stmt-kind invalidations
("fork-discovery statements are real killers") has **no §4.7 wire
shape** to verify against — recorded as a finding in the Gate-A
audit, not here, because closing it needs a wire mechanism, which is
out of tranche scope.
