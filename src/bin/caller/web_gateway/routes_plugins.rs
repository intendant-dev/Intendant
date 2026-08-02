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

pub(crate) async fn handle_plugins_list(
    stream: DemuxStream,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = plugins_list_api_response().await;
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

#[cfg(test)]
mod tests {
    use super::SetEnabledBody;

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
}
