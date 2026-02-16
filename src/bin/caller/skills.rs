use crate::project::{global_skills_dir, Project};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
}

fn parse_skill(path: &Path) -> Option<Skill> {
    let text = fs::read_to_string(path).ok()?;
    let trimmed = text.trim_start();

    if !trimmed.starts_with("---") {
        return None;
    }

    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    let frontmatter = &after_first[..end];
    let body = after_first[end + 3..].trim();

    let mut name = None;
    let mut description = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().trim_matches('"').to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().trim_matches('"').to_string());
        }
    }

    let name = name.or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
    })?;

    Some(Skill {
        name,
        description: description.unwrap_or_default(),
        content: body.to_string(),
    })
}

fn load_skills_from_dir(dir: &Path) -> Vec<Skill> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            if let Some(skill) = parse_skill(&path) {
                skills.push(skill);
            }
        }
    }
    skills
}

pub fn load_available_skills(project: &Project) -> Vec<Skill> {
    let mut skills_map: HashMap<String, Skill> = HashMap::new();

    // Load global skills first
    for skill in load_skills_from_dir(&global_skills_dir()) {
        skills_map.insert(skill.name.clone(), skill);
    }

    // Project skills override global by name
    for skill in load_skills_from_dir(&project.skills_dir()) {
        skills_map.insert(skill.name.clone(), skill);
    }

    skills_map.into_values().collect()
}

pub fn select_skills(skills: Vec<Skill>, enabled: &[String]) -> Vec<Skill> {
    if enabled.is_empty() {
        return skills;
    }
    skills
        .into_iter()
        .filter(|s| enabled.contains(&s.name))
        .collect()
}

pub fn format_skills_message(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut msg = String::from("[Active Skills]\n\n");
    for skill in skills {
        msg.push_str(&format!("### {}\n", skill.name));
        if !skill.description.is_empty() {
            msg.push_str(&format!("_{}_\n", skill.description));
        }
        msg.push_str(&skill.content);
        msg.push_str("\n\n");
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_with_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(
            &path,
            r#"---
name: rust-conventions
description: Rust coding standards
---
Always use `cargo fmt` before committing.
"#,
        )
        .unwrap();

        let skill = parse_skill(&path).unwrap();
        assert_eq!(skill.name, "rust-conventions");
        assert_eq!(skill.description, "Rust coding standards");
        assert!(skill.content.contains("cargo fmt"));
    }

    #[test]
    fn parse_skill_name_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docker-deploy.md");
        fs::write(
            &path,
            r#"---
description: Docker deployment guide
---
Use multi-stage builds.
"#,
        )
        .unwrap();

        let skill = parse_skill(&path).unwrap();
        assert_eq!(skill.name, "docker-deploy");
        assert_eq!(skill.description, "Docker deployment guide");
    }

    #[test]
    fn parse_skill_no_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(&path, "Just some text without frontmatter.").unwrap();
        assert!(parse_skill(&path).is_none());
    }

    #[test]
    fn parse_skill_incomplete_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(&path, "---\nname: broken\nNo closing delimiter").unwrap();
        assert!(parse_skill(&path).is_none());
    }

    #[test]
    fn load_skills_from_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = load_skills_from_dir(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skills_from_nonexistent_dir() {
        let skills = load_skills_from_dir(Path::new("/nonexistent/path"));
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skills_skips_non_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("test.txt"),
            "---\nname: test\n---\ncontent",
        )
        .unwrap();
        let skills = load_skills_from_dir(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skills_from_dir_with_skills() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.md"),
            "---\nname: alpha\n---\nAlpha content",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.md"),
            "---\nname: beta\n---\nBeta content",
        )
        .unwrap();
        let skills = load_skills_from_dir(dir.path());
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn select_skills_all_when_empty_filter() {
        let skills = vec![
            Skill {
                name: "a".to_string(),
                description: String::new(),
                content: String::new(),
            },
            Skill {
                name: "b".to_string(),
                description: String::new(),
                content: String::new(),
            },
        ];
        let result = select_skills(skills, &[]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn select_skills_filters() {
        let skills = vec![
            Skill {
                name: "a".to_string(),
                description: String::new(),
                content: String::new(),
            },
            Skill {
                name: "b".to_string(),
                description: String::new(),
                content: String::new(),
            },
        ];
        let result = select_skills(skills, &["a".to_string()]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "a");
    }

    #[test]
    fn format_skills_message_empty() {
        assert!(format_skills_message(&[]).is_none());
    }

    #[test]
    fn format_skills_message_with_skills() {
        let skills = vec![Skill {
            name: "rust".to_string(),
            description: "Rust conventions".to_string(),
            content: "Use clippy.".to_string(),
        }];
        let msg = format_skills_message(&skills).unwrap();
        assert!(msg.contains("[Active Skills]"));
        assert!(msg.contains("### rust"));
        assert!(msg.contains("_Rust conventions_"));
        assert!(msg.contains("Use clippy."));
    }

    #[test]
    fn project_skills_override_global() {
        let global_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();

        fs::write(
            global_dir.path().join("test.md"),
            "---\nname: test\n---\nGlobal version",
        )
        .unwrap();
        fs::write(
            project_dir.path().join("test.md"),
            "---\nname: test\n---\nProject version",
        )
        .unwrap();

        let mut skills_map: HashMap<String, Skill> = HashMap::new();
        for skill in load_skills_from_dir(global_dir.path()) {
            skills_map.insert(skill.name.clone(), skill);
        }
        for skill in load_skills_from_dir(project_dir.path()) {
            skills_map.insert(skill.name.clone(), skill);
        }

        let skill = skills_map.get("test").unwrap();
        assert!(skill.content.contains("Project version"));
    }
}
