use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::project::{CustomDomainDnsConfig, ValidatedCustomDomain};

use super::CertificateStatus;

const CERT_FILE: &str = "custom-domain-cert.pem";
const KEY_FILE: &str = "custom-domain-key.pem";
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const RENEW_BEFORE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

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
    Ok((cert_pem, key_pem))
}

pub(super) fn spawn(
    domain: ValidatedCustomDomain,
    dns: Option<CustomDomainDnsConfig>,
    issuance_enabled: bool,
    cert_dir: PathBuf,
    status: Arc<RwLock<CertificateStatus>>,
) {
    tokio::spawn(async move {
        loop {
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
    super::dns::retry_pending_challenge(cert_dir).await?;
    if crate::fleet_cert::is_service_controlled_name_in(cert_dir, &domain.name)? {
        return Err(
            "custom-domain name overlaps a service-controlled fleet name or zone".to_string(),
        );
    }
    let account = crate::fleet_cert::acme_account_in(cert_dir).await?;
    let account_uri = crate::fleet_cert::acme_account_uri_in(cert_dir)?
        .ok_or_else(|| "ACME account was created without an account URI".to_string())?;
    update_status(status, |current| {
        record_account_ready(current, account_uri, issuance_enabled);
    });
    if !issuance_enabled {
        return Ok(());
    }

    let current_not_after = status
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .not_after_unix_ms;
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

async fn request_certificate(
    domain: &ValidatedCustomDomain,
    dns_config: &CustomDomainDnsConfig,
    cert_dir: &Path,
    status: &Arc<RwLock<CertificateStatus>>,
    account: instant_acme::Account,
) -> Result<(), String> {
    let identifiers = [instant_acme::Identifier::Dns(domain.name.clone())];
    let mut order = account
        .new_order(&instant_acme::NewOrder::new(&identifiers))
        .await
        .map_err(|error| format!("custom-domain ACME new order: {error}"))?;

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
            super::dns::set_challenge_in(dns_config, &domain.name, &value, cert_dir).await?;
            let delay = super::dns::propagation_delay_secs(dns_config);
            if delay > 0 {
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            challenge
                .set_ready()
                .await
                .map_err(|error| format!("custom-domain ACME challenge ready: {error}"))?;
        }
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

    if let Err(cleanup_error) = super::dns::retry_pending_challenge(cert_dir).await {
        return Err(match authorization_result {
            Ok(()) => format!("custom-domain DNS-01 cleanup remains pending: {cleanup_error}"),
            Err(authorization_error) => format!(
                "{authorization_error}; custom-domain DNS-01 cleanup remains pending: {cleanup_error}"
            ),
        });
    }
    authorization_result?;

    let private_key_pem = order
        .finalize()
        .await
        .map_err(|error| format!("custom-domain ACME finalize: {error}"))?;
    let cert_chain_pem = order
        .poll_certificate(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|error| format!("custom-domain ACME certificate: {error}"))?;
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
