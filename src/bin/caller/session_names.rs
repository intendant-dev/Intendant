use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const OVERLAY_FILE: &str = "session_names.json";
const MAX_SESSION_NAME_CHARS: usize = 180;

pub fn normalize_session_name(raw: &str) -> Result<String, String> {
    let name = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("session name cannot be empty".to_string());
    }
    if name.chars().count() <= MAX_SESSION_NAME_CHARS {
        Ok(name)
    } else {
        let mut out = name
            .chars()
            .take(MAX_SESSION_NAME_CHARS.saturating_sub(3))
            .collect::<String>();
        out.push_str("...");
        Ok(out)
    }
}

pub fn normalize_source(raw: &str) -> String {
    let value = raw.trim().to_lowercase();
    match value.as_str() {
        "" | "session" => "intendant".to_string(),
        "intendant" => "intendant".to_string(),
        "codex" => "codex".to_string(),
        "claude-code" | "claude_code" | "claudecode" | "claude code" | "cc" => {
            "claude-code".to_string()
        }
        "gemini" | "gemini-cli" | "gemini_cli" | "gemini cli" => "gemini".to_string(),
        _ => value,
    }
}

pub fn rename_session(
    home: &Path,
    source: &str,
    session_id: &str,
    name: &str,
) -> Result<String, String> {
    let name = normalize_session_name(name)?;
    let source = normalize_source(source);
    if source == "intendant" {
        write_intendant_session_name(home, session_id, &name)?;
    } else {
        write_overlay_session_name(home, &source, session_id, &name)?;
    }
    Ok(name)
}

pub fn apply_session_name_overlays(home: &Path, sessions: &mut [Value]) {
    let overlays = read_overlay_map(home);
    if overlays.is_empty() {
        return;
    }
    for session in sessions {
        let Some(source) = session
            .get("source")
            .and_then(|v| v.as_str())
            .map(normalize_source)
        else {
            continue;
        };
        if source == "intendant" {
            continue;
        }
        let name = session
            .get("session_id")
            .and_then(|v| v.as_str())
            .and_then(|id| overlay_lookup(&overlays, &source, id))
            .or_else(|| {
                session
                    .get("resume_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| overlay_lookup(&overlays, &source, id))
            });
        if let Some(name) = name {
            if let Some(obj) = session.as_object_mut() {
                obj.insert("name".to_string(), Value::String(name));
            }
        }
    }
}

fn overlay_lookup(
    overlays: &HashMap<String, HashMap<String, String>>,
    source: &str,
    session_id: &str,
) -> Option<String> {
    overlays
        .get(source)
        .and_then(|m| m.get(session_id))
        .cloned()
}

fn overlay_path(home: &Path) -> PathBuf {
    home.join(".intendant").join(OVERLAY_FILE)
}

fn read_overlay_map(home: &Path) -> HashMap<String, HashMap<String, String>> {
    let path = overlay_path(home);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let Some(obj) = value.as_object() else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (source, entries) in obj {
        let Some(entries) = entries.as_object() else {
            continue;
        };
        let mut source_map = HashMap::new();
        for (session_id, name) in entries {
            let Some(name) = name.as_str() else {
                continue;
            };
            if let Ok(name) = normalize_session_name(name) {
                source_map.insert(session_id.clone(), name);
            }
        }
        if !source_map.is_empty() {
            out.insert(normalize_source(source), source_map);
        }
    }
    out
}

fn write_overlay_session_name(
    home: &Path,
    source: &str,
    session_id: &str,
    name: &str,
) -> Result<(), String> {
    let path = overlay_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create overlay dir: {e}"))?;
    }
    let mut root = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        Err(_) => Value::Object(Map::new()),
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("root is object");
    let source_value = root_obj
        .entry(source.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !source_value.is_object() {
        *source_value = Value::Object(Map::new());
    }
    source_value
        .as_object_mut()
        .expect("source is object")
        .insert(session_id.to_string(), Value::String(name.to_string()));
    let json =
        serde_json::to_string_pretty(&root).map_err(|e| format!("serialize overlay: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("write overlay: {e}"))
}

fn write_intendant_session_name(home: &Path, session_id: &str, name: &str) -> Result<(), String> {
    let dir = intendant_session_dir_from_home(home, session_id)
        .ok_or_else(|| format!("Intendant session {} not found", session_id))?;
    let meta_path = dir.join("session_meta.json");
    let raw =
        std::fs::read_to_string(&meta_path).map_err(|e| format!("read session metadata: {e}"))?;
    let mut meta: Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse session metadata: {e}"))?;
    let Some(obj) = meta.as_object_mut() else {
        return Err("session metadata is not a JSON object".to_string());
    };
    obj.insert("name".to_string(), Value::String(name.to_string()));
    let json =
        serde_json::to_string_pretty(&meta).map_err(|e| format!("serialize metadata: {e}"))?;
    std::fs::write(meta_path, json).map_err(|e| format!("write session metadata: {e}"))
}

fn intendant_session_dir_from_home(home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.contains('/') {
        return intendant_session_dir_from_slash_path(home, session_id);
    }

    let logs_dir = home.join(".intendant").join("logs");
    let direct = logs_dir.join(session_id);
    if direct.is_dir() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(session_id) {
            return Some(path);
        }
        let meta_path = path.join("session_meta.json");
        let Ok(meta_str) = std::fs::read_to_string(meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&meta_str) else {
            continue;
        };
        let Some(meta_id) = meta.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if meta_id == session_id || meta_id.starts_with(session_id) {
            return Some(path);
        }
    }

    None
}

pub(crate) fn intendant_session_dir_from_slash_path(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let candidate = PathBuf::from(session_id);
    if !candidate.is_dir() {
        return None;
    }
    let logs_dir = home.join(".intendant").join("logs");
    let logs_dir = std::fs::canonicalize(logs_dir).ok()?;
    let candidate = std::fs::canonicalize(candidate).ok()?;
    // Slash-form session paths must resolve inside the Intendant logs root.
    candidate.starts_with(&logs_dir).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_applies_external_session_name() {
        let home = tempfile::tempdir().unwrap();
        rename_session(home.path(), "claude-code", "abc123", "  Nice   name  ").unwrap();
        let mut sessions = vec![serde_json::json!({
            "source": "claude-code",
            "session_id": "abc123",
            "task": "Original task"
        })];

        apply_session_name_overlays(home.path(), &mut sessions);

        assert_eq!(sessions[0]["name"], "Nice name");
        assert_eq!(sessions[0]["task"], "Original task");
    }

    #[test]
    fn intendant_rename_updates_session_meta() {
        let home = tempfile::tempdir().unwrap();
        let session_dir = home.path().join(".intendant/logs/session-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "session-1",
                "created_at": "2026-05-20T00:00:00",
                "task": "Original task"
            })
            .to_string(),
        )
        .unwrap();

        rename_session(home.path(), "intendant", "session-1", "Renamed").unwrap();

        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(session_dir.join("session_meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta["name"], "Renamed");
        assert_eq!(meta["task"], "Original task");
    }

    #[test]
    fn intendant_slash_session_path_must_stay_under_logs_root() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let session_dir = outside.path().join("session-escape");
        std::fs::create_dir_all(&session_dir).unwrap();

        assert!(
            intendant_session_dir_from_slash_path(home.path(), &session_dir.to_string_lossy())
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn intendant_slash_session_path_rejects_symlink_escape() {
        let home = tempfile::tempdir().unwrap();
        let logs_dir = home.path().join(".intendant").join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_session = outside.path().join("session-escape");
        std::fs::create_dir_all(&outside_session).unwrap();
        let link = logs_dir.join("link-session");
        std::os::unix::fs::symlink(&outside_session, &link).unwrap();

        assert!(
            intendant_session_dir_from_slash_path(home.path(), &link.to_string_lossy()).is_none()
        );
    }
}
