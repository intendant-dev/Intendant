//! Native TLS for the `--web` dashboard gateway.
//!
//! Provides a pure-Rust ([`rustls`] + [`rcgen`]) path to serving the
//! dashboard over HTTPS/WSS on every platform. The accept
//! loop's per-connection demux (in [`crate::web_gateway`]) peeks the first
//! bytes of each connection and, when it sees a TLS ClientHello, wraps the
//! socket in the [`tokio_rustls::TlsAcceptor`] built here before handing
//! the decrypted stream to the existing HTTP/WebSocket handling.
//!
//! Crypto provider: everything rides the **`ring`** provider so it matches
//! the rest of the tree (`rustls` is pinned to `ring` in `Cargo.toml`; no
//! `aws-lc` is pulled in). `rcgen` is likewise built with its `ring`
//! feature, so cert generation and the TLS server share one C-free crypto
//! backend.

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::{RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsAcceptor;

pub type RustlsIdentity = (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
);

/// A stream with a replayed prefix in front of an inner async stream.
///
/// The gateway's demux reads the first chunk of the (decrypted, for TLS)
/// request to inspect the HTTP request line / headers, which consumes
/// those bytes from the underlying stream. The downstream HTTP/WebSocket
/// handlers, however, expect to read the request from byte zero. Wrapping
/// the stream in a `PrefixedStream` re-serves the already-read prefix
/// first, then transparently forwards to the inner stream — so the
/// handlers behave identically whether or not the demux pre-read anything.
///
/// Writes pass straight through. Used for the TLS path (prefix = decrypted
/// request head). The plain-TCP path uses an empty prefix, where it's a
/// zero-overhead pass-through (the kernel still holds the peeked bytes).
pub struct PrefixedStream<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            // Serve only the prefix on this poll; the next read drains the
            // inner stream. This keeps the logic simple and is fine for the
            // header-read path (callers loop until they have enough bytes).
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// First byte of a TLS record is the content type; `0x16` = Handshake.
/// A ClientHello is the first handshake message on a fresh connection, so
/// the very first byte a TLS client sends is always `0x16`. This value is
/// disjoint from every other protocol the gateway demuxes:
///
/// - HTTP request methods start with ASCII uppercase letters (`G`, `P`,
///   `O`, `D`, … all `>= 0x41`).
/// - ICE-TCP is RFC 4571 framed: a 2-byte big-endian length prefix whose
///   high byte (`buf[0]`) is the length's MSB. For a STUN binding request
///   the framed payload is ~20-28 bytes, so `buf[0] == 0x00`.
///
/// So `buf[0] == 0x16` unambiguously means TLS.
pub const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;

/// Returns true when a peeked byte prefix looks like the start of a TLS
/// ClientHello: a Handshake record (`0x16`) carrying a TLS major version
/// byte of `0x03` (TLS 1.0-1.3 all use legacy record version `3.x`).
///
/// We check two bytes rather than just the content type to avoid mistaking
/// a stray `0x16` (e.g. a truncated/garbage first byte) for TLS. The
/// record layout is: `[content_type=0x16][version_major=0x03][version_minor][len_hi][len_lo]...`.
pub fn looks_like_tls_client_hello(buf: &[u8]) -> bool {
    buf.len() >= 3 && buf[0] == TLS_HANDSHAKE_CONTENT_TYPE && buf[1] == 0x03
}

/// How the gateway should obtain its server certificate.
#[derive(Debug, Clone)]
pub enum TlsCertSource {
    /// Generate a self-signed cert at startup (the default when TLS is on).
    /// SAN list always includes `localhost`, `127.0.0.1`, and `::1`, plus
    /// the bind IP and any extra `hostname` supplied by the operator.
    SelfSigned {
        /// The address the listener is bound to. When it's a concrete
        /// (non-wildcard) IP it's added to the SAN list so the cert
        /// validates for direct-IP access.
        bind_ip: Option<IpAddr>,
        /// Optional extra DNS SAN (e.g. `intendant.local`, a tailnet name).
        hostname: Option<String>,
    },
    /// Use operator-supplied PEM files (cert chain + private key).
    Files {
        cert_path: std::path::PathBuf,
        key_path: std::path::PathBuf,
    },
}

/// Client certificate policy for the dashboard TLS acceptor.
#[derive(Debug, Clone)]
pub enum ClientAuth {
    /// Do not request or require a browser/client certificate.
    None,
    /// Request and verify a client certificate when one is presented, but
    /// allow the TLS handshake to complete without one. The HTTP layer must
    /// then decide which paths are allowed to proceed unauthenticated.
    OptionalCa { ca_path: std::path::PathBuf },
    /// Require a client certificate chaining to the supplied CA bundle.
    #[allow(dead_code)]
    RequireCa { ca_path: std::path::PathBuf },
}

/// Build a [`TlsAcceptor`] from the requested certificate source.
///
/// The returned acceptor wraps an `Arc<ServerConfig>` configured with the
/// `ring` provider and ALPN advertising `http/1.1` (the gateway speaks
/// HTTP/1.1 + WebSocket only — no HTTP/2).
#[cfg(test)]
pub fn build_acceptor(source: &TlsCertSource) -> Result<TlsAcceptor, String> {
    build_acceptor_with_client_auth(source, &ClientAuth::None)
}

/// Build an acceptor that serves only `source`, bypassing the process-global
/// fleet-SNI resolver. Root/client-certificate enrollment uses this stricter
/// form so a hosted fleet name can never replace the directly fingerprinted
/// access certificate during the bootstrap ceremony.
#[cfg(any(not(target_os = "windows"), test))]
pub fn build_single_cert_acceptor(source: &TlsCertSource) -> Result<TlsAcceptor, String> {
    let (cert_chain, key) = load_cert_source(source)?;
    let config = server_config_from(cert_chain, key, &ClientAuth::None, false)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Build a [`TlsAcceptor`] with an explicit client-certificate policy.
pub fn build_acceptor_with_client_auth(
    source: &TlsCertSource,
    client_auth: &ClientAuth,
) -> Result<TlsAcceptor, String> {
    let (cert_chain, key) = load_cert_source(source)?;
    let config = server_config_from(cert_chain, key, client_auth, true)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_cert_source(
    source: &TlsCertSource,
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    String,
> {
    let (cert_chain, key) = match source {
        TlsCertSource::SelfSigned { bind_ip, hostname } => {
            generate_self_signed(*bind_ip, hostname.as_deref())?
        }
        TlsCertSource::Files {
            cert_path,
            key_path,
        } => load_pem_cert_and_key(cert_path, key_path)?,
    };
    Ok((cert_chain, key))
}

/// Assemble a [`rustls::ServerConfig`] from a parsed cert chain + key.
///
/// Pins the `ring` crypto provider explicitly (rather than relying on the
/// process-default provider, which other subsystems may or may not have
/// installed) so this path is self-contained and order-independent.
fn server_config_from(
    cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
    client_auth: &ClientAuth,
    allow_fleet_sni: bool,
) -> Result<ServerConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("rustls protocol version setup failed: {e}"))?;
    let builder = match client_auth {
        ClientAuth::None => builder.with_no_client_auth(),
        ClientAuth::OptionalCa { ca_path } => {
            let roots = load_ca_roots(ca_path)?;
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .allow_unauthenticated()
                .build()
                .map_err(|e| {
                    format!(
                        "rustls optional client certificate verifier setup failed for {}: {e}",
                        ca_path.display()
                    )
                })?;
            builder.with_client_cert_verifier(verifier)
        }
        ClientAuth::RequireCa { ca_path } => {
            let roots = load_ca_roots(ca_path)?;
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|e| {
                    format!(
                        "rustls client certificate verifier setup failed for {}: {e}",
                        ca_path.display()
                    )
                })?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = if allow_fleet_sni {
        // A resolver instead of `with_single_cert`: the base cert (installed
        // access certs or the startup self-signed) answers by default, and a
        // fleet certificate (fleet_cert.rs) can be installed LIVE for its
        // SNI name — first issuance and every renewal apply without a daemon
        // restart.
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
            .map_err(|e| format!("rustls server key unusable: {e}"))?;
        let base = Arc::new(rustls::sign::CertifiedKey::new(cert_chain, signing_key));
        fleet_sni_resolver().set_base(base);
        builder.with_cert_resolver(fleet_sni_resolver())
    } else {
        // Enrollment must present exactly the access certificate whose
        // fingerprint the operator compares out of band. A WebPKI/fleet SNI
        // override here would let hosted DNS select a different certificate
        // and turn a terminal Enter press into root-client-bundle release.
        builder
            .with_single_cert(cert_chain, key)
            .map_err(|e| format!("rustls server certificate setup failed: {e}"))?
    };
    // The dashboard speaks HTTP/1.1 and upgrades to WebSocket; advertise
    // only http/1.1 so a browser never negotiates h2 (which the gateway's
    // hand-rolled HTTP/1 handler doesn't implement). KEEP THIS PINNED:
    // connection reuse is HTTP/1.1 keep-alive (web_gateway::keep_alive's
    // per-connection request loop), deliberately NOT h2 — adopting h2
    // would mean migrating the whole hand-rolled server stack (demux, WS
    // upgrade, streaming lanes), a separate program, not a config flip.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// SNI-aware certificate resolver: the daemon's base certificate by
/// default; the fleet certificate for requests naming the fleet name.
/// Process-global so `fleet_cert.rs` can hot-swap certificates into the
/// running acceptor.
#[derive(Debug, Default)]
pub struct FleetSniResolver {
    base: std::sync::RwLock<Option<Arc<rustls::sign::CertifiedKey>>>,
    fleet: std::sync::RwLock<Option<(String, Arc<rustls::sign::CertifiedKey>)>>,
    custom: std::sync::RwLock<std::collections::HashMap<String, Arc<rustls::sign::CertifiedKey>>>,
    // Never forget a name during this process. Accepted connections keep
    // their SNI for HTTP/1 keep-alive, and a hot fleet-name replacement must
    // not reclassify an already-open old-name connection as a pinned/direct
    // authority-bearing origin.
    fleet_names: std::sync::RwLock<std::collections::HashSet<String>>,
    // User-owned names are a separate provenance class. Keep their history
    // sticky for the same keep-alive/hot-reload reason as fleet names, without
    // ever folding them into the service-controlled fleet-name set.
    custom_names: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl FleetSniResolver {
    fn set_base(&self, key: Arc<rustls::sign::CertifiedKey>) {
        *self.base.write().expect("tls resolver poisoned") = Some(key);
    }

    fn set_fleet(&self, name: String, key: Arc<rustls::sign::CertifiedKey>) {
        self.register_fleet_name(&name);
        *self.fleet.write().expect("tls resolver poisoned") = Some((name, key));
    }

    fn register_fleet_name(&self, name: &str) {
        self.fleet_names
            .write()
            .expect("tls resolver poisoned")
            .insert(name.trim().trim_end_matches('.').to_ascii_lowercase());
    }

    fn is_fleet_name(&self, name: &str) -> bool {
        let normalized = name.trim().trim_end_matches('.').to_ascii_lowercase();
        self.fleet_names
            .read()
            .expect("tls resolver poisoned")
            .contains(&normalized)
    }

    fn set_custom(&self, name: String, key: Arc<rustls::sign::CertifiedKey>) {
        self.register_custom_name(&name);
        self.custom
            .write()
            .expect("tls resolver poisoned")
            .insert(name, key);
    }

    fn register_custom_name(&self, name: &str) {
        self.custom_names
            .write()
            .expect("tls resolver poisoned")
            .insert(name.trim().trim_end_matches('.').to_ascii_lowercase());
    }

    fn custom_name(&self, name: &str) -> Option<String> {
        let normalized = name.trim().trim_end_matches('.').to_ascii_lowercase();
        self.custom_names
            .read()
            .expect("tls resolver poisoned")
            .contains(&normalized)
            .then_some(normalized)
    }
}

impl rustls::server::ResolvesServerCert for FleetSniResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        if let Some(server_name) = client_hello.server_name() {
            let normalized = server_name.trim_end_matches('.').to_ascii_lowercase();
            if let Some(key) = self
                .custom
                .read()
                .expect("tls resolver poisoned")
                .get(&normalized)
            {
                return Some(key.clone());
            }
            let fleet = self.fleet.read().expect("tls resolver poisoned");
            if let Some((name, key)) = fleet.as_ref() {
                if server_name.eq_ignore_ascii_case(name) {
                    return Some(key.clone());
                }
            }
        }
        self.base.read().expect("tls resolver poisoned").clone()
    }
}

fn fleet_sni_resolver() -> Arc<FleetSniResolver> {
    static RESOLVER: std::sync::OnceLock<Arc<FleetSniResolver>> = std::sync::OnceLock::new();
    RESOLVER
        .get_or_init(|| Arc::new(FleetSniResolver::default()))
        .clone()
}

/// Whether an accepted TLS connection named a public fleet/WebPKI origin.
///
/// This is certificate-selection provenance, not a trust decision derived
/// from the mutable HTTP `Host` header. Gateway edges use it to keep the
/// convenient fleet-name endpoint discovery-only: service-controlled DNS can
/// serve same-origin JavaScript and therefore cannot be a root anchor even
/// when the browser also holds a daemon-issued client certificate.
pub fn is_fleet_server_name(name: Option<&str>) -> bool {
    name.is_some_and(|name| fleet_sni_resolver().is_fleet_name(name))
}

/// Remember a rendezvous-assigned fleet route before certificate issuance.
/// Classification follows service-controlled name provenance, not whichever
/// certificate happens to be active at the moment.
pub fn register_fleet_server_name(name: &str) {
    fleet_sni_resolver().register_fleet_name(name);
}

/// Whether an accepted TLS connection named a configured user-owned origin.
/// The exact SNI captured at the TLS boundary is returned for the HTTP
/// Host/Origin match; no mutable HTTP header can create this provenance.
pub fn custom_domain_server_name(name: Option<&str>) -> Option<String> {
    name.and_then(|name| fleet_sni_resolver().custom_name(name))
}

/// Remember a configured user-owned name before certificate issuance. This
/// keeps the name in its own public-origin class while ACME is pending.
pub fn register_custom_domain_server_name(name: &str) {
    fleet_sni_resolver().register_custom_name(name);
}

/// Install (or replace) the fleet certificate served for `name` —
/// called by `fleet_cert.rs` at startup-restore, first issuance, and
/// every renewal. Applies to live acceptors immediately.
pub fn install_fleet_certificate(
    name: &str,
    cert_chain_pem: &str,
    key_pem: &str,
) -> Result<(), String> {
    use rustls::pki_types::pem::PemObject;
    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(cert_chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parse fleet certificate: {e}"))?;
    if cert_chain.is_empty() {
        return Err("fleet certificate PEM holds no certificates".to_string());
    }
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| format!("parse fleet key: {e}"))?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("fleet key unusable: {e}"))?;
    fleet_sni_resolver().set_fleet(
        name.trim().trim_end_matches('.').to_ascii_lowercase(),
        Arc::new(rustls::sign::CertifiedKey::new(cert_chain, signing_key)),
    );
    Ok(())
}

/// Install (or replace) the certificate for one exact user-owned name. Custom
/// names use a separate resolver map and provenance set from fleet names.
pub fn install_custom_domain_certificate(
    name: &str,
    cert_chain_pem: &str,
    key_pem: &str,
) -> Result<(), String> {
    use rustls::pki_types::pem::PemObject;
    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(cert_chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parse custom-domain certificate: {e}"))?;
    if cert_chain.is_empty() {
        return Err("custom-domain certificate PEM holds no certificates".to_string());
    }
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| format!("parse custom-domain key: {e}"))?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("custom-domain key unusable: {e}"))?;
    fleet_sni_resolver().set_custom(
        name.trim().trim_end_matches('.').to_ascii_lowercase(),
        Arc::new(rustls::sign::CertifiedKey::new(cert_chain, signing_key)),
    );
    Ok(())
}

pub fn load_ca_roots(ca_path: &std::path::Path) -> Result<RootCertStore, String> {
    use rustls::pki_types::pem::PemObject;

    let ca_bytes = std::fs::read(ca_path)
        .map_err(|e| format!("reading mTLS client CA {}: {e}", ca_path.display()))?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(&ca_bytes)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parsing mTLS client CA {}: {e}", ca_path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "no certificates found in mTLS client CA {} (expected PEM)",
            ca_path.display()
        ));
    }

    let mut roots = RootCertStore::empty();
    let (accepted, ignored) = roots.add_parsable_certificates(certs);
    if accepted == 0 {
        return Err(format!(
            "mTLS client CA {} did not contain a usable root certificate \
             ({ignored} certificate(s) ignored)",
            ca_path.display()
        ));
    }
    Ok(roots)
}

pub fn load_native_root_store() -> Result<RootCertStore, String> {
    let result = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let mut ignored = 0usize;
    for cert in result.certs {
        if roots.add(cert).is_err() {
            ignored += 1;
        }
    }
    if roots.is_empty() {
        let errors = result
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if errors.is_empty() {
            format!("no native root certificates loaded ({ignored} certificate(s) ignored)")
        } else {
            format!(
                "no native root certificates loaded ({ignored} certificate(s) ignored): {errors}"
            )
        });
    }
    Ok(roots)
}

/// Mint a fresh self-signed certificate + key with `rcgen`.
///
/// SAN list: always `localhost` / `127.0.0.1` / `::1`, plus the bind IP
/// (when concrete) and an optional extra hostname. A wildcard bind
/// (`0.0.0.0` / `::`) contributes no IP SAN — clients reach such a server
/// by a concrete address, which they'll supply via `localhost`, the
/// hostname SAN, or by accepting the cert anyway (`curl -k`, browser
/// "proceed").
fn generate_self_signed(
    bind_ip: Option<IpAddr>,
    hostname: Option<&str>,
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    String,
> {
    let mut sans: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(ip) = bind_ip {
        if !ip.is_unspecified() {
            let s = ip.to_string();
            if !sans.contains(&s) {
                sans.push(s);
            }
        }
    }
    if let Some(h) = hostname {
        let h = h.trim();
        if !h.is_empty() && !sans.contains(&h.to_string()) {
            sans.push(h.to_string());
        }
    }

    // `generate_simple_self_signed` accepts a SAN list of strings and
    // classifies each entry as an IP or DNS name internally, mints a
    // keypair (ring provider), and returns the cert + serialized key.
    let cert = rcgen::generate_simple_self_signed(sans)
        .map_err(|e| format!("rcgen self-signed cert generation failed: {e}"))?;

    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
        .map_err(|e| format!("rcgen key -> rustls PrivateKeyDer failed: {e}"))?;

    Ok((vec![cert_der], key_der))
}

/// Load a PEM cert chain and private key from disk (operator override).
///
/// Accepts PKCS#8, PKCS#1 (RSA), and SEC1 (EC) private keys, returning
/// whichever appears first in the key file.
pub fn load_pem_cert_and_key(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<RustlsIdentity, String> {
    use rustls::pki_types::pem::{Error as PemError, PemObject};

    let cert_bytes = std::fs::read(cert_path)
        .map_err(|e| format!("reading TLS cert {}: {e}", cert_path.display()))?;
    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(&cert_bytes)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parsing TLS cert {}: {e}", cert_path.display()))?;
    if cert_chain.is_empty() {
        return Err(format!(
            "no certificates found in {} (expected PEM)",
            cert_path.display()
        ));
    }

    let key_bytes = std::fs::read(key_path)
        .map_err(|e| format!("reading TLS key {}: {e}", key_path.display()))?;
    let key: rustls::pki_types::PrivateKeyDer<'static> =
        rustls::pki_types::PrivateKeyDer::from_pem_slice(&key_bytes).map_err(|e| match e {
            PemError::NoItemsFound => {
                format!(
                    "no private key found in {} (expected PKCS#8/PKCS#1/SEC1 PEM)",
                    key_path.display()
                )
            }
            err => format!("parsing TLS key {}: {err}", key_path.display()),
        })?;

    Ok((cert_chain, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_client_hello_detection() {
        // Real ClientHello prefix: handshake record, TLS 1.2 record version.
        assert!(looks_like_tls_client_hello(&[0x16, 0x03, 0x01, 0x02, 0x00]));
        assert!(looks_like_tls_client_hello(&[0x16, 0x03, 0x03]));
    }

    #[test]
    fn tls_detection_rejects_http() {
        // "GET ", "POST", "OPTI" — all ASCII letters, never 0x16.
        assert!(!looks_like_tls_client_hello(b"GET /"));
        assert!(!looks_like_tls_client_hello(b"POST"));
        assert!(!looks_like_tls_client_hello(b"OPTIONS *"));
    }

    #[test]
    fn tls_detection_rejects_stun_ice_tcp() {
        // RFC 4571 framed STUN binding request: 2-byte BE length prefix
        // (0x00, 0x14 = 20), then STUN type 0x0001, len, magic cookie.
        // buf[0] == 0x00 != 0x16, so this is never mistaken for TLS.
        let stun = [
            0x00, 0x14, // RFC 4571 length = 20
            0x00, 0x01, // STUN binding request
            0x00, 0x08, // attrs length
            0x21, 0x12, 0xA4, 0x42, // magic cookie at offset 6
        ];
        assert!(!looks_like_tls_client_hello(&stun));
    }

    #[test]
    fn tls_detection_needs_version_byte() {
        // Lone 0x16 without the 0x03 version byte is not enough.
        assert!(!looks_like_tls_client_hello(&[0x16]));
        assert!(!looks_like_tls_client_hello(&[0x16, 0x99, 0x01]));
    }

    #[test]
    fn self_signed_acceptor_builds() {
        // End-to-end: generate a self-signed cert for a concrete bind IP +
        // hostname and assemble a working acceptor. Exercises rcgen ->
        // rustls plumbing on the ring provider.
        let src = TlsCertSource::SelfSigned {
            bind_ip: Some("192.168.1.42".parse().unwrap()),
            hostname: Some("intendant.local".to_string()),
        };
        let acceptor = build_acceptor(&src);
        assert!(
            acceptor.is_ok(),
            "acceptor build failed: {:?}",
            acceptor.err()
        );
    }

    #[test]
    fn single_cert_acceptor_bypasses_process_global_fleet_resolver() {
        let src = TlsCertSource::SelfSigned {
            bind_ip: Some("127.0.0.1".parse().unwrap()),
            hostname: None,
        };
        let single = build_single_cert_acceptor(&src).unwrap();
        let global: Arc<dyn rustls::server::ResolvesServerCert> = fleet_sni_resolver();
        assert!(
            !Arc::ptr_eq(&single.config().cert_resolver, &global),
            "strict enrollment must not consult the fleet SNI resolver"
        );

        let dashboard = build_acceptor(&src).unwrap();
        assert!(
            Arc::ptr_eq(&dashboard.config().cert_resolver, &global),
            "normal dashboard TLS should retain live fleet-SNI support"
        );
    }

    #[test]
    fn fleet_origin_classification_remembers_replaced_names() {
        let first_name = "old-fleet-origin.test";
        let second_name = "new-fleet-origin.test";
        register_fleet_server_name(first_name);
        register_fleet_server_name(second_name);

        assert!(is_fleet_server_name(Some(first_name)));
        assert!(is_fleet_server_name(Some(second_name)));
        assert!(is_fleet_server_name(Some("OLD-FLEET-ORIGIN.TEST.")));
        assert!(!is_fleet_server_name(Some("direct-daemon.test")));
    }

    #[test]
    fn custom_and_fleet_origin_classes_remain_distinct_and_sticky() {
        let fleet = "d-separate.fleet.example.test";
        let custom = "box.owner.example";
        register_fleet_server_name(fleet);
        register_custom_domain_server_name(custom);

        assert!(is_fleet_server_name(Some(fleet)));
        assert!(!is_fleet_server_name(Some(custom)));
        assert_eq!(
            custom_domain_server_name(Some("BOX.OWNER.EXAMPLE.")),
            Some(custom.to_string())
        );
        assert_eq!(custom_domain_server_name(Some(fleet)), None);
    }

    #[test]
    fn self_signed_acceptor_wildcard_bind() {
        // A wildcard bind contributes no IP SAN but still produces a cert
        // (localhost + ::1 + 127.0.0.1 are always present).
        let src = TlsCertSource::SelfSigned {
            bind_ip: Some("0.0.0.0".parse().unwrap()),
            hostname: None,
        };
        assert!(build_acceptor(&src).is_ok());
    }

    #[test]
    fn file_acceptor_loads_pem_cert_and_key() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        let src = TlsCertSource::Files {
            cert_path,
            key_path,
        };
        assert!(build_acceptor(&src).is_ok());
    }

    #[test]
    fn acceptor_builds_with_required_access_client_ca() {
        let dir = tempfile::tempdir().unwrap();
        let names = crate::access::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        crate::access::certs::ensure_certs(dir.path(), &names, "native-mtls-test", false).unwrap();

        let src = TlsCertSource::Files {
            cert_path: dir.path().join("server.crt"),
            key_path: dir.path().join("server.key"),
        };
        let client_auth = ClientAuth::RequireCa {
            ca_path: dir.path().join("ca.crt"),
        };
        assert!(build_acceptor_with_client_auth(&src, &client_auth).is_ok());
    }

    #[test]
    fn required_client_ca_must_be_pem() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let ca_path = dir.path().join("ca.crt");

        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
        std::fs::write(&ca_path, b"not a certificate").unwrap();

        let src = TlsCertSource::Files {
            cert_path,
            key_path,
        };
        let client_auth = ClientAuth::RequireCa { ca_path };
        let err = match build_acceptor_with_client_auth(&src, &client_auth) {
            Ok(_) => panic!("invalid client CA should not build an acceptor"),
            Err(err) => err,
        };
        assert!(err.contains("mTLS client CA"), "err: {err}");
    }
}
