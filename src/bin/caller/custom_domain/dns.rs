use base64::Engine as _;
use hickory_proto::op::{update_message, ResponseCode};
use hickory_proto::rr::rdata::{tsig::TsigAlgorithm, TXT};
use hickory_proto::rr::{Name, RData, Record, RecordSet, TSigner};
use hickory_proto::serialize::binary::BinEncodable as _;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::project::CustomDomainDnsConfig;

const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";
const DNS_PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
const CLOUDFLARE_RESPONSE_MAX_BYTES: usize = 256 * 1024;
const RFC2136_RESPONSE_MAX_BYTES: usize = 65_535;

pub(crate) enum ChallengeHandle {
    Cloudflare {
        zone_id: String,
        record_id: String,
        token: String,
    },
    Rfc2136 {
        server: String,
        zone: String,
        key_name: String,
        key: Vec<u8>,
        record_name: String,
        value: String,
        ttl_secs: u32,
    },
}

pub(crate) async fn set_challenge(
    config: &CustomDomainDnsConfig,
    domain: &str,
    value: &str,
) -> Result<ChallengeHandle, String> {
    let record_name = format!("_acme-challenge.{domain}");
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
            Ok(ChallengeHandle::Cloudflare {
                zone_id: zone_id.trim().to_string(),
                record_id,
                token,
            })
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
            Ok(ChallengeHandle::Rfc2136 {
                server: server.trim().to_string(),
                zone: zone.trim().to_string(),
                key_name: key_name.trim().to_string(),
                key,
                record_name,
                value: value.to_string(),
                ttl_secs: *ttl_secs,
            })
        }
    }
}

pub(crate) async fn clear_challenge(handle: ChallengeHandle) -> Result<(), String> {
    match handle {
        ChallengeHandle::Cloudflare {
            zone_id,
            record_id,
            token,
        } => cloudflare_delete(&zone_id, &record_id, &token).await,
        ChallengeHandle::Rfc2136 {
            server,
            zone,
            key_name,
            key,
            record_name,
            value,
            ttl_secs,
        } => {
            rfc2136_update(
                &server,
                &zone,
                &key_name,
                &key,
                &record_name,
                &value,
                ttl_secs,
                true,
            )
            .await
        }
    }
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
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("read Cloudflare DNS response: {error}"))?;
    if body.len() > CLOUDFLARE_RESPONSE_MAX_BYTES {
        return Err("Cloudflare DNS response exceeds the size cap".to_string());
    }
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
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("read Cloudflare DNS cleanup response: {error}"))?;
    if body.len() > CLOUDFLARE_RESPONSE_MAX_BYTES {
        return Err("Cloudflare DNS cleanup response exceeds the size cap".to_string());
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

fn cloudflare_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DNS_PROVIDER_TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("build Cloudflare DNS client: {error}"))
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
}
