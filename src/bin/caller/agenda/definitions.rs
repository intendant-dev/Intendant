//! Automation definitions (Track AW): actions and workflows as sealed
//! SKILL.md files, agentskills.io-conforming.
//!
//! A definition is a DIRECTORY `automations/<name>/SKILL.md` whose YAML
//! `---` frontmatter carries SPEC FIELDS ONLY (name == the directory
//! name, description, and the optional license/compatibility/metadata/
//! allowed-tools) and whose body declares one `## node: <id>` section
//! per node, each opening with a fenced ```toml config block (executor
//! pins, `relies_on` edges, cadence/trigger defaults) parsed with
//! deny-unknown rigor. Shape is DERIVED from arity — one node is an
//! action, 2..=8 a workflow; no shape field exists. Everything above the
//! first node heading is shared orientation (the hub body).
//!
//! Files DECLARE, the daemon ENFORCES: frontmatter and config values are
//! prefills into the existing manifest intake, never around it — the
//! stamp lane (`store.rs`) emits ordinary park/place/relies_on/propose
//! ops through the same validation every hand-proposed manifest passes,
//! and a definition binds a fired session only when its bytes are sealed
//! as a binding ref under an approval digest. An unsealed file is
//! context at best. Definition-level checks here are additional and
//! EARLIER; where the manifest intake is advisory (unrecognized Claude
//! model), this validator stays advisory (`advisories` chips), refusing
//! only structural invalidity.

use std::path::{Path, PathBuf};

/// Node-count rail: one node is an action, 2..=8 a workflow (the
/// registry bound, promoted).
pub(crate) const MAX_DEFINITION_NODES: usize = 8;

/// The agentskills.io spec's frontmatter vocabulary — the ONLY top-level
/// keys a definition may carry (Amendment A1: spec fields only).
const SPEC_FRONTMATTER_KEYS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
];

/// Spec bounds (agentskills.io/specification, fetched 2026-07-27).
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_COMPATIBILITY_CHARS: usize = 500;

/// The shipped house set, embedded so a packaged install carries its
/// definitions without a repo checkout. Materialized into
/// `<state root>/automations/.house/<name>/SKILL.md` at handle
/// construction (ruling R1) so v1 `file:` binding refs have a real path
/// to seal at stamp time.
pub(crate) const HOUSE_DEFINITIONS: &[(&str, &str)] = &[
    (
        "triage",
        include_str!("../../../../automations/triage/SKILL.md"),
    ),
    (
        "housekeeping",
        include_str!("../../../../automations/housekeeping/SKILL.md"),
    ),
    (
        "agenda-reconciliation",
        include_str!("../../../../automations/agenda-reconciliation/SKILL.md"),
    ),
    (
        "steward-gate",
        include_str!("../../../../automations/steward-gate/SKILL.md"),
    ),
    (
        "fix-task",
        include_str!("../../../../automations/fix-task/SKILL.md"),
    ),
    (
        "reconcile-backlog",
        include_str!("../../../../automations/reconcile-backlog/SKILL.md"),
    ),
    (
        "narrative-backfill",
        include_str!("../../../../automations/narrative-backfill/SKILL.md"),
    ),
    (
        "session-digest",
        include_str!("../../../../automations/session-digest/SKILL.md"),
    ),
    (
        "narrative-pyramid",
        include_str!("../../../../automations/narrative-pyramid/SKILL.md"),
    ),
];

/// Where a resolved definition came from — catalog provenance chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefinitionProvenance {
    /// The embedded house set, stamped against its materialized copy.
    House,
    /// `<state root>/automations/<name>/SKILL.md`.
    Personal,
    /// An explicit `file:` path outside the library roots.
    Path,
}

impl DefinitionProvenance {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            DefinitionProvenance::House => "house",
            DefinitionProvenance::Personal => "personal",
            DefinitionProvenance::Path => "path",
        }
    }
}

/// Cadence prefill on an action's single node (`[cadence]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefinitionCadence {
    pub(crate) every_ms: u64,
    pub(crate) suspend_after: Option<u32>,
}

/// Trigger prefill on an action's single node (`[trigger]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefinitionTrigger {
    pub(crate) item_kind: super::types::AgendaKind,
    pub(crate) tags: Vec<String>,
}

/// One parsed node: id from its `## node: <id>` heading, machine config
/// from the fenced toml block opening the section, goal prose after it.
#[derive(Debug, Clone)]
pub(crate) struct DefinitionNode {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) goal: String,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) relies_on: Vec<String>,
    pub(crate) project_root: Option<String>,
    pub(crate) cadence: Option<DefinitionCadence>,
    pub(crate) trigger: Option<DefinitionTrigger>,
}

impl DefinitionNode {
    /// The node's executor prefills as the launch vocabulary every
    /// session-creating lane speaks. `None` when the node pins nothing.
    /// Generic `model`/`effort` map onto the selected backend's own pin
    /// fields; the parse refuses combinations v1 cannot represent.
    pub(crate) fn launch_config(&self) -> Option<crate::event::AgentLaunchConfig> {
        if self.agent.is_none() && self.model.is_none() && self.effort.is_none() {
            return None;
        }
        let mut config = crate::event::AgentLaunchConfig {
            agent: self.agent.clone(),
            ..Default::default()
        };
        match self.agent.as_deref() {
            Some("codex") => {
                config.codex_model = self.model.clone();
                config.codex_reasoning_effort = self.effort.clone();
            }
            _ => {
                config.claude_model = self.model.clone();
                config.claude_effort = self.effort.clone();
            }
        }
        Some(config)
    }
}

/// One parsed, validated definition — the ONE internal form (Q10): an
/// action IS a one-node workflow here; `shape` never exists as data.
#[derive(Debug, Clone)]
pub(crate) struct AutomationDefinition {
    pub(crate) name: String,
    /// Spec description — served by the catalog.
    pub(crate) description: String,
    /// Display title: `metadata.title`, else the name.
    pub(crate) title: String,
    // The three spec surfaces below are parsed and bounds-checked (spec
    // conformance is the validator's law) but deliberately unserved in
    // v1 — future catalog/sheet vocabulary, additive whenever a surface
    // wants them (N4 reconcile: re-scoped from the closed slice-2
    // reader note).
    #[allow(dead_code)]
    pub(crate) metadata: Vec<(String, String)>,
    #[allow(dead_code)]
    pub(crate) license: Option<String>,
    #[allow(dead_code)]
    pub(crate) compatibility: Option<String>,
    #[allow(dead_code)]
    pub(crate) allowed_tools: Option<String>,
    /// Trimmed prose above the first node heading — the hub body for
    /// workflows, shared context every node reads. May be empty for
    /// actions.
    pub(crate) orientation: String,
    pub(crate) nodes: Vec<DefinitionNode>,
    /// Advisory chips (catalog surface), never refusals: the manifest
    /// intake's advisory class (unrecognized Claude model) plus launch
    /// vocabulary findings the intake would refuse at stamp time. One
    /// authority — nothing here re-implements an intake rule.
    pub(crate) advisories: Vec<String>,
}

impl AutomationDefinition {
    /// Shape is derived from arity (Amendment A1): 2..=8 nodes = workflow.
    pub(crate) fn is_workflow(&self) -> bool {
        self.nodes.len() > 1
    }

    /// `relies_on` flattened as (node, dependency) pairs, declaration
    /// order — test vocabulary only: the wire serves per-node
    /// `relies_on` (the same edge information in prefill shape), and
    /// the registry parity tests that read this died at the cutover
    /// (N4 reconcile: dropped to test scope).
    #[cfg(test)]
    pub(crate) fn edges(&self) -> Vec<(String, String)> {
        self.nodes
            .iter()
            .flat_map(|node| {
                node.relies_on
                    .iter()
                    .map(|dep| (node.id.clone(), dep.clone()))
            })
            .collect()
    }
}

/// The machinery-minted manifest goal for one stamped node: an execution
/// preamble pointing at the sealed definition — the FILE is the mandate
/// (ruled Q6); the goal carries nothing beyond this pointer and the
/// re-verify instruction.
pub(crate) fn node_preamble(definition_name: &str, node_id: &str) -> String {
    format!(
        "Execute node \"{node_id}\" of the sealed automation definition \
         \"{definition_name}\". The definition is sealed onto this manifest as a \
         binding ref; this task's rider lines name the approved sha256 and the \
         sealed copy — re-verify the hash, then read the definition in full. \
         Your node's section (`## node: {node_id}`) is your mandate; content \
         outside the node sections orients every node."
    )
}

// ---- Library roots ----

/// The automations library under an explicit state root (testable seam,
/// the `agenda_dir_in` convention).
pub(crate) fn automations_dir_in(state_root: &Path) -> PathBuf {
    state_root.join("automations")
}

/// The materialized house set's root. Dot-named so it can never collide
/// with a personal definition directory (spec names cannot contain `.`).
pub(crate) fn house_dir_in(state_root: &Path) -> PathBuf {
    automations_dir_in(state_root).join(".house")
}

/// Materialize the embedded house set into the state root (ruling R1):
/// v1 binding refs are `file:` only and intake reads from disk, so house
/// definitions need a real path to stamp against — a packaged install
/// has no repo checkout. Refreshes on byte drift (upgrades), never
/// touches personal definitions, and writes via tmp+rename so a
/// concurrent stamp never reads a torn file.
pub(crate) fn materialize_house_definitions(state_root: &Path) -> std::io::Result<()> {
    for (name, content) in HOUSE_DEFINITIONS {
        let dir = house_dir_in(state_root).join(name);
        let path = dir.join("SKILL.md");
        let current = std::fs::read(&path).ok();
        if current.as_deref() == Some(content.as_bytes()) {
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(".SKILL.md.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Resolve a stamp selector to the definition file it names. A plain
/// name resolves personal-shadows-house (the skills precedence rule);
/// `file:<absolute path ending in /SKILL.md>` stamps an explicit file.
/// Resolution grants nothing: the returned path is read once, validated,
/// and sealed by the stamp lane — bindingness comes from the approval
/// digest alone.
pub(crate) fn resolve_definition(
    state_root: &Path,
    selector: &str,
) -> Result<(PathBuf, DefinitionProvenance), String> {
    let selector = selector.trim();
    if let Some(raw) = selector.strip_prefix(super::types::BINDING_REF_FILE_SCHEME) {
        let path = Path::new(raw);
        if !path.is_absolute() {
            return Err(format!("definition path {selector:?} must be absolute"));
        }
        if path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
            return Err(format!(
                "definition path {selector:?} must name a <name>/SKILL.md file \
                 (agentskills.io directory shape)"
            ));
        }
        if !path.is_file() {
            return Err(format!(
                "definition path {selector:?} is not a readable file"
            ));
        }
        return Ok((path.to_path_buf(), DefinitionProvenance::Path));
    }
    if !valid_slug(selector) {
        return Err(format!(
            "{selector:?} is neither a definition name (lowercase alphanumerics and \
             single hyphens) nor a file:<absolute path>/SKILL.md selector"
        ));
    }
    let personal = automations_dir_in(state_root)
        .join(selector)
        .join("SKILL.md");
    if personal.is_file() {
        return Ok((personal, DefinitionProvenance::Personal));
    }
    let house = house_dir_in(state_root).join(selector).join("SKILL.md");
    if house.is_file() {
        return Ok((house, DefinitionProvenance::House));
    }
    Err(format!(
        "no definition named {selector:?} — looked in the personal library \
         (automations/{selector}/SKILL.md under the state root) and the \
         materialized house set"
    ))
}

// ---- The served catalog (slice 2) ----

/// One node's catalog surface — the Automate sheet's prefill material.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct CatalogNode {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) relies_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) every_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) suspend_after: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trigger_kind: Option<super::types::AgendaKind>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) trigger_tags: Vec<String>,
}

/// One catalog entry: a discovered definition with its validation state.
/// Invalid entries list with their refusal reason instead of vanishing
/// (the skills skip-don't-die posture, made visible); a shadowed house
/// entry stays listed with `shadowed:true` so a shadow is never silent.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DefinitionCatalogEntry {
    pub(crate) name: String,
    pub(crate) provenance: &'static str,
    pub(crate) shadowed: bool,
    pub(crate) valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) advisories: Vec<String>,
    pub(crate) workflow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) orientation: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) nodes: Vec<CatalogNode>,
    /// The full definition text — what a stamp of this entry would seal
    /// (read from the same path the stamp lane resolves).
    pub(crate) text: String,
    /// sha256 of `text` — the pin a stamp of this entry would seal right
    /// now, from the same read that produced `text` (absent only when
    /// the file was unreadable). What the sheet's exact-bytes expander
    /// labels and what an adopt restates as its fresh pin; the propose
    /// intake re-verifies any restated pin against its own read, so
    /// serving the hash grants nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    pub(crate) path: String,
}

/// Assemble the served catalog: the personal library plus the
/// materialized house set, each parsed by the one validator. Reads the
/// SAME paths the stamp lane resolves, so the catalog never describes a
/// file a stamp would not seal. Discovery grants nothing — bindingness
/// requires the stamp seal under an approval digest.
pub(crate) fn definition_catalog(state_root: &Path) -> Vec<DefinitionCatalogEntry> {
    let mut entries: Vec<DefinitionCatalogEntry> = Vec::new();
    let mut personal_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Ok(dir) = std::fs::read_dir(automations_dir_in(state_root)) {
        let mut dirs: Vec<PathBuf> = dir
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir_path in dirs {
            let Some(name) = dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            // Non-slug directories (`.house` among them) are not
            // personal definitions.
            if !valid_slug(&name) {
                continue;
            }
            let skill = dir_path.join("SKILL.md");
            if !skill.is_file() {
                continue;
            }
            personal_names.insert(name.clone());
            entries.push(catalog_entry(
                &skill,
                &name,
                DefinitionProvenance::Personal,
                false,
            ));
        }
    }
    for (name, _) in HOUSE_DEFINITIONS {
        let path = house_dir_in(state_root).join(name).join("SKILL.md");
        entries.push(catalog_entry(
            &path,
            name,
            DefinitionProvenance::House,
            personal_names.contains(*name),
        ));
    }
    // Name order; the active entry before its shadowed house twin.
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.shadowed.cmp(&b.shadowed)));
    entries
}

fn catalog_entry(
    path: &Path,
    name: &str,
    provenance: DefinitionProvenance,
    shadowed: bool,
) -> DefinitionCatalogEntry {
    let invalid = |reason: String, text: String, sha256: Option<String>| DefinitionCatalogEntry {
        name: name.to_string(),
        provenance: provenance.as_str(),
        shadowed,
        valid: false,
        reason: Some(reason),
        title: None,
        description: None,
        advisories: Vec::new(),
        workflow: false,
        orientation: None,
        nodes: Vec::new(),
        text,
        sha256,
        path: path.display().to_string(),
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => return invalid(format!("unreadable: {err}"), String::new(), None),
    };
    let sha256 = Some(super::sealed_blobs::digest_bytes(text.as_bytes()));
    match parse_definition(&text, name) {
        Ok(def) => DefinitionCatalogEntry {
            name: def.name.clone(),
            provenance: provenance.as_str(),
            shadowed,
            valid: true,
            reason: None,
            title: Some(def.title.clone()),
            description: Some(def.description.clone()),
            advisories: def.advisories.clone(),
            workflow: def.is_workflow(),
            orientation: (!def.orientation.is_empty()).then(|| def.orientation.clone()),
            nodes: def
                .nodes
                .iter()
                .map(|node| CatalogNode {
                    id: node.id.clone(),
                    title: node.title.clone(),
                    agent: node.agent.clone(),
                    model: node.model.clone(),
                    effort: node.effort.clone(),
                    relies_on: node.relies_on.clone(),
                    every_ms: node.cadence.as_ref().map(|c| c.every_ms),
                    suspend_after: node.cadence.as_ref().and_then(|c| c.suspend_after),
                    trigger_kind: node.trigger.as_ref().map(|t| t.item_kind),
                    trigger_tags: node
                        .trigger
                        .as_ref()
                        .map(|t| t.tags.clone())
                        .unwrap_or_default(),
                })
                .collect(),
            text,
            sha256,
            path: path.display().to_string(),
        },
        Err(reason) => invalid(reason, text, sha256),
    }
}

// ---- Parsing + validation (one validator, every call site) ----

/// The fenced toml config block's schema. `deny_unknown_fields` is the
/// Q5 rigor relocated (Amendment A1): an unknown key refuses the
/// definition by name instead of silently riding as prose.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct NodeConfigToml {
    title: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    relies_on: Option<Vec<String>>,
    project_root: Option<String>,
    cadence: Option<CadenceToml>,
    trigger: Option<TriggerToml>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CadenceToml {
    /// Humane duration: `<integer><m|h|d|w>` (e.g. `7d`), floored by the
    /// manifest intake's cadence rail at stamp time.
    every: String,
    suspend_after: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerToml {
    /// `note` | `task` | `question` — the manifest predicate vocabulary,
    /// deserialized as the wire kind itself so an unknown word refuses
    /// structurally.
    item_kind: super::types::AgendaKind,
    tags: Vec<String>,
}

/// Spec name grammar: 1..=64 lowercase alphanumerics with single
/// interior hyphens (shared by definition names and node ids).
pub(crate) fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= MAX_NAME_CHARS
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

/// Parse `<integer><m|h|d|w>` into milliseconds.
pub(crate) fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.len().saturating_sub(1);
    let (digits, unit) = s.split_at(split.min(s.len()));
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("duration {s:?} must be <integer><m|h|d|w>"))?;
    let unit_ms: u64 = match unit {
        "m" => 60 * 1000,
        "h" => 60 * 60 * 1000,
        "d" => 24 * 60 * 60 * 1000,
        "w" => 7 * 24 * 60 * 60 * 1000,
        _ => return Err(format!("duration {s:?} must be <integer><m|h|d|w>")),
    };
    value
        .checked_mul(unit_ms)
        .ok_or_else(|| format!("duration {s:?} overflows"))
}

/// Parse and validate one definition file. `expected_dir_name` is the
/// containing directory's name — the spec requires frontmatter `name`
/// to match it. Errors REFUSE the definition (structural invalidity);
/// intake-advisory findings ride `advisories` on the Ok value.
pub(crate) fn parse_definition(
    content: &str,
    expected_dir_name: &str,
) -> Result<AutomationDefinition, String> {
    let (yaml, body) = intendant_core::skills::split_frontmatter(content, Path::new("SKILL.md"))?;
    let entries = intendant_core::skills::parse_frontmatter_strict(yaml)?;

    // Frontmatter: spec fields only, refused by name otherwise.
    let mut name = None;
    let mut description = None;
    let mut license = None;
    let mut compatibility = None;
    let mut allowed_tools = None;
    let mut metadata: Vec<(String, String)> = Vec::new();
    for (key, value) in entries {
        if !SPEC_FRONTMATTER_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "frontmatter key {key:?} is not an agentskills.io field \
                 (spec fields: {})",
                SPEC_FRONTMATTER_KEYS.join(", ")
            ));
        }
        use intendant_core::skills::FrontmatterValue;
        match (key.as_str(), value) {
            ("metadata", FrontmatterValue::Map(map)) => metadata = map,
            ("metadata", FrontmatterValue::Scalar(s)) if s.is_empty() => {}
            ("metadata", FrontmatterValue::Scalar(_)) => {
                return Err(
                    "frontmatter `metadata` must be a one-level string-to-string map".into(),
                );
            }
            (_, FrontmatterValue::Map(_)) => {
                return Err(format!(
                    "frontmatter {key:?} must be a scalar — only `metadata` nests (one level)"
                ));
            }
            ("name", FrontmatterValue::Scalar(s)) => name = Some(s),
            ("description", FrontmatterValue::Scalar(s)) => description = Some(s),
            ("license", FrontmatterValue::Scalar(s)) => license = Some(s),
            ("compatibility", FrontmatterValue::Scalar(s)) => compatibility = Some(s),
            ("allowed-tools", FrontmatterValue::Scalar(s)) => allowed_tools = Some(s),
            _ => unreachable!("whitelist covers every key"),
        }
    }
    let name = name.ok_or("frontmatter is missing required field `name`")?;
    if !valid_slug(&name) {
        return Err(format!(
            "name {name:?} violates the spec grammar: 1..={MAX_NAME_CHARS} lowercase \
             alphanumerics with single interior hyphens"
        ));
    }
    if name != expected_dir_name {
        return Err(format!(
            "name {name:?} must equal the containing directory name \
             {expected_dir_name:?} (agentskills.io naming law)"
        ));
    }
    let description = description.ok_or("frontmatter is missing required field `description`")?;
    let description_chars = description.chars().count();
    if description_chars == 0 || description_chars > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "description must be 1..={MAX_DESCRIPTION_CHARS} characters (got {description_chars})"
        ));
    }
    if compatibility
        .as_ref()
        .is_some_and(|c| c.chars().count() > MAX_COMPATIBILITY_CHARS)
    {
        return Err(format!(
            "compatibility exceeds {MAX_COMPATIBILITY_CHARS} characters"
        ));
    }
    let title = metadata
        .iter()
        .find(|(k, _)| k == "title")
        .map(|(_, v)| v.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| name.clone());

    // Body: orientation + `## node: <id>` sections, each opening with a
    // fenced toml config block.
    let (orientation, raw_sections) = split_node_sections(body)?;
    if raw_sections.is_empty() {
        return Err(
            "a definition declares at least one `## node: <id>` section (the body \
             alone binds nobody)"
                .into(),
        );
    }
    if raw_sections.len() > MAX_DEFINITION_NODES {
        return Err(format!(
            "a definition declares at most {MAX_DEFINITION_NODES} nodes \
             (got {})",
            raw_sections.len()
        ));
    }

    let mut nodes: Vec<DefinitionNode> = Vec::with_capacity(raw_sections.len());
    for section in raw_sections {
        if !valid_slug(&section.id) {
            return Err(format!(
                "node id {:?} violates the slug grammar (lowercase alphanumerics, \
                 single interior hyphens)",
                section.id
            ));
        }
        if nodes.iter().any(|n| n.id == section.id) {
            return Err(format!("duplicate node heading `## node: {}`", section.id));
        }
        let config: NodeConfigToml = toml::from_str(&section.config).map_err(|err| {
            // `message()` is the span-free diagnostic — it names unknown
            // fields (refuse-by-name) without the caret art.
            format!(
                "node `{}` config block does not parse: {}",
                section.id,
                err.message()
            )
        })?;
        let goal = section.prose;
        if goal.trim().is_empty() {
            return Err(format!(
                "node `{}` has no mandate prose after its config block",
                section.id
            ));
        }
        if (config.model.is_some() || config.effort.is_some()) && config.agent.is_none() {
            return Err(format!(
                "node `{}` pins model/effort without an `agent` — executor prefills \
                 name their backend",
                section.id
            ));
        }
        if let Some(agent) = config.agent.as_deref() {
            if (config.model.is_some() || config.effort.is_some())
                && !matches!(agent, "claude-code" | "codex")
            {
                return Err(format!(
                    "node `{}`: model/effort prefills support claude-code and codex \
                     in v1 (agent {agent:?} takes only `agent`)",
                    section.id
                ));
            }
        }
        // Substantive bounds (cadence floor, tag counts, suspend >= 1,
        // project_root shape/existence) are deliberately NOT checked
        // here: prefills flow into the manifest intake at stamp time and
        // ITS refusals are the one authority (ruling R4 — definition
        // checks are structural only, never relocated intake rules).
        let cadence = match config.cadence {
            None => None,
            Some(c) => Some(DefinitionCadence {
                every_ms: parse_duration_ms(&c.every)
                    .map_err(|e| format!("node `{}` cadence: {e}", section.id))?,
                suspend_after: c.suspend_after,
            }),
        };
        let trigger = config.trigger.map(|t| DefinitionTrigger {
            item_kind: t.item_kind,
            tags: t.tags,
        });
        if cadence.is_some() && trigger.is_some() {
            return Err(format!(
                "node `{}` declares cadence AND trigger — a node is cadenced OR \
                 triggered (the manifest exclusivity law, mirrored as file grammar)",
                section.id
            ));
        }
        nodes.push(DefinitionNode {
            title: config
                .title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| section.id.clone()),
            id: section.id,
            goal,
            agent: config.agent,
            model: config.model,
            effort: config.effort,
            relies_on: config.relies_on.unwrap_or_default(),
            project_root: config.project_root,
            cadence,
            trigger,
        });
    }

    // Arity-derived shape rules.
    let workflow = nodes.len() > 1;
    if workflow {
        if orientation.trim().is_empty() {
            return Err(
                "a workflow definition needs orientation prose above its first node \
                 heading (the hub body)"
                    .into(),
            );
        }
        for node in &nodes {
            // Ruled absences (R3): workflow nodes are structurally
            // on_unblock — node-level cadence/trigger join the schema
            // only when the manifest grows that vocabulary.
            if node.cadence.is_some() || node.trigger.is_some() {
                return Err(format!(
                    "workflow node `{}` declares cadence/trigger — workflow nodes fire \
                     on_unblock; entry triggers are future vocabulary",
                    node.id
                ));
            }
        }
    } else {
        let node = &nodes[0];
        if !node.relies_on.is_empty() {
            return Err(format!(
                "action node `{}` declares relies_on — an action has no sibling nodes",
                node.id
            ));
        }
    }

    // relies_on targets + the DAG rule (Kahn), promoted from the
    // registry's own test to the one validator (ruling: two call sites,
    // one authority).
    for node in &nodes {
        for dep in &node.relies_on {
            if dep == &node.id {
                return Err(format!("node `{}` relies on itself", node.id));
            }
            if !nodes.iter().any(|n| &n.id == dep) {
                return Err(format!(
                    "node `{}` relies on undeclared node `{dep}`",
                    node.id
                ));
            }
            if node.relies_on.iter().filter(|d| d == &dep).count() > 1 {
                return Err(format!("node `{}` lists dependency `{dep}` twice", node.id));
            }
        }
    }
    let mut remaining: Vec<&DefinitionNode> = nodes.iter().collect();
    loop {
        let free: Vec<String> = remaining
            .iter()
            .filter(|node| {
                node.relies_on
                    .iter()
                    .all(|dep| !remaining.iter().any(|n| &n.id == dep))
            })
            .map(|node| node.id.clone())
            .collect();
        if free.is_empty() {
            break;
        }
        remaining.retain(|node| !free.contains(&node.id));
    }
    if !remaining.is_empty() {
        let cycle: Vec<&str> = remaining.iter().map(|n| n.id.as_str()).collect();
        return Err(format!(
            "relies_on edges form a cycle: {}",
            cycle.join(", ")
        ));
    }

    // Advisory lane: the launch vocabulary findings the manifest intake
    // enforces at stamp time (refuse class) or advises on (unrecognized
    // Claude model) — surfaced as catalog chips, never listing failures.
    let mut advisories = Vec::new();
    for node in &nodes {
        if let Some(config) = node.launch_config() {
            if let Err(err) = crate::session_supervisor::validate_launch_config(&config) {
                advisories.push(format!("node `{}`: {err}", node.id));
            }
            if let Some(warning) =
                crate::session_supervisor::unrecognized_claude_model_warning(&config)
            {
                advisories.push(format!("node `{}`: {warning}", node.id));
            }
        }
    }

    Ok(AutomationDefinition {
        name,
        description,
        title,
        metadata,
        license,
        compatibility,
        allowed_tools,
        orientation,
        nodes,
        advisories,
    })
}

struct RawSection {
    id: String,
    config: String,
    prose: String,
}

/// Split the body into orientation + node sections. The heading map is a
/// BIJECTION (ruling R2): the exact grammar is a level-2 heading
/// `## node: <id>`, and any near-miss heading that reads like a node
/// declaration (`### node: x`, `## Node: x`, `## node:x`) refuses the
/// definition — no orphan sections that read like mandates but bind
/// nobody.
fn split_node_sections(body: &str) -> Result<(String, Vec<RawSection>), String> {
    let mut orientation = String::new();
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut fence_depth = false;
    for line in body.lines() {
        let trimmed = line.trim_end();
        // Track fenced code regions so a ``` block quoting a heading in
        // prose never parses as one.
        if trimmed.trim_start().starts_with("```") {
            fence_depth = !fence_depth;
        }
        if !fence_depth {
            if let Some(rest) = trimmed.strip_prefix("## node: ") {
                let id = rest.trim();
                if id.is_empty() || id.contains(char::is_whitespace) {
                    return Err(format!("malformed node heading {trimmed:?}"));
                }
                sections.push((id.to_string(), String::new()));
                continue;
            }
            if trimmed.starts_with('#') {
                let text = trimmed.trim_start_matches('#').trim_start();
                if text.to_ascii_lowercase().starts_with("node:") {
                    return Err(format!(
                        "near-miss node heading {trimmed:?} — the grammar is exactly \
                         `## node: <id>`"
                    ));
                }
            }
        }
        match sections.last_mut() {
            Some((_, content)) => {
                content.push_str(line);
                content.push('\n');
            }
            None => {
                orientation.push_str(line);
                orientation.push('\n');
            }
        }
    }
    let orientation = trim_newlines(&orientation).to_string();
    let mut raw = Vec::with_capacity(sections.len());
    for (id, content) in sections {
        let (config, prose) = split_config_block(&id, &content)?;
        raw.push(RawSection { id, config, prose });
    }
    Ok((orientation, raw))
}

/// Each node section opens with a fenced ```toml config block (Amendment
/// A1's relocation of the machine schema); the rest of the section is
/// the node's mandate prose.
fn split_config_block(id: &str, section: &str) -> Result<(String, String), String> {
    let mut lines = section.lines();
    let mut config = String::new();
    // First non-blank line must open the fence.
    loop {
        match lines.next() {
            Some(line) if line.trim().is_empty() => continue,
            Some(line) if line.trim() == "```toml" => break,
            _ => {
                return Err(format!(
                    "node section `{id}` must open with a fenced ```toml config block \
                     (an empty block is allowed)"
                ));
            }
        }
    }
    loop {
        match lines.next() {
            Some(line) if line.trim() == "```" => break,
            Some(line) => {
                config.push_str(line);
                config.push('\n');
            }
            None => {
                return Err(format!(
                    "node section `{id}`'s config block never closes its ``` fence"
                ));
            }
        }
    }
    let prose: String = lines.collect::<Vec<_>>().join("\n");
    Ok((config, trim_newlines(&prose).to_string()))
}

fn trim_newlines(s: &str) -> &str {
    s.trim_matches(|c| c == '\n' || c == '\r')
}

/// Parse the embedded house set (panics on a broken embed — the tests
/// below and `house_definitions_satisfy_spec_naming_rules` gate it).
#[cfg(test)]
pub(crate) fn house_definitions() -> Vec<AutomationDefinition> {
    HOUSE_DEFINITIONS
        .iter()
        .map(|(name, content)| {
            parse_definition(content, name)
                .unwrap_or_else(|err| panic!("house definition {name}: {err}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(config: &str, prose: &str) -> String {
        format!(
            "---\nname: sample\ndescription: A sample action.\n---\n\n## node: sample\n\n```toml\n{config}```\n\n{prose}\n"
        )
    }

    #[test]
    fn action_normalizes_to_one_node_workflow() {
        let def = parse_definition(&action("", "Do the thing."), "sample").unwrap();
        assert!(!def.is_workflow());
        assert_eq!(def.nodes.len(), 1);
        assert_eq!(def.nodes[0].id, "sample");
        assert_eq!(def.nodes[0].title, "sample");
        assert_eq!(def.nodes[0].goal, "Do the thing.");
        assert_eq!(def.orientation, "");
        assert!(def.edges().is_empty());
        // The same parse carries workflows — one internal form, shape
        // derived from arity alone.
        let two = "---\nname: two\ndescription: d\n---\n\nOrient.\n\n## node: a\n\n```toml\n```\n\nA.\n\n## node: b\n\n```toml\nrelies_on = [\"a\"]\n```\n\nB.\n";
        let def = parse_definition(two, "two").unwrap();
        assert!(def.is_workflow());
        assert_eq!(def.edges(), vec![("b".to_string(), "a".to_string())]);
    }

    #[test]
    fn definition_frontmatter_is_spec_valid() {
        // Non-spec top-level keys refuse by name (the registry's `shape`
        // died in Amendment A1; a stray `shape:` must not resurrect it).
        let err = parse_definition(
            "---\nname: x\ndescription: d\nshape: action\n---\n\n## node: x\n\n```toml\n```\n\nP.\n",
            "x",
        )
        .unwrap_err();
        assert!(
            err.contains("\"shape\" is not an agentskills.io field"),
            "{err}"
        );

        // name == directory name (spec law).
        let err = parse_definition(
            "---\nname: x\ndescription: d\n---\n\n## node: x\n\n```toml\n```\n\nP.\n",
            "not-x",
        )
        .unwrap_err();
        assert!(
            err.contains("must equal the containing directory name"),
            "{err}"
        );

        // Name grammar: uppercase, double hyphens, hyphen edges refuse.
        for bad in ["Bad", "a--b", "-a", "a-"] {
            let doc = format!(
                "---\nname: {bad}\ndescription: d\n---\n\n## node: n\n\n```toml\n```\n\nP.\n"
            );
            let err = parse_definition(&doc, bad).unwrap_err();
            assert!(err.contains("spec grammar"), "{bad}: {err}");
        }

        // Description bounds (1..=1024 chars).
        let long = "x".repeat(1025);
        let doc =
            format!("---\nname: x\ndescription: {long}\n---\n\n## node: x\n\n```toml\n```\n\nP.\n");
        let err = parse_definition(&doc, "x").unwrap_err();
        assert!(err.contains("description must be 1..=1024"), "{err}");

        // metadata is the one nested form, one level, string-to-string;
        // title rides it.
        let doc = "---\nname: x\ndescription: d\nmetadata:\n  title: The X\n  team: house\n---\n\n## node: x\n\n```toml\n```\n\nP.\n";
        let def = parse_definition(doc, "x").unwrap();
        assert_eq!(def.title, "The X");
        assert_eq!(def.metadata.len(), 2);

        // A map under any other key refuses.
        let err = parse_definition(
            "---\nname: x\ndescription: d\nlicense:\n  spdx: MIT\n---\n\n## node: x\n\n```toml\n```\n\nP.\n",
            "x",
        )
        .unwrap_err();
        assert!(err.contains("only `metadata` nests"), "{err}");

        // Spec optional fields parse as scalars.
        let doc = "---\nname: x\ndescription: d\nlicense: MIT\ncompatibility: macOS only\nallowed-tools: Bash Read\n---\n\n## node: x\n\n```toml\n```\n\nP.\n";
        let def = parse_definition(doc, "x").unwrap();
        assert_eq!(def.license.as_deref(), Some("MIT"));
        assert_eq!(def.compatibility.as_deref(), Some("macOS only"));
        assert_eq!(def.allowed_tools.as_deref(), Some("Bash Read"));
    }

    #[test]
    fn node_config_blocks_parse_with_deny_unknown() {
        // Unknown config keys refuse by name (deny_unknown_fields).
        let err = parse_definition(&action("budget = 5\n", "P."), "sample").unwrap_err();
        assert!(err.contains("config block does not parse"), "{err}");
        assert!(err.contains("budget"), "{err}");

        // Unknown nested keys refuse too.
        let err = parse_definition(
            &action("[cadence]\nevery = \"7d\"\nspend = 1\n", "P."),
            "sample",
        )
        .unwrap_err();
        assert!(err.contains("spend"), "{err}");

        // A section without a config block refuses — machine schema is
        // never inferred from prose.
        let doc = "---\nname: sample\ndescription: d\n---\n\n## node: sample\n\nJust prose.\n";
        let err = parse_definition(doc, "sample").unwrap_err();
        assert!(
            err.contains("must open with a fenced ```toml config block"),
            "{err}"
        );

        // An unterminated fence refuses.
        let doc = "---\nname: sample\ndescription: d\n---\n\n## node: sample\n\n```toml\nagent = \"claude-code\"\n\nProse.\n";
        let err = parse_definition(doc, "sample").unwrap_err();
        assert!(err.contains("never closes"), "{err}");

        // An empty block is fine; prose after the block may carry its own
        // fenced examples.
        let def = parse_definition(
            &action(
                "",
                "Prose with an example:\n\n```toml\nnot = \"config\"\n```\n\nEnd.",
            ),
            "sample",
        )
        .unwrap();
        assert!(def.nodes[0].goal.contains("not = \"config\""));
    }

    #[test]
    fn definition_heading_map_is_a_bijection() {
        // Duplicate node ids refuse.
        let doc = "---\nname: x\ndescription: d\n---\n\nO.\n\n## node: a\n\n```toml\n```\n\nA.\n\n## node: a\n\n```toml\n```\n\nB.\n";
        let err = parse_definition(doc, "x").unwrap_err();
        assert!(err.contains("duplicate node heading"), "{err}");

        // Near-miss headings refuse — no orphan sections that read like
        // mandates but bind nobody (ruling R2).
        for near in ["### node: a", "## Node: a", "## node:a", "# node: a"] {
            let doc = format!(
                "---\nname: x\ndescription: d\n---\n\n## node: ok\n\n```toml\n```\n\nP.\n\n{near}\n\nOrphan prose.\n"
            );
            let err = parse_definition(&doc, "x").unwrap_err();
            assert!(
                err.contains("near-miss node heading") || err.contains("malformed"),
                "{near}: {err}"
            );
        }

        // A fenced code block quoting a heading is prose, not a section.
        let def = parse_definition(
            &action("", "Example:\n\n```\n## node: quoted\n```\n\nEnd."),
            "sample",
        )
        .unwrap();
        assert_eq!(def.nodes.len(), 1);

        // relies_on must name declared headings.
        let doc = "---\nname: x\ndescription: d\n---\n\nO.\n\n## node: a\n\n```toml\nrelies_on = [\"ghost\"]\n```\n\nA.\n\n## node: b\n\n```toml\n```\n\nB.\n";
        let err = parse_definition(doc, "x").unwrap_err();
        assert!(err.contains("undeclared node `ghost`"), "{err}");
    }

    #[test]
    fn definition_dag_rule_promoted_from_registry_test() {
        // A cycle refuses (Kahn leftovers named).
        let doc = "---\nname: x\ndescription: d\n---\n\nO.\n\n## node: a\n\n```toml\nrelies_on = [\"b\"]\n```\n\nA.\n\n## node: b\n\n```toml\nrelies_on = [\"a\"]\n```\n\nB.\n";
        let err = parse_definition(doc, "x").unwrap_err();
        assert!(err.contains("cycle"), "{err}");

        // A self-edge refuses by name.
        let doc = "---\nname: x\ndescription: d\n---\n\nO.\n\n## node: a\n\n```toml\nrelies_on = [\"a\"]\n```\n\nA.\n\n## node: b\n\n```toml\n```\n\nB.\n";
        let err = parse_definition(doc, "x").unwrap_err();
        assert!(err.contains("relies on itself"), "{err}");
    }

    #[test]
    fn arity_rules_and_ruled_absences_hold() {
        // Zero nodes refuse.
        let err = parse_definition("---\nname: x\ndescription: d\n---\n\nProse only.\n", "x")
            .unwrap_err();
        assert!(err.contains("at least one `## node:"), "{err}");

        // Nine nodes refuse.
        let mut doc = String::from("---\nname: x\ndescription: d\n---\n\nO.\n");
        for i in 0..9 {
            doc.push_str(&format!("\n## node: n{i}\n\n```toml\n```\n\nP{i}.\n"));
        }
        let err = parse_definition(&doc, "x").unwrap_err();
        assert!(err.contains("at most 8 nodes"), "{err}");

        // Workflow nodes carry no cadence/trigger (ruled absence R3).
        let doc = "---\nname: x\ndescription: d\n---\n\nO.\n\n## node: a\n\n```toml\n[cadence]\nevery = \"7d\"\n```\n\nA.\n\n## node: b\n\n```toml\n```\n\nB.\n";
        let err = parse_definition(doc, "x").unwrap_err();
        assert!(
            err.contains("workflow nodes fire\n                     on_unblock")
                || err.contains("on_unblock"),
            "{err}"
        );

        // A workflow without orientation refuses (the hub body).
        let doc = "---\nname: x\ndescription: d\n---\n\n## node: a\n\n```toml\n```\n\nA.\n\n## node: b\n\n```toml\n```\n\nB.\n";
        let err = parse_definition(doc, "x").unwrap_err();
        assert!(err.contains("orientation prose"), "{err}");

        // An action's node takes no relies_on.
        let err =
            parse_definition(&action("relies_on = [\"sample\"]\n", "P."), "sample").unwrap_err();
        assert!(
            err.contains("relies on itself") || err.contains("no sibling nodes"),
            "{err}"
        );

        // cadence XOR trigger on the action node.
        let err = parse_definition(
            &action(
                "[cadence]\nevery = \"7d\"\n\n[trigger]\nitem_kind = \"question\"\ntags = [\"gate\"]\n",
                "P.",
            ),
            "sample",
        )
        .unwrap_err();
        assert!(err.contains("cadenced OR"), "{err}");

        // Executor prefills need their backend named; v1 maps
        // claude-code and codex.
        let err =
            parse_definition(&action("model = \"claude-fable-5\"\n", "P."), "sample").unwrap_err();
        assert!(err.contains("without an `agent`"), "{err}");
        let err = parse_definition(
            &action("agent = \"pi\"\nmodel = \"gpt-x\"\n", "P."),
            "sample",
        )
        .unwrap_err();
        assert!(err.contains("claude-code and codex"), "{err}");
    }

    #[test]
    fn advisories_surface_launch_vocabulary_findings_without_refusing() {
        // A truly unknown model shape parses fine but chips an advisory
        // (the AU precedent: the intake advises; the definition mirror
        // stays advisory).
        let def = parse_definition(
            &action(
                "agent = \"claude-code\"\nmodel = \"gpt-9000\"\neffort = \"max\"\n",
                "P.",
            ),
            "sample",
        )
        .unwrap();
        assert!(
            def.advisories.iter().any(|a| a.contains("gpt-9000")),
            "{:?}",
            def.advisories
        );
        // The bare family-version landmine ("fable-5") heals by launch
        // normalization to the `claude-` full id — recognized, so no
        // advisory chips (the 2026-07-26 class is now canonicalized, not
        // refused).
        let def = parse_definition(
            &action(
                "agent = \"claude-code\"\nmodel = \"fable-5\"\neffort = \"max\"\n",
                "P.",
            ),
            "sample",
        )
        .unwrap();
        assert!(def.advisories.is_empty(), "{:?}", def.advisories);
        // An unknown agent name is a stamp-time intake refusal; here it
        // is a chip, not a listing failure (§2.6).
        let def = parse_definition(&action("agent = \"clud-code\"\n", "P."), "sample").unwrap();
        assert_eq!(def.advisories.len(), 1, "{:?}", def.advisories);
    }

    #[test]
    fn house_definitions_satisfy_spec_naming_rules() {
        let defs = house_definitions();
        assert_eq!(defs.len(), HOUSE_DEFINITIONS.len());
        let mut names = std::collections::BTreeSet::new();
        for def in &defs {
            // Spec naming + frontmatter constraints are asserted by our
            // validator (no external tool in CI): parse succeeded, so
            // name grammar, dir bijection, description bounds, and the
            // heading map all held. On top: unique names, non-empty
            // titles, and CLEAN advisory chips — the 2026-07-26
            // bare-`fable-5` landmine class fails here before shipping.
            assert!(names.insert(def.name.clone()), "duplicate {}", def.name);
            assert!(!def.title.trim().is_empty());
            assert!(
                def.advisories.is_empty(),
                "house definition {} ships advisory chips: {:?}",
                def.name,
                def.advisories
            );
            for node in &def.nodes {
                if let Some(model) = node.model.as_deref() {
                    match node.agent.as_deref() {
                        Some("codex") => assert!(
                            crate::project::codex_model_catalog_entry(model).is_some(),
                            "house codex model prefill {model:?} is not in the catalog"
                        ),
                        _ => assert!(
                            crate::project::claude_model_is_recognized(model),
                            "house model prefill {model:?} is not a CLI-accepted shape"
                        ),
                    }
                }
            }
        }
    }

    /// Card 01KYTW64HX: the three narrative definitions are
    /// TRANSLATIONS of the live armed NS schedules (their goal texts
    /// ride the commissioning gate as sealed refs). The hard-won laws
    /// must survive translation — every fragment below is a distinctive
    /// carry from the live goal texts, compared whitespace-normalized
    /// so prose re-wrapping never masks a dropped law. Losing one in an
    /// edit fails here instead of shipping a defanged mandate.
    #[test]
    fn ns_definitions_carry_the_live_laws_verbatim() {
        fn squash(s: &str) -> String {
            s.split_whitespace().collect::<Vec<_>>().join(" ")
        }
        let defs = house_definitions();
        let by_name = |name: &str| defs.iter().find(|d| d.name == name).expect("house def");
        let carries = |text: &str, label: &str, laws: &[&str]| {
            let squashed = squash(text);
            for law in laws {
                assert!(
                    squashed.contains(&squash(law)),
                    "{label} lost the live-law fragment {law:?}"
                );
            }
        };

        // narrative-backfill: the backfill arc's dispatch/quota/patience/
        // never-detach/cap discipline, translated one-shot.
        let backfill = by_name("narrative-backfill");
        carries(
            &backfill.nodes[0].goal,
            "narrative-backfill",
            &[
                // Dispatch law (owner-directed 2026-07-26).
                "SYNCHRONOUS ROLLING `codex exec` pool targeting 22 attached workers",
                "YOU remain the sole estate/journal writer",
                "NO BATCH BARRIER: reap, validate, and commit each completion immediately",
                "self-caps at 3 concurrent subagents",
                "Wait on every worker PID — no detaching, no daemons, no survivors",
                // Quota law: quota is a pool condition, never a skip.
                "never consumes a session's retry and never journals 'stalled'",
                "pause refilling and back off via shell sleeps (sleeping costs no quota)",
                // Patience law.
                "NEVER kill a live digest child",
                "can exceed 15 minutes even on tiny inputs",
                "journal {skipped:'stalled'} with a note and move to the NEXT target",
                // Never-detach law.
                "ITSELF IS THE ORCHESTRATOR",
                "never build your own survivor",
                // Auth + estimate laws (OAuth-always, caps are estimates).
                "Codex subscription OAuth ONLY — NEVER an API key or direct API billing",
                "ESTIMATES (API-equivalent arithmetic serving the cap",
                "observed:false",
                // Owner-set cap parameter, fail-closed, live enforcement.
                "NS CAP: $<amount>",
                "STOP REFILLING, wait out attached workers",
                "NS CAP REACHED",
                // Enumeration + digest + journal discipline.
                "(newest_mtime_ms,total_bytes)",
                "idle >6h",
                "OLDEST-first",
                "1-3K tokens",
                "(4K hard cap)",
                "cites ORIGINAL anchors",
                "<=240 chars VERBATIM",
                "anchor:{locator,ts_ms,role}",
                "prompt_version:'ns1-v2'",
                "exporter_version:'pr609'",
                "tmp+rename",
                "{skipped:'wrapper'|'empty'}",
                "{skipped:'trivial'}",
                "redaction stays ON; NEVER pass --redact off",
                // Chunking rides the pool; merges are worker tasks.
                "MERGE runs as a worker task in a slot",
                "a stated hole beats losing a giant session to one bad chunk",
                // The stalled sweep + completion handoff.
                "re-attempt every journal entry {skipped:'stalled'} exactly once each",
                "this sweep is their only second chance",
                "NS BACKFILL COMPLETE",
                // Product quarantine.
                "create agenda items or memory proposals from this mandate",
            ],
        );

        // session-digest: the daily mandate's compressed form of the
        // same conduct, plus the guard and the territory addendum.
        let daily = by_name("session-digest");
        carries(
            &daily.nodes[0].goal,
            "session-digest",
            &[
                "never double-digest",
                "SYNCHRONOUS ROLLING `codex exec` pool targeting up to 22 attached workers",
                "you the sole estate/journal writer",
                "self-caps at 3 — live incident 2026-07-26",
                "Wait on every worker PID — no detaching, daemons, or survivors",
                "Never kill a healthy worker; retry self-failures once in the same slot",
                "pause refilling pool-wide, back off via shell sleeps (they cost no quota)",
                "the cadence resurrects you",
                "a hard cap surfaces to the owner via the suspension breaker by design",
                "chunks are slot work units, the merge runs as a worker task in a slot (never in you)",
                "a chunk failing its retry merges as a stated hole, never a stalled session",
                "candidates = journal-absent or watermark-advanced sessions idle >6h",
                "1-3K tokens, [n] markers",
                "<=240-char VERBATIM quotes",
                "{locator,ts_ms,role}",
                "prompt_version:'ns1-v2'",
                "skipped:'wrapper'|'empty'",
                "skipped:'trivial'",
                "NS daily: <n> digested, <m> skipped, ~$<cost>",
                "redaction stays ON",
                // Territory addendum (the v2 extraction contract).
                "verbatim-observable from the transcript (tool calls, diffs, explicit reads/writes)",
                "{path, kind:'file'|'dir', anchor}",
                "empty array when none",
                "cap 24; never inferred, never normalized beyond trimming",
                "no extra model calls — extraction rides the digest pass",
            ],
        );

        // narrative-pyramid: the weekly manifest's laws — executor law,
        // safeguards law with the opus exception, audit, ruled caps.
        let pyramid = by_name("narrative-pyramid");
        carries(
            &pyramid.orientation,
            "narrative-pyramid orientation",
            &[
                "NS weekly: waiting on digests",
                "prior-lane episodes, staged-work notes, and defects",
                "episodes marked for the owner brief go IN the owner brief",
                "Absorb staged work from prior lanes",
                "rollup bulk NEVER enters a fable lane",
            ],
        );
        let node_goal = |id: &str| {
            pyramid
                .nodes
                .iter()
                .find(|n| n.id == id)
                .expect("pyramid node")
                .goal
                .as_str()
        };
        carries(
            node_goal("rollups"),
            "narrative-pyramid/rollups",
            &[
                "reasoning effort HIGH — owner directive 2026-07-26",
                "per-house narrative first, per-project sections inside",
                "3-8K tokens, EVERY claim citing digest locators (sessions/<source>/<id>)",
                "heavy weeks fold day-partitions first",
                "Atomic writes",
            ],
        );
        carries(
            node_goal("synthesis"),
            "narrative-pyramid/synthesis",
            &[
                "from ALL rollups (input budget <=300K tokens)",
                "arcs, decisions, reversals, unresolved threads",
                "every claim cites rollup locators (rollups/<week>)",
                "layered for both the context-switching and the deeply-focused reader",
                // The safeguards law, incl. the opus exception, verbatim.
                "NEVER build one giant synthesis request",
                "Work by PARTS: draft per-arc sections across separate turns",
                "DELEGATE security-heavy weeks' content to claude-opus-5 subagents",
                "distilled narrative-safe prose with citations preserved",
                "integrate the distilled parts in this lane so the final voice stays fable",
                "never resend those bytes from this lane — split smaller and delegate",
                "Keep each turn's added payload modest",
                "A context that re-flags on every request is DEAD: stand down cleanly",
                // Fidelity audit.
                "sample 5 random key_claims",
                "verify the quote appears VERBATIM at the anchor",
                "NS AUDIT FAILURE",
            ],
        );
        carries(
            node_goal("products"),
            "narrative-pyramid/products",
            &[
                // Owner brief per the briefing standard.
                "situate (one plain sentence), what changed this week (3-6 lines), depth pointer",
                "committed recommendation if any decision is pending",
                "Silence does nothing",
                // Product lanes: propose-only, ruled caps.
                "kind observation|decision",
                "'derived:track-ns'",
                "Propose-only; judgments are the owner's",
                "not bulk",
                "<=3 proposals per week",
                "tagged recovered-intent + track-ns",
                "one-line intent + <=240-char verbatim quote + plain context",
                "place under the intent hub if it exists",
                "'NS BACKFILL COMPLETE' annotation",
                "rank ALL digest intent-candidates by recency x explicitness",
                "propose the top <=25 under it (same shape as b)",
                "NEVER exceed 25; overflow stays greppable in digests",
                "exceed the caps",
            ],
        );

        // The executor stack is structural, not just prose: digests ride
        // codex sol/xhigh; rollups opus/HIGH; synthesis + products
        // fable/max (the owner-directed 2026-07-26 stack).
        for def in [backfill, daily] {
            let node = &def.nodes[0];
            assert_eq!(node.agent.as_deref(), Some("codex"), "{}", def.name);
            assert_eq!(node.model.as_deref(), Some("gpt-5.6-sol"), "{}", def.name);
            assert_eq!(node.effort.as_deref(), Some("xhigh"), "{}", def.name);
        }
        let rollups = pyramid.nodes.iter().find(|n| n.id == "rollups").unwrap();
        assert_eq!(rollups.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(rollups.effort.as_deref(), Some("high"));
        for id in ["synthesis", "products"] {
            let node = pyramid.nodes.iter().find(|n| n.id == id).unwrap();
            assert_eq!(node.agent.as_deref(), Some("claude-code"), "{id}");
            assert_eq!(node.model.as_deref(), Some("claude-fable-5"), "{id}");
            assert_eq!(node.effort.as_deref(), Some("max"), "{id}");
        }
        // Arity-derived shapes: one-shot bootstrap (no cadence, no
        // trigger), cadenced daily (1d, suspend after 3), three-lane
        // weekly workflow chained rollups -> synthesis -> products.
        assert!(!backfill.is_workflow());
        assert!(backfill.nodes[0].cadence.is_none() && backfill.nodes[0].trigger.is_none());
        assert!(!daily.is_workflow());
        let cadence = daily.nodes[0].cadence.as_ref().expect("daily cadence");
        assert_eq!(cadence.every_ms, 24 * 60 * 60 * 1000);
        assert_eq!(cadence.suspend_after, Some(3));
        assert!(pyramid.is_workflow());
        assert_eq!(
            pyramid.edges(),
            vec![
                ("synthesis".to_string(), "rollups".to_string()),
                ("products".to_string(), "synthesis".to_string()),
            ]
        );
    }

    #[test]
    fn definition_catalog_lists_house_personal_and_invalid_entries() {
        let root = tempfile::tempdir().unwrap();
        materialize_house_definitions(root.path()).unwrap();
        // Baseline: the whole house set, valid, house-chipped.
        let catalog = definition_catalog(root.path());
        assert_eq!(catalog.len(), HOUSE_DEFINITIONS.len());
        assert!(catalog
            .iter()
            .all(|e| e.valid && e.provenance == "house" && !e.shadowed));
        let fix = catalog.iter().find(|e| e.name == "fix-task").unwrap();
        assert!(fix.workflow);
        assert_eq!(fix.nodes.len(), 4);
        assert_eq!(fix.nodes[1].relies_on, vec!["investigate".to_string()]);
        assert!(fix.orientation.is_some());
        let triage = catalog.iter().find(|e| e.name == "triage").unwrap();
        assert!(!triage.workflow);
        assert_eq!(triage.nodes[0].every_ms, Some(7 * 24 * 60 * 60 * 1000));
        let steward = catalog.iter().find(|e| e.name == "steward-gate").unwrap();
        assert_eq!(
            steward.nodes[0].trigger_kind,
            Some(super::super::types::AgendaKind::Question)
        );
        assert_eq!(steward.nodes[0].trigger_tags, vec!["gate".to_string()]);
        assert!(catalog.iter().all(|e| !e.text.is_empty()));
        // Every readable entry serves the pin a stamp would seal right
        // now, from the same read that produced `text`.
        assert!(catalog.iter().all(|e| {
            e.sha256.as_deref()
                == Some(super::super::sealed_blobs::digest_bytes(e.text.as_bytes()).as_str())
        }));

        // A personal definition shadows its house twin VISIBLY: both
        // list, the personal entry first, the house one flagged.
        let personal = automations_dir_in(root.path()).join("triage");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::write(personal.join("SKILL.md"), HOUSE_DEFINITIONS[0].1).unwrap();
        // An invalid personal definition lists with its reason instead of
        // vanishing (skip-don't-die, visible).
        let broken = automations_dir_in(root.path()).join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join("SKILL.md"),
            "---\nname: broken\ndescription: d\nshape: action\n---\n\n## node: broken\n\n```toml\n```\n\nP.\n",
        )
        .unwrap();
        let catalog = definition_catalog(root.path());
        assert_eq!(catalog.len(), HOUSE_DEFINITIONS.len() + 2);
        let triage_entries: Vec<_> = catalog.iter().filter(|e| e.name == "triage").collect();
        assert_eq!(triage_entries.len(), 2);
        assert_eq!(triage_entries[0].provenance, "personal");
        assert!(!triage_entries[0].shadowed);
        assert_eq!(triage_entries[1].provenance, "house");
        assert!(triage_entries[1].shadowed);
        let broken_entry = catalog.iter().find(|e| e.name == "broken").unwrap();
        assert!(!broken_entry.valid);
        assert!(
            broken_entry
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("shape")),
            "{:?}",
            broken_entry.reason
        );
        // Invalid-but-readable still hashes: bytes are bytes.
        assert!(broken_entry.sha256.is_some());
    }

    #[test]
    fn duration_grammar_is_exact() {
        assert_eq!(parse_duration_ms("7d").unwrap(), 7 * 24 * 60 * 60 * 1000);
        assert_eq!(parse_duration_ms("15m").unwrap(), 15 * 60 * 1000);
        assert_eq!(parse_duration_ms("1w").unwrap(), 7 * 24 * 60 * 60 * 1000);
        assert_eq!(parse_duration_ms("36h").unwrap(), 36 * 60 * 60 * 1000);
        for bad in ["7", "d", "7 d", "-7d", "7s", "7dd", ""] {
            assert!(parse_duration_ms(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn house_definitions_materialize_before_stamp() {
        let root = tempfile::tempdir().unwrap();
        materialize_house_definitions(root.path()).unwrap();
        for (name, content) in HOUSE_DEFINITIONS {
            let path = house_dir_in(root.path()).join(name).join("SKILL.md");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                *content,
                "{name} materialized bytes drifted"
            );
            let (resolved, provenance) = resolve_definition(root.path(), name).unwrap();
            assert_eq!(resolved, path);
            assert_eq!(provenance, DefinitionProvenance::House);
        }
        // Refresh-on-drift: a stale materialized copy is rewritten.
        let stale = house_dir_in(root.path()).join("triage").join("SKILL.md");
        std::fs::write(&stale, "stale").unwrap();
        materialize_house_definitions(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap(),
            HOUSE_DEFINITIONS[0].1
        );
        // Personal shadows house, visibly distinct provenance.
        let personal = automations_dir_in(root.path()).join("triage");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::write(personal.join("SKILL.md"), HOUSE_DEFINITIONS[0].1).unwrap();
        let (resolved, provenance) = resolve_definition(root.path(), "triage").unwrap();
        assert_eq!(resolved, personal.join("SKILL.md"));
        assert_eq!(provenance, DefinitionProvenance::Personal);
        // Unknown names refuse with the looked-in error.
        let err = resolve_definition(root.path(), "no-such").unwrap_err();
        assert!(err.contains("no definition named"), "{err}");
        // Explicit paths must be .../SKILL.md.
        let wrong_name = root.path().join("definition.md");
        let selector = format!("file:{}", wrong_name.display());
        let err = resolve_definition(root.path(), &selector).unwrap_err();
        assert!(err.contains("SKILL.md"), "{err}");
    }

    /// The docs walkthrough blocks pin the FILES' prose (retargeted from
    /// the registry in the migration's step 1 — §2.8): the fenced
    /// ```text blocks under each walkthrough header byte-match the
    /// parsed house definitions.
    #[test]
    fn docs_walkthrough_blocks_byte_match_the_house_files() {
        // Actions checkout may materialize tracked text as CRLF on
        // Windows. The prose comparison is line-oriented, so normalize
        // the checkout representation before locating Markdown fences.
        let docs = include_str!("../../../../docs/src/agenda-and-memory.md").replace("\r\n", "\n");
        let block_after = |header: &str| -> &str {
            let at = docs.find(header).expect("docs section header present");
            let open = docs[at..].find("```text\n").expect("fenced block") + at + 8;
            let close = docs[open..].find("```").expect("fence closes") + open;
            docs[open..close].trim_end_matches('\n')
        };
        let blocks_after = |header: &str, count: usize| -> Vec<&str> {
            let mut at = docs.find(header).expect("docs section header present");
            let mut blocks = Vec::new();
            for _ in 0..count {
                let open = docs[at..].find("```text\n").expect("fenced block") + at + 8;
                let close = docs[open..].find("```").expect("fence closes") + open;
                blocks.push(docs[open..close].trim_end_matches('\n'));
                at = close + 3;
            }
            blocks
        };
        let defs = house_definitions();
        let by_name = |name: &str| defs.iter().find(|d| d.name == name).expect("house def");

        assert_eq!(
            block_after("### The triage mandate"),
            by_name("triage").nodes[0].goal
        );
        assert_eq!(
            block_after("### The housekeeping recipe"),
            by_name("housekeeping").nodes[0].goal
        );
        assert_eq!(
            block_after("### The agenda-reconciliation mandate"),
            by_name("agenda-reconciliation").nodes[0].goal
        );
        assert_eq!(
            block_after("### The steward-gate mandate"),
            by_name("steward-gate").nodes[0].goal
        );
        for workflow in ["fix-task", "reconcile-backlog", "narrative-pyramid"] {
            let def = by_name(workflow);
            let header = format!("### The {workflow} workflow");
            let blocks = blocks_after(&header, 1 + def.nodes.len());
            assert_eq!(blocks[0], def.orientation, "{workflow} orientation drifted");
            for (node, block) in def.nodes.iter().zip(&blocks[1..]) {
                assert_eq!(*block, node.goal, "node {} goal drifted", node.id);
            }
        }
        // The NS actions pin orientation AND goal — their orientation
        // carries the stamp guidance (cap parameter, evening anchor).
        for action in ["narrative-backfill", "session-digest"] {
            let def = by_name(action);
            let header = format!("### The {action} mandate");
            let blocks = blocks_after(&header, 2);
            assert_eq!(blocks[0], def.orientation, "{action} orientation drifted");
            assert_eq!(blocks[1], def.nodes[0].goal, "{action} goal drifted");
        }
        // The steward honesty note stays verbatim in the docs prose (the
        // T3 pin, carried through the walkthrough retarget).
        assert!(
            docs.contains(
                "a Fable-5 steward session RULES within delegated bounds and\n\
                 FLAGS owner-decisions to the rail — it inherits the human steward's\n\
                 delegation, not the owner's authority."
            ),
            "the honesty note must appear verbatim in the docs"
        );
    }
}
