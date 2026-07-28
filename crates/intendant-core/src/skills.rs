//! Skill discovery, parsing, and invocation.
//!
//! Skills are named instruction sets stored as `SKILL.md` files with YAML
//! frontmatter, following the Agent Skills open standard (agentskills.io).
//! They are discovered from these locations (first match wins per name):
//!
//! 1. `<project_root>/.agents/skills/<name>/SKILL.md`  (standard path)
//! 2. `<project_root>/skills/<name>/SKILL.md`
//! 3. `~/.agents/skills/<name>/SKILL.md`  (standard path)
//!
//! The model can invoke skills via the `invoke_skill` tool, or the user can
//! trigger them via the control socket / TUI / presence layer.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Parsed SKILL.md frontmatter.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillConfig {
    pub name: String,
    pub description: String,
    /// Override session autonomy level when this skill is active.
    /// Parsed frontmatter contract; the TUI skills panel was its last
    /// reader — enforcement/surfacing is future skills-system work.
    #[serde(default)]
    #[allow(dead_code)]
    pub autonomy: Option<String>,
    /// If true, the model cannot auto-invoke this skill — user must trigger it.
    #[serde(default, alias = "disable-auto-invocation")]
    pub disable_auto_invocation: bool,
    /// Override session sandbox setting. Same status as `autonomy` above.
    #[serde(default)]
    #[allow(dead_code)]
    pub sandbox: Option<bool>,
    /// Free-text environment requirements (Agent Skills standard
    /// `compatibility`). Surfaced verbatim in the injected catalog so the
    /// model can pre-filter — deliberately ahead of other harnesses,
    /// which drop the field before the model ever sees it (verified
    /// 2026-07-19 on claude 2.1.215 / codex 0.144.6).
    #[serde(default)]
    pub compatibility: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkillSource {
    /// `<project_root>/.agents/skills/` or `<project_root>/skills/`
    Project,
    /// `~/.agents/skills/`
    Personal,
}

/// A fully loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub config: SkillConfig,
    /// Markdown instructions after the frontmatter.
    pub body: String,
    /// Provenance, kept for future surfacing (the TUI skills panel was
    /// the last display of these).
    #[allow(dead_code)]
    pub source_path: PathBuf,
    #[allow(dead_code)]
    pub source: SkillSource,
}

/// Split a `SKILL.md`-shaped document into its raw YAML frontmatter and
/// markdown body.
///
/// The file must start with `---`, followed by YAML frontmatter, closed by
/// another `---` line. Everything after is the body. Shared by the lenient
/// skills lane ([`parse_skill_md`]) and the strict conforming-subset lane
/// ([`parse_frontmatter_strict`])'s callers.
pub fn split_frontmatter<'a>(
    content: &'a str,
    source_path: &Path,
) -> Result<(&'a str, &'a str), String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(format!(
            "{}: missing YAML frontmatter (must start with ---)",
            source_path.display()
        ));
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let rest = after_first.trim_start_matches(['\r', '\n']);
    let closing = rest.find("\n---");
    let Some(closing_pos) = closing else {
        return Err(format!(
            "{}: unterminated YAML frontmatter (missing closing ---)",
            source_path.display()
        ));
    };

    let yaml_str = &rest[..closing_pos];
    let body_start = closing_pos + 4; // skip "\n---"
    let body = rest[body_start..].trim_start_matches(['\r', '\n']);
    Ok((yaml_str, body))
}

/// Parse a SKILL.md file's content into config + body.
pub fn parse_skill_md(content: &str, source_path: &Path) -> Result<(SkillConfig, String), String> {
    let (yaml_str, body) = split_frontmatter(content, source_path)?;

    // Parse YAML frontmatter manually (flat key-value) to avoid serde_yaml dependency.
    let config =
        parse_frontmatter(yaml_str).map_err(|e| format!("{}: {}", source_path.display(), e))?;

    Ok((config, body.to_string()))
}

/// One value in the strict frontmatter lane: a scalar, or a one-level map
/// of string scalars (the Agent Skills spec's `metadata:` shape — its only
/// nested form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterValue {
    Scalar(String),
    Map(Vec<(String, String)>),
}

/// Parse frontmatter as the agentskills.io **conforming subset**: flat
/// `key: value` scalars (quotes stripped, `>`/`|` block scalars joined as
/// in the lenient lane) plus at most one level of string-to-string maps
/// (`metadata:`). Where the lenient skills parser skips what it does not
/// understand, this lane REFUSES it by name: junk lines, duplicate keys,
/// flow syntax, list items, block scalars inside maps, and any nesting
/// beyond a map's single level are errors — spec conformance is the
/// deny-unknown rigor at this layer. Key vocabulary is the CALLER's to
/// enforce; this parses shape only.
pub fn parse_frontmatter_strict(yaml: &str) -> Result<Vec<(String, FrontmatterValue)>, String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut entries: Vec<(String, FrontmatterValue)> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            i += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(format!(
                "unexpected indented line {line:?} — one-level maps follow an empty-valued key"
            ));
        }
        let Some(colon_pos) = line.find(':') else {
            return Err(format!("{line:?} is not a `key: value` line"));
        };
        let key = line[..colon_pos].trim();
        if key.is_empty() {
            return Err(format!("{line:?} has an empty key"));
        }
        if entries.iter().any(|(seen, _)| seen == key) {
            return Err(format!("duplicate frontmatter key {key:?}"));
        }
        let raw_value = line[colon_pos + 1..].trim();

        if raw_value == ">" || raw_value == "|" {
            // Block scalar, exactly the lenient lane's joining rule.
            let mut parts = Vec::new();
            i += 1;
            while i < lines.len() {
                let cont = lines[i];
                if cont.is_empty() || cont.starts_with(' ') || cont.starts_with('\t') {
                    parts.push(cont.trim());
                    i += 1;
                } else {
                    break;
                }
            }
            let joined = parts.join(if raw_value == ">" { " " } else { "\n" });
            entries.push((key.to_string(), FrontmatterValue::Scalar(joined)));
            continue;
        }

        if raw_value.is_empty() {
            // Empty value: a one-level map when indented `k: v` children
            // follow, otherwise an empty scalar.
            let mut children: Vec<(String, String)> = Vec::new();
            i += 1;
            while i < lines.len() {
                let child = lines[i];
                if child.trim().is_empty() {
                    // A blank line inside a child run is allowed only while
                    // more children follow; trailing blanks fall through.
                    let more = lines[i + 1..].iter().any(|l| !l.trim().is_empty());
                    let next_indented = lines[i + 1..]
                        .iter()
                        .find(|l| !l.trim().is_empty())
                        .is_some_and(|l| l.starts_with(' ') || l.starts_with('\t'));
                    if more && next_indented {
                        i += 1;
                        continue;
                    }
                    break;
                }
                if !child.starts_with(' ') && !child.starts_with('\t') {
                    break;
                }
                let child_line = child.trim();
                let Some(child_colon) = child_line.find(':') else {
                    return Err(format!(
                        "map entry {child_line:?} under {key:?} is not a `key: value` line"
                    ));
                };
                let child_key = child_line[..child_colon].trim();
                let child_value = child_line[child_colon + 1..].trim();
                if child_key.is_empty() {
                    return Err(format!(
                        "map entry {child_line:?} under {key:?} has an empty key"
                    ));
                }
                if children.iter().any(|(seen, _)| seen == child_key) {
                    return Err(format!("duplicate map key {child_key:?} under {key:?}"));
                }
                if child_value.is_empty() {
                    return Err(format!(
                        "map entry {child_key:?} under {key:?} has no scalar value — \
                         nesting beyond one level is outside the conforming subset"
                    ));
                }
                if child_value == ">" || child_value == "|" {
                    return Err(format!(
                        "map entry {child_key:?} under {key:?} uses a block scalar — \
                         map values are plain scalars in the conforming subset"
                    ));
                }
                if child_value.starts_with('{') || child_value.starts_with('[') {
                    return Err(format!(
                        "map entry {child_key:?} under {key:?} uses flow syntax — \
                         outside the conforming subset"
                    ));
                }
                children.push((child_key.to_string(), strip_quotes(child_value)));
                i += 1;
            }
            let value = if children.is_empty() {
                FrontmatterValue::Scalar(String::new())
            } else {
                FrontmatterValue::Map(children)
            };
            entries.push((key.to_string(), value));
            continue;
        }

        if raw_value.starts_with('{') || raw_value.starts_with('[') {
            return Err(format!(
                "{key:?} uses flow syntax — outside the conforming subset"
            ));
        }
        entries.push((
            key.to_string(),
            FrontmatterValue::Scalar(strip_quotes(raw_value)),
        ));
        i += 1;
    }

    Ok(entries)
}

/// Strip one matching pair of surrounding quotes (the lenient lane's rule).
fn strip_quotes(v: &str) -> String {
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Parse flat YAML-like frontmatter into a SkillConfig.
///
/// Supports simple key: value pairs, `>` block scalars for multi-line strings,
/// and boolean values (true/false). This avoids a serde_yaml dependency for
/// what is deliberately a minimal format.
fn parse_frontmatter(yaml: &str) -> Result<SkillConfig, String> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut autonomy: Option<String> = None;
    let mut disable_auto_invocation = false;
    let mut sandbox: Option<bool> = None;
    let mut compatibility: Option<String> = None;

    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Skip blank lines and comments
        if line.trim().is_empty() || line.trim().starts_with('#') {
            i += 1;
            continue;
        }

        // Must be a key: value line
        let Some(colon_pos) = line.find(':') else {
            i += 1;
            continue;
        };

        let key = line[..colon_pos].trim();
        let raw_value = line[colon_pos + 1..].trim();

        // Handle block scalar (key: > or key: |)
        let value = if raw_value == ">" || raw_value == "|" {
            // Collect indented continuation lines
            let mut parts = Vec::new();
            i += 1;
            while i < lines.len() {
                let cont = lines[i];
                if cont.is_empty() || cont.starts_with(' ') || cont.starts_with('\t') {
                    parts.push(cont.trim());
                    i += 1;
                } else {
                    break;
                }
            }
            parts.join(if raw_value == ">" { " " } else { "\n" })
        } else {
            i += 1;
            // Strip surrounding quotes
            let v = raw_value.trim();
            if (v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\''))
            {
                v[1..v.len() - 1].to_string()
            } else {
                v.to_string()
            }
        };

        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "autonomy" => autonomy = Some(value),
            "disable-auto-invocation" | "disable_auto_invocation" => {
                disable_auto_invocation = value == "true";
            }
            "sandbox" => sandbox = Some(value == "true"),
            "compatibility" => compatibility = Some(value),
            _ => {} // Ignore unknown fields for forward compatibility
        }
    }

    let name = name.ok_or("missing required field: name")?;
    let description = description.ok_or("missing required field: description")?;

    Ok(SkillConfig {
        name,
        description,
        autonomy,
        disable_auto_invocation,
        sandbox,
        compatibility: compatibility.filter(|c| !c.is_empty()),
    })
}

/// Discover skills from project and personal directories.
///
/// Project skills take precedence over personal skills with the same name.
/// Scans the Agent Skills standard path (`.agents/skills/`) first.
pub fn discover_skills(project_root: Option<&Path>) -> Vec<Skill> {
    discover_skills_in(project_root, dirs::home_dir().as_deref())
}

/// Home-injectable core of [`discover_skills`]. Tests pin `home` (usually to
/// `None` or a temp dir) so personal skills on the running machine leak into
/// no assertion — the self-hosted CI runners share a real `$HOME` that
/// legitimately contains `~/.agents/skills/`.
pub fn discover_skills_in(project_root: Option<&Path>, home: Option<&Path>) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // 1. Project-scoped skills.
    if let Some(root) = project_root {
        // Standard: .agents/skills/
        load_skills_from_dir(
            &root.join(".agents").join("skills"),
            SkillSource::Project,
            &mut skills,
            &mut seen_names,
        );
        // Visible project path: skills/ (this repo's own skills/ ships
        // through this one). Directories without a parseable SKILL.md are
        // skipped, so unrelated skills/ folders in other projects are
        // harmless.
        load_skills_from_dir(
            &root.join("skills"),
            SkillSource::Project,
            &mut skills,
            &mut seen_names,
        );
    }

    // 2. Personal skills.
    if let Some(home) = home {
        load_skills_from_dir(
            &home.join(".agents").join("skills"),
            SkillSource::Personal,
            &mut skills,
            &mut seen_names,
        );
    }

    skills
}

fn load_skills_from_dir(
    skills_dir: &Path,
    source: SkillSource,
    skills: &mut Vec<Skill>,
    seen_names: &mut std::collections::HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            eprintln!("Failed to read {}", skill_md.display());
            continue;
        };

        match parse_skill_md(&content, &skill_md) {
            Ok((config, body)) => {
                if seen_names.contains(&config.name) {
                    // Project skills take precedence
                    continue;
                }
                seen_names.insert(config.name.clone());
                skills.push(Skill {
                    config,
                    body,
                    source_path: skill_md,
                    source: source.clone(),
                });
            }
            Err(e) => {
                eprintln!("Skipping skill: {}", e);
            }
        }
    }
}

/// Format a skill catalog for injection into the system prompt / conversation.
///
/// Returns empty string if no skills are available.
pub fn format_skill_catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let auto_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.config.disable_auto_invocation)
        .collect();

    let manual_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| s.config.disable_auto_invocation)
        .collect();

    let mut out = String::from("## Available Skills\n\n");
    out.push_str("You can invoke skills using the `invoke_skill` tool.\n\n");

    if !auto_skills.is_empty() {
        out.push_str("**Auto-invocable** (use when the task matches):\n");
        for s in &auto_skills {
            push_catalog_line(&mut out, s);
        }
        out.push('\n');
    }

    if !manual_skills.is_empty() {
        out.push_str("**Manual only** (only invoke when explicitly requested):\n");
        for s in &manual_skills {
            push_catalog_line(&mut out, s);
        }
        out.push('\n');
    }

    out
}

/// One catalog entry. `compatibility` rides after the description so the
/// model can rule a skill out before invoking it (the field's intent per
/// the Agent Skills standard — description stays purpose-only).
fn push_catalog_line(out: &mut String, s: &Skill) {
    out.push_str(&format!(
        "- **{}**: {}",
        s.config.name, s.config.description
    ));
    if let Some(compat) = s.config.compatibility.as_deref() {
        out.push_str(&format!(" (compatibility: {compat})"));
    }
    out.push('\n');
}

/// Load a skill body with `$ARGUMENTS` substitution.
pub fn load_skill_body(skill: &Skill, arguments: &str) -> String {
    skill.body.replace("$ARGUMENTS", arguments)
}

/// Resolve a skill invocation into a full task string with embedded instructions.
///
/// Used by control socket / TUI / presence to convert an `InvokeSkill` message
/// into a task the agent loop can process directly.
pub fn resolve_skill_as_task(
    skills: &[Skill],
    skill_name: &str,
    arguments: &str,
) -> Result<String, String> {
    let skill = skills
        .iter()
        .find(|s| s.config.name == skill_name)
        .ok_or_else(|| {
            format!(
                "Skill '{}' not found. Available: {}",
                skill_name,
                skills
                    .iter()
                    .map(|s| s.config.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let body = load_skill_body(skill, arguments);
    Ok(format!(
        "[Skill: {}]\n\nFollow these instructions:\n\n{}",
        skill_name, body
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_SKILL: &str = r#"---
name: deploy-staging
description: Deploy the current branch to staging
autonomy: high
disable-auto-invocation: false
sandbox: true
---

Run the deploy script:
```bash
./scripts/deploy.sh $ARGUMENTS
```
"#;

    const MINIMAL_SKILL: &str = r#"---
name: lint
description: Run linting on changed files
---

Run `cargo clippy` on all targets.
"#;

    const MULTILINE_DESC: &str = r#"---
name: complex-deploy
description: >
  Deploy the current branch to the staging environment
  with full integration test suite
autonomy: low
---

Instructions here.
"#;

    #[test]
    fn parse_full_frontmatter() {
        let (config, body) = parse_skill_md(FULL_SKILL, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "deploy-staging");
        assert_eq!(config.description, "Deploy the current branch to staging");
        assert_eq!(config.autonomy, Some("high".to_string()));
        assert!(!config.disable_auto_invocation);
        assert_eq!(config.sandbox, Some(true));
        assert!(body.contains("deploy.sh"));
    }

    #[test]
    fn parse_minimal_frontmatter() {
        let (config, body) = parse_skill_md(MINIMAL_SKILL, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "lint");
        assert_eq!(config.description, "Run linting on changed files");
        assert!(config.autonomy.is_none());
        assert!(!config.disable_auto_invocation);
        assert!(config.sandbox.is_none());
        assert!(body.contains("cargo clippy"));
    }

    #[test]
    fn parse_multiline_description() {
        let (config, _body) = parse_skill_md(MULTILINE_DESC, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "complex-deploy");
        assert!(config.description.contains("Deploy the current branch"));
        assert!(config.description.contains("integration test suite"));
        assert_eq!(config.autonomy, Some("low".to_string()));
    }

    #[test]
    fn parse_compatibility() {
        let content = "---\nname: caller\ndescription: Operate the daemon\ncompatibility: >\n  Requires a reachable Intendant daemon.\n  Supervised sessions have $INTENDANT injected.\n---\nbody\n";
        let (config, _) = parse_skill_md(content, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(
            config.compatibility.as_deref(),
            Some("Requires a reachable Intendant daemon. Supervised sessions have $INTENDANT injected.")
        );

        // Absent / empty fields normalize to None.
        let (config, _) = parse_skill_md(
            "---\nname: a\ndescription: b\ncompatibility:\n---\nbody\n",
            Path::new("test/SKILL.md"),
        )
        .unwrap();
        assert_eq!(config.compatibility, None);
    }

    #[test]
    fn catalog_surfaces_compatibility_after_description() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("skills").join("gated");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: gated\ndescription: Do the thing.\ncompatibility: macOS only.\n---\nbody\n",
        )
        .unwrap();
        let plain = root.join("skills").join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(
            plain.join("SKILL.md"),
            "---\nname: plain\ndescription: No requirements.\n---\nbody\n",
        )
        .unwrap();

        let skills = discover_skills_in(Some(root), None);
        let catalog = format_skill_catalog(&skills);
        assert!(
            catalog.contains("- **gated**: Do the thing. (compatibility: macOS only.)"),
            "{catalog}"
        );
        assert!(
            catalog.contains("- **plain**: No requirements.\n"),
            "{catalog}"
        );
    }

    #[test]
    fn parse_missing_frontmatter() {
        let content = "# Just a markdown file\n\nNo frontmatter here.";
        let result = parse_skill_md(content, Path::new("test/SKILL.md"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing YAML frontmatter"));
    }

    #[test]
    fn parse_unterminated_frontmatter() {
        let content = "---\nname: broken\ndescription: no closing\n";
        let result = parse_skill_md(content, Path::new("test/SKILL.md"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unterminated"));
    }

    #[test]
    fn parse_missing_required_fields() {
        let content = "---\nname: only-name\n---\nBody.";
        let result = parse_skill_md(content, Path::new("test/SKILL.md"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("description"));
    }

    #[test]
    fn discover_skills_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = discover_skills_in(Some(tmp.path()), None);
        assert!(skills.is_empty());
    }

    #[test]
    fn discover_skills_standard_path() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join(".agents").join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), MINIMAL_SKILL).unwrap();

        let skills = discover_skills_in(Some(tmp.path()), None);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].config.name, "lint");
        assert_eq!(skills[0].source, SkillSource::Project);
    }

    #[test]
    fn discover_skills_visible_project_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), MINIMAL_SKILL).unwrap();

        // The documented `<project_root>/skills/` path loads (this repo's
        // own skills/ directory ships through it).
        let skills = discover_skills_in(Some(tmp.path()), None);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].config.name, "lint");
        assert_eq!(skills[0].source, SkillSource::Project);

        // Dotted paths still win over it for the same name.
        let standard_dir = tmp.path().join(".agents").join("skills").join("lint");
        std::fs::create_dir_all(&standard_dir).unwrap();
        std::fs::write(standard_dir.join("SKILL.md"), MINIMAL_SKILL).unwrap();
        let skills = discover_skills_in(Some(tmp.path()), None);
        assert_eq!(skills.len(), 1);
        assert!(skills[0].source_path.to_string_lossy().contains(".agents"));
    }

    #[test]
    fn discover_skills_ignores_removed_intendant_specific_paths() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for skill_dir in [
            project.path().join(".intendant/skills/project-skill"),
            home.path().join(".intendant/skills/personal-skill"),
        ] {
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), MINIMAL_SKILL).unwrap();
        }

        assert!(discover_skills_in(Some(project.path()), Some(home.path())).is_empty());
    }

    #[test]
    fn discover_skills_personal_home_and_project_precedence() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        // A personal skill in the injected home's standard path loads.
        let personal = home.path().join(".agents").join("skills").join("my-skill");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::write(personal.join("SKILL.md"), MINIMAL_SKILL).unwrap();
        let skills = discover_skills_in(Some(project.path()), Some(home.path()));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].config.name, "lint");
        assert_eq!(skills[0].source, SkillSource::Personal);

        // A project skill with the same name shadows the personal one.
        let project_skill = project.path().join(".agents").join("skills").join("lint");
        std::fs::create_dir_all(&project_skill).unwrap();
        std::fs::write(project_skill.join("SKILL.md"), MINIMAL_SKILL).unwrap();
        let skills = discover_skills_in(Some(project.path()), Some(home.path()));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source, SkillSource::Project);
    }

    #[test]
    fn format_catalog_empty() {
        assert_eq!(format_skill_catalog(&[]), "");
    }

    #[test]
    fn format_catalog_multiple() {
        let skills = vec![
            Skill {
                config: SkillConfig {
                    name: "deploy".to_string(),
                    description: "Deploy to staging".to_string(),
                    autonomy: None,
                    disable_auto_invocation: false,
                    sandbox: None,
                    compatibility: None,
                },
                body: String::new(),
                source_path: PathBuf::new(),
                source: SkillSource::Project,
            },
            Skill {
                config: SkillConfig {
                    name: "test-e2e".to_string(),
                    description: "Run E2E tests".to_string(),
                    autonomy: None,
                    disable_auto_invocation: true,
                    sandbox: None,
                    compatibility: None,
                },
                body: String::new(),
                source_path: PathBuf::new(),
                source: SkillSource::Personal,
            },
        ];

        let catalog = format_skill_catalog(&skills);
        assert!(catalog.contains("deploy"));
        assert!(catalog.contains("test-e2e"));
        assert!(catalog.contains("Auto-invocable"));
        assert!(catalog.contains("Manual only"));
    }

    #[test]
    fn argument_substitution() {
        let skill = Skill {
            config: SkillConfig {
                name: "deploy".to_string(),
                description: "Deploy".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
                compatibility: None,
            },
            body: "Deploy $ARGUMENTS to staging.".to_string(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        };

        assert_eq!(
            load_skill_body(&skill, "production"),
            "Deploy production to staging."
        );
    }

    #[test]
    fn argument_substitution_no_placeholder() {
        let skill = Skill {
            config: SkillConfig {
                name: "lint".to_string(),
                description: "Lint".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
                compatibility: None,
            },
            body: "Just run clippy.".to_string(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        };

        assert_eq!(load_skill_body(&skill, "ignored"), "Just run clippy.");
    }

    #[test]
    fn resolve_skill_found() {
        let skills = vec![Skill {
            config: SkillConfig {
                name: "deploy".to_string(),
                description: "Deploy".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
                compatibility: None,
            },
            body: "Deploy $ARGUMENTS now.".to_string(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        }];

        let result = resolve_skill_as_task(&skills, "deploy", "staging");
        assert!(result.is_ok());
        let task = result.unwrap();
        assert!(task.contains("[Skill: deploy]"));
        assert!(task.contains("Deploy staging now."));
    }

    #[test]
    fn resolve_skill_not_found() {
        let skills = vec![Skill {
            config: SkillConfig {
                name: "deploy".to_string(),
                description: "Deploy".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
                compatibility: None,
            },
            body: String::new(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        }];

        let result = resolve_skill_as_task(&skills, "nonexistent", "");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn parse_quoted_values() {
        let content = "---\nname: \"quoted-name\"\ndescription: 'single quoted'\n---\nBody.";
        let (config, _) = parse_skill_md(content, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "quoted-name");
        assert_eq!(config.description, "single quoted");
    }

    // ---- The strict conforming-subset lane ----

    fn strict(yaml: &str) -> Result<Vec<(String, FrontmatterValue)>, String> {
        parse_frontmatter_strict(yaml)
    }

    #[test]
    fn strict_parses_scalars_and_one_level_map() {
        let entries = strict(
            "name: fix-task\ndescription: \"a workflow\"\nmetadata:\n  title: Fix-task workflow\n  team: 'house'\nlicense: MIT\n",
        )
        .unwrap();
        assert_eq!(
            entries,
            vec![
                (
                    "name".to_string(),
                    FrontmatterValue::Scalar("fix-task".to_string())
                ),
                (
                    "description".to_string(),
                    FrontmatterValue::Scalar("a workflow".to_string())
                ),
                (
                    "metadata".to_string(),
                    FrontmatterValue::Map(vec![
                        ("title".to_string(), "Fix-task workflow".to_string()),
                        ("team".to_string(), "house".to_string()),
                    ])
                ),
                (
                    "license".to_string(),
                    FrontmatterValue::Scalar("MIT".to_string())
                ),
            ]
        );
    }

    #[test]
    fn strict_supports_block_scalars_and_comments_like_the_lenient_lane() {
        let entries =
            strict("# a comment\ndescription: >\n  spans two\n  lines\nname: x\n").unwrap();
        assert_eq!(
            entries[0],
            (
                "description".to_string(),
                FrontmatterValue::Scalar("spans two lines".to_string())
            )
        );
        assert_eq!(
            entries[1],
            (
                "name".to_string(),
                FrontmatterValue::Scalar("x".to_string())
            )
        );
        // Empty-valued key with no children is an empty scalar, the
        // lenient lane's `compatibility:` shape.
        let entries = strict("license:\nname: x\n").unwrap();
        assert_eq!(
            entries[0],
            (
                "license".to_string(),
                FrontmatterValue::Scalar(String::new())
            )
        );
    }

    #[test]
    fn strict_refuses_junk_duplicates_and_flow_syntax_by_name() {
        let junk = strict("name: x\njust some prose\n").unwrap_err();
        assert!(junk.contains("not a `key: value` line"), "{junk}");

        let dup = strict("name: x\nname: y\n").unwrap_err();
        assert!(dup.contains("duplicate frontmatter key \"name\""), "{dup}");

        let flow = strict("metadata: {a: b}\n").unwrap_err();
        assert!(flow.contains("flow syntax"), "{flow}");

        let list = strict("tags:\n  - gate\n").unwrap_err();
        assert!(list.contains("not a `key: value` line"), "{list}");

        let orphan = strict("  indented: first\n").unwrap_err();
        assert!(orphan.contains("unexpected indented line"), "{orphan}");
    }

    #[test]
    fn strict_refuses_nesting_beyond_one_level() {
        let nested = strict("metadata:\n  inner:\n    deep: value\n").unwrap_err();
        assert!(nested.contains("nesting beyond one level"), "{nested}");
        let dup = strict("metadata:\n  a: x\n  a: y\n").unwrap_err();
        assert!(dup.contains("duplicate map key \"a\""), "{dup}");
        let block = strict("metadata:\n  a: >\n    text\n").unwrap_err();
        assert!(block.contains("block scalar"), "{block}");
    }

    #[test]
    fn strict_allows_blank_lines_inside_a_map_run() {
        let entries = strict("metadata:\n  a: x\n\n  b: y\nname: z\n").unwrap();
        assert_eq!(
            entries[0],
            (
                "metadata".to_string(),
                FrontmatterValue::Map(vec![
                    ("a".to_string(), "x".to_string()),
                    ("b".to_string(), "y".to_string()),
                ])
            )
        );
    }

    #[test]
    fn split_frontmatter_matches_the_lenient_split() {
        let (yaml, body) = split_frontmatter(FULL_SKILL, Path::new("t/SKILL.md")).unwrap();
        assert!(yaml.contains("name: deploy-staging"));
        assert!(body.starts_with("Run the deploy script:"));
        assert!(split_frontmatter("no frontmatter", Path::new("t/SKILL.md")).is_err());
        assert!(split_frontmatter("---\nname: x\n", Path::new("t/SKILL.md")).is_err());
    }
}
