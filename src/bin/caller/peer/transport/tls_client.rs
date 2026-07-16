//! Client-side TLS helpers for peer federation.
//!
//! The dashboard gateway verifies inbound mTLS using the access CA. When this
//! daemon connects to another Intendant, it can act as an mTLS client by
//! presenting the installed access `client.crt` / `client.key` identity. These
//! helpers keep the initial Agent Card fetch and the later WebSocket attach on
//! the same TLS policy.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::peer::transport::pinning::{Fingerprint, PinnedFingerprintVerifier};
use crate::peer::PeerError;

/// PEM client identity on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentityPaths {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// True for URLs whose transport performs a TLS handshake.
pub fn url_uses_tls(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("wss://")
}

/// Return the installed access client identity if both PEM files exist.
///
/// Missing files are not an error here: TLS-only public peers do not need a
/// client cert, and non-TLS test/loopback peers should not be forced to run
/// `intendant access setup`. If a peer actually requires mTLS and these files
/// are absent, the handshake fails closed with the peer's TLS alert.
pub fn installed_access_client_identity_paths() -> Option<ClientIdentityPaths> {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let cert_path = cert_dir.join("client.crt");
    let key_path = cert_dir.join("client.key");
    if cert_path.exists() && key_path.exists() {
        Some(ClientIdentityPaths {
            cert_path,
            key_path,
        })
    } else {
        None
    }
}

/// Build a reqwest client for peer Agent Card discovery.
///
/// If `pinned_fingerprints` is non-empty, server certificate verification is
/// by exact SHA-256 fingerprint. If `client_identity` is present, the same
/// rustls config also presents the daemon's client certificate during the TLS
/// handshake.
pub fn reqwest_client(
    timeout: Duration,
    pinned_fingerprints: &[Fingerprint],
    client_identity: Option<&ClientIdentityPaths>,
) -> Result<reqwest::Client, PeerError> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(config) = rustls_client_config(pinned_fingerprints, client_identity)? {
        builder = builder.use_preconfigured_tls(config);
    }
    builder
        .build()
        .map_err(|e| PeerError::CardFetch(format!("build http client: {e}")))
}

/// Build a rustls client config for peer WebSocket/HTTP clients.
///
/// Returns `None` when neither pinning nor client-auth is required, allowing
/// callers to use their library's default TLS connector.
pub fn rustls_client_config(
    pinned_fingerprints: &[Fingerprint],
    client_identity: Option<&ClientIdentityPaths>,
) -> Result<Option<rustls::ClientConfig>, PeerError> {
    let identity = match client_identity {
        Some(paths) => Some(load_client_identity(paths)?),
        None => None,
    };

    if !pinned_fingerprints.is_empty() {
        let verifier = PinnedFingerprintVerifier::new(pinned_fingerprints.to_vec());
        let config = super::pinning::pinned_client_config_with_client_auth(verifier, identity)
            .map_err(|e| PeerError::Auth(format!("peer TLS client identity setup failed: {e}")))?;
        return Ok(Some(config));
    }

    let Some((cert_chain, key)) = identity else {
        return Ok(None);
    };

    let roots = crate::web_tls::load_native_root_store()
        .map_err(|e| PeerError::Auth(format!("load native TLS roots: {e}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| PeerError::Auth(format!("peer TLS protocol setup failed: {e}")))?
        .with_root_certificates(roots)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| PeerError::Auth(format!("peer TLS client identity setup failed: {e}")))?;
    Ok(Some(config))
}

fn load_client_identity(
    paths: &ClientIdentityPaths,
) -> Result<crate::web_tls::RustlsIdentity, PeerError> {
    crate::web_tls::load_pem_cert_and_key(&paths.cert_path, &paths.key_path).map_err(|e| {
        PeerError::Auth(format!(
            "load peer mTLS client identity {} / {}: {e}",
            paths.cert_path.display(),
            paths.key_path.display()
        ))
    })
}

/// Stat fingerprint of one PEM file — the freshness probe for
/// [`TlsClientCache`], so a re-issued `client.crt`/`client.key` is picked
/// up on the next use for two stats instead of re-reading and re-parsing
/// the PEMs (and, on the no-pin path, the whole native root store) per
/// connect attempt.
///
/// Same identity vocabulary as the repo's file-identity callers
/// ([`crate::platform::FileIdentity`]): certificate writes replace files
/// atomically, so a fresh identity — Unix `(dev, ino)`, Windows volume
/// serial + 64-bit file index — distinguishes a same-length,
/// timestamp-preserving replacement (`cp -p` rollback, same-granule
/// rewrite) that length+mtime alone would miss ON EVERY PLATFORM;
/// nanosecond mtime and length are the fallback where no reliable
/// identity exists (`identity: None` compares equal to itself only).
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    mtime_nanos: u128,
    identity: Option<crate::platform::FileIdentity>,
}

fn file_stamp(path: &std::path::Path) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    // Unix reads the identity off the metadata already in hand; Windows
    // needs an open handle (`from_path`). An unavailable or unreliable
    // identity stores `None` — the len+mtime fallback.
    let identity = crate::platform::FileIdentity::from_metadata(&metadata)
        .or_else(|| crate::platform::FileIdentity::from_path(path).ok())
        .filter(|identity| identity.is_reliable());
    Some(FileStamp {
        len: metadata.len(),
        mtime_nanos,
        identity,
    })
}

/// Lazily built, shared client-side TLS material for one credentials
/// bundle (see `TransportCredentials.tls`).
///
/// Before this cache, every transport `connect()` built the rustls
/// `ClientConfig` twice (agent-card fetch + WebSocket open) — re-reading
/// the client PEMs and, without pinning, loading and parsing the entire
/// native OS root store — and every peer MCP tool call built a fresh
/// `reqwest::Client` on top (a full TCP+TLS handshake per call, zero
/// keep-alive). Clones share the cell, so the card fetch, the WS connect,
/// and `/mcp` side-channel calls reuse one config and one pooled HTTP
/// client.
///
/// Freshness: the entry revalidates the identity PEMs by stat fingerprint
/// on every use and the pin list by equality, so certificate rotation
/// rebuilds on the next call; entries whose PEMs cannot be stat'ed are
/// never considered fresh (today's rebuild-every-time behavior).
#[derive(Clone, Debug, Default)]
pub struct TlsClientCache(Arc<std::sync::Mutex<Option<TlsCacheEntry>>>);

#[derive(Debug)]
struct TlsCacheEntry {
    pins: Vec<Fingerprint>,
    /// Identity paths plus the (cert, key) stamps captured at build time.
    identity: Option<(ClientIdentityPaths, Option<FileStamp>, Option<FileStamp>)>,
    /// `None` = neither pinning nor client auth: WS callers use their
    /// library's default TLS connector, exactly as before.
    config: Option<Arc<rustls::ClientConfig>>,
    /// Pooled HTTP client over `config` (or the library default). No
    /// client-level timeout — callers set per-request timeouts.
    http_client: reqwest::Client,
}

impl TlsCacheEntry {
    fn is_fresh(&self, pins: &[Fingerprint], identity: Option<&ClientIdentityPaths>) -> bool {
        if self.pins != pins {
            return false;
        }
        match (&self.identity, identity) {
            (None, None) => true,
            (Some((paths, cert_stamp, key_stamp)), Some(current)) => {
                paths == current
                    && cert_stamp
                        .as_ref()
                        .is_some_and(|stamp| file_stamp(&paths.cert_path).as_ref() == Some(stamp))
                    && key_stamp
                        .as_ref()
                        .is_some_and(|stamp| file_stamp(&paths.key_path).as_ref() == Some(stamp))
            }
            _ => false,
        }
    }
}

impl TlsClientCache {
    /// The rustls config for WebSocket connects: `None` when neither
    /// pinning nor client auth is configured (use the library default).
    pub fn client_config(
        &self,
        pinned_fingerprints: &[Fingerprint],
        client_identity: Option<&ClientIdentityPaths>,
    ) -> Result<Option<Arc<rustls::ClientConfig>>, PeerError> {
        Ok(self.entry(pinned_fingerprints, client_identity)?.0)
    }

    /// The pooled HTTP client on the same trust policy. Callers set
    /// per-request timeouts (`RequestBuilder::timeout`).
    pub fn http_client(
        &self,
        pinned_fingerprints: &[Fingerprint],
        client_identity: Option<&ClientIdentityPaths>,
    ) -> Result<reqwest::Client, PeerError> {
        Ok(self.entry(pinned_fingerprints, client_identity)?.1)
    }

    fn entry(
        &self,
        pins: &[Fingerprint],
        identity: Option<&ClientIdentityPaths>,
    ) -> Result<(Option<Arc<rustls::ClientConfig>>, reqwest::Client), PeerError> {
        {
            let cached = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cached.as_ref() {
                if entry.is_fresh(pins, identity) {
                    return Ok((entry.config.clone(), entry.http_client.clone()));
                }
            }
        }
        // Build outside the lock; a racing rebuild is harmless (last
        // writer wins, both entries are valid for the same inputs).
        let identity_stamps = identity.map(|paths| {
            (
                paths.clone(),
                file_stamp(&paths.cert_path),
                file_stamp(&paths.key_path),
            )
        });
        let config = rustls_client_config(pins, identity)?.map(Arc::new);
        let mut builder = reqwest::Client::builder();
        if let Some(config) = &config {
            builder = builder.use_preconfigured_tls(rustls::ClientConfig::clone(config));
        }
        let http_client = builder
            .build()
            .map_err(|e| PeerError::CardFetch(format!("build http client: {e}")))?;
        let entry = TlsCacheEntry {
            pins: pins.to_vec(),
            identity: identity_stamps,
            config: config.clone(),
            http_client: http_client.clone(),
        };
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(entry);
        Ok((config, http_client))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_uses_tls_detects_http_and_ws_schemes() {
        assert!(url_uses_tls("https://host/.well-known/agent-card.json"));
        assert!(url_uses_tls("wss://host/ws"));
        assert!(!url_uses_tls("http://host/.well-known/agent-card.json"));
        assert!(!url_uses_tls("ws://host/ws"));
    }

    #[test]
    fn rustls_client_config_none_without_pin_or_identity() {
        let config = rustls_client_config(&[], None).unwrap();
        assert!(config.is_none());
    }

    #[test]
    fn rustls_client_config_builds_with_pinned_server_and_client_identity() {
        let dir = tempfile::tempdir().unwrap();
        let names = crate::access::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        crate::access::certs::ensure_certs(dir.path(), &names, "peer-client-test", false).unwrap();

        let identity = ClientIdentityPaths {
            cert_path: dir.path().join("client.crt"),
            key_path: dir.path().join("client.key"),
        };
        let config = rustls_client_config(&[[0u8; 32]], Some(&identity));
        assert!(config.is_ok(), "config should build: {:?}", config.err());
        assert!(config.unwrap().is_some());
    }

    /// The cache reuses one built config across calls (pointer-equal Arc)
    /// and rebuilds when the identity PEMs change on disk (certificate
    /// rotation) or the pin set differs.
    #[test]
    fn tls_cache_reuses_config_until_identity_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let names = crate::access::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        crate::access::certs::ensure_certs(dir.path(), &names, "peer-client-cache-test", false)
            .unwrap();
        let identity = ClientIdentityPaths {
            cert_path: dir.path().join("client.crt"),
            key_path: dir.path().join("client.key"),
        };
        let pins = [[0u8; 32]];

        let cache = TlsClientCache::default();
        let first = cache
            .client_config(&pins, Some(&identity))
            .unwrap()
            .expect("pinned + identity builds a config");
        let second = cache
            .client_config(&pins, Some(&identity))
            .unwrap()
            .expect("cached config");
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged identity must reuse the built config"
        );

        // A different pin set never reuses the entry.
        let other_pins = [[1u8; 32]];
        let repinned = cache
            .client_config(&other_pins, Some(&identity))
            .unwrap()
            .expect("rebuilt config");
        assert!(!Arc::ptr_eq(&first, &repinned));

        // Rotate the cert on disk (appending keeps the PEM parseable but
        // changes the stat fingerprint): the next call rebuilds.
        let mut cert_pem = std::fs::read(&identity.cert_path).unwrap();
        cert_pem.push(b'\n');
        std::fs::write(&identity.cert_path, &cert_pem).unwrap();
        let rotated = cache
            .client_config(&pins, Some(&identity))
            .unwrap()
            .expect("rebuilt config after rotation");
        assert!(
            !Arc::ptr_eq(&first, &rotated),
            "a rotated identity must rebuild the config"
        );
    }

    /// The unpinned, identity-less shape stays `None` (library default
    /// TLS) through the cache, and the pooled HTTP client still builds.
    #[test]
    fn tls_cache_default_policy_yields_no_config_and_a_client() {
        let cache = TlsClientCache::default();
        assert!(cache.client_config(&[], None).unwrap().is_none());
        let _client = cache.http_client(&[], None).unwrap();
        assert!(cache.client_config(&[], None).unwrap().is_none());
    }

    /// An atomic certificate replacement that preserves BOTH length and
    /// mtime (a `cp -p`-style rollback, a same-granule rewrite) must
    /// still invalidate the cache: the fresh file identity is the
    /// discriminator — Unix `(dev, ino)`, Windows volume serial + file
    /// index (`FileIdentity`).
    #[cfg(unix)]
    #[test]
    fn tls_cache_detects_same_length_preserved_mtime_rotation() {
        same_length_preserved_mtime_rotation_rebuilds();
    }

    /// Windows mirror of the rotation test: the merge group runs full
    /// tests on the Windows leg, and `metadata_dev_ino`'s degenerate
    /// `(0, 0)` there was exactly the gap — `FileIdentity` carries the
    /// volume serial + file index instead.
    #[cfg(windows)]
    #[test]
    fn tls_cache_detects_same_length_preserved_mtime_rotation_windows() {
        same_length_preserved_mtime_rotation_rebuilds();
    }

    #[cfg(any(unix, windows))]
    fn same_length_preserved_mtime_rotation_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let names = crate::access::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        crate::access::certs::ensure_certs(dir.path(), &names, "peer-client-inode-test", false)
            .unwrap();
        let identity = ClientIdentityPaths {
            cert_path: dir.path().join("client.crt"),
            key_path: dir.path().join("client.key"),
        };
        let pins = [[0u8; 32]];
        let cache = TlsClientCache::default();
        let first = cache
            .client_config(&pins, Some(&identity))
            .unwrap()
            .expect("pinned + identity builds a config");

        // Stage identical bytes, copy the original mtime onto them, then
        // rename over the original: same length, same mtime, new inode.
        let original = std::fs::read(&identity.cert_path).unwrap();
        let mtime = std::fs::metadata(&identity.cert_path)
            .unwrap()
            .modified()
            .unwrap();
        let staged = identity.cert_path.with_extension("crt.staged");
        std::fs::write(&staged, &original).unwrap();
        let staged_file = std::fs::File::options().write(true).open(&staged).unwrap();
        staged_file.set_modified(mtime).unwrap();
        drop(staged_file);
        std::fs::rename(&staged, &identity.cert_path).unwrap();
        let after = std::fs::metadata(&identity.cert_path).unwrap();
        assert_eq!(after.len() as usize, original.len());
        assert_eq!(after.modified().unwrap(), mtime);

        let rotated = cache
            .client_config(&pins, Some(&identity))
            .unwrap()
            .expect("rebuilt config after replacement");
        assert!(
            !Arc::ptr_eq(&first, &rotated),
            "a same-length, mtime-preserved replacement must rebuild the config"
        );
    }
}
