//! Certificate generation for `intendant access`.
//!
//! Produces:
//! - A self-signed CA (10-year validity), reused across re-runs.
//! - A server cert with loopback, local interface IPs, and optional DNS names
//!   as SANs (825-day validity — iOS requires ≤825 days since 2020).
//! - A client cert (10-year validity).
//! - A password-protected PKCS#12 bundle containing the client key, cert,
//!   and CA chain, packaged with Apple auto-detect-compatible legacy
//!   algorithms (PBES1 / 3DES-CBC + SHA-1 MAC) that macOS profile
//!   installation and `security import` accept without explicit format
//!   hints (see `build_apple_compatible_p12`).
//!
//! Everything is idempotent — if certs already exist, the CA and client
//! cert are preserved and only the server cert is regenerated when the
//! access address set has changed.
//!
//! Pure-Rust: RSA keys are generated with RustCrypto `rsa`, certs are
//! signed with `rcgen` via the `ring` backend (no OpenSSL/aws-lc-sys C
//! toolchain), and the PKCS#12 bundle is built with `p12-keystore`
//! (legacy PBES1/3DES for Apple profile-import compatibility).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use p12_keystore::{
    Certificate as P12Certificate, EncryptionAlgorithm, KeyStore, KeyStoreEntry, MacAlgorithm,
    PrivateKeyChain,
};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PublicKeyData, SanType, SubjectPublicKeyInfo,
    PKCS_RSA_SHA256,
};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use time::{Duration, OffsetDateTime};

use super::{state, AccessError, AccessResult};

/// Filenames within `cert_dir`.
const CA_KEY: &str = "ca.key";
const CA_CRT: &str = "ca.crt";
const SERVER_KEY: &str = "server.key";
const SERVER_CRT: &str = "server.crt";
const CLIENT_KEY: &str = "client.key";
const CLIENT_CRT: &str = "client.crt";
const CLIENT_P12: &str = "client.p12";
const SERVER_NAMES: &str = "server_names";

/// True when `cert_dir` holds none of the access material this module
/// manages — no CA, no server pair, no client identity. This is the only
/// state in which first-boot self-provisioning may act: anything partial
/// means a human (or an older install) has been here, and regenerating a
/// CA would strand every browser enrolled against it.
pub fn dir_is_virgin(cert_dir: &Path) -> bool {
    [
        CA_KEY, CA_CRT, SERVER_KEY, SERVER_CRT, CLIENT_KEY, CLIENT_CRT, CLIENT_P12,
    ]
    .iter()
    .all(|name| !cert_dir.join(name).exists())
}

/// Data returned from `ensure_certs` and used downstream by the cert
/// distribution server.
pub struct CertState {
    pub cert_dir: PathBuf,
    pub p12_password: String,
    #[allow(dead_code)]
    pub label: String,
}

/// PEM-encoded client identity issued by this daemon's access CA.
#[derive(Debug)]
pub struct IssuedClientIdentity {
    pub cert_pem: String,
    pub key_pem: String,
}

#[derive(Debug, Clone)]
pub struct GeneratedClientKey {
    pub public_key_pem: String,
    pub key_pem: String,
}

/// Complete identity set for the dashboard server certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNames {
    pub primary_ip: IpAddr,
    pub ips: Vec<IpAddr>,
    pub dns_names: Vec<String>,
}

impl ServerNames {
    pub fn new<I, D>(primary_ip: IpAddr, ips: I, dns_names: D) -> AccessResult<Self>
    where
        I: IntoIterator<Item = IpAddr>,
        D: IntoIterator<Item = String>,
    {
        let mut all_ips = vec![
            primary_ip,
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        ];
        all_ips.extend(ips);
        all_ips.sort();
        all_ips.dedup();

        let mut all_dns = vec!["localhost".to_string()];
        for name in dns_names {
            let normalized = name.trim().trim_end_matches('.').to_ascii_lowercase();
            if normalized.is_empty() {
                continue;
            }
            Ia5String::try_from(normalized.as_str())
                .map_err(|e| AccessError(format!("invalid DNS name '{normalized}': {e}")))?;
            all_dns.push(normalized);
        }
        all_dns.sort();
        all_dns.dedup();

        Ok(Self {
            primary_ip,
            ips: all_ips,
            dns_names: all_dns,
        })
    }

    pub fn metadata(&self) -> String {
        let mut lines = Vec::with_capacity(1 + self.ips.len() + self.dns_names.len());
        lines.push(format!("primary_ip={}", self.primary_ip));
        lines.extend(self.ips.iter().map(|ip| format!("ip={ip}")));
        lines.extend(self.dns_names.iter().map(|name| format!("dns={name}")));
        lines.join("\n") + "\n"
    }

    pub fn display_summary(&self) -> String {
        let ips = self
            .ips
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let dns = self.dns_names.join(", ");
        format!("IPs [{ips}], DNS [{dns}]")
    }
}

/// Generate everything that's missing, idempotently. If the CA and
/// client cert already exist, they're reused; if the server cert identity set
/// doesn't match `server_names`, it's regenerated.
pub fn ensure_certs(
    cert_dir: &Path,
    server_names: &ServerNames,
    label: &str,
    force: bool,
) -> AccessResult<CertState> {
    let ca_exists = cert_dir.join(CA_KEY).exists() && cert_dir.join(CA_CRT).exists();
    let client_exists = cert_dir.join(CLIENT_KEY).exists() && cert_dir.join(CLIENT_P12).exists();

    if ca_exists && client_exists && !force {
        println!(
            ":: certs already exist in {} (use --force to regenerate)",
            cert_dir.display()
        );

        if cert_needs_regen_for_names(cert_dir, server_names)? {
            println!("!! access addresses changed — regenerating server cert");
            regenerate_server_cert(cert_dir, server_names)?;
        }

        let password = state::read_p12_password(cert_dir)?;
        return Ok(CertState {
            cert_dir: cert_dir.to_path_buf(),
            p12_password: password,
            label: label.to_string(),
        });
    }

    println!(":: generating certificates...");

    let (ca_cert, ca_key) = generate_ca(label)?;
    write_pem_cert(&cert_dir.join(CA_CRT), &ca_cert)?;
    write_pem_private_key(&cert_dir.join(CA_KEY), &ca_key)?;

    // The CA acts as issuer for both the server and client leaf certs.
    // Derive the issuer from the freshly generated CA cert PEM so it
    // carries the real subject DN + key-usage extensions (same path
    // `recert` uses when reading the CA back from disk).
    let ca_issuer = issuer_from_pem(&ca_cert.pem(), ca_key)?;

    let (server_cert, server_key) = generate_server_cert(&ca_issuer, server_names)?;
    write_pem_cert(&cert_dir.join(SERVER_CRT), &server_cert)?;
    write_pem_private_key(&cert_dir.join(SERVER_KEY), &server_key)?;
    write_server_names(cert_dir, server_names)?;

    let (client_cert, client_key) = generate_client_cert(&ca_issuer, label)?;
    write_pem_cert(&cert_dir.join(CLIENT_CRT), &client_cert)?;
    write_pem_private_key(&cert_dir.join(CLIENT_KEY), &client_key)?;

    let password = random_password(12);
    let p12_bytes =
        build_apple_compatible_p12(&client_key, &client_cert, &[ca_cert], label, &password)?;
    std::fs::write(cert_dir.join(CLIENT_P12), &p12_bytes)?;
    state::write_p12_password(cert_dir, &password)?;

    println!(":: certificates generated in {}", cert_dir.display());
    println!(
        ":: server cert issued for {} (valid 825 days)",
        server_names.display_summary()
    );

    Ok(CertState {
        cert_dir: cert_dir.to_path_buf(),
        p12_password: password,
        label: label.to_string(),
    })
}

/// Regenerate just the server cert, e.g. after an access address change. The CA,
/// client cert, and .p12 are preserved — clients that already imported
/// the CA don't need to do anything.
pub fn recert(cert_dir: &Path, server_names: &ServerNames, force: bool) -> AccessResult<()> {
    if !force && !cert_needs_regen_for_names(cert_dir, server_names)? {
        println!(
            ":: server cert already matches {} — nothing to do (use --force to regenerate)",
            server_names.display_summary()
        );
        return Ok(());
    }

    let old = current_cert_identity(cert_dir).unwrap_or_else(|_| "unknown".to_string());
    println!(
        ":: access addresses changed: {old} → {}",
        server_names.display_summary()
    );
    regenerate_server_cert(cert_dir, server_names)?;
    Ok(())
}

/// Issue an additional client certificate from the existing access CA.
///
/// Used by peer pairing: the accepting daemon signs a daemon-to-daemon
/// client identity for the joining daemon, without disturbing the browser
/// client cert generated by `intendant access setup`.
pub fn issue_client_identity(cert_dir: &Path, label: &str) -> AccessResult<IssuedClientIdentity> {
    let ca_path = cert_dir.join(CA_CRT);
    let key_path = cert_dir.join(CA_KEY);
    if !ca_path.exists() || !key_path.exists() {
        return Err(AccessError(format!(
            "no access CA found in {} — run `intendant access setup` first",
            cert_dir.display()
        )));
    }

    let ca_pem = std::fs::read_to_string(ca_path)?;
    let ca_key_pem = std::fs::read_to_string(key_path)?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_issuer = issuer_from_pem(&ca_pem, ca_key)?;
    let (cert, key) = generate_client_cert(&ca_issuer, label)?;
    Ok(IssuedClientIdentity {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Generate requester-side key material for peer access requests.
///
/// The private key stays on the requesting daemon. The public key is sent to
/// the target daemon and signed only after target-local approval.
pub fn generate_client_key_material() -> AccessResult<GeneratedClientKey> {
    let key = generate_rsa_key_pair()?;
    let public_key_pem = pem::encode(&pem::Pem::new("PUBLIC KEY", key.subject_public_key_info()));
    Ok(GeneratedClientKey {
        public_key_pem,
        key_pem: key.serialize_pem(),
    })
}

/// Issue a client certificate for a requester-supplied public key.
///
/// Used by access-request approval so the target daemon never receives the
/// requester's private key.
pub fn issue_client_certificate_for_public_key(
    cert_dir: &Path,
    label: &str,
    public_key_pem: &str,
) -> AccessResult<String> {
    let ca_path = cert_dir.join(CA_CRT);
    let key_path = cert_dir.join(CA_KEY);
    if !ca_path.exists() || !key_path.exists() {
        return Err(AccessError(format!(
            "no access CA found in {} — run `intendant access setup` first",
            cert_dir.display()
        )));
    }

    let ca_pem = std::fs::read_to_string(ca_path)?;
    let ca_key_pem = std::fs::read_to_string(key_path)?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_issuer = issuer_from_pem(&ca_pem, ca_key)?;
    let public_key = SubjectPublicKeyInfo::from_pem(public_key_pem)
        .map_err(|e| AccessError(format!("parse requester public key: {e}")))?;
    let params = client_cert_params(label)?;
    let cert = params.signed_by(&public_key, &ca_issuer)?;
    Ok(cert.pem())
}

fn regenerate_server_cert(cert_dir: &Path, server_names: &ServerNames) -> AccessResult<()> {
    let ca_pem = std::fs::read_to_string(cert_dir.join(CA_CRT))?;
    let ca_key_pem = std::fs::read_to_string(cert_dir.join(CA_KEY))?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_issuer = issuer_from_pem(&ca_pem, ca_key)?;

    let (server_cert, server_key) = generate_server_cert(&ca_issuer, server_names)?;
    write_pem_cert(&cert_dir.join(SERVER_CRT), &server_cert)?;
    write_pem_private_key(&cert_dir.join(SERVER_KEY), &server_key)?;
    write_server_names(cert_dir, server_names)?;
    println!(
        ":: server cert issued for {} (valid 825 days)",
        server_names.display_summary()
    );
    Ok(())
}

/// SHA-256 fingerprint of this daemon's local server cert, in
/// the lowercase-hex format that
/// [`crate::access::pinning::parse_fingerprint`] consumes.
///
/// Used by `[server.auth] advertised_transport = "pin-self-cert"`
/// to auto-fill the local Agent Card's
/// `auth.transport = PinnedMutualTls` field — operators don't have
/// to compute the fingerprint by hand. Returns `None` when no
/// `server.crt` is present in `cert_dir` (e.g. `intendant access
/// setup` hasn't been run yet); the caller treats `None` as a
/// configuration error since `pin-self-cert` without a cert is
/// nonsensical.
///
/// Reads the PEM cert, converts to DER, hashes via SHA-256.
/// Same byte-for-byte hash a connecting peer's
/// `PinnedFingerprintVerifier` will compute on the wire, so the
/// pin matches.
pub fn read_server_cert_fingerprint(cert_dir: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let der = read_cert_der(&cert_dir.join(SERVER_CRT)).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&der);
    let fp: [u8; 32] = hasher.finalize().into();
    let mut s = String::with_capacity(64);
    for byte in fp {
        s.push_str(&format!("{byte:02x}"));
    }
    Some(s)
}

/// Extract a displayable current server cert identity.
pub fn current_cert_identity(cert_dir: &Path) -> AccessResult<String> {
    if let Ok(primary) = current_cert_ip(cert_dir) {
        return Ok(format!("primary IP {primary}"));
    }
    if let Ok(metadata) = std::fs::read_to_string(cert_dir.join(SERVER_NAMES)) {
        return Ok(metadata.lines().collect::<Vec<_>>().join(", "));
    }
    Err(AccessError("no identity in server cert".into()))
}

/// Extract the current server cert's primary IP from metadata, falling back to
/// the subject CN for older certs that predate `server_names`.
pub fn current_cert_ip(cert_dir: &Path) -> AccessResult<String> {
    use x509_parser::prelude::*;

    if let Ok(metadata) = std::fs::read_to_string(cert_dir.join(SERVER_NAMES)) {
        if let Some(primary) = metadata
            .lines()
            .filter_map(|line| line.strip_prefix("primary_ip="))
            .next()
        {
            return Ok(primary.to_string());
        }
    }

    let der = read_cert_der(&cert_dir.join(SERVER_CRT))?;
    let (_, cert) = X509Certificate::from_der(&der)
        .map_err(|e| AccessError(format!("parse server cert: {e}")))?;
    for attr in cert.subject().iter_common_name() {
        if let Ok(cn) = attr.as_str() {
            return Ok(cn.to_string());
        }
    }
    Err(AccessError("no CN in server cert".into()))
}

fn cert_needs_regen_for_names(cert_dir: &Path, server_names: &ServerNames) -> AccessResult<bool> {
    if !cert_dir.join(SERVER_CRT).exists() {
        return Ok(true);
    }
    match std::fs::read_to_string(cert_dir.join(SERVER_NAMES)) {
        Ok(current) => Ok(current != server_names.metadata()),
        Err(_) => Ok(true),
    }
}

fn write_server_names(cert_dir: &Path, server_names: &ServerNames) -> AccessResult<()> {
    std::fs::write(cert_dir.join(SERVER_NAMES), server_names.metadata())?;
    Ok(())
}

// ── Cert primitives ─────────────────────────────────────────────────────────

/// Build the `CertificateParams` for the CA. Factored out so the same
/// shape is used whether we're self-signing or rederiving an issuer.
fn ca_params_for(label: &str) -> AccessResult<CertificateParams> {
    let mut params =
        CertificateParams::new(vec![]).map_err(|e| AccessError(format!("ca params: {e}")))?;
    params
        .distinguished_name
        .push(DnType::CommonName, format!("Intendant CA ({label})"));
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(3650);
    Ok(params)
}

fn generate_ca(label: &str) -> AccessResult<(Certificate, KeyPair)> {
    let params = ca_params_for(label)?;
    let key = generate_rsa_key_pair()?;
    let cert = params.self_signed(&key)?;
    Ok((cert, key))
}

fn generate_rsa_key_pair() -> AccessResult<KeyPair> {
    let mut rng = rand::rngs::OsRng;
    let rsa_key = rsa::RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|e| AccessError(format!("generate RSA key: {e}")))?;
    let pkcs8_pem = rsa_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| AccessError(format!("encode RSA key: {e}")))?;
    KeyPair::from_pem_and_sign_algo(pkcs8_pem.as_str(), &PKCS_RSA_SHA256)
        .map_err(|e| AccessError(format!("load RSA key: {e}")))
}

/// Reconstruct a signing [`Issuer`] from a CA cert in PEM form plus its
/// key pair. Used both right after generation and on the `recert` path
/// where the CA is read back from disk. The issuer captures the CA's
/// subject DN and key-usage extensions from the parsed cert.
fn issuer_from_pem(ca_pem: &str, ca_key: KeyPair) -> AccessResult<Issuer<'static, KeyPair>> {
    Issuer::from_ca_cert_pem(ca_pem, ca_key)
        .map_err(|e| AccessError(format!("load CA issuer: {e}")))
}

fn generate_server_cert(
    ca_issuer: &Issuer<'_, KeyPair>,
    server_names: &ServerNames,
) -> AccessResult<(Certificate, KeyPair)> {
    let mut params =
        CertificateParams::new(vec![]).map_err(|e| AccessError(format!("server params: {e}")))?;
    params
        .distinguished_name
        .push(DnType::CommonName, server_names.primary_ip.to_string());
    let mut san = Vec::with_capacity(server_names.ips.len() + server_names.dns_names.len());
    san.extend(server_names.ips.iter().copied().map(SanType::IpAddress));
    for name in &server_names.dns_names {
        let ia5 = Ia5String::try_from(name.as_str())
            .map_err(|e| AccessError(format!("invalid DNS name '{name}': {e}")))?;
        san.push(SanType::DnsName(ia5));
    }
    params.subject_alt_names = san;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    // iOS requires server cert validity ≤825 days.
    params.not_after = now + Duration::days(825);

    let key = generate_rsa_key_pair()?;
    let cert = params.signed_by(&key, ca_issuer)?;
    Ok((cert, key))
}

fn generate_client_cert(
    ca_issuer: &Issuer<'_, KeyPair>,
    label: &str,
) -> AccessResult<(Certificate, KeyPair)> {
    let params = client_cert_params(label)?;
    let key = generate_rsa_key_pair()?;
    let cert = params.signed_by(&key, ca_issuer)?;
    Ok((cert, key))
}

fn client_cert_params(label: &str) -> AccessResult<CertificateParams> {
    let mut params =
        CertificateParams::new(vec![]).map_err(|e| AccessError(format!("client params: {e}")))?;
    params
        .distinguished_name
        .push(DnType::CommonName, format!("Intendant Client ({label})"));
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(3650);
    Ok(params)
}

/// Build a PKCS#12 bundle using Apple auto-detect-compatible legacy
/// encryption. The modern `p12-keystore` default (PBES2/AES +
/// HMAC-SHA256) parses with OpenSSL and `SecPKCS12Import` when the caller
/// explicitly says "this is PKCS#12", but macOS `security import` and the
/// profile installer can reject it as a password/MAC authentication error
/// when they auto-detect the payload. PBES1 3DES + HMAC-SHA1 matches the
/// shape Apple's importers accept from `.mobileconfig` payloads.
fn build_apple_compatible_p12(
    key: &KeyPair,
    cert: &Certificate,
    chain: &[Certificate],
    friendly_name: &str,
    password: &str,
) -> AccessResult<Vec<u8>> {
    // Leaf (client) cert first, CA(s) after — the order p12-keystore and
    // Apple both expect (entity first, root last).
    let mut certs = Vec::with_capacity(1 + chain.len());
    certs.push(
        P12Certificate::from_der(cert.der())
            .map_err(|e| AccessError(format!("p12 leaf cert: {e}")))?,
    );
    for c in chain {
        certs.push(
            P12Certificate::from_der(c.der())
                .map_err(|e| AccessError(format!("p12 ca cert: {e}")))?,
        );
    }

    // Private key as PKCS#8 DER bytes.
    let key_der = key.serialize_der();
    let entry = KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(
        &key_der,
        friendly_name.as_bytes(),
        certs,
    ));

    let mut store = KeyStore::new();
    store.add_entry(friendly_name, entry);

    store
        .writer(password)
        .encryption_algorithm(EncryptionAlgorithm::PbeWithShaAnd3KeyTripleDesCbc)
        .encryption_iterations(2048)
        .mac_algorithm(MacAlgorithm::HmacSha1)
        .mac_iterations(2048)
        .write()
        .map_err(|e| AccessError(format!("p12 write: {e}")))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn random_password(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

fn write_pem_cert(path: &Path, cert: &Certificate) -> AccessResult<()> {
    std::fs::write(path, cert.pem())?;
    Ok(())
}

fn write_pem_private_key(path: &Path, key: &KeyPair) -> AccessResult<()> {
    std::fs::write(path, key.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Read a PEM-encoded certificate file and return its DER bytes.
pub(crate) fn read_cert_der(path: &Path) -> AccessResult<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let pem = pem::parse(&bytes).map_err(|e| AccessError(format!("parse cert PEM: {e}")))?;
    Ok(pem.into_contents())
}

#[cfg(test)]
mod tests {
    use super::*;
    use p12_keystore::KeyStore;
    use tempfile::TempDir;

    #[test]
    fn dir_is_virgin_only_without_any_access_material() {
        let tmp = TempDir::new().unwrap();
        assert!(dir_is_virgin(tmp.path()));
        assert!(dir_is_virgin(&tmp.path().join("never-created")));
        // A single stray file — even just half a server pair — must block
        // self-provisioning: it means someone has been here before.
        std::fs::write(tmp.path().join("server.crt"), "x").unwrap();
        assert!(!dir_is_virgin(tmp.path()));
        std::fs::remove_file(tmp.path().join("server.crt")).unwrap();
        std::fs::write(tmp.path().join("ca.key"), "x").unwrap();
        assert!(!dir_is_virgin(tmp.path()));
    }

    fn names(ip: &str) -> ServerNames {
        ServerNames::new(
            ip.parse().unwrap(),
            Vec::<IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap()
    }

    #[test]
    fn ensure_certs_produces_full_chain() {
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), &names("192.168.1.100"), "test-host", false).unwrap();
        for name in [
            "ca.crt",
            "ca.key",
            "server.crt",
            "server.key",
            "client.crt",
            "client.key",
            "client.p12",
        ] {
            let p = tmp.path().join(name);
            assert!(p.exists(), "missing: {}", p.display());
        }
        assert!(!state.p12_password.is_empty());
    }

    #[test]
    fn ensure_certs_produces_rsa_certificate_payloads() {
        use x509_parser::oid_registry::OID_PKCS1_RSAENCRYPTION;
        use x509_parser::prelude::*;

        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("192.168.1.100"), "rsa-test", false).unwrap();

        for name in [CA_CRT, SERVER_CRT, CLIENT_CRT] {
            let der = read_cert_der(&tmp.path().join(name)).unwrap();
            let (_, cert) = X509Certificate::from_der(&der).unwrap();
            assert_eq!(
                cert.public_key().algorithm.algorithm,
                OID_PKCS1_RSAENCRYPTION,
                "{name} must use an RSA public key for broad Apple profile compatibility"
            );
        }
    }

    #[test]
    fn server_cert_cn_matches_primary_ip() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.42"), "label", false).unwrap();
        let ip = current_cert_ip(tmp.path()).unwrap();
        assert_eq!(ip, "10.0.0.42");
    }

    #[test]
    fn server_cert_includes_loopback_and_access_sans() {
        use x509_parser::extensions::GeneralName;
        use x509_parser::prelude::*;

        let tmp = TempDir::new().unwrap();
        let server_names = ServerNames::new(
            "10.0.0.42".parse().unwrap(),
            vec!["192.168.1.42".parse().unwrap()],
            vec!["station.example.test".to_string()],
        )
        .unwrap();
        ensure_certs(tmp.path(), &server_names, "label", false).unwrap();

        let der = read_cert_der(&tmp.path().join(SERVER_CRT)).unwrap();
        let (_, cert) = X509Certificate::from_der(&der).unwrap();
        let san = cert
            .subject_alternative_name()
            .unwrap()
            .expect("server cert must include SANs");
        let names = &san.value.general_names;
        assert!(
            names.iter().any(|name| matches!(
                name,
                GeneralName::IPAddress(bytes) if *bytes == [127, 0, 0, 1]
            )),
            "server cert should include 127.0.0.1"
        );
        assert!(
            names.iter().any(|name| matches!(
                name,
                GeneralName::IPAddress(bytes) if *bytes == [192, 168, 1, 42]
            )),
            "server cert should include additional access IPs"
        );
        assert!(
            names.iter().any(|name| matches!(
                name,
                GeneralName::DNSName(name) if *name == "localhost"
            )),
            "server cert should include localhost"
        );
        assert!(
            names.iter().any(|name| matches!(
                name,
                GeneralName::DNSName(name) if *name == "station.example.test"
            )),
            "server cert should include explicit DNS SANs"
        );
    }

    /// `read_server_cert_fingerprint` returns `None` when no cert
    /// is present. The caller (`build_local_advertised_auth`) treats
    /// this as a configuration error for `pin-self-cert` since the
    /// pin would be empty.
    #[test]
    fn read_server_cert_fingerprint_returns_none_when_no_cert() {
        let tmp = TempDir::new().unwrap();
        assert!(read_server_cert_fingerprint(tmp.path()).is_none());
    }

    /// `read_server_cert_fingerprint` returns a 64-char lowercase
    /// hex string for an existing cert, matching the format
    /// `parse_fingerprint` consumes. Same SHA-256 a connecting
    /// peer's `PinnedFingerprintVerifier` will compute on the wire,
    /// so the pin matches.
    #[test]
    fn read_server_cert_fingerprint_matches_pinning_format() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.99"), "fp-test", false).unwrap();

        let fp = read_server_cert_fingerprint(tmp.path()).expect("cert exists");
        assert_eq!(fp.len(), 64, "lowercase hex, no separators");
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "all chars must be lowercase hex, got: {fp}"
        );
        // Round-trips through the pinning parser — same byte sequence
        // a connecting peer's verifier consumes.
        let parsed = crate::access::pinning::parse_fingerprint(&fp).unwrap();
        let reformatted = crate::access::pinning::format_fingerprint(&parsed);
        assert_eq!(fp, reformatted);
    }

    /// `read_server_cert_fingerprint` is deterministic: same cert →
    /// same fingerprint. Recerting (which writes a new cert) changes
    /// the fingerprint.
    #[test]
    fn read_server_cert_fingerprint_changes_on_recert() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.1"), "label", false).unwrap();
        let before = read_server_cert_fingerprint(tmp.path()).unwrap();

        recert(tmp.path(), &names("10.0.0.2"), false).unwrap();
        let after = read_server_cert_fingerprint(tmp.path()).unwrap();

        assert_ne!(
            before, after,
            "fingerprint must change when the cert is regenerated"
        );
    }

    #[test]
    fn issue_client_identity_uses_existing_access_ca() {
        use x509_parser::prelude::*;

        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.1"), "issuer", false).unwrap();

        let identity = issue_client_identity(tmp.path(), "peer-a").unwrap();
        assert!(identity.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(identity.key_pem.contains("BEGIN"));

        let pem = ::pem::parse(identity.cert_pem.as_bytes()).unwrap();
        let der = pem.into_contents();
        let (_, cert) = X509Certificate::from_der(&der).unwrap();
        let eku = cert
            .extended_key_usage()
            .unwrap()
            .expect("client cert must carry EKU");
        assert!(
            eku.value.client_auth,
            "issued peer identity must be valid for TLS client auth"
        );
    }

    #[test]
    fn issue_client_identity_requires_access_ca() {
        let tmp = TempDir::new().unwrap();
        let err = issue_client_identity(tmp.path(), "peer-a").unwrap_err();
        assert!(err.to_string().contains("intendant access setup"));
    }

    #[test]
    fn recert_regenerates_on_ip_change() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.1"), "label", false).unwrap();
        let before = std::fs::read(tmp.path().join("server.crt")).unwrap();

        recert(tmp.path(), &names("10.0.0.2"), false).unwrap();
        let after = std::fs::read(tmp.path().join("server.crt")).unwrap();
        assert_ne!(before, after, "server cert did not change on recert");
        assert_eq!(current_cert_ip(tmp.path()).unwrap(), "10.0.0.2");

        // CA should be unchanged.
        let ca1 = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        let ca2 = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        assert_eq!(ca1, ca2);
    }

    /// **Real Apple importer acceptance.** Round-tripping through the
    /// `p12-keystore` crate (see `p12_is_parseable_with_password`) only
    /// proves *we* can read what *we* wrote — it doesn't prove Apple's
    /// importer accepts the bundle. This test drives the actual OS importer,
    /// `SecPKCS12Import` (Security.framework), against the generated
    /// `client.p12`.
    ///
    /// It imports into a throwaway keychain created in a `TempDir` (never the
    /// user's login keychain) so it leaves no trace and triggers no
    /// interactive password prompt. A successful import that yields the
    /// client identity plus its cert chain is the proof that a genuine Apple
    /// importer — not just the `p12-keystore` reader — accepts the bundle.
    ///
    /// macOS-only: `SecPKCS12Import` and `SecKeychain` are Security.framework
    /// APIs, so the test is gated to `target_os = "macos"`.
    #[cfg(target_os = "macos")]
    #[test]
    fn p12_imports_via_real_macos_keychain() {
        use security_framework::import_export::Pkcs12ImportOptions;
        use security_framework::os::macos::keychain::CreateOptions;

        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), &names("10.0.0.1"), "keychain-import", false).unwrap();
        let p12_bytes = std::fs::read(tmp.path().join(CLIENT_P12)).unwrap();

        // A disposable, file-backed keychain in the temp dir. Its own
        // password is unrelated to the .p12 password; creating it leaves the
        // login keychain untouched and avoids any UI prompt. `.keychain(...)`
        // is an inherent macOS-only method on `Pkcs12ImportOptions`.
        let keychain = CreateOptions::new()
            .password("intendant-test-keychain")
            .create(tmp.path().join("import-test.keychain"))
            .expect("create temporary keychain");

        // Drive Apple's SecPKCS12Import. If the package shape is not accepted
        // by the real importer, this errors.
        let identities = Pkcs12ImportOptions::new()
            .passphrase(&state.p12_password)
            .keychain(keychain)
            .import(&p12_bytes)
            .expect("SecPKCS12Import must accept the generated client.p12");

        assert_eq!(
            identities.len(),
            1,
            "expected exactly one imported identity from client.p12"
        );
        let imported = &identities[0];
        assert!(
            imported.identity.is_some(),
            "imported item must carry a SecIdentity (private key + leaf cert)"
        );
        // Leaf (client) cert + CA in the validated chain.
        let chain_len = imported
            .cert_chain
            .as_ref()
            .map(|c| c.len())
            .unwrap_or_default();
        assert!(
            chain_len >= 1,
            "imported identity must carry at least the leaf cert in its chain, got {chain_len}"
        );
    }

    /// The `.mobileconfig` installer appears to take the same auto-detection
    /// path as `security import` without `-f pkcs12`. The modern PBES2/AES
    /// writer output parsed when the format was forced but failed here with
    /// "MAC verification failed", which surfaced in System Settings as a
    /// certificate authentication error. Keep this CLI-level assertion because
    /// `SecPKCS12Import` alone was not enough coverage.
    #[cfg(target_os = "macos")]
    #[test]
    fn p12_imports_via_security_cli_auto_detection() {
        use std::process::Command;

        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), &names("10.0.0.1"), "security-import", false).unwrap();
        let p12_path = tmp.path().join(CLIENT_P12);
        let keychain_path = tmp.path().join("security-import.keychain");

        let create = Command::new("security")
            .arg("create-keychain")
            .arg("-p")
            .arg("intendant-test-keychain")
            .arg(&keychain_path)
            .output()
            .expect("run security create-keychain");
        assert!(
            create.status.success(),
            "create-keychain failed: stdout={} stderr={}",
            String::from_utf8_lossy(&create.stdout),
            String::from_utf8_lossy(&create.stderr)
        );

        let unlock = Command::new("security")
            .arg("unlock-keychain")
            .arg("-p")
            .arg("intendant-test-keychain")
            .arg(&keychain_path)
            .output()
            .expect("run security unlock-keychain");
        assert!(
            unlock.status.success(),
            "unlock-keychain failed: stdout={} stderr={}",
            String::from_utf8_lossy(&unlock.stdout),
            String::from_utf8_lossy(&unlock.stderr)
        );

        let import = Command::new("security")
            .arg("import")
            .arg(&p12_path)
            .arg("-P")
            .arg(&state.p12_password)
            .arg("-k")
            .arg(&keychain_path)
            .output()
            .expect("run security import");

        let _ = Command::new("security")
            .arg("delete-keychain")
            .arg(&keychain_path)
            .output();

        assert!(
            import.status.success(),
            "security import auto-detection failed: stdout={} stderr={}",
            String::from_utf8_lossy(&import.stdout),
            String::from_utf8_lossy(&import.stderr)
        );
    }

    /// The generated PKCS#12 parses back with its password, yields the
    /// client identity, and carries the CA in the chain. Uses
    /// `p12-keystore`'s own reader.
    #[test]
    fn p12_is_parseable_with_password() {
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), &names("10.0.0.1"), "test", false).unwrap();
        let bytes = std::fs::read(tmp.path().join("client.p12")).unwrap();

        let store = KeyStore::from_pkcs12(&bytes, &state.p12_password).expect("p12 parse");
        let (_alias, chain) = store
            .private_key_chain()
            .expect("private key chain missing from p12");
        assert!(!chain.key().is_empty(), "client key missing from p12");
        // Leaf (client) + CA.
        assert_eq!(chain.chain().len(), 2, "expected client + CA in the chain");
    }

    #[test]
    fn idempotent_reuses_existing_certs() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.1"), "label", false).unwrap();
        let ca_before = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        let client_before = std::fs::read(tmp.path().join("client.p12")).unwrap();

        ensure_certs(tmp.path(), &names("10.0.0.1"), "label", false).unwrap();
        let ca_after = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        let client_after = std::fs::read(tmp.path().join("client.p12")).unwrap();

        assert_eq!(ca_before, ca_after);
        assert_eq!(client_before, client_after);
    }

    /// Drops a freshly-generated p12 into /tmp so it can be inspected
    /// with `openssl pkcs12 -info` or imported via `SecPKCS12Import`.
    /// Gated behind an env var so it only runs when we explicitly want
    /// a sample.
    #[test]
    fn dump_sample_p12() {
        if std::env::var("ACCESS_DUMP_SAMPLE_P12").is_err() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), &names("10.0.0.1"), "sample", false).unwrap();
        std::fs::copy(tmp.path().join("client.p12"), "/tmp/sample.p12").unwrap();
        std::fs::write("/tmp/sample-p12-password", &state.p12_password).unwrap();
        eprintln!("wrote /tmp/sample.p12 (password in /tmp/sample-p12-password)");
    }

    #[test]
    fn force_regenerates_everything() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), &names("10.0.0.1"), "label", false).unwrap();
        let ca_before = std::fs::read(tmp.path().join("ca.crt")).unwrap();

        ensure_certs(tmp.path(), &names("10.0.0.1"), "label", true).unwrap();
        let ca_after = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        assert_ne!(ca_before, ca_after, "force did not regenerate CA");
    }
}
