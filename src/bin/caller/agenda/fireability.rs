//! Propose-time fireability validation — THE one validator behind every
//! manifest mint surface (CLI `agenda schedule`, the automate modal, the
//! approval-time editor, the stamp lane) and the approve-time re-check.
//!
//! The principle (card 01KYSZAGQVHAAYS7BK9H3QFM3C): an approvable manifest
//! IS a fireable manifest. Every contradiction the fire path would
//! deterministically refuse — no resolvable project, a contradictory
//! executor config, a floor whose window has already passed — is named at
//! the mint, and named again at approve (the arm gate), instead of being
//! discovered as an approved-but-dead card. Because the validation lives
//! in the ProposeEffect intake arm ([`super::store`]'s `command_to_op`),
//! which every mint surface routes through (the stamp lane proposes each
//! node via the same arm), no surface can grow its own copy.
//!
//! Three legs, each derived from what the fire path itself does:
//! - **project** — [`super::spawn_project::resolve_spawn_project`], the
//!   SAME chain the scheduler dispatches through (explicit pin → the
//!   parking session's recorded root → the daemon default → named
//!   refusal). The chain is validated against, never narrowed: a
//!   fallback-resolved root passes, and the resolution is RECORDED on the
//!   manifest so the approval covers WHERE.
//! - **executor** — [`crate::session_supervisor::validate_launch_config`]
//!   over the config AS IT WILL BE RECORDED: an absent backend selection
//!   resolves to the daemon default NOW and is written into the manifest
//!   (`agent_config.agent`), so the owner approves a named executor
//!   rather than empty-means-whatever-the-daemon-defaults-to-later — and
//!   pins that contradict the resolved backend refuse at the mint
//!   instead of resolving surprisingly at fire time.
//! - **floor** — the planner's own missed rule mirrored
//!   ([`super::reminders`]: a non-triggered instant more than the policy
//!   staleness past due is `missed`, never fired): a one-shot whose
//!   window has already passed, or a series with no live instant left,
//!   refuses. A triggered manifest's `fire_at_ms` is the ARM FLOOR (T0
//!   ruling 3) and is never floor-refused.
//!
//! Refusals carry a machine-readable grammar —
//! `unfireable(<field>): <reason>` — so surfaces can map a refusal back
//! to the offending editor field (the approval-time edit prompt) without
//! a second vocabulary. [`fireability_schema_coverage`] pins the legs to
//! the [`super::types::SessionManifest`] schemars schema: a new manifest
//! field fails the parity test until its fireability class is declared.

use super::spawn_project::{resolve_spawn_project, SessionSpawnContext, SpawnProjectSource};
use super::types::{RecurrenceSpec, TriggerSpec};
use crate::event::AgentLaunchConfig;
use std::path::PathBuf;

/// The manifest field a refusal names — the editor-focus vocabulary every
/// surface shares (the sched sheet focuses this field on an approve-time
/// refusal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FireabilityField {
    Project,
    Executor,
    Floor,
}

impl FireabilityField {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FireabilityField::Project => "project",
            FireabilityField::Executor => "executor",
            FireabilityField::Floor => "floor",
        }
    }
}

/// The stable machine-readable head of every fireability refusal message:
/// `unfireable(<field>): <reason>`. The SPA maps an approve-time refusal
/// to the editor field by this grammar; changing it is a wire-contract
/// change (pinned by `refusal_grammar_is_pinned` and the static-asset
/// parity twin).
pub(crate) const FIREABILITY_REFUSAL_PREFIX: &str = "unfireable(";

/// One named fireability refusal: the offending field plus the reason.
/// `message()` is the wire form (the grammar above); `field`/`reason`
/// also ride the served [`FireabilityRefusalView`] so render surfaces
/// never re-parse the grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FireabilityRefusal {
    pub(crate) field: FireabilityField,
    pub(crate) reason: String,
}

impl FireabilityRefusal {
    fn new(field: FireabilityField, reason: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
        }
    }

    /// The wire/CLI message: `unfireable(<field>): <reason>`.
    pub(crate) fn message(&self) -> String {
        format!(
            "{}{}): {}",
            FIREABILITY_REFUSAL_PREFIX,
            self.field.as_str(),
            self.reason
        )
    }

    pub(crate) fn view(&self) -> FireabilityRefusalView {
        FireabilityRefusalView {
            field: self.field.as_str().to_string(),
            reason: self.reason.clone(),
        }
    }
}

/// The served shape of a refusal — decorated onto pending/suspended
/// effects at the serving seam (the `next_fire_ms` pattern: display-only,
/// never folded, never stored) so the dashboard can withhold the Approve
/// affordance and open the editor on the named field instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FireabilityRefusalView {
    /// `project` | `executor` | `floor` — the editor-focus key.
    pub(crate) field: String,
    /// Human reason, exactly the refusal the propose/approve lane would
    /// state (minus the grammar head).
    pub(crate) reason: String,
}

/// The manifest fields fireability reads — a borrow view so the propose
/// intake (pre-manifest command fields) and the approve re-check (the
/// stored manifest) share one validator without cloning.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FireabilityCandidate<'a> {
    pub(crate) fire_at_ms: u64,
    pub(crate) recurrence: Option<&'a RecurrenceSpec>,
    pub(crate) trigger: Option<&'a TriggerSpec>,
    pub(crate) project_root: Option<&'a str>,
    pub(crate) agent_config: Option<&'a AgentLaunchConfig>,
}

impl<'a> FireabilityCandidate<'a> {
    /// The approve-time view of a stored manifest.
    pub(crate) fn of_manifest(manifest: &'a super::types::SessionManifest) -> Self {
        Self {
            fire_at_ms: manifest.fire_at_ms,
            recurrence: manifest.recurrence.as_ref(),
            trigger: manifest.trigger.as_ref(),
            project_root: manifest.project_root.as_deref(),
            agent_config: manifest.agent_config.as_deref(),
        }
    }
}

/// What a passing validation resolved — the values the propose lane
/// RECORDS onto the manifest (approve re-checks resolve too, but record
/// nothing: an approval never rewrites the bytes it binds).
#[derive(Debug, Clone)]
pub(crate) struct FireabilityResolution {
    /// The concrete project root the spawn will run under.
    pub(crate) project_root: PathBuf,
    pub(crate) project_source: SpawnProjectSource,
    /// The effective executor backend (`internal`, `codex`, …): the
    /// explicit `agent_config.agent` when given, else the daemon default
    /// at this instant — the value the propose lane records.
    pub(crate) agent: String,
}

/// Validate one manifest candidate against the daemon's current state.
/// `provenance_session` is the parking session (item provenance) the
/// project chain may resolve through; `staleness_ms` is the reminder
/// policy's missed-window bound (the SAME bound the planner classifies
/// missed sessions with); `now_ms` is the validation instant — propose
/// time at the mint, approve time at the arm gate.
pub(crate) fn validate(
    candidate: FireabilityCandidate<'_>,
    provenance_session: Option<&str>,
    ctx: &SessionSpawnContext,
    staleness_ms: u64,
    now_ms: u64,
) -> Result<FireabilityResolution, FireabilityRefusal> {
    let (project_root, project_source) = resolve_spawn_project(
        candidate.project_root,
        provenance_session,
        ctx,
    )
    .map_err(|reason| FireabilityRefusal::new(FireabilityField::Project, reason))?;
    let agent = validate_executor(candidate.agent_config, ctx)?;
    validate_floor(&candidate, staleness_ms, now_ms)?;
    Ok(FireabilityResolution {
        project_root,
        project_source,
        agent,
    })
}

/// The executor leg: resolve the effective backend (explicit selection →
/// the daemon default → internal) and validate the config AS RESOLVED —
/// the same launch-config authority the session supervisor enforces, run
/// against the backend that will actually be recorded, so pins that
/// contradict the resolved default refuse at the mint instead of
/// resolving surprisingly at fire time.
fn validate_executor(
    config: Option<&AgentLaunchConfig>,
    ctx: &SessionSpawnContext,
) -> Result<String, FireabilityRefusal> {
    let explicit = config
        .and_then(|config| config.agent.as_deref())
        .map(str::trim)
        .filter(|agent| !agent.is_empty());
    let agent = explicit
        .or(ctx.default_agent.as_deref())
        .unwrap_or("internal")
        .to_string();
    let mut resolved = config.cloned().unwrap_or_default();
    resolved.agent = Some(agent.clone());
    crate::session_supervisor::validate_launch_config(&resolved).map_err(|reason| {
        let reason = if explicit.is_none() {
            format!("{reason} (backend {agent:?} is this daemon's default — pin --agent to override it)")
        } else {
            reason
        };
        FireabilityRefusal::new(FireabilityField::Executor, reason)
    })?;
    Ok(agent)
}

/// The floor leg — the planner's missed rule, applied at the instant a
/// human is deciding: a floor that already guarantees `missed` refuses
/// with the remedy named. Triggered manifests are exempt (their
/// `fire_at_ms` is the arm floor, and trigger occurrences never
/// stale-miss).
fn validate_floor(
    candidate: &FireabilityCandidate<'_>,
    staleness_ms: u64,
    now_ms: u64,
) -> Result<(), FireabilityRefusal> {
    if candidate.fire_at_ms == 0 {
        return Err(FireabilityRefusal::new(
            FireabilityField::Floor,
            "fire_at_ms must be set",
        ));
    }
    if candidate.trigger.is_some() {
        return Ok(());
    }
    let window_floor = now_ms.saturating_sub(staleness_ms);
    match candidate.recurrence {
        None => {
            if candidate.fire_at_ms < window_floor {
                return Err(FireabilityRefusal::new(
                    FireabilityField::Floor,
                    format!(
                        "the fire window already passed ({} + the {}h staleness bound is in \
                         the past) — it would resolve missed, never run; pick a fresh time \
                         (`--at` on ctl, the sheet's First-run field)",
                        format_instant(candidate.fire_at_ms),
                        staleness_ms / 3_600_000
                    ),
                ));
            }
        }
        Some(rec) => {
            // A stale FIRST instant is fine on a standing series (the
            // planner records it missed and the series continues); what
            // refuses is a series with no live instant left at all.
            if let Some(last) = last_series_instant(candidate.fire_at_ms, rec) {
                if last < window_floor {
                    return Err(FireabilityRefusal::new(
                        FireabilityField::Floor,
                        format!(
                            "the series is already spent — its last instant ({}) plus the \
                             {}h staleness bound is in the past; extend `--until`/the run \
                             cap or pick a fresh first run",
                            format_instant(last),
                            staleness_ms / 3_600_000
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The last instant a bounded series can fire, or `None` when unbounded
/// (no `until_ms`, no `max_occurrences` — always live). Saturating math:
/// a huge cap saturates to u64::MAX, which is simply "live".
fn last_series_instant(fire_at_ms: u64, rec: &RecurrenceSpec) -> Option<u64> {
    let by_max = rec.max_occurrences.map(|max| {
        fire_at_ms.saturating_add(u64::from(max.saturating_sub(1)).saturating_mul(rec.every_ms))
    });
    let by_until = rec.until_ms.map(|until| {
        if until <= fire_at_ms || rec.every_ms == 0 {
            fire_at_ms
        } else {
            fire_at_ms.saturating_add(((until - fire_at_ms) / rec.every_ms) * rec.every_ms)
        }
    });
    match (by_max, by_until) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn format_instant(ms: u64) -> String {
    use chrono::TimeZone as _;
    match chrono::Local.timestamp_millis_opt(ms as i64) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        _ => format!("{ms}ms"),
    }
}

/// How each `SessionManifest` schema field participates in fireability —
/// the derive-from-schema contract. The parity test pins this map's keys
/// to `schemars::schema_for!(SessionManifest)`, so a new manifest field
/// fails the suite until its fireability class is declared here (covered
/// by a leg, validated elsewhere in the same intake arm, or explicitly
/// not a fireability input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FireabilityCoverage {
    /// Read by a validator leg ([`FireabilityCandidate`] carries it).
    Leg(FireabilityField),
    /// Validated by the same ProposeEffect intake arm the validator runs
    /// in (shape/bounds/sealing), just not a fireability question.
    IntakeValidated,
    /// Launch/display shape with no failure mode the fire path could
    /// deterministically refuse.
    NotAFireabilityInput,
}

pub(crate) fn fireability_schema_coverage(
) -> std::collections::BTreeMap<&'static str, FireabilityCoverage> {
    use FireabilityCoverage as C;
    [
        ("goal", C::IntakeValidated),
        ("fire_at_ms", C::Leg(FireabilityField::Floor)),
        ("orchestrate", C::NotAFireabilityInput),
        ("interactive", C::NotAFireabilityInput),
        ("project_root", C::Leg(FireabilityField::Project)),
        ("agent_config", C::Leg(FireabilityField::Executor)),
        ("recurrence", C::Leg(FireabilityField::Floor)),
        ("trigger", C::Leg(FireabilityField::Floor)),
        ("binding_refs", C::IntakeValidated),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const HOUR: u64 = 3_600_000;
    const STALENESS: u64 = 12 * HOUR;
    const NOW: u64 = 1_800_000_000_000;

    fn ctx(default_project: Option<&Path>, default_agent: Option<&str>) -> SessionSpawnContext {
        SessionSpawnContext {
            home: std::env::temp_dir().join("fireability-no-home"),
            default_project_root: default_project.map(Path::to_path_buf),
            default_agent: default_agent.map(str::to_string),
        }
    }

    fn candidate(fire_at_ms: u64) -> FireabilityCandidate<'static> {
        FireabilityCandidate {
            fire_at_ms,
            recurrence: None,
            trigger: None,
            project_root: None,
            agent_config: None,
        }
    }

    /// The ONE-validator contract with the schema: every manifest field
    /// declares its fireability class, and the set tracks
    /// `SessionManifest` exactly — a new field fails here until covered.
    #[test]
    fn fireability_validator_covers_the_manifest_schema() {
        let declared: std::collections::BTreeSet<String> = fireability_schema_coverage()
            .keys()
            .map(|k| k.to_string())
            .collect();
        assert_eq!(
            declared,
            super::super::types::session_manifest_schema_fields(),
            "a SessionManifest field appeared without a declared fireability \
             class — extend fireability_schema_coverage (and the validator, \
             if the fire path can deterministically refuse on it)"
        );
    }

    /// The grammar every surface maps refusals by: pinned as a wire
    /// contract (the SPA's approve-time edit prompt parses it; the
    /// static-asset parity twin pins the fragment literal).
    #[test]
    fn refusal_grammar_is_pinned() {
        let refusal = FireabilityRefusal::new(FireabilityField::Project, "no project");
        assert_eq!(refusal.message(), "unfireable(project): no project");
        assert_eq!(FIREABILITY_REFUSAL_PREFIX, "unfireable(");
        for (field, name) in [
            (FireabilityField::Project, "project"),
            (FireabilityField::Executor, "executor"),
            (FireabilityField::Floor, "floor"),
        ] {
            assert_eq!(field.as_str(), name);
            assert!(FireabilityRefusal::new(field, "x")
                .message()
                .starts_with(&format!("unfireable({name}): ")));
        }
        let view = refusal.view();
        assert_eq!(view.field, "project");
        assert_eq!(view.reason, "no project");
    }

    /// Projectless daemon + no provenance + no pin = the named refusal,
    /// carrying the concrete flag to add.
    #[test]
    fn projectless_daemon_refuses_with_the_named_flag() {
        let err = validate(candidate(NOW), None, &ctx(None, None), STALENESS, NOW).unwrap_err();
        assert_eq!(err.field, FireabilityField::Project);
        assert!(err.message().starts_with("unfireable(project): "), "{err:?}");
        assert!(err.reason.contains("--project"), "{}", err.reason);
    }

    /// The fallback chain is validated against, never narrowed: a daemon
    /// default resolves a pin-less candidate, and the resolution names
    /// its source so the mint can record it.
    #[test]
    fn daemon_default_resolves_and_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = validate(
            candidate(NOW),
            None,
            &ctx(Some(dir.path()), None),
            STALENESS,
            NOW,
        )
        .unwrap();
        assert_eq!(resolved.project_root, dir.path());
        assert_eq!(resolved.project_source, SpawnProjectSource::DaemonDefault);
        assert_eq!(resolved.agent, "internal");
    }

    /// Executor resolution: explicit pin wins; otherwise the daemon
    /// default at this instant; otherwise internal.
    #[test]
    fn executor_resolves_explicit_then_default_then_internal() {
        let dir = tempfile::tempdir().unwrap();
        let base = ctx(Some(dir.path()), Some("claude-code"));
        let resolved = validate(candidate(NOW), None, &base, STALENESS, NOW).unwrap();
        assert_eq!(resolved.agent, "claude-code");
        let config = AgentLaunchConfig {
            agent: Some("codex".into()),
            ..Default::default()
        };
        let explicit = FireabilityCandidate {
            agent_config: Some(&config),
            ..candidate(NOW)
        };
        assert_eq!(
            validate(explicit, None, &base, STALENESS, NOW).unwrap().agent,
            "codex"
        );
    }

    /// Pins that contradict the backend the mint would record refuse as
    /// executor unfireability — at the mint, not surprisingly at fire.
    #[test]
    fn cross_backend_pins_refuse_against_the_resolved_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentLaunchConfig {
            claude_model: Some("claude-fable-5".into()),
            ..Default::default()
        };
        let cand = FireabilityCandidate {
            agent_config: Some(&config),
            ..candidate(NOW)
        };
        let err = validate(
            cand,
            None,
            &ctx(Some(dir.path()), Some("codex")),
            STALENESS,
            NOW,
        )
        .unwrap_err();
        assert_eq!(err.field, FireabilityField::Executor);
        assert!(err.reason.contains("daemon's default"), "{}", err.reason);
        // The same pins under a matching resolved backend pass.
        validate(
            cand,
            None,
            &ctx(Some(dir.path()), Some("claude-code")),
            STALENESS,
            NOW,
        )
        .unwrap();
    }

    /// One-shot floor sanity mirrors the planner's missed rule: within
    /// the staleness window passes (the CLI's `--at now` propose), past
    /// it refuses with the remedy named.
    #[test]
    fn one_shot_floor_refuses_only_past_the_staleness_window() {
        let dir = tempfile::tempdir().unwrap();
        let base = ctx(Some(dir.path()), None);
        validate(candidate(NOW - STALENESS + HOUR), None, &base, STALENESS, NOW).unwrap();
        validate(candidate(NOW + HOUR), None, &base, STALENESS, NOW).unwrap();
        let err =
            validate(candidate(NOW - STALENESS - HOUR), None, &base, STALENESS, NOW).unwrap_err();
        assert_eq!(err.field, FireabilityField::Floor);
        assert!(err.reason.contains("--at"), "{}", err.reason);
        let err = validate(candidate(0), None, &base, STALENESS, NOW).unwrap_err();
        assert_eq!(err.field, FireabilityField::Floor);
    }

    /// TRAP honored: a triggered manifest's floor is the arm floor — a
    /// past floor is an immediate arming, never a refusal.
    #[test]
    fn trigger_arm_floor_is_never_floor_refused() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = TriggerSpec::OnUnblock;
        let cand = FireabilityCandidate {
            trigger: Some(&trigger),
            fire_at_ms: NOW - 30 * 24 * HOUR,
            ..candidate(NOW)
        };
        validate(cand, None, &ctx(Some(dir.path()), None), STALENESS, NOW).unwrap();
    }

    /// Series liveness: a stale first instant on an unbounded or
    /// still-running series passes (the planner misses it and continues);
    /// a series whose LAST instant is past the window refuses.
    #[test]
    fn series_floor_refuses_only_spent_series() {
        let dir = tempfile::tempdir().unwrap();
        let base = ctx(Some(dir.path()), None);
        let day = 24 * HOUR;
        let unbounded = RecurrenceSpec {
            every_ms: day,
            until_ms: None,
            max_occurrences: None,
            suspend_after_failures: None,
        };
        let cand = FireabilityCandidate {
            recurrence: Some(&unbounded),
            fire_at_ms: NOW - 30 * day,
            ..candidate(NOW)
        };
        validate(cand, None, &base, STALENESS, NOW).unwrap();

        let spent = RecurrenceSpec {
            max_occurrences: Some(3),
            ..unbounded
        };
        let cand = FireabilityCandidate {
            recurrence: Some(&spent),
            fire_at_ms: NOW - 30 * day,
            ..candidate(NOW)
        };
        let err = validate(cand, None, &base, STALENESS, NOW).unwrap_err();
        assert_eq!(err.field, FireabilityField::Floor);
        assert!(err.reason.contains("already spent"), "{}", err.reason);

        let expired_until = RecurrenceSpec {
            until_ms: Some(NOW - 20 * day),
            ..unbounded
        };
        let cand = FireabilityCandidate {
            recurrence: Some(&expired_until),
            fire_at_ms: NOW - 30 * day,
            ..candidate(NOW)
        };
        let err = validate(cand, None, &base, STALENESS, NOW).unwrap_err();
        assert_eq!(err.field, FireabilityField::Floor);

        // A live bounded series (last instant ahead) passes.
        let live = RecurrenceSpec {
            until_ms: Some(NOW + 10 * day),
            ..unbounded
        };
        let cand = FireabilityCandidate {
            recurrence: Some(&live),
            fire_at_ms: NOW - 30 * day,
            ..candidate(NOW)
        };
        validate(cand, None, &base, STALENESS, NOW).unwrap();
    }
}
