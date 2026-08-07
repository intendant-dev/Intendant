//! Bundled-plugin catalog over HTTP: the dashboard's read of the plugin
//! registry (identity, enabled flag, derived lifecycle state, readiness
//! layers, per-skill install facts) and the enable/disable toggle. The
//! toggle persists state, reconciles skill materialization in the same
//! request, and reports what actually happened — it never claims an
//! activation the installer did not perform.

use super::*;

/// Transport-neutral core of `GET /api/plugins` (tunnel twin
/// `api_plugins_list`): one derived body the dashboard renders verbatim.
/// Registry, readiness, and install facts are all read fresh — disk and
/// env are the source of truth, never a cache.
pub(crate) async fn plugins_list_api_response() -> ApiResponse {
    match tokio::task::spawn_blocking(crate::plugin_registry::plugin_catalog_json).await {
        Ok(catalog) => ApiResponse::json(200, JsonBody::Value(catalog)),
        Err(error) => ApiResponse::json_error(500, format!("plugin catalog: {error}")),
    }
}

/// Transport-neutral core of `GET /api/skills` (tunnel twin
/// `api_skills_list`): the unified skill catalog — builtins plus bundled
/// plugin payloads, registry-driven, with per-root install facts read
/// fresh from disk. Read-only; a plugin payload's lifecycle lives on its
/// plugin card, and the row links there.
pub(crate) async fn skills_list_api_response() -> ApiResponse {
    match tokio::task::spawn_blocking(crate::skill_catalog::skills_catalog_json).await {
        Ok(catalog) => ApiResponse::json(200, JsonBody::Value(catalog)),
        Err(error) => ApiResponse::json_error(500, format!("skill catalog: {error}")),
    }
}

/// Body of `POST /api/plugins/{plugin_id}`. Unknown keys are ignored so
/// the tunnel twin can pass its whole `params` object through.
#[derive(serde::Deserialize)]
struct SetEnabledBody {
    enabled: bool,
}

/// Transport-neutral core of `POST /api/plugins/{plugin_id}` (tunnel twin
/// `api_plugin_set_enabled`): persist the flag, reconcile skill
/// materialization in the same request, then re-derive the catalog entry.
/// The response carries the same entry shape the list serves plus the
/// per-root install report, so the card's next render needs no second
/// fetch and reflects what the installer actually did.
pub(crate) async fn plugin_set_enabled_api_response(plugin_id: &str, body: &str) -> ApiResponse {
    let request: SetEnabledBody = match serde_json::from_str(body) {
        Ok(request) => request,
        Err(error) => {
            return ApiResponse::json_error(400, format!("invalid plugin request: {error}"))
        }
    };
    if crate::plugin_registry::bundled_plugin(plugin_id).is_none() {
        return ApiResponse::json_error(404, format!("unknown plugin id '{plugin_id}'"));
    }
    let id = plugin_id.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::plugin_registry::set_plugin_enabled(&id, request.enabled)?;
        let install = crate::skill_install::reconcile_global_skills().to_json();
        let plugin = crate::plugin_registry::plugin_entry_json(&id)
            .ok_or_else(|| format!("plugin '{id}' vanished mid-request"))?;
        Ok::<_, String>(serde_json::json!({ "plugin": plugin, "install": install }))
    })
    .await;
    match outcome {
        Ok(Ok(body)) => ApiResponse::json(200, JsonBody::Value(body)),
        Ok(Err(error)) => ApiResponse::json_error(500, &error),
        Err(error) => ApiResponse::json_error(500, format!("plugin toggle: {error}")),
    }
}

/// Transport-neutral core of `POST /api/skills/{name}` (tunnel twin
/// `api_skill_set_enabled`): flip one skill's enable state in the
/// persisted disabled-set, reconcile skill materialization in the same
/// request (the set outranks the sweep — both roots settle before the
/// response), then re-derive the catalog row. Refusals are per-kind and
/// named: a plugin-materialized skill refuses toward its plugin's toggle
/// (409), an unknown name refuses by name (404). `actor` is the caller's
/// gate-resolved attribution, mapped at the authenticated edge — never
/// request-body claims — and recorded on the disabling flip.
pub(crate) async fn skill_set_enabled_api_response(
    name: &str,
    body: &str,
    actor: &crate::access::actor::ActorBinding,
) -> ApiResponse {
    let request: SetEnabledBody = match serde_json::from_str(body) {
        Ok(request) => request,
        Err(error) => {
            return ApiResponse::json_error(400, format!("invalid skill request: {error}"))
        }
    };
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let record = crate::skill_state::DisabledRecord::from_actor(actor, at_ms);
    let name = name.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::skill_state::set_skill_enabled(&name, request.enabled, record)?;
        let install = crate::skill_install::reconcile_global_skills().to_json();
        let skill = crate::skill_catalog::skill_entry_json(&name).ok_or_else(|| {
            crate::skill_state::SkillToggleRefusal::io(format!(
                "skill '{name}' vanished mid-request"
            ))
        })?;
        Ok::<_, crate::skill_state::SkillToggleRefusal>(
            serde_json::json!({ "skill": skill, "install": install }),
        )
    })
    .await;
    match outcome {
        Ok(Ok(body)) => ApiResponse::json(200, JsonBody::Value(body)),
        Ok(Err(refusal)) => ApiResponse::json_error(refusal.http_status(), refusal.message()),
        Err(error) => ApiResponse::json_error(500, format!("skill toggle: {error}")),
    }
}

/// Body of `POST /api/skills` (the S4 add). `skill_md` is the ONLY
/// content lane — pasted and uploaded bytes both land here; there is no
/// URL and no path field, and unknown keys are ignored so the tunnel
/// twin can pass its whole `params` object through.
#[derive(serde::Deserialize)]
struct AddSkillBody {
    name: String,
    skill_md: String,
}

/// Transport-neutral core of `POST /api/skills` (tunnel twin
/// `api_skill_add`): validate + seal the submitted SKILL.md into the
/// daemon-owned user library with the caller's gate-resolved attribution
/// and sha256, reconcile skill materialization in the same request, then
/// serve the new catalog row + the per-root installer report + the
/// recorded sha (ruling R3). The body cap is re-checked here so the
/// tunnel lane (params-as-body, no HTTP BodyPolicy) is equally bounded.
pub(crate) async fn skill_add_api_response(
    body: &str,
    actor: &crate::access::actor::ActorBinding,
) -> ApiResponse {
    if body.len() > crate::user_skills::ADD_BODY_CAP_BYTES {
        return ApiResponse::json_error(
            413,
            format!(
                "request body exceeds the {} KiB user-skill cap",
                crate::user_skills::ADD_BODY_CAP_BYTES / 1024
            ),
        );
    }
    let request: AddSkillBody = match serde_json::from_str(body) {
        Ok(request) => request,
        Err(error) => {
            return ApiResponse::json_error(400, format!("invalid skill request: {error}"))
        }
    };
    let at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let added_by = crate::skill_state::DisabledRecord::from_actor(actor, at_ms);
    let outcome = tokio::task::spawn_blocking(move || {
        let state_root = intendant_core::state_paths::intendant_home();
        let record = crate::user_skills::add_user_skill_in(
            &state_root,
            &request.name,
            &request.skill_md,
            added_by,
        )?;
        let install = crate::skill_install::reconcile_global_skills().to_json();
        let skill = crate::skill_catalog::skill_entry_json(&request.name).ok_or_else(|| {
            crate::user_skills::UserSkillRefusal::Io {
                message: format!("skill '{}' vanished mid-request", request.name),
            }
        })?;
        Ok::<_, crate::user_skills::UserSkillRefusal>(serde_json::json!({
            "skill": skill,
            "install": install,
            "sha256": record.sha256,
        }))
    })
    .await;
    match outcome {
        Ok(Ok(body)) => ApiResponse::json(200, JsonBody::Value(body)),
        Ok(Err(refusal)) => ApiResponse::json_error(refusal.http_status(), refusal.message()),
        Err(error) => ApiResponse::json_error(500, format!("skill add: {error}")),
    }
}

/// Transport-neutral core of `DELETE /api/skills/{name}` (tunnel twin
/// `api_skill_remove`): delete the library entry + registry record, then
/// reconcile in the same request so the sweep clears the marked copies
/// from both roots — never an unmarked user-owned twin. Per-kind
/// refusals: builtins toward deactivate, plugin payloads toward their
/// plugin's toggle, unknown names 404.
pub(crate) async fn skill_remove_api_response(name: &str) -> ApiResponse {
    let name = name.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        let state_root = intendant_core::state_paths::intendant_home();
        let record = crate::user_skills::remove_user_skill_in(&state_root, &name)?;
        let install = crate::skill_install::reconcile_global_skills().to_json();
        Ok::<_, crate::user_skills::UserSkillRefusal>(serde_json::json!({
            "removed": record.name,
            "install": install,
        }))
    })
    .await;
    match outcome {
        Ok(Ok(body)) => ApiResponse::json(200, JsonBody::Value(body)),
        Ok(Err(refusal)) => ApiResponse::json_error(refusal.http_status(), refusal.message()),
        Err(error) => ApiResponse::json_error(500, format!("skill remove: {error}")),
    }
}

pub(crate) async fn handle_plugins_list(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = plugins_list_api_response().await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_skills_list(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = skills_list_api_response().await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_plugin_set_enabled(
    stream: DemuxStream,
    plugin_id: String,
    body: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = plugin_set_enabled_api_response(&plugin_id, &body).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_skill_set_enabled(
    stream: DemuxStream,
    name: String,
    body: String,
    actor: crate::access::actor::ActorBinding,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = skill_set_enabled_api_response(&name, &body, &actor).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_skill_add(
    stream: DemuxStream,
    body: String,
    actor: crate::access::actor::ActorBinding,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = skill_add_api_response(&body, &actor).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_skill_remove(
    stream: DemuxStream,
    name: String,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = skill_remove_api_response(&name).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::{AddSkillBody, SetEnabledBody};

    #[test]
    fn set_enabled_body_ignores_extra_keys_and_rejects_garbage() {
        let parsed: SetEnabledBody =
            serde_json::from_str(r#"{"enabled":true,"plugin_id":"x","id":"m1"}"#)
                .expect("tunnel params parse");
        assert!(parsed.enabled);
        assert!(serde_json::from_str::<SetEnabledBody>("not json").is_err());
        assert!(serde_json::from_str::<SetEnabledBody>(r#"{"enabled":"yes"}"#).is_err());
        assert!(serde_json::from_str::<SetEnabledBody>(r#"{}"#).is_err());
    }

    /// The add's ONLY content lane is the SKILL.md bytes field (intake
    /// §3a / ruling H4: no URL fetch, no path import — those are parked
    /// vocabulary). A body offering a `url` or `path` instead of bytes
    /// fails for the missing field; extra keys (the tunnel's params
    /// envelope) are ignored around the two real fields.
    #[test]
    fn add_skill_body_has_exactly_the_paste_upload_lane() {
        let parsed: AddSkillBody = serde_json::from_str(
            r#"{"name":"x","skill_md":"---","id":"m1","method":"api_skill_add"}"#,
        )
        .expect("tunnel params parse");
        assert_eq!(parsed.name, "x");
        assert_eq!(parsed.skill_md, "---");
        assert!(serde_json::from_str::<AddSkillBody>(r#"{"name":"x","url":"https://e"}"#).is_err());
        assert!(serde_json::from_str::<AddSkillBody>(r#"{"name":"x","path":"/tmp/s"}"#).is_err());
        assert!(serde_json::from_str::<AddSkillBody>(r#"{"skill_md":"---"}"#).is_err());
    }
}
