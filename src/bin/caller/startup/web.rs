//! Web-gateway startup: port discovery, single/dual-stack binding,
//! TLS/mTLS acceptor resolution from flags + installed access certs,
//! bind-safety validation, and the idle-web-daemon predicate.

use crate::access;
use crate::error::CallerError;
use crate::project;
use crate::web_tls;
use crate::CliFlags;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

/// Try binding to ports starting from `preferred`, returning the bound listener.
/// Avoids TOCTOU by keeping the listener alive instead of probing and releasing.
///
/// Binds dual-stack (IPv6 with `IPV6_V6ONLY=false`) so the listener
/// accepts both IPv6 and IPv4 connections. Without this, macOS
/// defaults `V6ONLY=true` on IPv6 sockets and an IPv4-only bind
/// would mismatch [`web_gateway::resolve_advertise_urls`], which
/// enumerates every routable interface (v4 and v6) into the Agent
/// Card. Federation code that picks a card URL verbatim — notably
/// slice 3b's `relay_advertise_url` — would then inject an
/// unreachable IPv6 ICE-TCP candidate and the browser would fail
/// to form a pair. Dual-stack keeps every advertised URL
/// truthful.
///
/// Falls back to IPv4-only if an IPv6 socket can't be created or
/// configured (containerized envs with no IPv6 stack, hardened
/// sandboxes that block V6ONLY toggling, etc). On those hosts
/// `routable_local_addrs` won't find any IPv6 interfaces either,
/// so the card's URL list stays consistent with the bind.
pub(crate) async fn find_available_port(
    preferred: u16,
    bind_ip: Option<IpAddr>,
) -> Result<(u16, tokio::net::TcpListener), CallerError> {
    for offset in 0..20u16 {
        let port = preferred.checked_add(offset).unwrap_or(preferred);
        match bind_web_listener(port, bind_ip).await {
            Ok(listener) => {
                // Report the port actually bound, not the one requested:
                // `--web 0` asks the kernel for an ephemeral port (the
                // race-free way to run parallel daemons — smoke rigs and
                // CI runner instances sharing a box), and `local_addr` is
                // the only truthful source for what it picked.
                let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                return Ok((bound, listener));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => {
                return Err(CallerError::Config(format!(
                    "Failed to bind web gateway port: {}",
                    e
                )));
            }
        }
    }
    Err(CallerError::Config(format!(
        "No available port found in range {}-{}",
        preferred,
        preferred + 19
    )))
}

pub(crate) async fn bind_web_listener(
    port: u16,
    bind_ip: Option<IpAddr>,
) -> std::io::Result<tokio::net::TcpListener> {
    match bind_ip {
        None => bind_dual_stack_or_v4(port).await,
        Some(IpAddr::V6(ip)) if ip.is_unspecified() => bind_dual_stack_or_v4(port).await,
        Some(IpAddr::V4(ip)) => {
            bind_single_stack(SocketAddr::new(IpAddr::V4(ip), port), socket2::Domain::IPV4)
        }
        Some(IpAddr::V6(ip)) => {
            bind_single_stack(SocketAddr::new(IpAddr::V6(ip), port), socket2::Domain::IPV6)
        }
    }
}

pub(crate) fn bind_single_stack(
    addr: SocketAddr,
    domain: socket2::Domain,
) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Protocol, Socket, Type};
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    let _ = socket.set_reuse_address(true);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Bind a TCP listener on `port`, preferring IPv6 dual-stack.
/// See [`find_available_port`] for why dual-stack matters.
///
/// Uses `socket2` directly because `tokio::net::TcpSocket` doesn't
/// expose `IPV6_V6ONLY`. The constructed `std::net::TcpListener` is
/// set non-blocking and handed to tokio via `from_std`, which is the
/// same path tokio's own `TcpSocket::listen` takes under the hood.
///
/// Sets `SO_REUSEADDR` so a restart lands on the same port even
/// when the previous daemon's sockets are still in `TIME_WAIT`.
/// Without this, the Intendant.app wrapper's IPv4 probe (which
/// does set `SO_REUSEADDR`) says 8765 is free — the backend then
/// fails to bind it and slides to 8766, the WKWebView's HTTP poll
/// keeps hitting 8765, and the UI shows "Failed to connect to
/// backend on port 8765" even though the backend is healthy on
/// the next port. Matching the wrapper's assumption keeps the
/// port stable across restarts.
pub(crate) async fn bind_dual_stack_or_v4(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    if let Ok(socket) = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) {
        // Flip V6ONLY off so the listener accepts IPv4 too. If the
        // kernel doesn't support the toggle (hardened sandboxes),
        // fall through to the IPv4 fallback path.
        if socket.set_only_v6(false).is_ok() {
            // Best-effort: SO_REUSEADDR isn't load-bearing for
            // correctness (ignore Err), but without it a quick
            // restart races the kernel's TIME_WAIT window.
            let _ = socket.set_reuse_address(true);
            let v6_wildcard: SocketAddr = format!("[::]:{port}")
                .parse()
                .expect("IPv6 wildcard literal parses");
            // Propagate bind errors (AddrInUse / EACCES / etc) so the
            // caller's loop can walk to the next port or fail loudly.
            // Don't silently fall back to IPv4 here — an in-use IPv6
            // port is in use for IPv4 too on a dual-stack host.
            socket.bind(&v6_wildcard.into())?;
            socket.listen(1024)?;
            // tokio::net::TcpListener::from_std requires the underlying
            // socket to be in non-blocking mode.
            socket.set_nonblocking(true)?;
            let std_listener: std::net::TcpListener = socket.into();
            return tokio::net::TcpListener::from_std(std_listener);
        }
    }
    // IPv4 fallback for hosts without an IPv6 stack. Same TIME_WAIT
    // reasoning as the v6 path above — set SO_REUSEADDR via socket2
    // rather than going through tokio's bind (which doesn't expose it).
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let _ = socket.set_reuse_address(true);
    let v4_wildcard: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("IPv4 wildcard literal parses");
    socket.bind(&v4_wildcard.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Build the optional TLS acceptor for the `--web` dashboard.
///
/// The dashboard defaults to mTLS. `--tls` / `[server.tls] enabled = true`
/// explicitly select TLS-only, and `--no-tls` is the cleartext debug escape.
/// When TLS is enabled, the cert source is resolved in priority order:
///   1. Explicit PEM files — CLI `--tls-cert`/`--tls-key` first, else
///      `[server.tls] cert`/`key`. Both halves of a pair must be present.
///   2. Installed access certs (`server.crt` / `server.key`) from the platform's
///      `intendant access` cert directory.
///   3. For TLS-only, otherwise a self-signed cert minted by `rcgen`, with the
///      listener bind IP plus `localhost` (and optional `[server.tls] hostname`)
///      in the SAN list. mTLS never silently falls back to self-signed because
///      the browser also needs an enrolled client identity.
///
/// Returns `Ok(None)` only for `--no-tls`, `Ok(Some(acceptor))`
/// when on and the cert built, or `Err` when enabled but misconfigured
/// (mismatched cert/key pair, unreadable/invalid PEM, cert-gen failure) —
/// surfaced loudly at startup rather than silently serving plain HTTP.
pub(crate) fn build_web_tls_acceptor(
    flags: &CliFlags,
    server_cfg: &project::ServerTlsConfig,
    mtls_cfg: &project::ServerMutualTlsConfig,
    bind_addr: Option<std::net::SocketAddr>,
) -> Result<Option<tokio_rustls::TlsAcceptor>, CallerError> {
    if flags.no_tls {
        return Ok(None);
    }
    let mtls_enabled = web_mtls_enabled(flags, server_cfg, mtls_cfg);

    // Resolve an explicit cert/key pair: CLI overrides config. A
    // half-specified pair (only cert or only key) is a configuration
    // error rather than a silent fallback to self-signed.
    let cert_path = flags.tls_cert.clone().or_else(|| server_cfg.cert.clone());
    let key_path = flags.tls_key.clone().or_else(|| server_cfg.key.clone());
    let source = match (cert_path, key_path) {
        (Some(c), Some(k)) => web_tls::TlsCertSource::Files {
            cert_path: c.into(),
            key_path: k.into(),
        },
        (Some(_), None) | (None, Some(_)) => {
            return Err(CallerError::Config(
                "TLS cert/key must be supplied together (got only one of --tls-cert/--tls-key \
                 or [server.tls] cert/key)"
                    .to_string(),
            ));
        }
        (None, None) => match installed_access_tls_cert_source()? {
            Some(source) => source,
            None if mtls_enabled => {
                // A service-managed first boot has no human at a prompt: on
                // a machine whose access dir has never existed, provision
                // the same durable material `intendant access setup` would
                // create (CA + server pair + enrollable client identity)
                // and continue. Anything short of virgin still gets the
                // loud error — minting a new CA over one that browsers
                // already enrolled against would silently strand them.
                let provisioned = access::provision_virgin_access_certs().map_err(|e| {
                    CallerError::Config(format!(
                        "Dashboard mTLS is enabled by default and first-boot access \
                         certificate provisioning failed: {e}. Run `intendant access \
                         setup`, pass `--tls` for HTTPS with certless authority limited \
                         to loopback, or pass `--no-tls --bind 127.0.0.1` only for \
                         explicit local/debug plaintext."
                    ))
                })?;
                match provisioned {
                    Some(cert_dir) => {
                        eprintln!(
                            "[access] first boot: generated dashboard access certificates \
                             in {} — enroll a browser with `intendant access serve-certs`; \
                             the Connect claim flow is discovery-only",
                            cert_dir.display()
                        );
                        installed_access_tls_cert_source()?.ok_or_else(|| {
                            CallerError::Config(missing_default_mtls_cert_message(
                                &installed_access_cert_dir(),
                            ))
                        })?
                    }
                    None => {
                        return Err(CallerError::Config(missing_default_mtls_cert_message(
                            &installed_access_cert_dir(),
                        )));
                    }
                }
            }
            None => web_tls::TlsCertSource::SelfSigned {
                bind_ip: bind_addr.map(|a| a.ip()),
                hostname: server_cfg.hostname.clone(),
            },
        },
    };

    let client_auth = if mtls_enabled {
        let ca_path = flags
            .mtls_ca
            .clone()
            .or_else(|| mtls_cfg.ca.clone())
            .map(PathBuf::from)
            .or_else(installed_access_mtls_ca_path);
        let Some(ca_path) = ca_path else {
            return Err(CallerError::Config(
                "mTLS requested, but no client CA was configured and no installed access CA \
                 was found. Run `intendant access setup` or pass --mtls-ca <ca.crt>."
                    .to_string(),
            ));
        };
        web_tls::ClientAuth::OptionalCa { ca_path }
    } else {
        web_tls::ClientAuth::None
    };

    // Legacy access stores predate setup-time IAM seeding. Complete that
    // exact-local-client.crt migration here, before the listener accepts any
    // request, whenever this gateway trusts the installed access CA. Request
    // authentication cannot mint or grant authority, so hosted JS cannot
    // reach a root mutation through an asset load or cross-origin navigation.
    let installed_cert_dir = installed_access_cert_dir();
    let installed_ca_path = installed_cert_dir.join("ca.crt");
    let trusts_installed_access_ca = match &client_auth {
        web_tls::ClientAuth::OptionalCa { ca_path }
        | web_tls::ClientAuth::RequireCa { ca_path } => ca_path == &installed_ca_path,
        web_tls::ClientAuth::None => false,
    };
    if trusts_installed_access_ca
        && access::iam::migrate_generated_browser_mtls_owner_root_at_startup(&installed_cert_dir)
            .map_err(|error| {
                CallerError::Config(format!(
                "migrate installed owner mTLS identity into local IAM before web startup: {error}"
            ))
            })?
    {
        eprintln!(
            "[access] completed trusted startup migration for the generated owner mTLS identity"
        );
    }

    match &source {
        web_tls::TlsCertSource::Files {
            cert_path,
            key_path,
        } => {
            eprintln!(
                "[web_gateway] TLS certificate source: {} / {}",
                cert_path.display(),
                key_path.display()
            );
        }
        web_tls::TlsCertSource::SelfSigned { .. } => {
            eprintln!("[web_gateway] TLS certificate source: ephemeral self-signed certificate");
        }
    }
    if let web_tls::ClientAuth::RequireCa { ca_path } = &client_auth {
        eprintln!("[web_gateway] mTLS client CA: {}", ca_path.display());
    }

    let acceptor = web_tls::build_acceptor_with_client_auth(&source, &client_auth)
        .map_err(|e| CallerError::Config(format!("TLS setup failed: {e}")))?;
    Ok(Some(acceptor))
}

pub(crate) fn web_mtls_enabled(
    flags: &CliFlags,
    server_cfg: &project::ServerTlsConfig,
    mtls_cfg: &project::ServerMutualTlsConfig,
) -> bool {
    if flags.no_tls {
        return false;
    }
    flags.mtls || mtls_cfg.enabled || web_default_mtls_enabled(flags, server_cfg)
}

pub(crate) fn web_default_mtls_enabled(
    flags: &CliFlags,
    server_cfg: &project::ServerTlsConfig,
) -> bool {
    !flags.no_tls
        && !flags.tls
        && !server_cfg.enabled
        && flags.tls_cert.is_none()
        && flags.tls_key.is_none()
}

pub(crate) fn missing_default_mtls_cert_message(cert_dir: &Path) -> String {
    format!(
        "Dashboard mTLS is enabled by default, but no installed access server certificate was \
         found in {cert_dir} (expected server.crt and server.key). The directory holds other \
         access material, so first-boot auto-provisioning stayed hands-off rather than touch an \
         existing CA. Run `intendant access setup` to (re)generate what's missing, pass `--tls` \
         for HTTPS with certless authority limited to loopback, or pass `--no-tls --bind \
         127.0.0.1` only for explicit local/debug plaintext.",
        cert_dir = cert_dir.display()
    )
}

pub(crate) fn installed_access_cert_dir() -> PathBuf {
    access::backend::select_backend().cert_dir()
}

pub(crate) fn installed_access_tls_cert_source(
) -> Result<Option<web_tls::TlsCertSource>, CallerError> {
    let cert_dir = installed_access_cert_dir();
    installed_access_tls_cert_source_from_dir(&cert_dir)
}

pub(crate) fn installed_access_tls_cert_source_from_dir(
    cert_dir: &Path,
) -> Result<Option<web_tls::TlsCertSource>, CallerError> {
    installed_access_tls_cert_source_from_dir_with_probe(cert_dir, |path| {
        std::fs::File::open(path).map(|_| ())
    })
}

pub(crate) fn installed_access_tls_cert_source_from_dir_with_probe(
    cert_dir: &Path,
    can_read: impl Fn(&Path) -> io::Result<()>,
) -> Result<Option<web_tls::TlsCertSource>, CallerError> {
    let cert_path = cert_dir.join("server.crt");
    let key_path = cert_dir.join("server.key");
    let cert_exists = cert_path.exists();
    let key_exists = key_path.exists();
    match (cert_exists, key_exists) {
        (true, true) => {
            ensure_installed_access_tls_file_readable(
                cert_dir,
                &cert_path,
                "certificate",
                &can_read,
            )?;
            ensure_installed_access_tls_file_readable(
                cert_dir,
                &key_path,
                "private key",
                &can_read,
            )?;
            Ok(Some(web_tls::TlsCertSource::Files {
                cert_path,
                key_path,
            }))
        }
        (false, false) => Ok(None),
        _ => Err(CallerError::Config(format!(
            "Installed access TLS certs are incomplete in {} (expected both server.crt and \
             server.key). Run `intendant access setup --force` or pass --tls-cert/--tls-key.",
            cert_dir.display()
        ))),
    }
}

pub(crate) fn ensure_installed_access_tls_file_readable(
    cert_dir: &Path,
    path: &Path,
    role: &str,
    can_read: &impl Fn(&Path) -> io::Result<()>,
) -> Result<(), CallerError> {
    can_read(path).map_err(|err| {
        CallerError::Config(installed_access_tls_unreadable_message(
            cert_dir, path, role, &err,
        ))
    })
}

pub(crate) fn installed_access_tls_unreadable_message(
    cert_dir: &Path,
    path: &Path,
    role: &str,
    err: &io::Error,
) -> String {
    let permission_hint = if err.kind() == io::ErrorKind::PermissionDenied {
        String::from(
            " To let this user run native `--tls` with the installed access cert, \
             fix ownership of the per-user access cert store or rerun \
             `intendant access setup --force` as that user.",
        )
    } else {
        String::new()
    };
    format!(
        "Installed access TLS {role} exists at {path}, but this process cannot read it: {err}. \
         Native `--tls` reads the server certificate and key directly from the per-user \
         access cert store at {cert_dir}.{permission_hint} Alternatively, pass a readable pair with \
         `--tls-cert <cert> --tls-key <key>`, or move the installed pair out of {cert_dir} to use \
         the self-signed fallback.",
        path = path.display(),
        cert_dir = cert_dir.display(),
    )
}

pub(crate) fn installed_access_mtls_ca_path() -> Option<PathBuf> {
    installed_access_mtls_ca_path_from_dir(&installed_access_cert_dir())
}

pub(crate) fn installed_access_mtls_ca_path_from_dir(cert_dir: &Path) -> Option<PathBuf> {
    let ca_path = cert_dir.join("ca.crt");
    ca_path.exists().then_some(ca_path)
}

pub(crate) fn dashboard_display_url(
    web_tls_acceptor: &Option<tokio_rustls::TlsAcceptor>,
    web_port: u16,
    web_bind: Option<IpAddr>,
) -> String {
    let scheme = if web_tls_acceptor.is_some() {
        "https"
    } else {
        "http"
    };
    let host = dashboard_display_host(web_bind);
    format!("{scheme}://{host}:{web_port}")
}

pub(crate) fn dashboard_display_host(web_bind: Option<IpAddr>) -> String {
    match web_bind {
        Some(IpAddr::V4(ip)) => ip.to_string(),
        Some(IpAddr::V6(ip)) => format!("[{ip}]"),
        None => "0.0.0.0".to_string(),
    }
}

pub(crate) fn dashboard_log_line(
    web_tls_acceptor: &Option<tokio_rustls::TlsAcceptor>,
    web_port: u16,
    web_bind: Option<IpAddr>,
) -> String {
    format!(
        "Dashboard: {}",
        dashboard_display_url(web_tls_acceptor, web_port, web_bind)
    )
}

pub(crate) fn validate_tls_cli_flags(flags: &CliFlags) -> Result<(), CallerError> {
    if flags.no_tls
        && (flags.tls
            || flags.mtls
            || flags.tls_cert.is_some()
            || flags.tls_key.is_some()
            || flags.mtls_ca.is_some())
    {
        return Err(CallerError::Config(
            "`--no-tls` cannot be combined with `--tls`, `--mtls`, `--tls-cert`, \
             `--tls-key`, or `--mtls-ca`."
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn effective_web_bind_ip(
    flags: &CliFlags,
    server_cfg: &project::ServerConfig,
) -> Option<IpAddr> {
    flags.web_bind.or(server_cfg.bind)
}

pub(crate) fn validate_plaintext_web_bind(
    flags: &CliFlags,
    bind_ip: Option<IpAddr>,
) -> Result<(), CallerError> {
    let public_addrs = public_routable_local_addrs();
    validate_plaintext_web_bind_with_public_addrs(flags, bind_ip, &public_addrs)
}

pub(crate) fn validate_plaintext_web_bind_with_public_addrs(
    flags: &CliFlags,
    bind_ip: Option<IpAddr>,
    public_addrs: &[IpAddr],
) -> Result<(), CallerError> {
    if !flags.no_tls
        || flags.allow_public_plaintext
        || !web_bind_is_wildcard(bind_ip)
        || public_addrs.is_empty()
    {
        return Ok(());
    }

    let public_list = public_addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Err(CallerError::Config(format!(
        "Refusing `--no-tls` on a wildcard dashboard listener because this host has public \
         interface address(es): {public_list}. Plain HTTP would expose Intendant on those \
         addresses. Use default mTLS, `--tls`, `--bind 127.0.0.1`, bind a specific private \
         interface, or pass `--allow-public-plaintext` if this is intentional."
    )))
}

pub(crate) fn web_bind_is_wildcard(bind_ip: Option<IpAddr>) -> bool {
    bind_ip.map(|ip| ip.is_unspecified()).unwrap_or(true)
}

pub(crate) fn public_routable_local_addrs() -> Vec<IpAddr> {
    let mut addrs = access::routable_local_addrs(false)
        .into_iter()
        .filter(is_public_ip)
        .collect::<Vec<_>>();
    addrs.sort_by_key(|ip| ip.to_string());
    addrs.dedup();
    addrs
}

pub(crate) fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(*ip),
        IpAddr::V6(ip) => is_public_ipv6(*ip),
    }
}

pub(crate) fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_private()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_documentation()
        && !is_shared_carrier_nat_ipv4(ip)
}

pub(crate) fn is_shared_carrier_nat_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
}

pub(crate) fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let first = segments[0];
    let is_unique_local = (first & 0xfe00) == 0xfc00;
    let is_link_local = (first & 0xffc0) == 0xfe80;
    let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !is_unique_local
        && !is_link_local
        && !is_documentation
}

pub(crate) fn should_start_idle_web_daemon(use_web: bool, flags: &CliFlags) -> bool {
    use_web
        && !flags.mcp
        && flags.task_file.is_none()
        && flags
            .task
            .as_ref()
            .map(|task| task.trim().is_empty())
            .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn find_available_port_reports_the_kernel_assigned_port_for_web_zero() {
        // `--web 0` = ephemeral bind: the returned port must be what the
        // kernel actually assigned (smoke rigs parse it from the
        // Dashboard log line), never the literal 0 that was requested.
        let loopback = Some("127.0.0.1".parse().unwrap());
        let (port, listener) = find_available_port(0, loopback).await.unwrap();
        assert_ne!(port, 0);
        assert_eq!(port, listener.local_addr().unwrap().port());
        // A second ephemeral bind coexists — the parallel-daemon case.
        let (port2, listener2) = find_available_port(0, loopback).await.unwrap();
        assert_ne!(port2, 0);
        assert_ne!(port2, port);
        drop((listener, listener2));
    }

    #[test]
    fn public_ip_classification_excludes_private_and_documentation_ranges() {
        assert!(is_public_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_public_ip(&"10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"192.168.1.10".parse().unwrap()));
        assert!(!is_public_ip(&"100.64.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"203.0.113.10".parse().unwrap()));
        assert!(is_public_ip(&"2001:4860:4860::8888".parse().unwrap()));
        assert!(!is_public_ip(&"fc00::1".parse().unwrap()));
        assert!(!is_public_ip(&"fe80::1".parse().unwrap()));
        assert!(!is_public_ip(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn installed_access_tls_source_uses_complete_server_pair() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("server.crt");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, b"cert").unwrap();
        std::fs::write(&key_path, b"key").unwrap();

        let source = installed_access_tls_cert_source_from_dir(dir.path())
            .unwrap()
            .expect("access cert pair should be discovered");
        match source {
            web_tls::TlsCertSource::Files {
                cert_path: c,
                key_path: k,
            } => {
                assert_eq!(c, cert_path);
                assert_eq!(k, key_path);
            }
            other => panic!("expected file source, got {other:?}"),
        }
    }

    #[test]
    fn installed_access_tls_source_ignores_absent_pair() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_access_tls_cert_source_from_dir(dir.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn installed_access_tls_source_errors_on_partial_pair() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.crt"), b"cert").unwrap();
        let err = installed_access_tls_cert_source_from_dir(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("incomplete"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn installed_access_tls_source_explains_unreadable_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.crt"), b"cert").unwrap();
        std::fs::write(dir.path().join("server.key"), b"key").unwrap();

        let err = installed_access_tls_cert_source_from_dir_with_probe(dir.path(), |path| {
            if path.ends_with("server.key") {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "permission denied",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot read it"), "msg: {msg}");
        assert!(msg.contains("server.key"), "msg: {msg}");
        assert!(msg.contains("per-user access cert store"), "msg: {msg}");
        assert!(msg.contains("intendant access setup --force"), "msg: {msg}");
        assert!(
            msg.contains("--tls-cert <cert> --tls-key <key>"),
            "msg: {msg}"
        );
    }

    #[test]
    fn installed_access_mtls_ca_uses_ca_crt_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.crt");
        std::fs::write(&ca_path, b"ca").unwrap();
        assert_eq!(
            installed_access_mtls_ca_path_from_dir(dir.path()).as_deref(),
            Some(ca_path.as_path())
        );
    }
}
