//! Per-skill enable state (skills/plugins unification S3): the
//! daemon-owned disabled-set the materialization sweep applies over both
//! discovery roots (`~/.agents/skills` for native discovery,
//! `~/.claude/skills` for the Claude Code bridge).
//!
//! One small versioned JSON file `<state_root>/skills/state.json`,
//! mirroring the plugin enable-state discipline
//! ([`crate::plugin_registry`]): loads are tolerant (missing/corrupt/
//! foreign version ⇒ empty — the shipped default is every builtin
//! active), writes are private (0600) and atomic, and both entries and
//! top-level fields this binary does not recognize are preserved across
//! rewrites, so an older daemon never disarms a newer one's records (the
//! planned S4 user-skill library rides this same file).
//!
//! **The set outranks the sweep.** Every sweep call site derives its
//! desired set through [`disabled_skill_names`], so rebuilds, daemon
//! restarts, plugin refreshes, and re-materializations all re-apply the
//! set — a disabled skill can never silently resurrect
//! (`crate::skill_install` subtracts it on every pass).
//!
//! **Per-kind law** (the intake's gesture table): builtin skills disable
//! via this set — the bytes stay in the binary, deactivate is the honest
//! verb; plugin-materialized skills are managed ONLY by their plugin's
//! toggle (one authority, no second switch) and are never in this set;
//! unknown names refuse. One classification ([`skill_lifecycle`]) drives
//! both the catalog row's toggle availability and the mutation gate, so
//! the two can never skew.
//!
//! Each disabled entry records the flip's gate-resolved attribution
//! (principal + actor kind + timestamp) in the skills plane's own
//! versioned record shape — mapped from
//! [`crate::access::actor::ActorBinding`] at the authenticated edge,
//! never serde of the seam type, never request-body claims.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
struct SkillsStateFile {
    version: u32,
    #[serde(default)]
    disabled: BTreeMap<String, DisabledRecord>,
    /// Top-level fields a newer binary owns (the S4 user-skill library is
    /// planned into this same file) — carried through rewrites verbatim.
    #[serde(flatten)]
    foreign: serde_json::Map<String, serde_json::Value>,
}

/// One disabled entry: the gate-resolved attribution of the disabling
/// flip. All fields optional by design — attribution must never block the
/// flip — and unknown fields from newer binaries ride `foreign`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct DisabledRecord {
    /// The IAM principal exactly as the gate named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    /// Actor class (`dashboard`, `local_process`, `peer`, …) so the row
    /// can say "by you" without parsing principal ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    /// Flip time, unix ms.
    #[serde(default)]
    pub(crate) at_ms: u64,
    /// Record fields a newer binary owns — preserved verbatim.
    #[serde(flatten)]
    pub(crate) foreign: serde_json::Map<String, serde_json::Value>,
}

impl DisabledRecord {
    /// Map the shared actor seam into the skills plane's own record shape
    /// at the authenticated edge (the `AgendaActor::from_binding`
    /// discipline transplanted).
    pub(crate) fn from_actor(binding: &crate::access::actor::ActorBinding, at_ms: u64) -> Self {
        let kind = (binding.kind != crate::access::actor::ActorKind::Unattributed)
            .then(|| binding.kind.as_str().to_string());
        Self {
            principal: binding.principal_id.clone(),
            kind,
            at_ms,
            foreign: serde_json::Map::new(),
        }
    }
}

/// `<state_root>/skills/state.json`.
pub(crate) fn skills_state_path_in(state_root: &Path) -> PathBuf {
    state_root.join("skills").join("state.json")
}

/// Tolerant load: missing, unreadable, corrupt, or foreign-version state
/// reads as the shipped default — nothing disabled (ruling R4).
fn load_state_in(state_root: &Path) -> SkillsStateFile {
    let raw = match std::fs::read(skills_state_path_in(state_root)) {
        Ok(raw) => raw,
        Err(_) => return SkillsStateFile::default(),
    };
    match serde_json::from_slice::<SkillsStateFile>(&raw) {
        Ok(state) if state.version == STATE_VERSION => state,
        _ => SkillsStateFile::default(),
    }
}

/// The persisted disabled-set with each entry's attribution — including
/// names this binary does not ship (a newer daemon's entries survive).
pub(crate) fn disabled_skills_in(state_root: &Path) -> BTreeMap<String, DisabledRecord> {
    load_state_in(state_root).disabled
}

/// Just the disabled names, for the installer's desired-set subtraction.
pub(crate) fn disabled_skill_names_in(state_root: &Path) -> BTreeSet<String> {
    disabled_skills_in(state_root).into_keys().collect()
}

/// [`disabled_skill_names_in`] against the daemon's own state root — the
/// read every sweep call site goes through (the set outranks the sweep).
pub(crate) fn disabled_skill_names() -> BTreeSet<String> {
    disabled_skill_names_in(&intendant_core::state_paths::intendant_home())
}

// ── Per-kind classification (one authority for row + mutation) ──────────────

/// Which lifecycle door manages one skill name. Derived from the shipped
/// registries; the catalog row's toggle availability and the mutation
/// gate both read THIS, so they cannot skew.
pub(crate) enum SkillLifecycle {
    /// A builtin: deactivate/re-enable via the persisted disabled-set.
    Builtin(&'static crate::builtin_skills::BuiltinSkill),
    /// A bundled plugin's payload: its lifecycle IS the plugin's toggle.
    PluginManaged(&'static crate::plugin_registry::BundledPlugin),
    /// Not a name the registries know.
    Unknown,
}

pub(crate) fn skill_lifecycle(name: &str) -> SkillLifecycle {
    if let Some(skill) = crate::builtin_skills::BUILTIN_SKILLS
        .iter()
        .find(|skill| skill.name == name)
    {
        return SkillLifecycle::Builtin(skill);
    }
    for plugin in crate::plugin_registry::BUNDLED_PLUGINS {
        if plugin.skills.iter().any(|skill| skill.name == name) {
            return SkillLifecycle::PluginManaged(plugin);
        }
    }
    SkillLifecycle::Unknown
}

/// The catalog row's `lifecycle` body, derived from [`skill_lifecycle`]
/// plus the persisted set. `control` names the door: `"toggle"` rows are
/// flippable here (with `enabled` + the disabling attribution when off);
/// `"plugin"` rows carry their managing plugin — the one authority, no
/// second switch. The frontend renders this verbatim and holds no kind
/// vocabulary of its own.
pub(crate) fn skill_lifecycle_json(
    name: &str,
    disabled: &BTreeMap<String, DisabledRecord>,
) -> serde_json::Value {
    match skill_lifecycle(name) {
        SkillLifecycle::Builtin(_) => match disabled.get(name) {
            None => serde_json::json!({ "control": "toggle", "enabled": true }),
            Some(record) => serde_json::json!({
                "control": "toggle",
                "enabled": false,
                "disabled_by": serde_json::to_value(record).unwrap_or_default(),
            }),
        },
        SkillLifecycle::PluginManaged(plugin) => serde_json::json!({
            "control": "plugin",
            "plugin_id": plugin.id,
            "plugin_display_name": plugin.display_name,
        }),
        SkillLifecycle::Unknown => serde_json::json!({ "control": "unknown" }),
    }
}

// ── The mutation (refusals named per kind) ──────────────────────────────────

/// A refused or failed toggle. Refusal text is composed here — beside the
/// classification — so the per-kind shapes are pinned in one place.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SkillToggleRefusal {
    /// The row's lifecycle door is its plugin's toggle (intake §2b /
    /// ruling H5: the refusal NAMES the managing plugin).
    PluginManaged { message: String },
    /// Not a name the registries know.
    UnknownSkill { message: String },
    /// State write failure.
    Io { message: String },
}

impl SkillToggleRefusal {
    fn plugin_managed(plugin: &'static crate::plugin_registry::BundledPlugin) -> Self {
        Self::PluginManaged {
            message: format!(
                "a plugin-materialized skill is managed by its plugin — toggle '{}' ({})",
                plugin.id, plugin.display_name
            ),
        }
    }

    fn unknown(name: &str) -> Self {
        Self::UnknownSkill {
            message: format!("unknown skill '{name}' — not a builtin or bundled plugin payload"),
        }
    }

    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self::Io {
            message: message.into(),
        }
    }

    pub(crate) fn http_status(&self) -> u16 {
        match self {
            Self::PluginManaged { .. } => 409,
            Self::UnknownSkill { .. } => 404,
            Self::Io { .. } => 500,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::PluginManaged { message }
            | Self::UnknownSkill { message }
            | Self::Io { message } => message,
        }
    }
}

/// Flip one skill's enable state in the persisted set. Idempotent: a
/// no-change flip writes nothing (a repeat disable keeps the ORIGINAL
/// attribution — the record is who disabled it, not who last asked).
/// Foreign entries and top-level fields survive the rewrite; the write is
/// private (0600) and atomic (tmp + rename). Callers reconcile the
/// installed roots after a successful flip.
pub(crate) fn set_skill_enabled_in(
    state_root: &Path,
    name: &str,
    enabled: bool,
    record: DisabledRecord,
) -> Result<(), SkillToggleRefusal> {
    match skill_lifecycle(name) {
        SkillLifecycle::Builtin(_) => {}
        SkillLifecycle::PluginManaged(plugin) => {
            return Err(SkillToggleRefusal::plugin_managed(plugin))
        }
        SkillLifecycle::Unknown => return Err(SkillToggleRefusal::unknown(name)),
    }
    let mut state = load_state_in(state_root);
    let changed = if enabled {
        state.disabled.remove(name).is_some()
    } else if state.disabled.contains_key(name) {
        false
    } else {
        state.disabled.insert(name.to_string(), record);
        true
    };
    if !changed {
        return Ok(());
    }
    state.version = STATE_VERSION;
    let path = skills_state_path_in(state_root);
    let parent = path
        .parent()
        .ok_or_else(|| SkillToggleRefusal::io("skills state path has no parent"))?;
    intendant_core::state_paths::create_private_dir_all(parent)
        .map_err(|error| SkillToggleRefusal::io(format!("create {}: {error}", parent.display())))?;
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| SkillToggleRefusal::io(format!("encode skills state: {error}")))?;
    let tmp = path.with_extension("json.tmp");
    intendant_core::state_paths::write_private_file(&tmp, &bytes)
        .map_err(|error| SkillToggleRefusal::io(format!("write {}: {error}", tmp.display())))?;
    std::fs::rename(&tmp, &path)
        .map_err(|error| SkillToggleRefusal::io(format!("rename {}: {error}", path.display())))
}

/// [`set_skill_enabled_in`] against the daemon's own state root.
pub(crate) fn set_skill_enabled(
    name: &str,
    enabled: bool,
    record: DisabledRecord,
) -> Result<(), SkillToggleRefusal> {
    set_skill_enabled_in(
        &intendant_core::state_paths::intendant_home(),
        name,
        enabled,
        record,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_name(index: usize) -> &'static str {
        crate::builtin_skills::BUILTIN_SKILLS[index].name
    }

    fn record(principal: &str, kind: &str, at_ms: u64) -> DisabledRecord {
        DisabledRecord {
            principal: Some(principal.to_string()),
            kind: Some(kind.to_string()),
            at_ms,
            foreign: serde_json::Map::new(),
        }
    }

    #[test]
    fn disable_round_trips_with_attribution_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(
            disabled_skills_in(root).is_empty(),
            "shipped default is nothing disabled (R4)"
        );

        let name = builtin_name(0);
        set_skill_enabled_in(root, name, false, record("principal:dash", "dashboard", 1234))
            .unwrap();
        let disabled = disabled_skills_in(root);
        let entry = disabled.get(name).expect("entry recorded");
        assert_eq!(entry.principal.as_deref(), Some("principal:dash"));
        assert_eq!(entry.kind.as_deref(), Some("dashboard"));
        assert_eq!(entry.at_ms, 1234);

        // A repeat disable is a no-op that keeps the ORIGINAL attribution.
        set_skill_enabled_in(root, name, false, record("principal:other", "peer", 9999)).unwrap();
        assert_eq!(
            disabled_skills_in(root).get(name).unwrap().at_ms,
            1234,
            "the record is who disabled it, not who last asked"
        );

        // Re-enable removes the entry; a repeat enable is a no-op.
        set_skill_enabled_in(root, name, true, DisabledRecord::default()).unwrap();
        assert!(disabled_skills_in(root).is_empty());
        set_skill_enabled_in(root, name, true, DisabledRecord::default()).unwrap();

        // The state file is private (0600) where the platform enforces it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            set_skill_enabled_in(root, name, false, record("p", "dashboard", 1)).unwrap();
            let mode = std::fs::metadata(skills_state_path_in(root))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "state file must be private");
        }
    }

    #[test]
    fn per_kind_refusals_are_named_and_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Plugin-managed: the refusal NAMES the managing plugin (H5 pin —
        // the intake §2b shape, exact text).
        let plugin = &crate::plugin_registry::BUNDLED_PLUGINS[0];
        let payload = plugin.skills[0].name;
        let refusal =
            set_skill_enabled_in(root, payload, false, DisabledRecord::default()).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!(
                "a plugin-materialized skill is managed by its plugin — toggle '{}' ({})",
                plugin.id, plugin.display_name
            )
        );
        assert_eq!(refusal.http_status(), 409);

        // Unknown name: refused by name, 404.
        let refusal =
            set_skill_enabled_in(root, "no-such-skill", false, DisabledRecord::default())
                .unwrap_err();
        assert_eq!(
            refusal.message(),
            "unknown skill 'no-such-skill' — not a builtin or bundled plugin payload"
        );
        assert_eq!(refusal.http_status(), 404);

        // Neither refusal touched the state file.
        assert!(!skills_state_path_in(root).exists());
    }

    /// The mutation gate and the served `lifecycle` body derive from the
    /// SAME classification: a name is toggle-controlled iff the flip
    /// accepts it, plugin-controlled iff the flip refuses toward the
    /// plugin (derive, don't mirror — no client re-derivation possible).
    #[test]
    fn toggle_availability_parity_between_row_and_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let disabled = disabled_skills_in(root);

        let mut names: Vec<&'static str> = crate::builtin_skills::BUILTIN_SKILLS
            .iter()
            .map(|skill| skill.name)
            .collect();
        for plugin in crate::plugin_registry::BUNDLED_PLUGINS {
            names.extend(plugin.skills.iter().map(|skill| skill.name));
        }
        for name in names {
            let control = skill_lifecycle_json(name, &disabled)["control"]
                .as_str()
                .unwrap()
                .to_string();
            let flip = set_skill_enabled_in(root, name, false, DisabledRecord::default());
            match control.as_str() {
                "toggle" => assert!(flip.is_ok(), "{name}: toggle rows must accept the flip"),
                "plugin" => assert!(
                    matches!(flip, Err(SkillToggleRefusal::PluginManaged { .. })),
                    "{name}: plugin rows must refuse toward their plugin"
                ),
                other => panic!("{name}: unexpected control '{other}'"),
            }
            // Leave the set as we found it.
            let _ = set_skill_enabled_in(root, name, true, DisabledRecord::default());
        }
        assert_eq!(
            skill_lifecycle_json("no-such-skill", &disabled)["control"],
            "unknown"
        );
    }

    #[test]
    fn lifecycle_json_carries_enabled_state_and_attribution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let name = builtin_name(0);

        let body = skill_lifecycle_json(name, &disabled_skills_in(root));
        assert_eq!(body["control"], "toggle");
        assert_eq!(body["enabled"], true);
        assert!(body.get("disabled_by").is_none());

        set_skill_enabled_in(root, name, false, record("principal:dash", "dashboard", 77)).unwrap();
        let body = skill_lifecycle_json(name, &disabled_skills_in(root));
        assert_eq!(body["enabled"], false);
        assert_eq!(body["disabled_by"]["principal"], "principal:dash");
        assert_eq!(body["disabled_by"]["kind"], "dashboard");
        assert_eq!(body["disabled_by"]["at_ms"], 77);

        let plugin = &crate::plugin_registry::BUNDLED_PLUGINS[0];
        let body = skill_lifecycle_json(plugin.skills[0].name, &disabled_skills_in(root));
        assert_eq!(body["control"], "plugin");
        assert_eq!(body["plugin_id"], plugin.id);
        assert_eq!(body["plugin_display_name"], plugin.display_name);
    }

    /// The plugin-state preservation discipline, transplanted: entries
    /// whose names this binary does not ship, unknown fields inside a
    /// record, and unknown top-level fields (the S4 user-library seam)
    /// all survive a rewrite; corrupt or foreign-version state reads as
    /// nothing-disabled.
    #[test]
    fn foreign_names_fields_and_versions_follow_the_plugin_discipline() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let name = builtin_name(1);
        set_skill_enabled_in(root, name, false, record("p", "dashboard", 5)).unwrap();

        // A newer binary's entry, record field, and top-level field.
        let path = skills_state_path_in(root);
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["disabled"]["future-user-skill"] =
            serde_json::json!({ "principal": "p2", "at_ms": 9, "reason": "kept" });
        raw["disabled"][name]["future_field"] = serde_json::json!("kept");
        raw["user"] = serde_json::json!([{ "name": "future-lib-entry" }]);
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

        // The foreign entry is visible to the sweep subtraction…
        assert!(disabled_skill_names_in(root).contains("future-user-skill"));

        // …and a rewrite by this binary preserves all three foreigners.
        set_skill_enabled_in(root, builtin_name(2), false, record("p", "dashboard", 6)).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["disabled"]["future-user-skill"]["reason"], "kept");
        assert_eq!(raw["disabled"][name]["future_field"], "kept");
        assert_eq!(raw["user"][0]["name"], "future-lib-entry");

        // Corrupt and foreign-version states read as nothing-disabled (R4).
        std::fs::write(&path, b"not json").unwrap();
        assert!(disabled_skills_in(root).is_empty());
        std::fs::write(
            &path,
            br#"{"version":99,"disabled":{"intendant-cli":{"at_ms":1}}}"#,
        )
        .unwrap();
        assert!(disabled_skills_in(root).is_empty());
    }

    #[test]
    fn disabled_record_maps_the_actor_seam() {
        let binding = crate::access::actor::ActorBinding::from_principal(
            &crate::access::iam::AccessPrincipal::local_loopback_mcp_default("http"),
            None,
        );
        let record = DisabledRecord::from_actor(&binding, 42);
        assert_eq!(record.principal, binding.principal_id);
        assert_eq!(
            record.kind.as_deref(),
            Some(binding.kind.as_str()),
            "actor kind rides the record verbatim"
        );
        assert_eq!(record.at_ms, 42);
    }
}
