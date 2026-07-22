//! `whoami` — the caller's own gate-resolved identity, for provenance when
//! writing memory and agenda entries (origin session ids, project root).
//!
//! Reports only what the authenticating gate bound (the `access::actor`
//! seam): a supervised session is claimed exclusively by session-token
//! possession — never from request fields or lane defaults — so the answer
//! is attribution-grade. Unsupervised callers (bare ctl loopback, dashboard,
//! peers, external MCP clients) get `supervised: false` plus the principal
//! the gate named, which is the id they should cite instead.

use super::*;

impl IntendantServer {
    /// Resolve the identity report for the gate-resolved `actor`. Persisted
    /// lookups (wrapper index, session log dirs) go through `self.home` so
    /// tests inject a temp home and never touch the live `~/.intendant`.
    pub(crate) async fn whoami_for_caller(
        &self,
        actor: &crate::access::actor::ActorBinding,
    ) -> String {
        let mut out = serde_json::Map::new();
        out.insert("actor_kind".into(), actor.kind.as_str().into());
        if let Some(principal) = actor.principal_id.as_deref() {
            out.insert("principal_id".into(), principal.into());
        }
        match actor.session_id.as_deref() {
            Some(session_id) => {
                out.insert("supervised".into(), true.into());
                out.insert("daemon_session_id".into(), session_id.into());
                self.fill_session_facts(&mut out, session_id).await;
            }
            None => {
                out.insert("supervised".into(), false.into());
                out.insert(
                    "note".into(),
                    "no supervised session is bound to this caller; cite the principal id for provenance"
                        .into(),
                );
            }
        }
        serde_json::to_string_pretty(&serde_json::Value::Object(out))
            .unwrap_or_else(|_| "{}".to_string())
    }

    /// Backend, harness session id, aliases, and paths for a gate-bound
    /// supervised session id, best-effort from live state and the persisted
    /// stores. Fields are added only when known — never guessed.
    async fn fill_session_facts(
        &self,
        out: &mut serde_json::Map<String, serde_json::Value>,
        session_id: &str,
    ) {
        // The daemon's own native head session lives in server state, not
        // the external wrapper index.
        {
            let state = self.state.read().await;
            if state.session_id == session_id {
                out.insert("backend".into(), "native".into());
                if let Some(root) = state.project_root.as_ref() {
                    out.insert("project_root".into(), root.display().to_string().into());
                }
                out.insert("log_dir".into(), state.log_dir.display().to_string().into());
                return;
            }
        }
        if let Some((source, backend_session_id)) =
            crate::external_wrapper_index::conversation_for_wrapper(&self.home, session_id)
        {
            out.insert("backend".into(), source.as_str().into());
            out.insert(
                "backend_session_id".into(),
                backend_session_id.as_str().into(),
            );
            let records = crate::external_wrapper_index::wrappers_for(
                &self.home,
                &source,
                &backend_session_id,
            );
            if let Some(own) = records
                .iter()
                .find(|record| record.intendant_session_id == session_id)
            {
                out.insert("log_dir".into(), own.log_path.as_str().into());
                if let Some(root) = own
                    .project_root
                    .as_deref()
                    .map(str::trim)
                    .filter(|root| !root.is_empty())
                {
                    out.insert("project_root".into(), root.into());
                }
            }
            if !out.contains_key("project_root") {
                if let Some(root) = crate::external_wrapper_index::recorded_project_root_for_wrapper(
                    &self.home, session_id,
                ) {
                    out.insert("project_root".into(), root.into());
                }
            }
            // Other wrapper ids of the same backend conversation (restart /
            // resume rotations): agenda items or memory written under an
            // earlier incarnation cite these, so surface them.
            let aliases: Vec<serde_json::Value> = records
                .iter()
                .map(|record| record.intendant_session_id.as_str())
                .filter(|id| *id != session_id)
                .map(Into::into)
                .collect();
            if !aliases.is_empty() {
                out.insert("wrapper_aliases".into(), aliases.into());
            }
            return;
        }
        // A supervised session the index doesn't know — a native sub-agent
        // or a pruned wrapper. Its persisted log dir still anchors
        // provenance when it exists.
        let log_dir = crate::platform::intendant_home_in(&self.home)
            .join("logs")
            .join(session_id);
        if log_dir.is_dir() {
            out.insert("backend".into(), "native".into());
            out.insert("log_dir".into(), log_dir.display().to_string().into());
            if let Some(root) = std::fs::read(log_dir.join("session_meta.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|meta| {
                    meta.get("project_root")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
            {
                out.insert("project_root".into(), root.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(report: &str) -> serde_json::Value {
        serde_json::from_str(report).expect("whoami output is JSON")
    }

    fn actor_for(session_id: &str) -> crate::access::actor::ActorBinding {
        crate::access::actor::ActorBinding::agent_session(
            Some(format!("principal:agent-session:{session_id}")),
            session_id.to_string(),
        )
    }

    /// Server over an injected temp home (persisted lookups stay hermetic);
    /// the in-memory head session is pinned to a known id.
    fn test_server_with_home(home: std::path::PathBuf) -> IntendantServer {
        let bus = EventBus::new();
        let mut state = McpAppState::new(
            "test".into(),
            "test".into(),
            crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        state.session_id = "head-session".to_string();
        IntendantServer::new_with_home(Arc::new(RwLock::new(state)), bus, home)
    }

    #[tokio::test]
    async fn unsupervised_caller_reports_supervised_false_with_principal() {
        let home = tempfile::tempdir().expect("tempdir");
        let server = test_server_with_home(home.path().to_path_buf());
        let actor =
            crate::access::actor::ActorBinding::dashboard(Some("principal:user:owner".into()));
        let report = parsed(&server.whoami_for_caller(&actor).await);
        assert_eq!(report["supervised"], serde_json::json!(false));
        assert_eq!(report["actor_kind"], serde_json::json!("dashboard"));
        assert_eq!(
            report["principal_id"],
            serde_json::json!("principal:user:owner")
        );
        assert!(report.get("daemon_session_id").is_none());
    }

    #[tokio::test]
    async fn head_native_session_reports_state_paths() {
        let home = tempfile::tempdir().expect("tempdir");
        let server = test_server_with_home(home.path().to_path_buf());
        server.state.write().await.project_root = Some(std::path::PathBuf::from("/work/project"));
        let report = parsed(&server.whoami_for_caller(&actor_for("head-session")).await);
        assert_eq!(report["supervised"], serde_json::json!(true));
        assert_eq!(
            report["daemon_session_id"],
            serde_json::json!("head-session")
        );
        assert_eq!(report["backend"], serde_json::json!("native"));
        assert_eq!(report["project_root"], serde_json::json!("/work/project"));
        assert_eq!(report["log_dir"], serde_json::json!("/tmp/test_session"));
    }

    #[tokio::test]
    async fn supervised_external_wrapper_reports_conversation_aliases_and_paths() {
        let home = tempfile::tempdir().expect("tempdir");
        let logs = crate::platform::intendant_home_in(home.path()).join("logs");
        let wrapper = "11111111-2222-4333-8444-555555555555";
        let alias = "66666666-7777-4888-9999-aaaaaaaaaaaa";
        let backend = "0f0e0d0c-0b0a-4908-8706-050403020100";
        for id in [wrapper, alias] {
            std::fs::create_dir_all(logs.join(id)).expect("log dir");
        }
        crate::external_wrapper_index::upsert(
            home.path(),
            "claude-code",
            backend,
            wrapper,
            &logs.join(wrapper),
            Some(std::path::Path::new("/work/repo")),
        )
        .expect("upsert wrapper");
        crate::external_wrapper_index::upsert(
            home.path(),
            "claude-code",
            backend,
            alias,
            &logs.join(alias),
            None,
        )
        .expect("upsert alias");

        let server = test_server_with_home(home.path().to_path_buf());
        let report = parsed(&server.whoami_for_caller(&actor_for(wrapper)).await);
        assert_eq!(report["supervised"], serde_json::json!(true));
        assert_eq!(report["daemon_session_id"], serde_json::json!(wrapper));
        assert_eq!(report["backend"], serde_json::json!("claude-code"));
        assert_eq!(report["backend_session_id"], serde_json::json!(backend));
        assert_eq!(
            report["log_dir"],
            serde_json::json!(logs.join(wrapper).display().to_string())
        );
        assert_eq!(report["project_root"], serde_json::json!("/work/repo"));
        let aliases = report["wrapper_aliases"]
            .as_array()
            .expect("aliases present");
        assert_eq!(aliases, &[serde_json::json!(alias)]);
    }

    #[tokio::test]
    async fn unknown_supervised_session_falls_back_to_persisted_log_dir() {
        let home = tempfile::tempdir().expect("tempdir");
        let session = "native-sub-1";
        let dir = crate::platform::intendant_home_in(home.path())
            .join("logs")
            .join(session);
        std::fs::create_dir_all(&dir).expect("log dir");
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::json!({"session_id": session, "project_root": "/work/native"}).to_string(),
        )
        .expect("meta");
        let server = test_server_with_home(home.path().to_path_buf());
        let report = parsed(&server.whoami_for_caller(&actor_for(session)).await);
        assert_eq!(report["supervised"], serde_json::json!(true));
        assert_eq!(report["backend"], serde_json::json!("native"));
        assert_eq!(report["project_root"], serde_json::json!("/work/native"));
        assert_eq!(
            report["log_dir"],
            serde_json::json!(dir.display().to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_routes_whoami_with_the_gate_actor() {
        let home = tempfile::tempdir().expect("tempdir");
        let server = test_server_with_home(home.path().to_path_buf());
        let caller = ToolCaller {
            trust: ToolCallerTrust::Scoped,
            actor: crate::access::actor::ActorBinding::local_process(Some(
                "principal:local-process".into(),
            )),
        };
        let result = server
            .call_tool_by_name_as_caller("whoami", serde_json::json!({}), None, None, caller)
            .await
            .expect("whoami dispatches");
        let text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone())
            .expect("text content");
        let report = parsed(&text);
        assert_eq!(report["supervised"], serde_json::json!(false));
        assert_eq!(report["actor_kind"], serde_json::json!("local_process"));
    }
}
