//! The repository's own skills, embedded at compile time so the daemon
//! can install `distribution: global` ones machine-wide (into
//! `~/.agents/skills/`) without a checkout on disk — the packaged app
//! ships them inside the binary exactly like the dashboard's static
//! assets.
//!
//! The table is a manual mirror of `skills/*/SKILL.md`; the parity test
//! below pins it to the filesystem so adding or renaming a skill without
//! updating this list fails the suite instead of silently shipping a
//! daemon that cannot distribute it. Global skills must be single-file
//! (SKILL.md only): the installer writes exactly one file per skill, and
//! the parity test enforces the constraint at the source.

/// `(directory name, SKILL.md contents)` for every skill the repo ships.
pub(crate) const BUILTIN_SKILLS: &[(&str, &str)] = &[
    (
        "intendant-agenda",
        include_str!("../../../skills/intendant-agenda/SKILL.md"),
    ),
    (
        "intendant-cli",
        include_str!("../../../skills/intendant-cli/SKILL.md"),
    ),
    (
        "intendant-log-search",
        include_str!("../../../skills/intendant-log-search/SKILL.md"),
    ),
    (
        "intendant-memory",
        include_str!("../../../skills/intendant-memory/SKILL.md"),
    ),
    (
        "phone-call",
        include_str!("../../../skills/phone-call/SKILL.md"),
    ),
    (
        "show-then-ask",
        include_str!("../../../skills/show-then-ask/SKILL.md"),
    ),
    (
        "station-e2e-qa",
        include_str!("../../../skills/station-e2e-qa/SKILL.md"),
    ),
    (
        "visual-collaboration",
        include_str!("../../../skills/visual-collaboration/SKILL.md"),
    ),
    (
        "voice-call-app",
        include_str!("../../../skills/voice-call-app/SKILL.md"),
    ),
    (
        "wayland-portal-e2e",
        include_str!("../../../skills/wayland-portal-e2e/SKILL.md"),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Pins the embedded table to `skills/` on disk: same directory set,
    /// same bytes, and every `distribution: global` skill is single-file.
    #[test]
    fn builtin_table_matches_the_skills_directory() {
        let skills_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let mut on_disk: Vec<String> = std::fs::read_dir(&skills_root)
            .expect("skills/ readable")
            .flatten()
            .filter(|e| e.path().join("SKILL.md").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let mut embedded: Vec<String> = BUILTIN_SKILLS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        embedded.sort();
        assert_eq!(
            embedded, on_disk,
            "builtin_skills.rs table drifted from skills/ — update the include list"
        );

        for (name, content) in BUILTIN_SKILLS {
            let disk = std::fs::read_to_string(skills_root.join(name).join("SKILL.md"))
                .unwrap_or_else(|e| panic!("read skills/{name}/SKILL.md: {e}"));
            assert_eq!(&disk, content, "embedded bytes for {name} are stale");

            let (config, _) = intendant_core::skills::parse_skill_md(content, Path::new(name))
                .unwrap_or_else(|e| panic!("skills/{name}/SKILL.md does not parse: {e}"));
            assert_eq!(&config.name, name, "frontmatter name must match the dir");
            if config.is_global() {
                let extra: Vec<String> = std::fs::read_dir(skills_root.join(name))
                    .unwrap()
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|f| f != "SKILL.md")
                    .collect();
                assert!(
                    extra.is_empty(),
                    "global skill {name} must be single-file (installer writes SKILL.md \
                     only); found support files: {extra:?}"
                );
            }
        }
    }
}
