#!/usr/bin/env python3
"""Rebuild coverage/obligations-13-3.json — the §13.3 obligation
ledger: line-range obligations with verbatim quote pins, mapped to
the vectors that exercise them. This script is the ledger's
maintenance tool: edit the ob() entries here, run it, and it
validates every quote (verbatim, within its claimed range), the
full line coverage of §13.3, and every vector name against
vectors/ BEFORE writing. The core suite's
obligations_ledger_is_sound then enforces the committed JSON
(including that gate_b_deferrals.outcomes equals the
UNCOVERED_10_4 pin exactly)."""
import json, os, sys

# The rfcs/owner-plane root, resolved from this script's location
# (coverage/build-obligations.py -> ..).
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
spec = open(os.path.join(ROOT, "owner-plane-d0a-spec.md")).read()
start = spec.find("### 13.3 Families")
end = spec.find("\n## 14.", start)
section = spec[start:end]
lines = section.splitlines()  # Rust str::lines parity
N = len(lines)

# Vector NAMES = file names minus the fNN- prefix.
names = set()
for f in os.listdir(os.path.join(ROOT, "vectors")):
    if f.endswith(".json"):
        names.add(f[:-5].split("-", 1)[1])

E = []
def ob(id, family, a, b, quote, obligation, vectors=(), status=None, note=None, tests=()):
    if status is None:
        status = "vectored" if vectors else "pending"
    e = {"id": id, "family": family, "lines": [a, b], "quote": quote,
         "obligation": obligation, "vectors": sorted(vectors), "status": status}
    if note: e["note"] = note
    if tests: e["tests"] = list(tests)
    E.append(e)

def V(*prefixes):
    """All vector names starting with any prefix."""
    out = [n for n in names if any(n.startswith(p) for p in prefixes)]
    assert out, f"no vectors match {prefixes}"
    return out

ob("s-header", 0, 1, 2, "### 13.3 Families", "Section heading.", status="structural")

ob("f1-encoding-caps", 1, 3, 5, "1 encoding+caps",
   "Encoding + caps incl. encoder-exact joint fits (314 B/132 B worst widths, 128+128 rotation fit, 48-KiB checkpoint budget, D-96).",
   [n for n in names if n in {"checkpoint-joint-budget","depth-nine","duplicate-map-key","erasemref-worst-width","float-rejected","frontierclose-sixty-six-heads","indefinite-length-array","kekwrap-worst-width","map-key-encoded-byte-order","negative-int-rejected","nonminimal-uint","rotation-128-wraps-128-manifest-joint-fit","trailing-bytes","txn-seventeen-records","uint-above-2p53","uint-boundary-widths","unsorted-map-keys"}])

ob("f2-domains", 2, 6, 6, "2 domains/key-ids;",
   "Hash domains and key ids.",
   ["hash-domain-frontier-empty","hash-domain-genstart","hash-domain-op","key-id-hpke-p256","key-id-p256","mat-id-role-neutral","separation-op-vs-body"])

ob("f3-signatures", 3, 7, 7, "3 signatures (low-S);",
   "Ed25519 + P-256 signatures incl. low-S.",
   ["ed25519-sign-then-verify","ed25519-verify-tampered","ed25519-verify-valid","p256-sign-then-verify-low-s","p256-verify-high-s-rejected","p256-verify-low-s-valid"])

ob("f4-hpke", 4, 8, 8, "4 HPKE (incl. malformed-point/identity-DH);",
   "HPKE seal/open incl. malformed point and identity DH.",
   ["identity-point-rejected","malformed-point-rejected","seal-open-roundtrip","tampered-ciphertext-rejected"])

ob("f5-item-crypto", 5, 8, 9, "5 item crypto (per-item wrap-key derivation +",
   "Per-item wrap-key derivation, rewrap byte-idempotence, wrapper-mismatch.",
   ["item-aead-tamper","item-open-full-chain","rewrap-byte-idempotence","wrapper-address-mismatch"])

ob("f6-frontier", 6, 9, 10, "6 frontier (sort key,",
   "Frontier sort key, retirement, cap.",
   ["duplicate-lineage-gen-pair","fold-sort-and-replace","retire-above-head-noop","retire-at-or-below-drops","unsorted-heads"])

ob("f7-walkthroughs", 7, 10, 13, "bootability walkthroughs",
   "Trusted-solo/hosted-solo/multi bootability; solo-to-multi transition; witness loss; C2 freeze; C3-prime placement/precedence/unknown-branch cut.",
   ["walkthrough-hosted-solo-boot","walkthrough-c2-freeze-both","walkthrough-c3-branch-cut-below-head"],
   status="partial", note="trusted-solo and multi walkthroughs, solo-to-multi transition, and witness loss are unvectored.")

ob("f7-hosted-ceiling", 7, 13, 18, "hosted ceiling incl. safe-verb grants accepted,",
   "Hosted ceiling: safe-verb grants, excluded verbs, system-only audit grant boot, exclusion-shaped rotation pair, recovery-succession admissibility, attested multi-head self-cutoff, lineage_reauth, consumed request_ids, drill.",
   ["hosted-ceiling-grant-verb-excluded","hosted-ceiling-zone-policy-inadmissible","consumed-request-id-fork","walkthrough-drill-acceptance","walkthrough-hosted-solo-boot"],
   status="partial", note="exclusion-shaped rotation pair, recovery-succession admissibility, attested self-cutoff, and lineage_reauth are unvectored; the audit-grant boot rides the hosted-solo walkthrough.")

ob("f7-zone-policy-position", 7, 19, 28, "zone-policy-advances-epoch",
   "Zone-policy epoch advance + arrival-order equivalence; position-relative authorization; strict-zone closure; grant-revocation cutoff + issuance-to-revoked-device; hosted cross-lineage negative; cutoff max-compose; renewal shapes.",
   ["issuance-to-revoked-device-rejects"],
   status="partial", note="the issuance negative is vectored; the equivalence pairs and renewal shapes are not.")

ob("f7-compound-pipeline", 7, 29, 37, "one-live-compound-per-revocation_id",
   "One live compound per revocation_id; derived grant revocation; the arm-indexed control pipeline (body-before-precedence, D-99); cutoff algebra (D-93); authored-but-unwrapped revocation.",
   ["second-live-compound-rejects","control-body-tamper","control-signature-tamper","control-wrong-proof-arm","c2-post-freeze-valid-op-frozen","c2-post-freeze-sig-invalid-kept","revoke-refs-post-wrap-exclusion-completes","revoke-refs-stale-rotation-rejects","revoke-cutoff-carried-head-completes","revoke-cutoff-head-mismatch-selects","revoke-cutoff-empty-heads-with-history-rejects","header-unknown-version-rejects"],
   status="partial", note="post-C.1: the D-71 rotation-refs linkage pair, the D-93 carried-head/mismatch/D-143-exactness triple, and the unknown-version parse negative are vectored; the ratify/snapshot cutoff-algebra ceremonies and authored-but-unwrapped revocation remain deferred sagas (D-203).")

ob("f7-revocation-constructibility", 7, 38, 47, "revocation constructibility",
   "Revocation constructibility across author/wrap domain shapes (D-165/D-173/D-180/D-200).")

ob("f7-recovery-adoption", 7, 48, 54, "recovery storage adoption",
   "Recovery storage adoption, storage-orphaned, provisional precedence (D-104/D-105/D-112).")

ob("f7-abandon-seal", 7, 54, 59, "abandon-seal vectors",
   "Abandon-seal per generation; the hosted composition (D-101/D-107/D-122).")

ob("f7-snapshot-wins", 7, 60, 63, "snapshot-wins cutoff five ways",
   "Snapshot-wins cutoff five ways + composition negatives (D-108/D-114).")

ob("f7-ratify-ceremonies", 7, 63, 76, "the multi-generation",
   "Multi-generation ratify ceremony, requester-form negatives, total-override snapshot, close-staging, cross-lineage negative, equation-only ceremony (D-120/D-129/D-135/D-136).")

ob("f7-override-growth", 7, 76, 84, "override/growth cycle",
   "Override/growth cycle, tagged-state probes, repeated-seal min-compose (D-121/D-135).")

ob("f7-per-gen-ratify", 7, 84, 88, "per-generation ratify",
   "Per-generation ratify inertia and owner truncation (D-114/D-122).")

ob("f7-incorporation-cap", 7, 89, 98, "held-bytes incorporation cap",
   "Held-bytes incorporation cap convergence + cap eligibility (D-122/D-128/D-141).")

ob("f7-adoption-bound", 7, 99, 101, "adoption bound (65th entry rejects",
   "Adoption bound; accepted-but-unFenced non-adoption (D-117).")

ob("f7-consumed-boundary", 7, 101, 111, "promotion as frontier materialization",
   "Consumed-boundary promotion, multi-head closure trace, maximum-lineage bound (D-152).")

ob("f7-staging-cycle", 7, 112, 127, "the one-shot staging cycle",
   "One-shot staging, the stage state machine, staged-lineage-revoked vacuous consumption (D-153/D-176/D-196).",
   ["staged-frontier-consumed-no-resurrection"],
   status="partial", note="the D-196 vacuous-consumption/no-resurrection trace is vectored; the pending-carrier machine is not.")

ob("f7-delayed-completion", 7, 128, 135, "delayed-completion cessation",
   "Delayed-completion cessation (D-195) and delayed-reference convergence (D-199).",
   ["pending-revocation-window-grant-completing-rotation","delayed-reference-convergence-c1-i-c2"])

ob("f7-old-signer", 7, 136, 143, "old-signer resurrection negative",
   "Old-signer resurrection negatives, proven-incompatible pair lifecycles, the four-axis recovery-frontier predicate (D-174/D-181).")

ob("f7-post-advance-wgen", 7, 144, 147, "a post-advance `w.gen`",
   "Post-advance w.gen at the old epoch dies on the total override; below-frontier ratify revival (D-136/D-143).")

ob("f7-fork-selection", 7, 148, 151, "exact-Head fork selection",
   "Exact-Head fork selection convergence (D-130).")

ob("f7-selector-reservation", 7, 151, 157, "reservation race",
   "Pending-selector reservation races (D-137/D-145/D-154).")

ob("f7-cap-anchor-race", 7, 157, 162, "cap-anchor",
   "The cap-anchor race: chain-membership never terminality (D-144/D-159).")

ob("f7-supersede-immutability", 7, 162, 164, "supersede-boundary immutability",
   "Supersede-boundary immutability (D-102).")

ob("f6-comparator", 6, 163, 165, "comparator",
   "Comparator negatives: equal coordinates, differing hash.",
   ["equal-coordinates-differing-hash"])

ob("f7-checkpoint-chain", 7, 165, 168, "checkpoint chain",
   "Checkpoint chain linkage, retirement, fold transition, coverage (D-88/D-96).")

ob("f7-genesis-renewal", 7, 169, 180, "genesis grant completeness",
   "Genesis grant completeness, class table, renewal non-resetting, current-membership renewal set, authorship-domain history coverage (D-125/D-133/D-141/D-151).")

ob("f7-custody-freshness", 7, 180, 198, "transitive-custody chain",
   "Transitive custody, KEM-key freshness, role-neutral material identity, alternate-role matching, negation-residual acceptance, effective-certificate wrap negative (D-141/D-150/D-175/D-182/D-190/D-197).",
   ["mat-id-role-neutral","negation-residual-acceptance"],
   status="partial", note="mat_id role-neutrality and the D-197 negation residual are vectored; custody chains and alternate-role matching are not.")

ob("f8-recovery-derivation", 8, 198, 209, "8 recovery",
   "Recovery derivation: the phrase/commitment KATs plus omitted-pair override, naming-terminates vs omission-continues, post-recovery first write (D-132/D-138/D-151/D-159).",
   ["commitment-from-recovery-pk","phrase-checksum-invalid-rejects","phrase-derive-fixed-entropy","phrase-derive-zero-entropy"],
   status="partial", note="the derivation KATs (incl. the D1 checksum negative) are vectored; the omission/naming fold shapes are not.")

ob("f8-total-refold", 8, 209, 215, "re-fold: a cut reauth re-quarantines",
   "The total re-fold: cut reauth/retirement re-derivation, dissolved caps, fork-selector removal (D-128/D-131/D-138/D-149).")

ob("f8-kem-adoption", 8, 215, 232, "KEM-renewal adoption",
   "KEM-renewal adoption, adopted-key reuse negatives, post-recovery-enrollment write, the cut-unadopted key differential (D-150/D-158/D-164/D-167/D-172).")

ob("f9-receipts-deadlines", 9, 232, 237, "self-receipt",
   "Self-receipt non-qualification, renewed-key self-receipt, deadline at/past, deadline-bearing vs deadline-free (D-56).",
   ["deadline-self-receipt-nonqualifying","deadline-receipted-admits","deadline-unreceipted-pends"],
   status="partial", note="renewed-key self-receipt and the fence-hardened pending arm are unvectored.")

ob("f9-t5-lease", 9, 237, 243, "T5 window binding",
   "T5 window binding; issuer-fork recovery; GC-fence hardening; service-key resolution; per-posture issuance; witnessless-zone unusability.",
   ["lease-stale-quarantines","lease-late-then-timely-receipt-admits","lease-lifecycle-sticky-reproposal","lease-online-grant-admits","lease-missing-pends","lease-present-no-receipt-pends","lease-overlong-window-invalid","witnessless-zone-deadline-unusable"],
   status="partial", note="the T5/lease shapes (incl. the D5 negatives) and witnessless zone are vectored; issuer-fork recovery, GC hardening, service keys, and posture issuance are not.")

ob("f9-succession-anchor", 9, 242, 249, "succession trio",
   "Succession trio, epoch-anchor qualification across a policy change, attestation freshness (D-108/D-114).")

ob("f9-chained-feeds", 9, 248, 254, "chained feeds",
   "Chained feeds: issuer-gap, backfill issuer-fork, boundary ancestry gating, min-merge, compromise cutoffs over leases (D-87/D-95).",
   ["compromise-cutoff-retro-disqualifies"],
   status="partial", note="post-C.1: the T4 compromise receipt-cutoff retro-disqualification is vectored (min-merge at the registry, through=0 boundary); the issuer chain machinery (gap/fork/ancestry) remains a deferred saga (D-203).")

ob("f9-reauth-freeze", 9, 254, 266, "the durable one-shot reauth posture",
   "One-shot reauth, issuer-fork freeze-both, the cross-carrier registry (D-100/D-115/D-124/D-131).")

ob("f9-key-freshness", 9, 265, 272, "plane-wide signing-key freshness",
   "Plane-wide signing-key freshness over the portable domain (D-131/D-141/D-150).")

ob("f9-feed-closure", 9, 272, 280, "(ancestry gating + min-merge + hardening exemption",
   "Feed closure as a T3 boundary, retro-disqualification, renewal closure, dropped-witness forever-pending (D-88/D-111/D-118/D-124).")

ob("f10-budget-core", 10, 279, 286, "10 lineage/budget",
   "Generation non-reset, window exhaustion, cutoff across generations, epoch-bump-only reset (D-79), canonical budget order, signed window anchor, displacement revival (D-86/D-94).",
   ["f10-budget-bump-reset","f10-budget-displacement","f10-budget-policy-noreset"],
   status="partial", note="bump reset, displacement order, and the zone-policy no-reset are vectored; exhaustion/cascade shapes are not.")

ob("f10-effect-finality", 10, 286, 292, "effect-finality gating",
   "Effect-finality gating and the incorporation-cap clamp (D-101/D-107/D-122).")

ob("f10-cap-negatives", 10, 292, 297, "64-open-gap cap negative",
   "The 64-gap, held-zones, and zone-recipient cap negatives; split-brain convergence; wildcard exclusion (D-103/D-109/D-110/D-125).")

ob("f10-portable-currency", 10, 296, 302, "grant-epoch",
   "Grant-epoch lower bound; portable currency (epoch-unopened pending + equivalence, monotonicity, slack, closure-cutoff) (D-78/D-93).",
   ["grant-epoch-lower-bound","epoch-unopened-pends-until-the-bump"],
   status="partial", note="the lower bound and epoch-unopened pendency are vectored; monotonicity/slack/closure negatives are not.")

ob("f10-chain-arithmetic", 10, 302, 303, "chain arithmetic",
   "w.gen axis bypass; chain arithmetic negatives per axis.",
   ["tenant-same-seq-fork","tenant-gap-pends-causal-missing"],
   status="partial", note="the same-seq fork and gap-pends axes are vectored; the remaining axes are not.")

ob("f10-last-known", 10, 303, 310, "`last_known` validation negatives",
   "last_known validation and effective-accepted-head retirement (D-167/D-175).")

ob("f10-reauth-widening", 10, 309, 312, "reauth-widening revival",
   "Reauth-widening revival on both orders (D-132).")

ob("f10-space-retirement", 10, 311, 318, "space retirement with mandatory frontier closure",
   "Space retirement with mandatory frontier closure (D-132/D-138/D-143).")

ob("f11-status-rows", 11, 318, 322, "all §11.2 rows",
   "All §11.2 status rows, unauthorized dispute, revival, hosted safe-human accept, assert batch, pin.safe limits, erase-request immediate exclusion.",
   ["status-author-retract","status-bare-daemon-retract-inert","status-bare-daemon-supersede-inert","status-dispute-recorded-not-counting","status-owner-accept","status-revival-on-replacement-loss","status-safe-human-dispute-counts","status-superseded-by-accepted-replacement","erase-deferral-nonterminal-journal"],
   status="partial", note="the status rows, revival, and immediate exclusion are vectored; hosted accept, assert batch, and pin.safe are not.")

ob("f11-classification", 11, 321, 324, "classification floor preservation",
   "Classification floor, concurrent-raise-beats-declassify, raise quota, dangling-evidence sensitive.")

ob("f11-export-import", 11, 323, 327, "export/import digest match",
   "Export/import digest match/mismatch/replay incl. source_op binding and cross-plane fail-closed; record-level transfer.",
   ["export-import-construct-and-rederive"],
   status="partial", note="construct-and-rederive is vectored; the mismatch/replay/cross-plane negatives are not.")

ob("f11-typed-erase", 11, 326, 328, "typed-erase recovery",
   "Typed-erase recovery: manifest to derived tombstones to erased status; target_op membership.",
   ["erase-crash-state5-tombstone-rederivation","erase-crash-state6-complete","kek-rotate-manifest-admits","kek-rotate-manifest-target-outside-rejects"],
   status="partial", note="the storage-lane manifest binding (D6) and the §5.4 fold-admission face incl. target_op membership (C.1 item 1) are vectored; the fold-side erased-status projection is not.")

ob("f11-actor-id", 11, 328, 329, "actor-id minting per kind",
   "Actor-id minting per kind; non-conforming id rejects.",
   ["actor-id-mint-negative"])

ob("f11-audit-partition", 11, 329, 334, "audit trigger all three branches",
   "Audit trigger branches, typed principal, partition exactness, result-id domain, the 4096 cap, flow-deadline bound.",
   ["audit-partition-two-chunks","audit-zero-result-single-chunk","audit-chunk-index-out-of-range","audit-duplicate-chunk-index","audit-changed-principal","audit-changed-scope","audit-changed-count","audit-overlapping-result-sets","audit-release-missing-middle-refused","audit-release-missing-last-refused","audit-release-omitted-result-refused","audit-release-extra-result-refused","audit-release-split-txn-refused"],
   status="partial", note="partition exactness (incl. the D9 conflicts) is vectored; trigger branches, the 4096 cap, and flow-deadline are not.")

ob("f11-surcharge", 11, 333, 339, "content-independent surcharge accounting",
   "Content-independent surcharge accounting; consumer mirrors on effect finality; the release_op critical section; PendingXfer dormancy (D-98/D-101/D-106/D-113/D-123).")

ob("f11-stamp-precedence", 11, 339, 345, "frozen-stamp",
   "Frozen-stamp recovery, stamp finality, state-derived terminal precedence (D-98/D-106).")

ob("f11-release-identity", 11, 345, 353, "release_op identity",
   "release_op identity vs export_id correlation; the one-record abort; the reason split (D-123/D-126).")

ob("f11-release-rederive", 11, 353, 356, "construct-and-rederive (sources + keys",
   "Release construct-and-rederive: content_digest and release_op independently re-derived (D-127).",
   ["export-import-construct-and-rederive"])

ob("f11-import-ownership", 11, 355, 382, "derived import ownership",
   "Derived import ownership, collision reservation/unfreeze cluster (D-139/D-146/D-155/D-161/D-169/D-177/D-196).",
   ["collision-loser-reenters-on-winner-death"],
   status="partial", note="the loser-reentry unfreeze trace is vectored; the reservation and issuance shapes are not.")

ob("f11-effect-keys", 11, 382, 386, "effect-key granularity",
   "Effect-key granularity and incarnation-keyed terminals (D-134/D-142/D-148).")

ob("f11-quarantine-cleanup", 11, 385, 389, "permanent-quarantine cleanup",
   "Permanent-quarantine cleanup aborts (D-134).")

ob("f11-reopenable-journal", 11, 388, 405, "the reopenable journal fold",
   "The reopenable journal fold: independent bases, C3-prime-cut reopen, collision-terminal unreachability, reopen before/after its invalidation, terminal-first reservation, the T0-R0-T1 battery (D-163/D-177/D-185/D-192/D-193).",
   ["reopen-basis-op-kind-and-unheld-invalidation","reopen-recovery-invalidation-unheld-pends","reopen-recovery-keeps-basis-rejects","reopen-forged-recovery-log-corrupt","reopen-unadmitted-recovery-pends","txn-internal-order-and-competing-terminals"],
   status="partial", note="reopen citation mechanics (pend/apply/verified-false, stmt-basis parse rejection, incarnation ordering, D-185 reservation) are vectored post-6b; the independent-bases pair and the full battery are not. The clause's held stmt-kind invalidation VERIFYING has no §4.7 wire shape to verify against - a Gate-A audit finding.")

ob("f11-monotone-cause", 11, 405, 411, "monotone-cause trace",
   "The monotone-cause trace: recorded cause dies, survivor re-terminals; stmt-kind fork-discovery invalidation (D-179/D-185).",
   status="pending", note="blocked in part by the same §4.7 fork-discovery wire gap.")

ob("f11-deferral-schedules", 11, 411, 421, "schedules (an erase request against a live transfer",
   "Erase-vs-transfer deferral schedules and the journal-closure remedy (D-112/D-198).",
   ["erase-deferral-nonterminal-journal"],
   status="partial", note="the nonterminal-journal deferral is vectored; the closure remedy and adopted-erasure residual are not.")

ob("f11-incarnation-dedupe", 11, 421, 426, "consume-once dedupe",
   "Abort-Reopen-Done through a consume-once store; wrong-incarnation reopen; XferDone never reopens (D-140/D-148/D-157/D-189/D-200).",
   ["txn-internal-order-and-competing-terminals"],
   status="partial", note="incarnation ordering and competing terminals are vectored; the dedupe-store walkthrough and reopen-after-Done are not.")

ob("f11-merkle", 11, 425, 437, "Merkle-proof battery",
   "The Merkle-proof and self-describing-leaf batteries + validator equivalence (D-147/D-156/D-162).",
   ["merkle-promotion-path-valid","merkle-wrong-index-invalid","merkle-leftover-sibling-rejects"],
   status="partial", note="proof construction, wrong-index, sibling, and odd-promotion shapes are vectored; the 127/128 trees, erasure interplay, and validator-equivalence trace are not.")

ob("f11-mimport-shape", 11, 437, 443, "narrow-shape battery",
   "The mimport narrow-shape battery and mirror-matrix equalities (D-142/D-151).")

ob("f11-relations-policies", 11, 443, 448, "relation-principal vectors",
   "Relation-principal vectors, valid_from/erased/revival, B.2/B.3 literal hash reproduction, policy hash mismatch.",
   ["status-author-retract","status-revival-on-replacement-loss","judge-policy-hash-mismatch-pends"],
   status="partial", note="author relation, revival, and the policy-hash mismatch are vectored; cross-session denial is not. B.2/B.3 byte reproduction is executed as unit tests (reducer policies module + core scenario), not as a vector shape.")

ob("f12-iam", 12, 447, 453, "12 IAM",
   "IAM: outcome-member coverage, six allow paths across four shapes, audit lane, enumerated fold outcomes, multi-fault precedence.",
   V("shape") + ["scope-negatives-per-axis","second-generation-fail-closed","header-unknown-version-rejects"],
   status="partial", note="the four shapes, their deny paths, and the D-203 cheap-gap batch (five scope axes, no-grant, no-flow, op-unknown, unknown-version, lineage-gen fail-close) are vectored; the remaining per-outcome debt is 12/59, every entry an explicit Gate-B deferral (gate_b_deferrals + the core pin); the audit lane and multi-fault precedence ride it.")

ob("f13-framing", 13, 453, 456, "framing v2",
   "Framing v2: truncation vs quarantine, mid-file corruption, resync, nlen mismatch, Txn atomicity.",
   ["framing-bad-sync-quarantines","framing-final-frame-bad-crc-quarantines","framing-nlen-mismatch-quarantines","framing-three-frame-roundtrip","crash-inside-sync-truncates","crash-mid-final-frame-truncates","txn-internal-order-and-competing-terminals"])

ob("f13-journal-recovery", 13, 456, 462, "PendingXfer/XferDone/XferAbort",
   "Journal record recovery; zone-scoped frontier + checkpoint fence coverage (D-118).",
   ["txn-internal-order-and-competing-terminals"],
   status="partial", note="journal replay is vectored; the checkpoint/fence storage shapes are not.")

ob("f13-lock", 13, 462, 463, "cross-process lock",
   "Cross-process lock: the exclusive loser is read-only.",
   ["lock-exclusive-loser-read-only"])

ob("f13-erase-crash", 13, 463, 469, "erase crash matrix",
   "The erase crash matrix across all six states, survivor completeness, third-rotation survivor, queue serialization, Fence activation (D-73/D-89/D-92).",
   [n for n in names if n.startswith("erase-crash-")])

ob("f13-staged-membership", 13, 469, 473, "staged >128-membership ceremony",
   "The staged >128-membership ceremony; committed Fence intent recovery (D-97/D-106).",
   ["erase-crash-state2-fence-intent-recovered"],
   status="partial", note="Fence-intent recovery is vectored; the wrap-add ceremony is not.")

ob("f13-renewal-custody", 13, 473, 479, "latest-epoch renewal wraps",
   "Latest-epoch renewal wraps and predecessor-KEM custody predicates (D-116/D-125/D-133).")

ob("f13-orphaned-wrapadd", 13, 479, 481, "`storage-orphaned` on mismatched/unadopted activation",
   "storage-orphaned on unadopted activation; the effective wrap-add map as storage state (D-117).")

ob("f13-recipientset-deferral", 13, 481, 484, "`recipientset` constructibility",
   "recipientset constructibility at the 256 cap; the durable-attempt deferral (D-110/D-119/D-125).")

ob("f13-journal-identity", 13, 483, 487, "release_op journal",
   "release_op journal identity; renewal wrap supersession (D-123).")

ob("f13-erase-covers", 13, 487, 488, "erase-covers-index/view/checkpoint/backup",
   "Erasure covers index, views, checkpoints, and backups.")

ob("f14-migration", 14, 487, 491, "14 migration/projection",
   "Migration/projection: re-encapsulation byte equality, stamp completeness, the offline-confirmation fixture (umbrella App C #2).",
   ["reencapsulation-byte-equality","projection-release-stamp-complete","projection-stamp-incomplete-rejects","offline-expiry-confirmation-pending"],
   note="the offline-confirmation fixture is minted and deliberately PENDING (owner choice, D11 open).")

ob("s-tail", 0, 491, N, "---", "Trailing separator.", status="structural")

# ---- validation ----
errs = []
covered = [False] * N
for e in E:
    a, b = e["lines"]
    if not (1 <= a <= b <= N):
        errs.append(f"{e['id']}: bad range {a}..{b} (N={N})")
        continue
    for i in range(a - 1, b):
        covered[i] = True
    if e["quote"] not in section:
        errs.append(f"{e['id']}: quote not in section: {e['quote']!r}")
    else:
        # the quote must START within the claimed range
        pos = section.find(e["quote"])
        qline = section[:pos].count("\n") + 1
        # find ANY occurrence within range
        ok = False
        p = pos
        while p != -1:
            ql = section[:p].count("\n") + 1
            if a <= ql <= b:
                ok = True
                break
            p = section.find(e["quote"], p + 1)
        if not ok:
            errs.append(f"{e['id']}: quote at line {qline}, outside {a}..{b}")
    for v in e["vectors"]:
        if v not in names:
            errs.append(f"{e['id']}: unknown vector {v!r}")
holes = [i + 1 for i, c in enumerate(covered) if not c]
if holes:
    errs.append(f"unclaimed lines: {holes}")
if errs:
    print("ERRORS:\n" + "\n".join(errs))
    sys.exit(1)

doc = {
    "$comment": "The §13.3 obligation ledger, hand-transcribed at clause-cluster granularity and machine-enforced by core coverage::tests::obligations_ledger_is_sound: quotes are verbatim §13.3 substrings, line ranges (1-indexed within the section) must jointly cover the whole section, vector names must exist, and statuses must match the vector lists. Statuses: vectored (fully exercised), partial (some clauses exercised - the note says which), pending (unvectored debt), code-test (executed by named Rust unit tests, no vector shape), structural (non-obligation lines).",
    "executed_surfaces": [
        "rust-core",
        "rust-reducer",
        "browser-chromium",
        "storage-macos",
        "storage-linux",
        "storage-windows",
    ],
    "surface_annotation_note": "A vector's surfaces array declares §13.2 applicability; it is NOT execution. See execution-lanes-plan.md for the browser and per-OS storage lanes.",
    "section_lines": N,
    "gate_b_deferrals": {
        "$comment": "The owner's ratified scope line (D-203, 2026-07-14): the cheap single-op §10.4 negatives are closed pre-Gate-A; everything below is EXPLICITLY deferred to Gate B - a decision, not drift. The outcomes list must equal core coverage::UNCOVERED_10_4 exactly (test-enforced).",
        "outcomes": ["audit-unavailable", "cert-superseded", "class-ceiling", "class-excluded", "gen-first-op", "issuer-fork", "issuer-gap", "provenance-ceiling", "recovery-competition", "source-erased", "storage-io", "storage-orphaned"],
        "sagas": [
            "f7 ratify/snapshot cutoff-algebra + checkpoint-machine ceremonies (D-203)",
            "f9 issuer feed chains (gap/fork/ancestry/cross-carrier registry) (D-203)",
            "f10 generation machine (gen >= 2 histories; fail-closed as lineage-gen in P1 v1) (D-203)",
            "f11 transfer composites (mimport battery, monotone-cause, adopted-erasure, effect keys) (D-203)",
            "f13 checkpoint/fence storage shapes + renewal custody predicates (audit-added deferral — NOT among the four sagas the D-203 ruling names; recorded per review R8.5)"
        ],
    },
    "obligations": E,
}
out = os.path.join(ROOT, "coverage", "obligations-13-3.json")
with open(out, "w") as f:
    json.dump(doc, f, indent=1, ensure_ascii=False)
    f.write("\n")
st = {}
for e in E:
    st[e["status"]] = st.get(e["status"], 0) + 1
print(f"wrote {out}: {len(E)} obligations over {N} lines; statuses {st}")
