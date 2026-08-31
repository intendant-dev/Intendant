use super::*;
use crate::event::EventBus;
use crate::mcp::{McpAppState, SharedMcpState};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

fn test_server(home: &Path) -> IntendantServer {
    let state: SharedMcpState = Arc::new(RwLock::new(McpAppState::new(
        "test".into(),
        "test".into(),
        crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
        home.join(".intendant/logs/test"),
    )));
    IntendantServer::new_with_home(state, EventBus::new(), home.to_path_buf())
}

fn add_user_skill(state_root: &Path, name: &str, body: &str) {
    let skill_md = format!("---\nname: {name}\ndescription: Owner workflow {name}\n---\n{body}\n");
    crate::user_skills::add_user_skill_in(
        state_root,
        name,
        &skill_md,
        crate::skill_state::DisabledRecord::default(),
    )
    .expect("user skill added");
}

#[test]
fn full_catalog_is_unbounded_and_includes_verified_user_skills() {
    let home = tempfile::tempdir().unwrap();
    let state_root = crate::platform::intendant_home_in(home.path());
    add_user_skill(
        &state_root,
        "owner-workflow",
        "Do the owner-specific thing.",
    );
    let server = test_server(home.path());

    let list = server
        .skills_over_mcp_list(&serde_json::json!({}), None)
        .unwrap();
    assert_eq!(list["resultType"], "complete");
    let skills = list["skills"].as_array().unwrap();
    assert!(
        skills.len() > 5,
        "the canonical catalog must not inherit OpenAI's five-skill ceiling"
    );
    let owner = skills
        .iter()
        .find(|skill| skill["frontmatter"]["name"] == "owner-workflow")
        .expect("verified user skill is exported");
    assert_eq!(owner["uri"], "skill://intendant/owner-workflow/SKILL.md");
    assert_eq!(owner["resources"].as_array().unwrap().len(), 1);
    assert!(owner["resources"][0]["size"].as_u64().unwrap() > 0);
}

#[test]
fn get_and_read_return_the_same_manifest_bytes_digest_and_size() {
    let home = tempfile::tempdir().unwrap();
    let server = test_server(home.path());
    let list = server
        .skills_over_mcp_list(&serde_json::json!({}), None)
        .unwrap();
    let first = &list["skills"][0];
    let uri = first["uri"].as_str().unwrap();
    let get = server
        .skills_over_mcp_get(&serde_json::json!({ "uri": uri }), None)
        .unwrap();
    assert_eq!(get["resultType"], "complete");
    assert_eq!(&get["skill"], first);

    let read = server
        .skills_over_mcp_read_resource(&serde_json::json!({ "uri": uri }), None)
        .unwrap();
    let content = &read["contents"][0];
    assert_eq!(content["uri"], uri);
    let text = content["text"].as_str().unwrap();
    let declared = &first["resources"][0];
    assert_eq!(
        declared["digest"],
        catalog::sha256_digest(text.as_bytes())
    );
    assert_eq!(declared["size"], text.len());
    let parsed = intendant_core::skills::parse_skill_md(text, Path::new("served/SKILL.md"))
        .expect("served document parses");
    assert_eq!(
        parsed.0.name,
        first["frontmatter"]["name"].as_str().unwrap()
    );
    assert_eq!(
        parsed.0.description,
        first["frontmatter"]["description"].as_str().unwrap()
    );
}

#[test]
fn openai_profile_packages_every_effective_skill_without_flattening_its_tree() {
    let home = tempfile::tempdir().unwrap();
    let state_root = crate::platform::intendant_home_in(home.path());
    add_user_skill(&state_root, "owner-workflow", "Unique owner body marker.");
    let server = test_server(home.path());
    let list = server
        .skills_over_mcp_list(&serde_json::json!({ "profile": "openai" }), None)
        .unwrap();
    assert_eq!(list["resultType"], "complete");
    let skills = list["skills"].as_array().unwrap();
    assert!(!skills.is_empty());
    assert!(skills.len() <= openai::MAX_PACKAGES);
    assert_eq!(skills[0]["frontmatter"]["name"], openai::AGGREGATE_NAME);

    let mut package_names = HashSet::new();
    let mut all_text = String::new();
    let mut all_uris = Vec::new();
    for skill in skills {
        let name = skill["frontmatter"]["name"].as_str().unwrap();
        assert!(package_names.insert(name.to_string()));
        let package_uri = skill["uri"].as_str().unwrap();
        let get = server
            .skills_over_mcp_get(&serde_json::json!({ "uri": package_uri }), None)
            .unwrap();
        assert_eq!(get["resultType"], "complete");
        assert_eq!(get["skill"], *skill);

        let resources = skill["resources"].as_array().unwrap();
        assert!(resources.len() <= openai::RESOURCE_LIMIT);
        for resource in resources {
            let uri = resource["uri"].as_str().unwrap();
            all_uris.push(uri.to_string());
            let read = server
                .skills_over_mcp_read_resource(&serde_json::json!({ "uri": uri }), None)
                .unwrap();
            let content = &read["contents"][0];
            assert_eq!(content["uri"], uri);
            if let Some(text) = content["text"].as_str() {
                assert_eq!(resource["size"], text.len());
                assert_eq!(
                    resource["digest"],
                    catalog::sha256_digest(text.as_bytes())
                );
                all_text.push_str(text);
            }
        }
    }

    assert!(all_text.contains("# Effective Intendant skill catalog"));
    assert!(all_text.contains("owner-workflow"));
    assert!(all_text.contains("Unique owner body marker."));
    assert!(all_uris.iter().any(|uri| {
        uri.ends_with("/references/skills/intendant-log-search/SKILL.md")
    }));
    assert!(all_uris.iter().any(|uri| {
        uri.ends_with(
            "/references/skills/intendant-log-search/references/query-recipes.md",
        )
    }));
    for builtin in crate::builtin_skills::BUILTIN_SKILLS {
        assert!(
            all_text.contains(builtin.name),
            "OpenAI packages omitted builtin {}",
            builtin.name
        );
    }
}

#[test]
fn openai_package_uris_select_the_profile_without_repeating_a_parameter() {
    for uri in [
        "skill://intendant/intendant-skills/SKILL.md",
        "skill://intendant/intendant-skills/references/catalog.md",
        "skill://intendant/intendant-skills-2/SKILL.md",
        "skill://intendant/intendant-skills-5/references/skills/example/SKILL.md",
    ] {
        assert_eq!(profile_for_uri(uri), Some("openai"), "{uri}");
    }
    for uri in [
        "skill://intendant/intendant-skills-6/SKILL.md",
        "skill://intendant/intendant-skills-evil/SKILL.md",
        "skill://intendant/intendant-skills-not-a-resource",
        "skill://intendant/ordinary/SKILL.md",
    ] {
        assert_eq!(profile_for_uri(uri), None, "{uri}");
    }
}

#[test]
fn cursor_and_resource_path_validation_fail_closed() {
    assert!(parse_cursor(&serde_json::json!({ "cursor": 7 })).is_err());
    assert!(parse_cursor(&serde_json::json!({ "cursor": "wat" })).is_err());
    for bad in [
        "",
        "/absolute",
        "../parent",
        "a/../b",
        "a\\b",
        "a//b",
        "a b",
        "a/%2e.md",
    ] {
        assert!(catalog::validate_resource_path(bad).is_err(), "{bad:?}");
    }
    assert!(catalog::validate_resource_path("references/good.md").is_ok());
}
