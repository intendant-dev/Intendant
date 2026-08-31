use super::*;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

const SKILL_URI_AUTHORITY: &str = "intendant";

pub(super) fn effective_skill_catalog(state_root: &Path) -> Result<Vec<ServedSkill>, String> {
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
    validate_skill_name(expected_name)?;
    let (frontmatter, normalized_skill_md) = normalized_skill_document(expected_name, skill_md)?;
    let uri = skill_uri(expected_name, "SKILL.md");
    let mut resources = vec![ServedResource {
        uri: uri.clone(),
        relative_path: "SKILL.md".to_string(),
        bytes: normalized_skill_md.into_bytes(),
        mime_type: "text/markdown".to_string(),
    }];
    let mut seen_paths = HashSet::from(["skill.md".to_string()]);
    for (relative_path, bytes) in support_files {
        validate_resource_path(&relative_path)?;
        if !seen_paths.insert(relative_path.to_ascii_lowercase()) {
            return Err(format!(
                "skill {expected_name:?} contains a normalization-conflicting resource path {relative_path:?}"
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
/// `SKILL.md` therefore cannot drift on whitespace, quoting, or block-scalar
/// formatting while every original frontmatter key and instruction body are
/// preserved.
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
    if frontmatter
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_none_or(|description| description.is_empty())
    {
        return Err(format!(
            "{}: frontmatter description must be a non-empty string",
            source.display()
        ));
    }

    let mut normalized = String::from("---\n");
    for (key, value) in ordered {
        write_yaml_entry(&mut normalized, &key, &value)?;
    }
    normalized.push_str("---\n");
    normalized.push_str(body);
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }
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

pub(super) fn write_yaml_entry(out: &mut String, key: &str, value: &Value) -> Result<(), String> {
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

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("unsafe skill name {name:?}"));
    }
    Ok(())
}

pub(super) fn validate_resource_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || !component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(format!("unsafe skill resource path {path:?}"));
    }
    Ok(())
}

pub(super) fn skill_uri(name: &str, relative_path: &str) -> String {
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

pub(super) fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
