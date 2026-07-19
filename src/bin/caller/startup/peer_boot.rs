//! Peer-federation startup: the advertised auth requirements and
//! URLs this daemon publishes, and peer-registry construction and
//! hydration from config.

use crate::access;
use crate::error::CallerError;
use crate::peer;
use crate::project::{self, Project};
use crate::CliFlags;
use std::path::{Path, PathBuf};

/// Build the [`peer::AuthRequirements`] this daemon advertises in
/// its own Agent Card from the project's `[server.auth]` config and
/// the access cert dir.
///
/// Resolution rules:
///
/// - `transport`:
///   - `advertised_transport = "none"` (default) → [`peer::TransportAuth::None`]
///   - `"mutual-tls"` → [`peer::TransportAuth::MutualTls`]
///   - `"pin-self-cert"` → read this daemon's own `server.crt` from
///     the access cert dir, compute its SHA-256 fingerprint, embed it
///     in [`peer::TransportAuth::PinnedMutualTls`]. Errors if no
///     cert is present (operator forgot to run `intendant access
///     setup`).
///   - any other value → config error
/// - `application`:
///   - `bearer_token = "..."` set → `Some(Bearer { hint, rotation_url: None })`
///     where `hint` documents where the token comes from so peers
///     can give operators a useful "configure me" message
///   - unset → `None`
///
/// Called once per spawn_web_gateway invocation, at daemon startup.
/// Errors propagate as `CallerError::Config` so the operator sees
/// a clean startup failure rather than a silent misconfigure.
pub(crate) fn build_local_advertised_auth(
    server_auth: &project::ServerAuthConfig,
    cert_dir: &std::path::Path,
) -> Result<peer::AuthRequirements, CallerError> {
    let transport = match server_auth.advertised_transport.as_str() {
        "none" => peer::TransportAuth::None,
        "mutual-tls" => peer::TransportAuth::MutualTls,
        "pin-self-cert" => {
            // `pin-self-cert` reads the local server cert produced by
            // `intendant access setup`. The cert store is per-user and is
            // consumed directly by native `--tls` / `--mtls`.
            let fp = access::certs::read_server_cert_fingerprint(cert_dir).ok_or_else(|| {
                CallerError::Config(format!(
                    "[server.auth] advertised_transport = \"pin-self-cert\" requires \
                     a local server cert at {}/server.crt — run `intendant access setup` \
                     first, or change advertised_transport to \"none\" / \"mutual-tls\"",
                    cert_dir.display()
                ))
            })?;
            peer::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints: vec![fp],
            }
        }
        other => {
            return Err(CallerError::Config(format!(
                "[server.auth] advertised_transport = {other:?} is not a valid value \
                 (accepted: \"none\", \"mutual-tls\", \"pin-self-cert\")"
            )));
        }
    };
    let application = server_auth
        .bearer_token
        .as_ref()
        .map(|_| peer::ApplicationAuth::Bearer {
            hint: Some("[server.auth] bearer_token".to_string()),
            rotation_url: None,
        });
    Ok(peer::AuthRequirements {
        transport,
        application,
    })
}

/// Resolve the advertise-URL list passed to `spawn_web_gateway`,
/// applying CLI > config > auto-detect precedence.
///
/// - If `--advertise-url` was given (one or more times), the CLI list
///   wins entirely. The operator at the command line beats the
///   operator at the config file.
/// - Otherwise, if `[server.advertise]` in `intendant.toml` is non-
///   empty, that list is used.
/// - If both are empty, an empty `Vec` is returned, which signals
///   `spawn_web_gateway` to fall back to its single-URL auto-detection
///   from the listener's bind address (the historical behavior).
///
/// Returns owned `String`s so the caller can move the list directly
/// into `spawn_web_gateway` without an extra clone.
pub(crate) fn resolve_advertise_urls_from_flags_and_config(
    flags: &CliFlags,
    project: &Project,
) -> Vec<String> {
    if !flags.advertise_urls.is_empty() {
        flags.advertise_urls.clone()
    } else {
        project.config.server.advertise.clone()
    }
}

/// Build a peer registry for this daemon and hydrate it from the
/// `[[peer]]` sections in `intendant.toml`.
///
/// Spawns the durable log writer task (appending
/// `TaggedPeerEvent`s as JSONL to `<log_dir>/peers.jsonl`) and
/// creates a [`crate::peer::PeerRegistry`] wired to its sender.
/// Each config entry fires a background `add_peer` task so
/// slow/unreachable peers don't block daemon startup — the
/// registry's own reconnect state machine handles those
/// asynchronously once the card fetch returns.
///
/// The returned registry is cheaply cloneable (`Arc`-backed) and
/// gets passed into `spawn_web_gateway` so the `/api/peers`
/// handlers can inspect and mutate the same store. The log
/// writer's join handle is intentionally dropped — the writer
/// exits cleanly when all its senders go away (peer actors +
/// registry clones), and we don't currently have an explicit
/// daemon shutdown path that would await it.
pub(crate) fn build_and_hydrate_peer_registry(
    log_dir: &Path,
    peer_configs: &[project::PeerConfig],
) -> peer::PeerRegistry {
    let log_path = log_dir.join("peers.jsonl");
    let (log_tx, _log_handle) = peer::spawn_peer_log_writer(log_path);
    let registry = peer::PeerRegistry::new(log_tx);
    for cfg in peer_configs {
        let registry_for_task = registry.clone();
        let card_url = cfg.card_url.clone();
        let label = cfg.label.clone();
        let bearer_token = cfg.bearer_token.clone();
        let via_urls = cfg.via_urls.clone();
        let pinned_fingerprints = cfg.pinned_fingerprints.clone();
        let browser_tcp_via_url = cfg.browser_tcp_via_url.clone();
        let certificate_witness_vantage = cfg.certificate_witness_vantage;
        let explicit_client_identity = match peer_client_identity_from_config(cfg) {
            Ok(identity) => identity,
            Err(e) => {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                     ({card_url}): {e}"
                );
                continue;
            }
        };
        tokio::spawn(async move {
            // via_urls, when non-empty, overrides the peer's self-advertised
            // transports. pinned_fingerprints, when non-empty, replaces the
            // card's auth.transport with
            // PinnedMutualTls — operator distrusts the card's claim
            // and pins against fingerprints they got out-of-band.
            // browser_tcp_via_url, when set, overrides the dashboard's
            // default `d.ws_url` fallback when opening WebRTC display
            // — used when the browser and primary can't share the
            // same URL (primary-side localhost tunnel, split
            // browser/primary machines, etc.).
            if let Err(e) = registry_for_task
                .add_peer_with_credentials_and_client_identity_label_and_witness_vantage(
                    &card_url,
                    via_urls,
                    bearer_token,
                    pinned_fingerprints,
                    browser_tcp_via_url,
                    explicit_client_identity,
                    label,
                    certificate_witness_vantage,
                )
                .await
            {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                     ({card_url}): {e}"
                );
            }
        });
    }
    registry
}

pub(crate) fn peer_client_identity_from_config(
    cfg: &project::PeerConfig,
) -> Result<Option<peer::transport::tls_client::ClientIdentityPaths>, CallerError> {
    match (&cfg.client_cert, &cfg.client_key) {
        (Some(cert), Some(key)) => Ok(Some(peer::transport::tls_client::ClientIdentityPaths {
            cert_path: PathBuf::from(cert),
            key_path: PathBuf::from(key),
        })),
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(CallerError::Config(format!(
            "[[peer]] card_url={} must set client_cert and client_key together",
            cfg.card_url
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access_names(ip: &str) -> access::certs::ServerNames {
        access::certs::ServerNames::new(
            ip.parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap()
    }

    fn peer_config_with_client_identity(
        client_cert: Option<&str>,
        client_key: Option<&str>,
    ) -> project::PeerConfig {
        project::PeerConfig {
            card_url: "https://peer.example/.well-known/agent-card.json".to_string(),
            label: None,
            bearer_token: None,
            via_urls: Vec::new(),
            client_cert: client_cert.map(str::to_string),
            client_key: client_key.map(str::to_string),
            pinned_fingerprints: Vec::new(),
            browser_tcp_via_url: None,
            certificate_witness_vantage: crate::peer::PeerWitnessVantage::Unknown,
        }
    }

    #[test]
    fn peer_client_identity_config_requires_cert_and_key() {
        let cfg =
            peer_config_with_client_identity(Some("/tmp/client.crt"), Some("/tmp/client.key"));
        let identity = peer_client_identity_from_config(&cfg).unwrap().unwrap();
        assert_eq!(identity.cert_path, PathBuf::from("/tmp/client.crt"));
        assert_eq!(identity.key_path, PathBuf::from("/tmp/client.key"));

        assert!(
            peer_client_identity_from_config(&peer_config_with_client_identity(None, None))
                .unwrap()
                .is_none()
        );
        let err =
            peer_client_identity_from_config(&peer_config_with_client_identity(Some("x"), None))
                .unwrap_err()
                .to_string();
        assert!(err.contains("client_cert and client_key together"));
    }

    /// `build_local_advertised_auth` with the default config (all
    /// `[server.auth]` fields unset) produces `AuthRequirements::none()`
    /// — the conservative default that doesn't advertise any auth.
    /// Doesn't touch the cert dir at all; safe to run with no access setup.
    #[test]
    fn build_local_advertised_auth_defaults_to_none() {
        let server_auth = project::ServerAuthConfig::default();
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        assert_eq!(auth, peer::AuthRequirements::none());
    }

    /// Catalog entry with surgical-test defaults; tests override the fields
    /// the chooser actually reads (lines, ordinal, eligibility, names).
    /// Regression test for the live 2026-06-11 context-stress failure: codex
    /// persists a tool's `function_call_output` *before* the `token_count` of
    /// the response that emitted the call, so that report never measured the
    /// output. Attributing it to the call/output group made `after` (which
    /// keeps the bulky output) look recovery-eligible and suppressed `before`
    /// (the only cut that actually recovers).
    /// Idempotence across listing-only growth: a recovery stall appends only
    /// management calls (listings, status polls), and those must not change
    /// the model-visible catalog accounting between two identical listings.
    /// The type-B dead-end from the 2026-06-12 bench: a thread whose only
    /// remaining items are management/status calls must say plainly that
    /// nothing is left to rewind to instead of returning a bare empty page.
    /// `advertised_transport = "mutual-tls"` advertises plain mTLS.
    /// Doesn't read the cert dir (no fingerprint to compute).
    #[test]
    fn build_local_advertised_auth_mutual_tls_no_cert_lookup() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "mutual-tls".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        assert!(matches!(auth.transport, peer::TransportAuth::MutualTls));
        assert!(auth.application.is_none());
    }

    /// `advertised_transport = "pin-self-cert"` reads the access cert
    /// dir, computes the fingerprint, embeds it in PinnedMutualTls.
    /// Uses `access::certs::ensure_certs` to populate a tempdir.
    /// `access::certs` is now pure-Rust and compiles everywhere, so this
    /// applies on all platforms.
    #[test]
    fn build_local_advertised_auth_pin_self_cert_reads_cert_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        access::certs::ensure_certs(tmp.path(), &access_names("10.0.0.1"), "test", false).unwrap();
        let expected_fp = access::certs::read_server_cert_fingerprint(tmp.path()).unwrap();

        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "pin-self-cert".to_string(),
        };
        let auth = build_local_advertised_auth(&server_auth, tmp.path()).unwrap();
        match &auth.transport {
            peer::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints,
            } => {
                assert_eq!(server_cert_fingerprints, &vec![expected_fp]);
            }
            other => panic!("expected PinnedMutualTls, got {other:?}"),
        }
    }

    /// `advertised_transport = "pin-self-cert"` with no cert in
    /// the dir errors with a clear message that points the
    /// operator at `intendant access setup`.
    #[test]
    fn build_local_advertised_auth_pin_self_cert_errors_without_cert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "pin-self-cert".to_string(),
        };
        let err = build_local_advertised_auth(&server_auth, tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server.crt"), "msg: {msg}");
        assert!(msg.contains("intendant access setup"), "msg: {msg}");
    }

    /// Unrecognized `advertised_transport` value errors loudly at
    /// startup so the operator notices the typo (vs. silent fall
    /// back to "none" which would surprise them).
    #[test]
    fn build_local_advertised_auth_rejects_invalid_transport_value() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "definitely-not-valid".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let err = build_local_advertised_auth(&server_auth, &cert_dir).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("definitely-not-valid"), "msg: {msg}");
        assert!(msg.contains("none"), "msg: {msg}");
        assert!(msg.contains("mutual-tls"), "msg: {msg}");
        assert!(msg.contains("pin-self-cert"), "msg: {msg}");
    }

    /// `bearer_token` set produces `application = Some(Bearer)`
    /// regardless of the transport value. The `hint` field
    /// documents where the token comes from so connecting peers
    /// can give operators a useful "configure me" message.
    #[test]
    fn build_local_advertised_auth_bearer_token_sets_application() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: Some("secret".to_string()),
            advertised_transport: "none".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        match &auth.application {
            Some(peer::ApplicationAuth::Bearer { hint, rotation_url }) => {
                assert!(hint.is_some(), "hint should document the source");
                assert!(hint.as_ref().unwrap().contains("[server.auth]"));
                assert!(
                    rotation_url.is_none(),
                    "rotation_url unset until rotation lands"
                );
            }
            other => panic!("expected Bearer application auth, got {other:?}"),
        }
    }

    /// Combination: `pin-self-cert` + `bearer_token` produces the
    /// full defense-in-depth advertise (PinnedMutualTls transport +
    /// Bearer application). The expected configuration for WAN-
    /// exposed daemons that want both wire-layer and app-layer auth.
    /// `access::certs` is now pure-Rust and compiles everywhere, so this
    /// applies on all platforms.
    #[test]
    fn build_local_advertised_auth_full_defense_in_depth() {
        let tmp = tempfile::TempDir::new().unwrap();
        access::certs::ensure_certs(tmp.path(), &access_names("10.0.0.99"), "wan-test", false)
            .unwrap();

        let server_auth = project::ServerAuthConfig {
            bearer_token: Some("wan-secret".to_string()),
            advertised_transport: "pin-self-cert".to_string(),
        };
        let auth = build_local_advertised_auth(&server_auth, tmp.path()).unwrap();
        assert!(matches!(
            auth.transport,
            peer::TransportAuth::PinnedMutualTls { .. }
        ));
        assert!(matches!(
            auth.application,
            Some(peer::ApplicationAuth::Bearer { .. })
        ));
    }
}
