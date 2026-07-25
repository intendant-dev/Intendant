//! The sealed credentials document: one versioned JSON blob carrying
//! the App's whole identity — App ID, installation id, and the RS256
//! private key. Sealed as a single custody entry
//! (`key_custody::GITHUB_APP_ENTRY`) because the ids are discovery
//! topology on a PUBLIC-origin repo and splitting them across stores
//! invites split-brain: one entry, one status, one atomic replace.
//! Born in custody — this document never exists as a plaintext file,
//! and there is deliberately no env/file fallback lane for this class.

use serde::{Deserialize, Serialize};

/// The sealed-document version this build writes.
const SEALED_DOC_VERSION: u32 = 1;

fn default_doc_version() -> u32 {
    SEALED_DOC_VERSION
}

/// The GitHub App identity as sealed into custody.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GithubAppCredentials {
    /// Document version; bump only on a breaking shape change. A newer
    /// version fails the parse by name rather than being half-read.
    /// (The pending-install and `slug` fields below are additive within
    /// v1: completed documents stay byte-compatible with older builds,
    /// and a pending document read by an older build fails its parse by
    /// name — the named pause lane — never a mis-read.)
    #[serde(default = "default_doc_version")]
    pub(crate) v: u32,
    /// The App ID (numeric) or client ID — either is a valid JWT `iss`.
    pub(crate) app_id: String,
    /// The installation of the App on the watched org/repos. Absent =
    /// sealed-pending-install: the manifest ceremony created the App
    /// and sealed its key, but the owner has not installed it yet, so
    /// no installation token can be minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) installation_id: Option<u64>,
    /// The App's slug (the `github.com/apps/{slug}` segment), recorded
    /// by the manifest ceremony for the install link. Navigation only —
    /// never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) slug: Option<String>,
    /// The App's RS256 private key, PEM (PKCS#1 or PKCS#8), normalized
    /// at intake by [`GithubAppCredentials::validate`].
    pub(crate) private_key_pem: String,
}

impl GithubAppCredentials {
    /// Serialize for sealing. Material bytes — callers hand these to
    /// custody, never to disk or logs.
    pub(crate) fn sealed_bytes(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| format!("serialize credentials: {error}"))
    }

    /// Parse an unsealed custody document, failing by name on an
    /// unsupported version — never a half-read of a newer shape.
    pub(crate) fn from_sealed_bytes(bytes: &[u8]) -> Result<Self, String> {
        let parsed: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("sealed credentials document: {error}"))?;
        if parsed.v != SEALED_DOC_VERSION {
            return Err(format!(
                "sealed credentials document version {} is not supported by this build \
                 (expected {SEALED_DOC_VERSION})",
                parsed.v
            ));
        }
        Ok(parsed)
    }

    /// Intake validation: non-empty identity fields and a private key
    /// that actually parses as an RSA signing key. Trims stray
    /// whitespace so a pasted PEM round-trips byte-stable. An absent
    /// installation_id is valid (sealed-pending-install); a present one
    /// must be positive.
    pub(crate) fn validate(&mut self) -> Result<(), String> {
        self.app_id = self.app_id.trim().to_string();
        if self.app_id.is_empty() {
            return Err("app_id must not be empty".to_string());
        }
        if self.installation_id == Some(0) {
            return Err("installation_id must be a positive integer".to_string());
        }
        if let Some(slug) = self.slug.as_mut() {
            *slug = slug.trim().to_string();
            if slug.is_empty() {
                self.slug = None;
            }
        }
        let trimmed = self.private_key_pem.trim();
        if trimmed.is_empty() {
            return Err("private_key_pem must not be empty".to_string());
        }
        self.private_key_pem = format!("{trimmed}\n");
        super::client::rsa_key_from_pem(&self.private_key_pem).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_document_round_trips_and_pins_its_version() {
        let doc = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "123456".to_string(),
            installation_id: Some(987),
            slug: None,
            private_key_pem: "irrelevant".to_string(),
        };
        let bytes = doc.sealed_bytes().unwrap();
        let back = GithubAppCredentials::from_sealed_bytes(&bytes).unwrap();
        assert_eq!(back.app_id, "123456");
        assert_eq!(back.installation_id, Some(987));
        assert_eq!(back.slug, None);
    }

    /// A completed document serializes without the pending-only fields,
    /// staying byte-compatible with pre-slug builds (which reject
    /// unknown fields never — this struct has no `deny_unknown_fields`),
    /// and a legacy document (no slug, numeric installation_id) parses
    /// into the evolved shape unchanged.
    #[test]
    fn completed_documents_stay_compatible_across_the_shape_evolution() {
        let doc = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "123456".to_string(),
            installation_id: Some(987),
            slug: None,
            private_key_pem: "irrelevant".to_string(),
        };
        let bytes = doc.sealed_bytes().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let map = json.as_object().unwrap();
        assert!(!map.contains_key("slug"), "absent slug must not serialize");
        assert_eq!(map["installation_id"], 987, "completed id stays a plain number");

        let legacy = serde_json::json!({
            "v": 1,
            "app_id": "77",
            "installation_id": 42,
            "private_key_pem": "x",
        })
        .to_string();
        let back = GithubAppCredentials::from_sealed_bytes(legacy.as_bytes()).unwrap();
        assert_eq!(back.installation_id, Some(42));
        assert_eq!(back.slug, None);
    }

    /// The pending shape (no installation_id) round-trips in this build;
    /// the same bytes fed to the pre-evolution shape fail by the named
    /// missing field — the fail-closed lane older builds land in.
    #[test]
    fn pending_documents_round_trip_here_and_fail_closed_on_old_shapes() {
        let doc = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "123456".to_string(),
            installation_id: None,
            slug: Some("intendant-example".to_string()),
            private_key_pem: "irrelevant".to_string(),
        };
        let bytes = doc.sealed_bytes().unwrap();
        let back = GithubAppCredentials::from_sealed_bytes(&bytes).unwrap();
        assert_eq!(back.installation_id, None);
        assert_eq!(back.slug.as_deref(), Some("intendant-example"));

        // The pre-evolution document shape, verbatim from the v1 struct
        // this build replaced: installation_id was a required u64.
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct LegacyShape {
            v: u32,
            app_id: String,
            installation_id: u64,
            private_key_pem: String,
        }
        let error = serde_json::from_slice::<LegacyShape>(&bytes).unwrap_err();
        assert!(
            error.to_string().contains("installation_id"),
            "old builds must fail the pending shape by the named field, got: {error}"
        );
    }

    #[test]
    fn newer_document_versions_fail_by_name() {
        let bytes = serde_json::json!({
            "v": 2,
            "app_id": "1",
            "installation_id": 1,
            "private_key_pem": "x",
        })
        .to_string();
        let error = GithubAppCredentials::from_sealed_bytes(bytes.as_bytes()).unwrap_err();
        assert!(error.contains("version 2"), "unexpected error: {error}");
    }

    #[test]
    fn validate_rejects_empty_identity_fields() {
        let mut doc = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "  ".to_string(),
            installation_id: Some(1),
            slug: None,
            private_key_pem: "x".to_string(),
        };
        assert!(doc.validate().unwrap_err().contains("app_id"));
        let mut doc = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "1".to_string(),
            installation_id: Some(0),
            slug: None,
            private_key_pem: "x".to_string(),
        };
        assert!(doc.validate().unwrap_err().contains("installation_id"));
    }

    #[test]
    fn validate_accepts_a_real_rsa_key_and_normalizes_whitespace() {
        let mut doc = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "123".to_string(),
            installation_id: Some(7),
            slug: Some("  intendant-example  ".to_string()),
            private_key_pem: format!("\n  {}  \n\n", super::super::client::test_rsa_pem()),
        };
        doc.validate().expect("test key must validate");
        assert!(doc.private_key_pem.starts_with("-----BEGIN"));
        assert!(doc.private_key_pem.ends_with("-----\n"));
        assert_eq!(doc.slug.as_deref(), Some("intendant-example"));

        // The pending shape validates too: absent installation_id is a
        // legal state, and a whitespace slug normalizes to absent.
        let mut pending = GithubAppCredentials {
            v: SEALED_DOC_VERSION,
            app_id: "123".to_string(),
            installation_id: None,
            slug: Some("   ".to_string()),
            private_key_pem: super::super::client::test_rsa_pem().to_string(),
        };
        pending.validate().expect("pending shape must validate");
        assert_eq!(pending.slug, None);
    }
}
