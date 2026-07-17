use std::collections::BTreeSet;

use crate::access::iam::{self, AccessPrincipal, LocalIamState};
use crate::access::{AccessError, AccessResult};
use crate::daemon_identity::{b64u, verify_b64u};

use super::runtime::{
    now_ms, push_audit, validate_fleet_origin, verify_p256_signature,
    ELIGIBLE_SIGNED_APP_DISTRIBUTIONS,
};
use super::*;

const WITNESS_MAX_SKEW_MS: u64 = 10 * 60 * 1000;

impl HostedControlRuntime {
    pub fn certificate_ledger(&self) -> Result<HostedCertificateLedger, String> {
        self.ensure_enabled()?;
        let identity = self.identity()?;
        let name = crate::fleet_cert::current_fleet_name_in(&self.cert_dir)?
            .ok_or_else(|| "fleet certificate name is unavailable".to_string())?;
        let fleet_origin = validate_fleet_origin(&format!("https://{name}"))?;
        let (serials, issued_unix_ms) =
            crate::fleet_cert::own_serial_ledger_for_name_in(&self.cert_dir, &name)
                .ok_or_else(|| "fleet certificate ledger has no issued certificate".to_string())?;
        let mut ledger = HostedCertificateLedger {
            protocol: CERTIFICATE_LEDGER_PROTOCOL.to_string(),
            daemon_id: self.daemon_id.clone(),
            daemon_public_key: identity.public_key_b64u(),
            fleet_origin,
            serials,
            issued_unix_ms,
            signature: String::new(),
        };
        ledger.signature = identity.sign_b64u(ledger.unsigned_payload().as_bytes());
        Ok(ledger)
    }

    pub fn lane_guard_snapshot(&self) -> AccessResult<HostedLaneGuardSnapshot> {
        let state = iam::load_state_cached_arc(&self.cert_dir)?;
        Ok(compute_lane_guard(
            &state,
            crate::fleet_cert::ct_foreign_serials(),
        ))
    }

    pub fn ensure_lane_available(&self) -> Result<HostedLaneGuardSnapshot, String> {
        let guard = self
            .lane_guard_snapshot()
            .map_err(|error| format!("load hosted certificate guard: {error}"))?;
        if guard.status == HostedLaneGuardStatus::Suspended {
            return Err("hosted control is suspended by the certificate guard".to_string());
        }
        Ok(guard)
    }

    pub fn receive_peer_witness(
        &self,
        report: HostedCertificateWitnessReport,
        peer_fingerprint: &str,
        peer_label: &str,
    ) -> Result<HostedLaneGuardSnapshot, String> {
        self.ensure_enabled()?;
        if report.observer_kind != HostedWitnessKind::Peer {
            return Err("peer transport accepts only peer witness reports".to_string());
        }
        verify_witness_report_shape(self, &report)?;
        if !verify_b64u(
            &report.observer_public_key,
            report.unsigned_payload().as_bytes(),
            &report.signature,
        ) {
            return Err("peer witness signature is invalid".to_string());
        }
        let fingerprint = crate::peer::access_policy::normalize_fingerprint(peer_fingerprint)
            .map_err(|_| "peer witness connection has no verified fingerprint".to_string())?;
        self.record_witness(
            report,
            format!("peer:{fingerprint}"),
            bounded_witness_label(peer_label, "peer"),
            None,
        )
    }

    pub fn build_peer_witness_report(
        &self,
        ledger: &HostedCertificateLedger,
        observed_serial_hex: &str,
        vantage: HostedWitnessVantage,
    ) -> Result<HostedCertificateWitnessReport, String> {
        self.ensure_enabled()?;
        verify_certificate_ledger(ledger)?;
        let identity = self.identity()?;
        let mut report = HostedCertificateWitnessReport {
            protocol: CERTIFICATE_WITNESS_PROTOCOL.to_string(),
            report_id: uuid::Uuid::new_v4().to_string(),
            observer_kind: HostedWitnessKind::Peer,
            observer_id: self.daemon_id.clone(),
            observer_public_key: identity.public_key_b64u(),
            target_daemon_id: ledger.daemon_id.clone(),
            fleet_origin: ledger.fleet_origin.clone(),
            ledger_sha256: ledger.document_sha256(),
            observed_serial_hex: normalized_serial(observed_serial_hex)?,
            vantage,
            observed_unix_ms: now_ms().max(0) as u64,
            signature: String::new(),
        };
        report.signature = identity.sign_b64u(report.unsigned_payload().as_bytes());
        Ok(report)
    }

    pub fn receive_signed_app_witness(
        &self,
        report: HostedCertificateWitnessReport,
    ) -> Result<HostedLaneGuardSnapshot, String> {
        self.ensure_enabled()?;
        if report.observer_kind != HostedWitnessKind::SignedApp {
            return Err("signed-application endpoint accepts only app witness reports".to_string());
        }
        if ELIGIBLE_SIGNED_APP_DISTRIBUTIONS.is_empty() {
            return Err(
                "no qualifying signed application distribution is enabled in this build"
                    .to_string(),
            );
        }
        verify_witness_report_shape(self, &report)?;
        verify_p256_signature(
            &report.observer_public_key,
            report.unsigned_payload().as_bytes(),
            &report.signature,
        )?;
        self.record_witness(
            report.clone(),
            format!("app:{}", report.observer_id),
            bounded_witness_label(&report.observer_id, "signed app"),
            Some(report.observer_id),
        )
    }

    fn record_witness(
        &self,
        report: HostedCertificateWitnessReport,
        observer_binding: String,
        observer_label: String,
        signed_app_device_id: Option<String>,
    ) -> Result<HostedLaneGuardSnapshot, String> {
        let ledger = self.certificate_ledger()?;
        if ledger.serials.contains(&report.observed_serial_hex) {
            return self
                .lane_guard_snapshot()
                .map_err(|error| format!("load hosted certificate guard: {error}"));
        }
        let ct_serials = crate::fleet_cert::ct_foreign_serials();
        iam::transact_state(&self.cert_dir, |state, _| {
            if let Some(device_id) = signed_app_device_id.as_deref() {
                let anchor = state
                    .hosted_control
                    .signed_app_anchors
                    .iter()
                    .find(|anchor| {
                        anchor.device_id == device_id
                            && anchor.active
                            && anchor.revoked_unix_ms.is_none()
                    })
                    .ok_or_else(|| {
                        AccessError("signed application witness is not accepted".to_string())
                    })?;
                if anchor.public_key != report.observer_public_key
                    || !ELIGIBLE_SIGNED_APP_DISTRIBUTIONS
                        .iter()
                        .any(|distribution| *distribution == anchor.distribution_id)
                {
                    return Err(AccessError(
                        "signed application witness is not accepted".to_string(),
                    ));
                }
            }
            if state
                .hosted_control
                .witnesses
                .reports
                .iter()
                .any(|record| record.report.report_id == report.report_id)
            {
                return Ok((compute_lane_guard(state, ct_serials), false));
            }
            let received_unix_ms = now_ms().max(0) as u64;
            if let Some(existing) =
                state
                    .hosted_control
                    .witnesses
                    .reports
                    .iter_mut()
                    .find(|record| {
                        record.observer_binding == observer_binding
                            && record.report.observed_serial_hex == report.observed_serial_hex
                    })
            {
                if report.vantage.is_strong() || !existing.report.vantage.is_strong() {
                    existing.report = report.clone();
                }
                existing.observer_label = observer_label.clone();
                existing.received_unix_ms = received_unix_ms;
            } else {
                state
                    .hosted_control
                    .witnesses
                    .reports
                    .push(HostedWitnessRecord {
                        report: report.clone(),
                        observer_binding: observer_binding.clone(),
                        observer_label: observer_label.clone(),
                        received_unix_ms,
                    });
            }
            state.hosted_control.normalize();
            push_audit(
                state,
                &format!("principal:witness:{observer_binding}"),
                "hosted_certificate_witness",
                &report.observed_serial_hex,
                format!(
                    "Recorded {} certificate observation from {} ({})",
                    report.observer_kind.as_str(),
                    observer_label,
                    report.vantage.as_str()
                ),
            );
            Ok((compute_lane_guard(state, ct_serials), true))
        })
        .map_err(|error| error.to_string())
    }

    pub fn confirm_witness_serial(
        &self,
        serial: &str,
        actor: &AccessPrincipal,
    ) -> AccessResult<HostedLaneGuardSnapshot> {
        self.ensure_enabled().map_err(AccessError)?;
        let serial = normalized_serial(serial).map_err(AccessError)?;
        let ct_serials = crate::fleet_cert::ct_foreign_serials();
        iam::transact_state(&self.cert_dir, |state, _| {
            let before = compute_lane_guard(state, ct_serials.clone());
            if !before.unexpected_serials.contains(&serial) {
                return Err(AccessError(
                    "certificate serial is not present in current witness evidence".to_string(),
                ));
            }
            let added_confirmation = !state
                .hosted_control
                .witnesses
                .owner_confirmed_serials
                .contains(&serial);
            if added_confirmation {
                state
                    .hosted_control
                    .witnesses
                    .owner_confirmed_serials
                    .push(serial.clone());
            }
            let cleared_override = state
                .hosted_control
                .witnesses
                .override_evidence_sha256
                .take()
                .is_some();
            if cleared_override {
                state.hosted_control.witnesses.override_actor = None;
                state.hosted_control.witnesses.override_unix_ms = None;
            }
            let changed = added_confirmation || cleared_override;
            if changed {
                state.hosted_control.normalize();
                push_audit(
                    state,
                    &actor.id,
                    "hosted_certificate_confirm",
                    &serial,
                    "Confirmed hosted certificate observation and suspended the lane".to_string(),
                );
            }
            Ok((compute_lane_guard(state, ct_serials), changed))
        })
    }

    pub fn override_witness_guard(
        &self,
        actor: &AccessPrincipal,
    ) -> AccessResult<HostedLaneGuardSnapshot> {
        self.ensure_enabled().map_err(AccessError)?;
        let ct_serials = crate::fleet_cert::ct_foreign_serials();
        iam::transact_state(&self.cert_dir, |state, _| {
            let guard = compute_lane_guard(state, ct_serials.clone());
            if guard.unexpected_serials.is_empty() {
                return Err(AccessError(
                    "certificate guard has no evidence to override".to_string(),
                ));
            }
            let changed = state
                .hosted_control
                .witnesses
                .override_evidence_sha256
                .as_deref()
                != Some(guard.evidence_sha256.as_str());
            if changed {
                state.hosted_control.witnesses.override_evidence_sha256 =
                    Some(guard.evidence_sha256.clone());
                state.hosted_control.witnesses.override_actor = Some(actor.id.clone());
                state.hosted_control.witnesses.override_unix_ms = Some(now_ms().max(0) as u64);
                push_audit(
                    state,
                    &actor.id,
                    "hosted_certificate_override",
                    &guard.evidence_sha256,
                    "Overrode the current hosted certificate evidence set".to_string(),
                );
            }
            Ok((compute_lane_guard(state, ct_serials), changed))
        })
    }
}

pub fn verify_certificate_ledger(ledger: &HostedCertificateLedger) -> Result<(), String> {
    if ledger.protocol != CERTIFICATE_LEDGER_PROTOCOL {
        return Err("unsupported fleet certificate ledger protocol".to_string());
    }
    if !valid_id_component(&ledger.daemon_id) {
        return Err("fleet certificate ledger daemon id is invalid".to_string());
    }
    validate_fleet_origin(&ledger.fleet_origin)?;
    if ledger.issued_unix_ms == 0
        || ledger.serials.is_empty()
        || ledger.serials.len() > HOSTED_CERTIFICATE_LEDGER_SERIALS_CAP
    {
        return Err("fleet certificate ledger is incomplete".to_string());
    }
    let normalized: Vec<String> = ledger
        .serials
        .iter()
        .map(|serial| normalized_serial(serial))
        .collect::<Result<_, _>>()?;
    let mut canonical = normalized.clone();
    canonical.sort();
    canonical.dedup();
    if ledger.serials != normalized || canonical != normalized {
        return Err("fleet certificate ledger serials are not canonical".to_string());
    }
    if !verify_b64u(
        &ledger.daemon_public_key,
        ledger.unsigned_payload().as_bytes(),
        &ledger.signature,
    ) {
        return Err("fleet certificate ledger signature is invalid".to_string());
    }
    Ok(())
}

pub fn compute_lane_guard(
    state: &LocalIamState,
    ct_serials: Vec<String>,
) -> HostedLaneGuardSnapshot {
    let report_serials: BTreeSet<String> = state
        .hosted_control
        .witnesses
        .reports
        .iter()
        .map(|record| record.report.observed_serial_hex.clone())
        .collect();
    let mut corroborated_serials = state.hosted_control.witnesses.corroborated_serials.clone();
    corroborated_serials.extend(corroborated_serials_from_reports(
        &state.hosted_control.witnesses.reports,
    ));
    corroborated_serials.sort();
    corroborated_serials.dedup();

    let mut ct_serials: Vec<String> = ct_serials
        .into_iter()
        .filter_map(|serial| normalized_serial(&serial).ok())
        .collect();
    ct_serials.sort();
    ct_serials.dedup();

    let mut owner_confirmed_serials = state
        .hosted_control
        .witnesses
        .owner_confirmed_serials
        .clone();
    owner_confirmed_serials.sort();
    owner_confirmed_serials.dedup();

    let mut unexpected = report_serials;
    unexpected.extend(corroborated_serials.iter().cloned());
    unexpected.extend(ct_serials.iter().cloned());
    unexpected.extend(owner_confirmed_serials.iter().cloned());
    let unexpected_serials: Vec<String> = unexpected.into_iter().collect();
    let evidence_sha256 = evidence_digest(&unexpected_serials);
    let confirmed = !corroborated_serials.is_empty()
        || !ct_serials.is_empty()
        || !owner_confirmed_serials.is_empty();
    let overridden = !unexpected_serials.is_empty()
        && state
            .hosted_control
            .witnesses
            .override_evidence_sha256
            .as_deref()
            == Some(evidence_sha256.as_str());
    let stale_override = !unexpected_serials.is_empty()
        && state
            .hosted_control
            .witnesses
            .override_evidence_sha256
            .as_deref()
            .is_some_and(|digest| digest != evidence_sha256);
    let status = if unexpected_serials.is_empty() {
        HostedLaneGuardStatus::Clear
    } else if overridden {
        HostedLaneGuardStatus::Overridden
    } else if confirmed || stale_override {
        HostedLaneGuardStatus::Suspended
    } else {
        HostedLaneGuardStatus::Alert
    };
    HostedLaneGuardSnapshot {
        status,
        evidence_sha256,
        unexpected_serials,
        corroborated_serials,
        ct_serials,
        owner_confirmed_serials,
        reports: state.hosted_control.witnesses.reports.clone(),
        override_actor: state.hosted_control.witnesses.override_actor.clone(),
        override_unix_ms: state.hosted_control.witnesses.override_unix_ms,
    }
}

fn verify_witness_report_shape(
    runtime: &HostedControlRuntime,
    report: &HostedCertificateWitnessReport,
) -> Result<(), String> {
    if report.protocol != CERTIFICATE_WITNESS_PROTOCOL {
        return Err("unsupported certificate witness protocol".to_string());
    }
    if !valid_id_component(&report.report_id) || !valid_id_component(&report.observer_id) {
        return Err("certificate witness identifier is invalid".to_string());
    }
    if report.target_daemon_id != runtime.daemon_id {
        return Err("certificate witness names a different target daemon".to_string());
    }
    let report_origin = validate_fleet_origin(&report.fleet_origin)?;
    let ledger = runtime.certificate_ledger()?;
    if report_origin != ledger.fleet_origin {
        return Err("certificate witness names a different fleet origin".to_string());
    }
    if report.ledger_sha256.len() != 43
        || !report
            .ledger_sha256
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("certificate witness ledger digest is invalid".to_string());
    }
    if report.ledger_sha256 != ledger.document_sha256() {
        return Err("certificate witness names a stale fleet certificate ledger".to_string());
    }
    let normalized = normalized_serial(&report.observed_serial_hex)?;
    if normalized != report.observed_serial_hex {
        return Err("certificate witness serial is not canonical".to_string());
    }
    let now = now_ms().max(0) as u64;
    if report.observed_unix_ms == 0 || now.abs_diff(report.observed_unix_ms) > WITNESS_MAX_SKEW_MS {
        return Err("certificate witness timestamp is outside the accepted window".to_string());
    }
    Ok(())
}

fn normalized_serial(serial: &str) -> Result<String, String> {
    let normalized = crate::fleet_cert::normalize_serial_hex(serial);
    if normalized.is_empty()
        || normalized.len() > 128
        || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("certificate serial must be 1..=128 hexadecimal characters".to_string());
    }
    Ok(normalized)
}

fn evidence_digest(serials: &[String]) -> String {
    b64u(ring::digest::digest(&ring::digest::SHA256, serials.join("\n").as_bytes()).as_ref())
}

fn bounded_witness_label(value: &str, fallback: &str) -> String {
    let label: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect();
    if label.is_empty() {
        fallback.to_string()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        id: &str,
        serial: &str,
        binding: &str,
        vantage: HostedWitnessVantage,
    ) -> HostedWitnessRecord {
        HostedWitnessRecord {
            report: HostedCertificateWitnessReport {
                protocol: CERTIFICATE_WITNESS_PROTOCOL.to_string(),
                report_id: id.to_string(),
                observer_kind: HostedWitnessKind::Peer,
                observer_id: binding.to_string(),
                observer_public_key: "key".to_string(),
                target_daemon_id: "target".to_string(),
                fleet_origin: "https://target.example.test".to_string(),
                ledger_sha256: "digest".to_string(),
                observed_serial_hex: serial.to_string(),
                vantage,
                observed_unix_ms: 1,
                signature: "signature".to_string(),
            },
            observer_binding: binding.to_string(),
            observer_label: binding.to_string(),
            received_unix_ms: 1,
        }
    }

    #[test]
    fn one_binding_alerts_and_repeats_do_not_corroborate() {
        let mut state = LocalIamState::default();
        state.hosted_control.witnesses.reports = vec![
            report("r1", "abc", "peer:a", HostedWitnessVantage::Remote),
            report("r2", "abc", "peer:a", HostedWitnessVantage::Remote),
        ];
        let guard = compute_lane_guard(&state, Vec::new());
        assert_eq!(guard.status, HostedLaneGuardStatus::Alert);
        assert!(guard.corroborated_serials.is_empty());
    }

    #[test]
    fn two_weak_bindings_alert_but_one_strong_vantage_corroborates() {
        let mut state = LocalIamState::default();
        state.hosted_control.witnesses.reports = vec![
            report("r1", "abc", "peer:a", HostedWitnessVantage::SameLan),
            report("r2", "abc", "peer:b", HostedWitnessVantage::Unknown),
        ];
        assert_eq!(
            compute_lane_guard(&state, Vec::new()).status,
            HostedLaneGuardStatus::Alert
        );
        state.hosted_control.witnesses.reports[1].report.vantage = HostedWitnessVantage::Remote;
        let guard = compute_lane_guard(&state, Vec::new());
        assert_eq!(guard.status, HostedLaneGuardStatus::Suspended);
        assert_eq!(guard.corroborated_serials, vec!["abc"]);
    }

    #[test]
    fn ct_and_owner_confirmation_suspend_independently() {
        let mut state = LocalIamState::default();
        assert_eq!(
            compute_lane_guard(&state, vec!["00AB".to_string()]).status,
            HostedLaneGuardStatus::Suspended
        );
        state
            .hosted_control
            .witnesses
            .owner_confirmed_serials
            .push("def".to_string());
        let guard = compute_lane_guard(&state, Vec::new());
        assert_eq!(guard.status, HostedLaneGuardStatus::Suspended);
        assert_eq!(guard.owner_confirmed_serials, vec!["def"]);
    }

    #[test]
    fn override_is_exact_to_the_unexpected_serial_set() {
        let mut state = LocalIamState::default();
        state.hosted_control.witnesses.reports.push(report(
            "r1",
            "abc",
            "peer:a",
            HostedWitnessVantage::Remote,
        ));
        let first = compute_lane_guard(&state, Vec::new());
        state.hosted_control.witnesses.override_evidence_sha256 =
            Some(first.evidence_sha256.clone());
        assert_eq!(
            compute_lane_guard(&state, Vec::new()).status,
            HostedLaneGuardStatus::Overridden
        );
        state.hosted_control.witnesses.reports.push(report(
            "r2",
            "def",
            "peer:b",
            HostedWitnessVantage::Remote,
        ));
        assert_eq!(
            compute_lane_guard(&state, Vec::new()).status,
            HostedLaneGuardStatus::Suspended
        );
    }

    #[test]
    fn corroboration_survives_bounded_report_history_pruning() {
        let mut state = LocalIamState::default();
        state.hosted_control.witnesses.reports = vec![
            report("r1", "abc", "peer:a", HostedWitnessVantage::Remote),
            report("r2", "abc", "peer:b", HostedWitnessVantage::SameLan),
        ];
        state.hosted_control.normalize();
        assert_eq!(
            state.hosted_control.witnesses.corroborated_serials,
            vec!["abc"]
        );

        state.hosted_control.witnesses.reports = (0..HOSTED_WITNESS_REPORTS_CAP)
            .map(|index| {
                report(
                    &format!("flood-{index}"),
                    &format!("{:x}", index + 0x1000),
                    "peer:c",
                    HostedWitnessVantage::Unknown,
                )
            })
            .collect();
        state.hosted_control.normalize();

        let guard = compute_lane_guard(&state, Vec::new());
        assert_eq!(guard.status, HostedLaneGuardStatus::Suspended);
        assert_eq!(guard.corroborated_serials, vec!["abc"]);
        assert!(guard.unexpected_serials.contains(&"abc".to_string()));
    }

    #[test]
    fn build_without_an_eligible_distribution_accepts_no_app_witness() {
        assert!(ELIGIBLE_SIGNED_APP_DISTRIBUTIONS.is_empty());
        let temp = tempfile::tempdir().unwrap();
        let runtime = HostedControlRuntime::new(
            true,
            temp.path().join("access"),
            Some(&temp.path().join("identity.pk8")),
            Some("target"),
            "Target".to_string(),
            false,
        );
        let witness = HostedCertificateWitnessReport {
            protocol: CERTIFICATE_WITNESS_PROTOCOL.to_string(),
            report_id: "report-1".to_string(),
            observer_kind: HostedWitnessKind::SignedApp,
            observer_id: "device-1".to_string(),
            observer_public_key: "not-used".to_string(),
            target_daemon_id: "target".to_string(),
            fleet_origin: "https://target.example.test".to_string(),
            ledger_sha256: "not-used".to_string(),
            observed_serial_hex: "abc".to_string(),
            vantage: HostedWitnessVantage::Cellular,
            observed_unix_ms: now_ms().max(0) as u64,
            signature: "not-used".to_string(),
        };
        assert!(runtime
            .receive_signed_app_witness(witness)
            .unwrap_err()
            .contains("no qualifying signed application distribution"));
    }

    #[test]
    fn owner_confirmation_can_end_an_existing_override() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = HostedControlRuntime::new(
            true,
            temp.path().join("access"),
            Some(&temp.path().join("identity.pk8")),
            Some("target"),
            "Target".to_string(),
            false,
        );
        iam::transact_state(&runtime.cert_dir, |state, _| {
            state.hosted_control.witnesses.reports.push(report(
                "r1",
                "abc",
                "peer:a",
                HostedWitnessVantage::Remote,
            ));
            state.hosted_control.normalize();
            let guard = compute_lane_guard(state, Vec::new());
            state.hosted_control.witnesses.override_evidence_sha256 = Some(guard.evidence_sha256);
            state.hosted_control.witnesses.override_actor = Some("owner".to_string());
            state.hosted_control.witnesses.override_unix_ms = Some(1);
            Ok(((), true))
        })
        .unwrap();

        let actor = AccessPrincipal::root_dashboard_session("owner", "Owner");
        let guard = runtime.confirm_witness_serial("abc", &actor).unwrap();
        assert_eq!(guard.status, HostedLaneGuardStatus::Suspended);
        assert_eq!(guard.owner_confirmed_serials, vec!["abc"]);
        assert!(guard.override_actor.is_none());
        assert!(guard.override_unix_ms.is_none());
    }

    #[test]
    fn certificate_ledger_signature_binds_the_exact_origin_and_serial_set() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let identity = crate::daemon_identity::DaemonIdentity::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut ledger = HostedCertificateLedger {
            protocol: CERTIFICATE_LEDGER_PROTOCOL.to_string(),
            daemon_id: "daemon-1".to_string(),
            daemon_public_key: identity.public_key_b64u(),
            fleet_origin: "https://fleet.example.test".to_string(),
            serials: vec!["a".to_string(), "b".to_string()],
            issued_unix_ms: 1,
            signature: String::new(),
        };
        ledger.signature = identity.sign_b64u(ledger.unsigned_payload().as_bytes());
        verify_certificate_ledger(&ledger).unwrap();

        let mut altered = ledger.clone();
        altered.fleet_origin = "https://other.example.test".to_string();
        assert!(verify_certificate_ledger(&altered)
            .unwrap_err()
            .contains("signature"));

        ledger.serials = vec!["00a".to_string()];
        ledger.signature = identity.sign_b64u(ledger.unsigned_payload().as_bytes());
        assert!(verify_certificate_ledger(&ledger)
            .unwrap_err()
            .contains("canonical"));
    }

    #[test]
    fn witness_report_must_name_the_current_ledger_document() {
        let temp = tempfile::tempdir().unwrap();
        let cert_dir = temp.path().join("access");
        std::fs::create_dir_all(&cert_dir).unwrap();
        std::fs::write(
            cert_dir.join("fleet-origin-provenance.json"),
            r#"{
                "schema_version": 1,
                "zone": "example.test",
                "name": "target.example.test",
                "known_names": ["target.example.test"],
                "provenance_incomplete": false
            }"#,
        )
        .unwrap();
        std::fs::write(
            cert_dir.join("fleet-cert-serials.json"),
            r#"[{
                "serial_hex": "abc",
                "name": "target.example.test",
                "directory": "test",
                "issued_unix_ms": 1
            }]"#,
        )
        .unwrap();
        let runtime = HostedControlRuntime::new(
            true,
            cert_dir,
            Some(&temp.path().join("identity.pk8")),
            Some("target"),
            "Target".to_string(),
            false,
        );
        let ledger = runtime.certificate_ledger().unwrap();
        let mut report = runtime
            .build_peer_witness_report(&ledger, "def", HostedWitnessVantage::Remote)
            .unwrap();
        report.ledger_sha256 = "A".repeat(43);

        assert!(verify_witness_report_shape(&runtime, &report)
            .unwrap_err()
            .contains("stale"));
    }
}
