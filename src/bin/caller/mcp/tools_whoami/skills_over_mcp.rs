//! Skills Over MCP: the live effective Intendant skill catalog.
//
// The transport speaks the draft `io.modelcontextprotocol/skills` extension
// without making that draft the source of truth. The same effective set the
// daemon materializes (enabled builtins, active plugin payloads, verified
// owner-added skills) is projected into `skills/list`, `skills/get`, and
// `resources/read`. The OpenAI import adapter deliberately folds that
// unbounded logical catalog into one package so the provider's five-skill
// intake ceiling never becomes an Intendant catalog ceiling.

use crate::mcp::IntendantServer;
use base64::Engine as _;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SKILL_URI_AUTHORITY: &str = "intendant";
const SKILLS_PAGE_SIZE: usize = 50;
const OPENAI_AGGREGATE_NAME: &str = "intendant-skills";
const OPENAI_SKILL_MD_LIMIT: usize = 256 * 1024;
const OPENAI_SUPPORT_FILE_LIMIT: usize = 1024 * 1024;
const OPENAI_SKILL_TOTAL_LIMIT: usize = 5 * 1024 * 1024;
const OPENAI_RESOURCE_LIMIT: usize = 100;
// Leave headroom for headings added around each source document.
const OPENAI_CHUNK_TARGET: usize = 900 * 1024;

#[derive(Clone, Debug)]
struct ServedResource {
    uri: String,
    relative_path: String,
    bytes: Vec<u8>,
    mime_type: String,
}

#[derive(Clone, Debug)]
struct ServedSkill {
    name: String,
    uri: String,
    frontmatter: Map<String, Value>,
    resources: Vec<ServedResource>,
}

impl ServedSkill {
    fn catalog_json(&self) -> Value {
        Value::Object(Map::from_iter([
            ("uri".to_string(), Value::String(self.uri.clone())),
            (
                "frontmatter".to_string(),
                Value::Object(self.frontmatter.clone()),
            ),
            (
                "resources".to_string(),
                Value::Array(
                    self.resources
                        .iter()
                        .map(|resource| {
                            serde_json::json!({
                                "uri": resource.uri,
                                "digest": sha256_digest(&resource.bytes),
                            })
                        })
                        .collect(),
                ),
            ),
        ]))
    }

    fn resource(&self, uri: &str) -> Option<&ServedResource> {
        self.resources.iter().find(|resource| resource.uri == uri)
    }
}

impl IntendantServer {
    /// The paginated Skills Over MCP catalog. `profile=openai` (or the
    /// endpoint query's `skill_profile=openai`) returns one aggregate skill
    /// package containing every effective skill, avoiding OpenAI's current
    /// five-skill import ceiling without truncating the logical catalog.
    pub(crate) fn skills_over_mcp_list(
        &self,
        params: &Value,
        skill_profile: Option<&str>,
    ) -> Result<Value, String> {
        let profile = requested_profile(params, skill_profile);
        let served = self.skills_over_mcp_catalog(profile)?;
        let cursor = parse_cursor(params)?;
        if cursor > served.len() {
            return Err(format!(
                "skills/list cursor {cursor} is past the catalog end ({})",
                served.len()
            ));
        }
        let end = cursor.saturating_add(SKILLS_PAGE_SIZE).min(served.len());
        let mut out = serde_json::json!({
            "skills": served[cursor..end]
                .iter()
                .map(ServedSkill::catalog_json)
                .collect::<Vec<_>>(),
        });
        if end < served.len() {
            out["nextCursor"] = Value::String(end.to_string());
        }
        Ok(out)
    }

    /// Return one complete skill entry by its catalog URI.
    pub(crate) fn skills_over_mcp_get(
        &self,
        params: &Value,
        skill_profile: Option<&str>,
    ) -> Result<Value, String> {
        let uri = required_string(params, "uri")?;
        let profile = requested_profile(params, skill_profile);
        let served = self.skills_over_mcp_catalog(profile)?;
        let skill = served
            .iter()
            .find(|skill| skill.uri == uri)
            .ok_or_else(|| format!("unknown skill URI {uri:?}"))?;
        Ok(serde_json::json!({ "skill": skill.catalog_json() }))
    }

    /// Read exactly one resource named by a listed skill manifest.
    pub(crate) fn skills_over_mcp_read_resource(
        &self,
        params: &Value,
        skill_profile: Option<&str>,
    ) -> Result<Value, String> {
        let uri = required_string(params, "uri")?;
        let profile = requested_profile(params, skill_profile);
        let served = self.skills_over_mcp_catalog(profile)?;
        let resource = served
            .iter()
            .find_map(|skill| skill.resource(uri))
            .ok_or_else(|| format!("unknown skill resource URI {uri:?}"))?;
        let content = match std::str::from_utf8(&resource.bytes) {
            Ok(text) => serde_json::json!({
                "uri": resource.uri,
                "mimeType": resource.mime_type,
                "text": text,
            }),
            Err(_) => serde_json::json!({
                "uri": resource.uri,
                "mimeType": resource.mime_type,
                "blob": base64::engine::general_purpose::STANDARD.encode(&resource.bytes),
            }),
        };
        Ok(serde_json::json!({ "contents": [content] }))
    }

    fn skills_over_mcp_catalog(
        &self,
        skill_profile: Option<&str>,
    ) -> Result<Vec<ServedSkill>, String> {
        let effective = effective_skill_catalog(&self.skills_over_mcp_state_root())?;
        if openai_aggregate_profile(skill_profile) {
            Ok(vec![openai_aggregate_skill(&effective)?])
        } else {
            Ok(effective)
        }
    }

    /// Production follows the process state-root seam (including an
    /// `INTENDANT_HOME` override); tests constructed with `new_with_home`
    /// remain hermetic under that injected home.
    fn skills_over_mcp_state_root(&self) -> PathBuf {
        if self.home == crate::platform::home_dir() {
            intendant_core::state_paths::intendant_home()
        } else {
            crate::platform::intendant_home_in(&self.home)
        }
    }
}

fn requested_profile<'a>(params: &'a Value, endpoint_profile: Option<&'a str>) -> Option<&'a str> {
    endpoint_profile.or_else(|| {
        params
            .get("profile")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
    })
}

fn openai_aggregate_profile(profile: Option<&str>) -> bool {
    profile
        .map(str::trim)
        .is_some_and(|profile| matches!(profile, "openai" | "openai-import"))
}

fn parse_cursor(params: &Value) -> Result<usize, String> {
    let Some(cursor) = params.get("cursor") else {
        return Ok(0);
    };
    let cursor = cursor
        .as_str()
        .ok_or_else(|| "skills/list cursor must be a decimal string".to_string())?;
    cursor
        .parse::<usize>()
        .map_err(|_| format!("invalid skills/list cursor {cursor:?}"))
}

fn required_string<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing {key}: expected a non-empty string"))
}

fn effective_skill_catalog(state_root: &Path) -> Result<Vec<ServedSkill>, String> {
    let disabled = crate::skill_state::disabled_skill_names_in(state_root);
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for skill in crate::builtin_skills::BUILTIN_SKILLS {
        if disabled.contains(skill.name) || !seen.insert(skill.name.to_string()) {
            continue;
        }
        out.push(served_skill(
            skill.name,
            skill.skill_md,
            skill
                .support_files
                .iter()
                .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
                .collect(),
        )?);
    }

    // Plugin payloads have one lifecycle authority: the plugin toggle.
    // `active_plugin_skills_in` already filters enabled + ready plugins;
    // the per-skill disabled set deliberately does not apply to this half.
    for (_plugin_id, skill) in crate::plugin_registry::active_plugin_skills_in(state_root) {
        if !seen.insert(skill.name.to_string()) {
            continue;
        }
        out.push(served_skill(
            skill.name,
            skill.skill_md,
            skill
                .support_files
                .iter()
                .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
                .collect(),
        )?);
    }

    for skill in crate::user_skills::active_user_skill_payloads_in(state_root) {
        if disabled.contains(&skill.name) || !seen.insert(skill.name.clone()) {
            continue;
        }
        out.push(served_skill(&skill.name, &skill.skill_md, Vec::new())?);
    }

    Ok(out)
}

fn served_skill(
    expected_name: &str,
    skill_md: &str,
    support_files: Vec<(String, Vec<u8>)>,
) -> Result<ServedSkill, String> {
    let (frontmatter, normalized_skill_md) = normalized_skill_document(expected_name, skill_md)?;
    let uri = skill_uri(expected_name, "SKILL.md");
    let mut resources = vec![ServedResource {
        uri: uri.clone(),
        relative_path: "SKILL.md".to_string(),
        bytes: normalized_skill_md.into_bytes(),
        mime_type: "text/markdown".to_string(),
    }];
    let mut seen_paths = HashSet::from(["SKILL.md".to_string()]);
    for (relative_path, bytes) in support_files {
        validate_resource_path(&relative_path)?;
        if !seen_paths.insert(relative_path.clone()) {
            return Err(format!(
                "skill {expected_name:?} contains duplicate resource path {relative_path:?}"
            ));
        }
        resources.push(ServedResource {
            uri: skill_uri(expected_name, &relative_path),
            mime_type: mime_type_for(&relative_path).to_string(),
            relative_path,
            bytes,
        });
    }
    Ok(ServedSkill {
        name: expected_name.to_string(),
        uri,
        frontmatter,
        resources,
    })
}

/// Parse the Agent Skills conforming frontmatter subset, then re-emit a
/// canonical document from that parsed object. The catalog and fetched
/// SKILL.md therefore cannot drift on whitespace, quoting, or block-scalar
/// formatting while every original frontmatter key and the instruction body
/// are preserved.
fn normalized_skill_document(
    expected_name: &str,
    skill_md: &str,
) -> Result<(Map<String, Value>, String), String> {
    let source = Path::new(expected_name).join("SKILL.md");
    let (yaml, body) = intendant_core::skills::split_frontmatter(skill_md, &source)?;
    let entries = intendant_core::skills::parse_frontmatter_strict(yaml)
        .map_err(|error| format!("{}: {error}", source.display()))?;
    let mut frontmatter = Map::new();
    let mut ordered = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let json = match value {
            intendant_core::skills::FrontmatterValue::Scalar(value) => {
                scalar_frontmatter_value(&key, value)?
            }
            intendant_core::skills::FrontmatterValue::Map(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            ),
        };
        frontmatter.insert(key.clone(), json.clone());
        ordered.push((key, json));
    }

    let name = frontmatter
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: frontmatter name must be a string", source.display()))?;
    if name != expected_name {
        return Err(format!(
            "{}: frontmatter name {name:?} does not match catalog name {expected_name:?}",
            source.display()
        ));
    }
    let description = frontmatter
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .ok_or_else(|| {
            format!(
                "{}: frontmatter description must be a non-empty string",
                source.display()
            )
        })?;

    let mut normalized = String::from("---\n");
    for (key, value) in ordered {
        write_yaml_entry(&mut normalized, &key, &value)?;
    }
    normalized.push_str("---\n");
    normalized.push_str(body);
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }

    debug_assert!(!description.is_empty());
    Ok((frontmatter, normalized))
}

fn scalar_frontmatter_value(key: &str, value: String) -> Result<Value, String> {
    match key {
        "disable-auto-invocation" | "disable_auto_invocation" | "sandbox" => match value.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            other => Err(format!("{key}: expected true or false, got {other:?}")),
        },
        _ => Ok(Value::String(value)),
    }
}

fn write_yaml_entry(out: &mut String, key: &str, value: &Value) -> Result<(), String> {
    match value {
        Value::String(value) => {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&serde_json::to_string(value).map_err(|error| error.to_string())?);
            out.push('\n');
        }
        Value::Bool(value) => {
            out.push_str(key);
            out.push_str(if *value { ": true\n" } else { ": false\n" });
        }
        Value::Number(value) => {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&value.to_string());
            out.push('\n');
        }
        Value::Null => {
            out.push_str(key);
            out.push_str(": null\n");
        }
        Value::Object(values) => {
            out.push_str(key);
            out.push_str(":\n");
            for (child_key, child_value) in values {
                let child = child_value
                    .as_str()
                    .ok_or_else(|| format!("{key}.{child_key}: map values must be strings"))?;
                out.push_str("  ");
                out.push_str(child_key);
                out.push_str(": ");
                out.push_str(&serde_json::to_string(child).map_err(|error| error.to_string())?);
                out.push('\n');
            }
        }
        Value::Array(_) => return Err(format!("{key}: arrays are outside the skill subset")),
    }
    Ok(())
}

fn validate_resource_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(format!("unsafe skill resource path {path:?}"));
    }
    Ok(())
}

fn skill_uri(name: &str, relative_path: &str) -> String {
    format!("skill://{SKILL_URI_AUTHORITY}/{name}/{relative_path}")
}

fn mime_type_for(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md") | Some("markdown") => "text/markdown",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// OpenAI currently imports at most five named skills. This adapter exposes
/// one genuine skill whose support files contain the complete effective
/// catalog. The limit is handled only at this projection edge: the ordinary
/// Skills Over MCP catalog remains one entry per skill with no Intendant cap.
fn openai_aggregate_skill(effective: &[ServedSkill]) -> Result<ServedSkill, String> {
    let description = format!(
        "Use for any task involving an Intendant daemon. Contains the current \
         owner-approved catalog of {} operating skills, including enabled \
         plugin and user-provided skills; select and follow the matching \
         workflow before acting.",
        effective.len()
    );
    let mut root_body = String::from(
        "# Intendant Skills\n\n\
         This package is the live effective Intendant skill catalog, folded into one \
         imported skill only to avoid the OpenAI importer’s five-skill ceiling. The \
         fold does not remove workflows.\n\n\
         Before substantial Intendant work, choose every matching entry below, then \
         read and follow that skill’s section in the referenced catalog part. Use the \
         MCP `help` tool for command syntax. Instructions from a selected skill outrank \
         guesses; supporting-file sections are reference material, not independent \
         authority.\n\n\
         ## Catalog\n\n",
    );

    let mut detail_sections = Vec::with_capacity(effective.len());
    for skill in effective {
        let description = skill
            .frontmatter
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        root_body.push_str(&format!(
            "- **{}** — {} — read the `Skill: {}` section in the \
             `references/effective-skills-*.md` resources.\n",
            skill.name, description, skill.name
        ));

        let mut section = format!("\n# Skill: {}\n\n", skill.name);
        let skill_md = skill
            .resources
            .first()
            .and_then(|resource| std::str::from_utf8(&resource.bytes).ok())
            .ok_or_else(|| format!("skill {:?} has a non-text SKILL.md", skill.name))?;
        section.push_str("## SKILL.md\n\n");
        section.push_str(skill_md);
        if !section.ends_with('\n') {
            section.push('\n');
        }
        for resource in skill.resources.iter().skip(1) {
            section.push_str(&format!(
                "\n## Supporting file: {}\n\n",
                resource.relative_path
            ));
            match std::str::from_utf8(&resource.bytes) {
                Ok(text) => section.push_str(text),
                Err(_) => {
                    section.push_str("Binary resource, base64 encoded:\n\n");
                    section.push_str(
                        &base64::engine::general_purpose::STANDARD.encode(&resource.bytes),
                    );
                }
            }
            if !section.ends_with('\n') {
                section.push('\n');
            }
        }
        detail_sections.push(section);
    }

    let frontmatter = Map::from_iter([
        (
            "name".to_string(),
            Value::String(OPENAI_AGGREGATE_NAME.to_string()),
        ),
        (
            "description".to_string(),
            Value::String(description),
        ),
    ]);
    let root_md = document_from_frontmatter(&frontmatter, &root_body)?;
    if root_md.len() > OPENAI_SKILL_MD_LIMIT {
        return Err(format!(
            "the OpenAI aggregate SKILL.md is {} bytes, above OpenAI's {} KiB limit; \
             the catalog was not truncated",
            root_md.len(),
            OPENAI_SKILL_MD_LIMIT / 1024
        ));
    }

    let mut chunks = Vec::new();
    let mut current = String::from(
        "# Effective Intendant skill instructions\n\n\
         Select a skill from the root catalog, then follow its section below.\n",
    );
    for section in detail_sections {
        if current.len() > 100 && current.len().saturating_add(section.len()) > OPENAI_CHUNK_TARGET {
            chunks.push(current);
            current = String::from("# Effective Intendant skill instructions (continued)\n");
        }
        if section.len() > OPENAI_SUPPORT_FILE_LIMIT {
            return Err(format!(
                "one aggregated skill section is {} bytes, above OpenAI's 1 MiB \
                 supporting-file limit; the skill was not truncated",
                section.len()
            ));
        }
        current.push_str(&section);
    }
    if current.len() > 100 || chunks.is_empty() {
        chunks.push(current);
    }
    if chunks.len().saturating_add(1) > OPENAI_RESOURCE_LIMIT {
        return Err(format!(
            "the OpenAI aggregate requires {} resources, above OpenAI's {}-file \
             limit; the catalog was not truncated",
            chunks.len() + 1,
            OPENAI_RESOURCE_LIMIT
        ));
    }

    let uri = skill_uri(OPENAI_AGGREGATE_NAME, "SKILL.md");
    let mut resources = vec![ServedResource {
        uri: uri.clone(),
        relative_path: "SKILL.md".to_string(),
        bytes: root_md.into_bytes(),
        mime_type: "text/markdown".to_string(),
    }];
    for (index, chunk) in chunks.into_iter().enumerate() {
        if chunk.len() > OPENAI_SUPPORT_FILE_LIMIT {
            return Err(format!(
                "OpenAI aggregate chunk {} is {} bytes, above the 1 MiB limit; \
                 the catalog was not truncated",
                index + 1,
                chunk.len()
            ));
        }
        let relative_path = format!("references/effective-skills-{:03}.md", index + 1);
        resources.push(ServedResource {
            uri: skill_uri(OPENAI_AGGREGATE_NAME, &relative_path),
            relative_path,
            bytes: chunk.into_bytes(),
            mime_type: "text/markdown".to_string(),
        });
    }
    let total: usize = resources.iter().map(|resource| resource.bytes.len()).sum();
    if total > OPENAI_SKILL_TOTAL_LIMIT {
        return Err(format!(
            "the complete OpenAI aggregate is {} bytes, above OpenAI's 5 MiB \
             per-skill limit; the catalog was not truncated",
            total
        ));
    }

    Ok(ServedSkill {
        name: OPENAI_AGGREGATE_NAME.to_string(),
        uri,
        frontmatter,
        resources,
    })
}

fn document_from_frontmatter(
    frontmatter: &Map<String, Value>,
    body: &str,
) -> Result<String, String> {
    let mut document = String::from("---\n");
    for (key, value) in frontmatter {
        write_yaml_entry(&mut document, key, value)?;
    }
    document.push_str("---\n");
    document.push_str(body);
    if !document.ends_with('\n') {
        document.push('\n');
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::mcp::{McpAppState, SharedMcpState};
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
        let skill_md = format!(
            "---\nname: {name}\ndescription: Owner workflow {name}\n---\n{body}\n"
        );
        crate::user_skills::add_user_skill_in(
            state_root,
            name,
            &skill_md,
            crate::skill_state::DisabledRecord::default(),
        )
        .expect("user skill added");
    }

    #[test]
    fn full_catalog_is_paginated_and_includes_verified_user_skills() {
        let home = tempfile::tempdir().unwrap();
        let state_root = crate::platform::intendant_home_in(home.path());
        add_user_skill(&state_root, "owner-workflow", "Do the owner-specific thing.");
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
        assert!(owner["resources"].as_array().unwrap().len() == 1);
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
        assert_eq!(declared, sha256_digest(text.as_bytes()));
        let parsed = intendant_core::skills::parse_skill_md(text, Path::new("served/SKILL.md"))
            .expect("served document parses");
        assert_eq!(parsed.0.name, first["frontmatter"]["name"].as_str().unwrap());
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
            .skills_over_mcp_list(&serde_json::json!({}), Some("openai"))
            .unwrap();
        let skills = list["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["frontmatter"]["name"], OPENAI_AGGREGATE_NAME);
        let resources = skills[0]["resources"].as_array().unwrap();
        assert!(resources.len() >= 2);
        assert!(resources.len() <= OPENAI_RESOURCE_LIMIT);

        let mut all_text = String::new();
        for resource in resources {
            let uri = resource["uri"].as_str().unwrap();
            let read = server
                .skills_over_mcp_read_resource(
                    &serde_json::json!({ "uri": uri }),
                    Some("openai"),
                )
                .unwrap();
            all_text.push_str(read["contents"][0]["text"].as_str().unwrap());
        }
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
        for bad in ["", "/absolute", "../parent", "a/../b", "a\\b", "a//b"] {
            assert!(validate_resource_path(bad).is_err(), "{bad:?}");
        }
        assert!(validate_resource_path("references/good.md").is_ok());
    }
}
