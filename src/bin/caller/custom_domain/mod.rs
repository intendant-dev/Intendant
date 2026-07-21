mod cert;
mod dns;
mod passkeys;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use crate::access::hosted_control::HostedControlRuntime;
use crate::project::{CustomDomainConfig, CustomDomainDnsConfig, ValidatedCustomDomain};

pub(crate) use passkeys::{
    AuthenticationFinishInput, AuthenticationStartInput, CeremonyStart, EnrollmentInvite,
    PasskeyLeaseResult, PasskeyView, RegistrationFinishInput, RegistrationInviteInput,
    RegistrationStartInput, RevokeInput,
};

pub(crate) struct RelayCertificateMaterial {
    pub(crate) server_name: String,
    pub(crate) certificate_chain_pem: String,
    pub(crate) private_key_pem: String,
}

/// Load durable DNS-cleanup authority at the process boundary before any
/// supervised coding child can inherit the controller environment. Gateway
/// construction repeats this against its explicit store for testability.
pub(crate) fn configure_pending_credential_child_scrub() {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    refresh_pending_credential_child_scrub_in(&cert_dir);
}

/// Refresh the child scrub from the shared durable journal immediately before
/// a supervised process spawn. Another daemon process can create the journal
/// after this process starts, so startup-only cache population is insufficient.
pub(crate) fn refresh_pending_credential_child_scrub_in(cert_dir: &Path) {
    if let Err(error) = dns::refresh_pending_credential_child_scrub(cert_dir) {
        eprintln!("[custom-domain] load pending DNS credential child-scrub state: {error}");
    }
}

pub(crate) fn with_pending_credential_child_scrub_in<T>(
    cert_dir: &Path,
    operation: impl FnOnce() -> T,
) -> T {
    dns::with_pending_credential_child_scrub(cert_dir, operation)
}

fn domain_control_error_in(
    cert_dir: &Path,
    domain: &ValidatedCustomDomain,
    current_fleet_zone_observed: Option<&AtomicBool>,
) -> Option<String> {
    if current_fleet_zone_observed.is_some_and(|observed| !observed.load(Ordering::SeqCst)) {
        return Some(
            "custom-domain control is waiting for the current Connect fleet-zone observation"
                .to_string(),
        );
    }
    match crate::fleet_cert::is_service_controlled_name_live_in(cert_dir, &domain.name) {
        Ok(false) => None,
        Ok(true) => {
            Some("custom-domain name overlaps a service-controlled fleet name or zone".to_string())
        }
        Err(error) => Some(format!("check custom-domain name provenance: {error}")),
    }
}

pub(crate) fn relay_certificate_material(
    config: &CustomDomainConfig,
) -> Result<Option<RelayCertificateMaterial>, String> {
    let Some(domain) = config.validated()? else {
        return Ok(None);
    };
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    if crate::fleet_cert::is_service_controlled_name_in(&cert_dir, &domain.name)? {
        return Err(
            "custom-domain name overlaps a service-controlled fleet name or zone".to_string(),
        );
    }
    let (certificate_chain_pem, private_key_pem) =
        cert::relay_certificate_material(&domain, &cert_dir)?;
    Ok(Some(RelayCertificateMaterial {
        server_name: domain.name,
        certificate_chain_pem,
        private_key_pem,
    }))
}

#[derive(Clone, Debug, Default)]
pub(super) struct CertificateStatus {
    pub(super) state: String,
    pub(super) not_after_unix_ms: Option<u64>,
    pub(super) acme_account_uri: Option<String>,
    /// A stored certificate/key could not be restored. Unlike a retryable
    /// ACME error, this remains visible until a fresh certificate replaces it.
    pub(super) restore_error: Option<String>,
    /// Retryable account, DNS, and issuance error from the latest check.
    pub(super) last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CustomDomainSnapshot {
    pub configured: bool,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rp_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_provider: Option<String>,
    pub acme_issuance_enabled: bool,
    pub certificate_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_not_after_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_account_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_error: Option<String>,
    pub passkeys: Vec<PasskeyView>,
}

pub(crate) struct CustomDomainRuntime {
    configured: bool,
    domain: Option<ValidatedCustomDomain>,
    dns: Option<CustomDomainDnsConfig>,
    acme_issuance_enabled: bool,
    cert_dir: PathBuf,
    certificate: Arc<RwLock<CertificateStatus>>,
    passkeys: Option<passkeys::PasskeyRuntime>,
    initialization_error: Option<String>,
    current_fleet_zone_observed: Option<Arc<AtomicBool>>,
}

impl std::fmt::Debug for CustomDomainRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CustomDomainRuntime")
            .field("configured", &self.configured)
            .field("domain", &self.domain)
            .field("cert_dir", &self.cert_dir)
            .field("initialization_error", &self.initialization_error)
            .finish_non_exhaustive()
    }
}

impl CustomDomainRuntime {
    pub(crate) fn new(
        config: &CustomDomainConfig,
        cert_dir: PathBuf,
        hosted: Arc<HostedControlRuntime>,
        current_fleet_zone_observed: Option<Arc<AtomicBool>>,
    ) -> Self {
        if let Err(error) = dns::refresh_pending_credential_child_scrub(&cert_dir) {
            eprintln!("[custom-domain] load pending DNS credential child-scrub state: {error}");
        }
        if !config.enabled {
            return Self {
                configured: false,
                domain: None,
                dns: None,
                acme_issuance_enabled: false,
                cert_dir,
                certificate: Arc::new(RwLock::new(CertificateStatus::default())),
                passkeys: None,
                initialization_error: None,
                current_fleet_zone_observed,
            };
        }

        let domain = match config.validated() {
            Ok(Some(domain)) => domain,
            Ok(None) => unreachable!("enabled custom-domain config validates to Some"),
            Err(error) => {
                return Self::invalid(cert_dir, error, current_fleet_zone_observed);
            }
        };
        match crate::fleet_cert::is_service_controlled_name_in(&cert_dir, &domain.name) {
            Ok(true) => {
                return Self::invalid(
                    cert_dir,
                    "custom-domain name overlaps a service-controlled fleet name or zone"
                        .to_string(),
                    current_fleet_zone_observed,
                );
            }
            Err(error) => {
                return Self::invalid(
                    cert_dir,
                    format!("check custom-domain name provenance: {error}"),
                    current_fleet_zone_observed,
                );
            }
            Ok(false) => {}
        }

        crate::web_tls::register_custom_domain_server_name(&domain.name);
        let certificate = Arc::new(RwLock::new(CertificateStatus {
            state: "pending".to_string(),
            ..Default::default()
        }));
        let mut initialization_errors = Vec::new();
        if let Err(error) = cert::restore(&domain, &cert_dir, &certificate) {
            let mut current = certificate
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            current.state = "error".to_string();
            current.restore_error = Some(error);
        }
        let passkeys = match passkeys::PasskeyRuntime::new(domain.clone(), cert_dir.clone(), hosted)
        {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                initialization_errors.push(format!("load custom-domain passkeys: {error}"));
                None
            }
        };
        Self {
            configured: true,
            domain: Some(domain),
            dns: config.dns.clone(),
            acme_issuance_enabled: config.acme_issuance_enabled,
            cert_dir,
            certificate,
            passkeys,
            initialization_error: (!initialization_errors.is_empty())
                .then(|| initialization_errors.join("; ")),
            current_fleet_zone_observed,
        }
    }

    fn invalid(
        cert_dir: PathBuf,
        error: String,
        current_fleet_zone_observed: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            configured: true,
            domain: None,
            dns: None,
            acme_issuance_enabled: false,
            cert_dir,
            certificate: Arc::new(RwLock::new(CertificateStatus {
                state: "error".to_string(),
                last_error: Some(error.clone()),
                ..Default::default()
            })),
            passkeys: None,
            initialization_error: Some(error),
            current_fleet_zone_observed,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.domain.is_some() && self.domain_control_error().is_none()
    }

    pub(crate) fn configured(&self) -> bool {
        self.configured
    }

    pub(crate) fn origin(&self) -> Option<&str> {
        self.domain.as_ref().map(|domain| domain.origin.as_str())
    }

    pub(crate) fn matches_origin(&self, origin: &str) -> bool {
        self.enabled() && self.origin() == Some(origin)
    }

    pub(crate) fn spawn_certificate_loop(&self) {
        // Cleanup survives disabling or invalidating the custom lane: a DNS
        // mutation journal from an earlier process still has to be retired.
        dns::spawn_cleanup_loop(self.cert_dir.clone());
        let Some(domain) = self.domain.clone() else {
            return;
        };
        cert::spawn(
            domain,
            self.dns.clone(),
            self.acme_issuance_enabled,
            self.cert_dir.clone(),
            Arc::clone(&self.certificate),
            self.current_fleet_zone_observed.clone(),
        );
    }

    pub(crate) fn snapshot(&self) -> CustomDomainSnapshot {
        let certificate = self
            .certificate
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let domain_control_error = self.domain_control_error();
        let passkeys = if domain_control_error.is_none() {
            self.passkeys
                .as_ref()
                .and_then(|runtime| runtime.views().ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        CustomDomainSnapshot {
            configured: self.configured,
            enabled: self.domain.is_some() && domain_control_error.is_none(),
            name: self.domain.as_ref().map(|domain| domain.name.clone()),
            rp_id: self.domain.as_ref().map(|domain| domain.rp_id.clone()),
            origin: self.domain.as_ref().map(|domain| domain.origin.clone()),
            dns_provider: self
                .dns
                .as_ref()
                .map(dns::provider_name)
                .map(str::to_string),
            acme_issuance_enabled: self.acme_issuance_enabled,
            certificate_state: if domain_control_error.is_some() {
                "error".to_string()
            } else if certificate.state.is_empty() {
                "disabled".to_string()
            } else {
                certificate.state
            },
            certificate_not_after_unix_ms: certificate.not_after_unix_ms,
            acme_account_uri: certificate.acme_account_uri,
            initialization_error: self
                .initialization_error
                .clone()
                .or(domain_control_error)
                .or(certificate.restore_error)
                .or(certificate.last_error),
            passkeys,
        }
    }

    pub(crate) fn registration_invite(
        &self,
        input: RegistrationInviteInput,
    ) -> Result<EnrollmentInvite, String> {
        self.passkeys()?
            .registration_invite(input, self.current_fleet_zone_observed.as_deref())
    }

    pub(crate) fn registration_start(
        &self,
        input: RegistrationStartInput,
        origin: &str,
    ) -> Result<CeremonyStart, String> {
        self.passkeys()?.registration_start(
            input,
            origin,
            self.current_fleet_zone_observed.as_deref(),
        )
    }

    pub(crate) fn registration_finish(
        &self,
        input: RegistrationFinishInput,
    ) -> Result<PasskeyView, String> {
        self.passkeys()?
            .registration_finish(input, self.current_fleet_zone_observed.as_deref())
    }

    pub(crate) fn authentication_start(
        &self,
        input: AuthenticationStartInput,
        origin: &str,
        source_bucket: Option<&str>,
    ) -> Result<CeremonyStart, String> {
        self.passkeys()?.authentication_start(
            input,
            origin,
            source_bucket,
            self.current_fleet_zone_observed.as_deref(),
        )
    }

    pub(crate) fn authentication_finish(
        &self,
        input: AuthenticationFinishInput,
        origin: &str,
    ) -> Result<PasskeyLeaseResult, String> {
        self.passkeys()?.authentication_finish(
            input,
            origin,
            self.current_fleet_zone_observed.as_deref(),
        )
    }

    pub(crate) fn revoke(&self, input: RevokeInput) -> Result<bool, String> {
        self.passkeys()?.revoke(input)
    }

    fn passkeys(&self) -> Result<&passkeys::PasskeyRuntime, String> {
        if let Some(error) = self.domain_control_error() {
            return Err(error);
        }
        self.passkeys.as_ref().ok_or_else(|| {
            self.initialization_error
                .clone()
                .unwrap_or_else(|| "custom-domain passkeys are not configured".to_string())
        })
    }

    fn domain_control_error(&self) -> Option<String> {
        let domain = self.domain.as_ref()?;
        domain_control_error_in(
            &self.cert_dir,
            domain,
            self.current_fleet_zone_observed.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_runtime_does_not_touch_custom_domain_state() {
        let dir = tempfile::tempdir().unwrap();
        let hosted = Arc::new(HostedControlRuntime::new(
            false,
            dir.path().to_path_buf(),
            None,
            None,
            String::new(),
            false,
        ));
        let runtime = CustomDomainRuntime::new(
            &CustomDomainConfig::default(),
            dir.path().into(),
            hosted,
            None,
        );
        assert!(!runtime.configured());
        assert!(!runtime.enabled());
        assert_eq!(runtime.snapshot().certificate_state, "disabled");
    }

    #[test]
    fn runtime_disables_itself_when_a_later_fleet_zone_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        let hosted = Arc::new(HostedControlRuntime::new(
            false,
            dir.path().to_path_buf(),
            None,
            None,
            String::new(),
            false,
        ));
        let config = CustomDomainConfig {
            enabled: true,
            name: Some("box.fleet.example.test".to_string()),
            ..Default::default()
        };
        let runtime = CustomDomainRuntime::new(&config, dir.path().into(), hosted, None);
        assert!(runtime.enabled());
        assert!(runtime.matches_origin("https://box.fleet.example.test"));

        crate::fleet_cert::remember_fleet_origin_for_test(
            dir.path(),
            Some("fleet.example.test"),
            "d-1234567890abcdef1234.fleet.example.test",
        )
        .unwrap();
        assert!(!runtime.enabled());
        assert!(!runtime.matches_origin("https://box.fleet.example.test"));
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.certificate_state, "error");
        assert!(snapshot
            .initialization_error
            .as_deref()
            .is_some_and(|error| error.contains("service-controlled fleet name or zone")));
    }

    #[test]
    fn runtime_live_guard_rejects_authority_store_contention() {
        let dir = tempfile::tempdir().unwrap();
        crate::fleet_cert::remember_fleet_origin_for_test(
            dir.path(),
            Some("fleet.example.test"),
            "d-1234567890abcdef1234.fleet.example.test",
        )
        .unwrap();
        let hosted = Arc::new(HostedControlRuntime::new(
            false,
            dir.path().to_path_buf(),
            None,
            None,
            String::new(),
            false,
        ));
        let config = CustomDomainConfig {
            enabled: true,
            name: Some("owner.example.test".to_string()),
            ..Default::default()
        };
        let runtime = CustomDomainRuntime::new(&config, dir.path().into(), hosted, None);
        assert!(runtime.enabled());

        let worker_dir = dir.path().to_path_buf();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            crate::access::authority_store::with_lock(&worker_dir, || {
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        locked_rx.recv().unwrap();

        let enabled_during_contention = runtime.enabled();
        let snapshot_during_contention = runtime.snapshot();

        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(!enabled_during_contention);
        assert!(snapshot_during_contention
            .initialization_error
            .as_deref()
            .is_some_and(|error| error.contains("authority-store lock") && error.contains("busy")));
        assert!(runtime.enabled());
    }

    #[test]
    fn enabled_connect_holds_an_existing_custom_lane_until_current_zone_is_observed() {
        let dir = tempfile::tempdir().unwrap();
        let config = CustomDomainConfig {
            enabled: true,
            name: Some("box.fleet.example.test".to_string()),
            ..Default::default()
        };
        let certificate =
            rcgen::generate_simple_self_signed(vec!["box.fleet.example.test".to_string()]).unwrap();
        let seeded_domain = ValidatedCustomDomain {
            name: "box.fleet.example.test".to_string(),
            rp_id: "box.fleet.example.test".to_string(),
            origin: "https://box.fleet.example.test".to_string(),
        };
        crate::access::authority_store::with_lock(dir.path(), || {
            cert::write_certificate_pair_locked(
                dir.path(),
                &seeded_domain,
                &certificate.cert.pem(),
                &certificate.signing_key.serialize_pem(),
            )
        })
        .unwrap();
        let hosted = || {
            Arc::new(HostedControlRuntime::new(
                false,
                dir.path().to_path_buf(),
                None,
                None,
                String::new(),
                false,
            ))
        };
        // Seed the durable passkey store as an existing installation.
        let seeded = CustomDomainRuntime::new(&config, dir.path().into(), hosted(), None);
        assert!(seeded.enabled());
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| {
                    name.starts_with("custom-domain-passkeys-") && name.ends_with(".json")
                })
        }));
        drop(seeded);

        let observed = Arc::new(AtomicBool::new(false));
        let runtime = CustomDomainRuntime::new(
            &config,
            dir.path().into(),
            hosted(),
            Some(Arc::clone(&observed)),
        );
        assert!(!runtime.enabled());
        assert!(!runtime.matches_origin("https://box.fleet.example.test"));
        assert!(runtime
            .snapshot()
            .initialization_error
            .as_deref()
            .is_some_and(|error| error.contains("waiting for the current Connect")));

        crate::fleet_cert::remember_fleet_origin_for_test(
            dir.path(),
            Some("fleet.example.test"),
            "d-1234567890abcdef1234.fleet.example.test",
        )
        .unwrap();
        observed.store(true, Ordering::SeqCst);
        assert!(
            !runtime.enabled(),
            "the delayed overlapping zone response keeps the lane closed"
        );
        assert!(runtime
            .snapshot()
            .initialization_error
            .as_deref()
            .is_some_and(|error| error.contains("service-controlled fleet name or zone")));
    }
}
