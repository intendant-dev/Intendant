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
    pub session: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub labels: Vec<String>,
    pub created_ms: u64,
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
    #[error("no claim matches id prefix {0:?}")]
    NotFound(String),
    #[error("ambiguous id prefix {0:?} (matches {1} claims)")]
    Ambiguous(String, usize),
    #[error("{0}")]
    InvalidArg(String),
}
