//! The tombed-memory cutover's CI absence gate (umbrella RFC
//! Appendix A): the legacy system's EXACT identifiers may never
//! reappear in the scanned surfaces — prompts, source (schemas,
//! runtime fields, control messages, Presence), browser fragments,
//! skills, and docs. Deliberately NOT a ban on the word "memory":
//! the Memory-plane service (`memory_search`/`memory_read`/
//! `memory_propose`), Codex `/memory-reset`, and OS/shared-memory
//! vocabulary are all legitimate. Leftover `.intendant/memory.json`
//! files on disk stay inert — nothing here (or anywhere) reads them.

#[cfg(test)]
mod tests {
    /// One entry per tombed identifier — exact substrings, chosen so
    /// no current or plausible future legitimate identifier collides.
    const DENYLIST: &[&str] = &[
        "store_memory",
        "recall_memory",
        "storeMemory",
        "recallMemory",
        "RecallMemory",
        "inherit_memory",
        "memory_file",
        "memory_key",
        "memory_summary",
        "memory_query",
        "memory_channel",
        "memory_tags",
        "memory_since",
        "memory_source",
        "memory_path",
        "MemoryConfig",
        "format_for_injection",
        "KnowledgeQuery",
        "knowledge_path",
        "Capability::Knowledge",
    ];

    /// Directories/files scanned, relative to the crate root. Scoped to
    /// the surfaces Appendix A names (plus docs and skills so teaching
    /// text cannot resurrect the vocabulary).
    const ROOTS: &[&str] = &[
        "src",
        "crates/intendant-core/src",
        "crates/presence-core/src",
        "crates/presence-web/src",
        "crates/station-web/src",
        "static/app",
        "docs/src",
        "skills",
        "tests",
    ];

    fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                // Vendored kernel corpus vectors carry arbitrary hex,
                // not identifiers; skip generated/vendor payload dirs.
                if name == "corpus" || name == "target" {
                    continue;
                }
                scan(&path, hits);
                continue;
            }
            // This gate's own file carries the denylist strings.
            if name == "cutover_absence.rs" {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // binary artifacts (wasm, images)
            };
            for needle in DENYLIST {
                if text.contains(needle) {
                    hits.push(format!("{}: {}", path.display(), needle));
                }
            }
        }
    }

    /// Appendix A's terminal gate: the tombed system stays deleted.
    #[test]
    fn tombed_identifiers_are_absent_everywhere() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut hits = Vec::new();
        for rel in ROOTS {
            scan(&root.join(rel), &mut hits);
        }
        for prompt in std::fs::read_dir(root).unwrap().flatten() {
            let name = prompt.file_name().to_string_lossy().to_string();
            if name.starts_with("SysPrompt") || name == "CLAUDE.md" || name == "AGENTS.md" {
                let text = std::fs::read_to_string(prompt.path()).unwrap_or_default();
                for needle in DENYLIST {
                    if text.contains(needle) {
                        hits.push(format!("{name}: {needle}"));
                    }
                }
            }
        }
        assert!(
            hits.is_empty(),
            "tombed-memory identifiers resurfaced:\n{}",
            hits.join("\n")
        );
    }
}
