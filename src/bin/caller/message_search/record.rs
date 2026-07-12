//! The shared `MessageRecord` every extractor produces and the store
//! persists (message-search plan §5). Records carry ORIGINAL text —
//! normalization and matching live in the query arena (C1) — and never a
//! stored active/superseded flag: supersession is DERIVED by replaying
//! [`SupersessionMark`]s, because a Codex same-thread restore can
//! reactivate previously superseded messages (plan D2).

use serde::{Deserialize, Serialize};

pub(crate) const PARSER_VERSION: u32 = 1;

/// Per-message text cap (plan §5): a pasted blob must not dominate a
/// shard. S0a measured the current corpus max at 126 KiB, so the cap is
/// currently free; exceeding it sets `truncated` and counts toward the
/// coverage report's omitted bytes.
pub(crate) const MESSAGE_TEXT_CAP_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    Intendant,
    Codex,
    ClaudeCode,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::Intendant => "intendant",
            Source::Codex => "codex",
            Source::ClaudeCode => "claude-code",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    User,
    Assistant,
}

/// Where a hit lives in its source — opaque to clients, resolvable by the
/// session-detail `locate` read (plan §7). Versioned by variant: new
/// locator kinds are additive, and resolution returns a typed
/// stale/unavailable result rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Locator {
    /// New-era native message: the `conversation_message` event's id.
    NativeMessageId { message_id: String },
    /// Legacy native user-side event: position + content hash (the event
    /// stream is append-only, so line identity is stable; the hash
    /// detects parser-era drift).
    NativeEvent {
        line_no: u64,
        content_hash16: String,
    },
    /// Legacy native assistant span in a `turns/*_model.txt` sidecar.
    NativeSidecarSpan {
        file: String,
        offset: u64,
        len: u64,
        content_hash16: String,
    },
    /// External record with a native id (Claude record `uuid`, Codex
    /// `response_item` id).
    ExternalRecordId { record_id: String },
    /// External record without a native id: file line + content hash;
    /// `generation` pins which rewrite of the source produced it (Codex
    /// same-thread restore rewrites the rollout).
    ExternalLine {
        generation: u32,
        line_no: u64,
        content_hash16: String,
    },
}

/// Which branch/generation of the source history a record belongs to.
/// `0` is the initial generation; Codex restores bump it. The reader
/// derives per-record active status from marks + membership; membership
/// alone never implies superseded.
pub(crate) type Generation = u32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MessageRecord {
    pub source: Source,
    pub session_id: String,
    pub role: Role,
    /// Epoch ms; the retention window keys on this.
    pub ts_ms: i64,
    /// ORIGINAL text (possibly capped at [`MESSAGE_TEXT_CAP_BYTES`]).
    pub text: String,
    pub locator: Locator,
    /// Native `Message.seq` (intendant), used by rewind-cut marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Codex user-turn ordinal, used by turn-count rollback marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_turn: Option<u32>,
    /// Source item id (Codex response_item), used by item-anchor marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subagent: bool,
    #[serde(default)]
    pub generation: Generation,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Supersession inputs, replayed IN ORDER by the reader to derive each
/// record's active status (plan §5: derived, recomputable, never an
/// irreversible stamp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SupersessionMark {
    /// Native rewind: records with `seq > cut_after_seq` are superseded.
    SeqCut { cut_after_seq: u64, at_ms: i64 },
    /// Codex `thread_rolled_back { num_turns }`: the last N still-active
    /// user turns (and their following assistant records) supersede.
    TurnCount { num_turns: u32, at_ms: i64 },
    /// Codex item-anchor rewind: everything after the anchored item (or
    /// from it, when `position == "before"`) supersedes.
    ItemAnchor {
        item_id: String,
        /// `"before"` or `"after"` (the anchor's own fate).
        position: String,
        at_ms: i64,
    },
    /// Codex same-thread restore: a new generation becomes active.
    /// Records of LATER generations supersede (they belong to a branch
    /// that was rolled away); records of `active_generation` reactivate.
    GenerationRestore {
        active_generation: Generation,
        at_ms: i64,
    },
}

/// Replay `marks` over `records` (both in source order) and return the
/// derived active flags, index-aligned with `records`.
///
/// The model is deliberately simple and total: marks apply to the records
/// that precede them in source order; a later `GenerationRestore` can
/// reactivate records a `TurnCount`/`ItemAnchor` superseded only if it
/// restores their generation (Codex re-writes restored turns into the new
/// generation, so reactivation-by-rewrite appears as fresh records; the
/// mark exists so records of abandoned generations stop reading active).
pub(crate) fn derive_active(records: &[MessageRecord], marks: &[SupersessionMark]) -> Vec<bool> {
    let mut active: Vec<bool> = records.iter().map(|_| true).collect();
    let mut active_generation: Generation = records
        .iter()
        .map(|record| record.generation)
        .max()
        .unwrap_or(0);
    for mark in marks {
        match mark {
            SupersessionMark::SeqCut { cut_after_seq, .. } => {
                for (index, record) in records.iter().enumerate() {
                    if record.seq.is_some_and(|seq| seq > *cut_after_seq) {
                        active[index] = false;
                    }
                }
            }
            SupersessionMark::TurnCount { num_turns, .. } => {
                // Collect still-active user turns in order, supersede the
                // trailing `num_turns` of them (bounded by what exists —
                // corrupt/huge counts must not loop; plan §5).
                let mut turns: Vec<u32> = Vec::new();
                for (index, record) in records.iter().enumerate() {
                    if active[index] {
                        if let Some(turn) = record.user_turn {
                            if !turns.contains(&turn) {
                                turns.push(turn);
                            }
                        }
                    }
                }
                let cut = turns.len().saturating_sub(*num_turns as usize);
                let superseded: &[u32] = &turns[cut..];
                for (index, record) in records.iter().enumerate() {
                    if record
                        .user_turn
                        .is_some_and(|turn| superseded.contains(&turn))
                    {
                        active[index] = false;
                    }
                }
            }
            SupersessionMark::ItemAnchor {
                item_id, position, ..
            } => {
                if let Some(anchor_index) = records
                    .iter()
                    .position(|record| record.item_id.as_deref() == Some(item_id.as_str()))
                {
                    let start = if position == "before" {
                        anchor_index
                    } else {
                        anchor_index + 1
                    };
                    for flag in active.iter_mut().skip(start) {
                        *flag = false;
                    }
                }
            }
            SupersessionMark::GenerationRestore {
                active_generation: restored,
                ..
            } => {
                active_generation = *restored;
            }
        }
    }
    for (index, record) in records.iter().enumerate() {
        if record.generation > active_generation {
            active[index] = false;
        }
    }
    active
}

/// Cap `text` at [`MESSAGE_TEXT_CAP_BYTES`] on a char boundary; returns
/// the (possibly trimmed) text and whether it was truncated.
pub(crate) fn cap_text(text: String) -> (String, bool) {
    if text.len() <= MESSAGE_TEXT_CAP_BYTES {
        return (text, false);
    }
    let mut cut = MESSAGE_TEXT_CAP_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = text;
    text.truncate(cut);
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u64, generation: Generation) -> MessageRecord {
        MessageRecord {
            source: Source::Intendant,
            session_id: "s".into(),
            role: Role::User,
            ts_ms: 1,
            text: format!("m{seq}"),
            locator: Locator::NativeMessageId {
                message_id: format!("id-{seq}"),
            },
            seq: Some(seq),
            user_turn: None,
            item_id: None,
            subagent: false,
            generation,
            truncated: false,
        }
    }

    #[test]
    fn seq_cut_supersedes_later_messages_only() {
        let records = vec![record(1, 0), record(2, 0), record(3, 0)];
        let marks = vec![SupersessionMark::SeqCut {
            cut_after_seq: 2,
            at_ms: 10,
        }];
        assert_eq!(derive_active(&records, &marks), vec![true, true, false]);
    }

    #[test]
    fn turn_count_is_bounded_by_existing_turns() {
        let mut records: Vec<MessageRecord> = (1..=3)
            .map(|turn| {
                let mut r = record(turn as u64, 0);
                r.user_turn = Some(turn);
                r
            })
            .collect();
        records.push(record(9, 0)); // no user_turn — untouched
        let marks = vec![SupersessionMark::TurnCount {
            num_turns: u32::MAX, // corrupt/huge count must not panic or loop
            at_ms: 10,
        }];
        assert_eq!(
            derive_active(&records, &marks),
            vec![false, false, false, true]
        );
    }

    #[test]
    fn item_anchor_supersedes_the_tail() {
        let mut a = record(1, 0);
        a.item_id = Some("item-a".into());
        let mut b = record(2, 0);
        b.item_id = Some("item-b".into());
        let c = record(3, 0);
        let records = vec![a, b, c];
        let after = vec![SupersessionMark::ItemAnchor {
            item_id: "item-b".into(),
            position: "after".into(),
            at_ms: 10,
        }];
        assert_eq!(derive_active(&records, &after), vec![true, true, false]);
        let before = vec![SupersessionMark::ItemAnchor {
            item_id: "item-b".into(),
            position: "before".into(),
            at_ms: 10,
        }];
        assert_eq!(derive_active(&records, &before), vec![true, false, false]);
    }

    #[test]
    fn generation_restore_reactivates_and_retires_branches() {
        // gen0 history, a gen1 branch, then a restore back to gen0: the
        // gen1 branch stops reading active; gen0 records stay active even
        // if a turn-count mark inside gen1's life had superseded nothing
        // of theirs.
        let records = vec![record(1, 0), record(2, 1), record(3, 1)];
        // Without a restore, the max generation (1) is active: everything
        // reads active.
        assert_eq!(
            derive_active(&records, &[]),
            vec![true, true, true],
            "no marks: all generations live"
        );
        let marks = vec![SupersessionMark::GenerationRestore {
            active_generation: 0,
            at_ms: 10,
        }];
        assert_eq!(derive_active(&records, &marks), vec![true, false, false]);
    }

    #[test]
    fn cap_text_cuts_on_char_boundary() {
        let text = "é".repeat(MESSAGE_TEXT_CAP_BYTES); // 2 bytes per char
        let (capped, truncated) = cap_text(text);
        assert!(truncated);
        assert!(capped.len() <= MESSAGE_TEXT_CAP_BYTES);
        assert!(capped.chars().all(|c| c == 'é'));
        let (same, truncated) = cap_text("short".into());
        assert!(!truncated);
        assert_eq!(same, "short");
    }

    #[test]
    fn record_roundtrips_and_skips_defaults() {
        let r = record(5, 0);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("subagent"), "false flags are skipped");
        assert!(!json.contains("item_id"), "absent options are skipped");
        let back: MessageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
