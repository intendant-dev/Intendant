//! Skills Over MCP: the live effective Intendant skill catalog.
//!
//! The transport speaks the draft `io.modelcontextprotocol/skills` extension
//! without making that draft the source of truth. The same effective set the
//! daemon materializes—enabled builtins, active plugin payloads, and verified
//! owner-added skills—is projected into `skills/list`, `skills/get`, and
//! `resources/read`. The OpenAI adapter folds that unbounded logical catalog
//! into one package so the provider's five-skill intake ceiling never becomes
//! an Intendant catalog ceiling.

use base64::Engine as _;
use serde_json::{Map, Value};
use std::path::PathBuf;

use crate::mcp::IntendantServer;

mod catalog;
mod openai;
#[cfg(test)]
mod tests;

const SKILLS_PAGE_SIZE: usize = 50;

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
        serde_json::json!({
            "uri": self.uri,
            "frontmatter": self.frontmatter,
            "resources": self
                .resources
                .iter()
                .map(|resource| serde_json::json!({
                    "uri": resource.uri,
                    "digest": catalog::sha256_digest(&resource.bytes),
                }))
                .collect::<Vec<_>>(),
        })
    }

    fn resource(&self, uri: &str) -> Option<&ServedResource> {
        self.resources.iter().find(|resource| resource.uri == uri)
    }
}

impl IntendantServer {
    /// Paginated Skills Over MCP catalog. `profile=openai`—or the endpoint's
    /// `skill_profile=openai` query—returns one aggregate package containing
    /// every effective skill, avoiding OpenAI's current named-skill ceiling
    /// without truncating the logical catalog.
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

    /// Return one complete skill entry by its catalog URI. Synthetic profile
    /// URIs are self-identifying, so a follow-up need not repeat the profile
    /// parameter used on `skills/list`.
    pub(crate) fn skills_over_mcp_get(
        &self,
        params: &Value,
        skill_profile: Option<&str>,
    ) -> Result<Value, String> {
        let uri = required_string(params, "uri")?;
        let profile = profile_for_uri(uri).or_else(|| requested_profile(params, skill_profile));
        let served = self.skills_over_mcp_catalog(profile)?;
        let skill = served
            .iter()
            .find(|skill| skill.uri == uri)
            .ok_or_else(|| format!("unknown skill URI {uri:?}"))?;
        Ok(serde_json::json!({ "skill": skill.catalog_json() }))
    }

    /// Read exactly one resource named by a listed skill manifest. Synthetic
    /// aggregate resource URIs likewise select their own profile.
    pub(crate) fn skills_over_mcp_read_resource(
        &self,
        params: &Value,
        skill_profile: Option<&str>,
    ) -> Result<Value, String> {
        let uri = required_string(params, "uri")?;
        let profile = profile_for_uri(uri).or_else(|| requested_profile(params, skill_profile));
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
        let effective = catalog::effective_skill_catalog(&self.skills_over_mcp_state_root())?;
        if openai_profile(skill_profile) {
            Ok(vec![openai::aggregate_skill(&effective)?])
        } else {
            Ok(effective)
        }
    }

    /// Production follows the process state-root seam, including an
    /// `INTENDANT_HOME` override. Tests constructed with `new_with_home`
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

fn profile_for_uri(uri: &str) -> Option<&'static str> {
    uri.starts_with(&format!(
        "skill://intendant/{}/",
        openai::AGGREGATE_NAME
    ))
    .then_some("openai")
}

fn openai_profile(profile: Option<&str>) -> bool {
    profile.is_some_and(|profile| {
        matches!(
            profile.trim().to_ascii_lowercase().as_str(),
            "openai" | "openai-import"
        )
    })
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
