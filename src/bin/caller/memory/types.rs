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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ClaimProvenance {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The supervised session the write acted as — bound by the gate
    /// through token possession, never echoed from request fields.
    #[serde(skip_serializing_if = "Option::is_none")]
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

/// A provenance-labeled claim view. This is DATA for surfaces to
/// render as quoted content — never instructions (§6.5).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClaimView {
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
    /// Always `"ephemeral"` in this build (the P1 write bar): the
    /// claim does not survive a daemon restart.
    pub durability: &'static str,
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
