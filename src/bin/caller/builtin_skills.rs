//! Skills shipped inside the Intendant binary.
//!
//! The daemon installs this complete manifest machine-wide under the independent
//! `~/.agents/skills/` and `~/.claude/skills/` roots, so packaged installs do
//! not need a repository checkout. The parity test pins every embedded byte to
//! `skills/` on disk: adding, removing, or renaming a support file without
//! updating this table fails the suite instead of shipping a partial skill.

/// One skill embedded in the binary.
pub(crate) struct BuiltinSkill {
    pub(crate) name: &'static str,
    pub(crate) skill_md: &'static str,
    /// `(path relative to the skill directory, bytes)`, excluding `SKILL.md`.
    pub(crate) support_files: &'static [(&'static str, &'static [u8])],
}

pub(crate) const BUILTIN_SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        name: "intendant-agenda",
        skill_md: include_str!("../../../skills/intendant-agenda/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "intendant-cli",
        skill_md: include_str!("../../../skills/intendant-cli/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "intendant-coordination",
        skill_md: include_str!("../../../skills/intendant-coordination/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "intendant-log-search",
        skill_md: include_str!("../../../skills/intendant-log-search/SKILL.md"),
        support_files: &[
            (
                "agents/openai.yaml",
                include_bytes!("../../../skills/intendant-log-search/agents/openai.yaml"),
            ),
            (
                "references/artifact-map.md",
                include_bytes!("../../../skills/intendant-log-search/references/artifact-map.md"),
            ),
            (
                "references/event-taxonomy.md",
                include_bytes!("../../../skills/intendant-log-search/references/event-taxonomy.md"),
            ),
            (
                "references/query-recipes.md",
                include_bytes!("../../../skills/intendant-log-search/references/query-recipes.md"),
            ),
        ],
    },
    BuiltinSkill {
        name: "intendant-memory",
        skill_md: include_str!("../../../skills/intendant-memory/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "phone-call",
        skill_md: include_str!("../../../skills/phone-call/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "show-then-ask",
        skill_md: include_str!("../../../skills/show-then-ask/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "station-e2e-qa",
        skill_md: include_str!("../../../skills/station-e2e-qa/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "visual-collaboration",
        skill_md: include_str!("../../../skills/visual-collaboration/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "voice-call-app",
        skill_md: include_str!("../../../skills/voice-call-app/SKILL.md"),
        support_files: &[],
    },
    BuiltinSkill {
        name: "wayland-portal-e2e",
        skill_md: include_str!("../../../skills/wayland-portal-e2e/SKILL.md"),
        support_files: &[],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn collect_files(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(root, &path, files);
            } else if path.is_file() {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }

    #[test]
    fn builtin_table_matches_the_skills_directory() {
        let skills_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let mut on_disk_dirs: Vec<String> = std::fs::read_dir(&skills_root)
            .expect("skills/ readable")
            .flatten()
            .filter(|entry| entry.path().join("SKILL.md").exists())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        on_disk_dirs.sort();
        let mut embedded_dirs: Vec<String> = BUILTIN_SKILLS
            .iter()
            .map(|skill| skill.name.to_string())
            .collect();
        embedded_dirs.sort();
        assert_eq!(
            embedded_dirs, on_disk_dirs,
            "builtin skill manifest drifted from skills/"
        );

        for skill in BUILTIN_SKILLS {
            let skill_root = skills_root.join(skill.name);
            let mut actual = BTreeMap::new();
            collect_files(&skill_root, &skill_root, &mut actual);

            let mut expected = BTreeMap::new();
            expected.insert(PathBuf::from("SKILL.md"), skill.skill_md.as_bytes());
            for (relative, bytes) in skill.support_files {
                assert!(
                    !Path::new(relative).is_absolute()
                        && !Path::new(relative)
                            .components()
                            .any(|part| part == std::path::Component::ParentDir),
                    "builtin skill support path must stay relative: {relative}"
                );
                assert!(
                    expected.insert(PathBuf::from(relative), bytes).is_none(),
                    "duplicate builtin skill path: {relative}"
                );
            }

            let actual_paths: Vec<&PathBuf> = actual.keys().collect();
            let expected_paths: Vec<&PathBuf> = expected.keys().collect();
            assert_eq!(
                actual_paths, expected_paths,
                "embedded file list for {} is stale",
                skill.name
            );
            for (relative, expected_bytes) in expected {
                assert_eq!(
                    actual.get(&relative).map(Vec::as_slice),
                    Some(expected_bytes),
                    "embedded bytes for {}/{} are stale",
                    skill.name,
                    relative.display()
                );
            }

            let (config, _) =
                intendant_core::skills::parse_skill_md(skill.skill_md, Path::new(skill.name))
                    .unwrap_or_else(|error| {
                        panic!("skills/{}/SKILL.md does not parse: {error}", skill.name)
                    });
            assert_eq!(
                config.name, skill.name,
                "frontmatter name must match the directory"
            );
        }
    }
}
