use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::project::{CustomDomainDnsConfig, ValidatedCustomDomain};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::CertificateStatus;

const CERT_FILE: &str = "custom-domain-cert.pem";
const KEY_FILE: &str = "custom-domain-key.pem";
const CERT_PAIR_FILE: &str = "custom-domain-certificate-pair.json";
const CERT_PAIR_SCHEMA_VERSION: u32 = 1;
const CERT_PAIR_MAX_BYTES: u64 = 512 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const ERROR_RETRY_INITIAL: Duration = Duration::from_secs(30);
const ERROR_RETRY_MAX: Duration = Duration::from_secs(30 * 60);
const RENEW_BEFORE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const DNS_CHALLENGE_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30 * 60);
const ISSUANCE_LEASE_FILE: &str = "custom-domain-cert-issuance.json";
const ISSUANCE_LEASE_SCHEMA_VERSION: u32 = 2;
const ISSUANCE_LEASE_MAX_BYTES: u64 = 128 * 1024;
const ISSUANCE_LEASE_MS: u64 = 45 * 60 * 1000;
const ISSUANCE_LOCAL_TTL_MS: u64 = 2 * 60 * 60 * 1000;
const ISSUANCE_RESUMABLE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificatePairRecord {
    schema_version: u32,
    name: String,
    certificate_chain_pem: String,
    private_key_pem: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuanceLeaseRecord {
    schema_version: u32,
    name: String,
    #[serde(default)]
    started_unix_ms: u64,
    #[serde(default)]
    updated_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_token: Option<String>,
    #[serde(default, alias = "expires_unix_ms")]
    owner_lease_expires_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_key_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    csr_der_b64: Option<String>,
}

enum IssuanceClaim {
    CertificateCurrent,
    Acquired(Box<IssuanceLeaseGuard>),
}

struct IssuanceLeaseGuard {
    cert_dir: PathBuf,
    record: IssuanceLeaseRecord,
    owner_token: String,
    active: bool,
}

pub(super) fn restore(
    domain: &ValidatedCustomDomain,
    cert_dir: &Path,
    status: &Arc<RwLock<CertificateStatus>>,
) -> Result<(), String> {
    let account_error = match crate::fleet_cert::acme_account_uri_in(cert_dir) {
        Ok(account_uri) => {
            update_status(status, |current| current.acme_account_uri = account_uri);
            None
        }
        Err(error) => Some(error),
    };
    let (cert_pem, key_pem) = match read_stored_certificate_pair(cert_dir)? {
        Some(pair) => pair,
        None => {
            update_status(status, |current| {
                current.state = "pending".to_string();
                current.not_after_unix_ms = None;
                current.restore_error = None;
                current.last_error = account_error;
            });
            return Ok(());
        }
    };
    require_exact_dns_name(&cert_pem, &domain.name)?;
    crate::web_tls::validate_custom_domain_certificate_key_pair(&cert_pem, &key_pem)?;
    let not_after = crate::fleet_cert::cert_not_after_unix_ms(&cert_pem)
        .ok_or_else(|| "custom-domain certificate has no usable expiry".to_string())?;
    if not_after <= now_unix_ms() {
        update_status(status, |current| {
            current.state = "expired".to_string();
            current.not_after_unix_ms = Some(not_after);
            current.restore_error = None;
            current.last_error = account_error;
        });
        return Ok(());
    }
    crate::web_tls::install_custom_domain_certificate(&domain.name, &cert_pem, &key_pem)?;
    update_status(status, |current| {
        current.state = "valid".to_string();
        current.not_after_unix_ms = Some(not_after);
        current.restore_error = None;
        current.last_error = account_error;
    });
    Ok(())
}

fn read_stored_certificate_pair(cert_dir: &Path) -> Result<Option<(String, String)>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let pair_path = cert_dir.join(CERT_PAIR_FILE);
        match read_certificate_pair_record_locked(&pair_path) {
            Ok(Some(record)) => {
                return Ok(Some((record.certificate_chain_pem, record.private_key_pem)));
            }
            Ok(None) => {}
            Err(error) => return Err(crate::access::AccessError(error)),
        }

        // Compatibility with the original two-file layout. Every new commit
        // uses the single atomic record above; a valid legacy pair is still
        // accepted until the next issuance writes a generation record.
        let cert_path = cert_dir.join(CERT_FILE);
        let key_path = cert_dir.join(KEY_FILE);
        match (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
        ) {
            (Ok(cert), Ok(key)) => Ok(Some((cert, key))),
            (Err(cert_error), Err(key_error))
                if cert_error.kind() == std::io::ErrorKind::NotFound
                    && key_error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            (Ok(_), Err(error)) | (Err(error), Ok(_))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                // The original layout committed key and certificate as two
                // independent replacements. A missing sibling is an
                // incomplete legacy generation, not durable authority; the
                // guarded issuance path may atomically replace it.
                Ok(None)
            }
            (Err(error), _) => Err(crate::access::AccessError(format!(
                "read {}: {error}",
                cert_path.display()
            ))),
            (_, Err(error)) => Err(crate::access::AccessError(format!(
                "read {}: {error}",
                key_path.display()
            ))),
        }
    })
    .map_err(|error| error.to_string())
}

fn read_certificate_pair_record_locked(
    path: &Path,
) -> Result<Option<CertificatePairRecord>, String> {
    use std::io::Read as _;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(CERT_PAIR_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > CERT_PAIR_MAX_BYTES {
        return Err(format!(
            "{} exceeds the custom-domain certificate-pair size cap",
            path.display()
        ));
    }
    let record: CertificatePairRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if record.schema_version != CERT_PAIR_SCHEMA_VERSION
        || record.name.is_empty()
        || record.name.len() > 253
        || record.certificate_chain_pem.is_empty()
        || record.private_key_pem.is_empty()
    {
        return Err(format!(
            "{} contains an invalid custom-domain certificate pair",
            path.display()
        ));
    }
    Ok(Some(record))
}

fn write_certificate_pair_locked(
    cert_dir: &Path,
    domain: &ValidatedCustomDomain,
    certificate_chain_pem: &str,
    private_key_pem: &str,
) -> crate::access::AccessResult<()> {
    let bytes = serde_json::to_vec(&CertificatePairRecord {
        schema_version: CERT_PAIR_SCHEMA_VERSION,
        name: domain.name.clone(),
        certificate_chain_pem: certificate_chain_pem.to_string(),
        private_key_pem: private_key_pem.to_string(),
    })
    .map_err(|error| {
        crate::access::AccessError(format!("serialize custom-domain certificate pair: {error}"))
    })?;
    if bytes.len() as u64 > CERT_PAIR_MAX_BYTES {
        return Err(crate::access::AccessError(
            "custom-domain certificate pair exceeds its size cap".to_string(),
        ));
    }
    crate::access::authority_store::atomic_write_private_locked(
        &cert_dir.join(CERT_PAIR_FILE),
        &bytes,
    )
}

fn issuance_lease_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(ISSUANCE_LEASE_FILE)
}

fn load_issuance_lease_locked(cert_dir: &Path) -> Result<Option<IssuanceLeaseRecord>, String> {
    use std::io::Read as _;

    let path = issuance_lease_path(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(ISSUANCE_LEASE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > ISSUANCE_LEASE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the custom-domain issuance-lease size cap",
            path.display()
        ));
    }
    let mut record: IssuanceLeaseRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if record.schema_version == 1 {
        record.schema_version = ISSUANCE_LEASE_SCHEMA_VERSION;
        record.started_unix_ms = record.started_unix_ms.max(record.updated_unix_ms).max(
            record
                .owner_lease_expires_unix_ms
                .saturating_sub(ISSUANCE_LEASE_MS),
        );
        record.updated_unix_ms = record.started_unix_ms;
    }
    if record.schema_version != ISSUANCE_LEASE_SCHEMA_VERSION
        || record.name.is_empty()
        || record.name.len() > 253
        || record.owner_token.as_ref().is_some_and(|token| {
            token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || record
            .order_url
            .as_ref()
            .is_some_and(|url| url.is_empty() || url.len() > 4096)
        || record
            .private_key_pem
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > 32 * 1024)
        || record
            .csr_der_b64
            .as_ref()
            .is_some_and(|csr| csr.is_empty() || csr.len() > 32 * 1024)
        || record.private_key_pem.is_some() != record.csr_der_b64.is_some()
    {
        return Err(format!(
            "{} contains an invalid custom-domain issuance lease",
            path.display()
        ));
    }
    let now = now_unix_ms();
    let age = now.saturating_sub(record.started_unix_ms);
    if (record.order_url.is_some() && age >= ISSUANCE_RESUMABLE_TTL_MS)
        || (record.order_url.is_none() && age >= ISSUANCE_LOCAL_TTL_MS)
    {
        return Ok(None);
    }
    Ok(Some(record))
}

fn write_issuance_lease_locked(
    cert_dir: &Path,
    record: &IssuanceLeaseRecord,
) -> crate::access::AccessResult<()> {
    let bytes = serde_json::to_vec(record).map_err(|error| {
        crate::access::AccessError(format!(
            "serialize custom-domain certificate issuance lease: {error}"
        ))
    })?;
    if bytes.len() as u64 > ISSUANCE_LEASE_MAX_BYTES {
        return Err(crate::access::AccessError(
            "custom-domain certificate issuance state exceeds its size cap".to_string(),
        ));
    }
    crate::access::authority_store::atomic_write_private_locked(
        &issuance_lease_path(cert_dir),
        &bytes,
    )
}

fn stored_certificate_is_current_locked(
    domain: &ValidatedCustomDomain,
    cert_dir: &Path,
) -> Result<bool, String> {
    let Some((cert_pem, key_pem)) = read_stored_certificate_pair(cert_dir)? else {
        return Ok(false);
    };
    if require_exact_dns_name(&cert_pem, &domain.name).is_err()
        || crate::web_tls::validate_custom_domain_certificate_key_pair(&cert_pem, &key_pem).is_err()
    {
        return Ok(false);
    }
    let Some(not_after) = crate::fleet_cert::cert_not_after_unix_ms(&cert_pem) else {
        return Ok(false);
    };
    Ok(not_after > now_unix_ms().saturating_add(RENEW_BEFORE_MS))
}

impl IssuanceLeaseGuard {
    fn begin(domain: &ValidatedCustomDomain, cert_dir: &Path) -> Result<IssuanceClaim, String> {
        crate::access::authority_store::with_lock(cert_dir, || {
            if stored_certificate_is_current_locked(domain, cert_dir)
                .map_err(crate::access::AccessError)?
            {
                // A committed pair is the terminal issuance record. Remove a
                // retained order left by cancellation after the pair commit.
                crate::access::authority_store::remove_file_locked(&issuance_lease_path(cert_dir))?;
                return Ok(IssuanceClaim::CertificateCurrent);
            }
            let now = now_unix_ms();
            let mut record = load_issuance_lease_locked(cert_dir)
                .map_err(crate::access::AccessError)?
                .filter(|record| record.name == domain.name)
                .unwrap_or_else(|| IssuanceLeaseRecord {
                    schema_version: ISSUANCE_LEASE_SCHEMA_VERSION,
                    name: domain.name.clone(),
                    started_unix_ms: now,
                    updated_unix_ms: now,
                    owner_token: None,
                    owner_lease_expires_unix_ms: 0,
                    order_url: None,
                    private_key_pem: None,
                    csr_der_b64: None,
                });
            if record.owner_token.is_some() && record.owner_lease_expires_unix_ms > now {
                return Err(crate::access::AccessError(
                    "a custom-domain certificate request is already running in another daemon process"
                        .to_string(),
                ));
            }
            let owner_token = uuid::Uuid::new_v4().simple().to_string();
            record.owner_token = Some(owner_token.clone());
            record.owner_lease_expires_unix_ms = now.saturating_add(ISSUANCE_LEASE_MS);
            record.updated_unix_ms = now;
            write_issuance_lease_locked(cert_dir, &record)?;
            Ok(IssuanceClaim::Acquired(Box::new(Self {
                cert_dir: cert_dir.to_path_buf(),
                record,
                owner_token,
                active: true,
            })))
        })
        .map_err(|error| error.to_string())
    }

    fn update(
        &mut self,
        update: impl FnOnce(&mut IssuanceLeaseRecord) -> crate::access::AccessResult<()>,
    ) -> Result<(), String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let mut record = load_issuance_lease_locked(&self.cert_dir)
                .map_err(crate::access::AccessError)?
                .ok_or_else(|| {
                    crate::access::AccessError(
                        "custom-domain certificate issuance lease disappeared".to_string(),
                    )
                })?;
            if record.owner_token.as_deref() != Some(self.owner_token.as_str()) {
                return Err(crate::access::AccessError(
                    "custom-domain certificate issuance ownership changed".to_string(),
                ));
            }
            update(&mut record)?;
            let now = now_unix_ms();
            record.updated_unix_ms = now;
            record.owner_lease_expires_unix_ms = now.saturating_add(ISSUANCE_LEASE_MS);
            write_issuance_lease_locked(&self.cert_dir, &record)?;
            Ok(record)
        })
        .map(|record| self.record = record)
        .map_err(|error| error.to_string())
    }

    fn renew(&mut self) -> Result<(), String> {
        self.update(|_| Ok(()))
    }

    fn order_url(&self) -> Option<&str> {
        self.record.order_url.as_deref()
    }

    fn record_order_url(&mut self, order_url: &str) -> Result<(), String> {
        if order_url.is_empty() || order_url.len() > 4096 {
            return Err("ACME returned an invalid custom-domain order URL".to_string());
        }
        let order_url = order_url.to_string();
        self.update(|record| {
            if record
                .order_url
                .as_deref()
                .is_some_and(|existing| existing != order_url)
            {
                return Err(crate::access::AccessError(
                    "custom-domain ACME order identity changed during recovery".to_string(),
                ));
            }
            record.order_url = Some(order_url);
            Ok(())
        })
    }

    fn restart_order(&mut self) -> Result<(), String> {
        self.update(|record| {
            record.order_url = None;
            record.private_key_pem = None;
            record.csr_der_b64 = None;
            record.started_unix_ms = now_unix_ms();
            Ok(())
        })
    }

    fn finalization_material(&mut self, name: &str) -> Result<(String, Vec<u8>), String> {
        if let (Some(private_key_pem), Some(csr_der_b64)) = (
            self.record.private_key_pem.as_ref(),
            self.record.csr_der_b64.as_ref(),
        ) {
            let csr = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(csr_der_b64)
                .map_err(|error| format!("decode durable custom-domain CSR: {error}"))?;
            return Ok((private_key_pem.clone(), csr));
        }
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()])
            .map_err(|error| format!("build custom-domain certificate CSR: {error}"))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let key = rcgen::KeyPair::generate()
            .map_err(|error| format!("generate custom-domain certificate key: {error}"))?;
        let csr = params
            .serialize_request(&key)
            .map_err(|error| format!("serialize custom-domain certificate CSR: {error}"))?;
        let private_key_pem = key.serialize_pem();
        let csr_der = csr.der().as_ref().to_vec();
        let csr_der_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&csr_der);
        self.update(|record| {
            record.private_key_pem = Some(private_key_pem.clone());
            record.csr_der_b64 = Some(csr_der_b64);
            Ok(())
        })?;
        Ok((private_key_pem, csr_der))
    }

    fn persisted_private_key(&self) -> Result<String, String> {
        self.record.private_key_pem.clone().ok_or_else(|| {
            "custom-domain order reached processing without durable key material".to_string()
        })
    }

    fn finish(mut self) -> Result<(), String> {
        let result = crate::access::authority_store::with_lock(&self.cert_dir, || {
            let Some(record) =
                load_issuance_lease_locked(&self.cert_dir).map_err(crate::access::AccessError)?
            else {
                return Ok(());
            };
            if record.owner_token.as_deref() != Some(self.owner_token.as_str()) {
                return Err(crate::access::AccessError(
                    "custom-domain certificate issuance ownership changed".to_string(),
                ));
            }
            crate::access::authority_store::remove_file_locked(&issuance_lease_path(&self.cert_dir))
        })
        .map_err(|error| error.to_string());
        if result.is_ok() {
            self.active = false;
        }
        result
    }

    fn release(&self) -> Result<(), String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let Some(mut record) =
                load_issuance_lease_locked(&self.cert_dir).map_err(crate::access::AccessError)?
            else {
                return Ok(());
            };
            if record.owner_token.as_deref() != Some(self.owner_token.as_str()) {
                return Ok(());
            }
            record.owner_token = None;
            record.owner_lease_expires_unix_ms = 0;
            record.updated_unix_ms = now_unix_ms();
            write_issuance_lease_locked(&self.cert_dir, &record)
        })
        .map_err(|error| error.to_string())
    }
}

impl Drop for IssuanceLeaseGuard {
    fn drop(&mut self) {
        if self.active {
            if let Err(error) = self.release() {
                eprintln!("[custom-domain] release certificate issuance lease: {error}");
            }
        }
    }
}

pub(super) fn relay_certificate_material(
    domain: &ValidatedCustomDomain,
    cert_dir: &Path,
) -> Result<(String, String), String> {
    let (cert_pem, key_pem) = read_stored_certificate_pair(cert_dir)?
        .ok_or_else(|| "custom-domain certificate is not installed yet".to_string())?;
    require_exact_dns_name(&cert_pem, &domain.name)?;
    let not_after = crate::fleet_cert::cert_not_after_unix_ms(&cert_pem)
        .ok_or_else(|| "custom-domain certificate has no usable expiry".to_string())?;
    if not_after <= now_unix_ms() {
        return Err("custom-domain certificate is expired".to_string());
    }
    crate::web_tls::validate_custom_domain_certificate_key_pair(&cert_pem, &key_pem)?;
    // Certificate files are shared across daemon processes, while the TLS
    // resolver is process-local. Install the exact generation this poll is
    // about to prove before advertising that this process can receive its
    // relay dialbacks.
    crate::web_tls::install_custom_domain_certificate(&domain.name, &cert_pem, &key_pem)?;
    Ok((cert_pem, key_pem))
}

pub(super) fn spawn(
    domain: ValidatedCustomDomain,
    dns: Option<CustomDomainDnsConfig>,
    issuance_enabled: bool,
    cert_dir: PathBuf,
    status: Arc<RwLock<CertificateStatus>>,
    current_fleet_zone_observed: Option<Arc<AtomicBool>>,
) {
    tokio::spawn(async move {
        let mut error_retry = ERROR_RETRY_INITIAL;
        loop {
            if current_fleet_zone_observed
                .as_ref()
                .is_some_and(|observed| !observed.load(Ordering::SeqCst))
            {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            if let Err(error) =
                ensure_certificate(&domain, dns.as_ref(), issuance_enabled, &cert_dir, &status)
                    .await
            {
                update_status(&status, |current| {
                    if current
                        .not_after_unix_ms
                        .is_none_or(|expiry| expiry <= now_unix_ms())
                    {
                        current.state = "error".to_string();
                    }
                    current.last_error = Some(error.clone());
                });
                eprintln!("[custom-domain] certificate check: {error}");
                let credential_granted =
                    crate::credential_leases::wait_for_dns_credential_grant(error_retry).await;
                error_retry = if credential_granted {
                    ERROR_RETRY_INITIAL
                } else {
                    std::cmp::min(error_retry.saturating_mul(2), ERROR_RETRY_MAX)
                };
                continue;
            }
            error_retry = ERROR_RETRY_INITIAL;
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

async fn ensure_certificate(
    domain: &ValidatedCustomDomain,
    dns_config: Option<&CustomDomainDnsConfig>,
    issuance_enabled: bool,
    cert_dir: &Path,
    status: &Arc<RwLock<CertificateStatus>>,
) -> Result<(), String> {
    // A prior crash/cancellation may have left the exact DNS-01 value live.
    // Clear its durable journal before account work or a new order so retries
    // never stack stale authority records.
    if !super::dns::retry_pending_challenge(cert_dir).await? {
        return Err(
            "a custom-domain DNS challenge is active in another daemon process".to_string(),
        );
    }
    if crate::fleet_cert::is_service_controlled_name_in(cert_dir, &domain.name)? {
        return Err(
            "custom-domain name overlaps a service-controlled fleet name or zone".to_string(),
        );
    }
    // Certificate files are shared across daemon processes while this status
    // cache is process-local. Refresh the durable pair before every renewal
    // decision so a sibling that started before issuance cannot place a
    // duplicate order after another process has already committed a valid
    // generation.
    let current_not_after = refresh_shared_certificate_status(domain, cert_dir, status)?;
    let account = crate::fleet_cert::acme_account_in(cert_dir).await?;
    let account_uri = crate::fleet_cert::acme_account_uri_in(cert_dir)?
        .ok_or_else(|| "ACME account was created without an account URI".to_string())?;
    update_status(status, |current| {
        record_account_ready(current, account_uri, issuance_enabled);
    });
    if !issuance_enabled {
        return Ok(());
    }

    if current_not_after
        .is_some_and(|expiry| expiry > now_unix_ms().saturating_add(RENEW_BEFORE_MS))
    {
        return Ok(());
    }
    let dns_config = dns_config.ok_or_else(|| {
        "custom-domain DNS provider is not configured; use the displayed ACME account URI to set CAA before enabling issuance"
            .to_string()
    })?;
    update_status(status, |current| {
        current.state = "requesting".to_string();
        current.last_error = None;
    });
    request_certificate(domain, dns_config, cert_dir, status, account).await
}

fn refresh_shared_certificate_status(
    domain: &ValidatedCustomDomain,
    cert_dir: &Path,
    status: &Arc<RwLock<CertificateStatus>>,
) -> Result<Option<u64>, String> {
    if let Err(error) = restore(domain, cert_dir, status) {
        // A fully readable but unusable legacy pair (including a crash-time
        // key/certificate mismatch) is repairable authority state. Keep it
        // out of the TLS resolver, surface the error, and let the guarded
        // issuance path atomically replace it.
        match read_stored_certificate_pair(cert_dir) {
            Ok(Some(_)) => {
                update_status(status, |current| {
                    current.state = "repairing".to_string();
                    current.not_after_unix_ms = None;
                    current.restore_error = Some(error);
                });
                return Ok(None);
            }
            Ok(None) => return Err(error),
            Err(read_error) => return Err(read_error),
        }
    }
    Ok(status
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .not_after_unix_ms)
}

async fn request_certificate(
    domain: &ValidatedCustomDomain,
    dns_config: &CustomDomainDnsConfig,
    cert_dir: &Path,
    status: &Arc<RwLock<CertificateStatus>>,
    account: instant_acme::Account,
) -> Result<(), String> {
    let mut issuance = match IssuanceLeaseGuard::begin(domain, cert_dir)? {
        IssuanceClaim::CertificateCurrent => {
            refresh_shared_certificate_status(domain, cert_dir, status)?;
            return Ok(());
        }
        IssuanceClaim::Acquired(guard) => *guard,
    };
    let identifiers = [instant_acme::Identifier::Dns(domain.name.clone())];
    let mut order = if let Some(order_url) = issuance.order_url() {
        match account.order(order_url.to_string()).await {
            Ok(order) => order,
            Err(error) if crate::fleet_cert::acme_order_resume_is_terminal(&error) => {
                issuance.restart_order()?;
                let order = account
                    .new_order(&instant_acme::NewOrder::new(&identifiers))
                    .await
                    .map_err(|error| {
                        format!("replace terminal custom-domain ACME order: {error}")
                    })?;
                issuance.record_order_url(order.url())?;
                order
            }
            Err(error) => return Err(format!("resume custom-domain ACME order: {error}")),
        }
    } else {
        let order = account
            .new_order(&instant_acme::NewOrder::new(&identifiers))
            .await
            .map_err(|error| format!("custom-domain ACME new order: {error}"))?;
        issuance.record_order_url(order.url())?;
        order
    };
    issuance.renew()?;

    let mut dns_challenge_lease = None;
    let authorization_result: Result<instant_acme::OrderStatus, String> = async {
        let mut order_status = order.state().status;
        if order_status == instant_acme::OrderStatus::Pending {
            {
                let mut authorizations = order.authorizations();
                while let Some(result) = authorizations.next().await {
                    let mut authorization = result
                        .map_err(|error| format!("custom-domain ACME authorization: {error}"))?;
                    match authorization.status {
                        instant_acme::AuthorizationStatus::Pending => {}
                        instant_acme::AuthorizationStatus::Valid => continue,
                        other => {
                            return Err(format!(
                                "custom-domain ACME authorization in unexpected state {other:?}"
                            ));
                        }
                    }
                    let mut challenge = authorization
                        .challenge(instant_acme::ChallengeType::Dns01)
                        .ok_or_else(|| {
                            "custom-domain ACME order offers no dns-01 challenge".to_string()
                        })?;
                    let value = challenge.key_authorization().dns_value();
                    let lease =
                        super::dns::set_challenge_in(dns_config, &domain.name, &value, cert_dir)
                            .await?;
                    dns_challenge_lease = Some(lease);
                    issuance.renew()?;
                    let mut propagation_wait =
                        Duration::from_secs(super::dns::propagation_delay_secs(dns_config));
                    while !propagation_wait.is_zero() {
                        let wait = propagation_wait.min(DNS_CHALLENGE_LEASE_RENEW_INTERVAL);
                        tokio::time::sleep(wait).await;
                        propagation_wait = propagation_wait.saturating_sub(wait);
                        super::dns::renew_pending_challenge(
                            cert_dir,
                            dns_challenge_lease
                                .as_ref()
                                .expect("DNS challenge lease was just stored"),
                        )?;
                        issuance.renew()?;
                    }
                    challenge
                        .set_ready()
                        .await
                        .map_err(|error| format!("custom-domain ACME challenge ready: {error}"))?;
                }
            }
            issuance.renew()?;
            order_status = order
                .poll_ready(&instant_acme::RetryPolicy::default())
                .await
                .map_err(|error| format!("custom-domain ACME validation: {error}"))?;
        }
        Ok(order_status)
    }
    .await;

    let cleanup_result = if let Some(lease) = dns_challenge_lease.as_ref() {
        match super::dns::mark_pending_challenge_cleanup(cert_dir, lease) {
            Ok(()) => match super::dns::retry_pending_challenge(cert_dir).await {
                Ok(true) => Ok(()),
                Ok(false) => Err("custom-domain DNS-01 cleanup is still leased".to_string()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    if let Err(cleanup_error) = cleanup_result {
        return Err(match authorization_result {
            Ok(_) => format!("custom-domain DNS-01 cleanup remains pending: {cleanup_error}"),
            Err(authorization_error) => format!(
                "{authorization_error}; custom-domain DNS-01 cleanup remains pending: {cleanup_error}"
            ),
        });
    }
    let order_status = authorization_result?;
    if order_status == instant_acme::OrderStatus::Invalid {
        issuance.finish()?;
        return Err("custom-domain ACME order became invalid".to_string());
    }
    let private_key_pem = match order_status {
        instant_acme::OrderStatus::Ready => {
            let (private_key_pem, csr_der) = issuance.finalization_material(&domain.name)?;
            order
                .finalize_csr(&csr_der)
                .await
                .map_err(|error| format!("custom-domain ACME finalize: {error}"))?;
            private_key_pem
        }
        instant_acme::OrderStatus::Processing | instant_acme::OrderStatus::Valid => {
            issuance.persisted_private_key()?
        }
        other => {
            return Err(format!(
                "custom-domain ACME order cannot be resumed from state {other:?}"
            ));
        }
    };
    let cert_chain_pem = order
        .poll_certificate(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|error| format!("custom-domain ACME certificate: {error}"))?;
    issuance.renew()?;
    require_exact_dns_name(&cert_chain_pem, &domain.name)?;
    crate::web_tls::validate_custom_domain_certificate_key_pair(&cert_chain_pem, &private_key_pem)?;
    let not_after = crate::fleet_cert::cert_not_after_unix_ms(&cert_chain_pem)
        .ok_or_else(|| "custom-domain certificate has no usable expiry".to_string())?;
    crate::access::authority_store::with_lock(cert_dir, || {
        if crate::fleet_cert::is_service_controlled_name_in(cert_dir, &domain.name)
            .map_err(crate::access::AccessError)?
        {
            return Err(crate::access::AccessError(
                "custom-domain name became service-controlled while issuance was running"
                    .to_string(),
            ));
        }
        write_certificate_pair_locked(cert_dir, domain, &cert_chain_pem, &private_key_pem)
    })
    .map_err(|error| error.to_string())?;
    crate::web_tls::install_custom_domain_certificate(
        &domain.name,
        &cert_chain_pem,
        &private_key_pem,
    )?;
    update_status(status, |current| {
        current.state = "valid".to_string();
        current.not_after_unix_ms = Some(not_after);
        current.restore_error = None;
        current.last_error = None;
    });
    issuance.finish()?;
    Ok(())
}

fn record_account_ready(
    status: &mut CertificateStatus,
    account_uri: String,
    issuance_enabled: bool,
) {
    status.acme_account_uri = Some(account_uri);
    status.last_error = None;
    if issuance_enabled {
        return;
    }
    status.state = match status.not_after_unix_ms {
        Some(expiry) if expiry > now_unix_ms() => "valid",
        Some(_) => "expired",
        None if status.restore_error.is_some() => "error",
        None => "waiting_for_caa",
    }
    .to_string();
}

fn require_exact_dns_name(cert_pem: &str, expected: &str) -> Result<(), String> {
    use rustls::pki_types::pem::PemObject as _;
    use x509_parser::extensions::GeneralName;
    use x509_parser::prelude::*;

    let leaf = rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .next()
        .transpose()
        .map_err(|error| format!("parse custom-domain certificate PEM: {error}"))?
        .ok_or_else(|| "custom-domain certificate PEM holds no certificates".to_string())?;
    let (_, certificate) = X509Certificate::from_der(leaf.as_ref())
        .map_err(|error| format!("parse custom-domain certificate: {error}"))?;
    let san = certificate
        .subject_alternative_name()
        .map_err(|error| format!("parse custom-domain certificate SAN: {error}"))?
        .ok_or_else(|| "custom-domain certificate has no subjectAltName extension".to_string())?;
    let exact = matches!(
        san.value.general_names.as_slice(),
        [GeneralName::DNSName(name)]
            if name.trim().trim_end_matches('.').eq_ignore_ascii_case(expected)
    );
    if exact {
        Ok(())
    } else {
        Err(format!(
            "custom-domain certificate SANs must equal the configured exact name {expected}"
        ))
    }
}

fn update_status(
    status: &Arc<RwLock<CertificateStatus>>,
    update: impl FnOnce(&mut CertificateStatus),
) {
    let mut status = status.write().unwrap_or_else(|error| error.into_inner());
    update(&mut status);
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_certificate_requires_only_the_exact_configured_san() {
        let cert =
            rcgen::generate_simple_self_signed(vec!["box.example.test".to_string()]).unwrap();
        assert!(require_exact_dns_name(&cert.cert.pem(), "box.example.test").is_ok());
        assert!(require_exact_dns_name(&cert.cert.pem(), "other.example.test").is_err());

        let cert = rcgen::generate_simple_self_signed(vec![
            "box.example.test".to_string(),
            "other.example.test".to_string(),
        ])
        .unwrap();
        assert!(require_exact_dns_name(&cert.cert.pem(), "box.example.test").is_err());
    }

    #[test]
    fn restore_rejects_a_certificate_and_key_from_different_writes() {
        let dir = tempfile::tempdir().unwrap();
        let certificate =
            rcgen::generate_simple_self_signed(vec!["box.example.test".to_string()]).unwrap();
        let other =
            rcgen::generate_simple_self_signed(vec!["other.example.test".to_string()]).unwrap();
        std::fs::write(dir.path().join(CERT_FILE), certificate.cert.pem()).unwrap();
        std::fs::write(dir.path().join(KEY_FILE), other.signing_key.serialize_pem()).unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let status = Arc::new(RwLock::new(CertificateStatus::default()));

        let error = restore(&domain, dir.path(), &status).unwrap_err();
        assert!(
            error.contains("custom-domain certificate and key do not match"),
            "{error}"
        );
        assert_ne!(
            status.read().unwrap().state,
            "valid",
            "a partial certificate/key update must not be restored as usable"
        );
    }

    #[test]
    fn stored_pair_read_waits_for_the_atomic_writer_transaction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CERT_FILE), b"old certificate").unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"old key").unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };

        let (writer_locked_tx, writer_locked_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let writer_dir = dir.path().to_path_buf();
        let writer_domain = domain.clone();
        let writer = std::thread::spawn(move || {
            crate::access::authority_store::with_lock(&writer_dir, || {
                writer_locked_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                write_certificate_pair_locked(
                    &writer_dir,
                    &writer_domain,
                    "new certificate",
                    "new key",
                )
            })
        });
        writer_locked_rx.recv().unwrap();

        let (reader_started_tx, reader_started_rx) = std::sync::mpsc::channel();
        let (pair_tx, pair_rx) = std::sync::mpsc::channel();
        let reader_dir = dir.path().to_path_buf();
        let reader = std::thread::spawn(move || {
            reader_started_tx.send(()).unwrap();
            pair_tx
                .send(read_stored_certificate_pair(&reader_dir))
                .unwrap();
        });
        reader_started_rx.recv().unwrap();
        assert!(
            pair_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "reader observed a partially replaced certificate pair"
        );
        finish_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(
            pair_rx.recv().unwrap().unwrap(),
            Some(("new certificate".to_string(), "new key".to_string()))
        );
        reader.join().unwrap();
    }

    #[test]
    fn atomic_pair_repairs_a_mismatched_legacy_generation() {
        let dir = tempfile::tempdir().unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let certificate = rcgen::generate_simple_self_signed(vec![domain.name.clone()]).unwrap();
        let other =
            rcgen::generate_simple_self_signed(vec!["other.example.test".to_string()]).unwrap();
        std::fs::write(dir.path().join(CERT_FILE), certificate.cert.pem()).unwrap();
        std::fs::write(dir.path().join(KEY_FILE), other.signing_key.serialize_pem()).unwrap();
        let status = Arc::new(RwLock::new(CertificateStatus::default()));

        assert_eq!(
            refresh_shared_certificate_status(&domain, dir.path(), &status).unwrap(),
            None
        );
        assert_eq!(status.read().unwrap().state, "repairing");

        crate::access::authority_store::with_lock(dir.path(), || {
            write_certificate_pair_locked(
                dir.path(),
                &domain,
                &certificate.cert.pem(),
                &certificate.signing_key.serialize_pem(),
            )
        })
        .unwrap();
        let expiry = refresh_shared_certificate_status(&domain, dir.path(), &status).unwrap();
        assert!(expiry.is_some());
        let status = status.read().unwrap();
        assert_eq!(status.state, "valid");
        assert_eq!(status.restore_error, None);
    }

    #[test]
    fn partial_legacy_pair_is_repairable() {
        let dir = tempfile::tempdir().unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let certificate = rcgen::generate_simple_self_signed(vec![domain.name.clone()]).unwrap();
        std::fs::write(
            dir.path().join(KEY_FILE),
            certificate.signing_key.serialize_pem(),
        )
        .unwrap();
        let status = Arc::new(RwLock::new(CertificateStatus {
            state: "valid".to_string(),
            not_after_unix_ms: Some(u64::MAX),
            restore_error: Some("stale restore error".to_string()),
            ..Default::default()
        }));

        assert_eq!(
            refresh_shared_certificate_status(&domain, dir.path(), &status).unwrap(),
            None
        );
        {
            let status = status.read().unwrap();
            assert_eq!(status.state, "pending");
            assert_eq!(status.not_after_unix_ms, None);
            assert_eq!(status.restore_error, None);
        }
        let guard = match IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap() {
            IssuanceClaim::Acquired(guard) => *guard,
            IssuanceClaim::CertificateCurrent => panic!("the legacy pair is incomplete"),
        };
        crate::access::authority_store::with_lock(dir.path(), || {
            write_certificate_pair_locked(
                dir.path(),
                &domain,
                &certificate.cert.pem(),
                &certificate.signing_key.serialize_pem(),
            )
        })
        .unwrap();
        guard.finish().unwrap();
        assert!(
            refresh_shared_certificate_status(&domain, dir.path(), &status)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn stale_sibling_refreshes_the_shared_pair_before_renewal() {
        let dir = tempfile::tempdir().unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let first = Arc::new(RwLock::new(CertificateStatus::default()));
        let sibling = Arc::new(RwLock::new(CertificateStatus::default()));
        restore(&domain, dir.path(), &first).unwrap();
        restore(&domain, dir.path(), &sibling).unwrap();
        assert!(sibling.read().unwrap().not_after_unix_ms.is_none());

        let certificate = rcgen::generate_simple_self_signed(vec![domain.name.clone()]).unwrap();
        crate::access::authority_store::with_lock(dir.path(), || {
            write_certificate_pair_locked(
                dir.path(),
                &domain,
                &certificate.cert.pem(),
                &certificate.signing_key.serialize_pem(),
            )
        })
        .unwrap();

        let expiry = refresh_shared_certificate_status(&domain, dir.path(), &sibling).unwrap();
        assert!(
            expiry.is_some_and(|expiry| { expiry > now_unix_ms().saturating_add(RENEW_BEFORE_MS) })
        );
        assert_eq!(sibling.read().unwrap().state, "valid");
    }

    #[test]
    fn durable_issuance_lease_serializes_sibling_order_creation() {
        let dir = tempfile::tempdir().unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let first = match IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap() {
            IssuanceClaim::Acquired(guard) => *guard,
            IssuanceClaim::CertificateCurrent => panic!("no certificate exists yet"),
        };
        let sibling_error = match IssuanceLeaseGuard::begin(&domain, dir.path()) {
            Ok(_) => panic!("a sibling must not create a concurrent order"),
            Err(error) => error,
        };
        assert!(sibling_error.contains("another daemon process"));
        first.finish().unwrap();

        let certificate = rcgen::generate_simple_self_signed(vec![domain.name.clone()]).unwrap();
        crate::access::authority_store::with_lock(dir.path(), || {
            write_certificate_pair_locked(
                dir.path(),
                &domain,
                &certificate.cert.pem(),
                &certificate.signing_key.serialize_pem(),
            )
        })
        .unwrap();
        assert!(matches!(
            IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap(),
            IssuanceClaim::CertificateCurrent
        ));
    }

    #[test]
    fn ambiguous_custom_order_resumes_with_the_same_key_and_csr() {
        let dir = tempfile::tempdir().unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let mut first = match IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap() {
            IssuanceClaim::Acquired(guard) => *guard,
            IssuanceClaim::CertificateCurrent => panic!("no certificate exists yet"),
        };
        first
            .record_order_url("https://acme.example.test/order/one")
            .unwrap();
        let (first_key, first_csr) = first.finalization_material(&domain.name).unwrap();
        drop(first);

        let mut resumed = match IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap() {
            IssuanceClaim::Acquired(guard) => *guard,
            IssuanceClaim::CertificateCurrent => panic!("no certificate exists yet"),
        };
        assert_eq!(
            resumed.order_url(),
            Some("https://acme.example.test/order/one")
        );
        let (resumed_key, resumed_csr) = resumed.finalization_material(&domain.name).unwrap();
        assert_eq!(resumed_key, first_key);
        assert_eq!(resumed_csr, first_csr);
        resumed.finish().unwrap();
    }

    #[test]
    fn stale_custom_order_allows_a_fresh_request() {
        let dir = tempfile::tempdir().unwrap();
        let domain = ValidatedCustomDomain {
            name: "box.example.test".to_string(),
            rp_id: "box.example.test".to_string(),
            origin: "https://box.example.test".to_string(),
        };
        let mut first = match IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap() {
            IssuanceClaim::Acquired(guard) => *guard,
            IssuanceClaim::CertificateCurrent => panic!("no certificate exists yet"),
        };
        first
            .record_order_url("https://acme.example.test/order/stale")
            .unwrap();
        drop(first);

        crate::access::authority_store::with_lock(dir.path(), || {
            let mut record = load_issuance_lease_locked(dir.path())
                .map_err(crate::access::AccessError)?
                .unwrap();
            let stale = now_unix_ms().saturating_sub(ISSUANCE_RESUMABLE_TTL_MS + 1);
            record.started_unix_ms = stale;
            // Reclaims update liveness but must not renew the order itself.
            record.updated_unix_ms = now_unix_ms();
            record.owner_token = None;
            record.owner_lease_expires_unix_ms = 0;
            write_issuance_lease_locked(dir.path(), &record)
        })
        .unwrap();

        let replacement = match IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap() {
            IssuanceClaim::Acquired(guard) => *guard,
            IssuanceClaim::CertificateCurrent => panic!("no certificate exists yet"),
        };
        assert_eq!(replacement.order_url(), None);
        replacement.finish().unwrap();
    }

    #[test]
    fn successful_account_retry_clears_only_transient_errors() {
        let mut transient = CertificateStatus {
            state: "error".to_string(),
            last_error: Some("temporary account lookup failure".to_string()),
            ..Default::default()
        };
        record_account_ready(
            &mut transient,
            "https://acme.example/account/1".to_string(),
            false,
        );
        assert_eq!(transient.state, "waiting_for_caa");
        assert_eq!(transient.last_error, None);

        let mut durable = CertificateStatus {
            state: "error".to_string(),
            restore_error: Some("stored certificate is unreadable".to_string()),
            last_error: Some("temporary account lookup failure".to_string()),
            ..Default::default()
        };
        record_account_ready(
            &mut durable,
            "https://acme.example/account/1".to_string(),
            false,
        );
        assert_eq!(durable.state, "error");
        assert_eq!(
            durable.restore_error.as_deref(),
            Some("stored certificate is unreadable")
        );
        assert_eq!(durable.last_error, None);

        let mut valid = CertificateStatus {
            state: "valid".to_string(),
            not_after_unix_ms: Some(u64::MAX),
            last_error: Some("temporary account lookup failure".to_string()),
            ..Default::default()
        };
        record_account_ready(
            &mut valid,
            "https://acme.example/account/1".to_string(),
            false,
        );
        assert_eq!(valid.state, "valid");
        assert_eq!(valid.last_error, None);
    }
}
