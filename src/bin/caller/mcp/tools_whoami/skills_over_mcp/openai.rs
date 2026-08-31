use super::*;
use std::collections::HashSet;

pub(super) const AGGREGATE_NAME: &str = "intendant-skills";
pub(super) const MAX_PACKAGES: usize = 5;
pub(super) const RESOURCE_LIMIT: usize = 100;
const SKILL_MD_LIMIT: usize = 256 * 1024;
const SUPPORT_FILE_LIMIT: usize = 1024 * 1024;
const SKILL_TOTAL_LIMIT: usize = 5 * 1024 * 1024;
const ROOT_RESERVE_BYTES: usize = 64 * 1024;
const GLOBAL_CATALOG_RESERVE_BYTES: usize = SUPPORT_FILE_LIMIT;
const GLOBAL_CATALOG_PATH: &str = "references/catalog.md";

#[derive(Debug)]
struct PreparedSkill {
    name: String,
    description: String,
    instruction_relative_path: String,
    resources: Vec<PreparedResource>,
}

#[derive(Debug)]
struct PreparedResource {
    relative_path: String,
    bytes: Vec<u8>,
    mime_type: String,
}

impl PreparedSkill {
    fn bytes_len(&self) -> usize {
        self.resources
            .iter()
            .map(|resource| resource.bytes.len())
            .sum()
    }
}

/// OpenAI currently imports at most five named skills. Use those slots as
/// packages for the complete effective catalog. Each source skill remains
/// atomic inside one package and keeps its directory-relative file tree under
/// `references/skills/<name>/`; the ordinary Skills Over MCP catalog remains
/// one entry per skill with no Intendant count cap.
pub(super) fn aggregate_skills(effective: &[ServedSkill]) -> Result<Vec<ServedSkill>, String> {
    let prepared = effective
        .iter()
        .map(prepare_skill)
        .collect::<Result<Vec<_>, _>>()?;
    let groups = partition(prepared)?;
    let package_names = (0..groups.len()).map(package_name).collect::<Vec<_>>();
    let global_catalog = global_catalog(&groups, &package_names);
    if global_catalog.len() > SUPPORT_FILE_LIMIT {
        return Err(format!(
            "the OpenAI aggregate catalog is {} bytes, above the 1 MiB \
             supporting-file limit; the catalog was not truncated",
            global_catalog.len()
        ));
    }

    groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            build_package(
                index,
                group,
                &groups,
                &package_names,
                (index == 0).then_some(global_catalog.as_str()),
            )
        })
        .collect()
}

fn prepare_skill(skill: &ServedSkill) -> Result<PreparedSkill, String> {
    let description = skill
        .frontmatter
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let root = format!("references/skills/{}", skill.name);
    let instruction_relative_path = format!("{root}/SKILL.md");
    let mut resources = Vec::with_capacity(skill.resources.len());

    for resource in &skill.resources {
        let relative_path = if resource.relative_path == "SKILL.md" {
            instruction_relative_path.clone()
        } else {
            format!("{root}/{}", resource.relative_path)
        };
        catalog::validate_resource_path(&relative_path)?;
        if resource.bytes.len() > SUPPORT_FILE_LIMIT {
            return Err(format!(
                "OpenAI package resource {relative_path:?} for skill {:?} is {} bytes, \
                 above the 1 MiB supporting-file limit; the skill was not truncated",
                skill.name,
                resource.bytes.len()
            ));
        }
        resources.push(PreparedResource {
            relative_path,
            bytes: resource.bytes.clone(),
            mime_type: resource.mime_type.clone(),
        });
    }

    Ok(PreparedSkill {
        name: skill.name.clone(),
        description,
        instruction_relative_path,
        resources,
    })
}

fn partition(prepared: Vec<PreparedSkill>) -> Result<Vec<Vec<PreparedSkill>>, String> {
    let mut groups: Vec<Vec<PreparedSkill>> = vec![Vec::new()];
    let mut resources_in_group = 2usize;
    let mut bytes_in_group = ROOT_RESERVE_BYTES + GLOBAL_CATALOG_RESERVE_BYTES;

    for skill in prepared {
        let skill_resources = skill.resources.len();
        let skill_bytes = skill.bytes_len();
        if skill_resources.saturating_add(1) > RESOURCE_LIMIT {
            return Err(format!(
                "skill {:?} needs {} OpenAI package resources by itself, above the \
                 {}-file limit; the catalog was not truncated",
                skill.name,
                skill_resources + 1,
                RESOURCE_LIMIT
            ));
        }
        if skill_bytes.saturating_add(ROOT_RESERVE_BYTES) > SKILL_TOTAL_LIMIT {
            return Err(format!(
                "skill {:?} needs {skill_bytes} supporting bytes in the OpenAI \
                 projection, leaving no room under the 5 MiB package limit; the \
                 catalog was not truncated",
                skill.name
            ));
        }

        let needs_new_group = resources_in_group.saturating_add(skill_resources) > RESOURCE_LIMIT
            || bytes_in_group.saturating_add(skill_bytes) > SKILL_TOTAL_LIMIT;
        if needs_new_group {
            if groups.len() == MAX_PACKAGES {
                return Err(format!(
                    "the complete effective catalog needs more than OpenAI's \
                     {MAX_PACKAGES} named skill packages; the catalog was not truncated"
                ));
            }
            groups.push(Vec::new());
            resources_in_group = 1;
            bytes_in_group = ROOT_RESERVE_BYTES;
        }
        resources_in_group += skill_resources;
        bytes_in_group += skill_bytes;
        groups.last_mut().expect("one group exists").push(skill);
    }

    Ok(groups)
}

fn build_package(
    index: usize,
    group: &[PreparedSkill],
    all_groups: &[Vec<PreparedSkill>],
    package_names: &[String],
    catalog: Option<&str>,
) -> Result<ServedSkill, String> {
    let name = package_names[index].clone();
    let description = package_description(index, package_names.len(), group);
    let frontmatter = Map::from_iter([
        ("name".to_string(), Value::String(name.clone())),
        ("description".to_string(), Value::String(description)),
    ]);
    let root_md = document_from_frontmatter(
        &frontmatter,
        &package_body(index, group, all_groups, package_names),
    )?;
    if root_md.len() > SKILL_MD_LIMIT {
        return Err(format!(
            "OpenAI package {name:?} has a {}-byte SKILL.md, above the 256 KiB \
             limit; the catalog was not truncated",
            root_md.len()
        ));
    }

    let uri = catalog::skill_uri(&name, "SKILL.md");
    let mut resources = vec![ServedResource {
        uri: uri.clone(),
        relative_path: "SKILL.md".to_string(),
        bytes: root_md.into_bytes(),
        mime_type: "text/markdown".to_string(),
    }];
    if let Some(catalog) = catalog {
        resources.push(ServedResource {
            uri: catalog::skill_uri(&name, GLOBAL_CATALOG_PATH),
            relative_path: GLOBAL_CATALOG_PATH.to_string(),
            bytes: catalog.as_bytes().to_vec(),
            mime_type: "text/markdown".to_string(),
        });
    }
    for skill in group {
        for resource in &skill.resources {
            resources.push(ServedResource {
                uri: catalog::skill_uri(&name, &resource.relative_path),
                relative_path: resource.relative_path.clone(),
                bytes: resource.bytes.clone(),
                mime_type: resource.mime_type.clone(),
            });
        }
    }

    validate_package(&name, &resources)?;
    Ok(ServedSkill {
        name,
        uri,
        frontmatter,
        resources,
    })
}

fn validate_package(name: &str, resources: &[ServedResource]) -> Result<(), String> {
    if resources.len() > RESOURCE_LIMIT {
        return Err(format!(
            "OpenAI package {name:?} needs {} resources, above the {RESOURCE_LIMIT}-file \
             limit; the catalog was not truncated",
            resources.len()
        ));
    }
    let total: usize = resources.iter().map(|resource| resource.bytes.len()).sum();
    if total > SKILL_TOTAL_LIMIT {
        return Err(format!(
            "OpenAI package {name:?} is {total} bytes, above the 5 MiB per-skill \
             limit; the catalog was not truncated"
        ));
    }
    let mut seen = HashSet::new();
    for resource in resources {
        let normalized = resource.relative_path.to_ascii_lowercase();
        if !seen.insert(normalized) {
            return Err(format!(
                "OpenAI package {name:?} contains a normalization-conflicting path {:?}",
                resource.relative_path
            ));
        }
    }
    Ok(())
}

fn package_name(index: usize) -> String {
    if index == 0 {
        AGGREGATE_NAME.to_string()
    } else {
        format!("{AGGREGATE_NAME}-{}", index + 1)
    }
}

fn package_description(index: usize, total: usize, group: &[PreparedSkill]) -> String {
    if group.is_empty() {
        return format!(
            "Use for any Intendant daemon task. Live owner-approved skill catalog router \
             for {total} imported packages; load it to locate and follow the matching \
             workflow."
        );
    }
    let names = group
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    truncate_chars(
        format!(
            "Use for Intendant daemon work. Live owner-approved workflow package {} of \
             {total}; contains: {names}. Load it when any listed workflow matches the \
             task, then follow the selected nested SKILL.md instructions.",
            index + 1
        ),
        1024,
    )
}

fn truncate_chars(value: String, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value;
    }
    let mut truncated = value
        .chars()
        .take(limit.saturating_sub(3))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

fn package_body(
    index: usize,
    group: &[PreparedSkill],
    all_groups: &[Vec<PreparedSkill>],
    package_names: &[String],
) -> String {
    let mut body = format!(
        "# Intendant skills package {} of {}\n\n\
         This imported package is one projection of Intendant's live effective skill \
         catalog. It exists only to fit OpenAI's current five-named-skill intake limit; \
         the source workflows remain distinct.\n\n",
        index + 1,
        package_names.len()
    );
    if index == 0 {
        body.push_str(
            "Read `references/catalog.md` to select the matching workflow and package. \
             If the selected workflow belongs to another package, load that imported \
             skill by name.\n\n",
        );
    } else {
        body.push_str(
            "The `intendant-skills` package carries the full cross-package catalog.\n\n",
        );
    }
    body.push_str(
        "After selecting a workflow in this package, read its nested `SKILL.md` below \
         and follow it as the operative instructions. Its sibling support files retain \
         their original relative paths. Treat nested frontmatter as source metadata, \
         not as permission to activate additional nested skills. Use the MCP `help` \
         tool for command syntax.\n\n## Packages\n\n",
    );
    for (package_index, skills) in all_groups.iter().enumerate() {
        body.push_str(&format!(
            "- `{}` — {} workflow{}\n",
            package_names[package_index],
            skills.len(),
            if skills.len() == 1 { "" } else { "s" }
        ));
    }
    body.push_str("\n## Workflows in this package\n\n");
    if group.is_empty() {
        body.push_str("This package is the router; its catalog points to the payload packages.\n");
    } else {
        for skill in group {
            body.push_str(&format!(
                "- **{}** — read `{}`\n",
                skill.name, skill.instruction_relative_path
            ));
        }
    }
    body
}

fn global_catalog(groups: &[Vec<PreparedSkill>], package_names: &[String]) -> String {
    let mut catalog = String::from(
        "# Effective Intendant skill catalog\n\n\
         Choose every workflow whose description matches the user's task. Load the \
         named imported package, then read the listed nested `SKILL.md`.\n\n",
    );
    for (index, group) in groups.iter().enumerate() {
        catalog.push_str(&format!("## `{}`\n\n", package_names[index]));
        for skill in group {
            catalog.push_str(&format!(
                "- **{}** — {}\n  - instructions: `{}`\n",
                skill.name, skill.description, skill.instruction_relative_path
            ));
        }
        catalog.push('\n');
    }
    catalog
}

fn document_from_frontmatter(
    frontmatter: &Map<String, Value>,
    body: &str,
) -> Result<String, String> {
    let mut document = String::from("---\n");
    for (key, value) in frontmatter {
        catalog::write_yaml_entry(&mut document, key, value)?;
    }
    document.push_str("---\n");
    document.push_str(body);
    if !document.ends_with('\n') {
        document.push('\n');
    }
    Ok(document)
}
