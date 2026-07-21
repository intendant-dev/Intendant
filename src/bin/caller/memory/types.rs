//! Service-facing types for the P1 Memory service.

use serde::Serialize;

/// Hex of a 32-byte identifier (claim ids are op hashes).
pub(crate) fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Arguments for a `propose` (claim authoring). The service maps
/// `kind`/`sensitivity` onto the kernel's closed vocabularies and
/// rejects unknown words — never a silently coerced value. One
/// documented SERVICE default (stated in every schema and help text,
/// and echoed back on the view): a surface that omits `sensitivity`
/// proposes at `private`. The kernel itself never defaults.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct ProposeArgs {
    pub kind: String,
    pub statement: String,
    /// Writer's sensitivity claim (a claim, never export authority).
    #[serde(default = "default_sensitivity")]
    pub sensitivity: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

fn default_sensitivity() -> String {
    "private".into()
}

/// Judgment `reason` intake cap (ruling R3, 2026-07-20 — the F2
/// annotation-cap idiom): the kernel's `? reason: text` is unbounded;
/// this DTO-edge bound rejects loudly, never truncates.
pub(crate) const MAX_REASON_CHARS: usize = 2000;

/// The verdict vocabulary this build MINTS — the v1 subset of the
/// kernel's closed §11.1 set (retract stays author/agent-lane
/// machinery surfaced read-only; raise_class/declassify are
/// fail-closed classification arms; pins are fail-closed at the
/// stamped kernel). Single source: the service's judge match, its
/// rejection text, and the Explorer's curation buttons (parity test)
/// all derive from this list.
pub(crate) const MINTED_VERDICTS: &[&str] = &["accept", "dispute", "retire", "supersede"];

/// Arguments for a judgment (`judge` — the owner curation lane).
/// `verdict` is the v1-minted subset of the kernel's closed §11.1
/// vocabulary: `accept`, `dispute`, `retire`, `supersede` (retract is
/// author/agent-lane machinery surfaced read-only in v1;
/// `raise_class`/`declassify` are fail-closed classification arms;
/// pins are fail-closed at the stamped kernel boundary). Unknown or
/// unminted words reject with the allowed set — never coerced.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgeArgs {
    /// One of: accept, dispute, retire, supersede.
    pub verdict: String,
    /// Target claim id prefix (≥ 8 hex chars of the claim op hash).
    pub id: String,
    /// Optional rationale, recorded verbatim in the sealed op
    /// (≤ 2000 chars). Rendered as quoted data, never instructions.
    #[serde(default)]
    pub reason: Option<String>,
    /// `supersede` only: the replacement claim's id prefix. The fold
    /// holds supersession only while the replacement's derived status
    /// is `accepted` (§11.2 rule 2) — accept it first.
    #[serde(default)]
    pub replacement: Option<String>,
}

/// Bounded search arguments (§6.5: bounded retrieval, candidates
/// excluded by default and results always status-labeled).
#[derive(Debug, Clone)]
pub(crate) struct SearchArgs {
    pub query: String,
    pub limit: usize,
    pub include_candidates: bool,
}

impl Default for SearchArgs {
    fn default() -> Self {
        SearchArgs {
            query: String::new(),
            limit: 10,
            include_candidates: false,
        }
    }
}

/// Who authored a claim, as the daemon's gates attributed it. This is
/// Memory's **own versioned provenance shape** (`v` names the shape
/// revision), mapped from `access::actor::ActorBinding` at the tenant
/// edge — per the seam contract the raw seam type is never serialized
/// into records; these fields are the record, and they evolve
/// additively. Unlike the claim body's `session`/`project` fields
/// (writer-stated context claims), every field here is gate-derived:
/// never parsed from tool arguments, request bodies, or query echoes.
/// (`Deserialize` exists only for the event-lane plumbing —
/// `OutboundEvent` derives it wholesale for the peer upcaster, which
/// drops memory events on arrival; nothing durable reads this back.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ClaimProvenance {
    /// Provenance shape revision (this build writes 1).
    pub v: u32,
    /// Actor class, snake_case (`agent_session`, `dashboard`,
    /// `local_process`, `peer`, `unattributed`). An unauthenticated or
    /// unstated caller is recorded EXPLICITLY as `unattributed` —
    /// fail-closed, never a defaulted principal.
    pub actor: String,
    /// The IAM principal exactly as the gate named it (verbatim — the
    /// P1 exit criterion asserts recorded actor == token-bound
    /// principal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The supervised session the write acted as — bound by the gate
    /// through token possession, never echoed from request fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl ClaimProvenance {
    /// Map the shared seam type into Memory's own record shape at the
    /// tenant edge (the only place the seam type is consumed).
    pub(crate) fn from_binding(binding: &crate::access::actor::ActorBinding) -> Self {
        ClaimProvenance {
            v: 1,
            actor: binding.kind.as_str().to_string(),
            principal: binding.principal_id.clone(),
            session: binding.session_id.clone(),
        }
    }
}

/// One judgment as history surfaces render it: who judged what, when
/// — the provenance is the product. Judgments are quoted DATA (the
/// `reason` included), never instructions. Every §11.2-recorded
/// judgment surfaces here, counting or not — the derived `status` on
/// the claim is the truth about what counted.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct JudgmentView {
    /// The judgment id: hex of the accepted `m.judge` op hash.
    pub id: String,
    /// accept / dispute / retire / supersede (plus, on recovered
    /// planes, any kernel-legal verdict another writer sealed —
    /// rendered verbatim, e.g. `retract`).
    pub verdict: String,
    /// The judged claim's id (hex op hash).
    pub target: String,
    /// `supersede` only: the replacement claim's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at_ms: u64,
    /// Who judged, in the DURABLE identity vocabulary (ruling R2,
    /// 2026-07-20): actor is `owner` / `session` / `peer` /
    /// `unattributed` — never a dashboard-vs-ctl surface distinction,
    /// which the closed `mjudge` CDDL cannot carry across a restart.
    /// (Claim `proposed_by` keeps its own richer live vocabulary.)
    pub judged_by: ClaimProvenance,
    /// The status policy the judgment cited (stamped server-side from
    /// the target space's binding — never caller input).
    pub policy: String,
}

/// A provenance-labeled claim view. This is DATA for surfaces to
/// render as quoted content — never instructions (§6.5).
/// (`Deserialize`: event-lane plumbing only, as on [`ClaimProvenance`].)
/// `pub` (not `pub(crate)`) because it rides the `pub` AppEvent/
/// OutboundEvent enums — same posture as `agenda::AgendaItem`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ClaimView {
    /// The claim id: hex of the accepted `m.claim` operation hash.
    pub id: String,
    pub kind: String,
    pub statement: String,
    pub sensitivity: String,
    /// Derived by the reducer's §11.2 status fold at read time
    /// (`candidate` / `accepted` / `disputed` / `superseded` /
    /// `retired`) — status is a derived view, never a mutable field.
    pub status: String,
    /// Writer-stated session context (a claim about the claim; when
    /// the writer states none, the service fills it from the
    /// gate-bound session). Attribution lives in `proposed_by`.
    pub session: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub labels: Vec<String>,
    pub created_ms: u64,
    /// Gate-derived authorship — see [`ClaimProvenance`].
    pub proposed_by: ClaimProvenance,
    /// Effective storage mode: `"durable"` or `"ephemeral"`.
    pub durability: String,
    /// Judgment history, oldest first. Populated on single-claim
    /// views (read, judge returns, the change event); search results
    /// stay lean (empty) — bounded retrieval per §6.5.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judgments: Vec<JudgmentView>,
}

/// Service errors. Kernel verdicts surface their named
/// outcome/disposition pair verbatim (D-203 §C.2).
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum MemoryError {
    #[error("rejected: {outcome} ({disposition})")]
    Rejected {
        outcome: &'static str,
        disposition: &'static str,
    },
    #[error("pending: {outcome} ({disposition})")]
    Pending {
        outcome: &'static str,
        disposition: &'static str,
    },
    /// The vendored engine met something outside its implemented
    /// registry — surfaced, never papered over. Unreachable for the
    /// shapes this service mints; reaching it is a bug report.
    #[error("kernel boundary: {0}")]
    Unimplemented(String),
    #[error("unknown {what}: {got:?} (expected one of {allowed})")]
    Vocabulary {
        what: &'static str,
        got: String,
        allowed: String,
    },
    /// The tenant-edge authorization outcome (named, fail-closed —
    /// §C.2 discipline applies to service-side denials too): the
    /// resolved actor class may not perform this write verb. Ring-2
    /// writers (supervised agent sessions, peers, unattributed
    /// callers) are propose-only; every other write verb needs an
    /// owner surface.
    #[error("rejected: actor-not-permitted ({actor} may not {verb}; owner surface required)")]
    NotPermitted {
        verb: &'static str,
        actor: &'static str,
    },
    #[error("no claim matches id prefix {0:?}")]
    NotFound(String),
    #[error("ambiguous id prefix {0:?} (matches {1} claims)")]
    Ambiguous(String, usize),
    #[error("{0}")]
    InvalidArg(String),
}
