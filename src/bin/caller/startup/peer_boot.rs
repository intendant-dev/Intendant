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
    // Durable root for attestation anti-rollback floors (RC-B2, A4):
    // beside the peer credential dirs in the access store, resolved here
    // at the production edge (tests build their own registries and
    // inject temp dirs).
    registry.set_attestation_state_dir(
        crate::access::identity_attestation::default_high_water_dir(
            &access::backend::select_backend().cert_dir(),
        ),
    );
    // Durable home for OpenClaw device identities + per-gateway device
    // tokens, beside the other private peer credential material in the
    // access store (same production-edge resolution as the attestation
    // floors above; tests inject their own registries + temp dirs).
    registry.set_openclaw_state_dir(
        access::backend::select_backend()
            .cert_dir()
            .join("openclaw"),
    );
    for cfg in peer_configs {
        // Per-entry failures degrade to a startup diagnostic and skip
        // that one peer — a typo'd [[peer]] block must not take down
        // the daemon or the other peers.
        let kind = match classify_peer_config(cfg) {
            Ok(kind) => kind,
            Err(e) => {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                     ({}): {e}",
                    describe_peer_config(cfg)
                );
                continue;
            }
        };
        // One token-resolution path for every entry kind: inline
        // `bearer_token` (legacy) or the `_env` / `_file` secret
        // references, resolved once here at the production edge.
        let bearer_token = match resolve_peer_bearer_token(cfg, |name| std::env::var(name).ok()) {
            Ok(token) => token,
            Err(e) => {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                         ({}): {e}",
                    describe_peer_config(cfg)
                );
                continue;
            }
        };
        let registry_for_task = registry.clone();
        let label = cfg.label.clone();
        let certificate_witness_vantage = cfg.certificate_witness_vantage;
        match kind {
            PeerConfigKind::CardDriven { card_url } => {
                let via_urls = cfg.via_urls.clone();
                let pinned_fingerprints = cfg.pinned_fingerprints.clone();
                let identity_public_key = cfg.identity_public_key.clone();
                let browser_tcp_via_url = cfg.browser_tcp_via_url.clone();
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
                        .add_peer_full(
                            &card_url,
                            via_urls,
                            bearer_token,
                            pinned_fingerprints,
                            browser_tcp_via_url,
                            explicit_client_identity,
                            label,
                            certificate_witness_vantage,
                            identity_public_key,
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
            PeerConfigKind::OpenClawWs { url, role } => {
                // No card fetch: an OpenClaw Gateway serves no
                // `/.well-known/agent-card.json`, so the entry
                // synthesizes a local card carrying one
                // `TransportSpec::OpenClawWs`. The resolved bearer
                // token rides `TransportCredentials::bearer_token`
                // into the transport as the pairing-bootstrap
                // credential (`auth.token` in the `connect` frame).
                //
                // Until this build ships the OpenClaw transport, the
                // registry's supported-transport filter rejects the
                // synthesized card at add time with the same clean
                // "advertises no transport this build supports"
                // diagnostic every unimplemented transport kind gets
                // — graceful degradation, not a panic or a silent
                // nothing.
                let card = openclaw_card_from_config(cfg, url.clone(), role);
                tokio::spawn(async move {
                    if let Err(e) = registry_for_task
                        .add_peer_with_card_and_auth_and_client_identity(
                            card,
                            Vec::new(),
                            bearer_token,
                            None,
                            None,
                            label,
                            certificate_witness_vantage,
                            None,
                        )
                        .await
                    {
                        eprintln!(
                            "intendant: failed to register openclaw peer from \
                             intendant.toml ({url}): {e}"
                        );
                    }
                });
            }
        }
    }
    registry
}

/// The `[[peer]]` entry shape resolved from its `transport` selector —
/// see [`classify_peer_config`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerConfigKind {
    /// Default: fetch the peer's Agent Card from `card_url`.
    CardDriven { card_url: String },
    /// `transport = "openclaw-ws"`: connect straight to an OpenClaw
    /// Gateway's WebSocket control plane; the Agent Card is
    /// synthesized locally ([`openclaw_card_from_config`]).
    OpenClawWs {
        url: String,
        role: peer::card::OpenClawRole,
    },
}

/// Entry identifier for boot diagnostics: whichever address the entry
/// configured, so the operator can find the offending block.
fn describe_peer_config(cfg: &project::PeerConfig) -> String {
    cfg.card_url
        .clone()
        .or_else(|| cfg.url.clone())
        .or_else(|| cfg.label.clone())
        .unwrap_or_else(|| "<address-less [[peer]] entry>".to_string())
}

/// Validate one `[[peer]]` block and resolve its kind.
///
/// Boot-time (not parse-time) by design: an invalid entry degrades to
/// a clear per-peer startup diagnostic instead of failing the whole
/// config load. Loud on everything that would otherwise silently do
/// nothing — an unrecognized `transport`, a `role` this build doesn't
/// know (local config never hydrates the wire-side `Unknown`
/// forward-compat fallback), and card-driven-only fields on an
/// openclaw entry.
pub(crate) fn classify_peer_config(
    cfg: &project::PeerConfig,
) -> Result<PeerConfigKind, CallerError> {
    match cfg.transport.as_deref() {
        None => {
            let card_url = cfg
                .card_url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| {
                    CallerError::Config(
                        "[[peer]] requires card_url (or set transport = \"openclaw-ws\" \
                         with url = \"ws://…\" for an OpenClaw Gateway entry)"
                            .to_string(),
                    )
                })?;
            for (set, name) in [(cfg.url.is_some(), "url"), (cfg.role.is_some(), "role")] {
                if set {
                    return Err(CallerError::Config(format!(
                        "[[peer]] card_url={card_url} sets `{name}`, which only applies \
                         to transport = \"openclaw-ws\" entries"
                    )));
                }
            }
            Ok(PeerConfigKind::CardDriven {
                card_url: card_url.to_string(),
            })
        }
        Some("openclaw-ws") => {
            let url = cfg
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| {
                    CallerError::Config(
                        "[[peer]] transport = \"openclaw-ws\" requires url = \
                         \"ws://<gateway-host>:18789\" (or wss://…)"
                            .to_string(),
                    )
                })?;
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                return Err(CallerError::Config(format!(
                    "[[peer]] transport = \"openclaw-ws\" url must be a ws:// or \
                     wss:// WebSocket URL, got {url:?}"
                )));
            }
            let role = match cfg.role.as_deref().map(str::trim) {
                None | Some("") => peer::card::OpenClawRole::Operator,
                Some(raw) => match peer::card::OpenClawRole::from_wire(raw) {
                    peer::card::OpenClawRole::Unknown => {
                        return Err(CallerError::Config(format!(
                            "[[peer]] transport = \"openclaw-ws\" role = {raw:?} is not \
                             a valid value (accepted: \"operator\", \"node\")"
                        )));
                    }
                    role => role,
                },
            };
            // Fields that belong to the card-driven flow would silently
            // do nothing here (or worse — `via_urls` would replace the
            // OpenClaw spec with IntendantWs candidates). Fail loud.
            for (set, name) in [
                (cfg.card_url.is_some(), "card_url"),
                (!cfg.via_urls.is_empty(), "via_urls"),
                (!cfg.pinned_fingerprints.is_empty(), "pinned_fingerprints"),
                (cfg.client_cert.is_some(), "client_cert"),
                (cfg.client_key.is_some(), "client_key"),
                (cfg.identity_public_key.is_some(), "identity_public_key"),
                (cfg.browser_tcp_via_url.is_some(), "browser_tcp_via_url"),
            ] {
                if set {
                    return Err(CallerError::Config(format!(
                        "[[peer]] transport = \"openclaw-ws\" ({url}) sets `{name}`, \
                         which only applies to card-driven Intendant peer entries"
                    )));
                }
            }
            Ok(PeerConfigKind::OpenClawWs {
                url: url.to_string(),
                role,
            })
        }
        Some(other) => Err(CallerError::Config(format!(
            "[[peer]] transport = {other:?} is not a valid value (accepted: absent \
             for card-driven Intendant peers, or \"openclaw-ws\")"
        ))),
    }
}

/// Resolve the outbound bearer token for one `[[peer]]` entry from
/// exactly one of its three sources: inline `bearer_token` (legacy),
/// `bearer_token_env` (the name of an environment variable), or
/// `bearer_token_file` (a file whose trimmed contents are the token).
/// More than one source set is a config error; none set is
/// `Ok(None)`.
///
/// `env` is injected (production passes a `std::env::var` wrapper) so
/// tests stay hermetic — no process-global environment mutation.
pub(crate) fn resolve_peer_bearer_token(
    cfg: &project::PeerConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, CallerError> {
    let sources = [
        cfg.bearer_token.is_some(),
        cfg.bearer_token_env.is_some(),
        cfg.bearer_token_file.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    if sources > 1 {
        return Err(CallerError::Config(format!(
            "[[peer]] ({}) sets more than one of bearer_token, bearer_token_env, \
             bearer_token_file — configure exactly one token source",
            describe_peer_config(cfg)
        )));
    }
    if let Some(token) = &cfg.bearer_token {
        return Ok(Some(token.clone()));
    }
    if let Some(name) = &cfg.bearer_token_env {
        let name = name.trim();
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(CallerError::Config(format!(
                "[[peer]] ({}) bearer_token_env {name:?} is not a valid environment \
                 variable name",
                describe_peer_config(cfg)
            )));
        }
        let value = env(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CallerError::Config(format!(
                    "[[peer]] ({}) bearer_token_env names {name}, but that environment \
                     variable is unset or empty",
                    describe_peer_config(cfg)
                ))
            })?;
        return Ok(Some(value));
    }
    if let Some(path) = &cfg.bearer_token_file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            CallerError::Config(format!(
                "[[peer]] ({}) bearer_token_file {path}: {e}",
                describe_peer_config(cfg)
            ))
        })?;
        let token = raw.trim();
        if token.is_empty() {
            return Err(CallerError::Config(format!(
                "[[peer]] ({}) bearer_token_file {path} is empty",
                describe_peer_config(cfg)
            )));
        }
        return Ok(Some(token.to_string()));
    }
    Ok(None)
}

/// Synthesize the local Agent Card for a `transport = "openclaw-ws"`
/// entry. A gateway serves no card, so this daemon constructs the
/// registry-facing identity itself:
///
/// - `id` = `openclaw:<label>`, where the label is the operator's
///   `label` when set, else `host[:port]` from the gateway URL. The
///   registry keys rows on this id, so two entries for the same
///   gateway (e.g. a future operator+node pair) need distinct labels
///   — the second otherwise rejects with `already_registered`.
/// - `transports` = exactly one [`peer::TransportSpec::OpenClawWs`].
/// - `version` is left empty (unknown until the transport's first
///   `hello-ok` refreshes the card) and `capabilities` empty — badges
///   render from what a live connection reports, never from static
///   assumptions.
pub(crate) fn openclaw_card_from_config(
    cfg: &project::PeerConfig,
    url: String,
    role: peer::card::OpenClawRole,
) -> peer::AgentCard {
    let label = cfg
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| openclaw_label_from_url(&url));
    peer::AgentCard {
        id: peer::PeerId::new(crate::peer::id::PeerKind::OpenClaw, &label),
        label,
        version: String::new(),
        git_sha: None,
        transports: vec![peer::TransportSpec::OpenClawWs { url, role }],
        capabilities: Vec::new(),
        auth: peer::AuthRequirements::none(),
        identity_attestation: None,
    }
}

/// Default display label for an openclaw entry without one:
/// `host` (default-port) or `host:port` from the gateway URL, falling
/// back to the raw URL when it doesn't parse.
fn openclaw_label_from_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => url.to_string(),
        },
        Err(_) => url.to_string(),
    }
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
            "[[peer]] ({}) must set client_cert and client_key together",
            describe_peer_config(cfg)
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
            card_url: Some("https://peer.example/.well-known/agent-card.json".to_string()),
            client_cert: client_cert.map(str::to_string),
            client_key: client_key.map(str::to_string),
            ..Default::default()
        }
    }

    fn openclaw_config(url: Option<&str>, role: Option<&str>) -> project::PeerConfig {
        project::PeerConfig {
            transport: Some("openclaw-ws".to_string()),
            url: url.map(str::to_string),
            role: role.map(str::to_string),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------
    // classify_peer_config
    // -----------------------------------------------------------------

    #[test]
    fn classify_default_entry_is_card_driven() {
        let cfg = project::PeerConfig {
            card_url: Some("https://peer.example/.well-known/agent-card.json".into()),
            ..Default::default()
        };
        assert_eq!(
            classify_peer_config(&cfg).unwrap(),
            PeerConfigKind::CardDriven {
                card_url: "https://peer.example/.well-known/agent-card.json".into()
            }
        );
    }

    #[test]
    fn classify_default_entry_without_card_url_errors() {
        let err = classify_peer_config(&project::PeerConfig::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires card_url"), "{err}");
        assert!(
            err.contains("openclaw-ws"),
            "error points at the alternative shape: {err}"
        );
    }

    #[test]
    fn classify_card_driven_rejects_openclaw_only_fields() {
        let mut cfg = project::PeerConfig {
            card_url: Some("https://peer.example/.well-known/agent-card.json".into()),
            url: Some("ws://gateway:18789".into()),
            ..Default::default()
        };
        let err = classify_peer_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("`url`"), "{err}");

        cfg.url = None;
        cfg.role = Some("operator".into());
        let err = classify_peer_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("`role`"), "{err}");
    }

    #[test]
    fn classify_openclaw_entry_defaults_role_to_operator() {
        let kind = classify_peer_config(&openclaw_config(Some("ws://gw:18789"), None)).unwrap();
        assert_eq!(
            kind,
            PeerConfigKind::OpenClawWs {
                url: "ws://gw:18789".into(),
                role: peer::card::OpenClawRole::Operator,
            }
        );
        // Explicit roles parse; wss URLs pass the scheme check.
        let kind =
            classify_peer_config(&openclaw_config(Some("wss://gw:18789"), Some("node"))).unwrap();
        assert_eq!(
            kind,
            PeerConfigKind::OpenClawWs {
                url: "wss://gw:18789".into(),
                role: peer::card::OpenClawRole::Node,
            }
        );
    }

    /// Local config must fail loud on values the wire's forward-compat
    /// fallback would hydrate as `Unknown` — an operator typo is a
    /// config error, not a future-version peer.
    #[test]
    fn classify_openclaw_entry_rejects_unknown_role_and_bad_url() {
        let err = classify_peer_config(&openclaw_config(Some("ws://gw:18789"), Some("supervisor")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("supervisor"), "{err}");
        assert!(err.contains("operator"), "{err}");
        assert!(err.contains("node"), "{err}");

        let err = classify_peer_config(&openclaw_config(Some("http://gw:18789"), None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ws://"), "{err}");

        let err = classify_peer_config(&openclaw_config(None, None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires url"), "{err}");
    }

    /// Card-driven-only fields on an openclaw entry would silently do
    /// nothing (or, for via_urls, actively replace the OpenClaw spec
    /// with IntendantWs candidates) — each fails loud instead.
    #[test]
    fn classify_openclaw_entry_rejects_card_driven_fields() {
        let cases: Vec<(&str, Box<dyn Fn(&mut project::PeerConfig)>)> = vec![
            (
                "card_url",
                Box::new(|cfg| cfg.card_url = Some("https://x/.well-known".into())),
            ),
            (
                "via_urls",
                Box::new(|cfg| cfg.via_urls = vec!["ws://x/ws".into()]),
            ),
            (
                "pinned_fingerprints",
                Box::new(|cfg| cfg.pinned_fingerprints = vec!["aa".into()]),
            ),
            (
                "client_cert",
                Box::new(|cfg| cfg.client_cert = Some("/x.crt".into())),
            ),
            (
                "client_key",
                Box::new(|cfg| cfg.client_key = Some("/x.key".into())),
            ),
            (
                "identity_public_key",
                Box::new(|cfg| cfg.identity_public_key = Some("k".into())),
            ),
            (
                "browser_tcp_via_url",
                Box::new(|cfg| cfg.browser_tcp_via_url = Some("ws://x/ws".into())),
            ),
        ];
        for (name, mutate) in cases {
            let mut cfg = openclaw_config(Some("ws://gw:18789"), None);
            mutate(&mut cfg);
            let err = classify_peer_config(&cfg).unwrap_err().to_string();
            assert!(
                err.contains(&format!("`{name}`")),
                "rejection names the field {name}: {err}"
            );
        }
    }

    #[test]
    fn classify_unknown_transport_value_errors() {
        let cfg = project::PeerConfig {
            transport: Some("a2a".into()),
            ..Default::default()
        };
        let err = classify_peer_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("a2a"), "{err}");
        assert!(err.contains("openclaw-ws"), "{err}");
    }

    // -----------------------------------------------------------------
    // resolve_peer_bearer_token
    // -----------------------------------------------------------------

    #[test]
    fn bearer_token_resolution_prefers_exactly_one_source() {
        // None configured → Ok(None).
        let cfg = openclaw_config(Some("ws://gw:18789"), None);
        assert_eq!(resolve_peer_bearer_token(&cfg, |_| None).unwrap(), None);

        // Inline plaintext (legacy) passes through.
        let cfg = project::PeerConfig {
            bearer_token: Some("inline-tok".into()),
            ..openclaw_config(Some("ws://gw:18789"), None)
        };
        assert_eq!(
            resolve_peer_bearer_token(&cfg, |_| None)
                .unwrap()
                .as_deref(),
            Some("inline-tok")
        );

        // Two sources set → config error naming the conflict.
        let cfg = project::PeerConfig {
            bearer_token: Some("inline-tok".into()),
            bearer_token_env: Some("TOK".into()),
            ..openclaw_config(Some("ws://gw:18789"), None)
        };
        let err = resolve_peer_bearer_token(&cfg, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one"), "{err}");
    }

    /// The env reference resolves through the injected lookup (tests
    /// never touch the process environment — hermetic-tests law), and
    /// an unset/empty variable is a loud config error, not a silent
    /// token-less connect.
    #[test]
    fn bearer_token_env_reference_resolves_and_fails_loud() {
        let cfg = project::PeerConfig {
            bearer_token_env: Some("OPENCLAW_GATEWAY_TOKEN".into()),
            ..openclaw_config(Some("ws://gw:18789"), None)
        };
        let token = resolve_peer_bearer_token(&cfg, |name| {
            (name == "OPENCLAW_GATEWAY_TOKEN").then(|| "  sekrit \n".to_string())
        })
        .unwrap();
        assert_eq!(token.as_deref(), Some("sekrit"), "resolved and trimmed");

        let err = resolve_peer_bearer_token(&cfg, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("OPENCLAW_GATEWAY_TOKEN"), "{err}");
        assert!(err.contains("unset or empty"), "{err}");

        // Invalid env-var names are rejected before lookup.
        let cfg = project::PeerConfig {
            bearer_token_env: Some("BAD=NAME".into()),
            ..openclaw_config(Some("ws://gw:18789"), None)
        };
        let err = resolve_peer_bearer_token(&cfg, |_| Some("x".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid environment"), "{err}");
    }

    #[test]
    fn bearer_token_file_reference_reads_and_trims() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("gateway-token");
        std::fs::write(&path, "file-tok\n").unwrap();
        let cfg = project::PeerConfig {
            bearer_token_file: Some(path.to_string_lossy().into_owned()),
            ..openclaw_config(Some("ws://gw:18789"), None)
        };
        assert_eq!(
            resolve_peer_bearer_token(&cfg, |_| None)
                .unwrap()
                .as_deref(),
            Some("file-tok")
        );

        // Empty file and missing file both fail loud with the path.
        std::fs::write(&path, "  \n").unwrap();
        let err = resolve_peer_bearer_token(&cfg, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is empty"), "{err}");

        let missing = dir.path().join("nope");
        let cfg = project::PeerConfig {
            bearer_token_file: Some(missing.to_string_lossy().into_owned()),
            ..openclaw_config(Some("ws://gw:18789"), None)
        };
        let err = resolve_peer_bearer_token(&cfg, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
    }

    // -----------------------------------------------------------------
    // openclaw_card_from_config
    // -----------------------------------------------------------------

    /// The synthesized card carries the openclaw peer identity and
    /// exactly one `TransportSpec::OpenClawWs` — the shape the
    /// registry's transport selection and the dashboard's kind badge
    /// both key on.
    #[test]
    fn openclaw_card_synthesis_shapes_id_label_and_transport() {
        // Labeled entry: the label is both display name and id suffix.
        let mut cfg = openclaw_config(Some("ws://gw.local:18789"), None);
        cfg.label = Some("home-gateway".into());
        let card = openclaw_card_from_config(
            &cfg,
            "ws://gw.local:18789".into(),
            peer::card::OpenClawRole::Operator,
        );
        assert_eq!(card.id.as_str(), "openclaw:home-gateway");
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::id::PeerKind::OpenClaw),
            "id kind drives the dashboard's kind badge"
        );
        assert_eq!(card.label, "home-gateway");
        assert_eq!(card.version, "", "version unknown until first hello-ok");
        assert!(card.capabilities.is_empty());
        match card.transports.as_slice() {
            [peer::TransportSpec::OpenClawWs { url, role }] => {
                assert_eq!(url, "ws://gw.local:18789");
                assert_eq!(*role, peer::card::OpenClawRole::Operator);
            }
            other => panic!("expected exactly one OpenClawWs transport, got {other:?}"),
        }

        // Unlabeled entry: host:port fallback keeps the id stable and
        // human-readable.
        let cfg = openclaw_config(Some("ws://gw.local:18789"), None);
        let card = openclaw_card_from_config(
            &cfg,
            "ws://gw.local:18789".into(),
            peer::card::OpenClawRole::Node,
        );
        assert_eq!(card.id.as_str(), "openclaw:gw.local:18789");
        assert_eq!(card.label, "gw.local:18789");
        match card.transports.as_slice() {
            [peer::TransportSpec::OpenClawWs { role, .. }] => {
                assert_eq!(*role, peer::card::OpenClawRole::Node);
            }
            other => panic!("expected OpenClawWs transport, got {other:?}"),
        }
    }

    /// End-to-end boot-shape check for the not-yet-shipped transport:
    /// the synthesized card presented to a registry WITHOUT an
    /// OpenClaw transport implementation is rejected at add time with
    /// the same clean "no supported transport" diagnostic every
    /// unimplemented transport kind gets (mirrors
    /// `registry::tests::add_peer_rejects_card_with_no_supported_transports`)
    /// — no panic, no silent nothing, no zombie registry entry. When
    /// the transport seat lands its `pick_supported_transports` /
    /// `build_transport` arms, this add starts succeeding and the
    /// assertion below flips — at that point this test should assert
    /// the registered entry instead.
    #[tokio::test]
    async fn openclaw_card_add_degrades_cleanly_until_transport_ships() {
        use tokio::sync::mpsc;
        let (log_tx, _log_rx) = mpsc::channel::<crate::peer::EnqueuedPeerEvent>(8);
        let registry = peer::PeerRegistry::new(log_tx);
        let cfg = openclaw_config(Some("ws://gw:18789"), None);
        let card = openclaw_card_from_config(
            &cfg,
            "ws://gw:18789".into(),
            peer::card::OpenClawRole::Operator,
        );
        match registry
            .add_peer_with_card_and_auth_and_client_identity(
                card,
                Vec::new(),
                Some("bootstrap-token".into()),
                None,
                None,
                None,
                crate::peer::PeerWitnessVantage::Unknown,
                None,
            )
            .await
        {
            Err(crate::peer::PeerError::CardFetch(msg)) => {
                assert!(msg.contains("no transport"), "{msg}");
            }
            Ok(_) => {
                // The OpenClaw transport has landed in this build:
                // the config path now registers a live entry. Keep the
                // invariant honest — the entry must exist and carry
                // the openclaw id.
                assert_eq!(registry.len(), 1);
            }
            Err(other) => panic!("expected clean CardFetch diagnostic, got {other:?}"),
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
