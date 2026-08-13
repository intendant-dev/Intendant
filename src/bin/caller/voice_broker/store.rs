//! D1 permanence: the durable presence-thread record (id + successor
//! lineage) and the persisted presence checkpoint that seeds every
//! realtime session.
//!
//! Permanence is checkpoint-authoritative: the durable thread carries
//! conversational identity across daemon restarts (resumed each
//! app-server boot; a successor is minted with lineage recorded when
//! resume fails), but the seedable memory is always the checkpoint —
//! realtime content does not materialize as thread items, so the
//! checkpoint, not the thread history, is what survives.
//!
//! Every function takes its root as a parameter; only the broker's
//! construction site resolves the real state root (tests inject
//! tempdirs).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Budget for the `initialItems` seed: deliberate headroom under the
/// provider's 8,192-estimated-token / 128-item caps, leaving room for
/// the live state block.
pub(crate) const SEED_TOKEN_BUDGET_EST: usize = 6_000;
pub(crate) const SEED_ITEM_BUDGET: usize = 120;
/// Rolling-summary target within the seed budget.
pub(crate) const CHECKPOINT_SUMMARY_TOKEN_BUDGET_EST: usize = 2_500;
/// Verbatim recent turns kept in the checkpoint.
pub(crate) const CHECKPOINT_RECENT_TURNS: usize = 12;

fn presence_dir(state_root: &Path) -> PathBuf {
    state_root.join("presence")
}

pub(crate) fn thread_store_path(state_root: &Path) -> PathBuf {
    presence_dir(state_root).join("voice_thread.json")
}

pub(crate) fn checkpoint_store_path(state_root: &Path) -> PathBuf {
    presence_dir(state_root).join("voice_checkpoint.json")
}

pub(crate) fn audit_log_path(state_root: &Path) -> PathBuf {
    presence_dir(state_root).join("voice_authority_audit.jsonl")
}

pub(crate) fn neutral_cwd_path(state_root: &Path) -> PathBuf {
    presence_dir(state_root).join("neutral-cwd")
}

/// Cap on retained lineage entries (freshest kept). The default
/// tool-lane policy retires one predecessor per call, so an unbounded
/// trail would grow forever on a chatty daemon.
pub(crate) const MAX_LINEAGE_ENTRIES: usize = 64;

/// A retired predecessor in the presence-thread lineage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThreadLineageEntry {
    pub(crate) thread_id: String,
    pub(crate) retired_epoch: u64,
    pub(crate) reason: String,
}

/// The durable presence-thread record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct VoiceThreadRecord {
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) created_epoch: Option<u64>,
    #[serde(default)]
    pub(crate) lineage: Vec<ThreadLineageEntry>,
    /// Transient: why the next `adopt` retires the predecessor
    /// (set when a resume fails, consumed by the successor mint).
    #[serde(skip)]
    pub(crate) pending_retire_reason: Option<String>,
}

impl VoiceThreadRecord {
    pub(crate) fn load(state_root: &Path) -> Self {
        let path = thread_store_path(state_root);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub(crate) fn save(&self, state_root: &Path) -> Result<(), String> {
        let dir = presence_dir(state_root);
        intendant_core::state_paths::create_private_dir_all(&dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        intendant_core::state_paths::write_private_file(
            &thread_store_path(state_root),
            raw.as_bytes(),
        )
        .map_err(|e| format!("write voice thread record: {e}"))
    }

    /// Adopt a freshly minted thread. When a predecessor existed, it
    /// retires into the lineage with the reason it was retired (resume
    /// failed, or the default tool-lane re-declaration policy). The
    /// lineage is a bounded provenance trail, not an audit log (that is
    /// the authority-audit JSONL): the default policy retires one
    /// predecessor per call, so the trail keeps only the freshest
    /// [`MAX_LINEAGE_ENTRIES`].
    pub(crate) fn adopt(&mut self, new_thread_id: &str, now_epoch: u64, retire_reason: &str) {
        if let Some(prior) = self.thread_id.take() {
            if prior != new_thread_id {
                self.lineage.push(ThreadLineageEntry {
                    thread_id: prior,
                    retired_epoch: now_epoch,
                    reason: retire_reason.to_string(),
                });
                let excess = self.lineage.len().saturating_sub(MAX_LINEAGE_ENTRIES);
                if excess > 0 {
                    self.lineage.drain(..excess);
                }
            }
        }
        self.thread_id = Some(new_thread_id.to_string());
        if self.created_epoch.is_none() {
            self.created_epoch = Some(now_epoch);
        }
    }

    /// Owner purge (`thread/delete` succeeded): drop the identity and
    /// the whole lineage — the next call mints a fresh thread.
    pub(crate) fn purge(&mut self) {
        self.thread_id = None;
        self.created_epoch = None;
        self.lineage.clear();
    }
}

/// One verbatim conversational turn kept in the checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CheckpointTurn {
    /// "user" | "assistant" (the provider's transcript roles).
    pub(crate) role: String,
    pub(crate) text: String,
}

/// The persisted presence checkpoint — the authoritative seed memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct VoiceCheckpoint {
    /// Rolling summary (target ≤ ~2,500 est tokens; truncated on save).
    #[serde(default)]
    pub(crate) summary: String,
    /// Last few turns verbatim, oldest first.
    #[serde(default)]
    pub(crate) recent_turns: Vec<CheckpointTurn>,
    #[serde(default)]
    pub(crate) updated_epoch: Option<u64>,
}

/// Cheap token estimate (chars/4, minimum 1 for non-empty) — the same
/// coarse arithmetic the provider documents for its initialItems cap.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        (text.chars().count() / 4).max(1)
    }
}

impl VoiceCheckpoint {
    pub(crate) fn load(state_root: &Path) -> Self {
        let path = checkpoint_store_path(state_root);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub(crate) fn save(&self, state_root: &Path) -> Result<(), String> {
        let dir = presence_dir(state_root);
        intendant_core::state_paths::create_private_dir_all(&dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
        let raw = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        intendant_core::state_paths::write_private_file(
            &checkpoint_store_path(state_root),
            raw.as_bytes(),
        )
        .map_err(|e| format!("write voice checkpoint: {e}"))
    }

    /// Fold a finished call's transcript into the checkpoint: keep the
    /// existing summary (a richer summarizer is a later pass — the
    /// browser presence checkpoint summary also feeds in via the
    /// broker), append the new turns, keep the last
    /// [`CHECKPOINT_RECENT_TURNS`], and bound the summary.
    pub(crate) fn fold_call(
        &mut self,
        call_turns: &[CheckpointTurn],
        presence_summary: Option<&str>,
        now_epoch: u64,
    ) {
        if let Some(summary) = presence_summary {
            let trimmed = summary.trim();
            if !trimmed.is_empty() {
                self.summary = trimmed.to_string();
            }
        }
        while estimate_tokens(&self.summary) > CHECKPOINT_SUMMARY_TOKEN_BUDGET_EST {
            // Truncate from the front — the tail is the freshest.
            let drop = self.summary.chars().count() / 4;
            self.summary = self.summary.chars().skip(drop.max(64)).collect();
        }
        self.recent_turns.extend_from_slice(call_turns);
        let excess = self
            .recent_turns
            .len()
            .saturating_sub(CHECKPOINT_RECENT_TURNS);
        if excess > 0 {
            self.recent_turns.drain(..excess);
        }
        self.updated_epoch = Some(now_epoch);
    }
}

/// Build the realtime `initialItems` seed: identity/state block first
/// (developer role), then the checkpoint summary (developer), then the
/// recent turns verbatim (their own roles) — budgeted to
/// [`SEED_TOKEN_BUDGET_EST`] / [`SEED_ITEM_BUDGET`] with oldest turns
/// dropped first.
pub(crate) fn build_seed_items(
    checkpoint: &VoiceCheckpoint,
    state_block: &str,
) -> Vec<serde_json::Value> {
    let mut head: Vec<(String, String)> = Vec::new();
    if !state_block.trim().is_empty() {
        head.push(("developer".to_string(), state_block.trim().to_string()));
    }
    if !checkpoint.summary.trim().is_empty() {
        head.push((
            "developer".to_string(),
            format!(
                "Presence checkpoint (rolling summary of prior sessions):\n{}",
                checkpoint.summary.trim()
            ),
        ));
    }
    let head_tokens: usize = head.iter().map(|(_, t)| estimate_tokens(t)).sum();
    let mut budget = SEED_TOKEN_BUDGET_EST.saturating_sub(head_tokens);
    let mut item_budget = SEED_ITEM_BUDGET.saturating_sub(head.len());

    // Take turns newest-first until the budget is spent, then restore
    // chronological order.
    let mut tail: Vec<(String, String)> = Vec::new();
    for turn in checkpoint.recent_turns.iter().rev() {
        let cost = estimate_tokens(&turn.text);
        if cost > budget || item_budget == 0 {
            break;
        }
        budget -= cost;
        item_budget -= 1;
        tail.push((turn.role.clone(), turn.text.clone()));
    }
    tail.reverse();

    head.into_iter()
        .chain(tail)
        .map(|(role, text)| serde_json::json!({ "role": role, "text": text }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_record_roundtrip_and_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut rec = VoiceThreadRecord::load(root);
        assert!(rec.thread_id.is_none());
        rec.adopt("t-1", 100, "initial");
        rec.save(root).unwrap();
        let mut rec = VoiceThreadRecord::load(root);
        assert_eq!(rec.thread_id.as_deref(), Some("t-1"));
        assert!(rec.lineage.is_empty(), "first adoption retires nothing");
        // Resume failure → successor with lineage (D1).
        rec.adopt("t-2", 200, "resume-failed");
        rec.save(root).unwrap();
        let rec = VoiceThreadRecord::load(root);
        assert_eq!(rec.thread_id.as_deref(), Some("t-2"));
        assert_eq!(rec.lineage.len(), 1);
        assert_eq!(rec.lineage[0].thread_id, "t-1");
        assert_eq!(rec.lineage[0].reason, "resume-failed");
    }

    #[test]
    fn lineage_is_bounded_to_the_freshest_entries() {
        let mut rec = VoiceThreadRecord::default();
        rec.adopt("t-0", 0, "initial");
        for i in 1..=(MAX_LINEAGE_ENTRIES + 10) {
            rec.adopt(&format!("t-{i}"), i as u64, "tool-lane-redeclare");
        }
        assert_eq!(rec.lineage.len(), MAX_LINEAGE_ENTRIES);
        // Freshest retirements survive; the oldest were drained.
        assert_eq!(
            rec.lineage.last().unwrap().thread_id,
            format!("t-{}", MAX_LINEAGE_ENTRIES + 9)
        );
        assert_eq!(rec.lineage.first().unwrap().thread_id, "t-10");
    }

    #[test]
    fn purge_drops_identity_and_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut rec = VoiceThreadRecord::default();
        rec.adopt("t-1", 100, "initial");
        rec.adopt("t-2", 200, "resume-failed");
        rec.purge();
        rec.save(root).unwrap();
        let rec = VoiceThreadRecord::load(root);
        assert!(rec.thread_id.is_none());
        assert!(rec.lineage.is_empty());
    }

    #[test]
    fn checkpoint_fold_keeps_recent_turns_bounded() {
        let mut cp = VoiceCheckpoint::default();
        let turns: Vec<CheckpointTurn> = (0..20)
            .map(|i| CheckpointTurn {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                text: format!("turn {i}"),
            })
            .collect();
        cp.fold_call(&turns, Some("summary line"), 42);
        assert_eq!(cp.recent_turns.len(), CHECKPOINT_RECENT_TURNS);
        assert_eq!(cp.recent_turns.last().unwrap().text, "turn 19");
        assert_eq!(cp.summary, "summary line");
        assert_eq!(cp.updated_epoch, Some(42));
    }

    // D1 budget law: the seed stays under the deliberate headroom
    // budget (≤ ~6,000 est tokens, well under the 8,192/128 caps), and
    // the freshest turns survive when the budget bites.
    #[test]
    fn seed_respects_budgets_and_keeps_freshest_turns() {
        let mut cp = VoiceCheckpoint::default();
        let big = "x".repeat(4 * 1_000); // ~1,000 est tokens per turn
        for i in 0..40 {
            cp.recent_turns.push(CheckpointTurn {
                role: "user".to_string(),
                text: format!("{big} {i}"),
            });
        }
        cp.summary = "s".repeat(4 * 500); // ~500 est tokens
        let items = build_seed_items(&cp, "state block");
        let total: usize = items
            .iter()
            .map(|i| estimate_tokens(i["text"].as_str().unwrap_or_default()))
            .sum();
        assert!(total <= SEED_TOKEN_BUDGET_EST, "seed {total} over budget");
        assert!(items.len() <= SEED_ITEM_BUDGET);
        // Freshest turn survives.
        assert!(items
            .iter()
            .any(|i| i["text"].as_str().unwrap_or_default().ends_with(" 39")));
        // Head items are developer-role; turns keep their own role.
        assert_eq!(items[0]["role"], "developer");
    }

    #[test]
    fn seed_item_count_stays_under_provider_cap_with_many_small_turns() {
        let mut cp = VoiceCheckpoint::default();
        for i in 0..500 {
            cp.recent_turns.push(CheckpointTurn {
                role: "assistant".to_string(),
                text: format!("t{i}"),
            });
        }
        let items = build_seed_items(&cp, "");
        assert!(items.len() <= SEED_ITEM_BUDGET);
    }
}
