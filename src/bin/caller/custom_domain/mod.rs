mod cert;
mod dns;
mod passkeys;

use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::access::hosted_control::HostedControlRuntime;
use crate::project::{CustomDomainConfig, CustomDomainDnsConfig, ValidatedCustomDomain};

pub(crate) use passkeys::{
    AuthenticationFinishInput, AuthenticationStartInput, CeremonyStart, EnrollmentInvite,
    PasskeyLeaseResult, PasskeyView, RegistrationFinishInput, RegistrationInviteInput,
    RegistrationStartInput, RevokeInput,
};

#[derive(Clone, Debug, Default)]
pub(super) struct CertificateStatus {
    pub(super) state: String,
    pub(super) not_after_unix_ms: Option<u64>,
    pub(super) acme_account_uri: Option<String>,
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
    ) -> Self {
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
            };
        }

        let domain = match config.validated() {
            Ok(Some(domain)) => domain,
            Ok(None) => unreachable!("enabled custom-domain config validates to Some"),
            Err(error) => {
                return Self::invalid(cert_dir, error);
            }
        };
        match crate::fleet_cert::is_known_fleet_name_in(&cert_dir, &domain.name) {
            Ok(true) => {
                return Self::invalid(
                    cert_dir,
                    "custom-domain name is already recorded as a service-controlled fleet name"
                        .to_string(),
                );
            }
            Err(error) => {
                return Self::invalid(
                    cert_dir,
                    format!("check custom-domain name provenance: {error}"),
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
            current.last_error = Some(error);
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
        }
    }

    fn invalid(cert_dir: PathBuf, error: String) -> Self {
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
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.domain.is_some()
    }

    pub(crate) fn configured(&self) -> bool {
        self.configured
    }

    pub(crate) fn origin(&self) -> Option<&str> {
        self.domain.as_ref().map(|domain| domain.origin.as_str())
    }

    pub(crate) fn matches_origin(&self, origin: &str) -> bool {
        self.origin() == Some(origin)
    }

    pub(crate) fn passkey_available(&self) -> bool {
        self.passkeys
            .as_ref()
            .and_then(|runtime| runtime.views().ok())
            .is_some_and(|passkeys| !passkeys.is_empty())
    }

    pub(crate) fn spawn_certificate_loop(&self) {
        let Some(domain) = self.domain.clone() else {
            return;
        };
        cert::spawn(
            domain,
            self.dns.clone(),
            self.acme_issuance_enabled,
            self.cert_dir.clone(),
            Arc::clone(&self.certificate),
        );
    }

    pub(crate) fn snapshot(&self) -> CustomDomainSnapshot {
        let certificate = self
            .certificate
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let passkeys = self
            .passkeys
            .as_ref()
            .and_then(|runtime| runtime.views().ok())
            .unwrap_or_default();
        CustomDomainSnapshot {
            configured: self.configured,
            enabled: self.enabled(),
            name: self.domain.as_ref().map(|domain| domain.name.clone()),
            rp_id: self.domain.as_ref().map(|domain| domain.rp_id.clone()),
            origin: self.domain.as_ref().map(|domain| domain.origin.clone()),
            dns_provider: self
                .dns
                .as_ref()
                .map(dns::provider_name)
                .map(str::to_string),
            acme_issuance_enabled: self.acme_issuance_enabled,
            certificate_state: if certificate.state.is_empty() {
                "disabled".to_string()
            } else {
                certificate.state
            },
            certificate_not_after_unix_ms: certificate.not_after_unix_ms,
            acme_account_uri: certificate.acme_account_uri,
            initialization_error: self.initialization_error.clone().or(certificate.last_error),
            passkeys,
        }
    }

    pub(crate) fn registration_invite(
        &self,
        input: RegistrationInviteInput,
    ) -> Result<EnrollmentInvite, String> {
        self.passkeys()?.registration_invite(input)
    }

    pub(crate) fn registration_start(
        &self,
        input: RegistrationStartInput,
        origin: &str,
    ) -> Result<CeremonyStart, String> {
        self.passkeys()?.registration_start(input, origin)
    }

    pub(crate) fn registration_finish(
        &self,
        input: RegistrationFinishInput,
    ) -> Result<PasskeyView, String> {
        self.passkeys()?.registration_finish(input)
    }

    pub(crate) fn authentication_start(
        &self,
        input: AuthenticationStartInput,
        origin: &str,
        source_bucket: Option<&str>,
    ) -> Result<CeremonyStart, String> {
        self.passkeys()?
            .authentication_start(input, origin, source_bucket)
    }

    pub(crate) fn authentication_finish(
        &self,
        input: AuthenticationFinishInput,
        origin: &str,
    ) -> Result<PasskeyLeaseResult, String> {
        self.passkeys()?.authentication_finish(input, origin)
    }

    pub(crate) fn revoke(&self, input: RevokeInput) -> Result<bool, String> {
        self.passkeys()?.revoke(input)
    }

    fn passkeys(&self) -> Result<&passkeys::PasskeyRuntime, String> {
        self.passkeys.as_ref().ok_or_else(|| {
            self.initialization_error
                .clone()
                .unwrap_or_else(|| "custom-domain passkeys are not configured".to_string())
        })
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
        let runtime =
            CustomDomainRuntime::new(&CustomDomainConfig::default(), dir.path().into(), hosted);
        assert!(!runtime.configured());
        assert!(!runtime.enabled());
        assert_eq!(runtime.snapshot().certificate_state, "disabled");
    }
}
