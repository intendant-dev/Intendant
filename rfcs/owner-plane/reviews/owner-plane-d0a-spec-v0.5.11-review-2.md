# Review 2: D0-A Core + Memory specification v0.5.11

*2026-07-13. Independent review of [`owner-plane-d0a-spec.md`](/Users/vm/owner-plane-d0a-spec.md)
v0.5.11 (3,808 lines, D-1..D-142), against the v0.5.10 synthesized
review's seven blockers, two highs, six owner rulings, and ten-step
sequence. Written without reading `owner-plane-d0a-spec-v0.5.10-review.md`.
Method: the full floor — convergence, constructibility, authority,
lifecycle totality — applied per ruling; every wire claim chased to
its CDDL rule (including one false alarm of my own: the `txnrec`
union's line wrap briefly hid `xferreopen`, whose rule, frame-table
entry, and `? basis` field are all present).*

## Executive verdict

**All seven blockers and both highs are discharged, and this round's
repairs include two of the strongest design moves in the series.**
D-135 replaces the broken `Top`-init max fold with the tagged
`Unbounded/Bounded` state and makes the snapshot override **total
over generations** (carried → head, uncarried → `"none"`; an empty
carried `heads` entry overrides every generation — the pure-snapshot
zero-history ceremony is genuinely total). D-136 makes authority
closes explicit scalars with the staged `ccutoff.closes` lane
(requesterless/trusted, outside the hosted ceiling), **withdraws**
D-130's per-generation freeze for under-closing futures, and —
exemplary hygiene — **removes** `ratified_through` (its last consumer
was coverage) and the `"none"` arm from `ratifycutoff` (an assertion
with no effect is unencodable under E4); all five remaining mentions
are ledger history. D-137 extends scope reservation to tenant
coordinates. D-138 makes recovery omission portable the elegant way —
an implicit revivable `"none"` override at the recovery's fold
position, dissolving the author-time-coordinate problem instead of
solving it (both sides of the indistinguishable pair quarantine, then
ratify growth revives) — makes C3′ removal a **total re-fold**
(incremental must converge to fresh; the D-131 list demoted to
instances), and fixes space retirement with mandatory scalar close
coverage so old-epoch backdating dies on the immutable close, not the
gameable anchor. **D-139 is the round's centerpiece:** one active
`import`-verb grant per destination zone (the one-live-lineage
pattern; the genesis grant counts) gives every import a canonical
`(gen, seq)` chain order — the first claimant holds the replay key
*forever by construction*, no displacement, no late winner, no hash
grinding; the lowest-`op_hash` rule is honestly voided. D-140 makes
terminals basis-stable (`? basis` on XferAbort; `XferReopen` in the
union, the frame table, and its own CDDL rule; `XferDone` never
reopens — stated residual; the §10.5 permanence lattice split by
boundary purpose). D-141 ratifies the deadline/lease cap exemption as
a residual, makes cap existence a derived function, re-keys renewal
history coverage to the **authorship domain** (exclusion never
orphans authored history), makes KEM custody transitive across
K0→K1→K2, and widens signing-key freshness to every non-genesis
certificate (the D1-signs/D2-witnesses bypass dies). D-142 lands my
B1 exactly as specified — `mimport`'s prohibited fields structurally
absent, provenance exactly the import tuple — plus the mirror-equality
matrix, namespaced effect keys, and the honest correction that
`release_op` in `mimport` is a *signed mirror* of a derived value.
My v0.5.10 M1 is closed twice over (`cspaceretire = {space_id,
? closes}` with coverage mandatory regardless of strictness).

**One finding, medium: the D-142 principle wasn't applied to
`mclaim`.** Nothing else survived the floor.

---

## Part 1 — Finding

### Medium

**M1. `mclaim` keeps a structurally-valid, always-rejected
`provenance.import` arm — the exact anti-pattern D-142 just
eliminated in `mimport`.** The mclaim CDDL retains
`? import: { from_plane, export_id, … }` with a comment that
`m.propose`/`m.assert` carrying it reject (D-142's rule). D-142's own
rationale was that prohibited content must be **structurally absent**,
not comment-forbidden; the arm is now dead weight — v1 imports ride
`m.import.claim` exclusively, whose narrowed shape carries the tuple,
and §11.6's `import class_floor` input is fed by that shape. Because
the arm is optional, producers simply omit it and every valid body
remains producible — hence medium, not blocking — but it is precisely
the schema-says-more-than-the-protocol drift the discrepancy audit
exists to catch, and it contradicts the round's own stated principle.
One-line fix: delete the arm from `mclaim` (its rejection rule then
follows from O3's closed dispatch with no special case).

---

## Part 2 — Gate-A readiness

Applying the four-question floor to this cut: **constructibility** —
the narrowed import mints trivially, the reopen record has its wire,
and the withdrawn/removed machinery leaves no dangling consumers;
**authority** — import identity is chain-ordered under a single
ruled grant, closes are explicit owner-grade scalars, and the cap
exemption is a ratified residual rather than an accident;
**lifecycle totality** — first-event folds, empty-snapshot totality,
reservation release, total re-fold on removal, basis-invalidated
reopen, and purpose-split permanence each have their transition;
**convergence** — every new rule is a fold of held bytes and control
order. Twelve rounds in, the residue is one dead optional arm. The
verdict on freezing is the synthesis's to make, not mine — my
v0.5.9 lesson stands — but I can state the observable: this is the
first cut where the raised floor, applied ruling by ruling, produced
no blocking or high finding, and the one medium is a one-line
consistency sweep of the round's own principle.

**Recommendation.** Fold M1's one line, then proceed to the artifact
sequence as ordered in every recent synthesis: author
`d0a-vector-cases.v1.json` first — its opening tranche should pin the
tagged fold's transition table, the total-snapshot override, the
staged-closes coverage, the D-139 chain-order import identity
(first-claimant-forever, both orders, fresh fold), the basis/reopen
matrix, and the authorship-domain renewal coverage — then the
independent core and harness, the corpus, family 14, every required
surface, and the prose↔CDDL↔companion↔vector discrepancy audit as the
Gate-A decider. Durable P1 writes remain prohibited until Gate B plus
the umbrella's P0.5/tombed-cutover prerequisites, unchanged.
