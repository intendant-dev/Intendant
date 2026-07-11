//! Codex thread actions and their prompt/goal vocabulary: compact, fork,
//! side threads, rollback, review, thread naming, and the goal CRUD methods,
//! plus the managed-context/side-prompt developer-instruction builders and
//! goal formatting helpers they lean on.

use super::*;

/// Codex-specific thread-action helpers. Each wraps one of Codex's app-server
/// JSON-RPC methods (`thread/compact/start`, `thread/fork`, `thread/inject_items`,
/// `thread/rollback`, `review/start`, `thread/name/set`, `thread/goal/*`, `memory/reset`) with the
/// `threadId` lookup boilerplate where the upstream method requires it.
/// All return a short human-readable status string on success for the
/// dashboard toast.
impl CodexAgent {
    pub(crate) async fn require_active_thread(&self) -> Result<String, CallerError> {
        let guard = self.active_thread_id.lock().await;
        guard
            .clone()
            .ok_or_else(|| CallerError::ExternalAgent("no active Codex thread".into()))
    }

    pub(crate) async fn thread_id_for_action(
        &self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        if let Some(thread_id) = extract_thread_id(params) {
            Ok(thread_id)
        } else {
            self.require_active_thread().await
        }
    }

    pub(crate) async fn ensure_thread_action_allowed(
        &self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<(), CallerError> {
        if matches!(op, "side-close" | "side_close") {
            return Ok(());
        }
        let thread_id = match extract_thread_id(params) {
            Some(thread_id) => Some(thread_id),
            None if matches!(op, "memory-reset" | "memory_reset") => None,
            None => self.active_thread_id.lock().await.clone(),
        };
        let Some(thread_id) = thread_id else {
            return Ok(());
        };
        let side_threads = self.side_threads.lock().await;
        if let Some(parent_thread_id) = side_threads.get(&thread_id) {
            return Err(CallerError::ExternalAgent(format!(
                "cannot /{} a /side conversation {}; use the parent thread {} instead",
                op, thread_id, parent_thread_id
            )));
        }
        Ok(())
    }

    pub(super) async fn dispatch_thread_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        self.ensure_thread_action_allowed(op, params).await?;
        match op {
            "compact" => self.compact_thread(params).await,
            "fork" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                self.fork_thread(params, name).await
            }
            "side" | "btw" => self.start_side_thread(params).await,
            "side-close" | "side_close" => self.close_side_thread(params).await,
            "undo" => {
                let turns = params.get("turns").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                self.rollback_turns_inner(params, turns).await
            }
            "rewind-anchor" | "rewind_anchor" | "rewind-to-item" | "rewind_to_item"
            | "rollback-anchor" | "rollback_anchor" | "rollback-to-item" | "rollback_to_item" => {
                // Enforce the managed-context capability at the backend, matching
                // `supports_item_anchor_rewind`, so no dispatch route can perform an
                // item-anchor rollback when managed context is disabled.
                if !self.managed_context {
                    return Err(CallerError::ExternalAgent(format!(
                        "/{op} item-anchor rewind requires Codex managed-context mode"
                    )));
                }
                self.rollback_anchor_inner(params).await
            }
            "review" => {
                let prompt = params
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                self.start_review(params, prompt).await
            }
            "rename" | "name-set" | "name_set" | "thread-name-set" | "thread_name_set" => {
                self.set_thread_name(params).await
            }
            "goal" | "goal-set" | "goal-edit" | "goal_get" | "goal-get" | "goal-status" => {
                self.dispatch_goal_action(op, params).await
            }
            "goal-clear" | "goal_clear" => self.clear_goal(params).await,
            "goal-pause" | "goal_pause" => self.update_goal_status(params, "paused").await,
            "goal-resume" | "goal_resume" => self.update_goal_status(params, "active").await,
            "goal-complete" | "goal_complete" => self.update_goal_status(params, "complete").await,
            "goal-budget-limited" | "goal_budget_limited" => {
                self.update_goal_status(params, "budgetLimited").await
            }
            "memory-reset" | "memory_reset" => self.reset_memory().await,
            "fast" => Ok(self.toggle_fast_service_tier()),
            other => Err(CallerError::ExternalAgent(format!(
                "unsupported Codex thread action: /{}",
                other
            ))),
        }
    }

    pub(crate) async fn compact_thread(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let params = serde_json::json!({ "threadId": thread_id });
        let _ = self
            .send_request("thread/compact/start", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/compact/start: {e}")))?;
        Ok("conversation compaction started".to_string())
    }

    pub(crate) async fn fork_thread(
        &mut self,
        params: &serde_json::Value,
        name: Option<String>,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String(thread_id));
        if let Some(n) = name.as_deref().filter(|s| !s.trim().is_empty()) {
            obj.insert(
                "name".into(),
                serde_json::Value::String(n.trim().to_string()),
            );
        }
        self.insert_service_tier_override(&mut obj);
        let response = self
            .send_request("thread/fork", Some(serde_json::Value::Object(obj)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/fork: {e}")))?;
        let new_id = response
            .pointer("/thread/id")
            .and_then(|v| v.as_str())
            .or_else(|| response.pointer("/threadId").and_then(|v| v.as_str()))
            .unwrap_or("(unknown)");
        // Do not retarget this running agent here. The dashboard control
        // plane attaches the forked thread as its own managed session so the
        // parent thread remains controllable from its original window.
        Ok(format!("forked into thread {}", new_id))
    }

    pub(crate) async fn start_side_thread(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let parent_thread_id = self.thread_id_for_action(params).await?;
        let prompt = side_prompt_from_params(params)?;

        let developer_instructions = self.effective_side_developer_instructions().await;
        let fork_params = self.side_fork_params(&parent_thread_id, developer_instructions);
        let fork_response = self
            .send_request("thread/fork", Some(fork_params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/fork: {e}")))?;
        let child_thread_id = extract_thread_id(&fork_response).ok_or_else(|| {
            CallerError::ExternalAgent("thread/fork response missing thread id".into())
        })?;

        let inject_params = serde_json::json!({
            "threadId": child_thread_id.clone(),
            "items": [side_boundary_prompt_item()],
        });
        if let Err(err) = self
            .send_request("thread/inject_items", Some(inject_params))
            .await
        {
            let _ = self
                .send_request(
                    "thread/unsubscribe",
                    Some(serde_json::json!({ "threadId": child_thread_id.clone() })),
                )
                .await;
            return Err(CallerError::ExternalAgent(format!(
                "thread/inject_items: {err}"
            )));
        }

        let mut turn_obj = serde_json::Map::new();
        turn_obj.insert(
            "threadId".into(),
            serde_json::Value::String(child_thread_id.clone()),
        );
        turn_obj.insert(
            "input".into(),
            serde_json::Value::Array(vec![serde_json::json!({"type": "text", "text": prompt})]),
        );
        self.insert_service_tier_override_consuming_clear(&mut turn_obj);
        let turn_params = serde_json::Value::Object(turn_obj);
        self.capture_turn_descendant_baseline();
        match self.send_request("turn/start", Some(turn_params)).await {
            Ok(response) => {
                if let Some(id) = extract_turn_id(&response) {
                    self.active_turns
                        .lock()
                        .await
                        .insert(child_thread_id.clone(), id);
                }
                self.side_threads
                    .lock()
                    .await
                    .insert(child_thread_id.clone(), parent_thread_id.clone());
                Ok(format!(
                    "side conversation started in thread {} from parent {}",
                    child_thread_id, parent_thread_id
                ))
            }
            Err(err) => {
                let _ = self
                    .send_request(
                        "thread/unsubscribe",
                        Some(serde_json::json!({ "threadId": child_thread_id.clone() })),
                    )
                    .await;
                Err(CallerError::ExternalAgent(format!("turn/start: {err}")))
            }
        }
    }

    pub(crate) async fn close_side_thread(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let child_thread_id = params
            .get("threadId")
            .or_else(|| params.get("thread_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| CallerError::ExternalAgent("side thread id is required".into()))?;
        let parent_thread_id = params
            .get("parentThreadId")
            .or_else(|| params.get("parent_thread_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                CallerError::ExternalAgent("side parent thread id is required".into())
            })?;

        let parent_turn_id = {
            let mut active_turns = self.active_turns.lock().await;
            active_turns.remove(&child_thread_id);
            active_turns.get(&parent_thread_id).cloned()
        };
        *self.active_turn_id.lock().await = parent_turn_id;
        *self.active_thread_id.lock().await = Some(parent_thread_id.clone());
        let _ = self
            .send_request(
                "thread/unsubscribe",
                Some(serde_json::json!({ "threadId": child_thread_id.clone() })),
            )
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/unsubscribe: {e}")))?;
        self.side_threads.lock().await.remove(&child_thread_id);
        Ok(format!(
            "side conversation {} closed; returned to parent {}",
            child_thread_id, parent_thread_id
        ))
    }

    pub(crate) async fn effective_side_developer_instructions(&mut self) -> String {
        match self.current_codex_developer_instructions().await {
            Ok(existing_instructions) => {
                side_developer_instructions(existing_instructions.as_deref())
            }
            Err(_) => side_developer_instructions(None),
        }
    }

    pub(crate) async fn effective_managed_context_developer_instructions(
        &mut self,
    ) -> Option<String> {
        if !self.managed_context {
            return None;
        }
        let existing_instructions = self
            .current_codex_developer_instructions()
            .await
            .ok()
            .flatten();
        Some(managed_context_developer_instructions_for_project(
            existing_instructions.as_deref(),
            self.working_dir.as_deref(),
        ))
    }

    pub(crate) async fn current_codex_developer_instructions(
        &mut self,
    ) -> Result<Option<String>, CallerError> {
        if self.writer.is_none() {
            return Ok(None);
        }

        let mut params = serde_json::Map::new();
        params.insert("includeLayers".into(), serde_json::Value::Bool(false));
        self.insert_working_dir_param(&mut params);
        let response = self
            .send_request("config/read", Some(serde_json::Value::Object(params)))
            .await?;
        Ok(response
            .pointer("/config/developer_instructions")
            .and_then(|v| v.as_str())
            .or_else(|| {
                response
                    .pointer("/config/developerInstructions")
                    .and_then(|v| v.as_str())
            })
            .map(str::to_string))
    }

    pub(crate) fn side_fork_params(
        &self,
        parent_thread_id: &str,
        developer_instructions: String,
    ) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "threadId".into(),
            serde_json::Value::String(parent_thread_id.to_string()),
        );
        obj.insert("ephemeral".into(), serde_json::Value::Bool(true));
        obj.insert(
            "developerInstructions".into(),
            serde_json::Value::String(developer_instructions),
        );
        if let Some(ref model) = self.model {
            obj.insert("model".into(), serde_json::Value::String(model.clone()));
        }
        let approval_policy = self.effective_approval_policy().trim();
        if !approval_policy.is_empty() {
            obj.insert(
                "approvalPolicy".into(),
                serde_json::Value::String(approval_policy.to_string()),
            );
        }
        if !self.sandbox.trim().is_empty() {
            obj.insert(
                "sandbox".into(),
                serde_json::Value::String(self.sandbox.clone()),
            );
        }
        self.insert_working_dir_param(&mut obj);
        self.insert_service_tier_override(&mut obj);
        serde_json::Value::Object(obj)
    }

    /// Build `thread/fork` params for a live-thread fission fork: `threadId`
    /// selects the source thread (the fork inherits its full conversation
    /// context and lineage prompt-cache key) and `cwd`, when given, moves the
    /// branch into its own checkout — typically a git worktree of the parent
    /// project.
    ///
    /// Worktree sandbox consideration: under Codex's `workspace-write`
    /// sandbox the branch may write inside its cwd, but a linked git
    /// worktree's git metadata lives under the MAIN repository's `.git`
    /// (private dir `<main>/.git/worktrees/<name>` plus the shared object and
    /// ref stores), so branch commits would be blocked. `thread/fork` accepts
    /// per-fork `config` overrides applied as dotted-key CLI config
    /// overrides, so when `cwd` is a linked worktree the main repository's
    /// common `.git` directory is added to
    /// `sandbox_workspace_write.writable_roots`. Launch-level extra roots are
    /// re-included because a per-fork value for the same key replaces — not
    /// extends — the launch-arg value.
    pub(crate) fn fork_thread_with_options_params(
        &self,
        thread_id: &str,
        cwd: Option<&Path>,
    ) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "threadId".into(),
            serde_json::Value::String(thread_id.to_string()),
        );
        if let Some(cwd) = cwd {
            obj.insert(
                "cwd".into(),
                serde_json::Value::String(cwd.to_string_lossy().to_string()),
            );
            if let Some(common_git_dir) = git_worktree_common_git_dir(cwd) {
                let roots: Vec<serde_json::Value> = self
                    .writable_roots
                    .iter()
                    .cloned()
                    .chain(std::iter::once(
                        common_git_dir.to_string_lossy().to_string(),
                    ))
                    .map(serde_json::Value::String)
                    .collect();
                let mut config = serde_json::Map::new();
                config.insert(
                    "sandbox_workspace_write.writable_roots".into(),
                    serde_json::Value::Array(roots),
                );
                obj.insert("config".into(), serde_json::Value::Object(config));
            }
        }
        self.insert_service_tier_override(&mut obj);
        serde_json::Value::Object(obj)
    }

    /// Inner implementation of the `/undo` thread action. Returns a
    /// human-readable status string for the dashboard toast. The
    /// `ExternalAgent::rollback_turns` trait method (impl below) wraps
    /// this same RPC without the status string — callers just need
    /// to know success/failure.
    pub(crate) async fn rollback_turns_inner(
        &mut self,
        params: &serde_json::Value,
        turns: u32,
    ) -> Result<String, CallerError> {
        if turns == 0 {
            return Err(CallerError::ExternalAgent(
                "rollback count must be at least 1".into(),
            ));
        }
        let thread_id = self.thread_id_for_action(params).await?;
        // Codex's `ThreadRollbackParams` accepts `numTurns`; the event it
        // emits after rollback currently uses `num_turns`.
        let params = serde_json::json!({
            "threadId": thread_id,
            "numTurns": turns,
        });
        let _ = self
            .send_request("thread/rollback", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/rollback: {e}")))?;
        self.reset_context_pressure_after_thread_rewrite().await;
        Ok(format!("rolled back {} turn(s)", turns))
    }

    pub(crate) async fn rollback_anchor_inner(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let item_id = rollback_anchor_item_id(params)?;
        let position = rollback_anchor_position(params)?;
        self.rollback_item_anchor_rpc(&thread_id, &item_id, position)
            .await?;
        Ok(format!(
            "rolled back to {} item {}",
            position.as_str(),
            item_id
        ))
    }

    pub(crate) async fn rollback_item_anchor_rpc(
        &mut self,
        thread_id: &str,
        item_id: &str,
        position: RollbackAnchorPosition,
    ) -> Result<(), CallerError> {
        let item_id = item_id.trim();
        if item_id.is_empty() {
            return Err(CallerError::ExternalAgent(
                "rollback anchor item id is required".into(),
            ));
        }
        let params = serde_json::json!({
            "threadId": thread_id,
            "numTurns": 0,
            "anchor": {
                "itemId": item_id,
                "position": position.as_str(),
            },
        });
        let _ = self
            .send_request("thread/rollback", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/rollback: {e}")))?;
        self.reset_context_pressure_after_thread_rewrite().await;
        Ok(())
    }

    pub(crate) async fn start_review(
        &mut self,
        params: &serde_json::Value,
        prompt: Option<String>,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String(thread_id));
        if let Some(p) = prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            obj.insert(
                "prompt".into(),
                serde_json::Value::String(p.trim().to_string()),
            );
        }
        let _ = self
            .send_request("review/start", Some(serde_json::Value::Object(obj)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("review/start: {e}")))?;
        Ok(match prompt {
            Some(p) if !p.trim().is_empty() => format!("review started with prompt: {}", p),
            _ => "review started on current changes".to_string(),
        })
    }

    pub(crate) async fn reset_memory(&mut self) -> Result<String, CallerError> {
        let _ = self
            .send_request("memory/reset", None)
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("memory/reset: {e}")))?;
        Ok("Codex memory reset".to_string())
    }

    pub(crate) async fn set_thread_name(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let name = params
            .get("name")
            .or_else(|| params.get("threadName"))
            .or_else(|| params.get("thread_name"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CallerError::ExternalAgent("thread name cannot be empty".into()))?;
        let request = serde_json::json!({ "threadId": thread_id, "name": name });
        let _ = self
            .send_request("thread/name/set", Some(request))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/name/set: {e}")))?;
        Ok(format!("Codex thread renamed to {}", name))
    }

    pub(crate) async fn dispatch_goal_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        if params
            .get("clear")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return self.clear_goal(params).await;
        }

        let status = params
            .get("status")
            .and_then(|v| v.as_str())
            .map(normalize_goal_status)
            .transpose()?;
        let objective = params
            .get("objective")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(objective) = objective {
            validate_goal_objective(objective)?;
        }
        let token_budget = parse_goal_token_budget(params)?;

        if objective.is_some()
            || status.is_some()
            || token_budget.is_some()
            || matches!(op, "goal-set")
        {
            return self
                .set_goal(params, objective, status.as_deref(), token_budget)
                .await;
        }

        self.get_goal(params).await
    }

    pub(crate) async fn set_goal(
        &mut self,
        params: &serde_json::Value,
        objective: Option<&str>,
        status: Option<&str>,
        token_budget: Option<Option<u64>>,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let mut obj = serde_json::Map::new();
        obj.insert("threadId".into(), serde_json::Value::String(thread_id));
        if let Some(objective) = objective {
            obj.insert(
                "objective".into(),
                serde_json::Value::String(objective.to_string()),
            );
        }
        if let Some(status) = status {
            obj.insert(
                "status".into(),
                serde_json::Value::String(status.to_string()),
            );
        }
        if let Some(token_budget) = token_budget {
            obj.insert(
                "tokenBudget".into(),
                token_budget
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
            );
        }

        let response = self
            .send_request("thread/goal/set", Some(serde_json::Value::Object(obj)))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/goal/set: {e}")))?;
        Ok(format_goal_response("goal updated", &response))
    }

    pub(crate) async fn get_goal(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let params = serde_json::json!({ "threadId": thread_id });
        let response = self
            .send_request("thread/goal/get", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/goal/get: {e}")))?;
        Ok(format_goal_response("current goal", &response))
    }

    pub(crate) async fn clear_goal(
        &mut self,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let thread_id = self.thread_id_for_action(params).await?;
        let params = serde_json::json!({ "threadId": thread_id });
        let response = self
            .send_request("thread/goal/clear", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/goal/clear: {e}")))?;
        let cleared = response
            .get("cleared")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(if cleared {
            "goal cleared".to_string()
        } else {
            "no goal to clear".to_string()
        })
    }

    pub(crate) async fn update_goal_status(
        &mut self,
        params: &serde_json::Value,
        status: &str,
    ) -> Result<String, CallerError> {
        self.set_goal(params, None, Some(status), None).await
    }

    pub(crate) async fn pause_active_goal_for_thread(
        &mut self,
        thread_id: &str,
    ) -> Result<AutonomousGoalPauseResult, CallerError> {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return Ok(AutonomousGoalPauseResult::default());
        }
        let params = serde_json::json!({ "threadId": thread_id });
        let response = self
            .send_request("thread/goal/get", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/goal/get: {e}")))?;
        let Some(current_goal) = response.get("goal").and_then(session_goal_from_value) else {
            return Ok(AutonomousGoalPauseResult {
                goal_absent: true,
                ..Default::default()
            });
        };
        if !current_goal
            .status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("active"))
        {
            return Ok(AutonomousGoalPauseResult {
                goal: Some(current_goal),
                goal_absent: false,
                paused: false,
            });
        }

        let params = serde_json::json!({
            "threadId": thread_id,
            "status": "paused",
        });
        let response = self
            .send_request("thread/goal/set", Some(params))
            .await
            .map_err(|e| CallerError::ExternalAgent(format!("thread/goal/set: {e}")))?;
        let goal = response
            .get("goal")
            .and_then(session_goal_from_value)
            .or_else(|| {
                let mut goal = current_goal;
                goal.status = Some("paused".to_string());
                Some(goal)
            });
        Ok(AutonomousGoalPauseResult {
            goal,
            goal_absent: false,
            paused: true,
        })
    }
}

pub(crate) fn rollback_anchor_item_id(params: &serde_json::Value) -> Result<String, CallerError> {
    let item_id = params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .or_else(|| params.pointer("/anchor/itemId"))
        .or_else(|| params.pointer("/anchor/item_id"))
        .and_then(|v| v.as_str())
        .or_else(|| params.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CallerError::ExternalAgent("rollback anchor item id is required".into()))?;
    Ok(item_id.to_string())
}

pub(crate) fn rollback_anchor_position(
    params: &serde_json::Value,
) -> Result<RollbackAnchorPosition, CallerError> {
    let raw = params
        .get("position")
        .or_else(|| params.pointer("/anchor/position"))
        .and_then(|v| v.as_str())
        .unwrap_or("after");
    RollbackAnchorPosition::from_str(raw).ok_or_else(|| {
        CallerError::ExternalAgent(format!(
            "rollback anchor position must be before or after, got {raw}"
        ))
    })
}

pub(crate) fn side_prompt_from_params(params: &serde_json::Value) -> Result<String, CallerError> {
    let prompt = ["prompt", "message", "text", "task"]
        .iter()
        .find_map(|key| params.get(*key).and_then(|v| v.as_str()))
        .or_else(|| params.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CallerError::ExternalAgent(
                "/side requires a prompt in Intendant; use `/side <question>`".into(),
            )
        })?;
    Ok(prompt.to_string())
}

pub(crate) fn side_developer_instructions(existing_instructions: Option<&str>) -> String {
    match existing_instructions {
        Some(existing_instructions) if !existing_instructions.trim().is_empty() => {
            format!("{existing_instructions}\n\n{SIDE_DEVELOPER_INSTRUCTIONS}")
        }
        _ => SIDE_DEVELOPER_INSTRUCTIONS.to_string(),
    }
}

pub(crate) fn managed_context_developer_instructions(
    existing_instructions: Option<&str>,
) -> String {
    match existing_instructions {
        Some(existing_instructions) if !existing_instructions.trim().is_empty() => {
            format!("{existing_instructions}\n\n{MANAGED_CONTEXT_DEVELOPER_INSTRUCTIONS}")
        }
        _ => MANAGED_CONTEXT_DEVELOPER_INSTRUCTIONS.to_string(),
    }
}

pub(crate) fn project_managed_context_instructions_path(working_dir: &Path) -> PathBuf {
    working_dir
        .join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_DIR)
        .join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_FILE)
}

/// Read the project's managed-context instructions extension, if any.
/// Returns `None` when the file is absent, empty, or unreadable — read
/// failures are logged and non-fatal so a bad project file can never block a
/// managed launch. Contents beyond
/// `MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_MAX_BYTES` are truncated (on a char
/// boundary) with an explicit marker.
pub(crate) fn project_managed_context_instructions(working_dir: Option<&Path>) -> Option<String> {
    let path = project_managed_context_instructions_path(working_dir?);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "[codex] Warning: failed to read project managed-context instructions {}: {}",
                    path.display(),
                    e
                );
            }
            return None;
        }
    };
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_MAX_BYTES {
        return Some(trimmed.to_string());
    }
    let mut end = MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_MAX_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!(
        "{}\n{}",
        trimmed[..end].trim_end(),
        MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_TRUNCATION_MARKER
    ))
}

/// Full managed-context developer instructions for one project: any existing
/// Codex developer instructions, then the generic managed-context block, then
/// the optional per-project extension from
/// `<working_dir>/.intendant/codex-managed-instructions.md`.
pub(crate) fn managed_context_developer_instructions_for_project(
    existing_instructions: Option<&str>,
    working_dir: Option<&Path>,
) -> String {
    let base = managed_context_developer_instructions(existing_instructions);
    match project_managed_context_instructions(working_dir) {
        Some(project_instructions) => format!(
            "{base}\n\n{MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING}\n\n{project_instructions}"
        ),
        None => base,
    }
}

/// When `cwd` is a linked git worktree — its `.git` is a *file* containing
/// `gitdir: <path>` — resolve the main repository's common `.git` directory.
///
/// A linked worktree's private git dir lives at `<main>/.git/worktrees/<name>`
/// and commits made inside the worktree write through it into the shared
/// object/ref stores under `<main>/.git`. The `commondir` file inside the
/// private dir points at that shared directory (usually `../..`); when it is
/// missing, fall back to the structural `worktrees/<name>` layout. Returns
/// `None` for ordinary checkouts (`.git` directory), non-repos, or malformed
/// `.git` files.
pub(crate) fn git_worktree_common_git_dir(cwd: &Path) -> Option<PathBuf> {
    let dot_git = cwd.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let gitdir = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        cwd.join(gitdir)
    };
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(commondir) => {
            let commondir = commondir.trim();
            if commondir.is_empty() {
                return None;
            }
            if Path::new(commondir).is_absolute() {
                PathBuf::from(commondir)
            } else {
                gitdir.join(commondir)
            }
        }
        Err(_) => {
            if gitdir.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("worktrees"))
            {
                return None;
            }
            gitdir.parent()?.parent()?.to_path_buf()
        }
    };
    // `commondir` is usually relative (`../..`); canonicalize so the sandbox
    // writable root is a clean absolute path without `..` segments. Keep the
    // joined path as a best-effort fallback when canonicalization fails.
    Some(std::fs::canonicalize(&common).unwrap_or(common))
}

pub(crate) fn side_boundary_prompt_item() -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": SIDE_BOUNDARY_PROMPT,
        }],
    })
}

pub(crate) fn effective_approval_policy_for_sandbox<'a>(
    sandbox: &str,
    approval_policy: &'a str,
) -> &'a str {
    if sandbox.trim() == CODEX_DANGER_FULL_ACCESS_SANDBOX {
        CODEX_NEVER_APPROVAL_POLICY
    } else {
        approval_policy
    }
}

pub(crate) fn extract_thread_id(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/thread/id")
        .and_then(|v| v.as_str())
        .or_else(|| value.pointer("/threadId").and_then(|v| v.as_str()))
        .or_else(|| value.pointer("/thread_id").and_then(|v| v.as_str()))
        .map(str::to_string)
}

pub(crate) fn extract_thread_path(value: &serde_json::Value) -> Option<PathBuf> {
    value
        .pointer("/thread/path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

pub(crate) async fn latest_codex_token_usage_from_rollout(
    path: &Path,
) -> Result<Option<serde_json::Value>, CallerError> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CallerError::ExternalAgent(format!(
                "open Codex rollout {}: {}",
                path.display(),
                e
            )));
        }
    };
    let mut lines = BufReader::new(file).lines();
    let mut latest = None;

    while let Some(line) = lines.next_line().await.map_err(|e| {
        CallerError::ExternalAgent(format!("read Codex rollout {}: {}", path.display(), e))
    })? {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
            continue;
        }
        let payload = event.get("payload").unwrap_or(&serde_json::Value::Null);
        if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
            continue;
        }
        if let Some(info) = payload.get("info").filter(|value| !value.is_null()) {
            latest = Some(info.clone());
        }
    }

    Ok(latest)
}

pub(crate) fn format_goal_response(prefix: &str, response: &serde_json::Value) -> String {
    match response.get("goal") {
        Some(serde_json::Value::Null) | None => "no goal set".to_string(),
        Some(goal) => format_goal(goal)
            .map(|goal| format!("{}: {}", prefix, goal))
            .unwrap_or_else(|| "no goal set".to_string()),
    }
}

pub(crate) fn goal_objective(goal: &serde_json::Value) -> Option<&str> {
    goal.get("objective")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn format_goal(goal: &serde_json::Value) -> Option<String> {
    let objective = goal_objective(goal)?;
    let status = goal
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let tokens_used = goal.get("tokensUsed").and_then(|v| v.as_u64());
    let token_budget = goal.get("tokenBudget").and_then(|v| v.as_u64());
    let time_used = goal.get("timeUsedSeconds").and_then(|v| v.as_u64());

    let mut details = vec![format!("status {}", status)];
    if let Some(tokens_used) = tokens_used {
        match token_budget {
            Some(budget) => details.push(format!("{} / {} tokens", tokens_used, budget)),
            None => details.push(format!("{} tokens", tokens_used)),
        }
    } else if let Some(budget) = token_budget {
        details.push(format!("budget {} tokens", budget));
    }
    if let Some(seconds) = time_used {
        details.push(format!("elapsed {}", format_duration_short(seconds)));
    }

    Some(format!("{} ({})", objective, details.join(", ")))
}

pub(crate) fn session_goal_from_value(
    goal: &serde_json::Value,
) -> Option<crate::types::SessionGoal> {
    let objective = goal_objective(goal)?.to_string();

    Some(crate::types::SessionGoal {
        objective,
        status: goal
            .get("status")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        elapsed_seconds: goal
            .get("timeUsedSeconds")
            .or_else(|| goal.get("elapsedSeconds"))
            .or_else(|| goal.get("elapsed_seconds"))
            .and_then(|v| v.as_u64()),
        tokens_used: goal
            .get("tokensUsed")
            .or_else(|| goal.get("tokens_used"))
            .and_then(|v| v.as_u64()),
        token_budget: goal
            .get("tokenBudget")
            .or_else(|| goal.get("token_budget"))
            .and_then(|v| v.as_u64()),
    })
}

pub(crate) fn format_duration_short(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, secs)
    } else {
        format!("{}s", secs)
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC wire types
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_agent::codex::tests::test_agent;

    #[tokio::test]
    async fn codex_rollout_token_usage_seed_reads_latest_non_null_info() {
        let tmp = tempfile::tempdir().unwrap();
        let rollout = tmp.path().join("rollout.jsonl");
        std::fs::write(
            &rollout,
            [
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": null
                    }
                })
                .to_string(),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {"total_tokens": 258400},
                            "model_context_window": 258400
                        }
                    }
                })
                .to_string(),
                serde_json::json!({
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {"total_tokens": 259545},
                            "model_context_window": 258400,
                            "model_hard_context_window": 272000
                        }
                    }
                })
                .to_string(),
                "not json".to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let usage = latest_codex_token_usage_from_rollout(&rollout)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(codex_usage_total_tokens(&usage), Some(259545));
        assert_eq!(
            codex_usage_token_count_kind(&usage),
            Some(AgentContextTokenCountKind::LocalEstimate)
        );
        assert_eq!(codex_usage_context_window(&usage), Some(258400));
        assert_eq!(codex_usage_hard_context_window(&usage), Some(272000));
    }

    #[test]
    fn danger_full_access_forces_effective_approval_policy_never() {
        assert_eq!(
            effective_approval_policy_for_sandbox("danger-full-access", "on-request"),
            "never"
        );
        assert_eq!(
            effective_approval_policy_for_sandbox("danger-full-access", "untrusted"),
            "never"
        );
        assert_eq!(
            effective_approval_policy_for_sandbox("workspace-write", "on-request"),
            "on-request"
        );
        assert_eq!(
            effective_approval_policy_for_sandbox("read-only", "untrusted"),
            "untrusted"
        );
    }

    /// Cache-prefix contract: the mid-session `thread/resume` retry must
    /// re-send the exact `developerInstructions` the thread was started
    /// with. A bare resume (no override) would rebuild the thread with the
    /// config-default developer block — dropping the managed-context policy
    /// and busting the prompt-cache prefix at maximum prompt size.
    #[test]
    fn followup_resume_params_resend_thread_start_developer_instructions() {
        let mut agent = test_agent();
        let instructions =
            managed_context_developer_instructions_for_project(Some("user base"), None);
        agent.thread_developer_instructions = Some(instructions.clone());

        let params = agent.followup_resume_params("thread-evicted");

        assert_eq!(params["threadId"], "thread-evicted");
        assert_eq!(params["excludeTurns"], true);
        assert_eq!(
            params["developerInstructions"].as_str(),
            Some(instructions.as_str()),
            "developerInstructions must be byte-identical to the thread-start override"
        );

        // A thread started without an override resumes without one too —
        // re-introducing instructions the thread never had would be just as
        // prefix-unstable as dropping them.
        let mut vanilla = test_agent();
        vanilla.thread_developer_instructions = None;
        let params = vanilla.followup_resume_params("thread-evicted");
        assert!(!params.contains_key("developerInstructions"));
    }

    #[test]
    fn thread_lifecycle_params_can_include_managed_context_instructions() {
        let mut agent = test_agent();
        let instructions =
            managed_context_developer_instructions(Some("Existing developer policy."));

        let params = agent.thread_lifecycle_params_with_developer_instructions(Some(instructions));

        let developer_instructions = params["developerInstructions"].as_str().unwrap();
        assert!(developer_instructions.contains("Existing developer policy."));
        assert!(developer_instructions.contains("managed_context=managed"));
        assert!(developer_instructions.contains("Keep the live transcript informationally dense"));
        assert!(developer_instructions.contains("read_screen"));
        assert!(developer_instructions.contains("take_screenshot"));
        assert!(developer_instructions.contains("execute_cu_actions"));
        assert!(developer_instructions.contains("pkill -f intendant"));
        assert!(developer_instructions.contains("one primary validation attempt"));
        assert!(developer_instructions.contains("one compact diagnostic retry"));
        assert!(developer_instructions.contains("Do not cycle through multiple automation stacks"));
        assert!(developer_instructions.contains("cliclick"));
        assert!(developer_instructions.contains("osascript"));
        assert!(developer_instructions.contains("already-built binaries"));
        assert!(
            developer_instructions.contains("A rewind can cancel the active long-running command")
        );
        assert!(developer_instructions
            .contains("After genuinely noisy or unexpectedly large tool output"));
        // Density-first policy: pruning is noise-triggered, never pressure-gated.
        assert!(
            developer_instructions.contains("Pruning is triggered by noise, not gated by pressure")
        );
        assert!(developer_instructions.contains("at any pressure, including `ok`"));
        assert!(developer_instructions.contains("cheap moment"));
        assert!(developer_instructions.contains("Rollback is a suffix cut"));
        assert!(
            developer_instructions.contains("intended working style, not an exceptional recovery")
        );
        assert!(developer_instructions.contains("safety net behind the noise-triggered habit"));
        // Decisive maintenance: one listing, then act in the same turn.
        assert!(developer_instructions.contains("list once and act"));
        assert!(developer_instructions.contains("do not list again"));
        assert!(developer_instructions.contains("call rewind_context now"));
        // The primer is a living index carrying pointers, not full content.
        assert!(developer_instructions.contains("living index"));
        assert!(developer_instructions.contains("never run extra tools to research primer content"));
        assert!(developer_instructions.contains("ids of earlier rewind records"));
        assert!(developer_instructions.contains("rewind_backout"));
        assert!(developer_instructions.contains("grows sublinearly"));
        // No surviving sentence may gate hygiene on pressure.
        assert!(!developer_instructions.contains("At `ok` pressure, do not discover anchors"));
        assert!(!developer_instructions.contains("at low context pressure"));
        assert!(developer_instructions.contains(
            "Do not call list_rewind_anchors merely because managed_context=managed is enabled"
        ));
        assert!(developer_instructions.contains("bounded searches with compact output"));
        assert!(developer_instructions
            .contains("when nothing noisy happened there is nothing to prune"));
        assert!(!developer_instructions
            .contains("failed exploration, broad research, or finishing a coherent subtask"));
        assert!(developer_instructions.contains("list_rewind_anchors"));
        assert!(developer_instructions.contains("inspect_rewind_anchor"));
        assert!(developer_instructions.contains("rewind_context"));
    }

    #[test]
    fn managed_context_instructions_include_fission_policy() {
        let instructions = managed_context_developer_instructions(None);
        for marker in [
            "fission_spawn",
            "fork from the last completed turn and do not see the current turn",
            "fission is ex-ante, rewind is ex-post",
            "stay available at `watch` pressure",
            "valid density action",
            "unavailable under rewind-only pressure",
            "continue your own non-overlapping work",
            "fission_control(op=\"wait\")",
            "`still_running` result is normal",
            "fission_control(op=\"import\")",
            "claim_fission_canonical",
            "fission ledger in `get_status`",
        ] {
            assert!(
                instructions.contains(marker),
                "managed instructions missing fission policy marker {marker:?}"
            );
        }
    }

    #[tokio::test]
    async fn fission_policy_absent_when_managed_context_off() {
        // With managed context off, no managed developer instructions are
        // injected at all — so no fission policy reaches the model.
        let mut agent = test_agent();
        agent.managed_context = false;
        assert!(agent
            .effective_managed_context_developer_instructions()
            .await
            .is_none());
        // The non-managed instruction surface (side conversations) must not
        // advertise fission tools either.
        assert!(!side_developer_instructions(None).contains("fission_spawn"));
        assert!(!SIDE_BOUNDARY_PROMPT.contains("fission_spawn"));
    }

    #[tokio::test]
    async fn fission_policy_present_in_effective_managed_instructions() {
        let mut agent = test_agent();
        agent.managed_context = true;
        let instructions = agent
            .effective_managed_context_developer_instructions()
            .await
            .expect("managed instructions when managed_context=true");
        assert!(instructions.contains("fission_spawn"));
        assert!(instructions.contains("claim_fission_canonical"));
    }

    #[test]
    fn managed_context_instructions_append_project_file_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_FILE),
            "Use `node scripts/validate-dashboard.cjs` for dashboard QA.\n",
        )
        .unwrap();

        let instructions = managed_context_developer_instructions_for_project(
            Some("Existing developer policy."),
            Some(tmp.path()),
        );

        assert!(instructions.contains("Existing developer policy."));
        assert!(instructions.contains("managed_context=managed"));
        assert!(instructions.contains(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING));
        assert!(instructions.contains("validate-dashboard.cjs"));
        // Project extension comes after the generic block, under the heading.
        let heading_at = instructions
            .find(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING)
            .unwrap();
        let generic_at = instructions.find("managed_context=managed").unwrap();
        let project_at = instructions.find("validate-dashboard.cjs").unwrap();
        assert!(generic_at < heading_at && heading_at < project_at);
        assert!(!instructions.contains(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_TRUNCATION_MARKER));
    }

    #[test]
    fn managed_context_instructions_cap_project_file_size() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let oversized = "project guidance line\n"
            .repeat(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_MAX_BYTES / 8)
            + "UNREACHABLE-TAIL-MARKER";
        std::fs::write(
            dir.join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_FILE),
            &oversized,
        )
        .unwrap();

        let instructions =
            managed_context_developer_instructions_for_project(None, Some(tmp.path()));

        assert!(instructions.contains(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING));
        assert!(instructions.contains(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_TRUNCATION_MARKER));
        assert!(!instructions.contains("UNREACHABLE-TAIL-MARKER"));
        let project_part = instructions
            .split(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_HEADING)
            .nth(1)
            .unwrap();
        assert!(
            project_part.len()
                <= MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_MAX_BYTES
                    + MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_TRUNCATION_MARKER.len()
                    + 8
        );
    }

    #[test]
    fn managed_context_instructions_without_project_file_are_generic_only() {
        let tmp = tempfile::tempdir().unwrap();

        let with_missing_file =
            managed_context_developer_instructions_for_project(None, Some(tmp.path()));
        assert_eq!(
            with_missing_file,
            managed_context_developer_instructions(None)
        );

        let without_working_dir = managed_context_developer_instructions_for_project(None, None);
        assert_eq!(
            without_working_dir,
            managed_context_developer_instructions(None)
        );

        // Unreadable file (a directory at the file path) is non-fatal and
        // also falls back to the generic block.
        let dir = tmp
            .path()
            .join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_DIR)
            .join(MANAGED_CONTEXT_PROJECT_INSTRUCTIONS_FILE);
        std::fs::create_dir_all(&dir).unwrap();
        let with_unreadable_file =
            managed_context_developer_instructions_for_project(None, Some(tmp.path()));
        assert_eq!(
            with_unreadable_file,
            managed_context_developer_instructions(None)
        );
    }

    #[tokio::test]
    async fn thread_id_for_action_prefers_explicit_target_over_active_thread() {
        let agent = test_agent();
        *agent.active_thread_id.lock().await = Some("side-child".into());

        let explicit = agent
            .thread_id_for_action(&serde_json::json!({ "threadId": "parent-thread" }))
            .await
            .unwrap();
        assert_eq!(explicit, "parent-thread");

        let nested = agent
            .thread_id_for_action(&serde_json::json!({ "thread": { "id": "fork-target" } }))
            .await
            .unwrap();
        assert_eq!(nested, "fork-target");

        let fallback = agent
            .thread_id_for_action(&serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(fallback, "side-child");
    }

    #[test]
    fn thread_read_snapshot_extracts_rollout_path() {
        let response = serde_json::json!({
            "thread": {
                "id": "thread-abc",
                "path": "/tmp/rollout.jsonl",
            },
        });
        assert_eq!(extract_thread_id(&response).as_deref(), Some("thread-abc"));
        assert_eq!(
            extract_thread_path(&response),
            Some(PathBuf::from("/tmp/rollout.jsonl"))
        );
    }

    /// Lay out `<root>/main` as a plain repository with a linked worktree at
    /// `<root>/wt`, optionally writing the private dir's `commondir` pointer.
    /// Returns (worktree_path, main_common_git_dir).
    fn fake_linked_worktree(root: &Path, commondir: Option<&str>) -> (PathBuf, PathBuf) {
        let main_git = root.join("main").join(".git");
        let private_gitdir = main_git.join("worktrees").join("fission-a");
        std::fs::create_dir_all(&private_gitdir).unwrap();
        if let Some(commondir) = commondir {
            std::fs::write(private_gitdir.join("commondir"), commondir).unwrap();
        }
        let worktree = root.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", private_gitdir.display()),
        )
        .unwrap();
        (worktree, main_git)
    }

    #[test]
    fn thread_fork_with_options_wire_format_carries_thread_id_cwd_and_worktree_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (worktree, main_git) = fake_linked_worktree(tmp.path(), Some("../..\n"));

        let mut agent = test_agent();
        agent.writable_roots = vec!["/srv/shared-cache".to_string()];
        let params = agent.fork_thread_with_options_params("thread-abc", Some(&worktree));

        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["cwd"], worktree.to_string_lossy().as_ref());
        // No rollout path: this is a live-thread fork.
        assert!(params.get("path").is_none());
        let roots = params["config"]["sandbox_workspace_write.writable_roots"]
            .as_array()
            .expect("writable-roots config override");
        // Launch-level extra roots are re-included (the per-fork key replaces
        // the launch-arg value), then the main repo's common `.git` dir.
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], "/srv/shared-cache");
        let expected_git = std::fs::canonicalize(&main_git).unwrap();
        assert_eq!(roots[1], expected_git.to_string_lossy().as_ref());
    }

    #[test]
    fn thread_fork_with_options_wire_format_is_minimal_without_cwd() {
        let agent = test_agent();
        let params = agent.fork_thread_with_options_params("thread-abc", None);
        assert_eq!(params["threadId"], "thread-abc");
        assert!(params.get("cwd").is_none());
        assert!(params.get("config").is_none());
    }

    #[test]
    fn thread_fork_with_options_skips_config_override_for_plain_checkout() {
        // A `.git` DIRECTORY means cwd is an ordinary repository whose git
        // metadata already lives inside cwd — no writable-roots override.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let agent = test_agent();
        let params = agent.fork_thread_with_options_params("thread-abc", Some(tmp.path()));
        assert_eq!(params["cwd"], tmp.path().to_string_lossy().as_ref());
        assert!(params.get("config").is_none());
    }

    #[test]
    fn git_worktree_common_git_dir_resolves_commondir_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let (worktree, main_git) = fake_linked_worktree(tmp.path(), Some("../..\n"));
        assert_eq!(
            git_worktree_common_git_dir(&worktree),
            Some(std::fs::canonicalize(&main_git).unwrap())
        );
    }

    #[test]
    fn git_worktree_common_git_dir_falls_back_to_worktrees_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let (worktree, main_git) = fake_linked_worktree(tmp.path(), None);
        assert_eq!(
            git_worktree_common_git_dir(&worktree),
            Some(std::fs::canonicalize(&main_git).unwrap())
        );
    }

    #[test]
    fn git_worktree_common_git_dir_resolves_relative_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        let (worktree, main_git) = fake_linked_worktree(tmp.path(), Some("../.."));
        // Rewrite the `.git` file with a worktree-relative gitdir pointer.
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../main/.git/worktrees/fission-a\n",
        )
        .unwrap();
        assert_eq!(
            git_worktree_common_git_dir(&worktree),
            Some(std::fs::canonicalize(&main_git).unwrap())
        );
    }

    #[test]
    fn git_worktree_common_git_dir_ignores_plain_and_missing_repos() {
        let tmp = tempfile::tempdir().unwrap();
        // No .git at all.
        assert_eq!(git_worktree_common_git_dir(tmp.path()), None);
        // Ordinary checkout: .git is a directory.
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        assert_eq!(git_worktree_common_git_dir(tmp.path()), None);
        // Malformed .git file.
        let malformed = tmp.path().join("malformed");
        std::fs::create_dir_all(&malformed).unwrap();
        std::fs::write(malformed.join(".git"), "not a gitdir pointer\n").unwrap();
        assert_eq!(git_worktree_common_git_dir(&malformed), None);
    }

    #[test]
    fn rollback_anchor_params_accept_top_level_and_nested_forms() {
        let top = serde_json::json!({
            "itemId": "call-1",
            "position": "before",
        });
        assert_eq!(rollback_anchor_item_id(&top).unwrap(), "call-1");
        assert_eq!(
            rollback_anchor_position(&top).unwrap(),
            RollbackAnchorPosition::Before
        );

        let nested = serde_json::json!({
            "anchor": {
                "item_id": "call-2",
                "position": "after",
            },
        });
        assert_eq!(rollback_anchor_item_id(&nested).unwrap(), "call-2");
        assert_eq!(
            rollback_anchor_position(&nested).unwrap(),
            RollbackAnchorPosition::After
        );
    }

    #[test]
    fn rollback_anchor_position_defaults_to_after() {
        let params = serde_json::json!({ "itemId": "call-1" });
        assert_eq!(
            rollback_anchor_position(&params).unwrap(),
            RollbackAnchorPosition::After
        );
    }

    #[test]
    fn thread_side_fork_wire_format_is_ephemeral_with_guardrails() {
        let mut agent = test_agent();
        agent.model = Some("gpt-5.5".to_string());
        agent.working_dir = Some(PathBuf::from("/tmp/intendant-side-workspace"));
        let params = agent.side_fork_params("thread-abc", side_developer_instructions(None));
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["ephemeral"], true);
        assert_eq!(params["model"], "gpt-5.5");
        assert_eq!(params["approvalPolicy"], "on-request");
        assert_eq!(params["sandbox"], "workspace-write");
        assert_eq!(params["cwd"], "/tmp/intendant-side-workspace");
        assert!(params["developerInstructions"]
            .as_str()
            .unwrap()
            .contains("You are in a side conversation"));
    }

    #[test]
    fn thread_side_fork_disables_approvals_for_danger_full_access() {
        let mut agent = test_agent();
        agent.approval_policy = "on-request".to_string();
        agent.sandbox = "danger-full-access".to_string();

        let params = agent.side_fork_params("thread-abc", side_developer_instructions(None));

        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["sandbox"], "danger-full-access");
    }

    #[test]
    fn thread_side_developer_instructions_append_existing_policy() {
        let instructions = side_developer_instructions(Some("Existing developer policy."));
        assert!(instructions.contains("Existing developer policy."));
        assert!(instructions.contains("You are in a side conversation, not the main thread."));
        assert!(instructions.contains(
            "Only instructions submitted after the side-conversation boundary are active"
        ));
        assert!(instructions.contains("non-mutating inspection"));
        assert!(instructions.contains("Do not modify files"));
    }

    #[test]
    fn thread_side_boundary_item_matches_codex_response_item_shape() {
        let item = side_boundary_prompt_item();
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "user");
        assert_eq!(item["content"][0]["type"], "input_text");
        assert!(item["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Side conversation boundary."));
    }

    #[test]
    fn goal_response_format_includes_status_usage_and_elapsed() {
        let response = serde_json::json!({
            "goal": {
                "threadId": "thread-abc",
                "objective": "Reduce p95 latency",
                "status": "active",
                "tokenBudget": 200000,
                "tokensUsed": 1200,
                "timeUsedSeconds": 125,
                "createdAt": 1776272400,
                "updatedAt": 1776272525
            }
        });
        let formatted = format_goal_response("goal updated", &response);
        assert!(formatted.contains("Reduce p95 latency"), "{}", formatted);
        assert!(formatted.contains("status active"), "{}", formatted);
        assert!(formatted.contains("1200 / 200000 tokens"), "{}", formatted);
        assert!(formatted.contains("2m 5s"), "{}", formatted);
    }

    #[test]
    fn malformed_goal_payloads_are_treated_as_no_goal() {
        let response = serde_json::json!({
            "goal": {
                "threadId": "thread-abc",
                "status": "active",
                "tokensUsed": 10,
                "timeUsedSeconds": 2
            }
        });

        assert_eq!(
            format_goal_response("current goal", &response),
            "no goal set"
        );
        assert!(session_goal_from_value(&response["goal"]).is_none());
        assert!(session_goal_from_value(&serde_json::json!({
            "objective": "   ",
            "status": "active"
        }))
        .is_none());
    }
}
