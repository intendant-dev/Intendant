//! The unified skill catalog: the one derived body `GET /api/skills`
//! (tunnel twin `api_skills_list`) serves.
//!
//! Registry-driven by construction — rows come from
//! [`crate::builtin_skills::BUILTIN_SKILLS`] and the payloads of
//! [`crate::plugin_registry::BUNDLED_PLUGINS`], never from enumerating the
//! installed roots: on-disk directories the registries do not know
//! (user-owned copies, dev-tier symlinks) are structurally out of frame,
//! exactly as they are for the plugin catalog. Per-root install facts are
//! per-entry reads ([`crate::skill_install::skill_install_status_in`]).
//! Everything the dashboard renders derives from this body; the frontend
//! holds no skill vocabulary of its own.

use std::path::Path;

use crate::builtin_skills::BuiltinSkill;

/// One catalog row: identity, kind, provenance, ambient description,
/// trust-posture line (derived from provenance, never free text), and
/// per-root install facts in the installer's own status vocabulary.
/// Plugin-payload rows also carry `plugin_id` so the dashboard can link
/// the row to the plugin card that owns its lifecycle — the one-authority
/// rule made visible.
fn skill_row_json(
    provenance: String,
    plugin: Option<&'static crate::plugin_registry::BundledPlugin>,
    skill: &'static BuiltinSkill,
    home: &Path,
) -> serde_json::Value {
    let description = intendant_core::skills::parse_skill_md(skill.skill_md, Path::new(skill.name))
        .map(|(config, _)| config.description)
        .unwrap_or_default();
    let trust_posture = match plugin {
        None => "Shipped bytes, parity-pinned in the daemon binary; installed unconditionally."
            .to_string(),
        Some(plugin) => format!(
            "Shipped via plugin '{}'; installed only while that plugin is enabled and ready.",
            plugin.display_name
        ),
    };
    let roots: serde_json::Map<String, serde_json::Value> =
        crate::skill_install::skill_install_status_in(home, skill)
            .into_iter()
            .map(|(root, status)| (root.to_string(), serde_json::json!(status)))
            .collect();
    let mut row = serde_json::json!({
        "name": skill.name,
        "kind": "skill",
        "provenance": provenance,
        "description": description,
        "trust_posture": trust_posture,
        "roots": roots,
    });
    if let Some(plugin) = plugin {
        row["plugin_id"] = serde_json::json!(plugin.id);
    }
    row
}

/// The full catalog as one JSON body: every skill the daemon manages, one
/// row each — builtins in table order, then every bundled plugin's
/// payloads in registry order (listed whether or not the plugin is
/// currently enabled: the row's install facts and its plugin card carry
/// the live state; the registry is what exists).
pub(crate) fn skills_catalog_json_in(home: &Path) -> serde_json::Value {
    let mut rows: Vec<serde_json::Value> = crate::builtin_skills::BUILTIN_SKILLS
        .iter()
        .map(|skill| skill_row_json("builtin".to_string(), None, skill, home))
        .collect();
    for plugin in crate::plugin_registry::BUNDLED_PLUGINS {
        for skill in plugin.skills {
            rows.push(skill_row_json(
                format!("plugin:{}", plugin.id),
                Some(plugin),
                skill,
                home,
            ));
        }
    }
    serde_json::json!({ "skills": rows })
}

/// [`skills_catalog_json_in`] against the daemon's own home.
pub(crate) fn skills_catalog_json() -> serde_json::Value {
    let home = dirs::home_dir().unwrap_or_default();
    skills_catalog_json_in(&home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The S1 parity pin: the served row id set is exactly
    /// `BUILTIN_SKILLS` ∪ the bundled plugins' payloads — nothing
    /// enumerated from disk, nothing dropped, nothing duplicated — and
    /// every row carries the §2a facts (kind, provenance, description,
    /// trust posture, both install roots).
    #[test]
    fn served_row_id_set_is_builtins_union_plugin_payloads() {
        let home = tempfile::tempdir().unwrap();
        let body = skills_catalog_json_in(home.path());
        let rows = body["skills"].as_array().expect("skills array");

        let mut expected = BTreeSet::new();
        for skill in crate::builtin_skills::BUILTIN_SKILLS {
            expected.insert(skill.name.to_string());
        }
        for plugin in crate::plugin_registry::BUNDLED_PLUGINS {
            for skill in plugin.skills {
                expected.insert(skill.name.to_string());
            }
        }
        let served: BTreeSet<String> = rows
            .iter()
            .map(|row| row["name"].as_str().expect("row name").to_string())
            .collect();
        assert_eq!(
            served, expected,
            "served skill ids drifted from the registries"
        );
        assert_eq!(
            rows.len(),
            expected.len(),
            "one row per registry entry — no duplicates"
        );

        let plugin_payload_names: BTreeSet<&str> = crate::plugin_registry::BUNDLED_PLUGINS
            .iter()
            .flat_map(|plugin| plugin.skills.iter().map(|skill| skill.name))
            .collect();
        for row in rows {
            let name = row["name"].as_str().unwrap();
            assert_eq!(row["kind"], "skill");
            assert!(
                !row["description"].as_str().unwrap().trim().is_empty(),
                "{name}: description is the ambient catalog line and must be non-empty"
            );
            assert!(
                !row["trust_posture"].as_str().unwrap().trim().is_empty(),
                "{name}: trust posture line must be derived, never absent"
            );
            let roots = row["roots"].as_object().unwrap();
            assert!(
                roots.contains_key("~/.agents/skills") && roots.contains_key("~/.claude/skills"),
                "{name}: both independent install roots must report"
            );
            // Hermetic home: the catalog reads only per-entry facts, so a
            // fresh home is uniformly absent — proof no real machine
            // state leaked into the derivation.
            assert!(
                roots.values().all(|status| status == "absent"),
                "{name}: fresh home must report absent, got {roots:?}"
            );
            if plugin_payload_names.contains(name) {
                let provenance = row["provenance"].as_str().unwrap();
                let plugin_id = row["plugin_id"]
                    .as_str()
                    .expect("plugin rows carry plugin_id");
                assert_eq!(
                    provenance,
                    format!("plugin:{plugin_id}"),
                    "{name}: plugin provenance names its managing plugin"
                );
                assert!(
                    crate::plugin_registry::bundled_plugin(plugin_id).is_some(),
                    "{name}: plugin_id must resolve in the bundled registry"
                );
            } else {
                assert_eq!(row["provenance"], "builtin");
                assert!(
                    row.get("plugin_id").is_none(),
                    "{name}: builtin rows carry no plugin link"
                );
            }
        }
    }
}
