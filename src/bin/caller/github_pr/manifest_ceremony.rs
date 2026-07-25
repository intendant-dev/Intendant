//! The one-click connect ceremony (Track GC): daemon-minted single-use
//! `state` for GitHub's App Manifest flow, plus the manifest the
//! browser form-POSTs to GitHub.
//!
//! The ceremony is **single-flight by replacement**: one pending slot,
//! and a new manifest-start overwrites (invalidates) the previous one.
//! This deliberately diverges from `auth_ceremony`'s refuse-second
//! discipline because the resource here is a slot, not a PTY — there is
//! no spawned process to orphan, so the newest owner gesture simply
//! wins. The state token is the callback's **entire** authorization
//! (the callback route is authority-free by necessity — GitHub's
//! redirect carries no daemon credential): 32 random bytes, stored
//! hashed, compared by digest in constant time, burned atomically
//! **before** the conversion call so a replayed or raced callback can
//! never reach GitHub. A conversion that then fails ends the ceremony —
//! fail-closed over convenient; the owner restarts it.
//!
//! Core operations live on [`ManifestCeremonySlot`] so tests drive
//! local instances; the thin free functions at the bottom are the only
//! thing touching the process global (the status.rs testability split).

use base64::Engine as _;
use ring::rand::SecureRandom as _;

/// How long a minted state stays redeemable. Inside GitHub's one-hour
/// code window, generous for a human reading GitHub's create page,
/// short enough that a forgotten tab is not a standing door.
pub(crate) const MANIFEST_STATE_TTL_MS: u64 = 30 * 60 * 1000;

/// The uniform refusal: unknown, replayed, expired, and absent states
/// are deliberately indistinguishable to the caller.
pub(crate) const STATE_REFUSED: &str =
    "this connect link is invalid, already used, or expired — start the ceremony again \
     from the dashboard";

/// One pending ceremony, minted at manifest-start and burned at the
/// callback. Everything here is non-secret except `state_hash` (a
/// digest, not the token).
#[derive(Debug, Clone)]
pub(crate) struct PendingManifest {
    /// SHA-256 of the state token, hex — the plaintext never rests.
    state_hash: String,
    pub(crate) expires_at_unix_ms: u64,
    /// The validated origin (`scheme://authority`) the ceremony started
    /// from; the redirect returns here and the callback page links back.
    pub(crate) origin: String,
    /// The principal whose CredentialsManage gesture opened the
    /// ceremony. The authority-free callback merely completes that
    /// authorized act, so this is the custody-audit actor at seal time
    /// (the general attribution rule for authority-free completion
    /// legs).
    pub(crate) starter_principal: String,
}

fn token_hash(token: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time byte equality (length gate, then an OR-fold over XOR).
/// ring deprecated its public helper with no side-channel promises, so
/// the ruled primitive lives here; it runs on digests anyway
/// (compare-after-hash), so this is belt and braces.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_state() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "generate ceremony state".to_string())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The single-flight pending slot. One per daemon process.
#[derive(Default)]
pub(crate) struct ManifestCeremonySlot {
    pending: std::sync::Mutex<Option<PendingManifest>>,
}

impl ManifestCeremonySlot {
    /// Begin a ceremony, replacing (invalidating) any pending one.
    /// Returns the plaintext state token — the only time it exists
    /// outside the browser's form action.
    pub(crate) fn begin(
        &self,
        origin: String,
        starter_principal: String,
        now_ms: u64,
    ) -> Result<String, String> {
        let token = random_state()?;
        let pending = PendingManifest {
            state_hash: token_hash(&token),
            expires_at_unix_ms: now_ms.saturating_add(MANIFEST_STATE_TTL_MS),
            origin,
            starter_principal,
        };
        *self.pending.lock().unwrap_or_else(|p| p.into_inner()) = Some(pending);
        Ok(token)
    }

    /// Atomically burn the pending ceremony against a presented state.
    /// The slot is taken **before** any comparison completes, so two
    /// raced callbacks cannot both pass, and a mismatch restores
    /// nothing (a wrong guess costs the attacker the owner's pending
    /// ceremony, never a second try at it — and costs the owner one
    /// restart). Unknown, replayed, expired, and absent states are
    /// indistinguishable ([`STATE_REFUSED`]).
    pub(crate) fn consume(
        &self,
        presented_state: &str,
        now_ms: u64,
    ) -> Result<PendingManifest, String> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| STATE_REFUSED.to_string())?;
        let presented_hash = token_hash(presented_state);
        if !ct_eq(presented_hash.as_bytes(), pending.state_hash.as_bytes()) {
            return Err(STATE_REFUSED.to_string());
        }
        if pending.expires_at_unix_ms <= now_ms {
            return Err(STATE_REFUSED.to_string());
        }
        Ok(pending)
    }

    /// Whether a live (unexpired) ceremony is pending. Test-only today;
    /// a status surface that wants it re-promotes it deliberately.
    #[cfg(test)]
    pub(crate) fn active(&self, now_ms: u64) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|p| p.expires_at_unix_ms > now_ms)
    }
}

// ---------------------------------------------------------------------------
// Manifest composition (pure; unit-tested)
// ---------------------------------------------------------------------------

/// GitHub caps App names at 34 characters; names are globally unique.
const APP_NAME_MAX: usize = 34;

/// `Intendant (<host>)`, truncated to GitHub's 34-char cap with the
/// closing paren kept. Collisions surface on GitHub's own create page,
/// where the name is owner-editable.
pub(crate) fn manifest_app_name(hostname: &str) -> String {
    const PREFIX: &str = "Intendant (";
    const SUFFIX: &str = ")";
    let budget = APP_NAME_MAX - PREFIX.len() - SUFFIX.len();
    let host: String = hostname.trim().chars().take(budget).collect();
    if host.is_empty() {
        return "Intendant".to_string();
    }
    format!("{PREFIX}{host}{SUFFIX}")
}

/// The GitHub form target for a ceremony: the personal or org create
/// page, with the state token in the query (GitHub echoes it beside
/// `code` on the redirect). The org handle is path-escaped — it came
/// from owner input, not from a trusted vocabulary.
pub(crate) fn manifest_form_action(target_org: Option<&str>, state: &str) -> String {
    let encoded_state: String = url::form_urlencoded::byte_serialize(state.as_bytes()).collect();
    match target_org {
        Some(org) => {
            let encoded_org: String =
                url::form_urlencoded::byte_serialize(org.as_bytes()).collect();
            format!(
                "https://github.com/organizations/{encoded_org}/settings/apps/new?state={encoded_state}"
            )
        }
        None => format!("https://github.com/settings/apps/new?state={encoded_state}"),
    }
}

/// The manifest document the browser form-POSTs. Read-only permissions
/// exactly matching the scanner's needs, a private App, and **no
/// webhook** — `hook_attributes` is omitted entirely (polling is the
/// only lane this track ships).
pub(crate) fn manifest_document(origin: &str, hostname: &str) -> serde_json::Value {
    serde_json::json!({
        "name": manifest_app_name(hostname),
        "url": "https://github.com/intendant-dev/Intendant",
        "redirect_url": format!("{origin}{CALLBACK_PATH}"),
        "public": false,
        "default_permissions": {
            "metadata": "read",
            "pull_requests": "read",
            "checks": "read",
        },
    })
}

/// The callback route's path — declared here so the route row, the
/// carve-out predicate, and the manifest's redirect_url all read one
/// constant.
pub(crate) const CALLBACK_PATH: &str = "/api/integrations/github/callback";

/// A manifest-flow `code` as GitHub mints it: short, URL-safe. Anything
/// else never reaches the conversion URL builder.
pub(crate) fn code_shape_ok(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 128
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ---------------------------------------------------------------------------
// Callback doorbell limiter (the enroll limiter's shape: per-source
// sliding window first, global backstop second; the burned slot is the
// real gate — this keeps grinding out of the logs)
// ---------------------------------------------------------------------------

const CALLBACK_RATE_WINDOW_MS: u64 = 60_000;
const CALLBACK_RATE_GLOBAL_MAX: usize = 60;
const CALLBACK_RATE_PER_SOURCE_MAX: usize = 10;

#[derive(Default)]
pub(crate) struct CallbackRateLimiter {
    global: std::collections::VecDeque<u64>,
    per_source: std::collections::HashMap<String, std::collections::VecDeque<u64>>,
}

impl CallbackRateLimiter {
    pub(crate) fn allow(&mut self, source: &str, now_ms: u64) -> bool {
        let prune = |queue: &mut std::collections::VecDeque<u64>| {
            while let Some(at_ms) = queue.front().copied() {
                if now_ms.saturating_sub(at_ms) < CALLBACK_RATE_WINDOW_MS {
                    break;
                }
                queue.pop_front();
            }
        };
        self.per_source.retain(|_, queue| {
            prune(queue);
            !queue.is_empty()
        });
        prune(&mut self.global);
        if self.global.len() >= CALLBACK_RATE_GLOBAL_MAX {
            return false;
        }
        let queue = self.per_source.entry(source.to_string()).or_default();
        if queue.len() >= CALLBACK_RATE_PER_SOURCE_MAX {
            return false;
        }
        queue.push_back(now_ms);
        self.global.push_back(now_ms);
        true
    }
}

// ---------------------------------------------------------------------------
// Process globals — transport edges only; cores take the structs.
// ---------------------------------------------------------------------------

pub(crate) fn slot() -> &'static ManifestCeremonySlot {
    static SLOT: std::sync::OnceLock<ManifestCeremonySlot> = std::sync::OnceLock::new();
    SLOT.get_or_init(ManifestCeremonySlot::default)
}

pub(crate) fn callback_rate_ok(source: &str, now_ms: u64) -> bool {
    static LIMITER: std::sync::OnceLock<std::sync::Mutex<CallbackRateLimiter>> =
        std::sync::OnceLock::new();
    LIMITER
        .get_or_init(|| std::sync::Mutex::new(CallbackRateLimiter::default()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .allow(source, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn begin(slot: &ManifestCeremonySlot) -> String {
        slot.begin(
            "http://127.0.0.1:8765".to_string(),
            "principal:test".to_string(),
            NOW,
        )
        .expect("begin")
    }

    #[test]
    fn state_burns_once_and_uniformly_refuses_replay_and_garbage() {
        let slot = ManifestCeremonySlot::default();
        let token = begin(&slot);
        assert!(slot.active(NOW));
        let pending = slot.consume(&token, NOW + 1).expect("first consume");
        assert_eq!(pending.starter_principal, "principal:test");
        assert_eq!(pending.origin, "http://127.0.0.1:8765");
        // Replay: the slot is empty — same refusal as a garbage state.
        let replay = slot.consume(&token, NOW + 2).unwrap_err();
        let garbage = ManifestCeremonySlot::default()
            .consume("nonsense", NOW)
            .unwrap_err();
        assert_eq!(replay, STATE_REFUSED);
        assert_eq!(garbage, STATE_REFUSED);
        assert!(!slot.active(NOW));
    }

    #[test]
    fn wrong_state_burns_the_pending_ceremony_without_matching() {
        let slot = ManifestCeremonySlot::default();
        let token = begin(&slot);
        assert_eq!(slot.consume("wrong-guess", NOW).unwrap_err(), STATE_REFUSED);
        // The guess consumed the slot: the honest token is now refused
        // too (fail-closed; the owner restarts the ceremony).
        assert_eq!(slot.consume(&token, NOW).unwrap_err(), STATE_REFUSED);
    }

    #[test]
    fn expiry_refuses_and_replacement_invalidates_the_first_ceremony() {
        let slot = ManifestCeremonySlot::default();
        let token = begin(&slot);
        assert_eq!(
            slot.consume(&token, NOW + MANIFEST_STATE_TTL_MS)
                .unwrap_err(),
            STATE_REFUSED,
            "an expired state must refuse uniformly"
        );
        let _first = begin(&slot);
        let second = slot
            .begin(
                "https://box.example:8443".to_string(),
                "principal:second".to_string(),
                NOW,
            )
            .expect("second begin");
        // Replacement single-flight: only the live (second) token
        // redeems, and it carries the second ceremony's parameters.
        let pending = slot.consume(&second, NOW + 1).expect("live token redeems");
        assert_eq!(pending.origin, "https://box.example:8443");
        assert_eq!(pending.starter_principal, "principal:second");

        // The voided first token: refused — and per burn-on-mismatch,
        // the attempt costs whatever ceremony is pending at the time
        // (here: none), never a retry.
        let first_again = begin(&slot);
        assert_eq!(
            slot.consume(&first_again[..first_again.len() - 1], NOW + 1)
                .unwrap_err(),
            STATE_REFUSED,
            "a near-miss token is refused"
        );
        assert_eq!(
            slot.consume(&first_again, NOW + 1).unwrap_err(),
            STATE_REFUSED,
            "and the mismatch burned the pending ceremony (fail-closed)"
        );
    }

    #[test]
    fn app_name_fits_githubs_cap_and_keeps_the_paren() {
        assert_eq!(manifest_app_name("macbook-a"), "Intendant (macbook-a)");
        let long = manifest_app_name("a-very-long-hostname-that-exceeds-github-limits");
        assert!(long.chars().count() <= 34, "{long:?} must fit 34 chars");
        assert!(long.ends_with(')'), "{long:?} must keep the closing paren");
        assert_eq!(manifest_app_name("   "), "Intendant");
    }

    #[test]
    fn form_action_targets_personal_or_org_and_escapes_both_inputs() {
        let personal = manifest_form_action(None, "tok_en-1");
        assert_eq!(
            personal,
            "https://github.com/settings/apps/new?state=tok_en-1"
        );
        let org = manifest_form_action(Some("example-org"), "t");
        assert_eq!(
            org,
            "https://github.com/organizations/example-org/settings/apps/new?state=t"
        );
        let hostile = manifest_form_action(Some("a/b?c"), "s&t");
        assert!(!hostile.contains("a/b?c"), "org handle must be escaped");
        assert!(!hostile.contains("s&t"), "state must be escaped");
    }

    #[test]
    fn manifest_document_is_private_readonly_and_webhookless() {
        let doc = manifest_document("http://127.0.0.1:8765", "box");
        assert_eq!(doc["public"], false);
        assert_eq!(
            doc["redirect_url"],
            "http://127.0.0.1:8765/api/integrations/github/callback"
        );
        assert_eq!(doc["default_permissions"]["metadata"], "read");
        assert_eq!(doc["default_permissions"]["pull_requests"], "read");
        assert_eq!(doc["default_permissions"]["checks"], "read");
        assert_eq!(doc["default_permissions"].as_object().unwrap().len(), 3);
        let map = doc.as_object().unwrap();
        assert!(
            !map.contains_key("hook_attributes"),
            "no webhook: hook_attributes stays omitted"
        );
        assert!(!map.contains_key("default_events"));
    }

    #[test]
    fn code_shape_gate_admits_github_codes_and_refuses_url_metacharacters() {
        assert!(code_shape_ok("a1B2-c3_d4.e5"));
        for bad in ["", "a/b", "a?b", "a#b", "a b", &"x".repeat(129)] {
            assert!(!code_shape_ok(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn limiter_bounds_per_source_then_globally_and_slides_its_window() {
        let mut limiter = CallbackRateLimiter::default();
        for i in 0..CALLBACK_RATE_PER_SOURCE_MAX {
            assert!(limiter.allow("src-a", NOW + i as u64), "call {i} allowed");
        }
        assert!(!limiter.allow("src-a", NOW + 20), "per-source cap holds");
        assert!(limiter.allow("src-b", NOW + 21), "other sources unaffected");
        assert!(
            limiter.allow("src-a", NOW + CALLBACK_RATE_WINDOW_MS + 25),
            "window slides"
        );
    }
}
