//! Shared integration status: what the last real GitHub exchange said.
//! Presence (the custody blob) and the watch config are read fresh per
//! status call — only the *outcome* of actual API exchanges is cached
//! here, so a status poll never touches the keystore and never claims
//! knowledge the daemon doesn't have. Core operations live on the
//! struct so tests drive local instances; the thin free functions are
//! the only thing touching the process global (the `background_tasks`
//! testability split).

use std::sync::{Mutex, OnceLock};

/// The named states the surface renders. `RateLimited` is served under
/// the `unreachable` wire label (transient, self-healing) with the rate
/// detail carried in `detail` — the vocabulary stays the ruled set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckResult {
    Valid,
    Unreachable(String),
    Denied(String),
    RateLimited(String),
}

#[derive(Debug, Clone)]
pub(crate) struct CheckOutcome {
    pub(crate) at_unix_ms: u64,
    pub(crate) result: CheckResult,
}

/// Process-wide integration runtime: the API base every client this
/// daemon builds targets, plus the latest exchange outcome.
pub(crate) struct GithubIntegrationRuntime {
    api_base: String,
    last: Mutex<Option<CheckOutcome>>,
}

impl GithubIntegrationRuntime {
    pub(crate) fn new(api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
            last: Mutex::new(None),
        }
    }

    pub(crate) fn api_base(&self) -> &str {
        &self.api_base
    }

    pub(crate) fn record(&self, result: CheckResult) {
        *self.last.lock().expect("github status poisoned") = Some(CheckOutcome {
            at_unix_ms: now_ms(),
            result,
        });
    }

    /// Forget cached outcomes (the Remove gesture): an unconfigured
    /// integration has no last exchange to report.
    pub(crate) fn clear(&self) {
        *self.last.lock().expect("github status poisoned") = None;
    }

    pub(crate) fn last(&self) -> Option<CheckOutcome> {
        self.last.lock().expect("github status poisoned").clone()
    }

    /// Map an [`super::client::ApiError`] onto the recorded outcome.
    pub(crate) fn record_error(&self, error: &super::client::ApiError) {
        use super::client::ApiError;
        self.record(match error {
            ApiError::Unreachable(message) => CheckResult::Unreachable(message.clone()),
            ApiError::Denied(message) => CheckResult::Denied(message.clone()),
            ApiError::RateLimited { message, .. } => CheckResult::RateLimited(message.clone()),
        });
    }
}

/// The wire status label for a presence + last-outcome pair. The ruled
/// vocabulary: `unconfigured` (no custody entry — nothing runs),
/// `configured` (entry present, no exchange yet this boot), `valid`,
/// `unreachable`, `denied`.
pub(crate) fn status_label(present: bool, last: Option<&CheckOutcome>) -> &'static str {
    if !present {
        return "unconfigured";
    }
    match last.map(|outcome| &outcome.result) {
        None => "configured",
        Some(CheckResult::Valid) => "valid",
        Some(CheckResult::Unreachable(_)) | Some(CheckResult::RateLimited(_)) => "unreachable",
        Some(CheckResult::Denied(_)) => "denied",
    }
}

pub(crate) fn status_detail(last: Option<&CheckOutcome>) -> Option<String> {
    match last.map(|outcome| &outcome.result) {
        Some(CheckResult::Unreachable(message)) | Some(CheckResult::Denied(message)) => {
            Some(message.clone())
        }
        Some(CheckResult::RateLimited(message)) => Some(format!("rate limited: {message}")),
        Some(CheckResult::Valid) | None => None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The process global — transport edges only; cores take `&runtime`.
pub(crate) fn global() -> &'static GithubIntegrationRuntime {
    static RUNTIME: OnceLock<GithubIntegrationRuntime> = OnceLock::new();
    RUNTIME.get_or_init(|| GithubIntegrationRuntime::new(super::client::GITHUB_API_BASE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_derive_from_presence_and_last_outcome() {
        assert_eq!(status_label(false, None), "unconfigured");
        assert_eq!(status_label(true, None), "configured");
        let valid = CheckOutcome {
            at_unix_ms: 1,
            result: CheckResult::Valid,
        };
        assert_eq!(status_label(true, Some(&valid)), "valid");
        let denied = CheckOutcome {
            at_unix_ms: 1,
            result: CheckResult::Denied("bad credentials".to_string()),
        };
        assert_eq!(status_label(true, Some(&denied)), "denied");
        assert_eq!(status_detail(Some(&denied)).unwrap(), "bad credentials");
        let limited = CheckOutcome {
            at_unix_ms: 1,
            result: CheckResult::RateLimited("slow down".to_string()),
        };
        assert_eq!(status_label(true, Some(&limited)), "unreachable");
        assert!(status_detail(Some(&limited))
            .unwrap()
            .starts_with("rate limited"));
        // An unconfigured integration reports nothing, even with a stale
        // outcome someone forgot to clear — presence wins.
        assert_eq!(status_label(false, Some(&denied)), "unconfigured");
    }

    #[test]
    fn runtime_records_and_clears() {
        let runtime = GithubIntegrationRuntime::new("http://fixture");
        assert!(runtime.last().is_none());
        runtime.record(CheckResult::Valid);
        assert_eq!(runtime.last().unwrap().result, CheckResult::Valid);
        runtime.clear();
        assert!(runtime.last().is_none());
    }
}
