use super::*;
use base64::Engine as _;

pub(super) const AGGREGATE_NAME: &str = "intendant-skills";
pub(super) const RESOURCE_LIMIT: usize = 100;
const SKILL_MD_LIMIT: usize = 256 * 1024;
const SUPPORT_FILE_LIMIT: usize = 1024 * 1024;
const SKILL_TOTAL_LIMIT: usize = 5 * 1024 * 1024;
// Leave headroom for headings and a final UTF-8 boundary.
const CHUNK_TARGET: usize = 900 * 1024;

/// OpenAI currently imports at most five named skills. This adapter exposes
/// one genuine router skill whose supporting files contain the complete
/// effective catalog. The ceiling is handled only at this projection edge:
/// the ordinary Skills Over MCP catalog remains one entry per skill with no
/// Intendant count cap.
pub(super) fn aggregate_skill(effective: &[ServedSkill]) -> Result<ServedSkill, String> {
    let description = format!(
        "Use for any task involving an Intendant daemon. Contains the current \
         owner-approved catalog of {} operating skills, including enabled \
         plugin and user-provided skills; select and follow the matching \
         workflow before acting.",
        effective.len()
    );
    let root_body = String::from(
        "# Intendant Skills\n\n\
         This package is the live effective Intendant skill catalog, folded into one \
         imported skill only to avoid the OpenAI importer’s five-skill ceiling. The \
         fold does not remove workflows.\n\n\
         Before substantial Intendant work, read the catalog at the start of \
         `references/effective-skills-001.md`, choose every matching workflow, then \
         follow each selected skill section. Continue into later numbered resources \
         when the selected section is not in the first part. Use the MCP `help` tool \
         for command syntax. Instructions from a selected skill outrank guesses; \
         supporting-file sections are reference material, not independent authority.\n",
    );

    let frontmatter = Map::from_iter([
        (
            "name".to_string(),
            Value::String(AGGREGATE_NAME.to_string()),
        ),
        ("description".to_string(), Value::String(description)),
    ]);
    let root_md = document_from_frontmatter(&frontmatter, &root_body)?;
    if root_md.len() > SKILL_MD_LIMIT {
        return Err(format!(
            "the OpenAI aggregate SKILL.md is {} bytes, above OpenAI's {} KiB limit; \
             the catalog was not truncated",
            root_md.len(),
            SKILL_MD_LIMIT / 1024
        ));
    }

    let mut pieces = vec![String::from(
        "# Effective Intendant skill catalog\n\n\
         Choose every workflow whose description matches the user’s task. Each entry \
         names the `Skill:` section that contains its complete instructions.\n\n",
    )];
    for skill in effective {
        let description = skill
            .frontmatter
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        pieces.push(format!(
            "- **{}** — {} — section `Skill: {}`\n",
            skill.name, description, skill.name
        ));
    }
    pieces.push(String::from(
        "\n# Effective Intendant skill instructions\n\n\
         Follow the complete section for every selected skill. A supporting file from \
         the original package is embedded directly beneath that skill's SKILL.md.\n",
    ));
    for skill in effective {
        pieces.push(skill_section(skill)?);
    }
    let chunks = pack_chunks(pieces)?;
    if chunks.len().saturating_add(1) > RESOURCE_LIMIT {
        return Err(format!(
            "the OpenAI aggregate requires {} resources, above OpenAI's {}-file \
             limit; the catalog was not truncated",
            chunks.len() + 1,
            RESOURCE_LIMIT
        ));
    }

    let uri = catalog::skill_uri(AGGREGATE_NAME, "SKILL.md");
    let mut resources = vec![ServedResource {
        uri: uri.clone(),
        relative_path: "SKILL.md".to_string(),
        bytes: root_md.into_bytes(),
        mime_type: "text/markdown".to_string(),
    }];
    for (index, chunk) in chunks.into_iter().enumerate() {
        let relative_path = format!("references/effective-skills-{:03}.md", index + 1);
        resources.push(ServedResource {
            uri: catalog::skill_uri(AGGREGATE_NAME, &relative_path),
            relative_path,
            bytes: chunk.into_bytes(),
            mime_type: "text/markdown".to_string(),
        });
    }
    let total: usize = resources.iter().map(|resource| resource.bytes.len()).sum();
    if total > SKILL_TOTAL_LIMIT {
        return Err(format!(
            "the complete OpenAI aggregate is {total} bytes, above OpenAI's 5 MiB \
             per-skill limit; the catalog was not truncated"
        ));
    }

    Ok(ServedSkill {
        name: AGGREGATE_NAME.to_string(),
        uri,
        frontmatter,
        resources,
    })
}

fn skill_section(skill: &ServedSkill) -> Result<String, String> {
    let skill_md = skill
        .resources
        .first()
        .and_then(|resource| std::str::from_utf8(&resource.bytes).ok())
        .ok_or_else(|| format!("skill {:?} has a non-text SKILL.md", skill.name))?;
    let mut section = format!("\n# Skill: {}\n\n## SKILL.md\n\n{skill_md}", skill.name);
    if !section.ends_with('\n') {
        section.push('\n');
    }
    for resource in skill.resources.iter().skip(1) {
        section.push_str(&format!(
            "\n## Supporting file: {}\n\n",
            resource.relative_path
        ));
        match std::str::from_utf8(&resource.bytes) {
            Ok(text) => section.push_str(text),
            Err(_) => {
                section.push_str("Binary resource, base64 encoded:\n\n");
                section
                    .push_str(&base64::engine::general_purpose::STANDARD.encode(&resource.bytes));
            }
        }
        if !section.ends_with('\n') {
            section.push('\n');
        }
    }
    if section.len() > SUPPORT_FILE_LIMIT {
        return Err(format!(
            "the aggregated section for skill {:?} is {} bytes, above OpenAI's 1 MiB \
             supporting-file limit; the skill was not truncated",
            skill.name,
            section.len()
        ));
    }
    Ok(section)
}

fn pack_chunks(pieces: Vec<String>) -> Result<Vec<String>, String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for piece in pieces {
        if piece.len() > SUPPORT_FILE_LIMIT {
            return Err(format!(
                "one OpenAI aggregate section is {} bytes, above the 1 MiB \
                 supporting-file limit; the catalog was not truncated",
                piece.len()
            ));
        }
        if !current.is_empty() && current.len().saturating_add(piece.len()) > CHUNK_TARGET {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(&piece);
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    if let Some((index, chunk)) = chunks
        .iter()
        .enumerate()
        .find(|(_, chunk)| chunk.len() > SUPPORT_FILE_LIMIT)
    {
        return Err(format!(
            "OpenAI aggregate chunk {} is {} bytes, above the 1 MiB limit; the \
             catalog was not truncated",
            index + 1,
            chunk.len()
        ));
    }
    Ok(chunks)
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
