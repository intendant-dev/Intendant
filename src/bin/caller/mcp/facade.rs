//! The CLI-shaped MCP facade: a context-efficient control surface.
//!
//! Instead of a client swallowing dozens of typed tool schemas up front,
//! `tool_profile=facade` advertises five meta-tools — `inspect`, `act`, and
//! `authorize` (risk-split argv executors), plus `help` and `docs` — whose
//! grammar is the declarative command registry below. Everything else is
//! discovered lazily: `help` renders the command map from the registry,
//! `docs` serves the embedded operate-skills corpus, and each executor call
//! names one registered command as an argv array.
//!
//! Two laws (docs/design-mcp-control-lane.md §3):
//!
//! - **The facade is a router, not a privilege.** A call is authorized as
//!   the *resolved* command's operation — derived from the underlying tool
//!   via [`crate::mcp::mcp_tool_operation`], never stored twice — against
//!   the caller's principal, at every ingress, before any side effect.
//!   Unknown argv is a parse error and never dispatches; a command invoked
//!   through the wrong risk lane is redirected by name, so host-side
//!   per-tool confirmation policy stays truthful.
//! - **The registry is pure.** No filesystem reads, no environment, no
//!   stdin/`@file` expansion, no output paths, no process exit — those are
//!   CLI-frontend behaviors (`ctl.rs`) that must never run inside the
//!   daemon. Argv values are literal strings.
//!
//! The starter table below covers the operate core (status, approvals,
//! input, ask/notify/notes, tasks, agenda, memory, display reads). Families
//! migrate here from `ctl.rs` incrementally; the terminal family lands with
//! its accessor plumbing in a follow-up.

use crate::peer::access_policy::PeerOperation;

/// Client-facing risk lane; one meta-tool per lane so MCP hosts can apply
/// per-tool allowlists and confirmation rules truthfully.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RiskLane {
    /// Provably read-only commands.
    Inspect,
    /// Ordinary mutating commands.
    Act,
    /// Approval-resolution and authority-adjacent commands.
    Authorize,
}

impl RiskLane {
    pub(crate) fn tool_name(self) -> &'static str {
        match self {
            RiskLane::Inspect => "inspect",
            RiskLane::Act => "act",
            RiskLane::Authorize => "authorize",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ValueKind {
    Str,
    U64,
    Bool,
    /// Repeatable string flag collected into a JSON array.
    StrList,
}

struct PositionalSpec {
    name: &'static str,
    json_key: &'static str,
    kind: ValueKind,
    required: bool,
    /// A greedy positional joins every remaining non-flag token with
    /// spaces (free-text tails like task descriptions). Must be last.
    greedy: bool,
}

struct FlagSpec {
    /// Flag name without the leading dashes.
    name: &'static str,
    json_key: &'static str,
    kind: ValueKind,
    help: &'static str,
}

pub(crate) struct CommandSpec {
    /// Command path segments, e.g. `["approval", "approve"]`.
    pub(crate) path: &'static [&'static str],
    pub(crate) lane: RiskLane,
    /// The underlying MCP tool this command executes as. Its IAM operation
    /// (via `mcp_tool_operation`) is the command's authorization target.
    pub(crate) tool: &'static str,
    /// JSON object merged into the arguments first (op tags, defaults).
    /// Positionals and flags overwrite seed keys.
    seed: &'static str,
    positionals: &'static [PositionalSpec],
    flags: &'static [FlagSpec],
    help: &'static str,
}

const fn p_str(
    name: &'static str,
    json_key: &'static str,
    required: bool,
    greedy: bool,
) -> PositionalSpec {
    PositionalSpec {
        name,
        json_key,
        kind: ValueKind::Str,
        required,
        greedy,
    }
}

const fn p_u64(name: &'static str, json_key: &'static str) -> PositionalSpec {
    PositionalSpec {
        name,
        json_key,
        kind: ValueKind::U64,
        required: true,
        greedy: false,
    }
}

macro_rules! flag {
    ($name:literal, $key:literal, $kind:ident, $help:literal) => {
        FlagSpec {
            name: $name,
            json_key: $key,
            kind: ValueKind::$kind,
            help: $help,
        }
    };
}

/// The starter command registry. Ordering groups families for help output;
/// resolution is exact-path so order carries no precedence.
pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        path: &["status"],
        lane: RiskLane::Inspect,
        tool: "get_status",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "Daemon and session status summary",
    },
    CommandSpec {
        path: &["whoami"],
        lane: RiskLane::Inspect,
        tool: "whoami",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "The caller's own gate-resolved identity (for provenance)",
    },
    CommandSpec {
        path: &["logs"],
        lane: RiskLane::Inspect,
        tool: "get_logs",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("since-id", "since_id", U64, "cursor: entries after this id"),
            flag!(
                "level",
                "level_filter",
                Str,
                "info|model|agent|error|warn|subagent|debug"
            ),
            flag!("limit", "limit", U64, "max entries (default 100)"),
        ],
        help: "Session log entries, cursor-paged",
    },
    CommandSpec {
        path: &["approval", "pending"],
        lane: RiskLane::Inspect,
        tool: "get_pending_approval",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "The pending approval request, if any",
    },
    CommandSpec {
        path: &["approval", "approve"],
        lane: RiskLane::Authorize,
        tool: "approve",
        seed: "{}",
        positionals: &[p_u64("ID", "id")],
        flags: &[],
        help: "Approve one pending request by id",
    },
    CommandSpec {
        path: &["approval", "deny"],
        lane: RiskLane::Authorize,
        tool: "deny",
        seed: "{}",
        positionals: &[p_u64("ID", "id")],
        flags: &[],
        help: "Deny one pending request by id",
    },
    CommandSpec {
        path: &["approval", "skip"],
        lane: RiskLane::Authorize,
        tool: "skip",
        seed: "{}",
        positionals: &[p_u64("ID", "id")],
        flags: &[],
        help: "Skip one pending request by id",
    },
    CommandSpec {
        path: &["approval", "approve-all"],
        lane: RiskLane::Authorize,
        tool: "approve_all",
        seed: "{}",
        positionals: &[p_u64("ID", "id")],
        flags: &[],
        help: "Blanket-approve the request's whole class (widest approval verb)",
    },
    CommandSpec {
        path: &["input", "pending"],
        lane: RiskLane::Inspect,
        tool: "get_pending_input",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "The pending askHuman question, if any",
    },
    CommandSpec {
        path: &["input", "respond"],
        lane: RiskLane::Act,
        tool: "respond",
        seed: "{}",
        positionals: &[p_str("TEXT", "text", true, true)],
        flags: &[],
        help: "Answer the pending askHuman question",
    },
    CommandSpec {
        path: &["ask"],
        lane: RiskLane::Act,
        tool: "ask_user",
        seed: "{}",
        positionals: &[p_str("QUESTION", "question", true, true)],
        flags: &[
            flag!("header", "header", Str, "short topic chip"),
            flag!(
                "option",
                "__options",
                StrList,
                "choice label (repeatable, max 4)"
            ),
        ],
        help: "Ask the user a blocking structured question (holds up to 900 s)",
    },
    CommandSpec {
        path: &["notify"],
        lane: RiskLane::Act,
        tool: "notify_user",
        seed: "{}",
        positionals: &[p_str("TEXT", "text", true, true)],
        flags: &[
            flag!("title", "title", Str, "short title"),
            flag!("urgency", "urgency", Str, "info|attention|urgent"),
        ],
        help: "Fire-and-forget notification to the user",
    },
    CommandSpec {
        path: &["session", "note"],
        lane: RiskLane::Act,
        tool: "post_session_note",
        seed: "{}",
        positionals: &[p_str("TEXT", "text", true, true)],
        flags: &[flag!("source", "source", Str, "short source label")],
        help: "Post a display-only note into the session transcript",
    },
    CommandSpec {
        path: &["task", "start"],
        lane: RiskLane::Act,
        tool: "start_task",
        seed: "{}",
        positionals: &[p_str("TASK", "task", true, true)],
        flags: &[
            flag!(
                "session",
                "session_id",
                Str,
                "target session (follow-up turn)"
            ),
            flag!(
                "orchestrate",
                "orchestrate",
                Bool,
                "force orchestration mode"
            ),
        ],
        help: "Start an agent task (or queue a follow-up into a session)",
    },
    CommandSpec {
        path: &["agenda", "list"],
        lane: RiskLane::Inspect,
        tool: "agenda_list",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("status", "status", Str, "open|done|retired"),
            flag!("q", "q", Str, "server-side search"),
        ],
        help: "List agenda items (oldest first, with counts)",
    },
    CommandSpec {
        path: &["agenda", "show"],
        lane: RiskLane::Inspect,
        tool: "agenda_item",
        seed: "{}",
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[],
        help: "One agenda item at full detail (id or unique prefix)",
    },
    CommandSpec {
        path: &["agenda", "add"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add","kind":"note"}"#,
        positionals: &[p_str("TITLE", "title", true, true)],
        flags: &[
            flag!("body", "body", Str, "markdown body"),
            flag!("tag", "tags", StrList, "tag (repeatable)"),
            flag!("kind", "kind", Str, "note|task|question"),
            flag!("due-ms", "due_ms", U64, "reminder instant, ms since epoch"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Park a durable item on the agenda",
    },
    CommandSpec {
        path: &["agenda", "answer"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"answer"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("REPLY", "text", true, true),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Answer an agenda question item",
    },
    CommandSpec {
        path: &["agenda", "complete"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"complete"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Mark an agenda item done",
    },
    CommandSpec {
        path: &["agenda", "annotate"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"annotate"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("NOTE", "text", true, true),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Append a note to an agenda item's thread",
    },
    CommandSpec {
        path: &["memory", "search"],
        lane: RiskLane::Inspect,
        tool: "memory_search",
        seed: "{}",
        positionals: &[p_str("QUERY", "query", false, true)],
        flags: &[
            flag!("limit", "limit", U64, "max results (default 10, cap 50)"),
            flag!(
                "candidates",
                "include_candidates",
                Bool,
                "include candidate claims"
            ),
        ],
        help: "Search provenance-labeled memory claims",
    },
    CommandSpec {
        path: &["memory", "read"],
        lane: RiskLane::Inspect,
        tool: "memory_read",
        seed: "{}",
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[],
        help: "Read one claim by id prefix (≥ 8 hex chars)",
    },
    CommandSpec {
        path: &["memory", "propose"],
        lane: RiskLane::Act,
        tool: "memory_propose",
        seed: r#"{"kind":"observation"}"#,
        positionals: &[p_str("STATEMENT", "statement", true, true)],
        flags: &[
            flag!("kind", "kind", Str, "claim kind (default observation)"),
            flag!("sensitivity", "sensitivity", Str, "sensitivity label"),
            flag!("label", "labels", StrList, "label (repeatable)"),
        ],
        help: "Propose a candidate memory claim (curation stays owner-side)",
    },
    CommandSpec {
        path: &["display", "list"],
        lane: RiskLane::Inspect,
        tool: "list_displays",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "Enumerate displays and their session state",
    },
    CommandSpec {
        path: &["display", "screenshot"],
        lane: RiskLane::Inspect,
        tool: "take_screenshot",
        seed: "{}",
        positionals: &[],
        flags: &[flag!(
            "target",
            "display_target",
            Str,
            "user_session, display_99, …"
        )],
        help: "Screenshot a display (returns an image content block)",
    },
    CommandSpec {
        path: &["display", "status"],
        lane: RiskLane::Inspect,
        tool: "display_readiness",
        seed: "{}",
        positionals: &[],
        flags: &[flag!(
            "target",
            "display_target",
            Str,
            "display target to probe"
        )],
        help: "Per-layer display/CU readiness diagnosis",
    },
    // The managed-context recovery family: under rewind-only pressure the
    // dispatcher's pressure gate admits only these (applied to the RESOLVED
    // tool — facade meta names are exempt at the envelope so recovery stays
    // reachable through the facade). On non-managed sessions the underlying
    // tools answer with their own managed-context guidance.
    CommandSpec {
        path: &["context", "anchors"],
        lane: RiskLane::Inspect,
        tool: "list_rewind_anchors",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("session", "session_id", Str, "target session"),
            flag!("limit", "limit", U64, "max anchors"),
            flag!("offset", "offset", U64, "pagination offset"),
            flag!("query", "query", Str, "filter anchors"),
        ],
        help: "List rewind anchors for a managed session",
    },
    CommandSpec {
        path: &["context", "inspect"],
        lane: RiskLane::Inspect,
        tool: "inspect_rewind_anchor",
        seed: "{}",
        positionals: &[p_str("ITEM_ID", "item_id", true, false)],
        flags: &[
            flag!("session", "session_id", Str, "target session"),
            flag!("radius", "radius", U64, "context radius around the anchor"),
        ],
        help: "Inspect one rewind anchor with surrounding context",
    },
    CommandSpec {
        path: &["context", "rewind"],
        lane: RiskLane::Authorize,
        tool: "rewind_context",
        seed: r#"{"anchor":{"position":"before"}}"#,
        positionals: &[
            p_str("ITEM_ID", "anchor.item_id", true, false),
            p_str("REASON", "reason", true, false),
            p_str("PRIMER", "primer", true, true),
        ],
        flags: &[
            flag!("session", "session_id", Str, "target session"),
            flag!(
                "position",
                "anchor.position",
                Str,
                "before (default) or after"
            ),
            flag!(
                "preserve",
                "preserve",
                StrList,
                "fact to carry across (repeatable)"
            ),
        ],
        help: "Rewind a managed session's context to an anchor and resume",
    },
    CommandSpec {
        path: &["context", "backout"],
        lane: RiskLane::Authorize,
        tool: "rewind_backout",
        seed: "{}",
        positionals: &[p_str("RECORD_ID", "record_id", true, false)],
        flags: &[
            flag!("session", "session_id", Str, "target session"),
            flag!("mode", "mode", Str, "restore or fork"),
            flag!("name", "name", Str, "label for the backout"),
            flag!(
                "allow-cache-reset",
                "allow_cache_reset",
                Bool,
                "permit a cache reset"
            ),
        ],
        help: "Back out of a rewind (restore or fork the saved thread)",
    },
    CommandSpec {
        path: &["cu", "elements"],
        lane: RiskLane::Inspect,
        tool: "read_screen",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("target", "display_target", Str, "display target"),
            flag!("format", "format", Str, "text (default) or json"),
        ],
        help: "Read the accessibility element tree",
    },
];

/// The meta-tool names the facade serves.
pub(crate) const FACADE_TOOLS: [&str; 5] = ["inspect", "act", "authorize", "help", "docs"];

/// Params for the three executor meta-tools (`inspect`/`act`/`authorize`).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FacadeRunParams {
    /// The command as an argv array, e.g. ["agenda","list","--status","open"].
    /// Values are literal strings — no shell, no file expansion. Discover
    /// commands with the help tool.
    pub argv: Vec<String>,
}

/// Params for the `help` meta-tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FacadeHelpParams {
    /// Omit for the family map; a family name (e.g. "agenda") or a full
    /// command path (e.g. "agenda add") for its usage lines.
    #[serde(default)]
    pub topic: Option<String>,
}

/// Params for the `docs` meta-tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FacadeDocsParams {
    /// Omit to list the embedded operating skills; a skill name to fetch
    /// its full text.
    #[serde(default)]
    pub skill: Option<String>,
}

pub(crate) fn is_facade_tool(name: &str) -> bool {
    FACADE_TOOLS.contains(&name)
}

/// The three risk-lane executors — the only facade tools that recurse into
/// dispatch (and therefore into the rewind-only pressure gate) with their
/// RESOLVED tool name. `help`/`docs` answer directly and stay behind the
/// envelope-level gate: under pressure they would otherwise add exactly the
/// context the gate exists to prevent.
pub(crate) fn is_facade_executor(name: &str) -> bool {
    matches!(name, "inspect" | "act" | "authorize")
}

/// One resolved, ready-to-dispatch call.
#[derive(Debug)]
pub(crate) struct PlannedCall {
    pub(crate) tool: &'static str,
    pub(crate) args: serde_json::Value,
}

fn argv_from_args(args: &serde_json::Value) -> Result<Vec<String>, String> {
    serde_json::from_value::<FacadeRunParams>(args.clone())
        .map(|params| params.argv)
        .map_err(|_| "missing argv: pass the command as an array of strings".to_string())
}

/// Longest exact path match: `["agenda","list"]` beats a hypothetical
/// one-segment `["agenda"]`; a bare family name resolves to nothing (no
/// prefix dispatch — the parser is fail-closed).
fn resolve_path(argv: &[String]) -> Result<&'static CommandSpec, String> {
    let mut best: Option<&'static CommandSpec> = None;
    for spec in COMMANDS {
        let n = spec.path.len();
        if argv.len() >= n
            && spec.path.iter().zip(argv.iter()).all(|(p, a)| p == a)
            && best.is_none_or(|b| n > b.path.len())
        {
            best = Some(spec);
        }
    }
    best.ok_or_else(|| {
        format!(
            "unknown command {:?} — call the help tool for the command map, or help {{\"topic\":\"<family>\"}}",
            argv.iter().take(2).cloned().collect::<Vec<_>>().join(" ")
        )
    })
}

fn insert_value(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    kind: ValueKind,
    raw: &str,
) -> Result<(), String> {
    // A dotted json_key nests one level ("anchor.item_id" lands inside the
    // "anchor" object) — enough for the wire shapes the registry maps.
    if let Some((outer, inner)) = key.split_once('.') {
        let nested = obj
            .entry(outer.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let nested = nested
            .as_object_mut()
            .ok_or_else(|| format!("{outer}: not an object"))?;
        return insert_value(nested, inner, kind, raw);
    }
    let value = match kind {
        ValueKind::Str => serde_json::Value::String(raw.to_string()),
        ValueKind::U64 => serde_json::Value::from(
            raw.parse::<u64>()
                .map_err(|_| format!("{key}: expected an unsigned integer, got {raw:?}"))?,
        ),
        ValueKind::Bool => serde_json::Value::Bool(true),
        ValueKind::StrList => {
            let entry = obj
                .entry(key.to_string())
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            entry
                .as_array_mut()
                .ok_or_else(|| format!("{key}: not an array"))?
                .push(serde_json::Value::String(raw.to_string()));
            return Ok(());
        }
    };
    obj.insert(key.to_string(), value);
    Ok(())
}

/// Pure argv → arguments builder for one command. No I/O, no environment,
/// no expansion: values are literal strings.
fn build_args(spec: &CommandSpec, rest: &[String]) -> Result<serde_json::Value, String> {
    let mut obj = match serde_json::from_str::<serde_json::Value>(spec.seed) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => {
            return Err(format!(
                "internal: seed for {} is not an object",
                spec.path.join(" ")
            ))
        }
    };

    let mut positional_index = 0usize;
    let mut greedy_parts: Vec<String> = Vec::new();
    let mut greedy_key: Option<&'static str> = None;
    // The conventional end-of-options marker: after a literal "--", every
    // remaining token is positional data, so free-text tails may contain
    // flag-shaped words (["task","start","--","cargo","build","--release"]).
    let mut options_ended = false;
    let mut i = 0usize;
    while i < rest.len() {
        let token = &rest[i];
        if !options_ended && token == "--" {
            options_ended = true;
            i += 1;
            continue;
        }
        if let Some(flag_name) = token.strip_prefix("--").filter(|_| !options_ended) {
            let (flag_name, inline_value) = match flag_name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (flag_name, None),
            };
            let Some(flag) = spec.flags.iter().find(|f| f.name == flag_name) else {
                return Err(format!(
                    "unknown flag --{flag_name} for `{}` — flags: {}",
                    spec.path.join(" "),
                    if spec.flags.is_empty() {
                        "none".to_string()
                    } else {
                        spec.flags
                            .iter()
                            .map(|f| format!("--{}", f.name))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ));
            };
            let raw = if flag.kind == ValueKind::Bool {
                if inline_value.is_some() {
                    return Err(format!("--{flag_name} takes no value"));
                }
                String::new()
            } else if let Some(v) = inline_value {
                v
            } else {
                i += 1;
                rest.get(i)
                    .cloned()
                    .ok_or_else(|| format!("--{flag_name} needs a value"))?
            };
            insert_value(&mut obj, flag.json_key, flag.kind, &raw)?;
        } else if let Some(pos) = spec.positionals.get(positional_index) {
            if pos.greedy {
                greedy_key = Some(pos.json_key);
                greedy_parts.push(token.clone());
            } else {
                insert_value(&mut obj, pos.json_key, pos.kind, token)?;
                positional_index += 1;
            }
        } else if greedy_key.is_some() {
            greedy_parts.push(token.clone());
        } else {
            return Err(format!(
                "unexpected argument {token:?} for `{}`",
                spec.path.join(" ")
            ));
        }
        i += 1;
    }
    if let Some(key) = greedy_key {
        obj.insert(
            key.to_string(),
            serde_json::Value::String(greedy_parts.join(" ")),
        );
    }
    // Dot-aware presence walk, matching `insert_value`'s nesting.
    fn key_present(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
        match key.split_once('.') {
            None => obj.contains_key(key),
            Some((outer, inner)) => obj
                .get(outer)
                .and_then(serde_json::Value::as_object)
                .is_some_and(|nested| key_present(nested, inner)),
        }
    }
    for pos in spec.positionals {
        if pos.required && !key_present(&obj, pos.json_key) {
            return Err(format!(
                "missing required {} for `{}` — usage: {}",
                pos.name,
                spec.path.join(" "),
                usage_line(spec)
            ));
        }
    }
    // The `ask` shape: repeatable --option labels become option objects.
    if let Some(options) = obj.remove("__options") {
        let labels = options.as_array().cloned().unwrap_or_default();
        obj.insert(
            "options".to_string(),
            serde_json::Value::Array(
                labels
                    .into_iter()
                    .map(|label| serde_json::json!({ "label": label }))
                    .collect(),
            ),
        );
    }
    Ok(serde_json::Value::Object(obj))
}

/// Resolve one executor call (`inspect`/`act`/`authorize`) into the command
/// it names. Pure and side-effect-free; both the ingress gates (for the
/// authorization target) and the dispatcher (for execution) call this, and
/// determinism over (meta, args) makes the two resolutions identical.
pub(crate) fn plan_for_meta(meta: &str, args: &serde_json::Value) -> Result<PlannedCall, String> {
    let argv = argv_from_args(args)?;
    if argv.is_empty() {
        return Err("empty argv — call the help tool for the command map".to_string());
    }
    let spec = resolve_path(&argv)?;
    if spec.lane.tool_name() != meta {
        return Err(format!(
            "`{}` is a {} command — call it through the `{}` tool",
            spec.path.join(" "),
            match spec.lane {
                RiskLane::Inspect => "read-only",
                RiskLane::Act => "mutating",
                RiskLane::Authorize => "authority-class",
            },
            spec.lane.tool_name()
        ));
    }
    let rest = &argv[spec.path.len()..];
    let built = build_args(spec, rest)?;
    Ok(PlannedCall {
        tool: spec.tool,
        args: built,
    })
}

/// The gate-side authorization resolver. `None`: not a facade tool (fall
/// through to the fixed per-tool map). `Some(Err)`: fail-closed parse
/// failure — return the error as a tool result, never dispatch.
/// `Some(Ok(op))`: authorize this operation, then dispatch.
pub(crate) fn facade_gate_operation(
    name: &str,
    args: &serde_json::Value,
) -> Option<Result<PeerOperation, String>> {
    match name {
        // The read-only meta surface: the command map and the embedded
        // skills corpus disclose less than get_status already does.
        "help" | "docs" => Some(Ok(PeerOperation::StatsRead)),
        "inspect" | "act" | "authorize" => Some(
            plan_for_meta(name, args).map(|planned| crate::mcp::mcp_tool_operation(planned.tool)),
        ),
        _ => None,
    }
}

/// Whether a facade tool should be ADVERTISED to a principal, given its
/// per-operation decision. `help`/`docs` ride the fixed read operation; an
/// executor lane is advertised when the principal passes at least one of
/// its commands — so `tools/list` keeps the "advertised == something is
/// callable" contract that the per-tool filter maintains for typed tools.
/// `None`: not a facade tool (callers fall back to the fixed name map).
pub(crate) fn facade_tool_advertised(
    name: &str,
    mut allowed: impl FnMut(PeerOperation) -> bool,
) -> Option<bool> {
    match name {
        "help" | "docs" => Some(allowed(PeerOperation::StatsRead)),
        "inspect" | "act" | "authorize" => Some(
            COMMANDS
                .iter()
                .filter(|spec| spec.lane.tool_name() == name)
                .any(|spec| allowed(crate::mcp::mcp_tool_operation(spec.tool))),
        ),
        _ => None,
    }
}

fn usage_line(spec: &CommandSpec) -> String {
    let mut out = spec.path.join(" ");
    for pos in spec.positionals {
        if pos.required {
            out.push_str(&format!(" <{}>", pos.name));
        } else {
            out.push_str(&format!(" [{}]", pos.name));
        }
    }
    for flag in spec.flags {
        match flag.kind {
            ValueKind::Bool => out.push_str(&format!(" [--{}]", flag.name)),
            ValueKind::StrList => out.push_str(&format!(" [--{} V]…", flag.name)),
            _ => out.push_str(&format!(" [--{} V]", flag.name)),
        }
    }
    out
}

/// `help` tool: the registry rendered as text. Top level = the family map;
/// a topic = that family's usage lines (derived from the specs — the help
/// text can never drift from what the parser accepts).
pub(crate) fn render_help(args: &serde_json::Value) -> String {
    let topic = serde_json::from_value::<FacadeHelpParams>(args.clone())
        .ok()
        .and_then(|params| params.topic)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let topic = topic.as_deref();
    match topic {
        None => {
            let mut families: Vec<&str> = Vec::new();
            for spec in COMMANDS {
                let family = spec.path[0];
                if !families.contains(&family) {
                    families.push(family);
                }
            }
            let mut out = String::from(
                "Intendant control facade. Commands run as argv arrays through the \
                 risk-lane tools: inspect (read-only), act (mutating), authorize \
                 (approvals/authority). Values are literal strings — no shell, no file \
                 expansion. Call help {\"topic\":\"<family>\"} for a family's commands; \
                 docs lists the operating skills.\n\nFamilies:\n",
            );
            for family in families {
                let rows: Vec<&CommandSpec> =
                    COMMANDS.iter().filter(|s| s.path[0] == family).collect();
                let lanes: Vec<&str> = {
                    let mut l: Vec<&str> = rows.iter().map(|s| s.lane.tool_name()).collect();
                    l.sort_unstable();
                    l.dedup();
                    l
                };
                out.push_str(&format!(
                    "  {family:<10} {} command(s) via {}\n",
                    rows.len(),
                    lanes.join("/")
                ));
            }
            out
        }
        Some(topic) => {
            let rows: Vec<&CommandSpec> = COMMANDS
                .iter()
                .filter(|s| s.path[0] == topic || s.path.join(" ") == topic)
                .collect();
            if rows.is_empty() {
                return format!(
                    "unknown help topic {topic:?} — call help with no topic for the family map"
                );
            }
            let mut out = String::new();
            for spec in rows {
                out.push_str(&format!(
                    "[{}] {}\n    {}\n",
                    spec.lane.tool_name(),
                    usage_line(spec),
                    spec.help
                ));
                for flag in spec.flags {
                    out.push_str(&format!("      --{:<14} {}\n", flag.name, flag.help));
                }
            }
            out
        }
    }
}

/// `docs` tool: the embedded operate-skills corpus, listed or fetched.
pub(crate) fn render_docs(args: &serde_json::Value) -> String {
    let skill = serde_json::from_value::<FacadeDocsParams>(args.clone())
        .ok()
        .and_then(|params| params.skill)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let skill = skill.as_deref();
    match skill {
        None => {
            let mut out = String::from(
                "Embedded operating skills (fetch one with docs {\"skill\":\"<name>\"}):\n",
            );
            for skill in crate::builtin_skills::BUILTIN_SKILLS {
                let first_heading = skill
                    .skill_md
                    .lines()
                    .find(|l| !l.trim().is_empty() && !l.starts_with("---"))
                    .unwrap_or("")
                    .trim();
                out.push_str(&format!("  {:<24} {}\n", skill.name, first_heading));
            }
            out
        }
        Some(name) => match crate::builtin_skills::BUILTIN_SKILLS
            .iter()
            .find(|s| s.name == name)
        {
            Some(skill) => skill.skill_md.to_string(),
            None => format!("unknown skill {name:?} — call docs with no arguments for the list"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> serde_json::Value {
        serde_json::json!({ "argv": parts })
    }

    /// Every registry row's operation derives from its underlying tool and
    /// stays consistent with its risk lane: inspect rows carry read-class
    /// operations only, and authority-class operations never hide in
    /// inspect/act. This is the "facade lifts no ceiling" pin.
    #[test]
    fn registry_lanes_match_derived_operations() {
        use crate::peer::access_policy::PeerOperation as Op;
        let read_ops = [
            Op::StatsRead,
            Op::SessionInspect,
            Op::AgendaRead,
            Op::MemoryRead,
            Op::DisplayView,
            Op::PeerInspect,
        ];
        for spec in COMMANDS {
            let op = crate::mcp::mcp_tool_operation(spec.tool);
            match spec.lane {
                RiskLane::Inspect => assert!(
                    read_ops.contains(&op),
                    "inspect row `{}` resolves to non-read op {op:?}",
                    spec.path.join(" ")
                ),
                RiskLane::Act | RiskLane::Authorize => {}
            }
            if op == Op::Approval {
                assert_eq!(
                    spec.lane,
                    RiskLane::Authorize,
                    "approval-op row `{}` must ride the authorize lane",
                    spec.path.join(" ")
                );
            }
        }
    }

    #[test]
    fn registry_paths_are_unique_and_non_shadowing() {
        for (i, a) in COMMANDS.iter().enumerate() {
            for b in &COMMANDS[i + 1..] {
                assert_ne!(a.path, b.path, "duplicate command path");
            }
        }
    }

    #[test]
    fn plan_builds_seeded_tagged_args() {
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda", "add", "fix", "the", "roof", "--tag", "house", "--tag", "urgent",
            ]),
        )
        .unwrap();
        assert_eq!(planned.tool, "agenda_op");
        assert_eq!(planned.args["op"], "add");
        assert_eq!(planned.args["kind"], "note");
        assert_eq!(planned.args["title"], "fix the roof");
        assert_eq!(planned.args["tags"], serde_json::json!(["house", "urgent"]));
    }

    #[test]
    fn plan_is_fail_closed_on_unknowns() {
        assert!(plan_for_meta("inspect", &argv(&["no-such"])).is_err());
        assert!(plan_for_meta("inspect", &argv(&["status", "--bogus"])).is_err());
        assert!(plan_for_meta("inspect", &argv(&[])).is_err());
        assert!(plan_for_meta("inspect", &serde_json::json!({})).is_err());
        assert!(plan_for_meta("inspect", &argv(&["approval", "approve", "not-a-number"])).is_err());
    }

    /// Only the executors are exempt from the envelope-level rewind-only
    /// pressure gate (their recursion meets it on the resolved tool);
    /// help/docs answer directly and stay gated (review round 3's P2).
    #[test]
    fn executor_split_pins_which_meta_tools_recurse() {
        for name in ["inspect", "act", "authorize"] {
            assert!(is_facade_executor(name), "{name} is an executor");
            assert!(is_facade_tool(name));
        }
        for name in ["help", "docs"] {
            assert!(!is_facade_executor(name), "{name} answers directly");
            assert!(is_facade_tool(name));
        }
        assert!(!is_facade_executor("get_status"));
    }

    /// The conventional `--` marker ends option parsing so free-text tails
    /// may contain flag-shaped words (review round 2's P2).
    #[test]
    fn double_dash_ends_option_parsing() {
        let planned = plan_for_meta(
            "act",
            &argv(&["task", "start", "--", "run", "cargo", "build", "--release"]),
        )
        .unwrap();
        assert_eq!(planned.args["task"], "run cargo build --release");
        let planned = plan_for_meta(
            "act",
            &argv(&["notify", "--title", "hm", "--", "--help", "is", "confusing"]),
        )
        .unwrap();
        assert_eq!(planned.args["text"], "--help is confusing");
        assert_eq!(planned.args["title"], "hm");
    }

    /// The recovery family builds the nested rewind wire shape and rides
    /// the authorize lane (review round 2's P1: recovery must stay
    /// reachable through the facade under rewind-only pressure).
    #[test]
    fn context_rewind_builds_the_nested_anchor_shape() {
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "context",
                "rewind",
                "item-7",
                "context pressure",
                "resume",
                "from",
                "here",
                "--preserve",
                "the port number",
            ]),
        )
        .unwrap();
        assert_eq!(planned.tool, "rewind_context");
        assert_eq!(planned.args["anchor"]["item_id"], "item-7");
        assert_eq!(planned.args["anchor"]["position"], "before");
        assert_eq!(planned.args["reason"], "context pressure");
        assert_eq!(planned.args["primer"], "resume from here");
        assert_eq!(
            planned.args["preserve"],
            serde_json::json!(["the port number"])
        );
    }

    #[test]
    fn wrong_lane_is_redirected_by_name() {
        let err = plan_for_meta("inspect", &argv(&["approval", "approve", "7"])).unwrap_err();
        assert!(err.contains("`authorize`"), "{err}");
        let err = plan_for_meta("act", &argv(&["status"])).unwrap_err();
        assert!(err.contains("`inspect`"), "{err}");
    }

    #[test]
    fn gate_operation_resolves_per_command() {
        use crate::peer::access_policy::PeerOperation as Op;
        let op = facade_gate_operation("authorize", &argv(&["approval", "approve", "7"]))
            .unwrap()
            .unwrap();
        assert_eq!(op, Op::Approval);
        let op = facade_gate_operation("inspect", &argv(&["status"]))
            .unwrap()
            .unwrap();
        assert_eq!(op, Op::StatsRead);
        assert!(facade_gate_operation("inspect", &argv(&["nope"]))
            .unwrap()
            .is_err());
        assert!(facade_gate_operation("get_status", &serde_json::json!({})).is_none());
        assert_eq!(
            facade_gate_operation("help", &serde_json::json!({}))
                .unwrap()
                .unwrap(),
            Op::StatsRead
        );
    }

    #[test]
    fn ask_options_become_labeled_objects() {
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "ask", "which", "one?", "--option", "A", "--option", "B", "--header", "Pick",
            ]),
        )
        .unwrap();
        assert_eq!(planned.tool, "ask_user");
        assert_eq!(planned.args["question"], "which one?");
        assert_eq!(planned.args["header"], "Pick");
        assert_eq!(
            planned.args["options"],
            serde_json::json!([{ "label": "A" }, { "label": "B" }])
        );
    }

    /// Listing availability derives from the facade's own model — help/docs
    /// at the fixed read operation, each executor lane advertised iff the
    /// principal passes at least one of its commands — so a scoped
    /// principal without runtime.control still discovers the facade
    /// (review round 1's P1: the fixed name map must never hide the
    /// listing from exactly the principals the facade serves).
    #[test]
    fn advertisement_follows_lane_availability_not_the_envelope_default() {
        use crate::peer::access_policy::PeerOperation as Op;
        let read_only = |op: Op| {
            matches!(
                op,
                Op::StatsRead
                    | Op::SessionInspect
                    | Op::AgendaRead
                    | Op::MemoryRead
                    | Op::DisplayView
            )
        };
        assert_eq!(facade_tool_advertised("help", read_only), Some(true));
        assert_eq!(facade_tool_advertised("docs", read_only), Some(true));
        assert_eq!(facade_tool_advertised("inspect", read_only), Some(true));
        assert_eq!(facade_tool_advertised("act", read_only), Some(false));
        assert_eq!(facade_tool_advertised("authorize", read_only), Some(false));
        let approvals_only = |op: Op| op == Op::Approval;
        assert_eq!(
            facade_tool_advertised("authorize", approvals_only),
            Some(true)
        );
        assert_eq!(
            facade_tool_advertised("inspect", approvals_only),
            Some(false)
        );
        let nothing = |_: Op| false;
        assert_eq!(facade_tool_advertised("help", nothing), Some(false));
        assert_eq!(facade_tool_advertised("get_status", read_only), None);
    }

    #[test]
    fn help_renders_families_and_topics_from_the_registry() {
        let top = render_help(&serde_json::json!({}));
        assert!(top.contains("agenda"));
        assert!(top.contains("approval"));
        let family = render_help(&serde_json::json!({ "topic": "approval" }));
        assert!(family.contains("approval approve <ID>"), "{family}");
        assert!(family.contains("[authorize]"), "{family}");
        let unknown = render_help(&serde_json::json!({ "topic": "zzz" }));
        assert!(unknown.contains("unknown help topic"));
    }

    #[test]
    fn docs_lists_and_fetches_embedded_skills() {
        let list = render_docs(&serde_json::json!({}));
        assert!(list.contains("intendant-cli"));
        let one = render_docs(&serde_json::json!({ "skill": "intendant-cli" }));
        assert!(
            one.len() > 1000,
            "skill body expected, got {} bytes",
            one.len()
        );
        assert!(render_docs(&serde_json::json!({ "skill": "zzz" })).contains("unknown skill"));
    }
}
