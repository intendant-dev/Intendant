//! Fleet certificates: a real, browser-trusted certificate for this
//! daemon's fleet name (docs/src/trust-tiers.md; the convenient direct
//! path). The rendezvous serves a delegated DNS subzone and this daemon
//! owns exactly one name under it — `d-<hash>.<zone>`, derived from its
//! Connect daemon id. Flow, all daemon-side:
//!
//! 1. the register response carries the `fleet_dns` hint (zone + name);
//! 2. on request, the daemon publishes its routable addresses for the
//!    name (LAN addresses are the point: public name + real certificate
//!    + private address needs no port forwarding);
//! 3. ACME (Let's Encrypt, DNS-01 via `instant-acme`): the TXT
//!    challenge is published through the rendezvous with a
//!    daemon-signed request — the ACME account key and the certificate
//!    private key never leave this machine;
//! 4. the minted certificate is installed live into the web gateway's
//!    SNI resolver (`web_tls::install_fleet_certificate`) and persisted
//!    beside the access certs; a background loop renews it.
//!
//! Honest limits: certificate names appear in public CT logs (the label
//! is an opaque hash for exactly that reason), and a hostile zone
//! operator could mint certificates for fleet names — enrolled browsers
//! stay safe via key verification, and CT makes mis-issuance public
//! evidence.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The daemon's fleet label: `d-<hex(sha256(daemon_id))[..20]>` —
/// REPLICATES `bin/connect/dns.rs::daemon_label` (the two binaries never
/// link each other); the golden test below twins the service's.
pub fn daemon_fleet_label(daemon_id: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let id = daemon_id.trim();
    if id.is_empty() {
        return None;
    }
    let digest = Sha256::digest(id.as_bytes());
    let hex: String = digest
        .iter()
        .take(10)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(format!("d-{hex}"))
}

#[derive(Clone, Debug, Default)]
pub struct FleetCertStatus {
    /// The delegated zone the rendezvous serves, from the register
    /// response (`None` = rendezvous has no fleet DNS).
    pub zone: Option<String>,
    /// This daemon's fully qualified fleet name.
    pub name: Option<String>,
    /// none | requesting | valid | error
    pub state: String,
    pub not_after_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    /// Addresses last published for the name (what the A/AAAA records say).
    pub addresses: Vec<String>,
    /// Certificate Transparency tripwire (docs/src/trust-tiers.md, first
    /// contact rung two): `unchecked` | `ok` | `alert`. An `alert` means
    /// the public CT logs hold a certificate for this daemon's name that
    /// this daemon never requested — the fleet-zone operator (or a CA)
    /// minted one, which is exactly the betrayal the rung's security
    /// argument says must leave evidence. Reflects the last successful
    /// check; fetch failures land in `ct_last_error` instead.
    pub ct_state: String,
    /// The foreign certificates behind an `alert`: "serial · issuer ·
    /// not_before" summaries.
    pub ct_unknown: Vec<String>,
    pub ct_checked_unix_ms: Option<u64>,
    pub ct_last_error: Option<String>,
}

fn registry() -> &'static Mutex<FleetCertStatus> {
    static STATUS: OnceLock<Mutex<FleetCertStatus>> = OnceLock::new();
    STATUS.get_or_init(|| {
        Mutex::new(FleetCertStatus {
            state: "none".to_string(),
            ct_state: "unchecked".to_string(),
            ..Default::default()
        })
    })
}

pub fn status_snapshot() -> FleetCertStatus {
    registry().lock().expect("fleet cert status poisoned").clone()
}

fn with_status(update: impl FnOnce(&mut FleetCertStatus)) {
    let mut status = registry().lock().expect("fleet cert status poisoned");
    update(&mut status);
}

/// Called from the Connect client when a register response carries the
/// fleet_dns hint. Also loads any existing on-disk certificate state the
/// first time the name is learned.
pub fn note_fleet_dns(zone: Option<String>, name: Option<String>) {
    let newly_named = {
        let mut status = registry().lock().expect("fleet cert status poisoned");
        let newly_named = name.is_some() && status.name != name;
        status.zone = zone;
        status.name = name;
        newly_named
    };
    if newly_named {
        refresh_installed_state();
    }
}

fn cert_dir() -> PathBuf {
    crate::access::backend::select_backend().cert_dir()
}

pub(crate) fn cert_path() -> PathBuf {
    cert_dir().join("fleet-cert.pem")
}

pub(crate) fn key_path() -> PathBuf {
    cert_dir().join("fleet-key.pem")
}

fn acme_account_path() -> PathBuf {
    cert_dir().join("acme-account.json")
}

/// Certificate expiry (`not_after`, unix ms) from a PEM chain's leaf.
fn cert_not_after_unix_ms(cert_pem: &str) -> Option<u64> {
    let leaf = pem_certificates(cert_pem).into_iter().next()?;
    let (_, parsed) = x509_parser::parse_x509_certificate(&leaf).ok()?;
    let seconds = parsed.validity().not_after.timestamp();
    (seconds > 0).then(|| seconds as u64 * 1000)
}

fn pem_certificates(pem: &str) -> Vec<Vec<u8>> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
        .filter_map(|item| item.ok())
        .map(|der| der.as_ref().to_vec())
        .collect()
}

/// Load on-disk certificate state into the registry and the live TLS
/// resolver (startup + after learning the name). Quietly does nothing
/// when no certificate exists yet.
pub fn refresh_installed_state() {
    let (Ok(cert_pem), Ok(key_pem)) = (
        std::fs::read_to_string(cert_path()),
        std::fs::read_to_string(key_path()),
    ) else {
        return;
    };
    let Some(name) = status_snapshot().name else {
        // Cert on disk but no name learned yet: install once the
        // register response names us (note_fleet_dns re-runs this).
        return;
    };
    let not_after = cert_not_after_unix_ms(&cert_pem);
    match crate::web_tls::install_fleet_certificate(&name, &cert_pem, &key_pem) {
        Ok(()) => with_status(|status| {
            status.state = "valid".to_string();
            status.not_after_unix_ms = not_after;
            status.last_error = None;
        }),
        Err(error) => with_status(|status| {
            status.state = "error".to_string();
            status.last_error = Some(format!("install stored certificate: {error}"));
        }),
    }
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

/// The ACME directory: Let's Encrypt production unless overridden (the
/// staging directory for rig runs: `INTENDANT_ACME_DIRECTORY`).
fn acme_directory() -> String {
    std::env::var("INTENDANT_ACME_DIRECTORY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| instant_acme::LetsEncrypt::Production.url().to_string())
}

async fn acme_account() -> Result<instant_acme::Account, String> {
    let path = acme_account_path();
    if let Ok(stored) = std::fs::read_to_string(&path) {
        if let Ok(credentials) = serde_json::from_str::<instant_acme::AccountCredentials>(&stored)
        {
            return instant_acme::Account::builder()
                .map_err(|e| format!("acme http client: {e}"))?
                .from_credentials(credentials)
                .await
                .map_err(|e| format!("restore acme account: {e}"));
        }
    }
    let (account, credentials) = instant_acme::Account::builder()
        .map_err(|e| format!("acme http client: {e}"))?
        .create(
            &instant_acme::NewAccount {
                contact: &[],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            acme_directory(),
            None,
        )
        .await
        .map_err(|e| format!("create acme account: {e}"))?;
    let serialized = serde_json::to_string(&credentials)
        .map_err(|e| format!("serialize acme credentials: {e}"))?;
    write_private(&path, serialized.as_bytes())?;
    Ok(account)
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    Ok(())
}

/// The default addresses to publish: every routable local address (LAN
/// included — that is the point), loopback excluded, capped at 8.
pub fn default_publish_addresses() -> Vec<String> {
    let mut addresses: Vec<String> = intendant_core::net::routable_local_addrs(false)
        .into_iter()
        .map(|ip| ip.to_string())
        .collect();
    addresses.dedup();
    addresses.truncate(8);
    addresses
}

/// One guarded flow at a time — a second request while one runs is a
/// no-op with an honest error.
fn request_in_flight() -> &'static std::sync::atomic::AtomicBool {
    static FLAG: OnceLock<std::sync::atomic::AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| std::sync::atomic::AtomicBool::new(false))
}

/// Publish addresses + run the ACME DNS-01 order + install the result.
/// Slow (Let's Encrypt polling); callers spawn it and watch the status
/// registry.
pub async fn request_certificate(addresses: Vec<String>) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if request_in_flight().swap(true, Ordering::SeqCst) {
        return Err("a certificate request is already running".to_string());
    }
    let result = request_certificate_inner(addresses).await;
    request_in_flight().store(false, Ordering::SeqCst);
    if let Err(error) = &result {
        with_status(|status| {
            status.state = "error".to_string();
            status.last_error = Some(error.clone());
        });
    }
    result
}

async fn request_certificate_inner(addresses: Vec<String>) -> Result<(), String> {
    let snapshot = status_snapshot();
    let name = snapshot.name.clone().ok_or_else(|| {
        "this daemon has no fleet name — enable Connect against a rendezvous with fleet DNS"
            .to_string()
    })?;
    with_status(|status| {
        status.state = "requesting".to_string();
        status.last_error = None;
    });

    // 1. Make the name resolve: publish the addresses (daemon-signed).
    let published = crate::connect_rendezvous::dns_publish_addresses(&addresses).await?;
    with_status(|status| status.addresses = published.clone());

    // 2. The ACME order.
    let account = acme_account().await?;
    let identifiers = [instant_acme::Identifier::Dns(name.clone())];
    let mut order = account
        .new_order(&instant_acme::NewOrder::new(&identifiers))
        .await
        .map_err(|e| format!("acme new order: {e}"))?;

    let mut authorizations = order.authorizations();
    while let Some(result) = authorizations.next().await {
        let mut authz = result.map_err(|e| format!("acme authorization: {e}"))?;
        match authz.status {
            instant_acme::AuthorizationStatus::Pending => {}
            instant_acme::AuthorizationStatus::Valid => continue,
            other => return Err(format!("acme authorization in unexpected state {other:?}")),
        }
        let mut challenge = authz
            .challenge(instant_acme::ChallengeType::Dns01)
            .ok_or_else(|| "acme order offers no dns-01 challenge".to_string())?;
        let txt_value = challenge.key_authorization().dns_value();
        crate::connect_rendezvous::dns_acme_set(&txt_value).await?;
        challenge
            .set_ready()
            .await
            .map_err(|e| format!("acme challenge ready: {e}"))?;
    }
    drop(authorizations);

    let status = order
        .poll_ready(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|e| format!("acme validation: {e}"))?;
    if status != instant_acme::OrderStatus::Ready {
        let _ = crate::connect_rendezvous::dns_acme_clear().await;
        return Err(format!("acme order did not become ready: {status:?}"));
    }
    let private_key_pem = order
        .finalize()
        .await
        .map_err(|e| format!("acme finalize: {e}"))?;
    let cert_chain_pem = order
        .poll_certificate(&instant_acme::RetryPolicy::default())
        .await
        .map_err(|e| format!("acme certificate: {e}"))?;
    // The CT tripwire's own-serial ledger — recorded before install so a
    // crash here can't make this certificate look foreign later.
    record_own_certificate(&cert_chain_pem, &name, &acme_directory());
    // Best-effort challenge cleanup; the TXT self-expires regardless.
    let _ = crate::connect_rendezvous::dns_acme_clear().await;

    // 3. Persist + install live.
    write_private(&key_path(), private_key_pem.as_bytes())?;
    write_private(&cert_path(), cert_chain_pem.as_bytes())?;
    crate::web_tls::install_fleet_certificate(&name, &cert_chain_pem, &private_key_pem)?;
    with_status(|status| {
        status.state = "valid".to_string();
        status.not_after_unix_ms = cert_not_after_unix_ms(&cert_chain_pem);
        status.last_error = None;
    });
    Ok(())
}

/* ── Certificate Transparency tripwire ──
The fleet rung's security argument is "the zone operator can only betray
loudly": a hijack needs a mis-issued certificate, and browsers only
accept CT-logged certificates. This monitor turns that in-principle
evidence into an actual alarm — the daemon records the serials of every
certificate IT obtained and periodically asks the public CT indexes
whether its name carries any it didn't. Advisory by nature (crt.sh is a
best-effort public service); failures are reported, never alarmed. */

#[derive(serde::Serialize, serde::Deserialize)]
struct OwnCertRecord {
    serial_hex: String,
    name: String,
    directory: String,
    issued_unix_ms: u64,
}

fn serials_path() -> PathBuf {
    cert_dir().join("fleet-cert-serials.json")
}

/// Lowercase hex with leading zeros trimmed — both our parsed serials
/// and crt.sh's strings normalize to this before comparison.
fn normalize_serial_hex(serial: &str) -> String {
    let lower = serial.trim().to_ascii_lowercase();
    let trimmed = lower.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The leaf certificate's serial from a PEM chain.
fn cert_serial_hex(cert_pem: &str) -> Option<String> {
    let leaf = pem_certificates(cert_pem).into_iter().next()?;
    let (_, parsed) = x509_parser::parse_x509_certificate(&leaf).ok()?;
    let hex: String = parsed
        .raw_serial()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(normalize_serial_hex(&hex))
}

fn own_serial_records() -> Vec<OwnCertRecord> {
    std::fs::read_to_string(serials_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Record a certificate this daemon obtained — BEFORE install, so a
/// crash between issuance and install can't leave an own-cert looking
/// foreign at the next check.
fn record_own_certificate(cert_pem: &str, name: &str, directory: &str) {
    let Some(serial) = cert_serial_hex(cert_pem) else {
        return;
    };
    let mut records = own_serial_records();
    if records.iter().any(|record| record.serial_hex == serial) {
        return;
    }
    records.push(OwnCertRecord {
        serial_hex: serial,
        name: name.to_string(),
        directory: directory.to_string(),
        issued_unix_ms: now_unix_ms(),
    });
    if let Ok(serialized) = serde_json::to_string_pretty(&records) {
        let _ = write_private(&serials_path(), serialized.as_bytes());
    }
}

#[derive(Debug, PartialEq)]
struct CtEntry {
    serial_hex: String,
    issuer: String,
    not_before: String,
}

/// Parse a crt.sh `output=json` response, deduplicating the
/// precertificate/leaf pairs that share a serial.
fn parse_crt_sh_entries(json_text: &str) -> Result<Vec<CtEntry>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(json_text).map_err(|e| format!("crt.sh response: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for row in rows {
        let serial = row
            .get("serial_number")
            .and_then(|v| v.as_str())
            .map(normalize_serial_hex)
            .unwrap_or_default();
        if serial.is_empty() || !seen.insert(serial.clone()) {
            continue;
        }
        entries.push(CtEntry {
            serial_hex: serial,
            issuer: row
                .get("issuer_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown issuer")
                .to_string(),
            not_before: row
                .get("not_before")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(entries)
}

/// The foreign entries: publicly logged certificates for our name whose
/// serials this daemon never recorded.
fn foreign_entries(logged: Vec<CtEntry>, own_serials: &[String]) -> Vec<CtEntry> {
    logged
        .into_iter()
        .filter(|entry| !own_serials.iter().any(|own| *own == entry.serial_hex))
        .collect()
}

/// One CT check against the public index. Advisory: fetch/parse failures
/// set `ct_last_error` and leave the last successful verdict standing.
pub async fn ct_check_once() {
    let Some(name) = status_snapshot().name else {
        return;
    };
    let own: Vec<String> = own_serial_records()
        .into_iter()
        .map(|record| record.serial_hex)
        .collect();
    let result = async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("intendant-fleet-cert-monitor")
            .build()
            .map_err(|e| e.to_string())?;
        let response = client
            .get(format!("https://crt.sh/?q={name}&output=json"))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("crt.sh HTTP {}", response.status()));
        }
        let text = response.text().await.map_err(|e| e.to_string())?;
        parse_crt_sh_entries(&text)
    }
    .await;
    let now = now_unix_ms();
    match result {
        Ok(logged) => {
            let foreign = foreign_entries(logged, &own);
            with_status(|status| {
                status.ct_checked_unix_ms = Some(now);
                status.ct_last_error = None;
                if foreign.is_empty() {
                    status.ct_state = "ok".to_string();
                    status.ct_unknown = Vec::new();
                } else {
                    status.ct_state = "alert".to_string();
                    status.ct_unknown = foreign
                        .iter()
                        .map(|entry| {
                            format!(
                                "{} · {} · {}",
                                entry.serial_hex, entry.issuer, entry.not_before
                            )
                        })
                        .collect();
                }
            });
            let status = status_snapshot();
            if status.ct_state == "alert" {
                eprintln!(
                    "[fleet-cert] CT ALERT: {} certificate(s) for {name} in the public CT logs \
                     that this daemon never requested: {:?} — if you did not mint these through \
                     another channel, treat the fleet route as compromised and reach this \
                     daemon directly",
                    status.ct_unknown.len(),
                    status.ct_unknown,
                );
            }
        }
        Err(error) => {
            with_status(|status| {
                status.ct_last_error = Some(error);
            });
        }
    }
}

/// Renewal + CT loop: first tick shortly after startup (registration
/// needs a moment to learn the fleet name), then twice daily. Renewal
/// fires inside the last 30 days of validity (Let's Encrypt certificates
/// run 90); the CT tripwire runs every tick. Spawned once at gateway
/// startup.
pub fn spawn_renewal_loop() {
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let delay = if first { 10 * 60 } else { 12 * 60 * 60 };
            first = false;
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            ct_check_once().await;
            let status = status_snapshot();
            let Some(not_after) = status.not_after_unix_ms else {
                continue;
            };
            if status.state != "valid" || status.name.is_none() {
                continue;
            }
            let remaining_ms = not_after.saturating_sub(now_unix_ms());
            if remaining_ms > 30 * 24 * 60 * 60 * 1000 {
                continue;
            }
            let addresses = if status.addresses.is_empty() {
                default_publish_addresses()
            } else {
                status.addresses.clone()
            };
            if let Err(error) = request_certificate(addresses).await {
                eprintln!("[fleet-cert] renewal failed: {error}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_label_twins_the_service_derivation() {
        // Golden value pinned in bin/connect/dns.rs — change together.
        assert_eq!(
            daemon_fleet_label("example-daemon-id").as_deref(),
            Some("d-30a08371a38c1b447038")
        );
        assert!(daemon_fleet_label(" ").is_none());
    }

    /// Operator-battery E2E (never in CI: live network + a registered
    /// daemon). Drives the WHOLE issuance flow — signed address publish,
    /// ACME DNS-01 against the rendezvous fleet zone, certificate
    /// install — against a REAL rendezvous and the Let's Encrypt
    /// staging CA. Run as:
    ///
    /// ```text
    /// INTENDANT_HOME=<scratch> \
    /// INTENDANT_CONNECT_RENDEZVOUS_URL=https://intendant.dev \
    /// INTENDANT_CONNECT_DAEMON_ID=<registered daemon id> \
    /// INTENDANT_ACME_DIRECTORY=https://acme-staging-v02.api.letsencrypt.org/directory \
    /// cargo test --bin intendant fleet_cert_staging -- --ignored --nocapture
    /// ```
    ///
    /// The daemon id must already be registered (the process signs with
    /// the default daemon identity key, so register with that same key
    /// first — e.g. by running a scratch daemon once).
    #[tokio::test]
    #[ignore = "operator battery: live rendezvous + Let's Encrypt staging"]
    async fn fleet_cert_staging_e2e() {
        let zone = std::env::var("INTENDANT_FLEET_ZONE")
            .unwrap_or_else(|_| "fleet.intendant.dev".to_string());
        let daemon_id = std::env::var("INTENDANT_CONNECT_DAEMON_ID")
            .expect("set INTENDANT_CONNECT_DAEMON_ID to a registered daemon id");
        assert!(
            std::env::var("INTENDANT_ACME_DIRECTORY")
                .unwrap_or_default()
                .contains("staging"),
            "refusing to run the battery against the production CA"
        );
        let name = format!("{}.{}", daemon_fleet_label(&daemon_id).unwrap(), zone);
        note_fleet_dns(Some(zone), Some(name.clone()));

        request_certificate(default_publish_addresses())
            .await
            .expect("staging issuance should succeed");

        let status = status_snapshot();
        assert_eq!(status.state, "valid");
        assert!(status.not_after_unix_ms.unwrap() > now_unix_ms());
        let cert_pem = std::fs::read_to_string(cert_path()).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        println!(
            "staging certificate issued for {name}, valid until {:?}, addresses {:?}",
            status.not_after_unix_ms, status.addresses
        );
    }

    #[test]
    fn not_after_parses_a_real_certificate() {
        // A throwaway self-signed cert exercises the PEM + x509 path.
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["d-test.fleet.example.test".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let not_after = cert_not_after_unix_ms(&cert.pem()).unwrap();
        assert!(not_after > now_unix_ms());
    }

    #[test]
    fn serial_extraction_and_normalization_agree_with_crt_sh_format() {
        // A fixed serial with a leading zero byte (DER's positive-sign
        // padding) must normalize to what crt.sh prints for it.
        let mut params =
            rcgen::CertificateParams::new(vec!["d-test.fleet.example.test".to_string()]).unwrap();
        params.serial_number = Some(rcgen::SerialNumber::from(vec![0x00, 0x8a, 0xbc, 0x01]));
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        assert_eq!(cert_serial_hex(&cert.pem()).as_deref(), Some("8abc01"));

        assert_eq!(normalize_serial_hex("038ABc"), "38abc");
        assert_eq!(normalize_serial_hex("0000"), "0");
        assert_eq!(normalize_serial_hex(" 03f2 "), "3f2");
    }

    #[test]
    fn crt_sh_parsing_dedupes_precert_pairs_and_flags_foreign_serials() {
        let fixture = r#"[
            {"issuer_name":"C=US, O=Let's Encrypt, CN=R11","serial_number":"03AB01","not_before":"2026-07-09T00:00:00"},
            {"issuer_name":"C=US, O=Let's Encrypt, CN=R11","serial_number":"03ab01","not_before":"2026-07-09T00:00:00"},
            {"issuer_name":"C=US, O=Evil CA","serial_number":"04ff02","not_before":"2026-07-10T00:00:00"}
        ]"#;
        let entries = parse_crt_sh_entries(fixture).unwrap();
        assert_eq!(entries.len(), 2, "precert/leaf pair must dedupe");

        let own = vec!["3ab01".to_string()];
        let foreign = foreign_entries(entries, &own);
        assert_eq!(foreign.len(), 1);
        assert_eq!(foreign[0].serial_hex, "4ff02");
        assert!(foreign[0].issuer.contains("Evil CA"));

        assert!(parse_crt_sh_entries("<html>rate limited</html>").is_err());
        assert_eq!(parse_crt_sh_entries("[]").unwrap().len(), 0);
    }
}
