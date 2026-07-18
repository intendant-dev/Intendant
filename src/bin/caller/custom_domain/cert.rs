use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::project::{CustomDomainDnsConfig, ValidatedCustomDomain};
use serde::{Deserialize, Serialize};

use super::CertificateStatus;

const CERT_FILE: &str = "custom-domain-cert.pem";
const KEY_FILE: &str = "custom-domain-key.pem";
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const RENEW_BEFORE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const DNS_CHALLENGE_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30 * 60);
const ISSUANCE_LEASE_FILE: &str = "custom-domain-cert-issuance.json";
const ISSUANCE_LEASE_SCHEMA_VERSION: u32 = 1;
const ISSUANCE_LEASE_MAX_BYTES: u64 = 4096;
const ISSUANCE_LEASE_MS: u64 = 45 * 60 * 1000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuanceLeaseRecord {
    schema_version: u32,
    name: String,
    owner_token: String,
    expires_unix_ms: u64,
}

enum IssuanceClaim {
    CertificateCurrent,
    Acquired(IssuanceLeaseGuard),
}

struct IssuanceLeaseGuard {
    cert_dir: PathBuf,
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
            update_status(status, |current| current.last_error = account_error);
            return Ok(());
        }
    };
    require_exact_dns_name(&cert_pem, &domain.name)?;
    let not_after = crate::fleet_cert::cert_not_after_unix_ms(&cert_pem)
        .ok_or_else(|| "custom-domain certificate has no usable expiry".to_string())?;
    if not_after <= now_unix_ms() {
        update_status(status, |current| {
            current.state = "expired".to_string();
            current.not_after_unix_ms = Some(not_after);
            current.last_error = account_error;
        });
        return Ok(());
    }
    crate::web_tls::install_custom_domain_certificate(&domain.name, &cert_pem, &key_pem)?;
    update_status(status, |current| {
        current.state = "valid".to_string();
        current.not_after_unix_ms = Some(not_after);
        current.last_error = account_error;
    });
    Ok(())
}

fn read_stored_certificate_pair(cert_dir: &Path) -> Result<Option<(String, String)>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
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
    let record: IssuanceLeaseRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if record.schema_version != ISSUANCE_LEASE_SCHEMA_VERSION
        || record.name.is_empty()
        || record.name.len() > 253
        || record.owner_token.len() != 32
        || !record
            .owner_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!(
            "{} contains an invalid custom-domain issuance lease",
            path.display()
        ));
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
    require_exact_dns_name(&cert_pem, &domain.name)?;
    crate::web_tls::validate_custom_domain_certificate_key_pair(&cert_pem, &key_pem)?;
    let not_after = crate::fleet_cert::cert_not_after_unix_ms(&cert_pem)
        .ok_or_else(|| "custom-domain certificate has no usable expiry".to_string())?;
    Ok(not_after > now_unix_ms().saturating_add(RENEW_BEFORE_MS))
}

impl IssuanceLeaseGuard {
    fn begin(domain: &ValidatedCustomDomain, cert_dir: &Path) -> Result<IssuanceClaim, String> {
        crate::access::authority_store::with_lock(cert_dir, || {
            if stored_certificate_is_current_locked(domain, cert_dir)
                .map_err(crate::access::AccessError)?
            {
                return Ok(IssuanceClaim::CertificateCurrent);
            }
            let now = now_unix_ms();
            if load_issuance_lease_locked(cert_dir)
                .map_err(crate::access::AccessError)?
                .is_some_and(|record| record.expires_unix_ms > now)
            {
                return Err(crate::access::AccessError(
                    "a custom-domain certificate request is already running in another daemon process"
                        .to_string(),
                ));
            }
            let owner_token = uuid::Uuid::new_v4().simple().to_string();
            write_issuance_lease_locked(
                cert_dir,
                &IssuanceLeaseRecord {
                    schema_version: ISSUANCE_LEASE_SCHEMA_VERSION,
                    name: domain.name.clone(),
                    owner_token: owner_token.clone(),
                    expires_unix_ms: now.saturating_add(ISSUANCE_LEASE_MS),
                },
            )?;
            Ok(IssuanceClaim::Acquired(Self {
                cert_dir: cert_dir.to_path_buf(),
                owner_token,
                active: true,
            }))
        })
        .map_err(|error| error.to_string())
    }

    fn renew(&self) -> Result<(), String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let mut record = load_issuance_lease_locked(&self.cert_dir)
                .map_err(crate::access::AccessError)?
                .ok_or_else(|| {
                    crate::access::AccessError(
                        "custom-domain certificate issuance lease disappeared".to_string(),
                    )
                })?;
            if record.owner_token != self.owner_token {
                return Err(crate::access::AccessError(
                    "custom-domain certificate issuance ownership changed".to_string(),
                ));
            }
            record.expires_unix_ms = now_unix_ms().saturating_add(ISSUANCE_LEASE_MS);
            write_issuance_lease_locked(&self.cert_dir, &record)
        })
        .map_err(|error| error.to_string())
    }

    fn finish(mut self) -> Result<(), String> {
        let result = self.release();
        if result.is_ok() {
            self.active = false;
        }
        result
    }

    fn release(&self) -> Result<(), String> {
        crate::access::authority_store::with_lock(&self.cert_dir, || {
            let Some(record) =
                load_issuance_lease_locked(&self.cert_dir).map_err(crate::access::AccessError)?
            else {
                return Ok(());
            };
            if record.owner_token != self.owner_token {
                return Ok(());
            }
            crate::access::authority_store::remove_file_locked(&issuance_lease_path(&self.cert_dir))
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
            }
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
    restore(domain, cert_dir, status)?;
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
    let issuance = match IssuanceLeaseGuard::begin(domain, cert_dir)? {
        IssuanceClaim::CertificateCurrent => {
            refresh_shared_certificate_status(domain, cert_dir, status)?;
            return Ok(());
        }
        IssuanceClaim::Acquired(guard) => guard,
    };
    let identifiers = [instant_acme::Identifier::Dns(domain.name.clone())];
    let mut order = account
        .new_order(&instant_acme::NewOrder::new(&identifiers))
        .await
        .map_err(|error| format!("custom-domain ACME new order: {error}"))?;
    issuance.renew()?;

    let mut dns_challenge_lease = None;
    let authorization_result: Result<(), String> = async {
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authorization =
                result.map_err(|error| format!("custom-domain ACME authorization: {error}"))?;
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
                .ok_or_else(|| "custom-domain ACME order offers no dns-01 challenge".to_string())?;
            let value = challenge.key_authorization().dns_value();
            let lease =
                super::dns::set_challenge_in(dns_config, &domain.name, &value, cert_dir).await?;
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
        issuance.renew()?;
        let order_status = order
            .poll_ready(&instant_acme::RetryPolicy::default())
            .await
            .map_err(|error| format!("custom-domain ACME validation: {error}"))?;
        if order_status != instant_acme::OrderStatus::Ready {
            return Err(format!(
                "custom-domain ACME order did not become ready: {order_status:?}"
            ));
        }
        Ok(())
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
            Ok(()) => format!("custom-domain DNS-01 cleanup remains pending: {cleanup_error}"),
            Err(authorization_error) => format!(
                "{authorization_error}; custom-domain DNS-01 cleanup remains pending: {cleanup_error}"
            ),
        });
    }
    authorization_result?;

    issuance.renew()?;
    let private_key_pem = order
        .finalize()
        .await
        .map_err(|error| format!("custom-domain ACME finalize: {error}"))?;
    let cert_chain_pem = order
        .poll_certificate(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|error| format!("custom-domain ACME certificate: {error}"))?;
    issuance.renew()?;
    require_exact_dns_name(&cert_chain_pem, &domain.name)?;
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
        crate::access::authority_store::atomic_write_private_locked(
            &cert_dir.join(KEY_FILE),
            private_key_pem.as_bytes(),
        )?;
        crate::access::authority_store::atomic_write_private_locked(
            &cert_dir.join(CERT_FILE),
            cert_chain_pem.as_bytes(),
        )
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

        let (key_written_tx, key_written_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let writer_dir = dir.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            crate::access::authority_store::with_lock(&writer_dir, || {
                crate::access::authority_store::atomic_write_private_locked(
                    &writer_dir.join(KEY_FILE),
                    b"new key",
                )?;
                key_written_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                crate::access::authority_store::atomic_write_private_locked(
                    &writer_dir.join(CERT_FILE),
                    b"new certificate",
                )
            })
        });
        key_written_rx.recv().unwrap();

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
            crate::access::authority_store::atomic_write_private_locked(
                &dir.path().join(KEY_FILE),
                certificate.signing_key.serialize_pem().as_bytes(),
            )?;
            crate::access::authority_store::atomic_write_private_locked(
                &dir.path().join(CERT_FILE),
                certificate.cert.pem().as_bytes(),
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
            IssuanceClaim::Acquired(guard) => guard,
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
            crate::access::authority_store::atomic_write_private_locked(
                &dir.path().join(KEY_FILE),
                certificate.signing_key.serialize_pem().as_bytes(),
            )?;
            crate::access::authority_store::atomic_write_private_locked(
                &dir.path().join(CERT_FILE),
                certificate.cert.pem().as_bytes(),
            )
        })
        .unwrap();
        assert!(matches!(
            IssuanceLeaseGuard::begin(&domain, dir.path()).unwrap(),
            IssuanceClaim::CertificateCurrent
        ));
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
