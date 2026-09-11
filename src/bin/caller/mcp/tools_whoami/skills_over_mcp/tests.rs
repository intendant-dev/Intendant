use super::*;
use crate::event::EventBus;
use crate::mcp::{McpAppState, SharedMcpState};
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
}

#[test]
fn get_and_read_return_the_same_manifest_bytes_and_digest() {
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
    assert_eq!(&get["skill"], first);

    let read = server
        .skills_over_mcp_read_resource(&serde_json::json!({ "uri": uri }), None)
        .unwrap();
    let content = &read["contents"][0];
    assert_eq!(content["uri"], uri);
    let text = content["text"].as_str().unwrap();
    let declared = first["resources"][0]["digest"].as_str().unwrap();
    assert_eq!(declared, catalog::sha256_digest(text.as_bytes()));
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
fn openai_profile_folds_every_effective_skill_into_one_importable_package() {
    let home = tempfile::tempdir().unwrap();
    let state_root = crate::platform::intendant_home_in(home.path());
    add_user_skill(&state_root, "owner-workflow", "Unique owner body marker.");
    let server = test_server(home.path());
    let list = server
        .skills_over_mcp_list(&serde_json::json!({ "profile": "openai" }), None)
        .unwrap();
    let skills = list["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["frontmatter"]["name"], openai::AGGREGATE_NAME);
    let aggregate_uri = skills[0]["uri"].as_str().unwrap();
    let get = server
        .skills_over_mcp_get(&serde_json::json!({ "uri": aggregate_uri }), None)
        .unwrap();
    assert_eq!(get["skill"], skills[0]);

    let resources = skills[0]["resources"].as_array().unwrap();
    assert!(resources.len() >= 2);
    assert!(resources.len() <= openai::RESOURCE_LIMIT);

    let mut all_text = String::new();
    for resource in resources {
        let uri = resource["uri"].as_str().unwrap();
        let read = server
            .skills_over_mcp_read_resource(&serde_json::json!({ "uri": uri }), None)
            .unwrap();
        all_text.push_str(read["contents"][0]["text"].as_str().unwrap());
    }
    assert!(all_text.contains("# Effective Intendant skill catalog"));
    assert!(all_text.contains("owner-workflow"));
    assert!(all_text.contains("Unique owner body marker."));
    for builtin in crate::builtin_skills::BUILTIN_SKILLS {
        assert!(
            all_text.contains(builtin.name),
            "aggregate omitted builtin {}",
            builtin.name
        );
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
