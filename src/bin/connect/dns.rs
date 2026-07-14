//! Embedded authoritative DNS for the delegated fleet subzone
//! (docs/src/self-hosted-rendezvous.md; docs/src/trust-tiers.md).
//!
//! The convenient-direct-path design: the parent zone NS-delegates one
//! subzone (e.g. `fleet.intendant.dev`) to this service, which answers
//! for exactly that subzone and nothing else — the DNS twin of the
//! separation from the daemon authority mint. Records served:
//!
//! - apex `SOA` / `NS` (this server, via the parent-zone glue name);
//! - `d-<daemon-id>.<zone>` `A`/`AAAA` — addresses a registered daemon
//!   published for itself (LAN addresses are legitimate: public name +
//!   real certificate + private address is the whole point);
//! - `_acme-challenge.d-<daemon-id>.<zone>` `TXT` — short-lived ACME
//!   DNS-01 tokens a daemon published while minting its certificate.
//!
//! Posture: authoritative-only (no recursion), `Refused` outside the
//! zone, AXFR/IXFR refused, `ANY` answered minimally per RFC 8482,
//! low TTLs (daemon addresses move). Publishing is authenticated at the
//! HTTP layer (daemon-signed requests against the registered identity
//! key — the same freshness pattern as unclaim); this module only
//! serves what the store hands it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;

use hickory_server::net::runtime::Time;
use hickory_server::proto::op::{
    Header, HeaderCounts, MessageType, Metadata, OpCode, ResponseCode,
};
use hickory_server::proto::rr::rdata::{HINFO, NS, SOA, TXT};
use hickory_server::proto::rr::{Name, RData, Record, RecordType};
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::MessageResponseBuilder;

/// Answer TTL for daemon records — addresses move (laptops roam, boxes
/// get replaced), so resolvers must not cache long.
const RECORD_TTL: u32 = 60;
/// Apex SOA/NS TTL.
const APEX_TTL: u32 = 300;
/// How long a published ACME TXT stays served before it self-expires.
/// Let's Encrypt validates within seconds of being told to; ten minutes
/// covers retries without leaving stale tokens around.
pub const ACME_TXT_TTL_MS: u64 = 10 * 60 * 1000;

/// The label under the zone for a daemon id: `d-<hex(sha256(id))[..20]>`.
///
/// Derived, not verbatim: daemon ids default to the base64url public key
/// (43 chars, may contain `_`) or are operator-chosen free text — neither
/// is a valid DNS label. A truncated hash is DNS-safe, deterministic on
/// BOTH sides (the daemon derives its own name offline — twin-pinned in
/// `bin/caller/fleet_cert.rs`), and opaque: these names end up in public
/// CT logs, so nothing meaningful may ride in them. 80 bits keeps
/// accidental collisions out of reach at any plausible fleet size.
pub fn daemon_label(daemon_id: &str) -> Option<String> {
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

#[derive(Default)]
struct ZoneData {
    /// label (`d-<id>`) → published addresses.
    v4: HashMap<String, Vec<Ipv4Addr>>,
    v6: HashMap<String, Vec<Ipv6Addr>>,
    /// label (`d-<id>`) → active ACME TXT values with their expiry.
    acme_txt: HashMap<String, Vec<(String, u64)>>,
}

/// The zone: origin, glue NS name, and the dynamic record table.
pub struct FleetZone {
    origin: Name,
    ns_name: Name,
    serial: AtomicU32,
    data: RwLock<ZoneData>,
}

impl FleetZone {
    pub fn new(zone: &str, ns_name: &str) -> Result<Self, String> {
        let mut origin = Name::from_utf8(zone)
            .map_err(|e| format!("invalid dns zone {zone:?}: {e}"))?
            .to_lowercase();
        // Config values arrive without the trailing dot; queries arrive
        // fully qualified — normalize so apex equality holds.
        origin.set_fqdn(true);
        let mut ns_name = Name::from_utf8(ns_name)
            .map_err(|e| format!("invalid dns ns name {ns_name:?}: {e}"))?
            .to_lowercase();
        ns_name.set_fqdn(true);
        if origin.is_root() || origin.num_labels() < 2 {
            return Err(format!("dns zone {zone:?} is too broad to serve"));
        }
        Ok(Self {
            origin,
            ns_name,
            serial: AtomicU32::new(1),
            data: RwLock::new(ZoneData::default()),
        })
    }

    pub fn origin_utf8(&self) -> String {
        self.origin.to_utf8().trim_end_matches('.').to_string()
    }

    /// The fully qualified name a daemon id resolves under, or None for
    /// an id that cannot be a DNS label.
    pub fn daemon_fqdn(&self, daemon_id: &str) -> Option<String> {
        Some(format!(
            "{}.{}",
            daemon_label(daemon_id)?,
            self.origin_utf8()
        ))
    }

    fn bump_serial(&self) {
        self.serial.fetch_add(1, Ordering::Relaxed);
    }

    /// Replace the published addresses for a daemon (empty = remove).
    pub fn set_daemon_addresses(
        &self,
        daemon_id: &str,
        addresses: &[IpAddr],
    ) -> Result<(), String> {
        let label = daemon_label(daemon_id).ok_or("daemon id is not a usable DNS label")?;
        let mut data = self.data.write().expect("fleet zone poisoned");
        let v4: Vec<Ipv4Addr> = addresses
            .iter()
            .filter_map(|ip| match ip {
                IpAddr::V4(v4) => Some(*v4),
                IpAddr::V6(_) => None,
            })
            .collect();
        let v6: Vec<Ipv6Addr> = addresses
            .iter()
            .filter_map(|ip| match ip {
                IpAddr::V6(v6) => Some(*v6),
                IpAddr::V4(_) => None,
            })
            .collect();
        if v4.is_empty() {
            data.v4.remove(&label);
        } else {
            data.v4.insert(label.clone(), v4);
        }
        if v6.is_empty() {
            data.v6.remove(&label);
        } else {
            data.v6.insert(label, v6);
        }
        drop(data);
        self.bump_serial();
        Ok(())
    }

    /// Drop every record for a daemon (release/sweep lifecycle).
    pub fn remove_daemon(&self, daemon_id: &str) {
        let Some(label) = daemon_label(daemon_id) else {
            return;
        };
        let mut data = self.data.write().expect("fleet zone poisoned");
        let removed = data.v4.remove(&label).is_some()
            | data.v6.remove(&label).is_some()
            | data.acme_txt.remove(&label).is_some();
        drop(data);
        if removed {
            self.bump_serial();
        }
    }

    /// Publish an ACME DNS-01 TXT value for a daemon (additive within
    /// the expiry window — Let's Encrypt may probe multiple values
    /// during retries; expired ones are swept on read and write).
    pub fn set_acme_txt(
        &self,
        daemon_id: &str,
        value: &str,
        now_unix_ms: u64,
    ) -> Result<(), String> {
        let label = daemon_label(daemon_id).ok_or("daemon id is not a usable DNS label")?;
        let value = value.trim();
        if value.is_empty() || value.len() > 255 {
            return Err("acme txt value must be 1..=255 bytes".to_string());
        }
        let mut data = self.data.write().expect("fleet zone poisoned");
        let entries = data.acme_txt.entry(label).or_default();
        entries.retain(|(existing, expires)| *expires > now_unix_ms && existing != value);
        entries.push((value.to_string(), now_unix_ms + ACME_TXT_TTL_MS));
        drop(data);
        self.bump_serial();
        Ok(())
    }

    /// Remove a daemon's ACME TXT values (the daemon finished its order).
    pub fn clear_acme_txt(&self, daemon_id: &str) {
        let Some(label) = daemon_label(daemon_id) else {
            return;
        };
        let mut data = self.data.write().expect("fleet zone poisoned");
        let removed = data.acme_txt.remove(&label).is_some();
        drop(data);
        if removed {
            self.bump_serial();
        }
    }

    fn soa_record(&self) -> Record {
        let soa = SOA::new(
            self.ns_name.clone(),
            Name::from_utf8(format!("hostmaster.{}", self.origin.to_utf8()))
                .unwrap_or_else(|_| self.ns_name.clone()),
            self.serial.load(Ordering::Relaxed),
            1200,    // refresh
            300,     // retry
            604_800, // expire
            60,      // negative-answer TTL
        );
        Record::from_rdata(self.origin.clone(), APEX_TTL, RData::SOA(soa))
    }

    fn ns_record(&self) -> Record {
        Record::from_rdata(
            self.origin.clone(),
            APEX_TTL,
            RData::NS(NS(self.ns_name.clone())),
        )
    }

    /// Answer a query for `qname`/`qtype`. `None` = the name is outside
    /// this zone (REFUSED); `Some(answer)` carries the records plus
    /// whether the name exists at all (NXDOMAIN vs NODATA).
    fn lookup(&self, qname: &Name, qtype: RecordType, now_unix_ms: u64) -> Option<ZoneAnswer> {
        let qname = qname.to_lowercase();
        if !self.origin.zone_of(&qname) {
            return None;
        }
        let mut answer = ZoneAnswer::default();

        if qname == self.origin {
            answer.name_exists = true;
            match qtype {
                RecordType::SOA => answer.records.push(self.soa_record()),
                RecordType::NS => answer.records.push(self.ns_record()),
                RecordType::ANY => {
                    // RFC 8482: a minimal single answer instead of "all".
                    answer.records.push(Record::from_rdata(
                        self.origin.clone(),
                        APEX_TTL,
                        RData::HINFO(HINFO::new("RFC8482".to_string(), String::new())),
                    ));
                }
                _ => {}
            }
            return Some(answer);
        }

        // Exactly one label under the zone: `d-<id>`, or two labels for
        // `_acme-challenge.d-<id>`. Anything deeper does not exist.
        let origin_labels = usize::from(self.origin.num_labels());
        let relative_labels = usize::from(qname.num_labels()).saturating_sub(origin_labels);
        let data = self.data.read().expect("fleet zone poisoned");
        match relative_labels {
            1 => {
                let label = label_at(&qname, origin_labels, 0);
                let v4 = data.v4.get(&label);
                let v6 = data.v6.get(&label);
                let has_txt = data
                    .acme_txt
                    .get(&label)
                    .map(|entries| entries.iter().any(|(_, expires)| *expires > now_unix_ms))
                    .unwrap_or(false);
                answer.name_exists = v4.is_some() || v6.is_some() || has_txt;
                match qtype {
                    RecordType::A => {
                        for ip in v4.into_iter().flatten() {
                            answer.records.push(Record::from_rdata(
                                qname.clone(),
                                RECORD_TTL,
                                RData::A((*ip).into()),
                            ));
                        }
                    }
                    RecordType::AAAA => {
                        for ip in v6.into_iter().flatten() {
                            answer.records.push(Record::from_rdata(
                                qname.clone(),
                                RECORD_TTL,
                                RData::AAAA((*ip).into()),
                            ));
                        }
                    }
                    RecordType::ANY if answer.name_exists => {
                        answer.records.push(Record::from_rdata(
                            qname.clone(),
                            RECORD_TTL,
                            RData::HINFO(HINFO::new("RFC8482".to_string(), String::new())),
                        ));
                    }
                    _ => {}
                }
            }
            2 => {
                let challenge = label_at(&qname, origin_labels, 1);
                let label = label_at(&qname, origin_labels, 0);
                if challenge == "_acme-challenge" {
                    if let Some(entries) = data.acme_txt.get(&label) {
                        let live: Vec<&String> = entries
                            .iter()
                            .filter(|(_, expires)| *expires > now_unix_ms)
                            .map(|(value, _)| value)
                            .collect();
                        answer.name_exists = !live.is_empty();
                        if qtype == RecordType::TXT {
                            for value in live {
                                answer.records.push(Record::from_rdata(
                                    qname.clone(),
                                    RECORD_TTL,
                                    RData::TXT(TXT::new(vec![value.clone()])),
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Some(answer)
    }
}

/// Label helper for zone-relative names (labels iterate leftmost-first):
/// `from_zone` counts up from the zone boundary, so for
/// `_acme-challenge.d-x.<zone>` with `origin_labels` = the zone's label
/// count, `from_zone = 0` is `d-x` and `from_zone = 1` is
/// `_acme-challenge`.
fn label_at(name: &Name, origin_labels: usize, from_zone: usize) -> String {
    let labels: Vec<String> = name
        .iter()
        .map(|l| String::from_utf8_lossy(l).to_ascii_lowercase())
        .collect();
    let relative = labels.len().saturating_sub(origin_labels);
    match relative.checked_sub(1 + from_zone) {
        Some(index) => labels.get(index).cloned().unwrap_or_default(),
        None => String::new(),
    }
}

#[derive(Default)]
struct ZoneAnswer {
    records: Vec<Record>,
    name_exists: bool,
}

/// The hickory request handler: answers for the zone, refuses the rest.
pub struct FleetZoneHandler {
    zone: std::sync::Arc<FleetZone>,
}

impl FleetZoneHandler {
    pub fn new(zone: std::sync::Arc<FleetZone>) -> Self {
        Self { zone }
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A `ResponseInfo` for the failed-to-send path (the crate-internal
/// `ResponseInfo::serve_failed` is not public).
fn send_failed_info(request_metadata: &Metadata) -> ResponseInfo {
    let mut metadata = Metadata::response_from_request(request_metadata);
    metadata.response_code = ResponseCode::ServFail;
    Header {
        metadata,
        counts: HeaderCounts::default(),
    }
    .into()
}

#[async_trait::async_trait]
impl RequestHandler for FleetZoneHandler {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &Request,
        mut response_handle: R,
    ) -> ResponseInfo {
        // `Request` derefs to the decoded MessageRequest: `metadata`,
        // `queries`, and `edns` are direct field reads.
        let error_code = 'error: {
            if request.metadata.op_code != OpCode::Query
                || request.metadata.message_type != MessageType::Query
            {
                break 'error Some(ResponseCode::Refused);
            }
            let Ok(info) = request.request_info() else {
                break 'error Some(ResponseCode::FormErr);
            };
            if matches!(info.query.query_type(), RecordType::AXFR | RecordType::IXFR) {
                break 'error Some(ResponseCode::Refused);
            }
            None
        };
        if let Some(code) = error_code {
            let response = MessageResponseBuilder::new(&request.queries, None)
                .error_msg(&request.metadata, code);
            return match response_handle.send_response(response).await {
                Ok(info) => info,
                Err(_) => send_failed_info(&request.metadata),
            };
        }

        // Infallible now — checked above.
        let info = match request.request_info() {
            Ok(info) => info,
            Err(_) => return send_failed_info(&request.metadata),
        };
        let qtype = info.query.query_type();
        let qname = Name::from(info.query.name().clone());
        let Some(answer) = self.zone.lookup(&qname, qtype, now_unix_ms()) else {
            // Outside the zone: authoritative-only servers refuse.
            let response = MessageResponseBuilder::new(&request.queries, None)
                .error_msg(&request.metadata, ResponseCode::Refused);
            return match response_handle.send_response(response).await {
                Ok(info) => info,
                Err(_) => send_failed_info(&request.metadata),
            };
        };

        let mut metadata = Metadata::response_from_request(&request.metadata);
        metadata.authoritative = true;
        metadata.recursion_available = false;
        // NODATA (name exists) and NXDOMAIN both carry the SOA in the
        // authority section for negative caching.
        let soa_records: Vec<Record> = if answer.records.is_empty() {
            if !answer.name_exists {
                metadata.response_code = ResponseCode::NXDomain;
            }
            vec![self.zone.soa_record()]
        } else {
            Vec::new()
        };
        let response = MessageResponseBuilder::new(&request.queries, None).build(
            metadata,
            answer.records.iter(),
            std::iter::empty(),
            soa_records.iter(),
            std::iter::empty(),
        );
        match response_handle.send_response(response).await {
            Ok(info) => info,
            Err(_) => send_failed_info(&request.metadata),
        }
    }
}

/// Bind the zone's UDP + TCP sockets on `listen` and return the serving
/// future. Binding is eager so a misconfigured listener (privileges,
/// port in use) fails startup loudly; the returned future is spawned
/// and runs until process exit like the other connect background tasks.
pub async fn bind_fleet_dns(
    zone: std::sync::Arc<FleetZone>,
    listen: std::net::SocketAddr,
) -> Result<impl std::future::Future<Output = Result<(), String>>, String> {
    let udp = tokio::net::UdpSocket::bind(listen)
        .await
        .map_err(|e| format!("bind dns udp {listen}: {e}"))?;
    let tcp = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("bind dns tcp {listen}: {e}"))?;
    let mut server = hickory_server::server::Server::new(FleetZoneHandler::new(zone));
    server.register_socket(udp);
    // 5s per-connection timeout; responses are tiny (a handful of
    // records), so a small per-connection output buffer suffices.
    server.register_listener(tcp, std::time::Duration::from_secs(5), 4096);
    Ok(async move {
        server
            .block_until_done()
            .await
            .map_err(|e| format!("dns server exited: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone() -> FleetZone {
        FleetZone::new("fleet.example.test", "ns-fleet.example.test").unwrap()
    }

    fn name(value: &str) -> Name {
        Name::from_utf8(value).unwrap()
    }

    #[test]
    fn daemon_labels_are_derived_dns_safe_and_opaque() {
        // Golden value — the daemon derives the same label offline
        // (bin/caller/fleet_cert.rs twin test); change them together.
        assert_eq!(
            daemon_label("example-daemon-id").as_deref(),
            Some("d-30a08371a38c1b447038")
        );
        // A default daemon id (43-char base64url public key, `_` and all)
        // still derives a valid label.
        let label = daemon_label("j8Xn_qUvT-43charBase64urlPublicKeyExample__").unwrap();
        assert!(label.starts_with("d-"));
        assert_eq!(label.len(), 22);
        assert!(label
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '-'));
        // Deterministic; whitespace-insensitive; empty refused.
        assert_eq!(daemon_label(" x "), daemon_label("x"));
        assert!(daemon_label("").is_none());
        assert!(daemon_label("   ").is_none());
        assert_ne!(daemon_label("a"), daemon_label("b"));
    }

    #[test]
    fn lookup_serves_apex_daemon_and_acme_records() {
        let zone = zone();
        zone.set_daemon_addresses(
            "abc123",
            &[
                "192.168.1.50".parse().unwrap(),
                "2001:db8::7".parse().unwrap(),
            ],
        )
        .unwrap();
        zone.set_acme_txt("abc123", "tok-en_VALUE", 1_000).unwrap();

        // Apex SOA + NS.
        let soa = zone
            .lookup(&name("fleet.example.test."), RecordType::SOA, 1_000)
            .unwrap();
        assert_eq!(soa.records.len(), 1);
        let ns = zone
            .lookup(&name("fleet.example.test."), RecordType::NS, 1_000)
            .unwrap();
        assert_eq!(ns.records.len(), 1);

        // Daemon A (LAN addresses are legitimate) + AAAA.
        let a = zone
            .lookup(
                &name("d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::A,
                1_000,
            )
            .unwrap();
        assert!(a.name_exists);
        assert_eq!(a.records.len(), 1);
        let aaaa = zone
            .lookup(
                &name("d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::AAAA,
                1_000,
            )
            .unwrap();
        assert_eq!(aaaa.records.len(), 1);

        // ACME TXT, case-insensitive owner, expiring.
        let txt = zone
            .lookup(
                &name("_ACME-CHALLENGE.D-6CA13D52CA70C883E0F0.fleet.example.test."),
                RecordType::TXT,
                1_000,
            )
            .unwrap();
        assert!(txt.name_exists);
        assert_eq!(txt.records.len(), 1);
        let expired = zone
            .lookup(
                &name("_acme-challenge.d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::TXT,
                1_000 + ACME_TXT_TTL_MS + 1,
            )
            .unwrap();
        assert!(!expired.name_exists);
        assert!(expired.records.is_empty());
    }

    #[test]
    fn lookup_distinguishes_nxdomain_nodata_and_out_of_zone() {
        let zone = zone();
        zone.set_daemon_addresses("abc123", &["10.0.0.9".parse().unwrap()])
            .unwrap();

        // Unknown name: NXDOMAIN.
        let missing = zone
            .lookup(&name("d-nope.fleet.example.test."), RecordType::A, 0)
            .unwrap();
        assert!(!missing.name_exists);
        assert!(missing.records.is_empty());

        // Known name, absent type: NODATA.
        let nodata = zone
            .lookup(
                &name("d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::AAAA,
                0,
            )
            .unwrap();
        assert!(nodata.name_exists);
        assert!(nodata.records.is_empty());

        // Too-deep names do not exist.
        let deep = zone
            .lookup(
                &name("x.y.d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::A,
                0,
            )
            .unwrap();
        assert!(!deep.name_exists);

        // Outside the zone: refused (None).
        assert!(zone
            .lookup(&name("example.test."), RecordType::A, 0)
            .is_none());
        assert!(zone
            .lookup(&name("evil.example.com."), RecordType::A, 0)
            .is_none());
    }

    #[test]
    fn lifecycle_updates_replace_and_remove() {
        let zone = zone();
        zone.set_daemon_addresses("abc123", &["10.0.0.9".parse().unwrap()])
            .unwrap();
        zone.set_daemon_addresses("abc123", &["10.0.0.10".parse().unwrap()])
            .unwrap();
        let a = zone
            .lookup(
                &name("d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::A,
                0,
            )
            .unwrap();
        assert_eq!(a.records.len(), 1);

        zone.set_acme_txt("abc123", "tok1", 0).unwrap();
        zone.set_acme_txt("abc123", "tok2", 0).unwrap();
        let txt = zone
            .lookup(
                &name("_acme-challenge.d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::TXT,
                1,
            )
            .unwrap();
        assert_eq!(txt.records.len(), 2);
        zone.clear_acme_txt("abc123");
        let cleared = zone
            .lookup(
                &name("_acme-challenge.d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::TXT,
                1,
            )
            .unwrap();
        assert!(cleared.records.is_empty());

        zone.remove_daemon("abc123");
        let gone = zone
            .lookup(
                &name("d-6ca13d52ca70c883e0f0.fleet.example.test."),
                RecordType::A,
                0,
            )
            .unwrap();
        assert!(!gone.name_exists);
    }
}
