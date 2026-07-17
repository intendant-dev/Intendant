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
const PENDING_CHALLENGE_SCHEMA_VERSION: u32 = 1;
const PENDING_CHALLENGE_MAX_BYTES: u64 = 64 * 1024;
const PENDING_CHALLENGE_RETRY_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

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
}

pub(crate) async fn set_challenge_in(
    config: &CustomDomainDnsConfig,
    domain: &str,
    value: &str,
    cert_dir: &Path,
) -> Result<(), String> {
    let record_name = format!("_acme-challenge.{domain}");
    let pending = PendingChallenge {
        schema_version: PENDING_CHALLENGE_SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().simple().to_string(),
        domain: domain.to_string(),
        record_name: record_name.clone(),
        value: value.to_string(),
        provider: config.clone(),
        cloudflare_record_id: None,
    };
    begin_pending_challenge(cert_dir, &pending)?;
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
            let record_id = cloudflare_create(zone_id.trim(), &token, &record_name, value).await?;
            record_cloudflare_id(cert_dir, &pending.id, record_id)
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
            Ok(())
        }
    }
}

pub(crate) async fn retry_pending_challenge(cert_dir: &Path) -> Result<(), String> {
    let Some(pending) = load_pending_challenge(cert_dir)? else {
        return Ok(());
    };
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
    remove_pending_challenge(cert_dir, &pending.id)
}

pub(crate) fn spawn_cleanup_loop(cert_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = retry_pending_challenge(&cert_dir).await {
                eprintln!("[custom-domain] pending DNS-01 cleanup: {error}");
            }
            tokio::time::sleep(PENDING_CHALLENGE_RETRY_INTERVAL).await;
        }
    });
}

fn pending_challenge_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(PENDING_CHALLENGE_FILE)
}

fn load_pending_challenge_locked(cert_dir: &Path) -> Result<Option<PendingChallenge>, String> {
    use std::io::Read as _;

    let path = pending_challenge_path(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
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
    let pending: PendingChallenge = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if pending.schema_version != PENDING_CHALLENGE_SCHEMA_VERSION
        || pending.id.is_empty()
        || pending.id.len() > 64
        || pending.domain.is_empty()
        || pending.domain.len() > 253
        || pending.record_name != format!("_acme-challenge.{}", pending.domain)
        || pending.value.is_empty()
        || pending.value.len() > 1024
        || pending.value.chars().any(char::is_control)
        || pending
            .cloudflare_record_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    {
        return Err(format!(
            "{} contains invalid pending DNS challenge state",
            path.display()
        ));
    }
    Ok(Some(pending))
}

fn load_pending_challenge(cert_dir: &Path) -> Result<Option<PendingChallenge>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
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
    crate::access::authority_store::with_lock(cert_dir, || {
        if load_pending_challenge_locked(cert_dir)
            .map_err(crate::access::AccessError)?
            .is_some()
        {
            return Err(crate::access::AccessError(
                "a pending custom-domain DNS challenge must be cleaned up before another is created"
                    .to_string(),
            ));
        }
        write_pending_challenge_locked(cert_dir, pending)
    })
    .map_err(|error| error.to_string())
}

fn record_cloudflare_id(
    cert_dir: &Path,
    pending_id: &str,
    record_id: String,
) -> Result<(), String> {
    if record_id.is_empty() || !record_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("Cloudflare DNS create returned an invalid record id".to_string());
    }
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut pending = load_pending_challenge_locked(cert_dir)
            .map_err(crate::access::AccessError)?
            .ok_or_else(|| {
                crate::access::AccessError(
                    "pending DNS challenge disappeared before its record id was saved".to_string(),
                )
            })?;
        if pending.id != pending_id {
            return Err(crate::access::AccessError(
                "pending DNS challenge changed before its record id was saved".to_string(),
            ));
        }
        pending.cloudflare_record_id = Some(record_id);
        write_pending_challenge_locked(cert_dir, &pending)
    })
    .map_err(|error| error.to_string())
}

fn remove_pending_challenge(cert_dir: &Path, pending_id: &str) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let Some(pending) =
            load_pending_challenge_locked(cert_dir).map_err(crate::access::AccessError)?
        else {
            return Ok(());
        };
        if pending.id != pending_id {
            return Err(crate::access::AccessError(
                "pending DNS challenge changed during cleanup".to_string(),
            ));
        }
        crate::access::authority_store::remove_file_locked(&pending_challenge_path(cert_dir))
    })
    .map_err(|error| error.to_string())
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
        };
        begin_pending_challenge(dir.path(), &pending).unwrap();
        assert!(begin_pending_challenge(dir.path(), &pending)
            .unwrap_err()
            .contains("must be cleaned up"));
        record_cloudflare_id(dir.path(), &pending.id, "record123".to_string()).unwrap();
        let restored = load_pending_challenge(dir.path()).unwrap().unwrap();
        assert_eq!(restored.cloudflare_record_id.as_deref(), Some("record123"));
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
        remove_pending_challenge(dir.path(), &pending.id).unwrap();
        assert!(load_pending_challenge(dir.path()).unwrap().is_none());
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
