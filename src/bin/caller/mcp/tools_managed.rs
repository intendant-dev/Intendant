//! The managed-context and controller-lifecycle tool implementations:
//! context rewind + anchors + backout, fission spawn/control/claim (and the
//! fission wait/cancel helpers), and the controller restart/halt/intervention
//! tools over the controller_loop machinery.

use super::*;

pub(crate) fn clamp_fission_wait_timeout_s(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(FISSION_WAIT_DEFAULT_TIMEOUT_S)
        .clamp(FISSION_WAIT_MIN_TIMEOUT_S, FISSION_WAIT_MAX_TIMEOUT_S)
}

/// Render a [`crate::fission_lifecycle::WaitOutcome`] as the
/// `fission_control(op="wait")` tool result. `Terminal`/`StillRunning` are
/// compact JSON group snapshots tagged with an `outcome` field —
/// `still_running` is a NORMAL result (the wait window simply elapsed), not
/// an error. `Detached` explains why the wait was refused and points at the
/// salvage paths; the not-found variants render as plain errors.
pub(crate) fn render_fission_wait_outcome(
    outcome: crate::fission_lifecycle::WaitOutcome,
    group_id: &str,
    branch_session_id: Option<&str>,
    timeout_s: u64,
) -> String {
    use crate::fission_lifecycle::WaitOutcome;
    let watched = branch_session_id.unwrap_or("any branch");
    let snapshot = |outcome: &str, group: &crate::fission_ledger::FissionGroup, message: String| {
        serde_json::to_string_pretty(&serde_json::json!({
            "op": "wait",
            "outcome": outcome,
            "group_id": group_id,
            "watched": watched,
            "message": message,
            "group": group,
        }))
        .unwrap_or_else(|_| format!("fission_control wait outcome: {outcome}"))
    };
    match outcome {
        WaitOutcome::Terminal(group) => snapshot(
            "terminal",
            &group,
            format!("{watched} reached a terminal status; use fission_control op=import to pull a branch outcome into this context"),
        ),
        WaitOutcome::StillRunning(group) => snapshot(
            "still_running",
            &group,
            format!("{watched} is still running after the {timeout_s}s wait window; this is a normal result, not an error — re-issue fission_control op=wait to keep waiting, or continue other work and check get_status fission_ledger later"),
        ),
        WaitOutcome::Detached(group) => snapshot(
            "detached",
            &group,
            format!("fission group `{group_id}` is detached (its spawn anchor left the effective history or it was explicitly severed), so it cannot be waited on or imported; salvage results manually via each branch's raw_log pointer in the group snapshot, or revisit the parent's pre-rewind state with rewind_backout"),
        ),
        WaitOutcome::GroupNotFound => format!(
            "fission_control wait failed: fission group `{group_id}` was not found in any candidate ledger; check get_status fission_ledger for known groups"
        ),
        WaitOutcome::BranchNotFound(group) => {
            let known: Vec<&str> = group
                .branches
                .iter()
                .map(|branch| branch.session_id.as_str())
                .collect();
            format!(
                "fission_control wait failed: branch `{watched}` is not part of fission group `{group_id}`; known branches: [{}]",
                known.join(", ")
            )
        }
    }
}

/// Flip a fission branch to the sticky `cancelled` status for
/// `fission_control(op="cancel")`. Verified against the ledger's setter rules
/// (`record_fission_observation`): an observation never *overwrites* a sticky
/// `detached`/`cancelled` status and never downgrades a terminal one, but
/// recording `cancelled` on a still-running branch is an allowed terminal
/// upgrade — so this explicit cancel intent can ride the observation path
/// instead of needing a dedicated ledger setter. Branches that already
/// reached a terminal status are left untouched (their recorded result stays
/// real); the terminal guard here is what prevents the one overwrite the
/// observation path WOULD permit (terminal-over-terminal, e.g. `completed`
/// -> `cancelled`).
pub(crate) fn mark_fission_branch_cancelled(
    log_dir: &std::path::Path,
    group_id: &str,
    branch_session_id: &str,
) -> Result<(String, crate::fission_ledger::FissionGroup), String> {
    let document = crate::fission_ledger::read_fission_ledger_document(log_dir)
        .map_err(|err| format!("failed to read fission ledger: {err}"))?
        .ok_or_else(|| format!("no fission ledger at {}", log_dir.display()))?;
    let group = document
        .groups
        .iter()
        .find(|group| group.group_id == group_id)
        .ok_or_else(|| format!("fission group `{group_id}` was not found"))?;
    let branch = group
        .branches
        .iter()
        .find(|branch| branch.session_id == branch_session_id)
        .ok_or_else(|| {
            format!("branch `{branch_session_id}` is not part of fission group `{group_id}`")
        })?;
    if crate::fission_ledger::branch_status_is_terminal(&branch.status) {
        return Ok((
            format!(
                "branch already has terminal status `{}`; ledger unchanged",
                branch.status
            ),
            group.clone(),
        ));
    }
    let observation = crate::fission_ledger::FissionObservation {
        parent_session_id: group.parent_session_id.clone(),
        anchor_item_id: group.anchor_item_id.clone(),
        tool: group.tool.clone(),
        status: "cancelled".to_string(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        branches: vec![crate::fission_ledger::FissionBranchObservation {
            session_id: branch_session_id.to_string(),
            status: "cancelled".to_string(),
            summary: None,
        }],
    };
    match crate::fission_ledger::record_fission_observation(log_dir, observation) {
        Ok(Some(group)) => Ok(("branch marked cancelled".to_string(), group)),
        Ok(None) => Err("ledger observation was dropped (missing ids)".to_string()),
        Err(err) => Err(format!("failed to record cancellation: {err}")),
    }
}

impl IntendantServer {
    #[tool(
        description = "Schedule a Codex context rewind to an exact item/tool-call anchor. Use it for routine noise-triggered hygiene — pruning genuinely noisy/unexpectedly large recent output at any pressure including ok, crystallizing its durable facts in the primer itself — and for managed-context recovery/density handoff guidance, rewind-only context pressure, or a watch-pressure density decision; do not use during ordinary startup/search work when nothing noisy happened. Call list_rewind_anchors once, choose one returned item_id, and rewind in the same turn; call inspect_rewind_anchor only when the compact row is ambiguous. Do not synthesize anchor ids from prior failed tool calls. The current turn will finish, Intendant will roll back Codex to the anchor, inject the primer as developer context, and resume the branch."
    )]
    pub(crate) async fn rewind_context(
        &self,
        Parameters(params): Parameters<RewindContextParams>,
    ) -> String {
        let reason = params.reason.trim();
        if reason.is_empty() {
            return "rewind_context requires a non-empty reason".to_string();
        }
        let primer = params.primer.trim();
        if primer.is_empty() {
            return "rewind_context requires a non-empty primer".to_string();
        }
        let item_id = params.anchor.item_id.trim();
        if item_id.is_empty() {
            return "rewind_context anchor.item_id must not be empty".to_string();
        }
        // Normalize case to match the action layer (RollbackAnchorPosition::from_str
        // lowercases), so `After`/`BEFORE` are accepted consistently end-to-end.
        let position = params.anchor.position.trim().to_ascii_lowercase();
        if !matches!(position.as_str(), "before" | "after") {
            return "rewind_context anchor.position must be `before` or `after`".to_string();
        }

        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "rewind_context".to_string(),
            serde_json::json!({
                "anchor": {
                    "item_id": item_id,
                    "position": position,
                },
                "reason": reason,
                "primer": primer,
                "preserve": params.preserve,
                "discard": params.discard,
                "artifacts": params.artifacts,
                "next_steps": params.next_steps,
            }),
            "rewind_context dispatched but no validation result was observed".to_string(),
        )
        .await
    }

    #[tool(
        description = "List exact Codex rewind anchors for routine noise-triggered hygiene — after genuinely noisy/unexpectedly large output, at any pressure including ok — or after recovery/density guidance or rewind-only/watch pressure. List once, then act on the returned rows via rewind_context in the same turn; do not call repeatedly — re-listing adds noise without surfacing better candidates. Do not call during ordinary startup/status/search turns or after bounded low-output searches when nothing noisy happened. Default output is a compact valid non-management page with exact item_id values, positions, summaries, filtered_total, and next_offset. Under managed density pressure, an omitted limit defaults to a one-anchor density/pruning page. Use offset/limit/query/reverse/detail for deliberate paging. For density, use density_candidates_only=true and include_pruning_estimates=true; rows hide anchors without density-valid positions and narrow positions to rewind_context-valid choices. include_non_recovery=true is diagnostic only; never pass recovery_eligible=false rows. Inspect ambiguous rows, then call rewind_context with an exact returned item_id and position."
    )]
    pub(crate) async fn list_rewind_anchors(
        &self,
        Parameters(params): Parameters<ListRewindAnchorsParams>,
    ) -> String {
        self.list_rewind_anchors_with_context(params, None).await
    }

    pub(crate) async fn list_rewind_anchors_with_context(
        &self,
        params: ListRewindAnchorsParams,
        managed_context_override: Option<bool>,
    ) -> String {
        let state = self.state.read().await;
        let density_watch = state.context_pressure_density_watch_for(
            params.session_id.as_deref(),
            managed_context_override,
        );
        let recovery_candidates_only = state.rewind_anchor_recovery_candidates_only_for(
            params.session_id.as_deref(),
            params.recovery_candidates_only,
            params.include_non_recovery,
        );
        drop(state);
        let density_maintenance_defaults =
            density_watch && !params.include_non_recovery && !params.detail;
        let effective_density_candidates_only =
            params.density_candidates_only || density_maintenance_defaults;
        let effective_include_pruning_estimates =
            params.include_pruning_estimates || density_maintenance_defaults;
        let effective_limit = params.limit.or_else(|| {
            density_maintenance_defaults.then_some(DENSITY_MAINTENANCE_ANCHOR_LIST_LIMIT)
        });
        let mut payload = serde_json::json!({
            "offset": params.offset.unwrap_or(0),
            "reverse": params.reverse,
            "include_management_tools": params.include_management_tools,
            "recovery_candidates_only": recovery_candidates_only,
            "include_non_recovery": params.include_non_recovery,
            "density_candidates_only": effective_density_candidates_only,
            "compact_catalog": !params.detail && params.offset.is_none() && params.limit.is_none() && !params.include_non_recovery,
        });
        if let Some(limit) = effective_limit {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "limit".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(limit)),
                );
            }
        }
        if effective_include_pruning_estimates {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "include_pruning_estimates".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
        }
        if params.detail {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("detail".to_string(), serde_json::Value::Bool(true));
            }
        }
        if let Some(query) = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
        {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "query".to_string(),
                    serde_json::Value::String(query.to_string()),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "list_rewind_anchors".to_string(),
            payload,
            "ok (managed-context rewind anchor listing dispatched)".to_string(),
        )
        .await
    }

    #[tool(
        description = "Inspect a single exact Codex rewind anchor with a compact before/after context window. Use only after list_rewind_anchors returns a candidate for an already-needed rewind, when the row is too lossy to choose safely."
    )]
    pub(crate) async fn inspect_rewind_anchor(
        &self,
        Parameters(params): Parameters<InspectRewindAnchorParams>,
    ) -> String {
        let item_id = params.item_id.trim();
        if item_id.is_empty() {
            return "inspect_rewind_anchor item_id must not be empty".to_string();
        }
        let mut payload = serde_json::json!({
            "anchor": {
                "item_id": item_id,
            },
            "radius": params.radius.unwrap_or(2),
        });
        if let Some(obj) = payload.as_object_mut() {
            if let Some(session_id) = params
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
            {
                obj.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "inspect_rewind_anchor".to_string(),
            payload,
            "ok (managed-context rewind anchor inspection dispatched)".to_string(),
        )
        .await
    }

    #[tool(
        description = "Recover a prior context rewind record. mode=\"inspect\" reports the saved pre-rewind rollout path. mode=\"restore\" restores the active Codex thread in place. mode=\"fork\"/\"backout\" creates a new Codex thread that inherits the lineage prompt-cache key when using the patched managed Codex binary."
    )]
    pub(crate) async fn rewind_backout(
        &self,
        Parameters(params): Parameters<RewindBackoutParams>,
    ) -> String {
        let record_id = params.record_id.trim();
        if record_id.is_empty() {
            return "rewind_backout requires a non-empty record_id".to_string();
        }
        let mode = params
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .unwrap_or("inspect");
        if !matches!(mode, "inspect" | "fork" | "backout" | "restore") {
            return "rewind_backout mode must be `inspect`, `fork`, `backout`, or `restore`"
                .to_string();
        }
        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let mut payload = serde_json::json!({
            "record_id": record_id,
            "mode": mode,
            "allow_cache_reset": params.allow_cache_reset,
        });
        if let Some(name) = name {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "name".to_string(),
                    serde_json::Value::String(name.to_string()),
                );
            }
        }

        let timeout_message = if mode == "inspect" {
            "ok (managed-context rewind record inspection dispatched)".to_string()
        } else if mode == "restore" {
            "ok (same-thread managed-context restore dispatched)".to_string()
        } else {
            "ok (managed-context lineage fork dispatched)".to_string()
        };
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "rewind_backout".to_string(),
            payload,
            timeout_message,
        )
        .await
    }

    #[tool(
        description = "Claim a fission group's canonical branch. Omit expected_canonical_session_id for first-writer-wins; provide it to deliberately compare-and-swap from the current canonical branch."
    )]
    pub(crate) async fn claim_fission_canonical(
        &self,
        Parameters(params): Parameters<ClaimFissionCanonicalParams>,
    ) -> String {
        let group_id = params.group_id.trim();
        if group_id.is_empty() {
            return "claim_fission_canonical requires a non-empty group_id".to_string();
        }
        let branch_session_id = params.branch_session_id.trim();
        if branch_session_id.is_empty() {
            return "claim_fission_canonical requires a non-empty branch_session_id".to_string();
        }
        let expected = params
            .expected_canonical_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, Some(branch_session_id), None)
            .await;
        // v1 anchor-reachability semantics: the MCP layer has no independent
        // view of the parent's effective (post-rewind) history, so the
        // ledger's own detached flag IS the reachability proxy — the rewind
        // path detaches every group whose anchor left the effective history
        // (`detach_groups_with_invalid_anchors`), and a still-attached group's
        // anchor is presumed reachable. `claim_canonical_checked` re-checks
        // the same flag internally; evaluating it here as the explicit
        // predicate keeps that v1 choice visible (and replaceable) at the
        // call site.
        let group_is_detached = crate::fission_ledger::read_fission_ledger_document(&log_dir)
            .ok()
            .flatten()
            .is_some_and(|document| document.group_is_detached(group_id));
        match crate::fission_ledger::claim_canonical_checked(
            &log_dir,
            group_id,
            branch_session_id,
            expected,
            |_anchor_item_id| !group_is_detached,
        ) {
            Ok(group) => serde_json::to_string_pretty(&group)
                .unwrap_or_else(|_| "ok (canonical branch claimed)".to_string()),
            Err(err) => format!("claim_fission_canonical failed: {err}"),
        }
    }

    /// Resolve the log dir whose `fission_ledger.json` carries `group_id`.
    /// Tries the in-process branch route registered at spawn time first, then
    /// the server's primary log dir, then every dir the calling session is
    /// known by (supervised parents log under `~/.intendant/logs/<id>/`) —
    /// the same candidate resolution the managed log/status handlers use. The
    /// first candidate whose ledger document knows the group wins; when none
    /// does, the first candidate is returned so the caller surfaces a clean
    /// group-not-found against the most authoritative dir.
    pub(crate) async fn resolve_fission_ledger_log_dir(
        &self,
        group_id: &str,
        branch_session_id: Option<&str>,
        session_id: Option<&str>,
    ) -> std::path::PathBuf {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        let push = |dir: std::path::PathBuf, candidates: &mut Vec<std::path::PathBuf>| {
            if !candidates.contains(&dir) {
                candidates.push(dir);
            }
        };
        if let Some(route) = branch_session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .and_then(crate::fission_lifecycle::branch_route)
        {
            push(route.log_dir, &mut candidates);
        }
        let (primary, session_id) = {
            let state = self.state.read().await;
            let session_id = session_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let active = state.session_id.trim();
                    (!active.is_empty()).then(|| active.to_string())
                });
            (state.log_dir.clone(), session_id)
        };
        push(primary.clone(), &mut candidates);
        if let Some(session_id) = session_id {
            for dir in requested_session_log_dirs(&self.home, &primary, &session_id) {
                push(dir, &mut candidates);
            }
        }
        for dir in &candidates {
            if let Ok(Some(document)) = crate::fission_ledger::read_fission_ledger_document(dir) {
                if document
                    .groups
                    .iter()
                    .any(|group| group.group_id == group_id)
                {
                    return dir.clone();
                }
            }
        }
        candidates.into_iter().next().unwrap_or(primary)
    }

    #[tool(
        description = "Fork this Codex thread into 1-4 full-context sibling branches that run in parallel as real sessions. Each branch needs a self-contained charter (objective + optional owned write_scope); branches fork from the last completed turn and do not see the current turn. Branches with a write_scope get an isolated git worktree by default. Returns group_id, branch session ids, and worktree paths; track progress via get_status fission_ledger."
    )]
    pub(crate) async fn fission_spawn(
        &self,
        Parameters(params): Parameters<FissionSpawnParams>,
    ) -> String {
        if params.branches.is_empty() || params.branches.len() > FISSION_SPAWN_MAX_BRANCHES {
            return format!(
                "fission_spawn requires between 1 and {FISSION_SPAWN_MAX_BRANCHES} branches; got {}",
                params.branches.len()
            );
        }
        let mut branches = Vec::with_capacity(params.branches.len());
        for (idx, branch) in params.branches.iter().enumerate() {
            let objective = branch.objective.trim();
            if objective.is_empty() {
                return format!(
                    "fission_spawn branches[{idx}] requires a non-empty self-contained objective"
                );
            }
            let mut spec = serde_json::json!({ "objective": objective });
            if let Some(write_scope) = &branch.write_scope {
                spec["write_scope"] = serde_json::json!(write_scope
                    .iter()
                    .map(|path| path.trim())
                    .filter(|path| !path.is_empty())
                    .collect::<Vec<_>>());
            }
            if let Some(name) = branch
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                spec["name"] = serde_json::Value::String(name.to_string());
            }
            branches.push(spec);
        }
        let mut payload = serde_json::json!({ "branches": branches });
        if let Some(use_worktree) = params.use_worktree {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "use_worktree".to_string(),
                    serde_json::Value::Bool(use_worktree),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "fission_spawn".to_string(),
            payload,
            "fission_spawn dispatched but no spawn result was observed".to_string(),
        )
        .await
    }

    #[tool(
        description = "Manage a fission branch. op=wait blocks (capped timeout_s, default 60, max 300) until the branch is terminal and returns the group snapshot — still_running on timeout is normal. op=import returns the branch outcome (summary, changed files, raw-log pointer) into this context and marks it imported. op=cancel stops the branch session. op=detach abandons it without stopping. Detached branches cannot be waited on or imported."
    )]
    pub(crate) async fn fission_control(
        &self,
        Parameters(params): Parameters<FissionControlParams>,
    ) -> String {
        let group_id = params.group_id.trim();
        if group_id.is_empty() {
            return "fission_control requires a non-empty group_id".to_string();
        }
        let op = params.op.trim().to_ascii_lowercase();
        if !matches!(op.as_str(), "wait" | "import" | "cancel" | "detach") {
            return format!(
                "fission_control op must be `wait`, `import`, `cancel`, or `detach`; got `{}`",
                params.op.trim()
            );
        }
        let branch_session_id = params
            .branch_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let Some(branch_session_id) = branch_session_id else {
            if op == "wait" {
                // Waiting without a branch watches for ANY branch of the
                // group to become terminal.
                return self
                    .fission_control_wait(
                        group_id,
                        None,
                        params.timeout_s,
                        params.session_id.as_deref(),
                    )
                    .await;
            }
            return format!("fission_control op={op} requires branch_session_id");
        };
        match op.as_str() {
            "wait" => {
                self.fission_control_wait(
                    group_id,
                    Some(branch_session_id),
                    params.timeout_s,
                    params.session_id.as_deref(),
                )
                .await
            }
            "import" => {
                // Stage A's `fission_import` thread-action handler injects the
                // branch outcome into the parent thread and returns it as the
                // action result message; this tool just relays that message.
                self.dispatch_codex_thread_action_and_wait(
                    params.session_id.clone(),
                    "fission_import".to_string(),
                    serde_json::json!({
                        "group_id": group_id,
                        "branch_session_id": branch_session_id,
                    }),
                    "fission_import dispatched but no import result was observed".to_string(),
                )
                .await
            }
            "cancel" => {
                self.fission_control_cancel(
                    group_id,
                    branch_session_id,
                    params.session_id.as_deref(),
                )
                .await
            }
            "detach" => {
                self.fission_control_detach(
                    group_id,
                    branch_session_id,
                    params.session_id.as_deref(),
                )
                .await
            }
            _ => unreachable!("op validated above"),
        }
    }

    pub(crate) async fn fission_control_wait(
        &self,
        group_id: &str,
        branch_session_id: Option<&str>,
        timeout_s: Option<u64>,
        session_id: Option<&str>,
    ) -> String {
        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, branch_session_id, session_id)
            .await;
        let timeout_s = clamp_fission_wait_timeout_s(timeout_s);
        match crate::fission_lifecycle::wait_for_branch_terminal(
            &log_dir,
            group_id,
            branch_session_id,
            std::time::Duration::from_secs(timeout_s),
        )
        .await
        {
            Ok(outcome) => {
                render_fission_wait_outcome(outcome, group_id, branch_session_id, timeout_s)
            }
            Err(err) => format!(
                "fission_control wait failed reading the fission ledger at {}: {err}",
                log_dir.display()
            ),
        }
    }

    pub(crate) async fn fission_control_cancel(
        &self,
        group_id: &str,
        branch_session_id: &str,
        session_id: Option<&str>,
    ) -> String {
        // Stop the live branch session through the same control-plane intent
        // as the dashboard's stop button (`ControlMsg::StopSession`); the
        // session supervisor owns the actual backend shutdown.
        self.bus
            .send(AppEvent::ControlCommand(ControlMsg::StopSession {
                session_id: branch_session_id.to_string(),
            }));
        let stop_note = format!("stop requested for branch session `{branch_session_id}`");

        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, Some(branch_session_id), session_id)
            .await;
        let (ledger_note, group) =
            match mark_fission_branch_cancelled(&log_dir, group_id, branch_session_id) {
                Ok((note, group)) => (note, Some(group)),
                Err(err) => (format!("ledger not updated: {err}"), None),
            };
        let mut result = serde_json::json!({
            "op": "cancel",
            "group_id": group_id,
            "branch_session_id": branch_session_id,
            "stop": stop_note,
            "ledger": ledger_note,
        });
        if let (Some(obj), Some(group)) = (result.as_object_mut(), group) {
            obj.insert(
                "group".to_string(),
                serde_json::to_value(group).unwrap_or_else(|_| serde_json::json!({})),
            );
        }
        serde_json::to_string_pretty(&result)
            .unwrap_or_else(|_| "ok (fission branch cancel dispatched)".to_string())
    }

    pub(crate) async fn fission_control_detach(
        &self,
        group_id: &str,
        branch_session_id: &str,
        session_id: Option<&str>,
    ) -> String {
        let log_dir = self
            .resolve_fission_ledger_log_dir(group_id, Some(branch_session_id), session_id)
            .await;
        match crate::fission_ledger::detach_group(&log_dir, group_id, FISSION_CONTROL_DETACH_REASON)
        {
            Ok(group) => {
                // Let frontends draw the severed edge: same relationship kind
                // the lineage ledger folds into a `detached` branch row.
                self.bus.send(AppEvent::SessionRelationship {
                    parent_session_id: group.parent_session_id.clone(),
                    child_session_id: branch_session_id.to_string(),
                    relationship: "fission-detached".to_string(),
                    ephemeral: false,
                });
                serde_json::to_string_pretty(&serde_json::json!({
                    "op": "detach",
                    "group_id": group_id,
                    "branch_session_id": branch_session_id,
                    "detach_reason": FISSION_CONTROL_DETACH_REASON,
                    "message": "group detached without stopping its sessions; detached branches cannot be waited on or imported",
                    "group": group,
                }))
                .unwrap_or_else(|_| "ok (fission group detached)".to_string())
            }
            Err(err) => format!("fission_control detach failed: {err}"),
        }
    }

    #[tool(
        description = "Schedule a controller restart workflow. Returns a restart ID and a completion token that must be passed to controller_turn_complete as the final controller action."
    )]
    pub(crate) async fn schedule_controller_restart(
        &self,
        Parameters(mut params): Parameters<ScheduleControllerRestartParams>,
    ) -> String {
        normalize_schedule_controller_restart_params(&mut params);
        if let Err(e) = validate_schedule_controller_restart_params(&params) {
            return schedule_error_response(e, None, None);
        }

        let restart = {
            let mut s = self.state.write().await;
            if let Some(active) = s.controller_restart.as_ref() {
                if matches!(
                    active.phase,
                    RestartPhase::AwaitingTurnComplete
                        | RestartPhase::Ready
                        | RestartPhase::Restarting
                ) {
                    return schedule_error_response(
                        format!(
                            "A restart is already active (id={}, phase={:?})",
                            active.restart_id, active.phase
                        ),
                        Some(active.restart_id.as_str()),
                        Some(active.phase),
                    );
                }
            }

            let restart = ControllerRestartState::new(&params);
            s.push_log(
                LogLevel::Info,
                format!(
                    "Controller restart scheduled for '{}' (id={})",
                    restart.controller_id, restart.restart_id
                ),
            );
            s.controller_restart = Some(restart.clone());
            persist_restart_state(&s.log_dir, &s.controller_restart);
            restart
        };

        let mut output = serde_json::json!({
            "status": "scheduled",
            "restart_id": restart.restart_id,
            "turn_complete_token": restart.turn_complete_token,
            "ok": true,
        });
        let mut command_ok = true;

        if matches!(restart.restart_after, RestartAfter::Now) {
            match self.run_scheduled_controller_restart().await {
                Ok(result) => {
                    output["execution"] = serde_json::Value::String(if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    });
                }
                Err(e) => {
                    command_ok = false;
                    output["execution_error"] = serde_json::Value::String(e);
                }
            }
        }
        output["ok"] = serde_json::Value::Bool(command_ok);
        let phase = {
            let s = self.state.read().await;
            s.controller_restart
                .as_ref()
                .map(restart_phase_value)
                .unwrap_or_else(|| {
                    serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                })
        };
        output["phase"] = phase;

        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Final handshake call from the controlling agent before ending its turn. Validates token and executes any pending scheduled restart."
    )]
    pub(crate) async fn controller_turn_complete(
        &self,
        Parameters(mut params): Parameters<ControllerTurnCompleteParams>,
    ) -> String {
        normalize_controller_turn_complete_params(&mut params);
        {
            let mut s = self.state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    None,
                    "No controller restart is scheduled".to_string(),
                );
            };

            if active.restart_id != params.restart_id {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "restart_id does not match the active restart".to_string(),
                );
            }
            if active.turn_complete_token != params.turn_complete_token {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "turn_complete_token is invalid".to_string(),
                );
            }
            if !matches!(active.phase, RestartPhase::AwaitingTurnComplete) {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    format!(
                        "Restart is not awaiting completion (phase={:?})",
                        active.phase
                    ),
                );
            }

            active.handoff_summary = params.handoff_summary.clone();
            active.completion_status = params.status.clone();
            active.phase = RestartPhase::Ready;
            active.updated_at = ControllerRestartState::now_string();
            let restart_id = active.restart_id.clone();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            s.push_log(
                LogLevel::Info,
                format!("Controller turn complete acknowledged (id={})", restart_id),
            );
        }

        match self.run_scheduled_controller_restart().await {
            Ok(result) => {
                let mut output = serde_json::json!({
                    "status": "completed",
                    "restart_id": params.restart_id,
                    "ok": true,
                });
                output["execution"] = serde_json::Value::String(if result.is_empty() {
                    "ok".to_string()
                } else {
                    result
                });
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart
                        .as_ref()
                        .map(restart_phase_value)
                        .unwrap_or(serde_json::Value::Null)
                };
                output["phase"] = phase;
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart.as_ref().map(|r| r.phase)
                };
                restart_error_response("restart_pending", &params.restart_id, phase, e)
            }
        }
    }

    #[tool(
        description = "Get the current controller restart state, if any. Returns null when no restart is tracked."
    )]
    pub(crate) async fn get_restart_status(&self) -> String {
        let s = self.state.read().await;
        let value = restart_state_public_value(s.controller_restart.as_ref());
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
    }

    #[tool(description = "Cancel a scheduled controller restart.")]
    pub(crate) async fn cancel_controller_restart(
        &self,
        Parameters(mut params): Parameters<CancelControllerRestartParams>,
    ) -> String {
        normalize_cancel_controller_restart_params(&mut params);
        let mut s = self.state.write().await;
        let log_dir = s.log_dir.clone();
        let requested_restart_id = params.restart_id.as_deref();
        let Some(active) = s.controller_restart.as_mut() else {
            return schedule_error_response(
                "No controller restart is scheduled".to_string(),
                requested_restart_id,
                None,
            );
        };

        if let Some(expected_id) = requested_restart_id {
            if expected_id != active.restart_id {
                return schedule_error_response(
                    format!(
                        "restart_id '{}' does not match active '{}'",
                        expected_id, active.restart_id
                    ),
                    Some(active.restart_id.as_str()),
                    Some(active.phase),
                );
            }
        }

        active.phase = RestartPhase::Cancelled;
        active.updated_at = ControllerRestartState::now_string();
        active.last_result = Some("Cancelled by operator".to_string());
        let restart_id = active.restart_id.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        s.push_log(
            LogLevel::Info,
            format!("Controller restart cancelled (id={})", restart_id),
        );
        serde_json::json!({
            "status": "cancelled",
            "ok": true,
            "restart_id": restart_id,
            "phase": RestartPhase::Cancelled,
        })
        .to_string()
    }

    #[tool(
        description = "Request graceful controller-loop halt. By default this blocks all future cycles until cleared; set persistent=false for one-shot halt-after-cycle behavior."
    )]
    pub(crate) async fn request_controller_loop_halt(
        &self,
        Parameters(params): Parameters<RequestControllerLoopHaltParams>,
    ) -> String {
        let loop_dir = controller_loop_dir();
        let persistent = params.persistent.unwrap_or(true);
        if let Err(e) = request_loop_halt_marker(&loop_dir, persistent) {
            return serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string();
        }
        self.state
            .read()
            .await
            .invalidate_controller_loop_raw_status_cache();
        collect_controller_loop_status(&loop_dir).to_string()
    }

    #[tool(description = "Clear controller-loop halt flags so future cycles may start again.")]
    pub(crate) async fn clear_controller_loop_halt(&self) -> String {
        let loop_dir = controller_loop_dir();
        if let Err(e) = clear_loop_halt_markers(&loop_dir) {
            return serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string();
        }
        self.state
            .read()
            .await
            .invalidate_controller_loop_raw_status_cache();
        collect_controller_loop_status(&loop_dir).to_string()
    }

    #[tool(
        description = "Intervene in the active controller loop: mode='stop' requests graceful stop; mode='abort' requests immediate kill."
    )]
    pub(crate) async fn intervene_controller_loop(
        &self,
        Parameters(params): Parameters<InterveneControllerLoopParams>,
    ) -> String {
        let loop_dir = controller_loop_dir();
        match request_loop_intervention_marker(&loop_dir, &params.mode) {
            Ok(intervention) => {
                self.state
                    .read()
                    .await
                    .invalidate_controller_loop_raw_status_cache();
                let mut status = collect_controller_loop_status(&loop_dir);
                add_controller_loop_intervention_report(&mut status, &intervention);
                serde_json::json!({
                    "ok": true,
                    "mode": intervention.mode.as_str(),
                    "intervention": controller_loop_intervention_report(&intervention),
                    "status": status,
                })
                .to_string()
            }
            Err(e) => serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string(),
        }
    }

    #[tool(
        description = "Get normalized controller-loop health: latest run pointers, halt/intervention flags, lock owner, and active wrapper/codex PID counts."
    )]
    pub(crate) async fn get_controller_loop_status(&self) -> String {
        collect_controller_loop_status_with_state(&controller_loop_dir(), &self.state)
            .await
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tests::{
        spawn_codex_thread_action_result, test_state, test_state_with_log_dir,
    };
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    /// Like [`spawn_codex_thread_action_result`], but returns the dispatched
    /// thread-action params so tests can assert the exact wire shape.
    fn spawn_codex_thread_action_capture(
        bus: EventBus,
        expected_action: &'static str,
        message: &'static str,
    ) -> tokio::task::JoinHandle<serde_json::Value> {
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                        session_id,
                        op,
                        params,
                        ..
                    })) if op == expected_action => {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id,
                            action: op,
                            success: true,
                            message: message.to_string(),
                            record_id: None,
                        });
                        return params;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return serde_json::Value::Null;
                    }
                }
            }
        })
    }

    #[test]
    fn usage_snapshot_preserves_known_hard_limit_when_backend_collapses_to_soft_limit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "codex".to_string(),
                tokens_used: 245_915,
                context_window: 258_400,
                hard_context_window: Some(272_000),
                usage_pct: 95.2,
                prompt_tokens: 245_915,
                completion_tokens: 0,
                cached_tokens: 0,
                ..Default::default()
            });
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "codex".to_string(),
                tokens_used: 258_400,
                context_window: 258_400,
                hard_context_window: Some(258_400),
                usage_pct: 100.0,
                prompt_tokens: 258_400,
                completion_tokens: 0,
                cached_tokens: 0,
                ..Default::default()
            });

            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["status"], "high");
            assert_eq!(pressure["used_tokens"], 258_400);
            assert_eq!(pressure["context_window"], 258_400);
            assert_eq!(pressure["hard_limit"], 272_000);
            assert_eq!(pressure["remaining_hard_tokens"], 13_600);
            assert_eq!(pressure["rewind_only"], true);
            assert_eq!(pressure["normal_tools_allowed"], false);
            assert_eq!(pressure["required_action"], "rewind_context");
        });
    }

    #[test]
    fn rewind_only_gate_blocks_non_rewind_tools_for_active_codex_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 80_000,
                completion_tokens: 20_000,
                cached_tokens: 0,
                ..Default::default()
            });

            let message = s
                .rewind_only_gate_message("take_screenshot")
                .expect("Codex action tool should be gated");
            assert!(message.contains(
                "model-facing tools are limited to get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout"
            ));
            assert!(message.contains("Read-only supervisor observability tools"));
            assert!(s.rewind_only_gate_message("get_status").is_none());
            assert!(s.rewind_only_gate_message("list_rewind_anchors").is_none());
            assert!(s.rewind_only_gate_message("inspect_rewind_anchor").is_none());
            assert!(s.rewind_only_gate_message("rewind_context").is_none());
            assert!(s.rewind_only_gate_message("rewind_backout").is_none());
            // Fission tools are deliberately absent from the rewind-only
            // recovery list: under rewind-only pressure, forking new branches
            // or importing their output is ordinary work and must be blocked
            // like any other model-facing tool — the parent must shrink
            // first. (At density watch, below rewind-only, they stay allowed;
            // see density_watch_does_not_gate_fission_tools.)
            assert!(s.rewind_only_gate_message("fission_spawn").is_some());
            assert!(s.rewind_only_gate_message("fission_control").is_some());
            assert!(s
                .rewind_only_gate_message("claim_fission_canonical")
                .is_some());
        });
    }

    #[test]
    fn density_watch_does_not_gate_fission_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            // 90k/100k: at or above the 85% recommended density threshold,
            // below the rewind-only limit — the density-watch band.
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 90_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 90.0,
                prompt_tokens: 70_000,
                completion_tokens: 20_000,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(
                s.context_pressure_density_watch_for(None, None),
                "test premise: usage must sit in the density-watch band"
            );
            // The MCP-side gate only fires at rewind-only pressure: fission
            // calls pass at watch band, where spawning a branch is itself a
            // valid density action.
            assert!(s.rewind_only_gate_message("fission_spawn").is_none());
            assert!(s.rewind_only_gate_message("fission_control").is_none());
            assert!(s
                .rewind_only_gate_message("claim_fission_canonical")
                .is_none());
            // The watch-band status message advertises fission delegation as
            // a density action.
            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["status"], "watch");
            assert!(pressure["message"]
                .as_str()
                .unwrap()
                .contains("Fission tools stay allowed at watch"));
        });
    }

    #[test]
    fn rewind_only_gate_allows_supervisor_observability_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 258_400,
                context_window: 258_400,
                hard_context_window: Some(272_000),
                usage_pct: 100.0,
                prompt_tokens: 258_000,
                completion_tokens: 400,
                cached_tokens: 0,
                ..Default::default()
            });

            assert!(s.rewind_only_gate_message("get_logs").is_none());
            assert!(s.rewind_only_gate_message("get_pending_approval").is_none());
            assert!(s.rewind_only_gate_message("get_pending_input").is_none());
            assert!(s
                .rewind_only_gate_message("get_controller_loop_status")
                .is_none());
            assert!(s.rewind_only_gate_message("get_restart_status").is_none());
            assert!(s
                .rewind_only_gate_message("request_controller_loop_halt")
                .is_some());
        });
    }

    #[test]
    fn get_logs_remains_callable_under_managed_rewind_only_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 258_400,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 100.0,
                    prompt_tokens: 258_000,
                    completion_tokens: 400,
                    cached_tokens: 0,
                    ..Default::default()
                });
                s.push_log(
                    LogLevel::Info,
                    "supervisor log is still readable".to_string(),
                );
            }
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .call_tool_by_name_for_session(
                    "get_logs",
                    serde_json::json!({ "limit": 160 }),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].content, "supervisor log is still readable");

            let controller_status = server
                .call_tool_by_name_for_session(
                    "get_controller_loop_status",
                    serde_json::json!({}),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(!controller_status.is_error.unwrap_or(false));
        });
    }

    #[test]
    fn list_tools_hides_rewind_tools_until_managed_context_is_enabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let server = IntendantServer::new(state.clone(), EventBus::new());

            let tools = server.list_tools_json().await;
            let names: Vec<_> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(names.contains(&"get_status"));
            assert!(!names.contains(&"list_rewind_anchors"));
            assert!(!names.contains(&"rewind_context"));
            assert!(!names.contains(&"rewind_backout"));

            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = true;
                s.codex_managed_context = true;
            }
            let tools = server.list_tools_json().await;
            let names: Vec<_> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(names.contains(&"list_rewind_anchors"));
            assert!(names.contains(&"rewind_context"));
            assert!(names.contains(&"rewind_backout"));
        });
    }

    #[test]
    fn list_tools_uses_session_scoped_managed_context_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = false;
                s.session_codex_managed_context
                    .insert("vanilla-session".to_string(), false);
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
            }
            let server = IntendantServer::new(state, EventBus::new());

            let vanilla = server
                .list_tools_json_for_session(Some("vanilla-session"), None, None)
                .await;
            let vanilla_names: Vec<_> = vanilla["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(!vanilla_names.contains(&"list_rewind_anchors"));
            assert!(!vanilla_names.contains(&"rewind_context"));
            assert!(!vanilla_names.contains(&"rewind_backout"));

            let managed = server
                .list_tools_json_for_session(Some("managed-session"), None, None)
                .await;
            let managed_names: Vec<_> = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_names.contains(&"list_rewind_anchors"));
            assert!(managed_names.contains(&"rewind_context"));
            assert!(managed_names.contains(&"rewind_backout"));

            let managed_by_url = server
                .list_tools_json_for_session(Some("vanilla-session"), Some(true), None)
                .await;
            let managed_by_url_names: Vec<_> = managed_by_url["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_by_url_names.contains(&"list_rewind_anchors"));
            assert!(managed_by_url_names.contains(&"rewind_context"));
        });
    }

    #[test]
    fn list_tools_core_profile_keeps_only_bootstrap_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            state
                .write()
                .await
                .session_codex_managed_context
                .insert("managed-session".to_string(), true);
            let server = IntendantServer::new(state, EventBus::new());

            let vanilla = server
                .list_tools_json_for_session(None, Some(false), Some("core"))
                .await;
            let vanilla_names: Vec<_> = vanilla["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(vanilla_names.contains(&"get_status"));
            assert!(vanilla_names.contains(&"show_shared_view"));
            assert!(vanilla_names.contains(&"focus_shared_view"));
            assert!(vanilla_names.contains(&"clear_shared_view_focus"));
            assert!(vanilla_names.contains(&"request_shared_view_input"));
            assert!(vanilla_names.contains(&"capture_shared_view_frame"));
            assert!(vanilla_names.contains(&"hide_shared_view"));
            // The minimal display/CU surface is part of the bootstrap set for
            // vanilla sessions too — every supervised backend gets screenshots
            // and input actions over MCP; only managed rewind/fission tools
            // stay behind managed context.
            assert!(vanilla_names.contains(&"list_displays"));
            assert!(vanilla_names.contains(&"grant_user_display"));
            assert!(vanilla_names.contains(&"revoke_user_display"));
            assert!(vanilla_names.contains(&"take_screenshot"));
            assert!(vanilla_names.contains(&"read_screen"));
            assert!(vanilla_names.contains(&"execute_cu_actions"));
            assert!(!vanilla_names.contains(&"spawn_live_audio"));
            assert!(!vanilla_names.contains(&"list_frames"));
            assert!(!vanilla_names.contains(&"list_rewind_anchors"));
            assert!(!vanilla_names.contains(&"rewind_context"));
            assert!(!vanilla_names.contains(&"fission_spawn"));
            assert!(!vanilla_names.contains(&"fission_control"));
            assert!(!vanilla_names.contains(&"claim_fission_canonical"));

            let managed = server
                .list_tools_json_for_session(Some("managed-session"), None, Some("core"))
                .await;
            let managed_names: Vec<_> = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_names.contains(&"list_rewind_anchors"));
            assert!(managed_names.contains(&"inspect_rewind_anchor"));
            assert!(managed_names.contains(&"rewind_context"));
            assert!(managed_names.contains(&"rewind_backout"));
            assert!(managed_names.contains(&"fission_spawn"));
            assert!(managed_names.contains(&"fission_control"));
            assert!(managed_names.contains(&"claim_fission_canonical"));
            assert!(managed_names.contains(&"list_displays"));
            assert!(managed_names.contains(&"grant_user_display"));
            assert!(managed_names.contains(&"revoke_user_display"));
            assert!(managed_names.contains(&"take_screenshot"));
            assert!(managed_names.contains(&"read_screen"));
            assert!(managed_names.contains(&"execute_cu_actions"));
            assert!(!managed_names.contains(&"spawn_live_audio"));

            let grant_schema = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "grant_user_display")
                .and_then(|tool| tool.pointer("/inputSchema/properties/display_id"))
                .expect("grant_user_display display_id schema");
            assert!(
                grant_schema["type"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|ty| ty.as_str() == Some("integer"))),
                "grant_user_display display_id schema: {grant_schema:?}"
            );

            let list_description = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "list_rewind_anchors")
                .and_then(|tool| tool["description"].as_str())
                .expect("list_rewind_anchors description");
            // Noise-triggered routine hygiene is the first listed use and is
            // valid at any pressure; the startup/search prohibition targets
            // no-noise situations, not low pressure.
            assert!(list_description.contains("routine noise-triggered hygiene"));
            assert!(list_description.contains("at any pressure including ok"));
            assert!(list_description.contains("List once"));
            assert!(list_description.contains("re-listing adds noise"));
            assert!(list_description
                .contains("Do not call during ordinary startup/status/search turns"));
            assert!(list_description.contains("bounded low-output searches"));
            assert!(list_description.contains("when nothing noisy happened"));
            assert!(list_description.contains("genuinely noisy/unexpectedly large"));
            assert!(!list_description.contains("context_pressure.status is ok"));
            assert!(!list_description.contains("call_"));

            let rewind_description = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "rewind_context")
                .and_then(|tool| tool["description"].as_str())
                .expect("rewind_context description");
            assert!(rewind_description.contains("routine noise-triggered hygiene"));
            assert!(rewind_description.contains("at any pressure including ok"));
            assert!(
                rewind_description.contains("crystallizing its durable facts in the primer itself")
            );
            assert!(rewind_description.contains("rewind in the same turn"));
            assert!(rewind_description.contains(
                "do not use during ordinary startup/search work when nothing noisy happened"
            ));
            assert!(!rewind_description.contains("ordinary low-pressure"));
        });
    }

    #[test]
    fn call_tool_rejects_rewind_tools_when_managed_context_is_disabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            let result = server
                .call_tool_by_name(
                    "rewind_context",
                    serde_json::json!({
                        "item_id": "call-1",
                        "primer": "carry forward enough state"
                    }),
                )
                .await
                .unwrap();
            let rendered = format!("{result:?}");
            assert!(rendered.contains("managed context is disabled"));
        });
    }

    #[test]
    fn call_tool_respects_session_scoped_managed_context_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = true;
            }
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({}),
                    Some("vanilla-session"),
                    Some(false),
                )
                .await
                .unwrap();
            let rendered = format!("{result:?}");
            assert!(rendered.contains("managed context is disabled"));
        });
    }

    #[test]
    fn rewind_context_defaults_to_http_session_id() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus.clone());

            let event_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "rewind_context" => {
                            let event = (session_id.clone(), op.clone(), params);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "context rewind scheduled".to_string(),
                                record_id: None,
                            });
                            break event;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({
                        "anchor": {"item_id": "call-1", "position": "after"},
                        "reason": "trim noisy branch",
                        "primer": "carry forward the durable facts"
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("context rewind scheduled")
            );

            let event = timeout(Duration::from_secs(1), event_task)
                .await
                .expect("expected CodexThreadAction control command")
                .unwrap();

            assert_eq!(event.0.as_deref(), Some("backend-session-1"));
            assert_eq!(event.1, "rewind_context");
            assert_eq!(event.2["anchor"]["item_id"], "call-1");
        });
    }

    #[test]
    fn rewind_context_surfaces_validation_failure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            ..
                        })) if op == "rewind_context" => {
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: false,
                                message:
                                    "rollback anchor item_id `rewind_context-call_6` was not found; call list_rewind_anchors"
                                        .to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({
                        "anchor": {"item_id": "rewind_context-call_6", "position": "after"},
                        "reason": "recover pressure",
                        "primer": "dense continuation"
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            assert!(text.contains("rewind_context failed"), "got: {text}");
            assert!(text.contains("call list_rewind_anchors"), "got: {text}");
            result_task.await.unwrap();
        });
    }

    #[test]
    fn list_rewind_anchors_defaults_to_http_session_id_and_returns_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 25);
                            assert_eq!(params["limit"], 50);
                            assert_eq!(params["query"], "tool");
                            assert_eq!(params["reverse"], true);
                            assert_eq!(params["include_pruning_estimates"], true);
                            assert_eq!(params["recovery_candidates_only"], true);
                            assert_eq!(params["include_non_recovery"], false);
                            assert_eq!(params["density_candidates_only"], true);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[]}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({
                        "offset": 25,
                        "limit": 50,
                        "query": "tool",
                        "reverse": true,
                        "include_pruning_estimates": true,
                        "density_candidates_only": true,
                        "recovery_candidates_only": false
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchors\":[]}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn list_rewind_anchors_omits_limit_when_unspecified() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 0);
                            assert!(
                                params.get("limit").is_none(),
                                "unspecified limit should let the backend compact default apply: {params}"
                            );
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[],\"limit\":5}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({}),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchors\":[],\"limit\":5}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn list_rewind_anchors_defaults_to_tiny_density_page_under_watch_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "backend-session-1".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 253_793,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 98.3,
                    prompt_tokens: 253_000,
                    completion_tokens: 793,
                    cached_tokens: 0,
                    ..Default::default()
                });
            }

            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 0);
                            assert_eq!(params["limit"], DENSITY_MAINTENANCE_ANCHOR_LIST_LIMIT);
                            assert_eq!(params["density_candidates_only"], true);
                            assert_eq!(params["include_pruning_estimates"], true);
                            assert_eq!(params["compact_catalog"], true);
                            assert_eq!(params["recovery_candidates_only"], true);
                            assert_eq!(params["include_non_recovery"], false);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[{\"item_id\":\"call_density_0\",\"positions\":[\"after\"],\"position_hint\":\"after\"}],\"limit\":1}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({}),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap();
            assert!(text.len() < 256, "density catalog result too large: {text}");
            assert!(text.contains("call_density_0"));
            result_task.await.unwrap();
        });
    }

    #[test]
    fn inspect_rewind_anchor_defaults_to_http_session_id_and_returns_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                            ..
                        })) if op == "inspect_rewind_anchor" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["anchor"]["item_id"], "call-1");
                            assert_eq!(params["radius"], 3);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchor\":{\"item_id\":\"call-1\"}}".to_string(),
                                record_id: None,
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "inspect_rewind_anchor",
                    serde_json::json!({
                        "item_id": "call-1",
                        "radius": 3
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchor\":{\"item_id\":\"call-1\"}}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn get_status_for_wrapper_hydrates_backend_context_snapshot_from_session_log() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let wrapper_dir = dir.path().join("wrapper-session");
            let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
            log.write_meta(None, Some("managed Codex task"));
            let capabilities = crate::types::SessionCapabilities {
                follow_up: true,
                steer: true,
                interrupt: true,
                thread_actions: Vec::new(),
                codex_thread_actions: vec!["rewind_context".to_string()],
                codex_managed_context: Some("managed".to_string()),
                codex_sandbox: Some("danger-full-access".to_string()),
                codex_approval_policy: Some("never".to_string()),
                codex_context_archive: None,
                codex_command: Some("/tmp/codex".to_string()),
                codex_fast_mode: None,
                codex_service_tier: None,
            };
            log.session_capabilities("wrapper-session", &capabilities);
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(wrapper_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:00.000",
                        "event": "session_identity",
                        "level": "info",
                        "message": "Session identity: wrapper-session -> codex:codex-thread",
                        "data": {
                            "session_id": "wrapper-session",
                            "source": "codex",
                            "backend_session_id": "codex-thread",
                        },
                    })
                )
                .unwrap();
            }
            log.session_started("codex-thread", Some("managed Codex task"));
            log.agent_started_with_session_id(
                Some("codex-thread"),
                5,
                "edit src/bin/caller/mcp.rs",
                None,
                Some("Codex"),
            );
            log.context_snapshot_for_session(
                Some("codex-thread"),
                "codex",
                "Codex resolved request payload",
                Some("req-1"),
                Some(1),
                Some(5),
                "openai.responses.resolved_request.v1",
                Some(50_332),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(64),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );

            let state = test_state_with_log_dir(wrapper_dir);
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
                s.configured_codex_managed_context = true;
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();

            assert_eq!(
                value.pointer("/session_id"),
                Some(&"wrapper-session".into())
            );
            assert_eq!(value.pointer("/phase"), Some(&"running_agent".into()));
            assert_eq!(value.pointer("/provider"), Some(&"openai".into()));
            assert_eq!(value.pointer("/model"), Some(&"gpt-5.2-codex".into()));
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&50_332.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&50_332.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&258_400.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn rewind_backout_fork_dispatches_without_cache_reset_opt_in() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let result_task = spawn_codex_thread_action_result(
                bus,
                "rewind_backout",
                "forked context rewind record rewind-1 with inherited lineage prompt-cache key into thread thread-2",
            );
            let forked = server
                .rewind_backout(Parameters(RewindBackoutParams {
                    session_id: None,
                    record_id: "rewind-1".to_string(),
                    mode: Some("fork".to_string()),
                    name: None,
                    allow_cache_reset: false,
                }))
                .await;
            assert_eq!(
                forked,
                "forked context rewind record rewind-1 with inherited lineage prompt-cache key into thread thread-2"
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn rewind_backout_returns_thread_action_result_to_caller() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let result_task = spawn_codex_thread_action_result(
                bus,
                "rewind_backout",
                "context rewind record rewind-1: pre-rewind rollout copied from source to recovery; restore uses same-thread Codex thread/restore when available",
            );

            let inspected = server
                .rewind_backout(Parameters(RewindBackoutParams {
                    session_id: None,
                    record_id: "rewind-1".to_string(),
                    mode: Some("inspect".to_string()),
                    name: None,
                    allow_cache_reset: false,
                }))
                .await;

            assert!(inspected.contains("same-thread Codex thread/restore"));
            assert!(!inspected.contains("dispatched"));
            result_task.await.unwrap();
        });
    }

    #[test]
    fn claim_fission_canonical_tool_updates_ledger() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            crate::fission_ledger::record_fission_observation(
                dir.path(),
                crate::fission_ledger::FissionObservation {
                    parent_session_id: "parent".to_string(),
                    anchor_item_id: "call-1".to_string(),
                    tool: "spawn_agent".to_string(),
                    status: "running".to_string(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    branches: vec![crate::fission_ledger::FissionBranchObservation {
                        session_id: "child".to_string(),
                        status: "running".to_string(),
                        summary: None,
                    }],
                },
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let group_id = crate::fission_ledger::group_id("parent", "call-1");

            let result = server
                .claim_fission_canonical(Parameters(ClaimFissionCanonicalParams {
                    group_id: group_id.clone(),
                    branch_session_id: "child".to_string(),
                    expected_canonical_session_id: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(
                value.pointer("/canonical_session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );

            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/canonical_session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    /// Seed a one-branch fission group via the observation path and return its
    /// group id.
    fn seed_fission_group(
        log_dir: &std::path::Path,
        parent: &str,
        anchor: &str,
        branch: &str,
        status: &str,
    ) -> String {
        crate::fission_ledger::record_fission_observation(
            log_dir,
            crate::fission_ledger::FissionObservation {
                parent_session_id: parent.to_string(),
                anchor_item_id: anchor.to_string(),
                tool: "fission_spawn".to_string(),
                status: status.to_string(),
                prompt: Some("test objective".to_string()),
                model: None,
                reasoning_effort: None,
                branches: vec![crate::fission_ledger::FissionBranchObservation {
                    session_id: branch.to_string(),
                    status: status.to_string(),
                    summary: None,
                }],
            },
        )
        .unwrap();
        crate::fission_ledger::group_id(parent, anchor)
    }

    fn test_fission_group(branch_status: &str) -> crate::fission_ledger::FissionGroup {
        crate::fission_ledger::FissionGroup {
            group_id: "fission-test-group".to_string(),
            parent_session_id: "parent".to_string(),
            anchor_item_id: "call-1".to_string(),
            tool: "fission_spawn".to_string(),
            objective: Some("test objective".to_string()),
            prompt: None,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
            canonical_session_id: None,
            branches: vec![crate::fission_ledger::FissionBranch {
                session_id: "branch-1".to_string(),
                backend_session_id: None,
                status: branch_status.to_string(),
                summary: None,
                task: None,
                model: None,
                reasoning_effort: None,
                worktree_path: None,
                raw_log: "session.jsonl#session_id=branch-1".to_string(),
                ephemeral: false,
                updated_at: "2026-06-10T00:00:00Z".to_string(),
            }],
        }
    }

    #[test]
    fn fission_tools_listed_in_named_profiles_under_managed_context() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            for profile in ["core", "screen", "managed"] {
                let listed = server
                    .list_tools_json_for_session(None, Some(true), Some(profile))
                    .await;
                let names: Vec<_> = listed["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|tool| tool["name"].as_str())
                    .collect();
                for name in [
                    "fission_spawn",
                    "fission_control",
                    "claim_fission_canonical",
                ] {
                    assert!(
                        names.contains(&name),
                        "{name} missing from managed `{profile}` profile listing"
                    );
                }

                let unmanaged = server
                    .list_tools_json_for_session(None, Some(false), Some(profile))
                    .await;
                let unmanaged_names: Vec<_> = unmanaged["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|tool| tool["name"].as_str())
                    .collect();
                for name in [
                    "fission_spawn",
                    "fission_control",
                    "claim_fission_canonical",
                ] {
                    assert!(
                        !unmanaged_names.contains(&name),
                        "{name} must be hidden from unmanaged `{profile}` profile listing"
                    );
                }
            }

            let spawn_description = server
                .list_tools_json_for_session(None, Some(true), Some("core"))
                .await["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "fission_spawn")
                .and_then(|tool| tool["description"].as_str())
                .map(str::to_string)
                .expect("fission_spawn description");
            assert!(spawn_description.contains("full-context sibling branches"));
            assert!(spawn_description.contains("do not see the current turn"));
        });
    }

    #[test]
    fn call_tool_rejects_fission_tools_when_managed_context_is_disabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            for (name, args) in [
                (
                    "fission_spawn",
                    serde_json::json!({ "branches": [{ "objective": "x" }] }),
                ),
                (
                    "fission_control",
                    serde_json::json!({ "group_id": "g", "op": "wait" }),
                ),
                (
                    "claim_fission_canonical",
                    serde_json::json!({ "group_id": "g", "branch_session_id": "b" }),
                ),
            ] {
                let result = server.call_tool_by_name(name, args).await.unwrap();
                assert!(result.is_error.unwrap_or(false), "{name} should be gated");
                let rendered = format!("{result:?}");
                assert!(rendered.contains("managed context is disabled"));
                assert!(rendered.contains("fission_spawn/fission_control/claim_fission_canonical"));
            }
        });
    }

    #[test]
    fn fission_spawn_validates_branch_params() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());

            let no_branches = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: None,
                    branches: vec![],
                    use_worktree: None,
                }))
                .await;
            assert!(no_branches.contains("requires between 1 and 4 branches"));

            let too_many = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: None,
                    branches: (0..5)
                        .map(|idx| FissionBranchSpec {
                            objective: format!("objective {idx}"),
                            write_scope: None,
                            name: None,
                        })
                        .collect(),
                    use_worktree: None,
                }))
                .await;
            assert!(too_many.contains("requires between 1 and 4 branches"));
            assert!(too_many.contains("got 5"));

            let empty_objective = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: None,
                    branches: vec![
                        FissionBranchSpec {
                            objective: "real objective".to_string(),
                            write_scope: None,
                            name: None,
                        },
                        FissionBranchSpec {
                            objective: "   ".to_string(),
                            write_scope: None,
                            name: None,
                        },
                    ],
                    use_worktree: None,
                }))
                .await;
            assert!(empty_objective.contains("branches[1] requires a non-empty"));
        });
    }

    #[test]
    fn fission_spawn_dispatches_thread_action_with_charters() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let capture = spawn_codex_thread_action_capture(
                bus,
                "fission_spawn",
                "fission group fission-p-c spawned: 2 branches",
            );

            let result = server
                .fission_spawn(Parameters(FissionSpawnParams {
                    session_id: Some("parent-session".to_string()),
                    branches: vec![
                        FissionBranchSpec {
                            objective: "  refactor parser  ".to_string(),
                            write_scope: Some(vec!["src/parser.rs".to_string(), " ".to_string()]),
                            name: Some("parser".to_string()),
                        },
                        FissionBranchSpec {
                            objective: "survey docs".to_string(),
                            write_scope: None,
                            name: None,
                        },
                    ],
                    use_worktree: Some(false),
                }))
                .await;
            assert_eq!(result, "fission group fission-p-c spawned: 2 branches");

            let params = capture.await.unwrap();
            let branches = params["branches"].as_array().expect("branches array");
            assert_eq!(branches.len(), 2);
            assert_eq!(branches[0]["objective"], "refactor parser");
            assert_eq!(
                branches[0]["write_scope"],
                serde_json::json!(["src/parser.rs"])
            );
            assert_eq!(branches[0]["name"], "parser");
            assert_eq!(branches[1]["objective"], "survey docs");
            assert!(branches[1].get("write_scope").is_none());
            assert!(branches[1].get("name").is_none());
            assert_eq!(params["use_worktree"], serde_json::Value::Bool(false));
        });
    }

    #[test]
    fn fission_control_validates_params() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());

            let empty_group = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: "  ".to_string(),
                    branch_session_id: None,
                    op: "wait".to_string(),
                    timeout_s: None,
                }))
                .await;
            assert!(empty_group.contains("requires a non-empty group_id"));

            let bad_op = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: "group-1".to_string(),
                    branch_session_id: None,
                    op: "pause".to_string(),
                    timeout_s: None,
                }))
                .await;
            assert!(bad_op.contains("op must be `wait`, `import`, `cancel`, or `detach`"));
            assert!(bad_op.contains("`pause`"));

            for op in ["import", "cancel", "detach"] {
                let missing_branch = server
                    .fission_control(Parameters(FissionControlParams {
                        session_id: None,
                        group_id: "group-1".to_string(),
                        branch_session_id: None,
                        op: op.to_string(),
                        timeout_s: None,
                    }))
                    .await;
                assert!(
                    missing_branch.contains(&format!("op={op} requires branch_session_id")),
                    "op={op} must require branch_session_id, got: {missing_branch}"
                );
            }
        });
    }

    #[test]
    fn fission_wait_timeout_clamping() {
        assert_eq!(clamp_fission_wait_timeout_s(None), 60);
        assert_eq!(clamp_fission_wait_timeout_s(Some(0)), 5);
        assert_eq!(clamp_fission_wait_timeout_s(Some(4)), 5);
        assert_eq!(clamp_fission_wait_timeout_s(Some(5)), 5);
        assert_eq!(clamp_fission_wait_timeout_s(Some(120)), 120);
        assert_eq!(clamp_fission_wait_timeout_s(Some(300)), 300);
        assert_eq!(clamp_fission_wait_timeout_s(Some(100_000)), 300);
    }

    #[test]
    fn render_fission_wait_outcome_variants() {
        use crate::fission_lifecycle::WaitOutcome;

        let terminal = render_fission_wait_outcome(
            WaitOutcome::Terminal(test_fission_group("completed")),
            "fission-test-group",
            Some("branch-1"),
            60,
        );
        let value: serde_json::Value = serde_json::from_str(&terminal).unwrap();
        assert_eq!(value["outcome"], "terminal");
        assert_eq!(value["group"]["group_id"], "fission-test-group");
        assert_eq!(value["group"]["branches"][0]["status"], "completed");

        // still_running is a NORMAL result: valid JSON snapshot, not an error
        // string.
        let still_running = render_fission_wait_outcome(
            WaitOutcome::StillRunning(test_fission_group("running")),
            "fission-test-group",
            None,
            42,
        );
        assert!(!still_running.starts_with("fission_control wait failed"));
        let value: serde_json::Value = serde_json::from_str(&still_running).unwrap();
        assert_eq!(value["outcome"], "still_running");
        assert_eq!(value["watched"], "any branch");
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("42s"));
        assert!(message.contains("normal result"));

        let detached = render_fission_wait_outcome(
            WaitOutcome::Detached(test_fission_group("detached")),
            "fission-test-group",
            Some("branch-1"),
            60,
        );
        let value: serde_json::Value = serde_json::from_str(&detached).unwrap();
        assert_eq!(value["outcome"], "detached");
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("rewind_backout"));
        assert!(message.contains("raw_log"));

        let missing_group =
            render_fission_wait_outcome(WaitOutcome::GroupNotFound, "fission-missing", None, 60);
        assert!(missing_group.starts_with("fission_control wait failed"));
        assert!(missing_group.contains("`fission-missing` was not found"));

        let missing_branch = render_fission_wait_outcome(
            WaitOutcome::BranchNotFound(test_fission_group("running")),
            "fission-test-group",
            Some("branch-9"),
            60,
        );
        assert!(missing_branch.starts_with("fission_control wait failed"));
        assert!(missing_branch.contains("`branch-9` is not part of fission group"));
        assert!(missing_branch.contains("branch-1"));
    }

    #[test]
    fn fission_control_wait_renders_ledger_outcomes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-wait-parent",
                "call-1",
                "fsb-wait-child",
                "completed",
            );
            let detached_group_id = seed_fission_group(
                dir.path(),
                "fsb-wait-parent",
                "call-2",
                "fsb-wait-child-2",
                "running",
            );
            crate::fission_ledger::detach_group(dir.path(), &detached_group_id, "test detach")
                .unwrap();

            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());

            let terminal = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-wait-child".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&terminal).unwrap();
            assert_eq!(value["outcome"], "terminal");
            assert_eq!(value["group"]["group_id"], serde_json::json!(group_id));

            let detached = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: detached_group_id.clone(),
                    branch_session_id: None,
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&detached).unwrap();
            assert_eq!(value["outcome"], "detached");

            let missing_group = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: "fission-does-not-exist".to_string(),
                    branch_session_id: None,
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            assert!(missing_group.contains("was not found in any candidate ledger"));

            let missing_branch = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-no-such-branch".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            assert!(missing_branch.contains("is not part of fission group"));
        });
    }

    #[test]
    fn fission_control_wait_resolves_log_dir_via_branch_route() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ledger_dir = tempdir().unwrap();
            let other_dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                ledger_dir.path(),
                "fsb-route-parent",
                "call-1",
                "fsb-route-child",
                "completed",
            );
            // The ledger lives in a dir that is NOT the server's primary log
            // dir; only the in-process branch route knows where it is.
            crate::fission_lifecycle::register_branch(
                "fsb-route-child",
                &group_id,
                ledger_dir.path(),
            );

            let state = test_state_with_log_dir(other_dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-route-child".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["outcome"], "terminal");

            crate::fission_lifecycle::drop_pending_deliveries(&[group_id]);
        });
    }

    #[test]
    fn fission_control_import_dispatches_thread_action() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let capture = spawn_codex_thread_action_capture(
                bus,
                "fission_import",
                "branch outcome injected into parent context and marked imported",
            );

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: Some("parent-session".to_string()),
                    group_id: "fission-group-7".to_string(),
                    branch_session_id: Some("branch-7".to_string()),
                    op: "import".to_string(),
                    timeout_s: None,
                }))
                .await;
            assert_eq!(
                result,
                "branch outcome injected into parent context and marked imported"
            );

            let params = capture.await.unwrap();
            assert_eq!(
                params,
                serde_json::json!({
                    "group_id": "fission-group-7",
                    "branch_session_id": "branch-7",
                })
            );
        });
    }

    #[test]
    fn fission_control_cancel_stops_branch_and_marks_ledger() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-cancel-parent",
                "call-1",
                "fsb-cancel-child",
                "running",
            );
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-cancel-child".to_string()),
                    op: "cancel".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["op"], "cancel");
            assert!(value["stop"]
                .as_str()
                .unwrap()
                .contains("stop requested for branch session `fsb-cancel-child`"));
            assert_eq!(value["ledger"], "branch marked cancelled");
            assert_eq!(value["group"]["branches"][0]["status"], "cancelled");

            // The stop intent is the same ControlMsg the dashboard stop path
            // sends.
            let mut saw_stop = false;
            while let Ok(Ok(event)) = timeout(Duration::from_secs(1), rx.recv()).await {
                if let AppEvent::ControlCommand(ControlMsg::StopSession { session_id }) = event {
                    assert_eq!(session_id, "fsb-cancel-child");
                    saw_stop = true;
                    break;
                }
            }
            assert!(saw_stop, "expected ControlMsg::StopSession on the bus");

            // Persisted: the branch carries the sticky cancelled status.
            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            let branch_status = document
                .groups
                .iter()
                .find(|group| group.group_id == group_id)
                .and_then(|group| group.branches.first())
                .map(|branch| branch.status.clone())
                .unwrap();
            assert_eq!(branch_status, "cancelled");

            // Cancelling again reports the sticky status instead of stomping
            // the ledger.
            let again = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-cancel-child".to_string()),
                    op: "cancel".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&again).unwrap();
            assert!(value["ledger"]
                .as_str()
                .unwrap()
                .contains("already has terminal status `cancelled`"));
        });
    }

    #[test]
    fn fission_control_cancel_leaves_completed_branch_untouched() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-cancel-done-parent",
                "call-1",
                "fsb-cancel-done-child",
                "completed",
            );
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-cancel-done-child".to_string()),
                    op: "cancel".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert!(value["ledger"]
                .as_str()
                .unwrap()
                .contains("already has terminal status `completed`"));

            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(
                document
                    .groups
                    .iter()
                    .find(|group| group.group_id == group_id)
                    .and_then(|group| group.branches.first())
                    .map(|branch| branch.status.as_str()),
                Some("completed"),
                "a completed branch's recorded result must not be stomped by cancel"
            );
        });
    }

    #[test]
    fn fission_control_detach_severs_group_and_emits_relationship() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-detach-parent",
                "call-1",
                "fsb-detach-child",
                "running",
            );
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-detach-child".to_string()),
                    op: "detach".to_string(),
                    timeout_s: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(value["op"], "detach");
            assert_eq!(value["group"]["branches"][0]["status"], "detached");
            assert!(value["message"]
                .as_str()
                .unwrap()
                .contains("cannot be waited on or imported"));

            let mut saw_relationship = false;
            while let Ok(Ok(event)) = timeout(Duration::from_secs(1), rx.recv()).await {
                if let AppEvent::SessionRelationship {
                    parent_session_id,
                    child_session_id,
                    relationship,
                    ephemeral,
                } = event
                {
                    assert_eq!(parent_session_id, "fsb-detach-parent");
                    assert_eq!(child_session_id, "fsb-detach-child");
                    assert_eq!(relationship, "fission-detached");
                    assert!(!ephemeral);
                    saw_relationship = true;
                    break;
                }
            }
            assert!(saw_relationship, "expected fission-detached relationship");

            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            assert!(document.group_is_detached(&group_id));

            // The sticky detach refuses later waits.
            let wait = server
                .fission_control(Parameters(FissionControlParams {
                    session_id: None,
                    group_id: group_id.clone(),
                    branch_session_id: Some("fsb-detach-child".to_string()),
                    op: "wait".to_string(),
                    timeout_s: Some(5),
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&wait).unwrap();
            assert_eq!(value["outcome"], "detached");
        });
    }

    #[test]
    fn claim_fission_canonical_refuses_detached_group() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let group_id = seed_fission_group(
                dir.path(),
                "fsb-claim-parent",
                "call-1",
                "fsb-claim-child",
                "completed",
            );
            crate::fission_ledger::detach_group(dir.path(), &group_id, "rewind crossed anchor")
                .unwrap();

            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .claim_fission_canonical(Parameters(ClaimFissionCanonicalParams {
                    group_id: group_id.clone(),
                    branch_session_id: "fsb-claim-child".to_string(),
                    expected_canonical_session_id: None,
                }))
                .await;
            assert!(result.starts_with("claim_fission_canonical failed"));
            assert!(result.contains("cannot be claimed at a detached anchor"));

            // The refused claim must not have been persisted.
            let document = crate::fission_ledger::read_fission_ledger_document(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(
                document
                    .groups
                    .iter()
                    .find(|group| group.group_id == group_id)
                    .and_then(|group| group.canonical_session_id.as_deref()),
                None
            );
        });
    }

    #[tokio::test]
    async fn schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("configure at least one restart action"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_failed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["execution_error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn schedule_restart_rejects_invalid_restart_after() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("later".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[tokio::test]
    async fn schedule_restart_rejects_empty_restart_command() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("   ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("Invalid request: restart_command must not be empty")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_when_active_with_json_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let first = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let first_json: serde_json::Value = serde_json::from_str(&first).unwrap();
        let restart_id = first_json["restart_id"].as_str().unwrap().to_string();

        let second = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop again".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("A restart is already active"));
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: Some("ok".to_string()),
                handoff_summary: Some("handoff".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("completed"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn get_restart_status_redacts_turn_complete_token() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let status = server.get_restart_status().await;
        let status_json: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(
            status_json["turn_complete_token"].as_str(),
            Some("[redacted]")
        );
        assert_ne!(
            status_json["turn_complete_token"].as_str(),
            Some(token.as_str())
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: "wrong".to_string(),
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(
            json["error"].as_str(),
            Some("turn_complete_token is invalid")
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_normalizes_ids_and_optional_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: format!("  {}  ", restart_id),
                turn_complete_token: format!("  {}  ", token),
                status: Some("   ".to_string()),
                handoff_summary: Some("  handoff summary  ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert!(restart.completion_status.is_none());
        assert_eq!(restart.handoff_summary.as_deref(), Some("handoff summary"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some(restart_id.clone()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("cancelled"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("cancelled"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("abc".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("No controller restart is scheduled")
        );
        assert_eq!(json["restart_id"].as_str(), Some("abc"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_treats_whitespace_guard_as_none() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("   ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
    }
}
