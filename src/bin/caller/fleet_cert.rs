//! Fleet certificates: a real, browser-trusted certificate for this
//! daemon's fleet name (docs/src/trust-tiers.md; the warning-free discovery
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
//! is an opaque hash for exactly that reason), and a hostile zone operator
//! can redirect a fleet name and mint a certificate for it. CT makes that
//! issuance public evidence, but evidence is not an authority anchor: the
//! gateway serves only public shell/discovery bytes on fleet-SNI connections
//! and rejects protected HTTP, MCP, signaling, and WebSocket access before it
//! resolves browser mTLS or daemon IAM authority.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const FLEET_ORIGIN_PROVENANCE_FILE: &str = "fleet-origin-provenance.json";
const FLEET_ORIGIN_PROVENANCE_SCHEMA_VERSION: u32 = 1;
const FLEET_CERT_REQUESTED_FILE: &str = "fleet-cert-requested";
const FLEET_CERT_REQUESTED_MARKER: &[u8] = b"intendant-fleet-certificate-requested-v1\n";
const FLEET_CERT_SERIALS_MAX_BYTES: u64 = 1024 * 1024;
const FLEET_CT_STATUS_FILE: &str = "fleet-cert-ct-status.json";
const FLEET_CT_STATUS_MAX_BYTES: u64 = 1024 * 1024;
const FLEET_CT_RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;
const FLEET_CT_FOREIGN_SERIALS_MAX: usize = 256;
const FLEET_CERT_ISSUANCE_FILE: &str = "fleet-cert-issuance.json";
const FLEET_CERT_ISSUANCE_SCHEMA_VERSION: u32 = 1;
const FLEET_CERT_ISSUANCE_MAX_BYTES: u64 = 64 * 1024;
const FLEET_CERT_ISSUANCE_TTL_MS: u64 = 2 * 60 * 60 * 1000;
const FLEET_CERT_ISSUANCE_MAX_ACTIVE: usize = 16;

/// Durable service-controlled-name provenance. Certificates and Connect
/// registration are both replaceable/optional at startup, but a name once
/// assigned by a rendezvous must never later be mistaken for an independent
/// direct origin merely because the daemon is offline or Connect is disabled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FleetOriginProvenance {
    #[serde(default = "fleet_origin_provenance_schema_version")]
    schema_version: u32,
    #[serde(default)]
    zone: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    known_names: Vec<String>,
    #[serde(default)]
    known_zones: Vec<String>,
    /// Recovery could not prove the complete historical name set. While this
    /// is true, IAM treats DNS-origin browser keys conservatively instead of
    /// allowing an unknown former fleet name to decay into a direct anchor.
    #[serde(default)]
    provenance_incomplete: bool,
}

fn fleet_origin_provenance_schema_version() -> u32 {
    FLEET_ORIGIN_PROVENANCE_SCHEMA_VERSION
}

impl Default for FleetOriginProvenance {
    fn default() -> Self {
        Self {
            schema_version: FLEET_ORIGIN_PROVENANCE_SCHEMA_VERSION,
            zone: None,
            name: None,
            known_names: Vec::new(),
            known_zones: Vec::new(),
            provenance_incomplete: false,
        }
    }
}

fn normalized_dns_name(value: &str) -> Option<String> {
    let normalized = value.trim().trim_end_matches('.').to_ascii_lowercase();
    (!normalized.is_empty()).then_some(normalized)
}

fn fleet_origin_provenance_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join(FLEET_ORIGIN_PROVENANCE_FILE)
}

fn write_fleet_origin_provenance_locked(
    cert_dir: &Path,
    provenance: &FleetOriginProvenance,
) -> crate::access::AccessResult<()> {
    let mut bytes = serde_json::to_vec_pretty(provenance).map_err(|error| {
        crate::access::AccessError(format!("serialize fleet origin provenance: {error}"))
    })?;
    bytes.push(b'\n');
    crate::access::authority_store::atomic_write_private_locked(
        &fleet_origin_provenance_path_in(cert_dir),
        &bytes,
    )
}

fn load_fleet_origin_provenance_in(cert_dir: &Path) -> Result<FleetOriginProvenance, String> {
    let path = fleet_origin_provenance_path_in(cert_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FleetOriginProvenance::default());
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut provenance: FleetOriginProvenance = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if provenance.schema_version > FLEET_ORIGIN_PROVENANCE_SCHEMA_VERSION {
        return Err(format!(
            "{} uses unsupported schema version {}",
            path.display(),
            provenance.schema_version
        ));
    }
    provenance.schema_version = FLEET_ORIGIN_PROVENANCE_SCHEMA_VERSION;
    provenance.zone = provenance.zone.as_deref().and_then(normalized_dns_name);
    provenance.name = provenance.name.as_deref().and_then(normalized_dns_name);
    provenance.known_zones = provenance
        .known_zones
        .iter()
        .filter_map(|zone| normalized_dns_name(zone))
        .collect();
    if let Some(zone) = provenance.zone.clone() {
        provenance.known_zones.push(zone);
    }
    provenance.known_zones.sort();
    provenance.known_zones.dedup();
    provenance.known_names = provenance
        .known_names
        .iter()
        .filter_map(|name| normalized_dns_name(name))
        .collect();
    if let Some(name) = provenance.name.clone() {
        provenance.known_names.push(name);
    }
    provenance.known_names.sort();
    provenance.known_names.dedup();
    Ok(provenance)
}

pub(crate) fn current_fleet_name_in(cert_dir: &Path) -> Result<Option<String>, String> {
    load_fleet_origin_provenance_in(cert_dir).map(|provenance| provenance.name)
}

pub(crate) fn is_service_controlled_name_in(cert_dir: &Path, name: &str) -> Result<bool, String> {
    let Some(name) = normalized_dns_name(name) else {
        return Ok(false);
    };
    let provenance = load_fleet_origin_provenance_in(cert_dir)?;
    if provenance.provenance_incomplete {
        return Err(
            "fleet-origin provenance is incomplete; custom-domain separation cannot be proven"
                .to_string(),
        );
    }
    Ok(provenance.known_names.iter().any(|known| known == &name)
        || provenance.known_zones.iter().any(|zone| {
            name == *zone
                || name
                    .strip_suffix(zone)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        }))
}

fn remember_fleet_origin_in(
    cert_dir: &Path,
    zone: Option<&str>,
    name: &str,
) -> Result<FleetOriginProvenance, String> {
    let name = normalized_dns_name(name)
        .ok_or_else(|| "rendezvous returned an empty fleet name".to_string())?;
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut provenance =
            load_fleet_origin_provenance_in(cert_dir).map_err(crate::access::AccessError)?;
        let before = provenance.clone();
        provenance.zone = zone.and_then(normalized_dns_name);
        if let Some(zone) = provenance.zone.clone() {
            provenance.known_zones.push(zone);
            provenance.known_zones.sort();
            provenance.known_zones.dedup();
        }
        provenance.name = Some(name.clone());
        provenance.known_names.push(name);
        provenance.known_names.sort();
        provenance.known_names.dedup();
        if provenance == before {
            return Ok(provenance);
        }
        write_fleet_origin_provenance_locked(cert_dir, &provenance)?;
        Ok(provenance)
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(crate) fn remember_fleet_origin_for_test(
    cert_dir: &Path,
    zone: Option<&str>,
    name: &str,
) -> Result<(), String> {
    remember_fleet_origin_in(cert_dir, zone, name).map(|_| ())
}

fn fleet_origin_provenance_incomplete_flag() -> &'static AtomicBool {
    static INCOMPLETE: AtomicBool = AtomicBool::new(false);
    &INCOMPLETE
}

/// Whether startup found fleet-origin state whose full historical exact-name
/// set could not be recovered. This is process-sticky: learning one current
/// name cannot prove that a corrupted file held no older service-controlled
/// names. IAM uses it only as a conservative browser-key binding guard.
pub(crate) fn fleet_origin_provenance_is_incomplete() -> bool {
    fleet_origin_provenance_incomplete_flag().load(Ordering::SeqCst)
}

fn mark_fleet_origin_provenance_incomplete() {
    fleet_origin_provenance_incomplete_flag().store(true, Ordering::SeqCst);
}

fn fleet_dns_observed_this_process() -> &'static AtomicBool {
    static OBSERVED: AtomicBool = AtomicBool::new(false);
    &OBSERVED
}

/// The daemon's fleet label: `d-<hex(sha256(daemon_id))[..20]>` —
/// REPLICATES `bin/connect/dns.rs::daemon_label` (the two binaries never
/// link each other); the golden test below twins the service's.
#[cfg(test)]
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
    /// Certificate Transparency tripwire (docs/src/trust-tiers.md, fleet
    /// discovery route): `unchecked` | `ok` | `alert`. An `alert` means
    /// the public CT logs hold a certificate for this daemon's name that
    /// this daemon never requested — the fleet-zone operator (or a CA)
    /// minted one, which is exactly the betrayal the rung's security
    /// argument says must leave evidence. Reflects the last successful
    /// check; fetch failures land in `ct_last_error` instead.
    pub ct_state: String,
    /// The foreign certificates behind an `alert`: "serial · issuer ·
    /// not_before" summaries.
    pub ct_unknown: Vec<String>,
    /// Normalized serials corresponding to `ct_unknown`. Kept structured so
    /// hosted-lane admission never parses display strings.
    pub ct_foreign_serials: Vec<String>,
    pub ct_checked_unix_ms: Option<u64>,
    pub ct_last_error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CtGuardEvidence {
    pub foreign_serials: Vec<String>,
    pub state_unavailable: bool,
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
    registry()
        .lock()
        .expect("fleet cert status poisoned")
        .clone()
}

pub fn ct_guard_evidence() -> CtGuardEvidence {
    let snapshot = status_snapshot();
    let Some(name) = snapshot.name.as_deref().and_then(normalized_dns_name) else {
        return ct_guard_evidence_from_status(&snapshot);
    };
    match durable_ct_status_for_name_in(&cert_dir(), &name) {
        Ok(Some(durable)) => {
            let evidence = CtGuardEvidence {
                foreign_serials: durable.foreign_serials.clone(),
                state_unavailable: false,
            };
            with_status(|status| {
                if fleet_name_matches(status.name.as_deref(), &name) {
                    let _ = apply_loaded_ct_status(status, Ok(Some(durable)));
                }
            });
            evidence
        }
        Ok(None) => ct_guard_evidence_from_status(&snapshot),
        Err(error) => {
            with_status(|status| {
                if fleet_name_matches(status.name.as_deref(), &name) {
                    status.ct_state = "unreadable".to_string();
                    status.ct_foreign_serials.clear();
                    status.ct_unknown.clear();
                    status.ct_last_error = Some(error);
                }
            });
            CtGuardEvidence {
                foreign_serials: Vec::new(),
                state_unavailable: true,
            }
        }
    }
}

fn ct_guard_evidence_from_status(status: &FleetCertStatus) -> CtGuardEvidence {
    CtGuardEvidence {
        foreign_serials: status.ct_foreign_serials.clone(),
        state_unavailable: status.ct_state == "unreadable",
    }
}

/// The delegated fleet zone alone. The request-path IAM evaluator only ever
/// reads `.zone`, and [`status_snapshot`] deep-clones the whole struct
/// (address vectors and CT serial summaries included) under the mutex —
/// per authorization decision, daemon-wide.
pub fn fleet_zone() -> Option<String> {
    registry()
        .lock()
        .expect("fleet cert status poisoned")
        .zone
        .clone()
}

fn with_status(update: impl FnOnce(&mut FleetCertStatus)) {
    let mut status = registry().lock().expect("fleet cert status poisoned");
    update(&mut status);
}

/// Called from the Connect client when a register response carries the
/// fleet_dns hint. Also loads any existing on-disk certificate state the
/// first time the name is learned.
pub fn note_fleet_dns(zone: Option<String>, name: Option<String>) {
    fleet_dns_observed_this_process().store(true, Ordering::SeqCst);
    if let Some(name) = name.as_deref() {
        // The rendezvous-assigned public name is never an authority anchor,
        // regardless of whether its WebPKI certificate has been issued or
        // installed yet. Register provenance before updating live state so
        // gateway requests fail closed during name/certificate transitions.
        crate::web_tls::register_fleet_server_name(name);
        match remember_fleet_origin_in(&cert_dir(), zone.as_deref(), name) {
            Ok(provenance) if provenance.provenance_incomplete => {
                mark_fleet_origin_provenance_incomplete();
            }
            Ok(_) => {}
            Err(error) => {
                // In-memory exact-name classification still fails closed for
                // this process, but persistence failure means older names may
                // be unrecoverable after restart. Keep the conservative IAM
                // guard sticky and surface the durability failure loudly.
                mark_fleet_origin_provenance_incomplete();
                eprintln!("[fleet-cert] persist fleet-origin provenance failed: {error}");
            }
        }
    }
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

fn cert_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join("fleet-cert.pem")
}

fn key_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join("fleet-key.pem")
}

fn issuance_requested_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join(FLEET_CERT_REQUESTED_FILE)
}

fn issuance_requested_locked_in(cert_dir: &Path) -> Result<bool, String> {
    let path = issuance_requested_path_in(cert_dir);
    match std::fs::read(&path) {
        Ok(bytes) if bytes == FLEET_CERT_REQUESTED_MARKER => Ok(true),
        Ok(_) => Err(format!(
            "{} contains an invalid fleet-certificate request marker",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn issuance_requested_in(cert_dir: &Path) -> Result<bool, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        issuance_requested_locked_in(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
}

fn mark_issuance_requested_in(cert_dir: &Path) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        if issuance_requested_locked_in(cert_dir).map_err(crate::access::AccessError)? {
            return Ok(());
        }
        crate::access::authority_store::atomic_write_private_locked(
            &issuance_requested_path_in(cert_dir),
            FLEET_CERT_REQUESTED_MARKER,
        )
    })
    .map_err(|error| error.to_string())
}

/// Certificate expiry (`not_after`, unix ms) from a PEM chain's leaf.
pub(crate) fn cert_not_after_unix_ms(cert_pem: &str) -> Option<u64> {
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

fn fleet_certificate_dns_names(cert_pem: &str) -> Result<Vec<String>, String> {
    use x509_parser::extensions::GeneralName;
    use x509_parser::prelude::*;

    let leaf = pem_certificates(cert_pem)
        .into_iter()
        .next()
        .ok_or_else(|| "fleet certificate PEM holds no certificates".to_string())?;
    let (_, certificate) = X509Certificate::from_der(&leaf)
        .map_err(|error| format!("parse fleet certificate: {error}"))?;
    let san = certificate
        .subject_alternative_name()
        .map_err(|error| format!("parse fleet certificate SAN: {error}"))?
        .ok_or_else(|| "fleet certificate has no subjectAltName extension".to_string())?;
    let mut names = Vec::new();
    for name in &san.value.general_names {
        let GeneralName::DNSName(name) = name else {
            continue;
        };
        if name.contains('*') {
            return Err(format!(
                "fleet certificate carries wildcard DNS SAN {name}; exact provenance is unrecoverable"
            ));
        }
        if let Some(name) = normalized_dns_name(name) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err("fleet certificate has no exact DNS SAN names".to_string());
    }
    Ok(names)
}

fn require_fleet_certificate_dns_name(cert_pem: &str, expected: &str) -> Result<(), String> {
    let expected = normalized_dns_name(expected)
        .ok_or_else(|| "current fleet certificate name is empty".to_string())?;
    let names = fleet_certificate_dns_names(cert_pem)?;
    if names.as_slice() == [expected.as_str()] {
        Ok(())
    } else {
        Err(format!(
            "fleet certificate SANs must equal only the current exact name {expected}"
        ))
    }
}

fn fleet_certificate_dns_names_in(cert_dir: &Path) -> Result<Option<Vec<String>>, String> {
    let path = cert_path_in(cert_dir);
    let pem = match std::fs::read_to_string(&path) {
        Ok(pem) => pem,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    fleet_certificate_dns_names(&pem).map(Some)
}

fn merge_recovered_fleet_names(
    provenance: &mut FleetOriginProvenance,
    recovered_names: Vec<String>,
) {
    if provenance.name.is_none() {
        provenance.name = recovered_names.first().cloned();
    }
    provenance.known_names.extend(recovered_names);
    provenance.known_names.sort();
    provenance.known_names.dedup();
}

#[derive(Debug)]
struct RestoredFleetOriginProvenance {
    provenance: FleetOriginProvenance,
    warning: Option<String>,
}

/// Restore the durable exact-name set, migrating pre-provenance installs from
/// the DNS SANs in `fleet-cert.pem`. The authority-store lock serializes this
/// read/merge/write with a concurrent Connect register response.
///
/// A malformed provenance file is never overwritten automatically: it may be
/// the only surviving record of older names. We recover the current
/// certificate's exact names for this process, mark the result incomplete,
/// and leave the malformed file in place so every later startup also fails
/// closed. An existing certificate whose exact DNS SANs cannot be recovered
/// persists `provenance_incomplete: true` in an otherwise valid state file.
fn restore_fleet_origin_provenance_in(cert_dir: &Path) -> RestoredFleetOriginProvenance {
    let restored = crate::access::authority_store::with_lock(cert_dir, || {
        let loaded = load_fleet_origin_provenance_in(cert_dir);
        let mut provenance = match loaded {
            Ok(provenance) => provenance,
            Err(load_error) => {
                let mut provenance = FleetOriginProvenance {
                    provenance_incomplete: true,
                    ..Default::default()
                };
                let certificate_error = match fleet_certificate_dns_names_in(cert_dir) {
                    Ok(Some(names)) => {
                        merge_recovered_fleet_names(&mut provenance, names);
                        None
                    }
                    Ok(None) => None,
                    Err(error) => Some(error),
                };
                let warning = match certificate_error {
                    Some(certificate_error) => format!(
                        "{load_error}; certificate provenance recovery also failed: {certificate_error}"
                    ),
                    None => load_error,
                };
                // Preserve the malformed/unsupported source file. Its
                // continued parse failure is itself a durable fail-closed
                // marker for the historical names we could not recover.
                return Ok(RestoredFleetOriginProvenance {
                    provenance,
                    warning: Some(warning),
                });
            }
        };

        let before = provenance.clone();
        let mut warning = None;
        match fleet_certificate_dns_names_in(cert_dir) {
            Ok(Some(names)) => merge_recovered_fleet_names(&mut provenance, names),
            Ok(None) => {}
            Err(error) => {
                provenance.provenance_incomplete = true;
                warning = Some(format!(
                    "existing fleet certificate provenance could not be recovered: {error}"
                ));
            }
        }
        if provenance != before {
            write_fleet_origin_provenance_locked(cert_dir, &provenance)?;
        }
        Ok(RestoredFleetOriginProvenance {
            provenance,
            warning,
        })
    });

    match restored {
        Ok(restored) => restored,
        Err(error) => {
            // A lock or durable-write failure must not turn an old fleet name
            // into a direct origin. Recover any readable exact names for this
            // process, but keep the broad conservative marker set.
            let mut provenance = load_fleet_origin_provenance_in(cert_dir).unwrap_or_else(|_| {
                FleetOriginProvenance {
                    provenance_incomplete: true,
                    ..Default::default()
                }
            });
            provenance.provenance_incomplete = true;
            if let Ok(Some(names)) = fleet_certificate_dns_names_in(cert_dir) {
                merge_recovered_fleet_names(&mut provenance, names);
            }
            RestoredFleetOriginProvenance {
                provenance,
                warning: Some(format!(
                    "restore fleet-origin provenance under authority lock: {error}"
                )),
            }
        }
    }
}

fn register_restored_fleet_origins(provenance: &FleetOriginProvenance) {
    for name in &provenance.known_names {
        crate::web_tls::register_fleet_server_name(name);
    }
}

/// Load on-disk certificate state into the registry and the live TLS
/// resolver (startup + after learning the name). Quietly does nothing
/// when no certificate exists yet.
pub fn refresh_installed_state() {
    refresh_installed_state_in(&cert_dir());
}

pub(crate) fn refresh_installed_state_in(cert_dir: &Path) {
    let restored = restore_fleet_origin_provenance_in(cert_dir);
    register_restored_fleet_origins(&restored.provenance);
    if restored.provenance.provenance_incomplete {
        mark_fleet_origin_provenance_incomplete();
    }
    if let Some(error) = restored.warning {
        eprintln!("[fleet-cert] restore fleet-origin provenance: {error}");
    }
    // Offline/Connect-disabled startup restores the last current name so an
    // installed certificate remains usable. A register response observed in
    // this process wins, including an explicit null hint; remembered names
    // remain discovery-only either way.
    if !fleet_dns_observed_this_process().load(Ordering::SeqCst) {
        with_status(|status| {
            status.zone = restored.provenance.zone;
            status.name = restored.provenance.name;
        });
    }
    // The durable CT verdict is exact-name bound, so restore the current name
    // before applying it. A verdict from an older fleet name fails closed.
    restore_ct_status_in(cert_dir);
    if let Err(error) = own_serial_records_in(cert_dir) {
        with_status(|status| {
            status.ct_state = "unreadable".to_string();
            status.ct_foreign_serials.clear();
            status.ct_unknown.clear();
            status.ct_last_error = Some(error.clone());
        });
        eprintln!(
            "[fleet-cert] own-certificate ledger is unreadable; hosted lane suspended: {error}"
        );
    }
    let (cert_pem, key_pem) = match read_stored_certificate_pair_in(cert_dir) {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            match missing_pair_retry_error_in(cert_dir) {
                Ok(Some(error)) => with_status(|status| {
                    status.state = "error".to_string();
                    status.not_after_unix_ms = None;
                    status.last_error = Some(error);
                }),
                Ok(None) => {}
                Err(error) => with_status(|status| {
                    status.state = "error".to_string();
                    status.not_after_unix_ms = None;
                    status.last_error =
                        Some(format!("read fleet-certificate request marker: {error}"));
                }),
            }
            return;
        }
        Err(error) => {
            with_status(|status| {
                status.state = "error".to_string();
                status.not_after_unix_ms = None;
                status.last_error = Some(error);
            });
            return;
        }
    };
    // Existing installs predate the explicit request marker. The stored pair
    // proves that renewal was enabled, so migrate that intent before serving
    // the certificate. A later crash that loses both pair files must remain
    // retryable rather than looking like a never-enabled daemon.
    if let Err(error) = mark_issuance_requested_in(cert_dir) {
        with_status(|status| {
            status.state = "error".to_string();
            status.not_after_unix_ms = None;
            status.last_error = Some(format!("persist fleet-certificate renewal intent: {error}"));
        });
        return;
    }
    let Some(name) = status_snapshot().name else {
        // Cert on disk but no name learned yet: install once the
        // register response names us (note_fleet_dns re-runs this).
        return;
    };
    let not_after = cert_not_after_unix_ms(&cert_pem);
    let install_result = require_fleet_certificate_dns_name(&cert_pem, &name)
        .and_then(|()| crate::web_tls::install_fleet_certificate(&name, &cert_pem, &key_pem));
    match install_result {
        Ok(()) => with_status(|status| {
            if fleet_name_matches(status.name.as_deref(), &name) {
                status.state = "valid".to_string();
                status.not_after_unix_ms = not_after;
                status.last_error = None;
            } else {
                status.state = "error".to_string();
                status.not_after_unix_ms = None;
                status.last_error = Some(
                    "fleet name changed while the stored certificate was being restored; renewal will retry"
                        .to_string(),
                );
            }
        }),
        Err(error) => with_status(|status| {
            status.state = "error".to_string();
            status.not_after_unix_ms = None;
            status.last_error = Some(format!("validate/install stored certificate: {error}"));
        }),
    }
}

fn missing_pair_retry_error_in(cert_dir: &Path) -> Result<Option<String>, String> {
    issuance_requested_in(cert_dir).map(|requested| {
        requested.then(|| {
            "fleet certificate issuance was requested, but no stored certificate pair exists; renewal will retry"
                .to_string()
        })
    })
}

fn fleet_name_matches(current: Option<&str>, expected: &str) -> bool {
    current.and_then(normalized_dns_name).as_deref() == normalized_dns_name(expected).as_deref()
}

fn ensure_current_fleet_name(expected: &str) -> Result<(), String> {
    if fleet_name_matches(status_snapshot().name.as_deref(), expected) {
        Ok(())
    } else {
        Err(format!(
            "fleet name changed while certificate issuance was running for {expected}; retry against the current name"
        ))
    }
}

fn read_stored_certificate_pair_in(cert_dir: &Path) -> Result<Option<(String, String)>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        read_stored_certificate_pair_locked_in(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
}

fn read_stored_certificate_pair_locked_in(
    cert_dir: &Path,
) -> Result<Option<(String, String)>, String> {
    let cert_path = cert_path_in(cert_dir);
    let key_path = key_path_in(cert_dir);
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
        (Err(error), _) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "stored fleet certificate pair is incomplete: {} is missing",
            cert_path.display()
        )),
        (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "stored fleet certificate pair is incomplete: {} is missing",
            key_path.display()
        )),
        (Err(error), _) => Err(format!("read {}: {error}", cert_path.display())),
        (_, Err(error)) => Err(format!("read {}: {error}", key_path.display())),
    }
}

fn now_unix_ms() -> u64 {
    crate::access::client_key::now_unix_ms().max(0) as u64
}

/// The ACME directory: Let's Encrypt production unless overridden (the
/// staging directory for rig runs: `INTENDANT_ACME_DIRECTORY`).
pub(crate) fn acme_directory() -> String {
    std::env::var("INTENDANT_ACME_DIRECTORY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| instant_acme::LetsEncrypt::Production.url().to_string())
}

async fn acme_account() -> Result<instant_acme::Account, String> {
    acme_account_in(&cert_dir()).await
}

pub(crate) async fn acme_account_in(cert_dir: &Path) -> Result<instant_acme::Account, String> {
    let path = cert_dir.join("acme-account.json");
    match std::fs::read_to_string(&path) {
        Ok(stored) => return restore_acme_account(&path, &stored).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read {}: {error}", path.display())),
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
    let installed =
        crate::access::authority_store::with_lock(cert_dir, || match std::fs::metadata(&path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::access::authority_store::atomic_write_private_locked(
                    &path,
                    serialized.as_bytes(),
                )?;
                Ok(true)
            }
            Err(error) => Err(crate::access::AccessError(format!(
                "inspect {}: {error}",
                path.display()
            ))),
        })
        .map_err(|error| error.to_string())?;
    if installed {
        return Ok(account);
    }

    // Another daemon process won the first-account race. The durable account
    // is the CAA-pinned identity; discard this process's unused account
    // instead of overwriting or silently rotating it.
    let stored = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    restore_acme_account(&path, &stored).await
}

async fn restore_acme_account(path: &Path, stored: &str) -> Result<instant_acme::Account, String> {
    let credentials =
        serde_json::from_str::<instant_acme::AccountCredentials>(stored).map_err(|error| {
            format!(
                "parse {}: {error}; refusing to rotate the stored ACME account",
                path.display()
            )
        })?;
    instant_acme::Account::builder()
        .map_err(|e| format!("acme http client: {e}"))?
        .from_credentials(credentials)
        .await
        .map_err(|e| format!("restore acme account {}: {e}", path.display()))
}

pub(crate) fn acme_account_uri_in(cert_dir: &Path) -> Result<Option<String>, String> {
    let path = cert_dir.join("acme-account.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let _credentials = serde_json::from_slice::<instant_acme::AccountCredentials>(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let account_uri = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{} has no ACME account URI", path.display()))?;
    let parsed = url::Url::parse(account_uri).map_err(|error| {
        format!(
            "{} has an invalid ACME account URI: {error}",
            path.display()
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!(
            "{} has an invalid ACME account URI",
            path.display()
        ));
    }
    Ok(Some(account_uri.to_string()))
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InFlightIssuance {
    token: String,
    name: String,
    started_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InFlightIssuanceStore {
    schema_version: u32,
    orders: Vec<InFlightIssuance>,
}

fn issuance_store_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join(FLEET_CERT_ISSUANCE_FILE)
}

fn load_issuance_store_locked_in(cert_dir: &Path) -> Result<InFlightIssuanceStore, String> {
    use std::io::Read as _;

    let path = issuance_store_path_in(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InFlightIssuanceStore {
                schema_version: FLEET_CERT_ISSUANCE_SCHEMA_VERSION,
                orders: Vec::new(),
            });
        }
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(FLEET_CERT_ISSUANCE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > FLEET_CERT_ISSUANCE_MAX_BYTES {
        return Err(format!(
            "{} exceeds the issuance-state size limit",
            path.display()
        ));
    }
    let mut store: InFlightIssuanceStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if store.schema_version != FLEET_CERT_ISSUANCE_SCHEMA_VERSION
        || store.orders.len() > FLEET_CERT_ISSUANCE_MAX_ACTIVE
        || store.orders.iter().any(|order| {
            order.token.is_empty()
                || order.token.len() > 64
                || normalized_dns_name(&order.name).as_deref() != Some(order.name.as_str())
        })
    {
        return Err(format!(
            "{} contains invalid issuance state",
            path.display()
        ));
    }
    let now = now_unix_ms();
    store
        .orders
        .retain(|order| now.saturating_sub(order.started_unix_ms) < FLEET_CERT_ISSUANCE_TTL_MS);
    Ok(store)
}

fn write_issuance_store_locked_in(
    cert_dir: &Path,
    store: &InFlightIssuanceStore,
) -> crate::access::AccessResult<()> {
    let mut serialized = serde_json::to_vec_pretty(store).map_err(|error| {
        crate::access::AccessError(format!("serialize fleet issuance state: {error}"))
    })?;
    serialized.push(b'\n');
    if serialized.len() as u64 > FLEET_CERT_ISSUANCE_MAX_BYTES {
        return Err(crate::access::AccessError(
            "fleet issuance state exceeds its size limit".to_string(),
        ));
    }
    crate::access::authority_store::atomic_write_private_locked(
        &issuance_store_path_in(cert_dir),
        &serialized,
    )
}

fn begin_issuance_in(cert_dir: &Path, name: &str) -> Result<String, String> {
    let name = normalized_dns_name(name)
        .ok_or_else(|| "cannot start issuance for an empty fleet name".to_string())?;
    crate::access::authority_store::with_lock(cert_dir, || {
        let provenance =
            load_fleet_origin_provenance_in(cert_dir).map_err(crate::access::AccessError)?;
        if provenance.name.as_deref() != Some(name.as_str()) {
            return Err(crate::access::AccessError(format!(
                "fleet name changed before issuance began for {name}"
            )));
        }
        let mut store =
            load_issuance_store_locked_in(cert_dir).map_err(crate::access::AccessError)?;
        if store.orders.iter().any(|order| order.name == name) {
            return Err(crate::access::AccessError(
                "a certificate request is already running in another daemon process".to_string(),
            ));
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        store.orders.push(InFlightIssuance {
            token: token.clone(),
            name,
            started_unix_ms: now_unix_ms(),
        });
        write_issuance_store_locked_in(cert_dir, &store)?;
        Ok(token)
    })
    .map_err(|error| error.to_string())
}

fn finish_issuance_in(cert_dir: &Path, token: &str) -> Result<(), String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        let mut store =
            load_issuance_store_locked_in(cert_dir).map_err(crate::access::AccessError)?;
        store.orders.retain(|order| order.token != token);
        write_issuance_store_locked_in(cert_dir, &store)
    })
    .map_err(|error| error.to_string())
}

fn issuance_active_locked_in(cert_dir: &Path, name: &str) -> Result<bool, String> {
    let store = load_issuance_store_locked_in(cert_dir)?;
    Ok(store.orders.iter().any(|order| order.name == name))
}

struct IssuanceGuard {
    cert_dir: PathBuf,
    token: Option<String>,
}

impl IssuanceGuard {
    fn begin(cert_dir: &Path, name: &str) -> Result<Self, String> {
        Ok(Self {
            cert_dir: cert_dir.to_path_buf(),
            token: Some(begin_issuance_in(cert_dir, name)?),
        })
    }

    fn finish(mut self) -> Result<(), String> {
        let token = self.token.take().expect("issuance guard already finished");
        let result = finish_issuance_in(&self.cert_dir, &token);
        if let Err(error) = &result {
            with_status(|status| {
                status.ct_state = "unreadable".to_string();
                status.ct_last_error =
                    Some(format!("clear durable certificate issuance state: {error}"));
            });
        }
        result
    }
}

impl Drop for IssuanceGuard {
    fn drop(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        if let Err(error) = finish_issuance_in(&self.cert_dir, &token) {
            eprintln!("[fleet-cert] clear durable issuance state: {error}");
            with_status(|status| {
                status.ct_state = "unreadable".to_string();
                status.ct_last_error =
                    Some(format!("clear durable certificate issuance state: {error}"));
            });
        }
    }
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
    // This is the durable opt-in to issuance and renewal. It lands before
    // any network side effect so a crash after ACME starts cannot make a
    // missing first pair look indistinguishable from a never-enabled daemon.
    let certificate_dir = cert_dir();
    mark_issuance_requested_in(&certificate_dir)?;
    let issuance = IssuanceGuard::begin(&certificate_dir, &name)?;
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
    require_fleet_certificate_dns_name(&cert_chain_pem, &name)?;
    ensure_current_fleet_name(&name)?;
    // The CT tripwire's own-serial ledger — recorded before install so a
    // crash here can't make this certificate look foreign later. Failure is
    // loud and retryable: installing an unrecorded certificate would create
    // a false CT alert.
    record_own_certificate_in(&certificate_dir, &cert_chain_pem, &name, &acme_directory())?;
    // Best-effort challenge cleanup after the serial is durable. Keeping the
    // issuance marker through both steps prevents a CT poll from classifying
    // the just-issued precertificate before its own-serial record exists.
    let _ = crate::connect_rendezvous::dns_acme_clear().await;

    // 3. Persist + install live. The per-file replacements are atomic and
    // the shared authority lock prevents two daemon processes from
    // interleaving different pairs. A crash between the two replacements is
    // detected at restore and retried by the renewal loop.
    persist_certificate_pair_in(&certificate_dir, &name, &cert_chain_pem, &private_key_pem)?;
    ensure_current_fleet_name(&name)?;
    crate::web_tls::install_fleet_certificate(&name, &cert_chain_pem, &private_key_pem)?;
    let mut name_changed = false;
    with_status(|status| {
        if fleet_name_matches(status.name.as_deref(), &name) {
            status.state = "valid".to_string();
            status.not_after_unix_ms = cert_not_after_unix_ms(&cert_chain_pem);
            status.last_error = None;
        } else {
            name_changed = true;
        }
    });
    if name_changed {
        return Err(format!(
            "fleet name changed after certificate issuance completed for {name}; retry against the current name"
        ));
    }
    issuance.finish()?;
    Ok(())
}

fn persist_certificate_pair_in(
    cert_dir: &Path,
    expected_name: &str,
    cert_chain_pem: &str,
    private_key_pem: &str,
) -> Result<(), String> {
    let expected_name = normalized_dns_name(expected_name)
        .ok_or_else(|| "cannot persist a certificate for an empty fleet name".to_string())?;
    crate::access::authority_store::with_lock(cert_dir, || {
        let provenance =
            load_fleet_origin_provenance_in(cert_dir).map_err(crate::access::AccessError)?;
        if provenance.name.as_deref() != Some(expected_name.as_str()) {
            return Err(crate::access::AccessError(format!(
                "fleet name changed before certificate commit for {expected_name}; no certificate pair was written"
            )));
        }
        crate::access::authority_store::atomic_write_private_locked(
            &key_path_in(cert_dir),
            private_key_pem.as_bytes(),
        )?;
        crate::access::authority_store::atomic_write_private_locked(
            &cert_path_in(cert_dir),
            cert_chain_pem.as_bytes(),
        )
    })
    .map_err(|error| error.to_string())
}

/* ── Certificate Transparency tripwire ──
A fleet-name hijack needs a certificate browsers accept, and public CAs log
those certificates to CT. This monitor turns that evidence into an alarm —
the daemon records the serials of every certificate IT obtained and
periodically asks the public CT indexes whether its name carries any it
didn't. A confirmed foreign serial suspends the dark hosted-lease lane while
direct/mTLS/local management remains available. The public index is still a
best-effort service: fetch failures preserve the last successful verdict
instead of creating new evidence. */

#[derive(serde::Serialize, serde::Deserialize)]
struct OwnCertRecord {
    serial_hex: String,
    name: String,
    directory: String,
    issued_unix_ms: u64,
}

fn serials_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join("fleet-cert-serials.json")
}

/// Lowercase hex with leading zeros trimmed — both our parsed serials
/// and crt.sh's strings normalize to this before comparison.
pub(crate) fn normalize_serial_hex(serial: &str) -> String {
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

fn own_serial_records_locked_in(cert_dir: &Path) -> Result<Vec<OwnCertRecord>, String> {
    use std::io::Read as _;

    let path = serials_path_in(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(FLEET_CERT_SERIALS_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > FLEET_CERT_SERIALS_MAX_BYTES {
        return Err(format!(
            "{} exceeds the own-certificate ledger size limit",
            path.display()
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn own_serial_records_in(cert_dir: &Path) -> Result<Vec<OwnCertRecord>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        own_serial_records_locked_in(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
fn own_serials_for_exact_name_in(cert_dir: &Path, name: &str) -> Result<Vec<String>, String> {
    let Some(name) = normalized_dns_name(name) else {
        return Ok(Vec::new());
    };
    let mut serials: Vec<String> = own_serial_records_in(cert_dir)?
        .into_iter()
        .filter(|record| normalized_dns_name(&record.name).as_deref() == Some(name.as_str()))
        .filter_map(|record| {
            let serial = normalize_serial_hex(&record.serial_hex);
            (!serial.is_empty()
                && serial.len() <= 128
                && serial.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(serial)
        })
        .collect();
    serials.sort();
    serials.dedup();
    Ok(serials)
}

#[cfg(test)]
pub(crate) fn own_serials_for_name_in(cert_dir: &Path, name: &str) -> Vec<String> {
    own_serial_ledger_for_name_in(cert_dir, name)
        .ok()
        .flatten()
        .map(|(serials, _)| serials)
        .unwrap_or_default()
}

pub(crate) fn own_serial_ledger_for_name_in(
    cert_dir: &Path,
    name: &str,
) -> Result<Option<(Vec<String>, u64)>, String> {
    let Some(name) = normalized_dns_name(name) else {
        return Ok(None);
    };
    let mut records: Vec<(String, u64)> = own_serial_records_in(cert_dir)?
        .into_iter()
        .filter(|record| normalized_dns_name(&record.name).as_deref() == Some(name.as_str()))
        .filter_map(|record| {
            let serial = normalize_serial_hex(&record.serial_hex);
            (record.issued_unix_ms > 0
                && !serial.is_empty()
                && serial.len() <= 128
                && serial.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some((serial, record.issued_unix_ms))
        })
        .collect();
    records.sort_by(|(serial_a, issued_a), (serial_b, issued_b)| {
        issued_b.cmp(issued_a).then_with(|| serial_a.cmp(serial_b))
    });
    let mut seen = std::collections::BTreeSet::new();
    records.retain(|(serial, _)| seen.insert(serial.clone()));
    records.truncate(crate::access::hosted_control::HOSTED_CERTIFICATE_LEDGER_SERIALS_CAP);
    let Some(issued_unix_ms) = records.iter().map(|(_, issued)| *issued).max() else {
        return Ok(None);
    };
    let mut serials: Vec<String> = records.into_iter().map(|(serial, _)| serial).collect();
    serials.sort();
    Ok(Some((serials, issued_unix_ms)))
}

/// Record a certificate this daemon obtained — BEFORE install, so a
/// crash between issuance and install can't leave an own-cert looking
/// foreign at the next check.
fn record_own_certificate_in(
    cert_dir: &Path,
    cert_pem: &str,
    name: &str,
    directory: &str,
) -> Result<(), String> {
    let serial = cert_serial_hex(cert_pem)
        .ok_or_else(|| "issued fleet certificate has no usable serial".to_string())?;
    let name = normalized_dns_name(name)
        .ok_or_else(|| "issued fleet certificate has no usable exact name".to_string())?;
    let reconciled = crate::access::authority_store::with_lock(cert_dir, || {
        let mut records =
            own_serial_records_locked_in(cert_dir).map_err(crate::access::AccessError)?;
        let already_recorded = records.iter().any(|record| {
            normalize_serial_hex(&record.serial_hex) == serial
                && normalized_dns_name(&record.name).as_deref() == Some(name.as_str())
        });
        if !already_recorded {
            records.push(OwnCertRecord {
                serial_hex: serial.clone(),
                name: name.clone(),
                directory: directory.to_string(),
                issued_unix_ms: now_unix_ms(),
            });
            let mut serialized = serde_json::to_vec_pretty(&records).map_err(|error| {
                crate::access::AccessError(format!("serialize own-certificate ledger: {error}"))
            })?;
            serialized.push(b'\n');
            if serialized.len() as u64 > FLEET_CERT_SERIALS_MAX_BYTES {
                return Err(crate::access::AccessError(
                    "own-certificate ledger would exceed its size limit; no record was changed"
                        .to_string(),
                ));
            }
            crate::access::authority_store::atomic_write_private_locked(
                &serials_path_in(cert_dir),
                &serialized,
            )?;
        }

        let Some(mut durable) =
            load_ct_status_locked_in(cert_dir).map_err(crate::access::AccessError)?
        else {
            return Ok(None);
        };
        if durable.name != name || !durable.foreign_serials.contains(&serial) {
            return Ok(None);
        }
        durable.foreign_serials.retain(|foreign| foreign != &serial);
        durable.unknown_summaries.retain(|summary| {
            !summary
                .strip_prefix(&serial)
                .is_some_and(|suffix| suffix.starts_with(" ·"))
        });
        durable.state = if durable.foreign_serials.is_empty() {
            "ok".to_string()
        } else {
            "alert".to_string()
        };
        write_ct_status_locked_in(cert_dir, &durable)?;
        Ok(Some(durable))
    })
    .map_err(|error| error.to_string())?;
    if let Some(durable) = reconciled {
        with_status(|status| {
            if fleet_name_matches(status.name.as_deref(), &name) {
                let _ = apply_loaded_ct_status(status, Ok(Some(durable)));
            }
        });
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct CtEntry {
    serial_hex: String,
    issuer: String,
    not_before: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DurableCtStatus {
    #[serde(default)]
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    foreign_serials: Vec<String>,
    #[serde(default)]
    unknown_summaries: Vec<String>,
    #[serde(default)]
    checked_unix_ms: Option<u64>,
}

fn ct_status_path_in(cert_dir: &Path) -> PathBuf {
    cert_dir.join(FLEET_CT_STATUS_FILE)
}

#[cfg(test)]
fn durable_ct_status_from_live(status: &FleetCertStatus) -> Result<DurableCtStatus, String> {
    let name = status
        .name
        .as_deref()
        .and_then(normalized_dns_name)
        .ok_or_else(|| "cannot persist a CT verdict without an exact fleet name".to_string())?;
    Ok(DurableCtStatus {
        name,
        state: status.ct_state.clone(),
        foreign_serials: status.ct_foreign_serials.clone(),
        unknown_summaries: status.ct_unknown.clone(),
        checked_unix_ms: status.ct_checked_unix_ms,
    })
}

fn write_ct_status_locked_in(
    cert_dir: &Path,
    durable: &DurableCtStatus,
) -> crate::access::AccessResult<()> {
    let mut serialized = serde_json::to_vec_pretty(durable).map_err(|error| {
        crate::access::AccessError(format!("serialize durable CT status: {error}"))
    })?;
    serialized.push(b'\n');
    if serialized.len() as u64 > FLEET_CT_STATUS_MAX_BYTES {
        return Err(crate::access::AccessError(
            "durable CT status exceeds its size limit".to_string(),
        ));
    }
    crate::access::authority_store::atomic_write_private_locked(
        &ct_status_path_in(cert_dir),
        &serialized,
    )
}

#[cfg(test)]
fn persist_ct_status_in(cert_dir: &Path, status: &FleetCertStatus) -> Result<(), String> {
    let durable = durable_ct_status_from_live(status)?;
    crate::access::authority_store::with_lock(cert_dir, || {
        write_ct_status_locked_in(cert_dir, &durable)
    })
    .map_err(|error| error.to_string())
}

fn load_ct_status_locked_in(cert_dir: &Path) -> Result<Option<DurableCtStatus>, String> {
    use std::io::Read as _;

    let path = ct_status_path_in(cert_dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(FLEET_CT_STATUS_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > FLEET_CT_STATUS_MAX_BYTES {
        return Err(format!(
            "{} exceeds the durable CT status size limit",
            path.display()
        ));
    }
    let mut durable: DurableCtStatus = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    durable.name = normalized_dns_name(&durable.name)
        .ok_or_else(|| format!("{} has no exact fleet name", path.display()))?;
    if durable.foreign_serials.len() > FLEET_CT_FOREIGN_SERIALS_MAX
        || durable.unknown_summaries.len() > FLEET_CT_FOREIGN_SERIALS_MAX
        || durable
            .unknown_summaries
            .iter()
            .any(|summary| summary.len() > 1024)
    {
        return Err(format!(
            "{} exceeds the durable CT evidence bounds",
            path.display()
        ));
    }
    let normalized = durable
        .foreign_serials
        .iter()
        .map(|serial| {
            let normalized = normalize_serial_hex(serial);
            (!normalized.is_empty()
                && normalized.len() <= 128
                && normalized.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(normalized)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| format!("{} contains an invalid certificate serial", path.display()))?;
    durable.foreign_serials = normalized;
    durable.foreign_serials.sort();
    durable.foreign_serials.dedup();
    match durable.state.as_str() {
        "ok" if durable.foreign_serials.is_empty() => {}
        "alert" if !durable.foreign_serials.is_empty() => {}
        _ => {
            return Err(format!(
                "{} contains an inconsistent CT verdict",
                path.display()
            ));
        }
    }
    Ok(Some(durable))
}

fn load_ct_status_in(cert_dir: &Path) -> Result<Option<DurableCtStatus>, String> {
    crate::access::authority_store::with_lock(cert_dir, || {
        load_ct_status_locked_in(cert_dir).map_err(crate::access::AccessError)
    })
    .map_err(|error| error.to_string())
}

fn durable_ct_status_for_name_in(
    cert_dir: &Path,
    name: &str,
) -> Result<Option<DurableCtStatus>, String> {
    let name = normalized_dns_name(name)
        .ok_or_else(|| "cannot load CT evidence for an empty fleet name".to_string())?;
    crate::access::authority_store::with_lock(cert_dir, || {
        // The own-serial ledger is part of the verdict's interpretation. A
        // malformed ledger must suspend every process, not look like no own
        // certificates exist.
        own_serial_records_locked_in(cert_dir).map_err(crate::access::AccessError)?;
        let durable = load_ct_status_locked_in(cert_dir).map_err(crate::access::AccessError)?;
        if let Some(durable) = durable.as_ref() {
            if durable.name != name {
                return Err(crate::access::AccessError(format!(
                    "durable CT verdict belongs to {}, not the current fleet name {name}",
                    durable.name
                )));
            }
        }
        Ok(durable)
    })
    .map_err(|error| error.to_string())
}

fn restore_ct_status_in(cert_dir: &Path) {
    let loaded = load_ct_status_in(cert_dir);
    let mut warning = None;
    with_status(|status| warning = apply_loaded_ct_status(status, loaded));
    if let Some(error) = warning {
        eprintln!("[fleet-cert] durable CT status is unreadable; hosted lane suspended: {error}");
    }
}

fn apply_loaded_ct_status(
    status: &mut FleetCertStatus,
    loaded: Result<Option<DurableCtStatus>, String>,
) -> Option<String> {
    match loaded {
        Ok(Some(durable))
            if status
                .name
                .as_deref()
                .and_then(normalized_dns_name)
                .as_deref()
                == Some(durable.name.as_str()) =>
        {
            status.ct_state = durable.state;
            status.ct_foreign_serials = durable.foreign_serials;
            status.ct_unknown = durable.unknown_summaries;
            status.ct_checked_unix_ms = durable.checked_unix_ms;
            status.ct_last_error = None;
            None
        }
        Ok(Some(durable)) => {
            let current = status
                .name
                .as_deref()
                .and_then(normalized_dns_name)
                .unwrap_or_else(|| "<none>".to_string());
            let error = format!(
                "durable CT verdict belongs to {}, not the current fleet name {current}",
                durable.name
            );
            status.ct_state = "unreadable".to_string();
            status.ct_foreign_serials.clear();
            status.ct_unknown.clear();
            status.ct_last_error = Some(error.clone());
            Some(error)
        }
        Ok(None) => None,
        Err(error) => {
            status.ct_state = "unreadable".to_string();
            status.ct_foreign_serials.clear();
            status.ct_unknown.clear();
            status.ct_last_error = Some(error.clone());
            Some(error)
        }
    }
}

fn extend_ct_response(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if bytes.len().saturating_add(chunk.len()) > FLEET_CT_RESPONSE_MAX_BYTES {
        return Err(format!(
            "crt.sh response exceeds the {} byte limit",
            FLEET_CT_RESPONSE_MAX_BYTES
        ));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
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
#[cfg(test)]
fn foreign_entries(logged: Vec<CtEntry>, own_serials: &[String]) -> Vec<CtEntry> {
    logged
        .into_iter()
        .filter(|entry| !own_serials.contains(&entry.serial_hex))
        .collect()
}

enum CtCommit {
    Applied(DurableCtStatus),
    DeferredForIssuance,
}

fn ct_entry_summary(entry: &CtEntry) -> String {
    format!(
        "{} · {} · {}",
        entry.serial_hex, entry.issuer, entry.not_before
    )
}

fn commit_ct_entries_in(
    cert_dir: &Path,
    name: &str,
    logged: Vec<CtEntry>,
    checked_unix_ms: u64,
) -> Result<CtCommit, String> {
    let name = normalized_dns_name(name)
        .ok_or_else(|| "cannot commit CT evidence for an empty fleet name".to_string())?;
    crate::access::authority_store::with_lock(cert_dir, || {
        let provenance =
            load_fleet_origin_provenance_in(cert_dir).map_err(crate::access::AccessError)?;
        if provenance.name.as_deref() != Some(name.as_str()) {
            return Err(crate::access::AccessError(format!(
                "fleet name changed before CT evidence commit for {name}"
            )));
        }
        if issuance_active_locked_in(cert_dir, &name).map_err(crate::access::AccessError)? {
            return Ok(CtCommit::DeferredForIssuance);
        }

        let own: std::collections::BTreeSet<String> = own_serial_records_locked_in(cert_dir)
            .map_err(crate::access::AccessError)?
            .into_iter()
            .filter(|record| normalized_dns_name(&record.name).as_deref() == Some(name.as_str()))
            .filter_map(|record| {
                let serial = normalize_serial_hex(&record.serial_hex);
                (!serial.is_empty()
                    && serial.len() <= 128
                    && serial.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then_some(serial)
            })
            .collect();
        let mut summaries = std::collections::BTreeMap::<String, String>::new();
        for entry in logged {
            if !own.contains(&entry.serial_hex) {
                summaries.insert(entry.serial_hex.clone(), ct_entry_summary(&entry));
            }
        }

        // A second process may have committed evidence after this process
        // began its public-index fetch. Merge it under the authority lock so
        // an older empty result cannot erase an alert. A serial leaves the
        // set only when the local issuance ledger proves it belongs here.
        match load_ct_status_locked_in(cert_dir) {
            Ok(Some(previous)) if previous.name == name => {
                for serial in previous.foreign_serials {
                    if own.contains(&serial) {
                        continue;
                    }
                    let prior_summary = previous
                        .unknown_summaries
                        .iter()
                        .find(|summary| {
                            summary
                                .strip_prefix(&serial)
                                .is_some_and(|suffix| suffix.starts_with(" ·"))
                        })
                        .cloned()
                        .unwrap_or_else(|| format!("{serial} · previously observed"));
                    summaries.entry(serial).or_insert(prior_summary);
                }
            }
            Ok(_) => {}
            Err(_) => {
                // A complete successful fetch can reconstruct the verdict
                // after a malformed prior file. The bounded response parser
                // has already rejected partial/oversized evidence.
            }
        }
        if summaries.len() > FLEET_CT_FOREIGN_SERIALS_MAX {
            return Err(crate::access::AccessError(
                "CT evidence exceeds the durable foreign-serial limit".to_string(),
            ));
        }
        let foreign_serials = summaries.keys().cloned().collect::<Vec<_>>();
        let unknown_summaries = summaries.into_values().collect::<Vec<_>>();
        let durable = DurableCtStatus {
            name,
            state: if foreign_serials.is_empty() {
                "ok".to_string()
            } else {
                "alert".to_string()
            },
            foreign_serials,
            unknown_summaries,
            checked_unix_ms: Some(checked_unix_ms),
        };
        write_ct_status_locked_in(cert_dir, &durable)?;
        Ok(CtCommit::Applied(durable))
    })
    .map_err(|error| error.to_string())
}

/// One CT check against the public index. Fetch/parse failures set
/// `ct_last_error` and leave the last successful verdict standing.
pub async fn ct_check_once() {
    use futures_util::StreamExt as _;

    let Some(name) = status_snapshot().name else {
        return;
    };
    let certificate_dir = cert_dir();
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
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| error.to_string())?;
            extend_ct_response(&mut bytes, &chunk)?;
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| format!("crt.sh response is not UTF-8: {error}"))?;
        parse_crt_sh_entries(text)
    }
    .await;
    let now = now_unix_ms();
    match result {
        Ok(logged) => match commit_ct_entries_in(&certificate_dir, &name, logged, now) {
            Ok(CtCommit::DeferredForIssuance) => {
                with_status(|status| {
                    if fleet_name_matches(status.name.as_deref(), &name) {
                        status.ct_last_error = Some(
                            "CT verdict deferred while certificate issuance is active".to_string(),
                        );
                    }
                });
            }
            Ok(CtCommit::Applied(durable)) => {
                let alert = durable.state == "alert";
                let summaries = durable.unknown_summaries.clone();
                with_status(|status| {
                    if fleet_name_matches(status.name.as_deref(), &name) {
                        let _ = apply_loaded_ct_status(status, Ok(Some(durable)));
                    }
                });
                if alert {
                    eprintln!(
                            "[fleet-cert] CT ALERT: {} certificate(s) for {name} in the public CT logs \
                             that this daemon never requested: {:?} — if you did not mint these through \
                             another channel, treat the fleet route as compromised and reach this \
                             daemon directly",
                            summaries.len(),
                            summaries,
                        );
                }
            }
            Err(error) => {
                eprintln!("[fleet-cert] commit durable CT status: {error}");
                with_status(|status| {
                    if fleet_name_matches(status.name.as_deref(), &name) {
                        status.ct_state = "unreadable".to_string();
                        status.ct_foreign_serials.clear();
                        status.ct_unknown.clear();
                        status.ct_last_error = Some(format!("commit durable CT status: {error}"));
                    }
                });
            }
        },
        Err(error) => {
            with_status(|status| {
                if fleet_name_matches(status.name.as_deref(), &name) {
                    status.ct_last_error = Some(error);
                }
            });
        }
    }
}

/// Renewal + CT loop: first tick shortly after startup (registration
/// needs a moment to learn the fleet name), then twice daily. Renewal
/// fires inside the last 30 days of validity (Let's Encrypt certificates
/// run 90), and an error restoring or issuing a certificate is retried even
/// when no usable expiry could be recovered. The CT tripwire runs every tick.
/// Spawned once at gateway startup.
fn should_request_certificate(status: &FleetCertStatus, now_unix_ms: u64) -> bool {
    if status.name.is_none() || status.state == "requesting" {
        return false;
    }
    if status.state == "error" {
        return true;
    }
    status.state == "valid"
        && status.not_after_unix_ms.is_some_and(|not_after| {
            not_after.saturating_sub(now_unix_ms) <= 30 * 24 * 60 * 60 * 1000
        })
}

pub fn spawn_renewal_loop() {
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let delay = if first { 10 * 60 } else { 12 * 60 * 60 };
            first = false;
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            ct_check_once().await;
            let status = status_snapshot();
            if !should_request_certificate(&status, now_unix_ms()) {
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

    #[tokio::test]
    async fn malformed_stored_acme_account_is_never_silently_rotated() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("acme-account.json"), b"{").unwrap();
        let error = match acme_account_in(temp.path()).await {
            Ok(_) => panic!("malformed account must not be replaced"),
            Err(error) => error,
        };
        assert!(error.contains("refusing to rotate"), "{error}");
        assert_eq!(
            std::fs::read(temp.path().join("acme-account.json")).unwrap(),
            b"{"
        );
    }

    fn write_legacy_fleet_certificate(cert_dir: &Path, names: &[&str]) {
        let certificate = rcgen::generate_simple_self_signed(
            names
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        std::fs::write(cert_path_in(cert_dir), certificate.cert.pem()).unwrap();
    }

    #[test]
    fn fleet_label_twins_the_service_derivation() {
        // Golden value pinned in bin/connect/dns.rs — change together.
        assert_eq!(
            daemon_fleet_label("example-daemon-id").as_deref(),
            Some("d-30a08371a38c1b447038")
        );
        assert!(daemon_fleet_label(" ").is_none());
    }

    #[test]
    fn fleet_origin_provenance_persists_current_and_replaced_names() {
        let temp = tempfile::tempdir().unwrap();
        let first = remember_fleet_origin_in(
            temp.path(),
            Some("Fleet.Example.Test."),
            "Old.Fleet.Example.Test.",
        )
        .unwrap();
        assert_eq!(first.zone.as_deref(), Some("fleet.example.test"));
        assert_eq!(first.name.as_deref(), Some("old.fleet.example.test"));

        remember_fleet_origin_in(
            temp.path(),
            Some("fleet.example.test"),
            "new.fleet.example.test",
        )
        .unwrap();
        let restored = load_fleet_origin_provenance_in(temp.path()).unwrap();
        assert_eq!(restored.name.as_deref(), Some("new.fleet.example.test"));
        assert_eq!(restored.known_zones, vec!["fleet.example.test".to_string()]);
        assert_eq!(
            restored.known_names,
            vec![
                "new.fleet.example.test".to_string(),
                "old.fleet.example.test".to_string(),
            ]
        );
        assert!(is_service_controlled_name_in(temp.path(), "custom.fleet.example.test").unwrap());
        assert!(is_service_controlled_name_in(temp.path(), "fleet.example.test").unwrap());
        assert!(!is_service_controlled_name_in(temp.path(), "fleet-example.test").unwrap());

        let metadata = std::fs::metadata(fleet_origin_provenance_path_in(temp.path())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        let _ = metadata;
    }

    #[test]
    fn existing_fleet_certificate_backfills_missing_provenance_before_registration() {
        let temp = tempfile::tempdir().unwrap();
        let fleet_name = "legacy-backfill.fleet.example.test";
        write_legacy_fleet_certificate(temp.path(), &[fleet_name]);
        assert!(!fleet_origin_provenance_path_in(temp.path()).exists());

        let restored = restore_fleet_origin_provenance_in(temp.path());
        assert!(restored.warning.is_none(), "{:?}", restored.warning);
        assert!(!restored.provenance.provenance_incomplete);
        assert_eq!(restored.provenance.name.as_deref(), Some(fleet_name));
        assert_eq!(restored.provenance.known_names, vec![fleet_name]);

        let persisted = load_fleet_origin_provenance_in(temp.path()).unwrap();
        assert_eq!(persisted, restored.provenance);
        register_restored_fleet_origins(&restored.provenance);
        assert!(crate::web_tls::is_fleet_server_name(Some(fleet_name)));
    }

    #[test]
    fn unbackfillable_existing_certificate_persists_incomplete_provenance() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(cert_path_in(temp.path()), b"not a certificate").unwrap();

        let restored = restore_fleet_origin_provenance_in(temp.path());
        assert!(restored.provenance.provenance_incomplete);
        assert!(restored.provenance.known_names.is_empty());
        assert!(restored
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("could not be recovered")));

        let persisted = load_fleet_origin_provenance_in(temp.path()).unwrap();
        assert!(persisted.provenance_incomplete);
        assert!(persisted.known_names.is_empty());
    }

    #[test]
    fn malformed_provenance_recovers_current_cert_name_but_stays_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let fleet_name = "malformed-recovery.fleet.example.test";
        write_legacy_fleet_certificate(temp.path(), &[fleet_name]);
        std::fs::write(
            fleet_origin_provenance_path_in(temp.path()),
            b"{ definitely not valid json",
        )
        .unwrap();

        let restored = restore_fleet_origin_provenance_in(temp.path());
        assert!(restored.provenance.provenance_incomplete);
        assert_eq!(restored.provenance.name.as_deref(), Some(fleet_name));
        assert_eq!(restored.provenance.known_names, vec![fleet_name]);
        assert!(restored
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("parse")));

        // Never overwrite the only possible record of older exact names.
        assert!(load_fleet_origin_provenance_in(temp.path()).is_err());
        let restarted = restore_fleet_origin_provenance_in(temp.path());
        assert!(restarted.provenance.provenance_incomplete);
        assert_eq!(restarted.provenance.known_names, vec![fleet_name]);
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
        let cert_pem = std::fs::read_to_string(cert_path_in(&cert_dir())).unwrap();
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
    fn renewal_retries_an_unusable_restored_pair_without_an_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let name = "d-test.fleet.example.test";
        remember_fleet_origin_in(temp.path(), Some("fleet.example.test"), name).unwrap();
        assert!(read_stored_certificate_pair_in(temp.path())
            .unwrap()
            .is_none());
        persist_certificate_pair_in(temp.path(), name, "certificate", "private key").unwrap();
        assert_eq!(
            read_stored_certificate_pair_in(temp.path()).unwrap(),
            Some(("certificate".to_string(), "private key".to_string()))
        );
        std::fs::remove_file(cert_path_in(temp.path())).unwrap();
        assert!(read_stored_certificate_pair_in(temp.path())
            .unwrap_err()
            .contains("pair is incomplete"));

        let repair = FleetCertStatus {
            name: Some(name.to_string()),
            state: "error".to_string(),
            not_after_unix_ms: None,
            ..Default::default()
        };
        assert!(should_request_certificate(&repair, now_unix_ms()));

        let mut unrequested = repair.clone();
        unrequested.state = "none".to_string();
        assert!(!should_request_certificate(&unrequested, now_unix_ms()));

        let mut requesting = repair;
        requesting.state = "requesting".to_string();
        assert!(!should_request_certificate(&requesting, now_unix_ms()));
    }

    #[test]
    fn issuance_intent_is_durable_before_the_first_certificate_pair() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!issuance_requested_in(temp.path()).unwrap());
        mark_issuance_requested_in(temp.path()).unwrap();
        assert!(issuance_requested_in(temp.path()).unwrap());
        assert!(read_stored_certificate_pair_in(temp.path())
            .unwrap()
            .is_none());
        assert!(missing_pair_retry_error_in(temp.path())
            .unwrap()
            .is_some_and(|error| error.contains("renewal will retry")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(issuance_requested_path_in(temp.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn certificate_commit_is_bound_to_the_current_durable_fleet_name() {
        let temp = tempfile::tempdir().unwrap();
        let old_name = "old.fleet.example.test";
        let new_name = "new.fleet.example.test";
        remember_fleet_origin_in(temp.path(), Some("fleet.example.test"), old_name).unwrap();
        persist_certificate_pair_in(temp.path(), old_name, "old certificate", "old key").unwrap();
        remember_fleet_origin_in(temp.path(), Some("fleet.example.test"), new_name).unwrap();

        let error =
            persist_certificate_pair_in(temp.path(), old_name, "stale certificate", "stale key")
                .unwrap_err();
        assert!(error.contains("fleet name changed"), "{error}");
        assert_eq!(
            read_stored_certificate_pair_in(temp.path()).unwrap(),
            Some(("old certificate".to_string(), "old key".to_string()))
        );
    }

    #[test]
    fn fleet_certificate_must_name_the_current_exact_fleet_origin() {
        let cert =
            rcgen::generate_simple_self_signed(vec!["old.fleet.example.test".to_string()]).unwrap();
        assert!(
            require_fleet_certificate_dns_name(&cert.cert.pem(), "old.fleet.example.test").is_ok()
        );
        let error = require_fleet_certificate_dns_name(&cert.cert.pem(), "new.fleet.example.test")
            .unwrap_err();
        assert!(error.contains("current exact name"), "{error}");

        let extra = rcgen::generate_simple_self_signed(vec![
            "old.fleet.example.test".to_string(),
            "custom.example.test".to_string(),
        ])
        .unwrap();
        let error = require_fleet_certificate_dns_name(&extra.cert.pem(), "old.fleet.example.test")
            .unwrap_err();
        assert!(error.contains("must equal only"), "{error}");
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
    fn own_serial_ledger_is_exact_name_canonical_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let records = vec![
            OwnCertRecord {
                serial_hex: "000b".to_string(),
                name: "One.Fleet.Example.Test.".to_string(),
                directory: "test".to_string(),
                issued_unix_ms: 1,
            },
            OwnCertRecord {
                serial_hex: "0A".to_string(),
                name: "one.fleet.example.test".to_string(),
                directory: "test".to_string(),
                issued_unix_ms: 2,
            },
            OwnCertRecord {
                serial_hex: "000b".to_string(),
                name: "one.fleet.example.test".to_string(),
                directory: "test".to_string(),
                issued_unix_ms: 3,
            },
            OwnCertRecord {
                serial_hex: "ff".to_string(),
                name: "two.fleet.example.test".to_string(),
                directory: "test".to_string(),
                issued_unix_ms: 4,
            },
        ];
        std::fs::write(
            serials_path_in(temp.path()),
            serde_json::to_vec(&records).unwrap(),
        )
        .unwrap();

        assert_eq!(
            own_serials_for_name_in(temp.path(), "ONE.fleet.example.test"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            own_serial_ledger_for_name_in(temp.path(), "ONE.fleet.example.test"),
            Ok(Some((vec!["a".to_string(), "b".to_string()], 3)))
        );
        assert_eq!(
            own_serials_for_exact_name_in(temp.path(), "two.fleet.example.test"),
            Ok(vec!["ff".to_string()])
        );
    }

    #[test]
    fn own_serial_ledger_is_stably_bounded_to_the_newest_issuances() {
        let temp = tempfile::tempdir().unwrap();
        let count = crate::access::hosted_control::HOSTED_CERTIFICATE_LEDGER_SERIALS_CAP + 3;
        let records: Vec<OwnCertRecord> = (0..count)
            .map(|index| OwnCertRecord {
                serial_hex: format!("{:x}", index + 1),
                name: "one.fleet.example.test".to_string(),
                directory: "test".to_string(),
                issued_unix_ms: (index + 1) as u64,
            })
            .collect();
        std::fs::write(
            serials_path_in(temp.path()),
            serde_json::to_vec(&records).unwrap(),
        )
        .unwrap();

        let (serials, issued_unix_ms) =
            own_serial_ledger_for_name_in(temp.path(), "one.fleet.example.test")
                .unwrap()
                .unwrap();
        assert_eq!(
            serials.len(),
            crate::access::hosted_control::HOSTED_CERTIFICATE_LEDGER_SERIALS_CAP
        );
        assert_eq!(issued_unix_ms, count as u64);
        assert!(!serials.contains(&"1".to_string()));
        assert!(!serials.contains(&"2".to_string()));
        assert!(!serials.contains(&"3".to_string()));
    }

    #[test]
    fn concurrent_own_certificate_records_do_not_overwrite_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let name = "one.fleet.example.test";
        let certificate = |serial: u8| {
            let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
            params.serial_number = Some(rcgen::SerialNumber::from(vec![serial]));
            let key = rcgen::KeyPair::generate().unwrap();
            params.self_signed(&key).unwrap().pem()
        };
        let first = certificate(1);
        let second = certificate(2);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|certificate| {
                let cert_dir = temp.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    record_own_certificate_in(&cert_dir, &certificate, name, "test")
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(
            own_serials_for_exact_name_in(temp.path(), name).unwrap(),
            vec!["1".to_string(), "2".to_string()]
        );
    }

    #[test]
    fn malformed_own_certificate_ledger_is_not_treated_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(serials_path_in(temp.path()), b"{").unwrap();
        assert!(
            own_serials_for_exact_name_in(temp.path(), "one.fleet.example.test")
                .unwrap_err()
                .contains("parse")
        );
        assert!(
            own_serial_ledger_for_name_in(temp.path(), "one.fleet.example.test")
                .unwrap_err()
                .contains("parse")
        );
    }

    #[test]
    fn successful_ct_verdict_round_trips_through_the_durable_store() {
        let temp = tempfile::tempdir().unwrap();
        let status = FleetCertStatus {
            name: Some("one.fleet.example.test".to_string()),
            ct_state: "alert".to_string(),
            ct_unknown: vec!["b · issuer · time".to_string()],
            ct_foreign_serials: vec!["000b".to_string(), "0A".to_string()],
            ct_checked_unix_ms: Some(42),
            ..Default::default()
        };
        persist_ct_status_in(temp.path(), &status).unwrap();

        assert_eq!(
            load_ct_status_in(temp.path()),
            Ok(Some(DurableCtStatus {
                name: "one.fleet.example.test".to_string(),
                state: "alert".to_string(),
                foreign_serials: vec!["a".to_string(), "b".to_string()],
                unknown_summaries: vec!["b · issuer · time".to_string()],
                checked_unix_ms: Some(42),
            }))
        );
    }

    #[test]
    fn malformed_existing_ct_verdict_suspends_until_a_successful_check() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(ct_status_path_in(temp.path()), b"{").unwrap();

        let mut status = FleetCertStatus::default();
        let warning = apply_loaded_ct_status(&mut status, load_ct_status_in(temp.path()));
        assert!(warning.unwrap().contains("parse"));
        let evidence = ct_guard_evidence_from_status(&status);
        assert!(evidence.state_unavailable);
        assert!(evidence.foreign_serials.is_empty());
    }

    #[test]
    fn ct_verdict_is_bound_to_the_exact_fleet_name() {
        let temp = tempfile::tempdir().unwrap();
        let old_name = "old.fleet.example.test";
        let new_name = "new.fleet.example.test";
        persist_ct_status_in(
            temp.path(),
            &FleetCertStatus {
                name: Some(old_name.to_string()),
                ct_state: "ok".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut status = FleetCertStatus {
            name: Some(new_name.to_string()),
            ..Default::default()
        };
        let warning = apply_loaded_ct_status(&mut status, load_ct_status_in(temp.path()));
        assert!(warning
            .as_deref()
            .is_some_and(|warning| warning.contains(old_name) && warning.contains(new_name)));
        assert_eq!(status.ct_state, "unreadable");
    }

    #[test]
    fn ct_commits_merge_foreign_evidence_until_the_serial_is_recorded_as_own() {
        let temp = tempfile::tempdir().unwrap();
        let name = "one.fleet.example.test";
        remember_fleet_origin_in(temp.path(), Some("fleet.example.test"), name).unwrap();
        let first = commit_ct_entries_in(
            temp.path(),
            name,
            vec![CtEntry {
                serial_hex: "a".to_string(),
                issuer: "issuer".to_string(),
                not_before: "time".to_string(),
            }],
            1,
        )
        .unwrap();
        assert!(matches!(first, CtCommit::Applied(_)));
        let stale_empty = commit_ct_entries_in(temp.path(), name, Vec::new(), 2).unwrap();
        let CtCommit::Applied(stale_empty) = stale_empty else {
            panic!("no issuance is active");
        };
        assert_eq!(stale_empty.foreign_serials, vec!["a".to_string()]);

        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        params.serial_number = Some(rcgen::SerialNumber::from(vec![0x0a]));
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        record_own_certificate_in(temp.path(), &certificate.pem(), name, "test").unwrap();
        let reconciled = load_ct_status_in(temp.path()).unwrap().unwrap();
        assert_eq!(reconciled.state, "ok");
        assert!(reconciled.foreign_serials.is_empty());
    }

    #[test]
    fn ct_commit_defers_while_a_durable_issuance_is_active() {
        let temp = tempfile::tempdir().unwrap();
        let name = "one.fleet.example.test";
        remember_fleet_origin_in(temp.path(), Some("fleet.example.test"), name).unwrap();
        let token = begin_issuance_in(temp.path(), name).unwrap();
        assert!(matches!(
            commit_ct_entries_in(
                temp.path(),
                name,
                vec![CtEntry {
                    serial_hex: "a".to_string(),
                    issuer: "issuer".to_string(),
                    not_before: "time".to_string(),
                }],
                1,
            )
            .unwrap(),
            CtCommit::DeferredForIssuance
        ));
        finish_issuance_in(temp.path(), &token).unwrap();
        assert!(matches!(
            commit_ct_entries_in(temp.path(), name, Vec::new(), 2).unwrap(),
            CtCommit::Applied(_)
        ));
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

    #[test]
    fn crt_sh_response_accumulation_is_bounded() {
        let mut bytes = vec![0; FLEET_CT_RESPONSE_MAX_BYTES];
        assert!(extend_ct_response(&mut bytes, &[1])
            .unwrap_err()
            .contains("byte limit"));
        assert_eq!(bytes.len(), FLEET_CT_RESPONSE_MAX_BYTES);
    }
}
