use base64::Engine as _;
use futures_util::StreamExt as _;
use hickory_proto::op::{update_message, ResponseCode};
use hickory_proto::rr::rdata::{tsig::TsigAlgorithm, TXT};
use hickory_proto::rr::{Name, RData, Record, RecordSet, TSigner};
use hickory_proto::serialize::binary::BinEncodable as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::project::CustomDomainDnsConfig;

const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";
const DNS_PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
const CLOUDFLARE_RESPONSE_MAX_BYTES: usize = 256 * 1024;
const RFC2136_RESPONSE_MAX_BYTES: usize = 65_535;
const PENDING_CHALLENGE_FILE: &str = "custom-domain-dns-challenge.json";
const PENDING_CHALLENGE_SCHEMA_VERSION: u32 = 4;
const PENDING_CHALLENGE_MAX_BYTES: u64 = 64 * 1024;
const LATE_MUTATION_FILE: &str = "custom-domain-dns-late-cleanup.json";
const LATE_MUTATION_SCHEMA_VERSION: u32 = 3;
const LATE_MUTATION_MAX_BYTES: u64 = 256 * 1024;
const LATE_MUTATION_MAX_ENTRIES: usize = 16;
const PENDING_CHALLENGE_RETRY_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const PENDING_CHALLENGE_ACTIVE_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const PENDING_CHALLENGE_ERROR_RETRY_INITIAL: Duration = Duration::from_secs(30);
const PENDING_CHALLENGE_ERROR_RETRY_MAX: Duration = Duration::from_secs(30 * 60);
const PENDING_CHALLENGE_CREATE_LEASE_MS: u64 = 2 * 60 * 1000;
const PENDING_CHALLENGE_ACTIVE_LEASE_MS: u64 = 2 * 60 * 60 * 1000;
const PENDING_CHALLENGE_CLEANUP_LEASE_MS: u64 = 60 * 60 * 1000;
const PENDING_CHALLENGE_STALE_GRACE_MS: u64 = 60 * 1000;
// A request that timed out after transmission may still settle remotely.
// Keep exact cleanup authority for a full day before an authoritative
// lookup/delete is allowed to prove that uncertainty closed. This remains
// comfortably inside the 30-day certificate-renewal window.
const PENDING_CHALLENGE_MUTATION_UNCERTAINTY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PendingChallengePhase {
    Reserved,
    Creating,
    Active,
    Cleanup,
}

fn legacy_pending_challenge_phase() -> PendingChallengePhase {
    PendingChallengePhase::Active
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingChallenge {
    schema_version: u32,
    id: String,
    domain: String,
    record_name: String,
    value: String,
    provider: CustomDomainDnsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cloudflare_record_id: Option<String>,
    #[serde(default = "legacy_pending_challenge_phase")]
    phase: PendingChallengePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_token: Option<String>,
    #[serde(default)]
    lease_expires_unix_ms: u64,
    /// False while the call is only reserved or while a transmitted provider
    /// mutation may still complete after a stale creator loses this journal.
    /// The phase distinguishes those cases before cleanup removes authority.
    #[serde(default)]
    mutation_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LateMutationStore {
    schema_version: u32,
    entries: Vec<PendingChallenge>,
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_PENDING_CHALLENGE_WRITE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static FAIL_NEXT_LATE_MUTATION_WRITE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static FAIL_NEXT_PENDING_CREDENTIAL_SCRUB_REFRESH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static FAIL_AFTER_LATE_MUTATION_WRITE_COUNTDOWN: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug)]
pub(crate) struct DnsChallengeLease {
    id: String,
    owner_token: String,
}

enum ChallengeMutationResult {
    Applied(Option<String>),
    SettledWithoutMutation(String),
    ProviderResultAmbiguous(String),
}

enum ProviderMutationError {
    SettledWithoutMutation(String),
    ResultAmbiguous(String),
}

impl ProviderMutationError {
    fn settled(message: String) -> Self {
        Self::SettledWithoutMutation(message)
    }

    fn ambiguous(message: String) -> Self {
        Self::ResultAmbiguous(message)
    }

    fn into_challenge_result(self) -> ChallengeMutationResult {
        match self {
            Self::SettledWithoutMutation(error) => {
                ChallengeMutationResult::SettledWithoutMutation(error)
            }
            Self::ResultAmbiguous(error) => ChallengeMutationResult::ProviderResultAmbiguous(error),
        }
    }

    fn into_message(self) -> String {
        match self {
            Self::SettledWithoutMutation(error) | Self::ResultAmbiguous(error) => error,
        }
    }
}

pub(crate) async fn set_challenge_in(
    config: &CustomDomainDnsConfig,
    domain: &str,
    value: &str,
    cert_dir: &Path,
) -> Result<DnsChallengeLease, String> {
    let record_name = format!("_acme-challenge.{domain}");
    let owner_token = uuid::Uuid::new_v4().simple().to_string();
    let pending = PendingChallenge {
        schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().simple().to_string(),
        domain: domain.to_string(),
        record_name: record_name.clone(),
        value: value.to_string(),
        provider: config.clone(),
        cloudflare_record_id: None,
        phase: PendingChallengePhase::Creating,
        owner_token: Some(owner_token.clone()),
        lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
        mutation_complete: false,
    };
    let lease = DnsChallengeLease {
        id: pending.id.clone(),
        owner_token,
    };
    begin_pending_challenge(cert_dir, &pending)?;
    let mutation = match config {
        CustomDomainDnsConfig::Cloudflare {
            zone_id, token_env, ..
        } => match provider_secret(
            "dns:cloudflare",
            token_env.as_deref(),
            "CLOUDFLARE_API_TOKEN",
            "_API_TOKEN",
        ) {
            Ok(token) => match cloudflare_create(zone_id.trim(), &token, &record_name, value).await
            {
                Ok(record_id) => ChallengeMutationResult::Applied(Some(record_id)),
                Err(error) => error.into_challenge_result(),
            },
            Err(error) => ChallengeMutationResult::SettledWithoutMutation(error),
        },
        CustomDomainDnsConfig::Rfc2136 {
            server,
            zone,
            key_name,
            secret_env,
            ttl_secs,
            ..
        } => match provider_secret(
            "dns:rfc2136",
            secret_env.as_deref(),
            "INTENDANT_RFC2136_TSIG_SECRET",
            "_TSIG_SECRET",
        )
        .and_then(|secret| decode_tsig_secret(&secret))
        {
            Ok(key) => match rfc2136_update(
                server.trim(),
                zone.trim(),
                key_name.trim(),
                &key,
                &record_name,
                value,
                *ttl_secs,
                false,
            )
            .await
            {
                Ok(()) => ChallengeMutationResult::Applied(None),
                Err(error) => error.into_challenge_result(),
            },
            Err(error) => ChallengeMutationResult::SettledWithoutMutation(error),
        },
    };
    let (record_id, mutation_error, provider_result_settled, provider_mutation_possible) =
        match mutation {
            ChallengeMutationResult::Applied(record_id) => (record_id, None, true, true),
            ChallengeMutationResult::SettledWithoutMutation(error) => {
                (None, Some(error), true, false)
            }
            ChallengeMutationResult::ProviderResultAmbiguous(error) => {
                (None, Some(error), false, true)
            }
        };
    let cleanup_record_id = record_id.clone();
    let completion = record_challenge_mutation_complete_with_mode(
        cert_dir,
        &pending,
        &lease,
        record_id,
        mutation_error.is_none(),
        provider_result_settled,
    );
    match (completion, mutation_error) {
        (Ok(ChallengeMutationCompletion::Active), None) => Ok(lease),
        (Ok(ChallengeMutationCompletion::Active), Some(error)) => Err(error),
        (Ok(ChallengeMutationCompletion::CleanupRequired), original_error) => {
            let cleanup = if provider_mutation_possible {
                match retry_pending_challenge(cert_dir).await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("pending DNS challenge cleanup is still leased".to_string()),
                    Err(error) => Err(error),
                }
            } else {
                let mut resolved = pending.clone();
                resolved.phase = PendingChallengePhase::Cleanup;
                resolved.owner_token = None;
                resolved.lease_expires_unix_ms = 0;
                resolved.mutation_complete = true;
                retire_mutation_after_direct_resolution(cert_dir, &resolved)
            };
            let operation_error = original_error.unwrap_or_else(|| {
                "pending DNS challenge creation ownership changed before commit".to_string()
            });
            Err(match cleanup {
                Ok(()) => operation_error,
                Err(cleanup_error) => format!(
                    "{operation_error}; custom-domain DNS-01 cleanup remains pending: {cleanup_error}"
                ),
            })
        }
        (Err(completion_error), original_error) => {
            // A different challenge may have started after a stale cleanup
            // removed this creator's primary journal. Restore the bounded
            // secondary obligation before any direct delete so a crash or
            // transient write/delete failure remains retryable.
            let mut late = pending;
            late.cloudflare_record_id = cleanup_record_id;
            late.phase = PendingChallengePhase::Cleanup;
            late.owner_token = None;
            late.mutation_complete = provider_result_settled;
            late.lease_expires_unix_ms = if provider_result_settled {
                0
            } else {
                mutation_uncertainty_deadline()
            };
            let journal = ensure_late_mutation_cleanup(cert_dir, &late);
            let cleanup = if provider_mutation_possible {
                cleanup_pending_challenge(&late).await
            } else {
                Ok(())
            };
            let retire = if cleanup.is_ok() && provider_result_settled {
                retire_mutation_after_direct_resolution(cert_dir, &late)
            } else {
                Ok(())
            };
            let operation_error = original_error.unwrap_or(completion_error);
            let mut cleanup_errors = Vec::new();
            let direct_cleanup_reconciled =
                provider_result_settled && cleanup.is_ok() && retire.is_ok();
            if !direct_cleanup_reconciled {
                if let Err(error) = journal {
                    cleanup_errors
                        .push(format!("restore durable late-cleanup obligation: {error}"));
                }
            }
            if let Err(error) = cleanup {
                cleanup_errors.push(format!("exact cleanup of the late DNS mutation: {error}"));
            }
            if let Err(error) = retire {
                cleanup_errors.push(format!("retire durable late-cleanup obligation: {error}"));
            }
            if !provider_result_settled {
                cleanup_errors.push(
                    "provider result remains uncertain; durable exact cleanup will retry after the uncertainty window"
                        .to_string(),
                );
            }
            if cleanup_errors.is_empty() {
                Err(operation_error)
            } else {
                Err(format!(
                    "{operation_error}; custom-domain DNS-01 cleanup remains pending: {}",
                    cleanup_errors.join("; ")
                ))
            }
        }
    }
}

pub(crate) fn renew_pending_challenge(
    cert_dir: &Path,
    lease: &DnsChallengeLease,
) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut pending = load_pending_challenge_locked(cert_dir)
            .map_err(crate::access::AccessError)?
            .ok_or_else(|| {
                crate::access::AccessError(
                    "pending DNS challenge disappeared while it was active".to_string(),
                )
            })?;
        require_challenge_owner(&pending, lease)?;
        if pending.phase != PendingChallengePhase::Active {
            return Err(crate::access::AccessError(
                "pending DNS challenge is no longer active".to_string(),
            ));
        }
        pending.lease_expires_unix_ms =
            now_unix_ms().saturating_add(PENDING_CHALLENGE_ACTIVE_LEASE_MS);
        write_pending_challenge_locked(cert_dir, &pending)
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(crate) fn seed_active_challenge_for_test(
    cert_dir: &Path,
    domain: &str,
) -> Result<DnsChallengeLease, String> {
    let lease = DnsChallengeLease {
        id: uuid::Uuid::new_v4().simple().to_string(),
        owner_token: uuid::Uuid::new_v4().simple().to_string(),
    };
    let pending = PendingChallenge {
        schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
        id: lease.id.clone(),
        domain: domain.to_string(),
        record_name: format!("_acme-challenge.{domain}"),
        value: "test-active-challenge".to_string(),
        provider: CustomDomainDnsConfig::Cloudflare {
            zone_id: "test-zone".to_string(),
            token_env: Some("TEST_DNS_API_TOKEN".to_string()),
            propagation_delay_secs: 0,
        },
        cloudflare_record_id: Some("testrecord".to_string()),
        phase: PendingChallengePhase::Active,
        owner_token: Some(lease.owner_token.clone()),
        lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_ACTIVE_LEASE_MS),
        mutation_complete: true,
    };
    crate::access::authority_store::with_lock(cert_dir, || {
        write_pending_challenge_locked(cert_dir, &pending)
    })
    .map_err(|error| error.to_string())?;
    Ok(lease)
}

#[cfg(test)]
pub(crate) fn expire_active_challenge_for_test(
    cert_dir: &Path,
    lease: &DnsChallengeLease,
) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut pending = load_pending_challenge_locked(cert_dir)
            .map_err(crate::access::AccessError)?
            .ok_or_else(|| {
                crate::access::AccessError("test DNS challenge disappeared".to_string())
            })?;
        require_challenge_owner(&pending, lease)?;
        if pending.phase != PendingChallengePhase::Active {
            return Err(crate::access::AccessError(
                "test DNS challenge is no longer active".to_string(),
            ));
        }
        pending.lease_expires_unix_ms = now_unix_ms().saturating_sub(1);
        write_pending_challenge_locked(cert_dir, &pending)
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(crate) fn active_challenge_lease_is_current_for_test(
    cert_dir: &Path,
    lease: &DnsChallengeLease,
) -> Result<bool, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let pending =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?;
        Ok(pending.is_some_and(|pending| {
            pending.id == lease.id
                && pending.owner_token.as_deref() == Some(lease.owner_token.as_str())
                && pending.phase == PendingChallengePhase::Active
                && pending.lease_expires_unix_ms > now_unix_ms()
        }))
    })
    .map_err(|error| error.to_string())
}

pub(crate) fn mark_pending_challenge_cleanup(
    cert_dir: &Path,
    lease: &DnsChallengeLease,
) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut pending = load_pending_challenge_locked(cert_dir)
            .map_err(crate::access::AccessError)?
            .ok_or_else(|| {
                crate::access::AccessError(
                    "pending DNS challenge disappeared before cleanup".to_string(),
                )
            })?;
        if pending.id != lease.id {
            return Err(crate::access::AccessError(
                "pending DNS challenge changed before cleanup".to_string(),
            ));
        }
        if pending.phase != PendingChallengePhase::Cleanup {
            require_challenge_owner(&pending, lease)?;
            pending.phase = PendingChallengePhase::Cleanup;
            pending.owner_token = None;
            pending.lease_expires_unix_ms = 0;
            write_pending_challenge_locked(cert_dir, &pending)?;
        }
        Ok(())
    })
    .map_err(|error| error.to_string())
}

pub(crate) async fn retry_pending_challenge(cert_dir: &Path) -> Result<bool, String> {
    retire_unstarted_challenges(cert_dir)?;
    let primary_clear = if let Some((pending, cleanup_token)) = claim_pending_cleanup(cert_dir)? {
        let cleanup_result = cleanup_pending_challenge(&pending).await;
        if let Err(error) = cleanup_result {
            release_cleanup_claim(cert_dir, &pending.id, &cleanup_token)?;
            return Err(error);
        }
        if let Err(error) = remove_pending_challenge(cert_dir, &pending, &cleanup_token) {
            let _ = release_cleanup_claim(cert_dir, &pending.id, &cleanup_token);
            return Err(error);
        }
        true
    } else {
        load_pending_challenge(cert_dir)?.is_none()
    };

    let late_clear = if let Some((pending, cleanup_token)) = claim_late_mutation_cleanup(cert_dir)?
    {
        let cleanup_result = cleanup_pending_challenge(&pending).await;
        if let Err(error) = cleanup_result {
            release_late_mutation_cleanup(cert_dir, &pending.id, &cleanup_token)?;
            return Err(error);
        }
        if finish_late_mutation_cleanup(cert_dir, &pending, &cleanup_token)? {
            load_late_mutation_store(cert_dir)?.entries.is_empty()
        } else {
            false
        }
    } else {
        load_late_mutation_store(cert_dir)?.entries.is_empty()
    };
    Ok(primary_clear && late_clear)
}

async fn cleanup_pending_challenge(pending: &PendingChallenge) -> Result<(), String> {
    match &pending.provider {
        CustomDomainDnsConfig::Cloudflare {
            zone_id, token_env, ..
        } => {
            let token = provider_secret(
                "dns:cloudflare",
                token_env.as_deref(),
                "CLOUDFLARE_API_TOKEN",
                "_API_TOKEN",
            )?;
            if let Some(record_id) = pending.cloudflare_record_id.as_deref() {
                cloudflare_delete(zone_id.trim(), record_id, &token).await?;
            } else {
                let record_ids = cloudflare_find_exact(
                    zone_id.trim(),
                    &token,
                    &pending.record_name,
                    &pending.value,
                )
                .await?;
                for record_id in record_ids {
                    cloudflare_delete(zone_id.trim(), &record_id, &token).await?;
                }
            }
        }
        CustomDomainDnsConfig::Rfc2136 {
            server,
            zone,
            key_name,
            secret_env,
            ttl_secs,
            ..
        } => {
            let secret = provider_secret(
                "dns:rfc2136",
                secret_env.as_deref(),
                "INTENDANT_RFC2136_TSIG_SECRET",
                "_TSIG_SECRET",
            )?;
            let key = decode_tsig_secret(&secret)?;
            rfc2136_update(
                server.trim(),
                zone.trim(),
                key_name.trim(),
                &key,
                &pending.record_name,
                &pending.value,
                *ttl_secs,
                true,
            )
            .await
            .map_err(ProviderMutationError::into_message)?;
        }
    }
    Ok(())
}

pub(crate) fn spawn_cleanup_loop(cert_dir: PathBuf) {
    tokio::spawn(async move {
        let mut error_retry = PENDING_CHALLENGE_ERROR_RETRY_INITIAL;
        loop {
            let credential_generation = crate::credential_leases::dns_credential_grant_generation();
            match retry_pending_challenge(&cert_dir).await {
                Ok(true) => {
                    error_retry = PENDING_CHALLENGE_ERROR_RETRY_INITIAL;
                    tokio::time::sleep(PENDING_CHALLENGE_RETRY_INTERVAL).await;
                }
                Ok(false) => {
                    error_retry = PENDING_CHALLENGE_ERROR_RETRY_INITIAL;
                    tokio::time::sleep(PENDING_CHALLENGE_ACTIVE_RETRY_INTERVAL).await;
                }
                Err(error) => {
                    eprintln!("[custom-domain] pending DNS-01 cleanup: {error}");
                    let credential_granted =
                        crate::credential_leases::wait_for_dns_credential_grant_after(
                            credential_generation,
                            error_retry,
                        )
                        .await;
                    error_retry = if credential_granted {
                        PENDING_CHALLENGE_ERROR_RETRY_INITIAL
                    } else {
                        std::cmp::min(
                            error_retry.saturating_mul(2),
                            PENDING_CHALLENGE_ERROR_RETRY_MAX,
                        )
                    };
                }
            }
        }
    });
}

fn pending_challenge_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(PENDING_CHALLENGE_FILE)
}

fn late_mutation_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(LATE_MUTATION_FILE)
}

fn load_pending_challenge_locked(cert_dir: &Path) -> Result<Option<PendingChallenge>, String> {
    use std::io::Read as _;

    let path = pending_challenge_path(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let modified_unix_ms = file
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_else(now_unix_ms);
    let mut bytes = Vec::new();
    file.take(PENDING_CHALLENGE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > PENDING_CHALLENGE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the pending DNS challenge size limit",
            path.display()
        ));
    }
    let mut pending: PendingChallenge = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let loaded_schema = pending.schema_version;
    let legacy_schema = loaded_schema == 1;
    if legacy_schema {
        pending.phase = PendingChallengePhase::Active;
        pending.owner_token = None;
        pending.lease_expires_unix_ms =
            modified_unix_ms.saturating_add(PENDING_CHALLENGE_ACTIVE_LEASE_MS);
    }
    if (1..3).contains(&loaded_schema) {
        pending.mutation_complete = pending.phase != PendingChallengePhase::Creating;
    }
    if (1..PENDING_CHALLENGE_SCHEMA_VERSION).contains(&loaded_schema) {
        pending.schema_version = PENDING_CHALLENGE_SCHEMA_VERSION;
    }
    if !pending_challenge_is_valid(&pending, legacy_schema) {
        return Err(format!(
            "{} contains invalid pending DNS challenge state",
            path.display()
        ));
    }
    Ok(Some(pending))
}

fn pending_challenge_is_valid(pending: &PendingChallenge, legacy_schema: bool) -> bool {
    pending.schema_version == PENDING_CHALLENGE_SCHEMA_VERSION
        && !pending.id.is_empty()
        && pending.id.len() <= 64
        && !pending.domain.is_empty()
        && pending.domain.len() <= 253
        && pending.record_name == format!("_acme-challenge.{}", pending.domain)
        && !pending.value.is_empty()
        && pending.value.len() <= 1024
        && !pending.value.chars().any(char::is_control)
        && !pending
            .cloudflare_record_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        && !pending.owner_token.as_ref().is_some_and(|token| {
            token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        && (!matches!(
            pending.phase,
            PendingChallengePhase::Reserved
                | PendingChallengePhase::Creating
                | PendingChallengePhase::Active
        ) || pending.owner_token.is_some()
            || legacy_schema)
        && (!matches!(
            pending.phase,
            PendingChallengePhase::Reserved | PendingChallengePhase::Creating
        ) || !pending.mutation_complete)
        && (pending.phase != PendingChallengePhase::Active || pending.mutation_complete)
}

fn load_late_mutation_store_locked(cert_dir: &Path) -> Result<LateMutationStore, String> {
    use std::io::Read as _;

    let path = late_mutation_path(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LateMutationStore {
                schema_version: LATE_MUTATION_SCHEMA_VERSION,
                entries: Vec::new(),
            });
        }
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(LATE_MUTATION_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > LATE_MUTATION_MAX_BYTES {
        return Err(format!(
            "{} exceeds the late DNS cleanup size limit",
            path.display()
        ));
    }
    let mut store: LateMutationStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if (1..=LATE_MUTATION_SCHEMA_VERSION).contains(&store.schema_version) {
        store.schema_version = LATE_MUTATION_SCHEMA_VERSION;
        for pending in &mut store.entries {
            if (1..3).contains(&pending.schema_version) {
                pending.mutation_complete = pending.phase != PendingChallengePhase::Creating;
            }
            if (1..PENDING_CHALLENGE_SCHEMA_VERSION).contains(&pending.schema_version) {
                pending.schema_version = PENDING_CHALLENGE_SCHEMA_VERSION;
            }
        }
    }
    let mut ids = std::collections::HashSet::new();
    if store.schema_version != LATE_MUTATION_SCHEMA_VERSION
        || store.entries.len() > LATE_MUTATION_MAX_ENTRIES
        || store.entries.iter().any(|pending| {
            !matches!(
                (pending.phase, pending.mutation_complete),
                (PendingChallengePhase::Reserved, false)
                    | (PendingChallengePhase::Creating, false)
                    | (PendingChallengePhase::Cleanup, false)
                    | (PendingChallengePhase::Cleanup, true)
            ) || !pending_challenge_is_valid(pending, false)
                || !ids.insert(pending.id.clone())
        })
    {
        return Err(format!(
            "{} contains invalid late DNS cleanup state",
            path.display()
        ));
    }
    Ok(store)
}

fn load_late_mutation_store(cert_dir: &Path) -> Result<LateMutationStore, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
}

fn write_late_mutation_store_locked(
    cert_dir: &Path,
    store: &LateMutationStore,
) -> crate::access::AccessResult<()> {
    #[cfg(test)]
    if FAIL_NEXT_LATE_MUTATION_WRITE.with(|fail| fail.replace(false)) {
        return Err(crate::access::AccessError(
            "injected late DNS cleanup write failure".to_string(),
        ));
    }
    #[cfg(test)]
    let fail_after_write = FAIL_AFTER_LATE_MUTATION_WRITE_COUNTDOWN.with(|countdown| {
        let remaining = countdown.get();
        if remaining == 0 {
            false
        } else {
            countdown.set(remaining - 1);
            remaining == 1
        }
    });
    let result = if store.entries.is_empty() {
        crate::access::authority_store::remove_file_locked(&late_mutation_path(cert_dir))
    } else {
        let mut bytes = serde_json::to_vec_pretty(store).map_err(|error| {
            crate::access::AccessError(format!("serialize late DNS cleanup state: {error}"))
        })?;
        bytes.push(b'\n');
        if bytes.len() as u64 > LATE_MUTATION_MAX_BYTES {
            return Err(crate::access::AccessError(
                "late DNS cleanup state exceeds its size limit".to_string(),
            ));
        }
        crate::access::authority_store::atomic_write_private_locked(
            &late_mutation_path(cert_dir),
            &bytes,
        )
    };
    #[cfg(test)]
    if result.is_ok() && fail_after_write {
        return Err(crate::access::AccessError(
            "injected post-commit late DNS cleanup write failure".to_string(),
        ));
    }
    result
}

fn reserve_late_mutation_locked(
    cert_dir: &Path,
    pending: PendingChallenge,
) -> crate::access::AccessResult<()> {
    let mut store =
        load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
    if !matches!(
        pending.phase,
        PendingChallengePhase::Reserved | PendingChallengePhase::Creating
    ) || pending.mutation_complete
    {
        return Err(crate::access::AccessError(
            "late DNS cleanup reservation is invalid".to_string(),
        ));
    }
    if store.entries.iter().any(|entry| entry.id == pending.id) {
        return Err(crate::access::AccessError(
            "late DNS cleanup reservation already exists".to_string(),
        ));
    }
    if store.entries.len() >= LATE_MUTATION_MAX_ENTRIES {
        return Err(crate::access::AccessError(
            "late DNS cleanup backlog is full".to_string(),
        ));
    }
    store.entries.push(pending);
    write_late_mutation_store_locked(cert_dir, &store)
}

fn mark_late_mutation_may_transmit_locked(
    cert_dir: &Path,
    pending: &PendingChallenge,
) -> crate::access::AccessResult<()> {
    if pending.phase != PendingChallengePhase::Creating || pending.mutation_complete {
        return Err(crate::access::AccessError(
            "pending DNS challenge transmission marker is invalid".to_string(),
        ));
    }
    let mut store =
        load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
    let reservation = store
        .entries
        .iter_mut()
        .find(|reservation| reservation.id == pending.id)
        .ok_or_else(|| {
            crate::access::AccessError(
                "late DNS cleanup reservation disappeared before provider transmission".to_string(),
            )
        })?;
    if reservation.phase != PendingChallengePhase::Reserved
        || reservation.mutation_complete
        || reservation.owner_token != pending.owner_token
        || !cleanup_targets_match(reservation, pending)?
    {
        return Err(crate::access::AccessError(
            "late DNS cleanup reservation changed before provider transmission".to_string(),
        ));
    }
    reservation.phase = PendingChallengePhase::Creating;
    write_late_mutation_store_locked(cert_dir, &store)
}

fn retire_failed_challenge_setup_locked(
    cert_dir: &Path,
    pending: &PendingChallenge,
) -> crate::access::AccessResult<()> {
    let primary = load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?;
    let mut store =
        load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
    let reservation = store
        .entries
        .iter()
        .find(|reservation| reservation.id == pending.id);
    if primary.is_none() && reservation.is_none() {
        return Ok(());
    }
    if let Some(primary) = primary.as_ref() {
        if !matches!(
            primary.phase,
            PendingChallengePhase::Reserved | PendingChallengePhase::Creating
        ) || primary.mutation_complete
            || primary.owner_token != pending.owner_token
            || !cleanup_targets_match(primary, pending)?
        {
            return Err(crate::access::AccessError(
                "failed DNS challenge setup left inconsistent primary state".to_string(),
            ));
        }
    }
    let reservation = reservation.ok_or_else(|| {
        crate::access::AccessError(
            "failed DNS challenge setup lost its cleanup reservation".to_string(),
        )
    })?;
    if !matches!(
        reservation.phase,
        PendingChallengePhase::Reserved | PendingChallengePhase::Creating
    ) || reservation.mutation_complete
        || reservation.owner_token != pending.owner_token
        || !cleanup_targets_match(reservation, pending)?
    {
        return Err(crate::access::AccessError(
            "failed DNS challenge setup left inconsistent reservation state".to_string(),
        ));
    }

    if primary.is_some() {
        crate::access::authority_store::remove_file_locked(&pending_challenge_path(cert_dir))?;
    }
    store
        .entries
        .retain(|reservation| reservation.id != pending.id);
    write_late_mutation_store_locked(cert_dir, &store)
}

fn retire_unstarted_challenges_locked(cert_dir: &Path) -> crate::access::AccessResult<bool> {
    let mut store =
        load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
    if !store
        .entries
        .iter()
        .any(|pending| pending.phase == PendingChallengePhase::Reserved)
    {
        return Ok(false);
    }

    if let Some(primary) =
        load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
    {
        let reservation = store
            .entries
            .iter()
            .find(|reservation| reservation.id == primary.id)
            .ok_or_else(|| {
                crate::access::AccessError(
                    "unstarted DNS challenge journals are inconsistent".to_string(),
                )
            })?;
        if reservation.phase != PendingChallengePhase::Reserved
            || !matches!(
                primary.phase,
                PendingChallengePhase::Reserved | PendingChallengePhase::Creating
            )
            || primary.mutation_complete
            || primary.owner_token != reservation.owner_token
            || !cleanup_targets_match(&primary, reservation)?
        {
            return Err(crate::access::AccessError(
                "unstarted DNS challenge journals are inconsistent".to_string(),
            ));
        }
        // Remove the primary first. If retiring the secondary store then
        // fails, its durable Reserved phase remains proof that no provider
        // transmission was authorized and recovery can repeat safely.
        crate::access::authority_store::remove_file_locked(&pending_challenge_path(cert_dir))?;
    }

    store
        .entries
        .retain(|pending| pending.phase != PendingChallengePhase::Reserved);
    write_late_mutation_store_locked(cert_dir, &store)?;
    Ok(true)
}

fn retire_unstarted_challenges(cert_dir: &Path) -> Result<(), String> {
    let changed = crate::access::authority_store::with_lock(cert_dir, || {
        retire_unstarted_challenges_locked(cert_dir)
    })
    .map_err(|error| error.to_string())?;
    if changed {
        refresh_pending_credential_child_scrub(cert_dir)?;
    }
    Ok(())
}

fn load_pending_challenge(cert_dir: &Path) -> Result<Option<PendingChallenge>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
}

fn pending_challenge_credential_env_name(pending: &PendingChallenge) -> Result<String, String> {
    match &pending.provider {
        CustomDomainDnsConfig::Cloudflare { token_env, .. } => {
            crate::credential_leases::dns_credential_env_name(
                token_env.as_deref(),
                "CLOUDFLARE_API_TOKEN",
                "_API_TOKEN",
            )
        }
        CustomDomainDnsConfig::Rfc2136 { secret_env, .. } => {
            crate::credential_leases::dns_credential_env_name(
                secret_env.as_deref(),
                "INTENDANT_RFC2136_TSIG_SECRET",
                "_TSIG_SECRET",
            )
        }
    }
}

/// Synchronize the supervised-child scrub with durable cleanup authority.
/// The scrub setter records an unreadable journal as fail-closed state.
pub(super) fn refresh_pending_credential_child_scrub(cert_dir: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_NEXT_PENDING_CREDENTIAL_SCRUB_REFRESH.with(|fail| fail.replace(false)) {
        let error = "injected pending DNS credential child-scrub refresh failure".to_string();
        crate::credential_leases::configure_pending_dns_credential_child_scrub(
            cert_dir,
            Err(error.clone()),
        );
        return Err(error);
    }
    let env_name = load_pending_challenge(cert_dir).and_then(|pending| {
        let mut names = pending
            .as_ref()
            .map(pending_challenge_credential_env_name)
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        names.extend(
            load_late_mutation_store(cert_dir)?
                .entries
                .iter()
                .map(pending_challenge_credential_env_name)
                .collect::<Result<Vec<_>, _>>()?,
        );
        names.sort();
        names.dedup();
        match names.as_slice() {
            [] => Ok(None),
            [name] => Ok(Some(name.clone())),
            _ => {
                Err("late DNS cleanup backlog references multiple credential fallbacks".to_string())
            }
        }
    });
    crate::credential_leases::configure_pending_dns_credential_child_scrub(
        cert_dir,
        env_name.clone(),
    );
    env_name.map(|_| ())
}

/// Refresh the journal-derived scrub and run one child-spawn edge under the
/// same authority-store lock. Every journal writer uses this lock, so a
/// sibling process cannot create cleanup authority between the refresh and
/// the point where the operating system copies the child's environment.
pub(super) fn with_pending_credential_child_scrub<T>(
    cert_dir: &Path,
    operation: impl FnOnce() -> T,
) -> T {
    let mut operation = Some(operation);
    let locked = crate::access::authority_store::with_lock(cert_dir, || {
        if let Err(error) = refresh_pending_credential_child_scrub(cert_dir) {
            eprintln!("[custom-domain] load pending DNS credential child-scrub state: {error}");
        }
        Ok(operation.take().expect(
            "pending DNS credential child-scrub operation consumed",
        )())
    });
    match locked {
        Ok(result) => result,
        Err(error) => {
            crate::credential_leases::configure_pending_dns_credential_child_scrub(
                cert_dir,
                Err(error.to_string()),
            );
            eprintln!("[custom-domain] lock pending DNS credential child-scrub state: {error}");
            operation
                .take()
                .expect("pending DNS credential child-scrub operation consumed")()
        }
    }
}

fn write_pending_challenge_locked(
    cert_dir: &Path,
    pending: &PendingChallenge,
) -> crate::access::AccessResult<()> {
    #[cfg(test)]
    if FAIL_NEXT_PENDING_CHALLENGE_WRITE.with(|fail| fail.replace(false)) {
        return Err(crate::access::AccessError(
            "injected pending DNS challenge write failure".to_string(),
        ));
    }
    let mut bytes = serde_json::to_vec_pretty(pending).map_err(|error| {
        crate::access::AccessError(format!("serialize pending DNS challenge: {error}"))
    })?;
    bytes.push(b'\n');
    if bytes.len() as u64 > PENDING_CHALLENGE_MAX_BYTES {
        return Err(crate::access::AccessError(
            "pending DNS challenge exceeds its size limit".to_string(),
        ));
    }
    crate::access::authority_store::atomic_write_private_locked(
        &pending_challenge_path(cert_dir),
        &bytes,
    )
}

fn begin_pending_challenge(cert_dir: &Path, pending: &PendingChallenge) -> Result<(), String> {
    // Validate scrub metadata before writing either journal. A configuration
    // rejected at this point has made no provider call and therefore must not
    // leave a cleanup obligation that waits for an unusable credential name.
    pending_challenge_credential_env_name(pending)?;
    if pending.phase != PendingChallengePhase::Creating || pending.mutation_complete {
        return Err("new pending DNS challenge state is invalid".to_string());
    }
    let result = crate::access::authority_store::with_lock(cert_dir, || {
        retire_unstarted_challenges_locked(cert_dir)?;
        if load_pending_challenge_locked(cert_dir)
            .map_err(crate::access::AccessError)?
            .is_some()
            || !load_late_mutation_store_locked(cert_dir)
                .map_err(crate::access::AccessError)?
                .entries
                .is_empty()
        {
            return Err(crate::access::AccessError(
                "a pending custom-domain DNS challenge must be cleaned up before another is created"
                    .to_string(),
            ));
        }
        // Reserved is a durable proof that no provider call became eligible.
        // Keep the authority lock through both journals' transition to
        // Creating so a live caller never exposes a retireable reservation.
        let mut reserved = pending.clone();
        reserved.phase = PendingChallengePhase::Reserved;
        let setup = (|| {
            reserve_late_mutation_locked(cert_dir, reserved.clone())?;
            write_pending_challenge_locked(cert_dir, &reserved)?;
            write_pending_challenge_locked(cert_dir, pending)?;
            refresh_pending_credential_child_scrub(cert_dir).map_err(crate::access::AccessError)?;
            // This commit marker is the final fallible operation before the
            // caller can enter the provider path.
            mark_late_mutation_may_transmit_locked(cert_dir, pending)
        })();
        if let Err(error) = setup {
            return match retire_failed_challenge_setup_locked(cert_dir, pending) {
                Ok(()) => Err(error),
                Err(retire_error) => Err(crate::access::AccessError(format!(
                    "{error}; retire failed DNS challenge setup: {retire_error}"
                ))),
            };
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => match refresh_pending_credential_child_scrub(cert_dir) {
            Ok(()) => Err(error.to_string()),
            Err(refresh_error) => Err(format!(
                "{error}; refresh pending DNS credential child-scrub state: {refresh_error}"
            )),
        },
    }
}

fn require_challenge_owner(
    pending: &PendingChallenge,
    lease: &DnsChallengeLease,
) -> crate::access::AccessResult<()> {
    if pending.id != lease.id || pending.owner_token.as_deref() != Some(lease.owner_token.as_str())
    {
        return Err(crate::access::AccessError(
            "pending DNS challenge ownership changed".to_string(),
        ));
    }
    Ok(())
}

fn cleanup_targets_match(
    left: &PendingChallenge,
    right: &PendingChallenge,
) -> crate::access::AccessResult<bool> {
    let left_provider = serde_json::to_vec(&left.provider).map_err(|error| {
        crate::access::AccessError(format!(
            "serialize stored DNS provider configuration: {error}"
        ))
    })?;
    let right_provider = serde_json::to_vec(&right.provider).map_err(|error| {
        crate::access::AccessError(format!(
            "serialize completed DNS provider configuration: {error}"
        ))
    })?;
    Ok(left.id == right.id
        && left.domain == right.domain
        && left.record_name == right.record_name
        && left.value == right.value
        && left_provider == right_provider)
}

fn merge_cleanup_record_id(
    stored: &mut PendingChallenge,
    completed: &PendingChallenge,
) -> crate::access::AccessResult<()> {
    if stored.cloudflare_record_id.is_some()
        && completed.cloudflare_record_id.is_some()
        && stored.cloudflare_record_id != completed.cloudflare_record_id
    {
        return Err(crate::access::AccessError(
            "DNS cleanup record id changed during provider completion".to_string(),
        ));
    }
    if completed.cloudflare_record_id.is_some() {
        stored.cloudflare_record_id = completed.cloudflare_record_id.clone();
    }
    Ok(())
}

/// Restore the cleanup target after a completion-journal error. The
/// pre-provider reservation may still be in Reserved or Creating state, so upgrade both
/// matching journals without weakening a previously settled result.
fn ensure_late_mutation_cleanup(
    cert_dir: &Path,
    completed: &PendingChallenge,
) -> Result<(), String> {
    if completed.phase != PendingChallengePhase::Cleanup
        || completed.owner_token.is_some()
        || !pending_challenge_is_valid(completed, false)
    {
        return Err("completed late DNS cleanup obligation is invalid".to_string());
    }
    let result = crate::access::authority_store::with_lock(cert_dir, || {
        let mut store =
            load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
        if let Some(stored) = store
            .entries
            .iter_mut()
            .find(|stored| stored.id == completed.id)
        {
            if !cleanup_targets_match(stored, completed)? {
                return Err(crate::access::AccessError(
                    "late DNS cleanup target changed during provider completion".to_string(),
                ));
            }
            merge_cleanup_record_id(stored, completed)?;
            stored.phase = PendingChallengePhase::Cleanup;
            stored.owner_token = None;
            if completed.mutation_complete {
                stored.mutation_complete = true;
                stored.lease_expires_unix_ms = 0;
            } else if !stored.mutation_complete {
                stored.lease_expires_unix_ms = stored
                    .lease_expires_unix_ms
                    .max(completed.lease_expires_unix_ms);
            }
        } else {
            if store.entries.len() >= LATE_MUTATION_MAX_ENTRIES {
                return Err(crate::access::AccessError(
                    "late DNS cleanup backlog is full".to_string(),
                ));
            }
            store.entries.push(completed.clone());
        }
        // Persist the secondary obligation first. If the primary write fails,
        // restart recovery still has a complete exact cleanup target.
        write_late_mutation_store_locked(cert_dir, &store)?;

        if let Some(mut primary) =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
        {
            if primary.id == completed.id {
                if !cleanup_targets_match(&primary, completed)? {
                    return Err(crate::access::AccessError(
                        "primary DNS cleanup target changed during provider completion".to_string(),
                    ));
                }
                merge_cleanup_record_id(&mut primary, completed)?;
                primary.phase = PendingChallengePhase::Cleanup;
                primary.owner_token = None;
                if completed.mutation_complete {
                    primary.mutation_complete = true;
                    primary.lease_expires_unix_ms = 0;
                } else if !primary.mutation_complete {
                    primary.lease_expires_unix_ms = primary
                        .lease_expires_unix_ms
                        .max(completed.lease_expires_unix_ms);
                }
                write_pending_challenge_locked(cert_dir, &primary)?;
            }
        }
        Ok(())
    });
    result.map_err(|error| error.to_string())?;
    refresh_pending_credential_child_scrub(cert_dir)
}

/// A direct resolution completed after the normal journal transition failed:
/// either exact provider cleanup succeeded, or validation proved no provider
/// call occurred. Remove only matching targets; a replacement primary is
/// untouched. Primary is removed first so any later write failure leaves the
/// secondary obligation durable rather than an incomplete primary without
/// its fence.
fn retire_mutation_after_direct_resolution(
    cert_dir: &Path,
    completed: &PendingChallenge,
) -> Result<(), String> {
    if completed.phase != PendingChallengePhase::Cleanup
        || completed.owner_token.is_some()
        || !completed.mutation_complete
        || !pending_challenge_is_valid(completed, false)
    {
        return Err("settled direct DNS cleanup target is invalid".to_string());
    }
    let result = crate::access::authority_store::with_lock(cert_dir, || {
        let mut store =
            load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
        if let Some(stored) = store
            .entries
            .iter()
            .find(|stored| stored.id == completed.id)
        {
            if !cleanup_targets_match(stored, completed)? {
                return Err(crate::access::AccessError(
                    "late DNS cleanup target changed before direct retirement".to_string(),
                ));
            }
            if stored.cloudflare_record_id.is_some()
                && completed.cloudflare_record_id.is_some()
                && stored.cloudflare_record_id != completed.cloudflare_record_id
            {
                return Err(crate::access::AccessError(
                    "late DNS cleanup record id changed before direct retirement".to_string(),
                ));
            }
        }

        if let Some(primary) =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
        {
            if primary.id == completed.id {
                if !cleanup_targets_match(&primary, completed)? {
                    return Err(crate::access::AccessError(
                        "primary DNS cleanup target changed before direct retirement".to_string(),
                    ));
                }
                if primary.cloudflare_record_id.is_some()
                    && completed.cloudflare_record_id.is_some()
                    && primary.cloudflare_record_id != completed.cloudflare_record_id
                {
                    return Err(crate::access::AccessError(
                        "primary DNS cleanup record id changed before direct retirement"
                            .to_string(),
                    ));
                }
                crate::access::authority_store::remove_file_locked(&pending_challenge_path(
                    cert_dir,
                ))?;
            }
        }

        store.entries.retain(|stored| stored.id != completed.id);
        write_late_mutation_store_locked(cert_dir, &store)
    });
    result.map_err(|error| error.to_string())?;
    refresh_pending_credential_child_scrub(cert_dir)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChallengeMutationCompletion {
    Active,
    CleanupRequired,
}

#[cfg(test)]
fn record_challenge_mutation_complete(
    cert_dir: &Path,
    template: &PendingChallenge,
    lease: &DnsChallengeLease,
    record_id: Option<String>,
) -> Result<ChallengeMutationCompletion, String> {
    record_challenge_mutation_complete_with_mode(cert_dir, template, lease, record_id, true, true)
}

fn record_challenge_mutation_complete_with_mode(
    cert_dir: &Path,
    template: &PendingChallenge,
    lease: &DnsChallengeLease,
    record_id: Option<String>,
    provider_succeeded: bool,
    provider_result_settled: bool,
) -> Result<ChallengeMutationCompletion, String> {
    if provider_succeeded && !provider_result_settled {
        return Err("a successful DNS provider result must be settled".to_string());
    }
    if record_id.as_ref().is_some_and(|record_id| {
        record_id.is_empty() || !record_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err("Cloudflare DNS create returned an invalid record id".to_string());
    }
    let uncertainty_deadline = if provider_result_settled {
        0
    } else {
        mutation_uncertainty_deadline()
    };
    let result = crate::access::authority_store::with_lock(cert_dir, || {
        let mut late_store =
            load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
        let reservation = late_store
            .entries
            .iter_mut()
            .find(|pending| pending.id == lease.id)
            .ok_or_else(|| {
                crate::access::AccessError(
                    "late DNS cleanup reservation disappeared before provider completion"
                        .to_string(),
                )
            })?;
        let reserved_provider = serde_json::to_vec(&reservation.provider).map_err(|error| {
            crate::access::AccessError(format!(
                "serialize reserved DNS provider configuration: {error}"
            ))
        })?;
        let template_provider = serde_json::to_vec(&template.provider).map_err(|error| {
            crate::access::AccessError(format!(
                "serialize completed DNS provider configuration: {error}"
            ))
        })?;
        if reservation.domain != template.domain
            || reservation.record_name != template.record_name
            || reservation.value != template.value
            || reserved_provider != template_provider
        {
            return Err(crate::access::AccessError(
                "late DNS cleanup reservation changed before provider completion".to_string(),
            ));
        }
        reservation.cloudflare_record_id = record_id.clone();
        reservation.phase = PendingChallengePhase::Cleanup;
        reservation.owner_token = None;
        reservation.lease_expires_unix_ms = uncertainty_deadline;
        // A transport error after transmission does not prove that the
        // provider rejected the mutation. Preserve that distinction until
        // the bounded uncertainty window closes.
        reservation.mutation_complete = provider_result_settled;
        // Persist the cleanup-capable state before consulting the replaceable
        // primary journal. A failed write leaves the pre-call Creating
        // reservation intact, which stale recovery also treats as an exact
        // cleanup obligation.
        write_late_mutation_store_locked(cert_dir, &late_store)?;

        let current =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?;
        let mut pending = match current {
            Some(pending) if pending.id == lease.id => pending,
            Some(_) | None => return Ok(ChallengeMutationCompletion::CleanupRequired),
        };
        let owner_is_current = require_challenge_owner(&pending, lease).is_ok()
            && pending.phase == PendingChallengePhase::Creating
            && now_unix_ms()
                <= pending
                    .lease_expires_unix_ms
                    .saturating_add(PENDING_CHALLENGE_STALE_GRACE_MS);
        pending.cloudflare_record_id = record_id;
        pending.mutation_complete = provider_result_settled;
        let completion = if provider_succeeded && owner_is_current {
            pending.phase = PendingChallengePhase::Active;
            pending.lease_expires_unix_ms =
                now_unix_ms().saturating_add(PENDING_CHALLENGE_ACTIVE_LEASE_MS);
            ChallengeMutationCompletion::Active
        } else {
            pending.phase = PendingChallengePhase::Cleanup;
            pending.owner_token = None;
            pending.lease_expires_unix_ms = uncertainty_deadline;
            ChallengeMutationCompletion::CleanupRequired
        };
        write_pending_challenge_locked(cert_dir, &pending)?;
        if provider_result_settled {
            // The primary journal now carries the settled obligation. Retire
            // the reservation last; if this write fails, duplicate durable
            // cleanup is safe and the caller does not proceed as though
            // creation succeeded.
            late_store
                .entries
                .retain(|reservation| reservation.id != lease.id);
            write_late_mutation_store_locked(cert_dir, &late_store)?;
        }
        Ok(completion)
    })
    .map_err(|error| error.to_string())?;
    refresh_pending_credential_child_scrub(cert_dir)?;
    Ok(result)
}

fn claim_late_mutation_cleanup(
    cert_dir: &Path,
) -> Result<Option<(PendingChallenge, String)>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut store =
            load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
        let now = now_unix_ms();
        let mut changed = false;
        let mut claim_index = None;
        for (index, pending) in store.entries.iter_mut().enumerate() {
            let stale = now
                > pending
                    .lease_expires_unix_ms
                    .saturating_add(PENDING_CHALLENGE_STALE_GRACE_MS);
            match pending.phase {
                PendingChallengePhase::Creating if stale => {
                    // A creator disappeared without recording whether its
                    // transmitted request settled. Start a fresh uncertainty
                    // window; an immediate absence check is not conclusive.
                    pending.phase = PendingChallengePhase::Cleanup;
                    pending.owner_token = None;
                    pending.lease_expires_unix_ms = mutation_uncertainty_deadline();
                    changed = true;
                }
                PendingChallengePhase::Cleanup if !pending.mutation_complete => {
                    if pending.owner_token.is_some() {
                        if stale {
                            // This may be a claim minted by an older binary
                            // that did not preserve the uncertainty horizon.
                            pending.owner_token = None;
                            pending.lease_expires_unix_ms = mutation_uncertainty_deadline();
                            changed = true;
                        }
                    } else if pending.lease_expires_unix_ms == 0 {
                        // Upgrade an older incomplete cleanup journal.
                        pending.lease_expires_unix_ms = mutation_uncertainty_deadline();
                        changed = true;
                    } else if now > pending.lease_expires_unix_ms {
                        // The bounded uncertainty window has closed. The
                        // ensuing exact provider cleanup is authoritative.
                        pending.mutation_complete = true;
                        claim_index = Some(index);
                        break;
                    }
                }
                PendingChallengePhase::Cleanup if pending.owner_token.is_none() || stale => {
                    claim_index = Some(index);
                    break;
                }
                PendingChallengePhase::Reserved
                | PendingChallengePhase::Creating
                | PendingChallengePhase::Active
                | PendingChallengePhase::Cleanup => {}
            }
        }
        let Some(claim_index) = claim_index else {
            if changed {
                write_late_mutation_store_locked(cert_dir, &store)?;
            }
            return Ok(None);
        };
        let cleanup_token = uuid::Uuid::new_v4().simple().to_string();
        let pending = &mut store.entries[claim_index];
        pending.owner_token = Some(cleanup_token.clone());
        pending.lease_expires_unix_ms = now.saturating_add(PENDING_CHALLENGE_CLEANUP_LEASE_MS);
        let claimed = pending.clone();
        write_late_mutation_store_locked(cert_dir, &store)?;
        Ok(Some((claimed, cleanup_token)))
    })
    .map_err(|error| error.to_string())
}

fn release_late_mutation_cleanup(
    cert_dir: &Path,
    pending_id: &str,
    cleanup_token: &str,
) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut store =
            load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
        let pending = store
            .entries
            .iter_mut()
            .find(|pending| pending.id == pending_id)
            .ok_or_else(|| {
                crate::access::AccessError("late DNS cleanup obligation disappeared".to_string())
            })?;
        if pending.owner_token.as_deref() != Some(cleanup_token) {
            return Err(crate::access::AccessError(
                "late DNS cleanup ownership changed".to_string(),
            ));
        }
        pending.owner_token = None;
        pending.lease_expires_unix_ms = 0;
        write_late_mutation_store_locked(cert_dir, &store)
    })
    .map_err(|error| error.to_string())
}

fn remove_late_mutation_cleanup(
    cert_dir: &Path,
    claimed: &PendingChallenge,
    cleanup_token: &str,
) -> Result<(), String> {
    if !claimed.mutation_complete {
        return Err(
            "cannot retire a late DNS cleanup before provider completion is recorded".to_string(),
        );
    }
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut store =
            load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
        let pending = store
            .entries
            .iter()
            .find(|pending| pending.id == claimed.id)
            .ok_or_else(|| {
                crate::access::AccessError("late DNS cleanup obligation disappeared".to_string())
            })?;
        if pending.owner_token.as_deref() != Some(cleanup_token)
            || pending.cloudflare_record_id != claimed.cloudflare_record_id
            || pending.value != claimed.value
        {
            return Err(crate::access::AccessError(
                "late DNS cleanup obligation changed".to_string(),
            ));
        }
        store.entries.retain(|pending| pending.id != claimed.id);
        write_late_mutation_store_locked(cert_dir, &store)
    })
    .map_err(|error| error.to_string())?;
    refresh_pending_credential_child_scrub(cert_dir)
}

fn finish_late_mutation_cleanup(
    cert_dir: &Path,
    claimed: &PendingChallenge,
    cleanup_token: &str,
) -> Result<bool, String> {
    if claimed.mutation_complete {
        remove_late_mutation_cleanup(cert_dir, claimed, cleanup_token)?;
        Ok(true)
    } else {
        // Exact cleanup succeeded, but the provider call that may create this
        // record has not durably completed. Keep the bounded reservation and
        // retry instead of turning a transient absence into lost authority.
        release_late_mutation_cleanup(cert_dir, &claimed.id, cleanup_token)?;
        Ok(false)
    }
}

fn claim_pending_cleanup(cert_dir: &Path) -> Result<Option<(PendingChallenge, String)>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let Some(mut pending) =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
        else {
            return Ok(None);
        };
        let now = now_unix_ms();
        let stale = now
            > pending
                .lease_expires_unix_ms
                .saturating_add(PENDING_CHALLENGE_STALE_GRACE_MS);
        let cleanup_available = match pending.phase {
            PendingChallengePhase::Reserved => false,
            PendingChallengePhase::Creating if stale => {
                pending.phase = PendingChallengePhase::Cleanup;
                pending.owner_token = None;
                pending.lease_expires_unix_ms = mutation_uncertainty_deadline();
                write_pending_challenge_locked(cert_dir, &pending)?;
                return Ok(None);
            }
            PendingChallengePhase::Creating => false,
            PendingChallengePhase::Active => stale,
            PendingChallengePhase::Cleanup if !pending.mutation_complete => {
                if pending.owner_token.is_some() {
                    if stale {
                        pending.owner_token = None;
                        pending.lease_expires_unix_ms = mutation_uncertainty_deadline();
                        write_pending_challenge_locked(cert_dir, &pending)?;
                    }
                    return Ok(None);
                }
                if pending.lease_expires_unix_ms == 0 {
                    pending.lease_expires_unix_ms = mutation_uncertainty_deadline();
                    write_pending_challenge_locked(cert_dir, &pending)?;
                    return Ok(None);
                }
                if now <= pending.lease_expires_unix_ms {
                    return Ok(None);
                }
                pending.mutation_complete = true;
                true
            }
            PendingChallengePhase::Cleanup => pending.owner_token.is_none() || stale,
        };
        if !cleanup_available {
            return Ok(None);
        }
        let cleanup_token = uuid::Uuid::new_v4().simple().to_string();
        pending.phase = PendingChallengePhase::Cleanup;
        pending.owner_token = Some(cleanup_token.clone());
        pending.lease_expires_unix_ms = now.saturating_add(PENDING_CHALLENGE_CLEANUP_LEASE_MS);
        write_pending_challenge_locked(cert_dir, &pending)?;
        Ok(Some((pending, cleanup_token)))
    })
    .map_err(|error| error.to_string())
}

fn release_cleanup_claim(
    cert_dir: &Path,
    pending_id: &str,
    cleanup_token: &str,
) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let Some(mut pending) =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
        else {
            return Ok(());
        };
        if pending.id != pending_id
            || pending.phase != PendingChallengePhase::Cleanup
            || pending.owner_token.as_deref() != Some(cleanup_token)
        {
            return Err(crate::access::AccessError(
                "pending DNS challenge cleanup ownership changed".to_string(),
            ));
        }
        pending.owner_token = None;
        pending.lease_expires_unix_ms = 0;
        write_pending_challenge_locked(cert_dir, &pending)
    })
    .map_err(|error| error.to_string())
}

fn remove_pending_challenge(
    cert_dir: &Path,
    claimed: &PendingChallenge,
    cleanup_token: &str,
) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let Some(pending) =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
        else {
            return Ok(());
        };
        if pending.id != claimed.id
            || pending.phase != PendingChallengePhase::Cleanup
            || pending.owner_token.as_deref() != Some(cleanup_token)
            || pending.mutation_complete != claimed.mutation_complete
            || pending.cloudflare_record_id != claimed.cloudflare_record_id
        {
            return Err(crate::access::AccessError(
                "pending DNS challenge changed during cleanup".to_string(),
            ));
        }
        if !pending.mutation_complete {
            let late_store =
                load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
            let has_ambiguous_reservation = late_store.entries.iter().any(|reservation| {
                reservation.id == pending.id
                    && reservation.record_name == pending.record_name
                    && reservation.value == pending.value
                    && !reservation.mutation_complete
            });
            if !has_ambiguous_reservation {
                return Err(crate::access::AccessError(
                    "ambiguous DNS mutation has no durable late-cleanup reservation".to_string(),
                ));
            }
        }
        crate::access::authority_store::remove_file_locked(&pending_challenge_path(cert_dir))
    })
    .map_err(|error| error.to_string())?;
    refresh_pending_credential_child_scrub(cert_dir)
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

fn mutation_uncertainty_deadline() -> u64 {
    now_unix_ms().saturating_add(PENDING_CHALLENGE_MUTATION_UNCERTAINTY_MS)
}

pub(crate) fn propagation_delay_secs(config: &CustomDomainDnsConfig) -> u64 {
    match config {
        CustomDomainDnsConfig::Cloudflare {
            propagation_delay_secs,
            ..
        }
        | CustomDomainDnsConfig::Rfc2136 {
            propagation_delay_secs,
            ..
        } => *propagation_delay_secs,
    }
}

pub(crate) fn provider_name(config: &CustomDomainDnsConfig) -> &'static str {
    match config {
        CustomDomainDnsConfig::Cloudflare { .. } => "cloudflare",
        CustomDomainDnsConfig::Rfc2136 { .. } => "rfc2136",
    }
}

fn provider_secret(
    kind: &str,
    configured_env: Option<&str>,
    default_env: &str,
    required_suffix: &str,
) -> Result<String, String> {
    let env_name = crate::credential_leases::dns_credential_env_name(
        configured_env,
        default_env,
        required_suffix,
    )?;
    if let Some(secret) =
        crate::credential_leases::leased_secret(kind).filter(|value| !value.trim().is_empty())
    {
        return Ok(secret.trim().to_string());
    }
    std::env::var(&env_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("no active {kind} credential lease or {env_name} value"))
}

#[derive(Deserialize)]
struct CloudflareEnvelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CloudflareError>,
    result: Option<T>,
    #[serde(default)]
    result_info: Option<CloudflareResultInfo>,
}

#[derive(Deserialize)]
struct CloudflareError {
    #[serde(default)]
    code: u64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct CloudflareRecord {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    content: String,
    #[serde(default, rename = "type")]
    record_type: String,
}

#[derive(Deserialize)]
struct CloudflareResultInfo {
    page: u64,
    total_pages: u64,
}

async fn cloudflare_create(
    zone_id: &str,
    token: &str,
    name: &str,
    value: &str,
) -> Result<String, ProviderMutationError> {
    if zone_id.is_empty() || !zone_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(ProviderMutationError::settled(
            "Cloudflare zone_id is invalid".to_string(),
        ));
    }
    let response = cloudflare_client()
        .map_err(ProviderMutationError::settled)?
        .post(format!("{CLOUDFLARE_API_BASE}/zones/{zone_id}/dns_records"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "type": "TXT",
            "name": name,
            "content": value,
            "ttl": 60,
            "comment": "Intendant custom-domain ACME DNS-01",
        }))
        .send()
        .await
        .map_err(cloudflare_create_send_error)?;
    let status = response.status();
    let body = cloudflare_response_body(response, "response")
        .await
        .map_err(|error| cloudflare_create_response_error(status, false, error))?;
    let envelope: CloudflareEnvelope<CloudflareRecord> =
        serde_json::from_slice(&body).map_err(|error| {
            cloudflare_create_response_error(
                status,
                false,
                format!("parse Cloudflare DNS response ({status}): {error}"),
            )
        })?;
    if !status.is_success() || !envelope.success {
        return Err(cloudflare_create_response_error(
            status,
            !envelope.success,
            format!(
                "Cloudflare DNS create failed ({status}): {}",
                cloudflare_error_text(&envelope.errors)
            ),
        ));
    }
    cloudflare_created_record_id(envelope.result, name, value)
}

fn cloudflare_created_record_id(
    record: Option<CloudflareRecord>,
    requested_name: &str,
    requested_value: &str,
) -> Result<String, ProviderMutationError> {
    let record = record.ok_or_else(|| {
        ProviderMutationError::ambiguous(
            "Cloudflare DNS create returned no record result".to_string(),
        )
    })?;
    if record.record_type != "TXT"
        || !record
            .name
            .trim_end_matches('.')
            .eq_ignore_ascii_case(requested_name.trim_end_matches('.'))
        || record.content != requested_value
    {
        // A successful but mismatched response does not establish that this
        // id belongs to our mutation. Leave the id out of the journal so the
        // ambiguous-result path cleans up only an exact name/value match.
        return Err(ProviderMutationError::ambiguous(
            "Cloudflare DNS create result did not match the requested TXT record".to_string(),
        ));
    }
    if record.id.is_empty() || !record.id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(ProviderMutationError::ambiguous(
            "Cloudflare DNS create returned no valid record id".to_string(),
        ));
    }
    Ok(record.id)
}

fn cloudflare_create_send_error(error: reqwest::Error) -> ProviderMutationError {
    let settled_without_mutation = error.is_builder() || error.is_connect();
    let error = format!("Cloudflare DNS create: {error}");
    if settled_without_mutation {
        ProviderMutationError::settled(error)
    } else {
        ProviderMutationError::ambiguous(error)
    }
}

fn cloudflare_create_response_error(
    status: reqwest::StatusCode,
    provider_rejected: bool,
    error: String,
) -> ProviderMutationError {
    if status.is_client_error() || (status.is_success() && provider_rejected) {
        ProviderMutationError::settled(error)
    } else {
        ProviderMutationError::ambiguous(error)
    }
}

async fn cloudflare_delete(zone_id: &str, record_id: &str, token: &str) -> Result<(), String> {
    let response = cloudflare_client()?
        .delete(format!(
            "{CLOUDFLARE_API_BASE}/zones/{zone_id}/dns_records/{record_id}"
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| format!("Cloudflare DNS cleanup: {error}"))?;
    let status = response.status();
    let body = cloudflare_response_body(response, "cleanup response").await?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    let envelope: CloudflareEnvelope<serde_json::Value> = serde_json::from_slice(&body)
        .map_err(|error| format!("parse Cloudflare DNS cleanup response ({status}): {error}"))?;
    if !status.is_success() || !envelope.success {
        return Err(format!(
            "Cloudflare DNS cleanup failed ({status}): {}",
            cloudflare_error_text(&envelope.errors)
        ));
    }
    Ok(())
}

async fn cloudflare_find_exact(
    zone_id: &str,
    token: &str,
    name: &str,
    value: &str,
) -> Result<Vec<String>, String> {
    if zone_id.is_empty() || !zone_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("Cloudflare zone_id is invalid".to_string());
    }
    let response = cloudflare_client()?
        .get(format!("{CLOUDFLARE_API_BASE}/zones/{zone_id}/dns_records"))
        .bearer_auth(token)
        .query(&[
            ("type", "TXT"),
            ("name", name),
            ("per_page", "100"),
            ("page", "1"),
        ])
        .send()
        .await
        .map_err(|error| format!("Cloudflare DNS cleanup lookup: {error}"))?;
    let status = response.status();
    let body = cloudflare_response_body(response, "cleanup lookup response").await?;
    let envelope: CloudflareEnvelope<Vec<CloudflareRecord>> = serde_json::from_slice(&body)
        .map_err(|error| format!("parse Cloudflare DNS cleanup lookup ({status}): {error}"))?;
    if !status.is_success() || !envelope.success {
        return Err(format!(
            "Cloudflare DNS cleanup lookup failed ({status}): {}",
            cloudflare_error_text(&envelope.errors)
        ));
    }
    cloudflare_exact_record_ids(envelope, name, value)
}

fn cloudflare_exact_record_ids(
    envelope: CloudflareEnvelope<Vec<CloudflareRecord>>,
    name: &str,
    value: &str,
) -> Result<Vec<String>, String> {
    let records = envelope
        .result
        .ok_or_else(|| "Cloudflare DNS cleanup lookup omitted result".to_string())?;
    let result_info = envelope
        .result_info
        .ok_or_else(|| "Cloudflare DNS cleanup lookup omitted result_info".to_string())?;
    if result_info.page != 1 || result_info.total_pages > 1 {
        return Err(
            "Cloudflare DNS cleanup lookup did not return one complete bounded page".to_string(),
        );
    }
    Ok(records
        .into_iter()
        .filter(|record| {
            record.record_type == "TXT"
                && record.name.trim_end_matches('.').eq_ignore_ascii_case(name)
                && record.content == value
                && !record.id.is_empty()
                && record.id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(|record| record.id)
        .collect())
}

fn cloudflare_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DNS_PROVIDER_TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("build Cloudflare DNS client: {error}"))
}

async fn cloudflare_response_body(
    response: reqwest::Response,
    context: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > CLOUDFLARE_RESPONSE_MAX_BYTES as u64)
    {
        return Err(format!("Cloudflare DNS {context} exceeds the size cap"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("read Cloudflare DNS {context}: {error}"))?;
        append_cloudflare_response_chunk(&mut body, &chunk, context)?;
    }
    Ok(body)
}

fn append_cloudflare_response_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    context: &str,
) -> Result<(), String> {
    if body
        .len()
        .checked_add(chunk.len())
        .is_none_or(|length| length > CLOUDFLARE_RESPONSE_MAX_BYTES)
    {
        return Err(format!("Cloudflare DNS {context} exceeds the size cap"));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn cloudflare_error_text(errors: &[CloudflareError]) -> String {
    if errors.is_empty() {
        return "provider returned no detail".to_string();
    }
    errors
        .iter()
        .map(|error| format!("{} {}", error.code, error.message.trim()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn decode_tsig_secret(secret: &str) -> Result<Vec<u8>, String> {
    let secret = secret.trim();
    base64::engine::general_purpose::STANDARD
        .decode(secret)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(secret))
        .map_err(|error| format!("RFC2136 TSIG secret is not base64: {error}"))
        .and_then(|key| {
            if key.len() < 16 {
                Err("RFC2136 TSIG secret must decode to at least 16 bytes".to_string())
            } else {
                Ok(key)
            }
        })
}

#[allow(clippy::too_many_arguments)]
async fn rfc2136_update(
    server: &str,
    zone: &str,
    key_name: &str,
    key: &[u8],
    record_name: &str,
    value: &str,
    ttl_secs: u32,
    delete: bool,
) -> Result<(), ProviderMutationError> {
    let zone_name = absolute_name(zone, "RFC2136 zone").map_err(ProviderMutationError::settled)?;
    let record_name = absolute_name(record_name, "RFC2136 record name")
        .map_err(ProviderMutationError::settled)?;
    if !zone_name.zone_of(&record_name) {
        return Err(ProviderMutationError::settled(
            "RFC2136 challenge name is outside the configured zone".to_string(),
        ));
    }
    let key_name =
        absolute_name(key_name, "RFC2136 TSIG key name").map_err(ProviderMutationError::settled)?;
    let record = Record::from_rdata(
        record_name,
        ttl_secs.max(1),
        RData::TXT(TXT::new(vec![value.to_string()])),
    );
    let rrset = RecordSet::from(record);
    let mut message = if delete {
        update_message::delete_by_rdata(rrset, zone_name, true)
    } else {
        update_message::append(rrset, zone_name, false, true)
    };
    let signer = TSigner::new(key.to_vec(), TsigAlgorithm::HmacSha256, key_name, 300)
        .map_err(|error| ProviderMutationError::settled(format!("RFC2136 TSIG signer: {error}")))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            ProviderMutationError::settled(format!("system clock before unix epoch: {error}"))
        })?
        .as_secs();
    let mut verifier = message
        .finalize(&signer, now)
        .map_err(|error| ProviderMutationError::settled(format!("sign RFC2136 update: {error}")))?
        .ok_or_else(|| {
            ProviderMutationError::settled("RFC2136 update produced no TSIG verifier".to_string())
        })?;
    let wire = message.to_bytes().map_err(|error| {
        ProviderMutationError::settled(format!("encode RFC2136 update: {error}"))
    })?;
    if wire.len() > u16::MAX as usize {
        return Err(ProviderMutationError::settled(
            "RFC2136 update exceeds the TCP DNS message limit".to_string(),
        ));
    }
    let request_may_reach_provider = std::sync::atomic::AtomicBool::new(false);
    let result = tokio::time::timeout(DNS_PROVIDER_TIMEOUT, async {
        let mut stream = tokio::net::TcpStream::connect(server)
            .await
            .map_err(|error| {
                ProviderMutationError::settled(format!("connect RFC2136 server {server}: {error}"))
            })?;
        // From the first write onward an I/O error cannot prove whether the
        // complete request reached the authoritative server.
        request_may_reach_provider.store(true, std::sync::atomic::Ordering::Relaxed);
        stream.write_u16(wire.len() as u16).await.map_err(|error| {
            ProviderMutationError::ambiguous(format!("write RFC2136 update length: {error}"))
        })?;
        stream.write_all(&wire).await.map_err(|error| {
            ProviderMutationError::ambiguous(format!("write RFC2136 update: {error}"))
        })?;
        let response_len = stream.read_u16().await.map_err(|error| {
            ProviderMutationError::ambiguous(format!("read RFC2136 response length: {error}"))
        })? as usize;
        if response_len == 0 || response_len > RFC2136_RESPONSE_MAX_BYTES {
            return Err(ProviderMutationError::ambiguous(
                "RFC2136 response length is invalid".to_string(),
            ));
        }
        let mut response = vec![0u8; response_len];
        stream.read_exact(&mut response).await.map_err(|error| {
            ProviderMutationError::ambiguous(format!("read RFC2136 response: {error}"))
        })?;
        let response = verifier.verify(&response).map_err(|error| {
            ProviderMutationError::ambiguous(format!("verify RFC2136 TSIG response: {error}"))
        })?;
        rfc2136_response_result(response.response_code)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) if request_may_reach_provider.load(std::sync::atomic::Ordering::Relaxed) => Err(
            ProviderMutationError::ambiguous("RFC2136 update timed out".to_string()),
        ),
        Err(_) => Err(ProviderMutationError::settled(
            "RFC2136 update timed out before connecting to the provider".to_string(),
        )),
    }
}

fn rfc2136_response_result(response_code: ResponseCode) -> Result<(), ProviderMutationError> {
    if response_code == ResponseCode::NoError {
        Ok(())
    } else {
        Err(ProviderMutationError::settled(format!(
            "RFC2136 update returned {response_code}"
        )))
    }
}

fn absolute_name(value: &str, field: &str) -> Result<Name, String> {
    let value = value.trim().trim_end_matches('.');
    if value.is_empty() {
        return Err(format!("{field} is empty"));
    }
    Name::from_ascii(format!("{value}.")).map_err(|error| format!("{field} is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creating_cloudflare_challenge(id: &str, owner_token: &str) -> PendingChallenge {
        PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: id.to_string(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "reserved-challenge-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(owner_token.to_string()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        }
    }

    #[test]
    fn definitive_provider_rejections_are_settled_without_uncertainty() {
        assert!(matches!(
            cloudflare_create_response_error(
                reqwest::StatusCode::UNAUTHORIZED,
                false,
                "unauthorized".to_string(),
            ),
            ProviderMutationError::SettledWithoutMutation(_)
        ));
        assert!(matches!(
            cloudflare_create_response_error(
                reqwest::StatusCode::OK,
                true,
                "provider rejected the request".to_string(),
            ),
            ProviderMutationError::SettledWithoutMutation(_)
        ));
        assert!(matches!(
            rfc2136_response_result(ResponseCode::Refused),
            Err(ProviderMutationError::SettledWithoutMutation(_))
        ));
        assert!(rfc2136_response_result(ResponseCode::NoError).is_ok());
    }

    #[test]
    fn indeterminate_provider_results_remain_ambiguous() {
        assert!(matches!(
            cloudflare_create_response_error(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                true,
                "provider response was not conclusive".to_string(),
            ),
            ProviderMutationError::ResultAmbiguous(_)
        ));
        assert!(matches!(
            cloudflare_create_response_error(
                reqwest::StatusCode::OK,
                false,
                "successful response omitted the mutation id".to_string(),
            ),
            ProviderMutationError::ResultAmbiguous(_)
        ));
    }

    #[test]
    fn cloudflare_create_id_is_trusted_only_for_the_requested_txt_record() {
        let record = |record_type: &str, name: &str, content: &str| CloudflareRecord {
            id: "record123".to_string(),
            name: name.to_string(),
            content: content.to_string(),
            record_type: record_type.to_string(),
        };
        let requested_name = "_acme-challenge.box.example.test";
        let requested_value = "challenge-value";
        let trusted = match cloudflare_created_record_id(
            Some(record(
                "TXT",
                "_ACME-CHALLENGE.BOX.EXAMPLE.TEST.",
                requested_value,
            )),
            requested_name,
            requested_value,
        ) {
            Ok(record_id) => record_id,
            Err(_) => panic!("matching Cloudflare create result must be trusted"),
        };
        assert_eq!(trusted, "record123");
        for mismatched in [
            record("A", requested_name, requested_value),
            record("TXT", "_acme-challenge.other.example.test", requested_value),
            record("TXT", requested_name, "other-challenge-value"),
        ] {
            assert!(matches!(
                cloudflare_created_record_id(Some(mismatched), requested_name, requested_value,),
                Err(ProviderMutationError::ResultAmbiguous(_))
            ));
        }
    }

    #[test]
    fn incomplete_preprovider_journaling_is_retired_before_retry() {
        let dir = tempfile::tempdir().unwrap();
        let pending = creating_cloudflare_challenge(
            "flow-preprovider-write",
            "45454545454545454545454545454545",
        );
        FAIL_NEXT_PENDING_CHALLENGE_WRITE.with(|fail| fail.set(true));
        assert!(begin_pending_challenge(dir.path(), &pending)
            .unwrap_err()
            .contains("injected pending DNS challenge write failure"));
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        let reserved = load_late_mutation_store(dir.path()).unwrap();
        assert!(reserved.entries.is_empty());

        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert_eq!(
            load_pending_challenge(dir.path()).unwrap().unwrap().phase,
            PendingChallengePhase::Creating
        );
        let reserved = load_late_mutation_store(dir.path()).unwrap();
        assert_eq!(reserved.entries.len(), 1);
        assert_eq!(reserved.entries[0].phase, PendingChallengePhase::Creating);
    }

    #[test]
    fn partial_transmission_marker_remains_provably_unstarted() {
        let dir = tempfile::tempdir().unwrap();
        let pending =
            creating_cloudflare_challenge("flow-partial-start", "46464646464646464646464646464646");
        crate::access::authority_store::with_lock(dir.path(), || {
            let mut reserved = pending.clone();
            reserved.phase = PendingChallengePhase::Reserved;
            reserve_late_mutation_locked(dir.path(), reserved)?;
            // This is the last crash point before the secondary commit marker:
            // the primary has advanced, but the provider call is not eligible.
            write_pending_challenge_locked(dir.path(), &pending)
        })
        .unwrap();
        assert_eq!(
            load_pending_challenge(dir.path()).unwrap().unwrap().phase,
            PendingChallengePhase::Creating
        );
        assert_eq!(
            load_late_mutation_store(dir.path()).unwrap().entries[0].phase,
            PendingChallengePhase::Reserved
        );

        retire_unstarted_challenges(dir.path()).unwrap();
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn postcommit_transmission_marker_error_retires_known_unstarted_pair() {
        let dir = tempfile::tempdir().unwrap();
        let pending = creating_cloudflare_challenge(
            "flow-postcommit-marker-error",
            "49494949494949494949494949494949",
        );
        // The reservation is the first late-store write and the transmission
        // marker is the second. Report an error only after the latter has
        // reached durable storage.
        FAIL_AFTER_LATE_MUTATION_WRITE_COUNTDOWN.with(|countdown| countdown.set(2));
        assert!(begin_pending_challenge(dir.path(), &pending)
            .unwrap_err()
            .contains("injected post-commit late DNS cleanup write failure"));
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn postcommit_scrub_refresh_error_retires_known_unstarted_pair() {
        let dir = tempfile::tempdir().unwrap();
        let pending = creating_cloudflare_challenge(
            "flow-postcommit-scrub-error",
            "50505050505050505050505050505050",
        );
        FAIL_NEXT_PENDING_CREDENTIAL_SCRUB_REFRESH.with(|fail| fail.set(true));
        assert!(begin_pending_challenge(dir.path(), &pending)
            .unwrap_err()
            .contains("injected pending DNS credential child-scrub refresh failure"));
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn completed_transmission_marker_preserves_uncertainty_authority() {
        let dir = tempfile::tempdir().unwrap();
        let pending = creating_cloudflare_challenge(
            "flow-complete-start",
            "47474747474747474747474747474747",
        );
        begin_pending_challenge(dir.path(), &pending).unwrap();
        retire_unstarted_challenges(dir.path()).unwrap();
        assert_eq!(
            load_pending_challenge(dir.path()).unwrap().unwrap().phase,
            PendingChallengePhase::Creating
        );
        assert_eq!(
            load_late_mutation_store(dir.path()).unwrap().entries[0].phase,
            PendingChallengePhase::Creating
        );
    }

    fn expire_primary_mutation_uncertainty(cert_dir: &Path) {
        crate::access::authority_store::with_lock(cert_dir, || {
            let mut pending = load_pending_challenge_locked(cert_dir)
                .map_err(crate::access::AccessError)?
                .expect("pending DNS challenge");
            pending.lease_expires_unix_ms = now_unix_ms().saturating_sub(1);
            write_pending_challenge_locked(cert_dir, &pending)
        })
        .unwrap();
    }

    fn expire_late_mutation_uncertainty(cert_dir: &Path) {
        crate::access::authority_store::with_lock(cert_dir, || {
            let mut store =
                load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
            store
                .entries
                .first_mut()
                .expect("late DNS cleanup")
                .lease_expires_unix_ms = now_unix_ms().saturating_sub(1);
            write_late_mutation_store_locked(cert_dir, &store)
        })
        .unwrap();
    }

    #[test]
    fn tsig_secret_requires_real_base64_key_material() {
        assert!(decode_tsig_secret("not-base64").is_err());
        assert!(decode_tsig_secret("AQID").is_err());
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(decode_tsig_secret(&encoded).unwrap(), vec![7u8; 32]);
    }

    #[test]
    fn challenge_name_must_stay_inside_rfc2136_zone() {
        let zone = absolute_name("example.test", "zone").unwrap();
        assert!(zone.zone_of(&absolute_name("_acme-challenge.box.example.test", "record").unwrap()));
        assert!(!zone.zone_of(&absolute_name("_acme-challenge.other.test", "record").unwrap()));
    }

    #[test]
    fn configured_dns_secret_names_remain_runtime_scrubbable() {
        assert_eq!(
            crate::credential_leases::dns_credential_env_name(
                Some("OWNER_DNS_API_TOKEN"),
                "CLOUDFLARE_API_TOKEN",
                "_API_TOKEN",
            )
            .unwrap(),
            "OWNER_DNS_API_TOKEN"
        );
        assert!(crate::credential_leases::dns_credential_env_name(
            Some("INTENDANT_OTHER_SECRET"),
            "INTENDANT_RFC2136_TSIG_SECRET",
            "_TSIG_SECRET",
        )
        .is_err());
        assert!(crate::credential_leases::dns_credential_env_name(
            Some("OWNER_DNS_PASSWORD"),
            "INTENDANT_RFC2136_TSIG_SECRET",
            "_TSIG_SECRET",
        )
        .is_err());
    }

    #[test]
    fn invalid_scrub_metadata_is_rejected_before_journaling() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: "flow-invalid-scrub".to_string(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "challenge-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("NOT_A_DNS_CREDENTIAL".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some("44444444444444444444444444444444".to_string()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        };
        assert!(begin_pending_challenge(dir.path(), &pending).is_err());
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn cloudflare_response_cap_is_enforced_while_streaming() {
        let mut body = vec![0; CLOUDFLARE_RESPONSE_MAX_BYTES - 2];
        append_cloudflare_response_chunk(&mut body, &[1, 2], "response").unwrap();
        let before = body.len();
        let error = append_cloudflare_response_chunk(&mut body, &[3], "response").unwrap_err();
        assert!(error.contains("size cap"), "{error}");
        assert_eq!(body.len(), before, "the over-cap chunk is never retained");
    }

    #[test]
    fn cloudflare_cleanup_lookup_requires_complete_result_metadata() {
        let parse = |value: serde_json::Value| {
            serde_json::from_value::<CloudflareEnvelope<Vec<CloudflareRecord>>>(value)
        };
        let exact = serde_json::json!({
            "success": true,
            "errors": [],
            "result": [{
                "id": "record123",
                "name": "_acme-challenge.box.example.test",
                "content": "challenge-value",
                "type": "TXT"
            }],
            "result_info": {
                "page": 1,
                "total_pages": 1
            }
        });
        assert_eq!(
            cloudflare_exact_record_ids(
                parse(exact.clone()).unwrap(),
                "_acme-challenge.box.example.test",
                "challenge-value",
            )
            .unwrap(),
            vec!["record123".to_string()]
        );

        let mut missing_result = exact.clone();
        missing_result.as_object_mut().unwrap().remove("result");
        assert!(cloudflare_exact_record_ids(
            parse(missing_result).unwrap(),
            "_acme-challenge.box.example.test",
            "challenge-value",
        )
        .unwrap_err()
        .contains("omitted result"));

        let mut missing_info = exact.clone();
        missing_info.as_object_mut().unwrap().remove("result_info");
        assert!(cloudflare_exact_record_ids(
            parse(missing_info).unwrap(),
            "_acme-challenge.box.example.test",
            "challenge-value",
        )
        .unwrap_err()
        .contains("omitted result_info"));

        let mut incomplete = exact.clone();
        incomplete["result_info"]["total_pages"] = serde_json::json!(2);
        assert!(cloudflare_exact_record_ids(
            parse(incomplete).unwrap(),
            "_acme-challenge.box.example.test",
            "challenge-value",
        )
        .unwrap_err()
        .contains("complete bounded page"));

        let mut missing_total_pages = exact;
        missing_total_pages["result_info"]
            .as_object_mut()
            .unwrap()
            .remove("total_pages");
        assert!(parse(missing_total_pages).is_err());
    }

    #[test]
    fn pending_challenge_journal_survives_until_exact_cleanup_completes() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DnsChallengeLease {
            id: "flow-one".to_string(),
            owner_token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: "flow-one".to_string(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "challenge-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(lease.owner_token.clone()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert!(begin_pending_challenge(dir.path(), &pending)
            .unwrap_err()
            .contains("must be cleaned up"));
        assert_eq!(
            record_challenge_mutation_complete(
                dir.path(),
                &pending,
                &lease,
                Some("record123".to_string())
            )
            .unwrap(),
            ChallengeMutationCompletion::Active
        );
        let restored = load_pending_challenge(dir.path()).unwrap().unwrap();
        assert_eq!(restored.cloudflare_record_id.as_deref(), Some("record123"));
        assert_eq!(restored.phase, PendingChallengePhase::Active);
        assert!(
            claim_pending_cleanup(dir.path()).unwrap().is_none(),
            "a sibling cleanup worker cannot claim a live challenge"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(pending_challenge_path(dir.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        mark_pending_challenge_cleanup(dir.path(), &lease).unwrap();
        let (claimed, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
        assert_eq!(claimed.id, pending.id);
        remove_pending_challenge(dir.path(), &claimed, &cleanup_token).unwrap();
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
    }

    #[test]
    fn child_spawn_refreshes_a_journal_created_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: "flow-cross-process".to_string(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "challenge-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OTHER_PROCESS_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Active,
            owner_token: Some("cccccccccccccccccccccccccccccccc".to_string()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_ACTIVE_LEASE_MS),
            mutation_complete: true,
        };
        crate::credential_leases::configure_pending_dns_credential_child_scrub(
            dir.path(),
            Ok(None),
        );
        crate::access::authority_store::with_lock(dir.path(), || {
            write_pending_challenge_locked(dir.path(), &pending)
        })
        .unwrap();

        let mut command = tokio::process::Command::new(dir.path().join("missing-supervised-child"));
        command.env("OTHER_PROCESS_DNS_API_TOKEN", "must not be inherited");
        assert!(crate::credential_leases::spawn_with_dns_credential_scrub(
            &mut command,
            None,
            Some(dir.path()),
        )
        .is_err());
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| { *name == std::ffi::OsStr::new("OTHER_PROCESS_DNS_API_TOKEN") })
                .and_then(|(_, value)| value),
            None
        );

        std::fs::remove_file(pending_challenge_path(dir.path())).unwrap();
        refresh_pending_credential_child_scrub(dir.path()).unwrap();
    }

    #[test]
    fn stale_creation_waits_for_mutation_uncertainty_before_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: "flow-stale".to_string(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "challenge-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
            lease_expires_unix_ms: now_unix_ms()
                .saturating_sub(PENDING_CHALLENGE_STALE_GRACE_MS + 1),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert!(claim_pending_cleanup(dir.path()).unwrap().is_none());
        let waiting = load_pending_challenge(dir.path()).unwrap().unwrap();
        assert_eq!(waiting.phase, PendingChallengePhase::Cleanup);
        assert!(!waiting.mutation_complete);
        assert!(waiting.lease_expires_unix_ms > now_unix_ms());

        expire_primary_mutation_uncertainty(dir.path());
        let (claimed, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
        assert_eq!(claimed.phase, PendingChallengePhase::Cleanup);
        assert!(claimed.mutation_complete);
        remove_pending_challenge(dir.path(), &claimed, &cleanup_token).unwrap();

        assert!(claim_late_mutation_cleanup(dir.path()).unwrap().is_none());
        expire_late_mutation_uncertainty(dir.path());
        let (claimed, cleanup_token) = claim_late_mutation_cleanup(dir.path()).unwrap().unwrap();
        assert!(claimed.mutation_complete);
        assert!(finish_late_mutation_cleanup(dir.path(), &claimed, &cleanup_token).unwrap());
    }

    #[test]
    fn ambiguous_provider_failure_waits_before_authoritative_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DnsChallengeLease {
            id: "flow-ambiguous".to_string(),
            owner_token: "abababababababababababababababab".to_string(),
        };
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: lease.id.clone(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "ambiguous-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(lease.owner_token.clone()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert_eq!(
            record_challenge_mutation_complete_with_mode(
                dir.path(),
                &pending,
                &lease,
                None,
                false,
                false,
            )
            .unwrap(),
            ChallengeMutationCompletion::CleanupRequired
        );

        let primary = load_pending_challenge(dir.path()).unwrap().unwrap();
        assert_eq!(primary.phase, PendingChallengePhase::Cleanup);
        assert!(!primary.mutation_complete);
        assert!(primary.lease_expires_unix_ms > now_unix_ms());
        let reserved = load_late_mutation_store_locked(dir.path()).unwrap();
        assert_eq!(reserved.entries.len(), 1);
        assert!(!reserved.entries[0].mutation_complete);
        assert!(reserved.entries[0].lease_expires_unix_ms > now_unix_ms());
        assert!(claim_pending_cleanup(dir.path()).unwrap().is_none());
        assert!(claim_late_mutation_cleanup(dir.path()).unwrap().is_none());

        expire_primary_mutation_uncertainty(dir.path());
        let (claimed, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
        assert!(claimed.mutation_complete);
        remove_pending_challenge(dir.path(), &claimed, &cleanup_token).unwrap();

        expire_late_mutation_uncertainty(dir.path());
        let (claimed, cleanup_token) = claim_late_mutation_cleanup(dir.path()).unwrap().unwrap();
        assert!(claimed.mutation_complete);
        assert!(finish_late_mutation_cleanup(dir.path(), &claimed, &cleanup_token).unwrap());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn settled_failure_retires_without_provider_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DnsChallengeLease {
            id: "flow-local-failure".to_string(),
            owner_token: "acacacacacacacacacacacacacacacac".to_string(),
        };
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: lease.id.clone(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "local-failure-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(lease.owner_token.clone()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert_eq!(
            record_challenge_mutation_complete_with_mode(
                dir.path(),
                &pending,
                &lease,
                None,
                false,
                true,
            )
            .unwrap(),
            ChallengeMutationCompletion::CleanupRequired
        );
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
        let mut resolved = pending;
        resolved.phase = PendingChallengePhase::Cleanup;
        resolved.owner_token = None;
        resolved.lease_expires_unix_ms = 0;
        resolved.mutation_complete = true;
        retire_mutation_after_direct_resolution(dir.path(), &resolved).unwrap();
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
    }

    #[test]
    fn late_provider_mutation_recreates_the_cleanup_obligation_after_stale_reap() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DnsChallengeLease {
            id: "flow-late".to_string(),
            owner_token: "dddddddddddddddddddddddddddddddd".to_string(),
        };
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: lease.id.clone(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "late-challenge-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(lease.owner_token.clone()),
            lease_expires_unix_ms: now_unix_ms()
                .saturating_sub(PENDING_CHALLENGE_STALE_GRACE_MS + 1),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert!(claim_pending_cleanup(dir.path()).unwrap().is_none());
        expire_primary_mutation_uncertainty(dir.path());
        let (stale, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
        assert!(stale.mutation_complete);
        remove_pending_challenge(dir.path(), &stale, &cleanup_token).unwrap();
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());

        assert_eq!(
            record_challenge_mutation_complete(
                dir.path(),
                &pending,
                &lease,
                Some("lateRecord123".to_string())
            )
            .unwrap(),
            ChallengeMutationCompletion::CleanupRequired
        );
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        let backlog = load_late_mutation_store(dir.path()).unwrap();
        assert_eq!(backlog.entries.len(), 1);
        let restored = &backlog.entries[0];
        assert_eq!(restored.phase, PendingChallengePhase::Cleanup);
        assert!(restored.mutation_complete);
        assert_eq!(
            restored.cloudflare_record_id.as_deref(),
            Some("lateRecord123")
        );
        assert!(
            begin_pending_challenge(dir.path(), &pending).is_err(),
            "a new challenge stays blocked until the late mutation is exactly deleted"
        );
    }

    #[test]
    fn late_provider_mutation_is_backlogged_beside_a_newer_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DnsChallengeLease {
            id: "flow-old".to_string(),
            owner_token: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string(),
        };
        let old = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: lease.id.clone(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "old-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(lease.owner_token.clone()),
            lease_expires_unix_ms: now_unix_ms()
                .saturating_sub(PENDING_CHALLENGE_STALE_GRACE_MS + 1),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &old).unwrap();
        assert!(claim_pending_cleanup(dir.path()).unwrap().is_none());
        expire_primary_mutation_uncertainty(dir.path());
        let (stale, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
        assert!(stale.mutation_complete);
        remove_pending_challenge(dir.path(), &stale, &cleanup_token).unwrap();

        let mut replacement = old.clone();
        replacement.id = "flow-new".to_string();
        replacement.value = "new-value".to_string();
        replacement.owner_token = Some("ffffffffffffffffffffffffffffffff".to_string());
        replacement.lease_expires_unix_ms =
            now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS);
        // Simulate a newer primary written by a pre-reservation generation.
        // New code blocks this state until the durable reservation is retired.
        crate::access::authority_store::with_lock(dir.path(), || {
            write_pending_challenge_locked(dir.path(), &replacement)
        })
        .unwrap();

        assert_eq!(
            record_challenge_mutation_complete(
                dir.path(),
                &old,
                &lease,
                Some("oldRecord123".to_string())
            )
            .unwrap(),
            ChallengeMutationCompletion::CleanupRequired
        );
        assert_eq!(
            load_pending_challenge(dir.path()).unwrap().unwrap().id,
            replacement.id
        );
        let backlog = load_late_mutation_store(dir.path()).unwrap();
        assert_eq!(backlog.entries.len(), 1);
        assert_eq!(backlog.entries[0].id, old.id);
        assert_eq!(
            backlog.entries[0].cloudflare_record_id.as_deref(),
            Some("oldRecord123")
        );
        let mut next = replacement.clone();
        next.id = "flow-next".to_string();
        assert!(
            begin_pending_challenge(dir.path(), &next).is_err(),
            "the late-cleanup backlog prevents a third challenge from exhausting its bound"
        );
    }

    #[test]
    fn pre_call_reservation_survives_completion_write_and_cleanup_failures() {
        let dir = tempfile::tempdir().unwrap();
        let lease = DnsChallengeLease {
            id: "flow-reserved".to_string(),
            owner_token: "11111111111111111111111111111111".to_string(),
        };
        let old = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: lease.id.clone(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "reserved-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some(lease.owner_token.clone()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &old).unwrap();
        let mut replacement = old.clone();
        replacement.id = "flow-replacement".to_string();
        replacement.value = "replacement-value".to_string();
        replacement.owner_token = Some("22222222222222222222222222222222".to_string());
        crate::access::authority_store::with_lock(dir.path(), || {
            write_pending_challenge_locked(dir.path(), &replacement)
        })
        .unwrap();

        FAIL_NEXT_LATE_MUTATION_WRITE.with(|fail| fail.set(true));
        let error = record_challenge_mutation_complete(
            dir.path(),
            &old,
            &lease,
            Some("reservedRecord123".to_string()),
        )
        .unwrap_err();
        assert!(error.contains("injected late DNS cleanup write failure"));
        let reserved = load_late_mutation_store(dir.path()).unwrap();
        assert_eq!(reserved.entries.len(), 1);
        assert_eq!(reserved.entries[0].phase, PendingChallengePhase::Creating);
        assert_eq!(reserved.entries[0].record_name, old.record_name);
        assert_eq!(reserved.entries[0].value, old.value);

        crate::access::authority_store::with_lock(dir.path(), || {
            let mut store =
                load_late_mutation_store_locked(dir.path()).map_err(crate::access::AccessError)?;
            store.entries[0].lease_expires_unix_ms =
                now_unix_ms().saturating_sub(PENDING_CHALLENGE_STALE_GRACE_MS + 1);
            write_late_mutation_store_locked(dir.path(), &store)
        })
        .unwrap();
        assert!(
            claim_late_mutation_cleanup(dir.path()).unwrap().is_none(),
            "a stale creator starts the uncertainty window instead of trusting immediate absence"
        );

        let restarted = load_late_mutation_store_locked(dir.path()).unwrap();
        assert_eq!(restarted.entries.len(), 1);
        assert_eq!(restarted.entries[0].phase, PendingChallengePhase::Cleanup);
        assert!(
            !restarted.entries[0].mutation_complete,
            "the in-flight provider mutation remains explicitly uncertain"
        );
        assert_eq!(
            record_challenge_mutation_complete(
                dir.path(),
                &old,
                &lease,
                Some("reservedRecord123".to_string()),
            )
            .unwrap(),
            ChallengeMutationCompletion::CleanupRequired
        );
        let completed = load_late_mutation_store_locked(dir.path()).unwrap();
        assert_eq!(completed.entries.len(), 1);
        assert!(completed.entries[0].mutation_complete);
        assert_eq!(
            completed.entries[0].cloudflare_record_id.as_deref(),
            Some("reservedRecord123")
        );
        let (claimed, token) = claim_late_mutation_cleanup(dir.path()).unwrap().unwrap();
        release_late_mutation_cleanup(dir.path(), &claimed.id, &token).unwrap();
        let (claimed, token) = claim_late_mutation_cleanup(dir.path()).unwrap().unwrap();
        assert!(finish_late_mutation_cleanup(dir.path(), &claimed, &token).unwrap());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
        assert_eq!(
            load_pending_challenge(dir.path()).unwrap().unwrap().id,
            replacement.id
        );
    }

    #[test]
    fn direct_cleanup_retires_reservations_after_transient_journal_failure() {
        let dir = tempfile::tempdir().unwrap();
        let pending = PendingChallenge {
            schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
            id: "flow-direct-retire".to_string(),
            domain: "box.example.test".to_string(),
            record_name: "_acme-challenge.box.example.test".to_string(),
            value: "direct-retire-value".to_string(),
            provider: CustomDomainDnsConfig::Cloudflare {
                zone_id: "abc123".to_string(),
                token_env: Some("OWNER_DNS_API_TOKEN".to_string()),
                propagation_delay_secs: 0,
            },
            cloudflare_record_id: None,
            phase: PendingChallengePhase::Creating,
            owner_token: Some("33333333333333333333333333333333".to_string()),
            lease_expires_unix_ms: now_unix_ms().saturating_add(PENDING_CHALLENGE_CREATE_LEASE_MS),
            mutation_complete: false,
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();

        let mut completed = pending.clone();
        completed.cloudflare_record_id = Some("directRecord123".to_string());
        completed.phase = PendingChallengePhase::Cleanup;
        completed.owner_token = None;
        completed.lease_expires_unix_ms = 0;
        completed.mutation_complete = true;

        FAIL_NEXT_LATE_MUTATION_WRITE.with(|fail| fail.set(true));
        assert!(ensure_late_mutation_cleanup(dir.path(), &completed)
            .unwrap_err()
            .contains("injected late DNS cleanup write failure"));
        assert!(load_pending_challenge(dir.path()).unwrap().is_some());
        assert_eq!(
            load_late_mutation_store(dir.path()).unwrap().entries.len(),
            1
        );

        // This models a successful exact direct delete after the preceding
        // durable-upgrade attempt failed. The second locked reconciliation
        // retires both pre-call reservations instead of blocking renewal.
        retire_mutation_after_direct_resolution(dir.path(), &completed).unwrap();
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
        assert!(load_late_mutation_store(dir.path())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn legacy_journal_gets_an_active_lease_before_cleanup_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            pending_challenge_path(dir.path()),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "id": "flow-legacy",
                "domain": "box.example.test",
                "record_name": "_acme-challenge.box.example.test",
                "value": "legacy-challenge-value",
                "provider": {
                    "provider": "cloudflare",
                    "zone_id": "abc123",
                    "token_env": "OWNER_DNS_API_TOKEN",
                    "propagation_delay_secs": 0
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let loaded = load_pending_challenge(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.phase, PendingChallengePhase::Active);
        assert!(loaded.owner_token.is_none());
        assert!(
            claim_pending_cleanup(dir.path()).unwrap().is_none(),
            "an upgraded process must not immediately remove a possibly live legacy challenge"
        );
    }

    #[test]
    fn malformed_pending_challenge_never_decays_into_an_empty_journal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pending_challenge_path(dir.path()), b"{").unwrap();
        assert!(load_pending_challenge(dir.path())
            .unwrap_err()
            .contains("parse"));

        let mut unsupported =
            creating_cloudflare_challenge("flow-unsupported", "48484848484848484848484848484848");
        unsupported.schema_version = 0;
        std::fs::write(
            pending_challenge_path(dir.path()),
            serde_json::to_vec(&unsupported).unwrap(),
        )
        .unwrap();
        assert!(load_pending_challenge(dir.path())
            .unwrap_err()
            .contains("invalid"));
    }
}
