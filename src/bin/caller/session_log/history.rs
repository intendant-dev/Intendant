//! Read-back of conversation history from a session log directory:
//! voice_log/user_transcript turns for presence context, keyword search
//! over voice entries, and raw recent-entry tails.

use super::*;

/// A reconstructed conversation turn from voice_log / user_transcript events.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub role: String, // "user" or "model"
    pub text: String,
    #[allow(dead_code)]
    pub seq: u64,
}

/// Reconstruct recent conversation turns from voice_log and user_transcript events
/// in session.jsonl. Returns the last `max_entries` turns ordered by seq.
pub fn recent_conversation(log_dir: &Path, max_entries: usize) -> Vec<ConversationTurn> {
    // Prefer transcript.jsonl (simpler, faster to parse) if available
    let transcript_path = log_dir.join("transcript.jsonl");
    if transcript_path.exists() {
        if let Ok(content) = fs::read_to_string(&transcript_path) {
            let mut turns: Vec<ConversationTurn> = Vec::new();
            for line in content.lines() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    let role = val["role"].as_str().unwrap_or("");
                    let text = val["text"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() && (role == "user" || role == "model") {
                        turns.push(ConversationTurn {
                            role: role.to_string(),
                            text,
                            seq: 0,
                        });
                    }
                }
            }
            let start = turns.len().saturating_sub(max_entries);
            return turns[start..].to_vec();
        }
    }

    // Fall back to session.jsonl parsing
    let path = log_dir.join("session.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut turns: Vec<ConversationTurn> = Vec::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = val["event"].as_str().unwrap_or("");
        let text = val["message"].as_str().unwrap_or("").to_string();
        if text.is_empty() {
            continue;
        }
        let seq = val["data"]["seq"].as_u64().unwrap_or(0);

        match event {
            "user_transcript" => {
                turns.push(ConversationTurn {
                    role: "user".to_string(),
                    text,
                    seq,
                });
            }
            "voice_log" => {
                // Only include transcript entries (model speech), not tool calls
                let tool_ctx = val["data"]["tool_context"].as_str().unwrap_or("");
                if tool_ctx == "transcript" {
                    turns.push(ConversationTurn {
                        role: "model".to_string(),
                        text,
                        seq,
                    });
                }
            }
            _ => {}
        }
    }

    // Entries are already in chronological order from the JSONL file —
    // don't sort by seq since user_transcript and voice_log have independent
    // sequence counters that would interleave incorrectly.
    let start = turns.len().saturating_sub(max_entries);
    turns[start..].to_vec()
}

/// Search voice_log and user_transcript entries for keyword matches.
/// Returns formatted results (up to `max_results`).
pub fn search_voice_entries(
    log_dir: &Path,
    keywords: &[String],
    max_results: usize,
) -> Vec<String> {
    let path = log_dir.join("session.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = val["event"].as_str().unwrap_or("");
        if event != "voice_log" && event != "user_transcript" {
            continue;
        }
        let text = val["message"].as_str().unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let lower = text.to_lowercase();
        if keywords.iter().any(|kw| lower.contains(&kw.to_lowercase())) {
            let role = if event == "user_transcript" {
                "User"
            } else {
                "Model"
            };
            results.push(format!("[{}] {}", role, text));
            if results.len() >= max_results {
                break;
            }
        }
    }
    results
}

/// Read the last `count` lines from the session.jsonl file in the given log directory.
/// Returns an empty vec if the file doesn't exist or can't be read.
pub fn recent_entries(log_dir: &std::path::Path, count: usize) -> Vec<String> {
    let path = log_dir.join("session.jsonl");
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(count);
            lines[start..].iter().map(|l| l.to_string()).collect()
        }
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_entries_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();
        let jsonl_path = log_dir.join("session.jsonl");
        let mut f = fs::File::create(&jsonl_path).unwrap();
        for i in 0..10 {
            use std::io::Write;
            writeln!(f, r#"{{"event":"test","index":{}}}"#, i).unwrap();
        }
        drop(f);

        let entries = recent_entries(&log_dir, 3);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].contains("\"index\":7"));
        assert!(entries[2].contains("\"index\":9"));
    }

    #[test]
    fn recent_entries_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let entries = recent_entries(dir.path(), 5);
        assert!(entries.is_empty());
    }

    #[test]
    fn recent_entries_fewer_than_count() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();
        let jsonl_path = log_dir.join("session.jsonl");
        fs::write(&jsonl_path, "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let entries = recent_entries(&log_dir, 100);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn recent_conversation_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let turns = recent_conversation(dir.path(), 10);
        assert!(turns.is_empty());
    }

    #[test]
    fn recent_conversation_reconstructs_turns() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("what's in this project?", 1);
        log.voice_log("It's an autonomous agent runtime.", 2, Some("transcript"));
        log.voice_log("[tool] check_status({})", 3, Some("check_status"));
        log.user_transcript("can you fix the auth bug?", 4);
        log.voice_log("I'll submit that task now.", 5, Some("transcript"));
        // Flush buffered voice utterance (normally happens on turnComplete/session end)
        log.mark_interrupted();
        drop(log);

        let turns = recent_conversation(&log_dir, 10);
        assert_eq!(turns.len(), 4); // tool call excluded
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "what's in this project?");
        assert_eq!(turns[1].role, "model");
        assert_eq!(turns[1].text, "It's an autonomous agent runtime.");
        assert_eq!(turns[2].role, "user");
        assert_eq!(turns[3].role, "model");
    }

    #[test]
    fn recent_conversation_respects_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        for i in 0..10 {
            log.user_transcript(&format!("msg {}", i), i);
        }

        let turns = recent_conversation(&log_dir, 3);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].text, "msg 7");
        assert_eq!(turns[2].text, "msg 9");
    }

    #[test]
    fn search_voice_entries_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("fix the authentication bug", 1);
        log.voice_log("I'll check the auth module.", 2, Some("transcript"));
        log.user_transcript("also check the database", 3);
        log.voice_log("[tool] check_status({})", 4, Some("check_status"));

        let results = search_voice_entries(&log_dir, &["auth".to_string()], 10);
        assert_eq!(results.len(), 2);
        assert!(results[0].starts_with("[User]"));
        assert!(results[1].starts_with("[Model]"));
    }

    #[test]
    fn search_voice_entries_respects_max_results() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        for i in 0..10 {
            log.user_transcript(&format!("test message {}", i), i);
        }

        let results = search_voice_entries(&log_dir, &["test".to_string()], 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_voice_entries_empty_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("hello world", 1);

        let results = search_voice_entries(&log_dir, &["nonexistent".to_string()], 10);
        assert!(results.is_empty());
    }
}
