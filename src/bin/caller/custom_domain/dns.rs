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
const PENDING_CHALLENGE_SCHEMA_VERSION: u32 = 3;
const PENDING_CHALLENGE_MAX_BYTES: u64 = 64 * 1024;
const LATE_MUTATION_FILE: &str = "custom-domain-dns-late-cleanup.json";
const LATE_MUTATION_SCHEMA_VERSION: u32 = 2;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PendingChallengePhase {
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
    /// False only while the provider mutation may still complete after a
    /// stale creator loses this journal. Cleanup workers compare this bit
    /// before removing the durable obligation.
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
    static FAIL_NEXT_LATE_MUTATION_WRITE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[derive(Clone, Debug)]
pub(crate) struct DnsChallengeLease {
    id: String,
    owner_token: String,
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
    let mutation: Result<Option<String>, String> = async {
        match config {
            CustomDomainDnsConfig::Cloudflare {
                zone_id, token_env, ..
            } => {
                let token = provider_secret(
                    "dns:cloudflare",
                    token_env.as_deref(),
                    "CLOUDFLARE_API_TOKEN",
                    "_API_TOKEN",
                )?;
                cloudflare_create(zone_id.trim(), &token, &record_name, value)
                    .await
                    .map(Some)
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
                    &record_name,
                    value,
                    *ttl_secs,
                    false,
                )
                .await?;
                Ok(None)
            }
        }
    }
    .await;
    let (record_id, mutation_error) = match mutation {
        Ok(record_id) => (record_id, None),
        Err(error) => (None, Some(error)),
    };
    let cleanup_record_id = record_id.clone();
    let completion = record_challenge_mutation_complete_with_mode(
        cert_dir,
        &pending,
        &lease,
        record_id,
        mutation_error.is_none(),
    );
    match (completion, mutation_error) {
        (Ok(ChallengeMutationCompletion::Active), None) => Ok(lease),
        (Ok(ChallengeMutationCompletion::Active), Some(error)) => Err(error),
        (Ok(ChallengeMutationCompletion::CleanupRequired), original_error) => {
            let cleanup = match retry_pending_challenge(cert_dir).await {
                Ok(true) => Ok(()),
                Ok(false) => Err("pending DNS challenge cleanup is still leased".to_string()),
                Err(error) => Err(error),
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
            // removed this creator's journal. Exact-delete the completed old
            // mutation directly; its value/id cannot affect the new entry.
            let mut late = pending;
            late.cloudflare_record_id = cleanup_record_id;
            late.phase = PendingChallengePhase::Cleanup;
            late.owner_token = None;
            late.lease_expires_unix_ms = 0;
            late.mutation_complete = true;
            let cleanup = cleanup_pending_challenge(&late).await;
            let operation_error = original_error.unwrap_or(completion_error);
            Err(match cleanup {
                Ok(()) => operation_error,
                Err(cleanup_error) => format!(
                    "{operation_error}; exact cleanup of the late DNS mutation failed: {cleanup_error}"
                ),
            })
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
        remove_late_mutation_cleanup(cert_dir, &pending, &cleanup_token)?;
        load_late_mutation_store(cert_dir)?.entries.is_empty()
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
            .await?;
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
    if loaded_schema < 3 {
        pending.mutation_complete = pending.phase != PendingChallengePhase::Creating;
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
            PendingChallengePhase::Creating | PendingChallengePhase::Active
        ) || pending.owner_token.is_some()
            || legacy_schema)
        && (pending.phase != PendingChallengePhase::Creating || !pending.mutation_complete)
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
    if store.schema_version == 1 {
        store.schema_version = LATE_MUTATION_SCHEMA_VERSION;
    }
    let mut ids = std::collections::HashSet::new();
    if store.schema_version != LATE_MUTATION_SCHEMA_VERSION
        || store.entries.len() > LATE_MUTATION_MAX_ENTRIES
        || store.entries.iter().any(|pending| {
            !matches!(
                (pending.phase, pending.mutation_complete),
                (PendingChallengePhase::Creating, false) | (PendingChallengePhase::Cleanup, true)
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
    if store.entries.is_empty() {
        return crate::access::authority_store::remove_file_locked(&late_mutation_path(cert_dir));
    }
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
}

fn reserve_late_mutation_locked(
    cert_dir: &Path,
    pending: PendingChallenge,
) -> crate::access::AccessResult<()> {
    let mut store =
        load_late_mutation_store_locked(cert_dir).map_err(crate::access::AccessError)?;
    if pending.phase != PendingChallengePhase::Creating || pending.mutation_complete {
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
    let result = crate::access::authority_store::with_lock(cert_dir, || {
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
        // Reserve a durable exact-cleanup slot before the provider call. If
        // the primary creation lease is reaped or replaced while that call is
        // in flight, this reservation remains sufficient to find/delete the
        // exact name+value after restart.
        reserve_late_mutation_locked(cert_dir, pending.clone())?;
        write_pending_challenge_locked(cert_dir, pending)
    });
    let refresh = refresh_pending_credential_child_scrub(cert_dir);
    result.map_err(|error| error.to_string())?;
    refresh
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
    record_challenge_mutation_complete_with_mode(cert_dir, template, lease, record_id, true)
}

fn record_challenge_mutation_complete_with_mode(
    cert_dir: &Path,
    template: &PendingChallenge,
    lease: &DnsChallengeLease,
    record_id: Option<String>,
    activate_if_owned: bool,
) -> Result<ChallengeMutationCompletion, String> {
    if record_id.as_ref().is_some_and(|record_id| {
        record_id.is_empty() || !record_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err("Cloudflare DNS create returned an invalid record id".to_string());
    }
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
        reservation.lease_expires_unix_ms = 0;
        reservation.mutation_complete = true;
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
        pending.mutation_complete = true;
        let completion = if activate_if_owned && owner_is_current {
            pending.phase = PendingChallengePhase::Active;
            pending.lease_expires_unix_ms =
                now_unix_ms().saturating_add(PENDING_CHALLENGE_ACTIVE_LEASE_MS);
            ChallengeMutationCompletion::Active
        } else {
            pending.phase = PendingChallengePhase::Cleanup;
            if owner_is_current {
                pending.owner_token = None;
                pending.lease_expires_unix_ms = 0;
            }
            ChallengeMutationCompletion::CleanupRequired
        };
        write_pending_challenge_locked(cert_dir, &pending)?;
        // The primary journal now carries the full obligation. Retire the
        // reservation last; if this write fails, duplicate durable cleanup is
        // safe and the caller does not proceed as though creation succeeded.
        late_store
            .entries
            .retain(|reservation| reservation.id != lease.id);
        write_late_mutation_store_locked(cert_dir, &late_store)?;
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
        let Some(pending) = store.entries.iter_mut().find(|pending| {
            let stale = now
                > pending
                    .lease_expires_unix_ms
                    .saturating_add(PENDING_CHALLENGE_STALE_GRACE_MS);
            match pending.phase {
                PendingChallengePhase::Creating => stale,
                PendingChallengePhase::Cleanup => pending.owner_token.is_none() || stale,
                PendingChallengePhase::Active => false,
            }
        }) else {
            return Ok(None);
        };
        if pending.phase == PendingChallengePhase::Creating {
            // A process ended while the provider effect was ambiguous. The
            // pre-call reservation already binds the exact name/value, so it
            // now becomes an ordinary idempotent cleanup entry.
            pending.phase = PendingChallengePhase::Cleanup;
            pending.mutation_complete = true;
        }
        let cleanup_token = uuid::Uuid::new_v4().simple().to_string();
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
            PendingChallengePhase::Creating | PendingChallengePhase::Active => stale,
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
        crate::access::authority_store::remove_file_locked(&pending_challenge_path(cert_dir))
    })
    .map_err(|error| error.to_string())?;
    refresh_pending_credential_child_scrub(cert_dir)
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
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
    #[serde(default)]
    total_pages: u64,
}

async fn cloudflare_create(
    zone_id: &str,
    token: &str,
    name: &str,
    value: &str,
) -> Result<String, String> {
    if zone_id.is_empty() || !zone_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("Cloudflare zone_id is invalid".to_string());
    }
    let response = cloudflare_client()?
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
        .map_err(|error| format!("Cloudflare DNS create: {error}"))?;
    let status = response.status();
    let body = cloudflare_response_body(response, "response").await?;
    let envelope: CloudflareEnvelope<CloudflareRecord> = serde_json::from_slice(&body)
        .map_err(|error| format!("parse Cloudflare DNS response ({status}): {error}"))?;
    if !status.is_success() || !envelope.success {
        return Err(format!(
            "Cloudflare DNS create failed ({status}): {}",
            cloudflare_error_text(&envelope.errors)
        ));
    }
    envelope
        .result
        .map(|record| record.id)
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .ok_or_else(|| "Cloudflare DNS create returned no record id".to_string())
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
    if envelope
        .result_info
        .as_ref()
        .is_some_and(|info| info.total_pages > 1)
    {
        return Err(
            "Cloudflare DNS cleanup lookup returned more than 100 exact-name records".to_string(),
        );
    }
    Ok(envelope
        .result
        .unwrap_or_default()
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
) -> Result<(), String> {
    let zone_name = absolute_name(zone, "RFC2136 zone")?;
    let record_name = absolute_name(record_name, "RFC2136 record name")?;
    if !zone_name.zone_of(&record_name) {
        return Err("RFC2136 challenge name is outside the configured zone".to_string());
    }
    let key_name = absolute_name(key_name, "RFC2136 TSIG key name")?;
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
        .map_err(|error| format!("RFC2136 TSIG signer: {error}"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock before unix epoch: {error}"))?
        .as_secs();
    let mut verifier = message
        .finalize(&signer, now)
        .map_err(|error| format!("sign RFC2136 update: {error}"))?
        .ok_or_else(|| "RFC2136 update produced no TSIG verifier".to_string())?;
    let wire = message
        .to_bytes()
        .map_err(|error| format!("encode RFC2136 update: {error}"))?;
    if wire.len() > u16::MAX as usize {
        return Err("RFC2136 update exceeds the TCP DNS message limit".to_string());
    }
    tokio::time::timeout(DNS_PROVIDER_TIMEOUT, async {
        let mut stream = tokio::net::TcpStream::connect(server)
            .await
            .map_err(|error| format!("connect RFC2136 server {server}: {error}"))?;
        stream
            .write_u16(wire.len() as u16)
            .await
            .map_err(|error| format!("write RFC2136 update length: {error}"))?;
        stream
            .write_all(&wire)
            .await
            .map_err(|error| format!("write RFC2136 update: {error}"))?;
        let response_len = stream
            .read_u16()
            .await
            .map_err(|error| format!("read RFC2136 response length: {error}"))?
            as usize;
        if response_len == 0 || response_len > RFC2136_RESPONSE_MAX_BYTES {
            return Err("RFC2136 response length is invalid".to_string());
        }
        let mut response = vec![0u8; response_len];
        stream
            .read_exact(&mut response)
            .await
            .map_err(|error| format!("read RFC2136 response: {error}"))?;
        let response = verifier
            .verify(&response)
            .map_err(|error| format!("verify RFC2136 TSIG response: {error}"))?;
        if response.response_code != ResponseCode::NoError {
            return Err(format!(
                "RFC2136 update returned {}",
                response.response_code
            ));
        }
        Ok(())
    })
    .await
    .map_err(|_| "RFC2136 update timed out".to_string())?
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
    fn cloudflare_response_cap_is_enforced_while_streaming() {
        let mut body = vec![0; CLOUDFLARE_RESPONSE_MAX_BYTES - 2];
        append_cloudflare_response_chunk(&mut body, &[1, 2], "response").unwrap();
        let before = body.len();
        let error = append_cloudflare_response_chunk(&mut body, &[3], "response").unwrap_err();
        assert!(error.contains("size cap"), "{error}");
        assert_eq!(body.len(), before, "the over-cap chunk is never retained");
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
    fn stale_creation_is_claimable_only_after_its_grace_window() {
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
        let (claimed, _) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
        assert_eq!(claimed.phase, PendingChallengePhase::Cleanup);
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
        let (stale, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
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
        let (stale, cleanup_token) = claim_pending_cleanup(dir.path()).unwrap().unwrap();
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
    fn pre_call_reservation_survives_completion_and_cleanup_failures() {
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
        let (claimed, token) = claim_late_mutation_cleanup(dir.path()).unwrap().unwrap();
        assert_eq!(claimed.record_name, old.record_name);
        assert_eq!(claimed.value, old.value);
        release_late_mutation_cleanup(dir.path(), &claimed.id, &token).unwrap();

        let restarted = load_late_mutation_store_locked(dir.path()).unwrap();
        assert_eq!(restarted.entries.len(), 1);
        assert_eq!(restarted.entries[0].phase, PendingChallengePhase::Cleanup);
        let (claimed, token) = claim_late_mutation_cleanup(dir.path()).unwrap().unwrap();
        remove_late_mutation_cleanup(dir.path(), &claimed, &token).unwrap();
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
    }
}
