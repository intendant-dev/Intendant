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
    let cert_path = cert_dir.join(CERT_FILE);
    let key_path = cert_dir.join(KEY_FILE);
    let (cert_pem, key_pem) = match (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        (Ok(cert), Ok(key)) => (cert, key),
        (Err(cert_error), Err(key_error))
            if cert_error.kind() == std::io::ErrorKind::NotFound
                && key_error.kind() == std::io::ErrorKind::NotFound =>
        {
            return account_error.map_or(Ok(()), Err);
        }
        (Err(error), _) => return Err(format!("read {}: {error}", cert_path.display())),
        (_, Err(error)) => return Err(format!("read {}: {error}", key_path.display())),
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
    let account = crate::fleet_cert::acme_account_in(cert_dir).await?;
    let account_uri = crate::fleet_cert::acme_account_uri_in(cert_dir)?
        .ok_or_else(|| "ACME account was created without an account URI".to_string())?;
    update_status(status, |current| {
        current.acme_account_uri = Some(account_uri);
    });
    if !issuance_enabled {
        update_status(status, |current| {
            if current.not_after_unix_ms.is_none() {
                current.state = "waiting_for_caa".to_string();
            }
            current.last_error = None;
        });
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

    let mut challenge_handle = None;
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
            let handle = super::dns::set_challenge(dns_config, &domain.name, &value).await?;
            challenge_handle = Some(handle);
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

    if let Some(handle) = challenge_handle {
        if let Err(error) = super::dns::clear_challenge(handle).await {
            eprintln!("[custom-domain] DNS-01 cleanup (best-effort): {error}");
        }
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
        current.last_error = None;
    });
    Ok(())
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
}
