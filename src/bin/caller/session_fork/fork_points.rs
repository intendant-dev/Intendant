//! Fork-point catalog builders for the codex and native backends.
//!
//! Codex derives from the same rollout scan the managed-context rewind
//! machinery uses (`shared_context_rewind_anchor_scan` — cached, racy-write
//! quiescent) joined with the effective user-turn list; native derives from
//! the persisted `conversation.jsonl` without loading a full
//! `Conversation`. Both are read-only over the parent artifacts.

use super::*;
use crate::managed_context_ops::{
    rollout_user_turns, shared_context_rewind_anchor_scan, ContextRewindAnchorCatalogEntry,
    RolloutUserTurn,
};
use std::io;
use std::path::Path;

/// Fork points for a codex session, derived from its rollout file.
pub(crate) fn codex_fork_points(
    session_id: &str,
    backend_session_id: &str,
    rollout_path: &Path,
    query: &ForkPointQuery,
) -> io::Result<ForkPointCatalog> {
    let scan = shared_context_rewind_anchor_scan(rollout_path)?;
    let turns = rollout_user_turns(rollout_path)?;
    Ok(codex_fork_points_from_parts(
        session_id,
        backend_session_id,
        &scan.catalog,
        &turns,
        query,
    ))
}

/// Pure assembly over an anchor catalog + effective user-turn list
/// (unit-tested without a rollout file).
pub(crate) fn codex_fork_points_from_parts(
    session_id: &str,
    backend_session_id: &str,
    anchors: &[ContextRewindAnchorCatalogEntry],
    turns: &[RolloutUserTurn],
    query: &ForkPointQuery,
) -> ForkPointCatalog {
    // Sort key: file position, descending (newest history first). A
    // turn boundary "after turn i" lives where turn i+1 begins (the last
    // boundary sorts above everything).
    let mut keyed: Vec<(usize, ForkPoint)> = Vec::new();

    for (i, turn) in turns.iter().enumerate() {
        let ordinal = turn.index;
        let sort_key = turns
            .get(i + 1)
            .map(|next| next.line.saturating_sub(1))
            .unwrap_or(usize::MAX);
        keyed.push((
            sort_key,
            ForkPoint {
                id: format!("turn:{ordinal}"),
                kind: "turn-boundary",
                granularity: "turn",
                turn: Some(ordinal),
                seq: None,
                item_id: None,
                position: None,
                preview: fork_point_preview(&turn.text),
                eligible: true,
                eligibility_reasons: Vec::new(),
                effective_cut: None,
            },
        ));
    }

    for entry in anchors {
        let recovery_eligible = entry.recovery_eligible;
        if recovery_eligible == Some(false) && !query.include_non_recovery {
            continue;
        }
        // The whole turn the anchor's first line falls inside; a vanilla
        // fork rounds down to the boundary before that turn.
        let containing_turn = turns
            .iter()
            .take_while(|turn| turn.line <= entry.first_line)
            .last()
            .map(|turn| turn.index)
            .unwrap_or(0);
        let eligible = recovery_eligible.unwrap_or(true);
        let mut reasons = Vec::new();
        if recovery_eligible == Some(false) {
            reasons.push(
                "not recovery-eligible for in-place rewind (forking is still possible)".to_string(),
            );
        }
        keyed.push((
            entry.first_line,
            ForkPoint {
                id: format!("item:{}:{}", entry.item_id, entry.position_hint),
                kind: "item-anchor",
                granularity: "item",
                turn: Some(containing_turn),
                seq: None,
                item_id: Some(entry.item_id.clone()),
                position: Some(entry.position_hint),
                preview: fork_point_preview(&entry.summary),
                eligible,
                eligibility_reasons: reasons,
                effective_cut: Some(format!("turn:{}", containing_turn.saturating_sub(1))),
            },
        ));
    }

    // Descending file order; boundaries above item anchors at the same
    // position (the coarser, always-available choice lists first).
    keyed.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            let rank = |p: &ForkPoint| usize::from(p.kind != "turn-boundary");
            rank(&a.1).cmp(&rank(&b.1))
        })
    });
    let points: Vec<ForkPoint> = keyed.into_iter().map(|(_, point)| point).collect();

    let mut catalog = ForkPointCatalog {
        session_id: session_id.to_string(),
        source: "codex".to_string(),
        backend_session_id: Some(backend_session_id.to_string()),
        supported: true,
        unsupported_reason: None,
        notes: vec![
            "item anchors cut exactly on the managed codex binary; on the vanilla binary a fork rounds down to the annotated effective_cut turn boundary".to_string(),
            "turn-boundary points fork on any binary".to_string(),
        ],
        total: 0,
        offset: 0,
        limit: 0,
        next_offset: None,
        fork_points: Vec::new(),
    };
    page_fork_points(&mut catalog, points, query);
    catalog
}

/// Fork points for a native (intendant) session, derived from the
/// persisted `conversation.jsonl` in its log dir.
pub(crate) fn native_fork_points(
    session_id: &str,
    log_dir: &Path,
    query: &ForkPointQuery,
) -> io::Result<ForkPointCatalog> {
    let conversation_path = log_dir.join("conversation.jsonl");
    let mut catalog = ForkPointCatalog {
        session_id: session_id.to_string(),
        source: "intendant".to_string(),
        backend_session_id: None,
        supported: true,
        unsupported_reason: None,
        notes: vec![
            "round boundaries of the last persisted conversation state; a live session's unsaved tail is not reflected".to_string(),
        ],
        total: 0,
        offset: 0,
        limit: 0,
        next_offset: None,
        fork_points: Vec::new(),
    };
    if !conversation_path.exists() {
        catalog.supported = false;
        catalog.unsupported_reason =
            Some("no persisted conversation.jsonl in this session's log dir".to_string());
        return Ok(catalog);
    }

    // Minimal per-line view; the full Message struct is not needed here.
    struct Line {
        role: String,
        seq: u64,
        preview: String,
    }
    let raw = std::fs::read_to_string(&conversation_path)?;
    let mut lines: Vec<Line> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        lines.push(Line {
            role: value
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            seq: value
                .get("seq")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            preview: fork_point_preview(
                value
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(""),
            ),
        });
    }

    let user_ordinals: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.role == "user")
        .map(|(i, _)| i)
        .collect();
    let round_count = user_ordinals.len() as u32;
    let mut points: Vec<ForkPoint> = Vec::new();

    if let Some(last) = lines.last() {
        let eligible = last.seq > 0;
        points.push(ForkPoint {
            id: "head".to_string(),
            kind: "head",
            granularity: "round",
            turn: Some(round_count),
            seq: Some(last.seq),
            item_id: None,
            position: None,
            preview: format!("{}: {}", last.role, last.preview),
            eligible,
            eligibility_reasons: if eligible {
                Vec::new()
            } else {
                vec!["legacy message without a seq ordinal".to_string()]
            },
            effective_cut: None,
        });
    }

    // "Before round r" = keep everything up to the message preceding the
    // r-th user message; latest divergence first. Round 1 is skipped (a
    // fork keeping nothing has no value).
    for (round, &msg_index) in user_ordinals.iter().enumerate().skip(1).rev() {
        let round = round as u32 + 1;
        let prev = &lines[msg_index - 1];
        let eligible = prev.seq > 0;
        points.push(ForkPoint {
            id: format!("seq:{}", prev.seq),
            kind: "round",
            granularity: "round",
            turn: Some(round - 1),
            seq: Some(prev.seq),
            item_id: None,
            position: None,
            preview: lines[msg_index].preview.clone(),
            eligible,
            eligibility_reasons: if eligible {
                Vec::new()
            } else {
                vec!["legacy message without a seq ordinal".to_string()]
            },
            effective_cut: None,
        });
    }

    page_fork_points(&mut catalog, points, query);
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_anchor_entry(
        item_id: &str,
        first_line: usize,
        recovery_eligible: Option<bool>,
    ) -> ContextRewindAnchorCatalogEntry {
        ContextRewindAnchorCatalogEntry {
            ordinal: 0,
            item_id: item_id.to_string(),
            first_line,
            last_line: first_line,
            first_item_type: "function_call".to_string(),
            last_item_type: "function_call_output".to_string(),
            last_item_is_model: false,
            positions: vec!["after"],
            position_hint: "after",
            names: Vec::new(),
            roles: Vec::new(),
            summary: format!("tool call {item_id}"),
            backend_usage_at_or_after_anchor: None,
            backend_usage_before_anchor: None,
            rewind_only_limit_at_or_after_anchor: None,
            recommended_rewind_limit_at_or_after_anchor: None,
            prefix_estimated_tokens_before_anchor: None,
            prefix_estimated_tokens_after_anchor: None,
            approx_pruned_tokens_before: None,
            approx_pruned_tokens_after: None,
            prefix_tokens_after: None,
            latest_rewind_usage_after_anchor: None,
            latest_rewind_limit_after_anchor: None,
            recovery_eligible,
            recovery_eligible_positions: None,
            density_eligible: None,
            density_eligible_positions: None,
            managed_context_recovery_start_line: None,
        }
    }

    fn test_turns(lines: &[(u32, usize, &str)]) -> Vec<RolloutUserTurn> {
        lines
            .iter()
            .map(|(index, line, text)| RolloutUserTurn {
                index: *index,
                line: *line,
                text: (*text).to_string(),
            })
            .collect()
    }

    #[test]
    fn preview_collapses_whitespace_and_caps() {
        assert_eq!(fork_point_preview("  a\n\n  b\tc "), "a b c");
        let long = "x".repeat(400);
        assert!(fork_point_preview(&long).len() <= 150);
    }

    #[test]
    fn codex_points_merge_boundaries_and_anchors_newest_first() {
        let turns = test_turns(&[(1, 2, "first task"), (2, 10, "second task")]);
        let anchors = vec![
            test_anchor_entry("item_a", 12, Some(true)),
            test_anchor_entry("item_b", 5, Some(false)),
        ];
        let catalog = codex_fork_points_from_parts(
            "wrapper",
            "backend-id",
            &anchors,
            &turns,
            &ForkPointQuery::default(),
        );
        assert!(catalog.supported);
        let ids: Vec<&str> = catalog
            .fork_points
            .iter()
            .map(|point| point.id.as_str())
            .collect();
        // item_b is recovery-ineligible and hidden by default.
        assert_eq!(ids, vec!["turn:2", "item:item_a:after", "turn:1"]);
        let anchor = &catalog.fork_points[1];
        assert_eq!(anchor.turn, Some(2));
        assert_eq!(anchor.effective_cut.as_deref(), Some("turn:1"));
        assert!(anchor.eligible);
    }

    #[test]
    fn codex_include_non_recovery_lists_ineligible_anchors() {
        let turns = test_turns(&[(1, 2, "task")]);
        let anchors = vec![test_anchor_entry("item_b", 5, Some(false))];
        let query = ForkPointQuery {
            include_non_recovery: true,
            ..ForkPointQuery::default()
        };
        let catalog =
            codex_fork_points_from_parts("wrapper", "backend-id", &anchors, &turns, &query);
        let anchor = catalog
            .fork_points
            .iter()
            .find(|point| point.kind == "item-anchor")
            .expect("anchor listed");
        assert!(!anchor.eligible);
        assert!(!anchor.eligibility_reasons.is_empty());
    }

    #[test]
    fn codex_anchor_before_first_turn_rounds_to_empty() {
        let turns = test_turns(&[(1, 10, "task")]);
        let anchors = vec![test_anchor_entry("item_pre", 3, Some(true))];
        let catalog = codex_fork_points_from_parts(
            "wrapper",
            "backend-id",
            &anchors,
            &turns,
            &ForkPointQuery::default(),
        );
        let anchor = catalog
            .fork_points
            .iter()
            .find(|point| point.kind == "item-anchor")
            .expect("anchor listed");
        assert_eq!(anchor.turn, Some(0));
        assert_eq!(anchor.effective_cut.as_deref(), Some("turn:0"));
    }

    #[test]
    fn codex_end_to_end_over_real_rollout_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rollout = dir.path().join("rollout-test.jsonl");
        let lines = [
            serde_json::json!({"timestamp":"t","type":"session_meta","payload":{"id":"0000","cwd":"/tmp"}}),
            serde_json::json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"first task"}}),
            serde_json::json!({"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
            serde_json::json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"second task"}}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&rollout, body).expect("write rollout");

        let catalog = codex_fork_points(
            "wrapper",
            "backend-id",
            &rollout,
            &ForkPointQuery::default(),
        )
        .expect("catalog");
        let boundary_turns: Vec<u32> = catalog
            .fork_points
            .iter()
            .filter(|point| point.kind == "turn-boundary")
            .filter_map(|point| point.turn)
            .collect();
        assert_eq!(boundary_turns, vec![2, 1]);
    }

    fn write_native_conversation(dir: &Path, messages: &[(&str, u64, &str)]) {
        let body: String = messages
            .iter()
            .map(|(role, seq, content)| {
                format!(
                    "{}\n",
                    serde_json::json!({"role": role, "content": content, "seq": seq})
                )
            })
            .collect();
        std::fs::write(dir.join("conversation.jsonl"), body).expect("write conversation");
    }

    #[test]
    fn native_points_are_round_boundaries_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_native_conversation(
            dir.path(),
            &[
                ("user", 1, "round one"),
                ("assistant", 2, "answer one"),
                ("user", 3, "round two"),
                ("assistant", 4, "answer two"),
                ("user", 5, "round three"),
            ],
        );
        let catalog = native_fork_points("native-id", dir.path(), &ForkPointQuery::default())
            .expect("catalog");
        assert!(catalog.supported);
        let ids: Vec<&str> = catalog
            .fork_points
            .iter()
            .map(|point| point.id.as_str())
            .collect();
        assert_eq!(ids, vec!["head", "seq:4", "seq:2"]);
        assert_eq!(catalog.fork_points[0].turn, Some(3));
        assert_eq!(catalog.fork_points[1].preview, "round three");
        assert_eq!(catalog.fork_points[2].turn, Some(1));
        assert!(catalog.fork_points.iter().all(|point| point.eligible));
    }

    #[test]
    fn native_legacy_zero_seq_is_ineligible() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_native_conversation(
            dir.path(),
            &[
                ("user", 0, "legacy round"),
                ("assistant", 0, "legacy answer"),
                ("user", 7, "new round"),
            ],
        );
        let catalog = native_fork_points("native-id", dir.path(), &ForkPointQuery::default())
            .expect("catalog");
        let before_round_two = catalog
            .fork_points
            .iter()
            .find(|point| point.kind == "round")
            .expect("round point");
        assert!(!before_round_two.eligible);
        assert!(!before_round_two.eligibility_reasons.is_empty());
    }

    #[test]
    fn native_missing_conversation_reports_unsupported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = native_fork_points("native-id", dir.path(), &ForkPointQuery::default())
            .expect("catalog");
        assert!(!catalog.supported);
        assert!(catalog.unsupported_reason.is_some());
    }

    #[test]
    fn paging_windows_and_reports_next_offset() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_native_conversation(
            dir.path(),
            &[
                ("user", 1, "r1"),
                ("user", 2, "r2"),
                ("user", 3, "r3"),
                ("user", 4, "r4"),
            ],
        );
        let query = ForkPointQuery {
            include_non_recovery: false,
            offset: 1,
            limit: 2,
        };
        let catalog = native_fork_points("native-id", dir.path(), &query).expect("catalog");
        assert_eq!(catalog.total, 4);
        assert_eq!(catalog.fork_points.len(), 2);
        assert_eq!(catalog.next_offset, Some(3));
    }
}
