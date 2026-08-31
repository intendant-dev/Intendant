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

mod registry;
pub(crate) use registry::*;

/// The meta-tool names the facade serves.
pub(crate) const FACADE_TOOLS: [&str; 6] =
    ["inspect", "act", "authorize", "help", "docs", "events"];

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
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct FacadeDocsParams {
    /// Omit to list the embedded operating skills; a skill name to fetch
    /// its full text (plus its support-file manifest, when it has one).
    #[serde(default)]
    pub skill: Option<String>,
    /// A support-file path from the skill's manifest (e.g.
    /// "references/query-recipes.md") to fetch that file instead.
    #[serde(default)]
    pub file: Option<String>,
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

/// One resolved, ready-to-dispatch call. `spec` rides along so the
/// dispatcher can substitute sentinels at the exact argument paths the
/// registry declares — never inside caller-owned opaque JSON — and
/// `caller_defaults` names the top-level keys whose seed-declared
/// caller-identity default the argv left unfilled: they are absent
/// from `args` until the dispatcher fills the caller's identity, so no
/// input string is reserved.
#[derive(Debug)]
pub(crate) struct PlannedCall {
    pub(crate) tool: &'static str,
    pub(crate) args: serde_json::Value,
    pub(crate) spec: &'static CommandSpec,
    pub(crate) caller_defaults: Vec<String>,
}

fn argv_from_args(args: &serde_json::Value) -> Result<Vec<String>, String> {
    // Deserialized from a reference: cloning the whole params Value
    // first would double a near-cap request's allocation during the
    // gate's PRE-auth resolution (security review).
    use serde::Deserialize as _;
    FacadeRunParams::deserialize(args)
        .map(|params| params.argv)
        .map_err(|_| "missing argv: pass the command as an array of strings".to_string())
}

/// Rewrite ctl's alias spellings onto their canonical paths before
/// resolution: the family token first ([`FAMILY_ALIASES`]), then one
/// whole-path rewrite ([`COMMAND_ALIASES`]) — so `browsers ls` becomes
/// `browser list` and resolves to the canonical row's own vocabulary.
/// Exact-match and fail-closed like the resolver itself; the registry
/// pin holds that no alias shadows a registered spelling.
fn normalize_aliases(mut argv: Vec<String>) -> Vec<String> {
    let Some(first) = argv.first() else {
        return argv;
    };
    if let Some((_, canonical)) = FAMILY_ALIASES.iter().find(|(alias, _)| alias == first) {
        argv[0] = (*canonical).to_string();
    }
    for (alias, canonical) in COMMAND_ALIASES {
        if argv.len() >= alias.len() && argv[..alias.len()].iter().eq(alias.iter()) {
            let mut rewritten: Vec<String> =
                canonical.iter().map(|seg| (*seg).to_string()).collect();
            rewritten.extend(argv.drain(alias.len()..));
            return rewritten;
        }
    }
    argv
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
        // Reflected tokens are char-capped: an unknown command's argv can
        // be arbitrarily large, and this message must stay small even when
        // it is the only output a pressured session receives.
        let shown = |token: &String| -> String {
            if token.chars().count() > 48 {
                let mut cut: String = token.chars().take(48).collect();
                cut.push('…');
                cut
            } else {
                token.clone()
            }
        };
        format!(
            "unknown command {:?} — call the help tool for the command map, or help {{\"topic\":\"<family>\"}}",
            argv.iter().take(2).map(shown).collect::<Vec<_>>().join(" ")
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
    let value =
        match kind {
            ValueKind::Str => serde_json::Value::String(raw.to_string()),
            ValueKind::U64 => serde_json::Value::from(raw.parse::<u64>().map_err(|_| {
                format!(
                    "{key}: expected an unsigned integer, got {:?}",
                    shown_value(raw)
                )
            })?),
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
            ValueKind::Json => serde_json::from_str(raw)
                .map_err(|e| format!("{key}: expected literal JSON ({e})"))?,
            ValueKind::JsonOrText => serde_json::from_str(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
            // The clock/timezone-dependent forms resolve at dispatch;
            // planning stays pure by carrying the raw text.
            ValueKind::When => serde_json::Value::String(format!("__when:{raw}")),
            ValueKind::Interval => {
                // Bounded before ctl's parser, whose refusals echo the
                // raw value — planning runs pre-auth (security review).
                if raw.len() > 64 {
                    return Err(format!(
                        "{key}: interval too long ({} bytes; try 45m, 2h, 7d, 1w, or ms)",
                        raw.len()
                    ));
                }
                serde_json::Value::from(crate::ctl::parse_duration_ms(raw)?)
            }
        };
    obj.insert(key.to_string(), value);
    Ok(())
}

/// `ancestor` covers `key` when they are equal or `ancestor` is a
/// dotted prefix of `key` (`patch` covers `patch.title`; `patch` does
/// not cover `patchwork`).
fn path_covers(ancestor: &str, key: &str) -> bool {
    key == ancestor
        || (key.len() > ancestor.len()
            && key.starts_with(ancestor)
            && key.as_bytes()[ancestor.len()] == b'.')
}

/// Merge a freshly built positional value into the arguments while
/// keeping every value at a flag-written dotted path — ctl's flag
/// precedence field by field, so a positional parent object cannot
/// clobber `--title` while its own other fields still land.
fn merge_flag_precedent(
    existing: &mut serde_json::Map<String, serde_json::Value>,
    incoming: serde_json::Map<String, serde_json::Value>,
    flag_paths: &std::collections::HashSet<&'static str>,
    prefix: &str,
) {
    for (key, value) in incoming {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if flag_paths.contains(path.as_str()) {
            // the explicit flag's value stands
            continue;
        }
        if flag_paths
            .iter()
            .any(|flag_path| path_covers(&path, flag_path))
        {
            // flag paths live DEEPER under this key: recurse so they
            // survive while the incoming object's other fields land (a
            // non-object here cannot hold them, so it yields to the
            // flag entirely).
            if let serde_json::Value::Object(src) = value {
                let dst = existing
                    .entry(key)
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                if let Some(dst) = dst.as_object_mut() {
                    merge_flag_precedent(dst, src, flag_paths, &path);
                }
            }
            continue;
        }
        existing.insert(key, value);
    }
}

/// Pure argv → arguments builder for one command. No I/O, no environment,
/// no expansion: values are literal strings. Returns the built
/// object plus the top-level keys whose seed declared a caller-identity
/// default (`"__caller"`) that the argv did NOT override: those keys are
/// REMOVED from the object and returned by name, so the dispatcher can
/// fill the caller's identity out-of-band — a caller's own literal
/// `"__caller"` value stays caller data (review round 25).
fn build_args(
    spec: &CommandSpec,
    rest: &[String],
) -> Result<(serde_json::Value, Vec<String>), String> {
    let seed = match serde_json::from_str::<serde_json::Value>(spec.seed) {
        Ok(serde_json::Value::Object(map)) => map,
        _ => {
            return Err(format!(
                "internal: seed for {} is not an object",
                spec.path.join(" ")
            ))
        }
    };
    let mut obj = seed.clone();
    // Top-level keys the ARGV wrote (flags, positionals, the greedy
    // tail) — the discriminator between a seed default and an explicit
    // caller value that happens to spell the same string. Flag-written
    // paths are tracked separately and in FULL: ctl gives an explicit
    // flag precedence over its positional twin, and the precedence
    // decision must distinguish `patch.title` from the whole `patch`
    // object — an outer-key check would drop a positional PATCH beside
    // --title, or block `anchor.item_id` beside --position (round 37).
    let mut written: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    let mut flag_written: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();
    // Which flag NAME wrote each scalar key: two synonymous spellings
    // for one field (--session/--session-id) refuse together instead of
    // last-wins — ctl's or_else picks a fixed winner regardless of argv
    // order, and silently dispatching the other spelling's value would
    // diverge from it. Repeatable list flags append and stay exempt.
    let mut flag_name_by_key: std::collections::HashMap<&'static str, &'static str> =
        std::collections::HashMap::new();
    fn outer(key: &'static str) -> &'static str {
        key.split('.').next().unwrap_or(key)
    }

    let mut positional_index = 0usize;
    let mut greedy_parts: Vec<String> = Vec::new();
    let mut greedy_key: Option<&'static str> = None;
    let mut greedy_kind = ValueKind::Str;
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
            if flag.kind != ValueKind::StrList {
                if let Some(prev) = flag_name_by_key.get(flag.json_key) {
                    if *prev != flag.name {
                        return Err(format!(
                            "pass --{prev} or --{}, not both — they set the same field",
                            flag.name
                        ));
                    }
                }
                flag_name_by_key.insert(flag.json_key, flag.name);
            }
            // A full-object flag beside its own nested flags fills only
            // what they did not set — in EITHER argv order (the audio
            // --args precedent): the specific spelling wins, so
            // `--agent codex --agent-config '{"dial":..}'` keeps both.
            let nested_flags = flag_written.iter().any(|flag_path| {
                path_covers(flag.json_key, flag_path) && *flag_path != flag.json_key
            });
            if nested_flags {
                let mut scratch = serde_json::Map::new();
                insert_value(&mut scratch, flag.json_key, flag.kind, &raw)?;
                merge_flag_precedent(&mut obj, scratch, &flag_written, "");
            } else {
                insert_value(&mut obj, flag.json_key, flag.kind, &raw)?;
            }
            written.insert(outer(flag.json_key));
            flag_written.insert(flag.json_key);
        } else if let Some(pos) = spec.positionals.get(positional_index) {
            if pos.greedy {
                greedy_key = Some(pos.json_key);
                greedy_kind = pos.kind;
                greedy_parts.push(token.clone());
            } else {
                // ctl's precedence for dual spellings: an explicit flag
                // beats its positional twin in either order. A flag
                // path covering this positional's path skips it (the
                // slot is still consumed); flag paths NESTED UNDER it
                // (--title beside the whole PATCH object) deep-merge
                // instead — the positional's other fields land, the
                // flag's values stand.
                let covered = flag_written
                    .iter()
                    .any(|flag_path| path_covers(flag_path, pos.json_key));
                if !covered {
                    let nested_flags = flag_written
                        .iter()
                        .any(|flag_path| path_covers(pos.json_key, flag_path));
                    if nested_flags {
                        let mut scratch = serde_json::Map::new();
                        insert_value(&mut scratch, pos.json_key, pos.kind, token)?;
                        merge_flag_precedent(&mut obj, scratch, &flag_written, "");
                    } else {
                        insert_value(&mut obj, pos.json_key, pos.kind, token)?;
                    }
                    written.insert(outer(pos.json_key));
                }
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
        let value = match greedy_kind {
            // A rest-list keeps word boundaries — the shape a command
            // argv needs; a greedy Str stays the joined free-text tail.
            ValueKind::StrList => serde_json::Value::Array(
                greedy_parts
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
            _ => serde_json::Value::String(greedy_parts.join(" ")),
        };
        // ctl's precedence: an explicit flag beats the free-text tail
        // (`--task` wins over the positional, which ctl silently
        // ignores beside it) — never the other way around, which would
        // dispatch a different value than the caller's explicit flag.
        if !flag_written
            .iter()
            .any(|flag_path| path_covers(flag_path, key))
        {
            obj.insert(key.to_string(), value);
            written.insert(outer(key));
        }
    }
    // Seed-declared caller-identity defaults the argv left alone: strip
    // them and hand the key names back for the dispatcher's out-of-band
    // fill (an argv-written value — even the literal "__caller" — stays).
    let mut caller_defaults = Vec::new();
    for (key, value) in &seed {
        if value.as_str() == Some("__caller") && !written.contains(key.as_str()) {
            obj.remove(key);
            caller_defaults.push(key.clone());
        }
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
    // The `terminal write` shape: --no-enter maps onto the tool's
    // enter=false (a positive flag would read "submit" — the CLI-shaped
    // negative reads better in argv).
    if obj.remove("__no_enter").is_some() {
        obj.insert("enter".to_string(), serde_json::Value::Bool(false));
    }
    // The `ask` shape: repeatable --option values become option objects,
    // splitting ctl's "Label[:description]" form the way
    // `ctl::ask_option_entries` does.
    if let Some(options) = obj.remove("__options") {
        let raw_options = options.as_array().cloned().unwrap_or_default();
        obj.insert(
            "options".to_string(),
            serde_json::Value::Array(option_entries(&raw_options)),
        );
    }
    // ctl's `session note --image PATH` reads and base64-encodes the
    // file client-side — same exclusion class as the preview files.
    if obj.remove("__image_file_client").is_some() {
        return Err(
            "--image reads and encodes the file client-side (a ctl behavior the facade \
             excludes); base64 the content yourself and pass --images \
             '[{\"media_type\":..,\"data\":..,\"name\"?:..}]'"
                .to_string(),
        );
    }
    // ctl's `--schema FILE|-` reads the multi-question JSON from the
    // caller's file or stdin — client I/O the pure registry excludes;
    // the refusal names the inline form.
    if obj.remove("__schema_client").is_some() {
        return Err(
            "--schema reads the question file (or stdin) client-side (a ctl behavior the \
             facade excludes); pass the questions inline with --questions '[...]'"
                .to_string(),
        );
    }
    // ctl's file-backed preview forms read files CLIENT-side (the
    // daemon deliberately accepts inline content only — a sandboxed
    // agent must not make the unsandboxed daemon read arbitrary
    // paths), so they refuse by name with the inline alternative.
    if obj.remove("__preview_file_client").is_some() {
        return Err(
            "--preview-html/--preview-image read the referenced file client-side (a ctl \
             behavior the facade excludes); inline the content yourself and pass \
             --previews '[{\"label\":..,\"html\":..}]' or \
             '[{\"label\":..,\"image\":..,\"media_type\":..}]'"
                .to_string(),
        );
    }
    // ctl's inline preview cards: repeatable --preview-text LABEL=VALUE
    // with ctl's own split and preview cap.
    if let Some(specs) = obj.remove("__preview_text") {
        if obj.contains_key("previews") {
            return Err(
                "pass --preview-text cards or the --previews JSON array, not both".to_string(),
            );
        }
        let mut previews = Vec::new();
        for spec_value in specs.as_array().cloned().unwrap_or_default() {
            let raw = spec_value.as_str().unwrap_or_default();
            let split = raw
                .split_once('=')
                .map(|(label, text)| (label.trim(), text.trim()));
            let Some((label, text)) =
                split.filter(|(label, text)| !label.is_empty() && !text.is_empty())
            else {
                return Err(format!("--preview-text expects LABEL=VALUE, got '{raw}'"));
            };
            previews.push(serde_json::json!({ "label": label, "text": text }));
        }
        if previews.len() > crate::mcp::ASK_USER_MAX_PREVIEWS {
            return Err(format!(
                "too many previews: {} (max {})",
                previews.len(),
                crate::mcp::ASK_USER_MAX_PREVIEWS
            ));
        }
        obj.insert("previews".to_string(), serde_json::Value::Array(previews));
    }
    // ctl's `--pick MIN[-MAX]` becomes the explicit pick bounds — the
    // same split and refusals as ctl's own parse.
    if let Some(pick) = obj.remove("__pick") {
        if obj.contains_key("multi_select") {
            return Err("--pick replaces --multi — provide one or the other".to_string());
        }
        if obj.contains_key("pick_min") || obj.contains_key("pick_max") {
            return Err(
                "pass --pick MIN[-MAX] or the --pick-min/--pick-max pair, not both".to_string(),
            );
        }
        let (min, max) = crate::ctl::parse_pick_spec(pick.as_str().unwrap_or_default())?;
        obj.insert("pick_min".to_string(), serde_json::Value::from(min));
        obj.insert("pick_max".to_string(), serde_json::Value::from(max));
    }
    // `--no-free-text` is the explicit option-only form (free_text:
    // false); bare `--free-text` documents intent, exactly like ctl.
    if obj.remove("__no_free_text").is_some() {
        if obj.contains_key("free_text") {
            return Err("pass --free-text or --no-free-text, not both".to_string());
        }
        obj.insert("free_text".to_string(), serde_json::Value::Bool(false));
    }
    // ctl's repeatable `--env KEY=VALUE` becomes the tool's env object.
    if let Some(pairs) = obj.remove("__env_kv") {
        let mut env = serde_json::Map::new();
        for pair in pairs.as_array().cloned().unwrap_or_default() {
            let Some((key, value)) = pair.as_str().and_then(|p| p.split_once('=')) else {
                return Err("--env expects KEY=VALUE".to_string());
            };
            env.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        obj.insert("env".to_string(), serde_json::Value::Object(env));
    }
    // ctl's `--allow-dirty` is the negative of the tool's require_clean
    // — contradicting an explicit --require-clean refuses instead of
    // silently allowing a dirty tree (round 40).
    if obj.remove("__allow_dirty").is_some() {
        if flag_written.contains("require_clean") {
            return Err("pass --allow-dirty or --require-clean, not both".to_string());
        }
        obj.insert("require_clean".to_string(), serde_json::Value::Bool(false));
    }
    // ctl's `--one-shot` is the negative of the halt's persistent field
    // — same contradiction guard.
    if obj.remove("__one_shot").is_some() {
        if flag_written.contains("persistent") {
            return Err("pass --one-shot or --persistent, not both".to_string());
        }
        obj.insert("persistent".to_string(), serde_json::Value::Bool(false));
    }
    // ctl's task-start mode pair: bare --orchestrate / --direct set the
    // tri-state orchestrate field; both together is a contradiction.
    let wants_orchestrate = obj.remove("__orchestrate").is_some();
    let wants_direct = obj.remove("__direct").is_some();
    match (wants_orchestrate, wants_direct) {
        (true, true) => return Err("pass --orchestrate or --direct, not both".to_string()),
        (true, false) => {
            obj.insert("orchestrate".to_string(), serde_json::Value::Bool(true));
        }
        (false, true) => {
            obj.insert("orchestrate".to_string(), serde_json::Value::Bool(false));
        }
        (false, false) => {}
    }
    // ctl's patch clears: --clear-tags empties the tag list, --clear-due
    // nulls the due instant (the merge-patch's explicit-clear forms).
    let clear_tags = obj.remove("__clear_tags").is_some();
    let clear_due = obj.remove("__clear_due").is_some();
    if clear_tags || clear_due {
        let patch = obj
            .entry("patch".to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let patch = patch
            .as_object_mut()
            .ok_or_else(|| "patch: not an object".to_string())?;
        if clear_tags {
            patch.insert("tags".to_string(), serde_json::Value::Array(Vec::new()));
        }
        if clear_due {
            patch.insert("due_ms".to_string(), serde_json::Value::Null);
        }
    }
    // ctl's `[TYPE:]LOCATOR` ref grammar: a recognized type prefix is
    // decoded and stripped (an explicit --type overrides the prefix, but
    // a recognized prefix still leaves the locator); with neither, a
    // http(s) locator infers url. ctl's remaining inferences probe the
    // client filesystem — meaningless here — so they refuse in ctl's
    // own words instead.
    if let Some(raw) = obj.remove("__ref_locator") {
        let raw = raw.as_str().unwrap_or_default();
        let explicit = obj
            .get("ref_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let (ref_type, locator) = ref_type_and_locator(raw, explicit.as_deref())?;
        obj.insert("ref_type".to_string(), serde_json::Value::String(ref_type));
        obj.insert("locator".to_string(), serde_json::Value::String(locator));
    }
    // ctl's `agenda add` kind vocabulary: an explicit --kind wins (its
    // value validated and lowercased in ctl's words, the selectors
    // ignored beside it — ctl's own precedence); otherwise the
    // valueless --note/--task shorthands pick the kind, refusing the
    // contradictory pair.
    let kind_note = obj.remove("__kind_note").is_some();
    let kind_task = obj.remove("__kind_task").is_some();
    if let Some(explicit) = obj.remove("__kind_explicit") {
        let raw = explicit.as_str().unwrap_or_default();
        let kind = match raw.trim().to_ascii_lowercase().as_str() {
            kind @ ("note" | "task" | "question") => kind.to_string(),
            other => return Err(format!("unknown kind '{other}' (note, task, or question)")),
        };
        obj.insert("kind".to_string(), serde_json::Value::String(kind));
    } else if kind_note && kind_task {
        return Err("pass --note or --task, not both".to_string());
    } else if kind_note {
        obj.insert(
            "kind".to_string(),
            serde_json::Value::String("note".to_string()),
        );
    } else if kind_task {
        obj.insert(
            "kind".to_string(),
            serde_json::Value::String("task".to_string()),
        );
    }
    // ctl's `remote wait --for` budgets a CLIENT-side chunking loop;
    // the facade waits one bounded server chunk, so the ctl spelling
    // maps through only within the chunk cap.
    if let Some(budget) = obj.remove("__wait_for") {
        if obj.contains_key("wait_s") {
            return Err("pass --for or --wait-s, not both".to_string());
        }
        let seconds = budget.as_u64().unwrap_or_default();
        if !(1..=60).contains(&seconds) {
            return Err(
                "--for budgets ctl's client-side wait loop — the facade waits one bounded \
                 chunk (1-60s); pass --for 60 or less and re-invoke until the job is terminal"
                    .to_string(),
            );
        }
        obj.insert("wait_s".to_string(), serde_json::Value::from(seconds));
    }
    // ctl's park-time ref gesture on `agenda add`/`agenda ask`:
    // repeatable `--ref [TYPE:]LOCATOR`, with `--must-read`/`--label`
    // applying to every ref of this park, built into the same atomic
    // refs array the --refs JSON form carries.
    let park_must_read = obj.remove("__park_must_read").is_some();
    let park_label = obj.remove("__park_label");
    if let Some(specs) = obj.remove("__park_refs") {
        if obj.contains_key("refs") {
            return Err("pass --ref specs or the --refs JSON array, not both".to_string());
        }
        let mut refs = Vec::new();
        for spec_value in specs.as_array().cloned().unwrap_or_default() {
            let raw = spec_value.as_str().unwrap_or_default();
            let (ref_type, locator) = ref_type_and_locator(raw, None)?;
            let mut entry = serde_json::Map::new();
            entry.insert("ref_type".to_string(), serde_json::Value::String(ref_type));
            entry.insert("locator".to_string(), serde_json::Value::String(locator));
            if park_must_read {
                entry.insert("must_read".to_string(), serde_json::Value::Bool(true));
            }
            if let Some(label) = park_label.as_ref().and_then(serde_json::Value::as_str) {
                entry.insert(
                    "label".to_string(),
                    serde_json::Value::String(label.to_string()),
                );
            }
            refs.push(serde_json::Value::Object(entry));
        }
        obj.insert("refs".to_string(), serde_json::Value::Array(refs));
    } else if park_must_read || park_label.is_some() {
        return Err("--must-read/--label describe refs: pass --ref too".to_string());
    }
    // ctl's repeatable `--binding-ref TYPE:PATH` reads and digests the
    // referenced file CLIENT-side into the sealed manifest — a
    // filesystem read the pure registry must never perform (and the
    // digest must bind what the CALLER saw, not what the daemon can
    // read). Registered so the refusal explains the working
    // alternative instead of an unknown-flag error.
    if obj.remove("__binding_ref_client").is_some() {
        return Err(
            "--binding-ref digests the referenced file client-side (a ctl behavior the \
             facade excludes); compute each sha256 yourself and pass --binding-refs \
             '[{\"locator\":..,\"sha256\":..}]'"
                .to_string(),
        );
    }
    // Attest's `--ref` is the same class: ctl hashes the file into the
    // attestation pin — "the pin says what THIS side read".
    if obj.remove("__attest_ref_client").is_some() {
        return Err(
            "--ref hashes the referenced file client-side into the attestation pin (a \
             ctl behavior the facade excludes); compute each sha256 yourself and pass \
             --refs '[{\"locator\":..,\"sha256\":..}]'"
                .to_string(),
        );
    }
    // ctl's `agenda start --goal-run`: the autonomous shape sends
    // interactive:false; absent stays absent on the wire (the daemon
    // defaults interactive, and on a standing manifest absent means
    // "fire as approved").
    if obj.remove("__goal_run").is_some() {
        if obj.contains_key("interactive") {
            return Err("--goal-run is the autonomous shape — drop --interactive".to_string());
        }
        obj.insert("interactive".to_string(), serde_json::Value::Bool(false));
    }
    // ctl's repeatable `--dial-approve CATEGORY=RULE` builds the
    // launch dial's approvals object (ctl's own split and wording).
    if let Some(specs) = obj.remove("__dial_approve") {
        let mut approvals = serde_json::Map::new();
        for spec_value in specs.as_array().cloned().unwrap_or_default() {
            let spec_text = spec_value.as_str().unwrap_or_default();
            let split = spec_text
                .split_once('=')
                .map(|(category, rule)| (category.trim(), rule.trim()));
            let Some((category, rule)) = split.filter(|(c, r)| !c.is_empty() && !r.is_empty())
            else {
                return Err(format!(
                    "--dial-approve wants CATEGORY=RULE (e.g. network=deny), got '{spec_text}'"
                ));
            };
            approvals.insert(
                category.to_string(),
                serde_json::Value::String(rule.to_string()),
            );
        }
        if !approvals.is_empty() {
            let agent_config = obj
                .entry("agent_config".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            let agent_config = agent_config
                .as_object_mut()
                .ok_or_else(|| "agent_config: not an object".to_string())?;
            let dial = agent_config
                .entry("dial".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            let dial = dial
                .as_object_mut()
                .ok_or_else(|| "dial: not an object".to_string())?;
            // Merged per category into whatever --agent-config already
            // carried: the specific --dial-approve spelling wins its own
            // categories, unspecified entries survive (round 42 — a
            // wholesale insert silently dropped them).
            let approvals_slot = dial
                .entry("approvals".to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            let existing = approvals_slot
                .as_object_mut()
                .ok_or_else(|| "dial.approvals: not an object".to_string())?;
            for (category, rule) in approvals {
                existing.insert(category, rule);
            }
        }
    }
    // ctl's `--remove` edge forms flip the seeded add op to its remove
    // twin. The relates remove drops/refuses --kind (it types the link
    // being ADDED — ctl's own refusal), and the ref remove drops the
    // add-only fields (the remove op's strict shape rejects them).
    if obj.remove("__remove_relies").is_some() {
        obj.insert(
            "op".to_string(),
            serde_json::Value::String("remove_relies_on".to_string()),
        );
    }
    if obj.remove("__remove_relates").is_some() {
        if obj.contains_key("link_kind") {
            return Err(
                "agenda relates --kind types the link being added; drop it with --remove"
                    .to_string(),
            );
        }
        obj.insert(
            "op".to_string(),
            serde_json::Value::String("remove_relates_to".to_string()),
        );
    }
    if obj.remove("__remove_ref").is_some() {
        obj.insert(
            "op".to_string(),
            serde_json::Value::String("remove_ref".to_string()),
        );
        obj.remove("must_read");
        obj.remove("label");
    }
    // ctl's trigger pair: `--on-unblock` is the dependency-gated shape,
    // `--on-item-match KIND:TAG[,TAG…]` the item-match shape; one
    // trigger only, and a triggered manifest is trigger OR cadence.
    let on_unblock = obj.remove("__on_unblock").is_some();
    let on_item_match = obj.remove("__on_item_match");
    if on_unblock || on_item_match.is_some() {
        if obj.contains_key("trigger") {
            return Err(
                "pass one trigger form — --on-unblock, --on-item-match, or --trigger".to_string(),
            );
        }
        if on_unblock && on_item_match.is_some() {
            return Err("pass --on-unblock OR --on-item-match, not both".to_string());
        }
        let trigger = if on_unblock {
            serde_json::json!({ "kind": "on_unblock" })
        } else {
            let spec = on_item_match
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let Some((kind, tags)) = spec.split_once(':') else {
                return Err(
                    "--on-item-match takes KIND:TAG[,TAG...] (e.g. question:gate)".to_string(),
                );
            };
            let tags: Vec<serde_json::Value> = tags
                .split(',')
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(|tag| serde_json::Value::String(tag.to_string()))
                .collect();
            serde_json::json!({
                "kind": "on_item_match",
                "item_kind": kind.trim(),
                "tags": tags,
            })
        };
        obj.insert("trigger".to_string(), trigger);
    }
    // A schedule is cadenced OR triggered (ctl's own refusal), and a
    // triggered manifest's omitted fire instant is the ARM FLOOR —
    // "armed on approval", filled with the dispatch clock via the
    // `__now` sentinel (the planner is pure and reads no clock).
    if spec.tool == "agenda_op"
        && obj.get("op").and_then(serde_json::Value::as_str) == Some("propose_effect")
    {
        if !obj.contains_key("goal") {
            return Err("agenda schedule requires --goal TEXT".to_string());
        }
        if obj.contains_key("trigger") && obj.contains_key("recurrence") {
            return Err(
                "a manifest is cadenced OR triggered: pass --every or a trigger flag, not both"
                    .to_string(),
            );
        }
        if !obj.contains_key("fire_at_ms") {
            if obj.contains_key("trigger") {
                obj.insert(
                    "fire_at_ms".to_string(),
                    serde_json::Value::String("__now".to_string()),
                );
            } else {
                return Err(
                    "agenda schedule requires --at WHEN (or FIRE_AT_MS in epoch ms) — or a trigger flag, whose omitted instant means armed on approval"
                        .to_string(),
                );
            }
        }
    }
    // ctl's `agenda place ID --remove` removes the CURRENT placement:
    // the empty parent id tells the daemon to resolve it (the
    // sole-blocker idiom).
    if obj.remove("__unplace").is_some() {
        if obj.contains_key("under") {
            return Err("pass UNDER to re-parent or --remove to unplace, not both".to_string());
        }
        obj.insert(
            "op".to_string(),
            serde_json::Value::String("remove_part_of".to_string()),
        );
        obj.remove("under");
        obj.insert(
            "parent_id".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    // ctl's `--region x,y,width,height` (normalized 0-1 floats) becomes
    // the tool's region object.
    if let Some(csv) = obj.remove("__region_csv") {
        if obj.contains_key("region") {
            return Err("pass one region — either the REGION positional or --region".to_string());
        }
        let region = region_from_csv("region", &csv)?;
        obj.insert("region".to_string(), region);
    }
    // ctl's `shared show --focus x,y,w,h` opens the view already
    // focused — the same CSV vocabulary as `shared focus --region`,
    // filling the tool's focus_region object.
    if let Some(csv) = obj.remove("__focus_csv") {
        if obj.contains_key("focus_region") {
            return Err(
                "pass one focus — either --focus x,y,w,h or --focus-region JSON".to_string(),
            );
        }
        let region = region_from_csv("focus", &csv)?;
        obj.insert("focus_region".to_string(), region);
    }
    // ctl's `audio spawn --args '{...}'` passes the tool object whole;
    // entries fill only keys the decomposed flags did not set.
    if let Some(args_obj) = obj.remove("__audio_args") {
        let Some(map) = args_obj.as_object() else {
            return Err("--args expects a JSON object".to_string());
        };
        for (key, value) in map {
            if !obj.contains_key(key) {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
    // ctl's list lifecycle selectors, in ctl's own precedence (--all
    // lifts the status filter entirely; then --done, --retired, --open
    // rewrite the seeded open default).
    let list_open = obj.remove("__list_open").is_some();
    let list_done = obj.remove("__list_done").is_some();
    let list_retired = obj.remove("__list_retired").is_some();
    if obj.remove("__all_statuses").is_some() {
        obj.remove("status");
    } else if list_done {
        obj.insert(
            "status".to_string(),
            serde_json::Value::String("done".to_string()),
        );
    } else if list_retired {
        obj.insert(
            "status".to_string(),
            serde_json::Value::String("retired".to_string()),
        );
    } else if list_open {
        obj.insert(
            "status".to_string(),
            serde_json::Value::String("open".to_string()),
        );
    }
    // ctl's structured `agenda ask` flags: any of --option/--multi/
    // --pick/--header makes the decision-card form (ctl's own
    // detector), building the one-question op:ask park with the same
    // vocabulary and refusals as `ctl::agenda_ask_args`.
    let agenda_options = obj.remove("__agenda_options");
    let agenda_pick = obj.remove("__agenda_pick");
    let agenda_multi = obj.remove("__agenda_multi").is_some();
    let agenda_header = obj.remove("__agenda_header");
    let agenda_consequence = obj.remove("__agenda_consequence");
    if agenda_options.is_some() || agenda_pick.is_some() || agenda_multi || agenda_header.is_some()
    {
        if obj.contains_key("__ask_questions") {
            return Err("use either the --option flags or --questions, not both".to_string());
        }
        let options = agenda_options
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        if options.is_empty() {
            return Err("--multi/--pick/--header describe options: pass --option too".to_string());
        }
        if options.len() > crate::mcp::ASK_USER_MAX_OPTIONS {
            return Err(format!(
                "too many options: {} (max {}; omit --option for free-text only)",
                options.len(),
                crate::mcp::ASK_USER_MAX_OPTIONS
            ));
        }
        let Some(text) = obj.remove("title") else {
            return Err("agenda ask requires question text".to_string());
        };
        let mut question = serde_json::Map::new();
        question.insert("question".to_string(), text);
        if let Some(header) = agenda_header {
            question.insert("header".to_string(), header);
        }
        question.insert(
            "options".to_string(),
            serde_json::Value::Array(option_entries(&options)),
        );
        // The park wire speaks precise pick bounds only (ctl's rule):
        // --pick verbatim; --multi = any subset of the options.
        if let Some(pick) = agenda_pick {
            if agenda_multi {
                return Err("--pick replaces --multi — provide one or the other".to_string());
            }
            let (min, max) = crate::ctl::parse_pick_spec(pick.as_str().unwrap_or_default())?;
            question.insert("pick_min".to_string(), serde_json::Value::from(min));
            question.insert("pick_max".to_string(), serde_json::Value::from(max));
        } else if agenda_multi {
            question.insert("pick_min".to_string(), serde_json::Value::from(1));
            question.insert(
                "pick_max".to_string(),
                serde_json::Value::from(options.len()),
            );
        }
        if let Some(consequence) = agenda_consequence {
            question.insert("consequence".to_string(), consequence);
        }
        obj.insert(
            "op".to_string(),
            serde_json::Value::String("ask".to_string()),
        );
        obj.remove("kind");
        obj.insert(
            "questions".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object(question)]),
        );
    } else if agenda_consequence.is_some() {
        return Err("--consequence rides the structured form: pass --option too".to_string());
    }
    // The `agenda ask` split, mirroring ctl: plain TEXT parks an
    // ordinary question item (op add, kind question); --questions is
    // the structured option-bearing form (op ask), which carries no
    // kind or title.
    if let Some(questions) = obj.remove("__ask_questions") {
        if obj.contains_key("title") {
            return Err(
                "use either plain TEXT or --questions, not both — the structured form carries its questions inside the array".to_string(),
            );
        }
        obj.insert(
            "op".to_string(),
            serde_json::Value::String("ask".to_string()),
        );
        obj.remove("kind");
        obj.insert("questions".to_string(), questions);
    } else if obj.get("op").and_then(serde_json::Value::as_str) == Some("add")
        && obj.get("kind").and_then(serde_json::Value::as_str) == Some("question")
        && !obj.contains_key("title")
    {
        return Err(
            "agenda ask needs the question TEXT (or --questions for the structured form)"
                .to_string(),
        );
    }
    // The multi-question ask carries its decision fields INSIDE each
    // question object — the daemon ignores the flat twins when
    // `questions` is present, so accepting the combination would
    // silently drop the caller's constraints (review round 6). Reject
    // it at plan time instead.
    if obj.get("questions").is_some() {
        for (flat, inside) in [
            ("question", "the QUESTION positional"),
            ("options", "options"),
            ("previews", "previews"),
            ("pick_min", "pick_min"),
            ("pick_max", "pick_max"),
            ("multi_select", "pick bounds"),
            ("free_text", "free_text"),
            ("consequence", "consequence"),
            ("header", "header"),
        ] {
            if obj.contains_key(flat) {
                return Err(format!(
                    "--questions carries per-question fields inside each object — move {inside} into the question entries instead of the flat form"
                ));
            }
        }
    }
    Ok((serde_json::Value::Object(obj), caller_defaults))
}

/// Split ctl's "Label[:description]" option values into option objects
/// (`ctl::ask_option_entries`'s split): the label is what the person
/// picks; a non-empty description becomes the card's explainer.
fn option_entries(values: &[serde_json::Value]) -> Vec<serde_json::Value> {
    values
        .iter()
        .map(|value| {
            let raw = value.as_str().unwrap_or_default();
            let (label, description) = match raw.split_once(':') {
                Some((label, description)) => (label.trim(), Some(description.trim())),
                None => (raw.trim(), None),
            };
            let mut entry = serde_json::Map::new();
            entry.insert(
                "label".to_string(),
                serde_json::Value::String(label.to_string()),
            );
            if let Some(description) = description.filter(|d| !d.is_empty()) {
                entry.insert(
                    "description".to_string(),
                    serde_json::Value::String(description.to_string()),
                );
            }
            serde_json::Value::Object(entry)
        })
        .collect()
}

/// Decode ctl's `[TYPE:]LOCATOR` ref grammar (`ctl::agenda_ref_spec`
/// minus its client-filesystem inferences, which are meaningless
/// daemon-side): a recognized prefix is stripped, an explicit type
/// overrides the prefix (validated against the closed vocabulary,
/// lowercased), http(s) infers url, and anything else refuses in ctl's
/// own cannot-infer words.
fn ref_type_and_locator(raw: &str, explicit: Option<&str>) -> Result<(String, String), String> {
    const REF_TYPES: [&str; 5] = ["file", "dir", "memory", "session", "url"];
    let (prefixed, rest) = match raw.split_once(':') {
        Some((t, rest)) if REF_TYPES.contains(&t) => (Some(t), rest),
        _ => (None, raw),
    };
    let ref_type = match explicit {
        Some(explicit) => {
            let t = explicit.trim().to_ascii_lowercase();
            if !REF_TYPES.contains(&t.as_str()) {
                return Err(format!(
                    "unknown ref type '{explicit}' (file, dir, memory, session, or url)"
                ));
            }
            t
        }
        None => match prefixed {
            Some(t) => t.to_string(),
            None if raw.starts_with("http://") || raw.starts_with("https://") => "url".to_string(),
            None => {
                return Err(format!(
                    "cannot infer the ref type of {raw:?} — prefix it \
                     (file:…, dir:…, memory:…, session:…, url:https://…) or pass --type"
                ))
            }
        },
    };
    let locator = if prefixed.is_some() { rest } else { raw };
    // ctl canonicalizes relative file/dir locators against ITS working
    // directory — a filesystem the facade cannot see — and the store
    // rejects non-absolute paths, so a relative path must refuse at
    // plan time with the reason, not after dispatch.
    if (ref_type == "file" || ref_type == "dir") && !std::path::Path::new(locator).is_absolute() {
        return Err(format!(
            "file/dir refs need an absolute path here (got {locator:?}) — ctl resolves \
             relative paths against its own working directory, which the facade cannot see"
        ));
    }
    Ok((ref_type, locator.to_string()))
}

/// Cap a caller value reflected into a planning error: planning runs
/// during the ingress gate's resolution — BEFORE the operation check —
/// so an oversized value must never be echoed wholesale (security
/// review: pre-auth reflection is a memory lever).
fn shown_value(raw: &str) -> String {
    if raw.chars().count() > 48 {
        let mut cut: String = raw.chars().take(48).collect();
        cut.push('…');
        cut
    } else {
        raw.to_string()
    }
}

/// Parse ctl's compact region CSV ("x,y,width,height", normalized 0-1)
/// into the region object the display tools take; `flag` names the
/// spelling in refusals.
fn region_from_csv(flag: &str, csv: &serde_json::Value) -> Result<serde_json::Value, String> {
    let text = csv.as_str().unwrap_or_default();
    // Bounded BEFORE parsing or reflecting: planning runs pre-auth in
    // the ingress gate, and an unbounded CSV would be collected into a
    // Vec<f64> (an 8 MB probe cost ~49 MB RSS) before IAM could deny
    // the call (security review). Four normalized floats fit easily.
    if text.len() > 128 {
        return Err(format!(
            "--{flag} expects x,y,width,height (got {} bytes)",
            text.len()
        ));
    }
    let parts: Vec<f64> = text
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("--{flag} expects x,y,width,height (got {text:?})"))?;
    let [x, y, width, height] = parts.as_slice() else {
        return Err(format!("--{flag} expects four values (got {text:?})"));
    };
    Ok(serde_json::json!({ "x": x, "y": y, "width": width, "height": height }))
}

fn dispatch_now_ms() -> serde_json::Value {
    serde_json::Value::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

/// Follow one registry-declared dotted path (the nesting `insert_value`
/// builds) to its value, if present.
fn value_at_path<'a>(
    args: &'a mut serde_json::Value,
    path: &str,
) -> Option<&'a mut serde_json::Value> {
    match path.split_once('.') {
        None => args.as_object_mut()?.get_mut(path),
        Some((outer, inner)) => value_at_path(args.as_object_mut()?.get_mut(outer)?, inner),
    }
}

/// Substitute the planner's dispatch-time defaults. The planner is
/// pure — it cannot know who is calling and reads no clock — so a
/// seed-declared caller-identity default (browser acquire's holder: a
/// CONSTANT default would make two facade sessions collide as the
/// "same" holder, and the browser-lease registry rejects only a
/// DIFFERENT holder id, so a constant silently hands one session's
/// exclusive lease to another) rides OUT-OF-BAND as
/// `PlannedCall::caller_defaults` — the key is absent from the args
/// until this step fills the caller's identity, so no input string is
/// reserved and a caller's own literal `"__caller"` stays caller
/// data. A triggered schedule's omitted arm floor fills `fire_at_ms`
/// with `"__now"` (armed on approval; unforgeable — the key is U64-
/// or When-kind in every row, so planning never passes that string
/// through from input), and `When`-kind values ride as
/// `"__when:<raw>"` until this step resolves them with
/// `ctl::parse_due_ms` — the daemon's clock is the one schedules fire
/// on, so it is also the right one to parse `+2h` and calendar forms
/// against. The dispatcher substitutes after gate resolution — the
/// gate ignores argument values, so authorization is unaffected — and
/// every substitution is scoped to the exact place the planner put
/// it: `__when:` only at the dotted paths the resolved row declares
/// as When-kind. Caller-owned opaque JSON (a peer task's `--context`,
/// a raw `--recurrence` object) is never walked, so caller data that
/// happens to spell a sentinel — even under a same-named key —
/// reaches its tool untouched.
pub(crate) fn substitute_dispatch_sentinels(
    planned: &mut PlannedCall,
    identity: &str,
) -> Result<(), String> {
    if let Some(obj) = planned.args.as_object_mut() {
        for key in &planned.caller_defaults {
            obj.insert(key.clone(), serde_json::Value::String(identity.to_string()));
        }
        for (key, value) in obj.iter_mut() {
            if key == "fire_at_ms" && value.as_str() == Some("__now") {
                *value = dispatch_now_ms();
            }
        }
    }
    let when_paths = planned
        .spec
        .flags
        .iter()
        .filter(|flag| flag.kind == ValueKind::When)
        .map(|flag| flag.json_key)
        .chain(
            planned
                .spec
                .positionals
                .iter()
                .filter(|pos| pos.kind == ValueKind::When)
                .map(|pos| pos.json_key),
        );
    for path in when_paths {
        let Some(value) = value_at_path(&mut planned.args, path) else {
            continue;
        };
        if let Some(text) = value.as_str().and_then(|s| s.strip_prefix("__when:")) {
            *value = serde_json::Value::from(crate::ctl::parse_due_ms(text)?);
        }
    }
    Ok(())
}

/// Resolve one executor call's argv to its command WITHOUT building
/// arguments: extraction, alias normalization, path resolution, and the
/// lane check only. This is all the gate's authorization target needs —
/// argument values never affect authorization — and it is what keeps
/// value parsing (caller JSON included) strictly AFTER the resolved
/// operation's `access.decision` (security review: a denied caller
/// could otherwise allocate ~100 MB from a 16 MiB array before being
/// refused).
fn resolve_meta_argv(
    meta: &str,
    args: &serde_json::Value,
) -> Result<(Vec<String>, &'static CommandSpec), String> {
    let argv = argv_from_args(args)?;
    if argv.is_empty() {
        return Err("empty argv — call the help tool for the command map".to_string());
    }
    let argv = normalize_aliases(argv);
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
    Ok((argv, spec))
}

/// Resolve one executor call (`inspect`/`act`/`authorize`) into the command
/// it names and build its arguments. Pure and side-effect-free; the
/// dispatcher calls this AFTER the gate authorized the resolved
/// operation (the gate itself resolves via [`resolve_meta_argv`] only),
/// and determinism over (meta, args) makes the two resolutions name the
/// same command.
pub(crate) fn plan_for_meta(meta: &str, args: &serde_json::Value) -> Result<PlannedCall, String> {
    let (argv, spec) = resolve_meta_argv(meta, args)?;
    let rest = &argv[spec.path.len()..];
    let (built, caller_defaults) = build_args(spec, rest)?;
    Ok(PlannedCall {
        tool: spec.tool,
        args: built,
        spec,
        caller_defaults,
    })
}

/// The underlying tool an executor call resolves to, if it resolves —
/// argv-only, values never parsed (the [`resolve_meta_argv`]
/// discipline), so a transport may consult it pre-dispatch (per-request
/// SSE mode selection) at the same cost class as the gate. `None`: not
/// an executor call, or its path does not resolve.
pub(crate) fn facade_resolved_tool(name: &str, args: &serde_json::Value) -> Option<&'static str> {
    match name {
        "inspect" | "act" | "authorize" => resolve_meta_argv(name, args)
            .ok()
            .map(|(_, spec)| spec.tool),
        _ => None,
    }
}

/// The gate-side authorization resolver. `None`: not a facade tool (fall
/// through to the fixed per-tool map). `Some(op)`: authorize this
/// operation, then dispatch. Resolution here NEVER builds arguments
/// ([`resolve_meta_argv`]) — the gate ignores argument values, so caller
/// JSON is parsed only after `access.decision` passes. An executor call
/// whose PATH fails to resolve authorizes at the harmless read floor:
/// nothing will execute — dispatch re-plans, applies the rewind-only
/// pressure gate first, and returns the parse error as a tool result —
/// and the error's content is registry shape, the same disclosure class
/// as `help`. A call with a malformed VALUE authorizes at its resolved
/// command's operation and fails at dispatch planning the same way.
pub(crate) fn facade_gate_operation(name: &str, args: &serde_json::Value) -> Option<PeerOperation> {
    match name {
        // The read-only meta surface: the command map and the embedded
        // skills corpus disclose less than get_status already does.
        "help" | "docs" => Some(PeerOperation::StatsRead),
        // The event stream carries only session/approval/task lifecycle
        // (the ring's ingest allowlist), i.e. what the session.inspect
        // read tools already serve — push semantics, not new authority.
        "events" => Some(PeerOperation::SessionInspect),
        "inspect" | "act" | "authorize" => Some(
            resolve_meta_argv(name, args)
                .map(|(_, spec)| crate::mcp::mcp_tool_operation(spec.tool))
                .unwrap_or(PeerOperation::StatsRead),
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
        "events" => Some(allowed(PeerOperation::SessionInspect)),
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
            ValueKind::Json => out.push_str(&format!(" [--{} JSON]", flag.name)),
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

/// `docs` tool: the embedded operate-skills corpus — list, fetch a skill,
/// or fetch one of its support files. The file vocabulary derives from
/// `BuiltinSkill::support_files`, so a skill whose text references its
/// bundled files is always a complete package through the facade.
pub(crate) fn render_docs(args: &serde_json::Value) -> String {
    let params = serde_json::from_value::<FacadeDocsParams>(args.clone()).unwrap_or_default();
    let normalize = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let skill = normalize(params.skill);
    let file = normalize(params.file);
    let Some(name) = skill.as_deref() else {
        if file.is_some() {
            return "a file fetch needs its skill: docs {\"skill\":\"<name>\",\"file\":\"<path>\"}"
                .to_string();
        }
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
        return out;
    };
    let Some(skill) = crate::builtin_skills::BUILTIN_SKILLS
        .iter()
        .find(|s| s.name == name)
    else {
        return format!("unknown skill {name:?} — call docs with no arguments for the list");
    };
    match file.as_deref() {
        None => {
            let mut out = skill.skill_md.to_string();
            if !skill.support_files.is_empty() {
                out.push_str("\n\n---\nSupport files (fetch with docs {\"skill\":\"");
                out.push_str(skill.name);
                out.push_str("\",\"file\":\"<path>\"}):\n");
                for (path, bytes) in skill.support_files {
                    out.push_str(&format!("  {path} ({} bytes)\n", bytes.len()));
                }
            }
            out
        }
        Some(path) => match skill.support_files.iter().find(|(p, _)| *p == path) {
            Some((_, bytes)) => String::from_utf8_lossy(bytes).into_owned(),
            None => {
                let available: Vec<&str> = skill.support_files.iter().map(|(p, _)| *p).collect();
                format!(
                    "unknown file {path:?} for skill {name:?} — available: {}",
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                )
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> serde_json::Value {
        serde_json::json!({ "argv": parts })
    }

    /// A platform-absolute test path: the absolute-locator check runs
    /// on the daemon's own platform, so unix-spelled pins must speak
    /// Windows paths on a Windows test host.
    fn abs_path(unix: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", unix.replace('/', "\\"))
        } else {
            unix.to_string()
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
        assert_eq!(planned.args["kind"], "task");
        assert_eq!(planned.args["title"], "fix the roof");
        assert_eq!(planned.args["tags"], serde_json::json!(["house", "urgent"]));
    }

    /// Json-kind values parse into real JSON at plan time — a structured
    /// param arrives as an array/object on the wire, and malformed JSON
    /// is a plan error that never dispatches.
    #[test]
    fn json_kind_values_parse_at_plan_time() {
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "cu",
                "actions",
                "[{\"type\":\"screenshot\"}]",
                "--settle",
                "250",
            ]),
        )
        .unwrap();
        assert_eq!(planned.tool, "execute_cu_actions");
        assert_eq!(planned.args["actions"][0]["type"], "screenshot");
        assert_eq!(planned.args["settle"], 250);
        let err = plan_for_meta("act", &argv(&["cu", "actions", "{not json"])).unwrap_err();
        assert!(err.contains("literal JSON"), "{err}");
    }

    /// Negative booleans travel as literal JSON (review round 1's P2):
    /// the option vocabulary that flips a default-true field — the
    /// autonomous goal-run, the one-shot halt, the dirty-tree remote —
    /// must be expressible through the facade, and `false` arrives as a
    /// real JSON bool, not a string.
    #[test]
    fn json_flags_express_negative_booleans() {
        let planned = plan_for_meta(
            "authorize",
            &argv(&["agenda", "start", "abc123", "--interactive", "false"]),
        )
        .unwrap();
        assert_eq!(planned.args["interactive"], serde_json::json!(false));
        let planned = plan_for_meta(
            "authorize",
            &argv(&["controller", "halt", "--persistent", "false"]),
        )
        .unwrap();
        assert_eq!(planned.args["persistent"], serde_json::json!(false));
        let planned = plan_for_meta(
            "authorize",
            &argv(&["remote", "start", "[\"true\"]", "--require-clean", "false"]),
        )
        .unwrap();
        assert_eq!(planned.args["require_clean"], serde_json::json!(false));
        // The task-start mode override is tri-state via ctl's bare flag
        // pair (rounds 2 and 19): --direct forces direct, --orchestrate
        // forces orchestration, omitted keeps automatic selection.
        let planned =
            plan_for_meta("act", &argv(&["task", "start", "fix it", "--direct"])).unwrap();
        assert_eq!(planned.args["orchestrate"], serde_json::json!(false));
        let planned = plan_for_meta("act", &argv(&["task", "start", "fix it"])).unwrap();
        assert!(planned.args.get("orchestrate").is_none());
        // The computer-use context travels too (review round 3):
        // repeatable frames and the display target reach the CU runner.
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "task",
                "start",
                "click it",
                "--frame",
                "f1",
                "--frame",
                "f2",
                "--display-target",
                "display_99",
            ]),
        )
        .unwrap();
        assert_eq!(
            planned.args["reference_frame_ids"],
            serde_json::json!(["f1", "f2"])
        );
        assert_eq!(planned.args["display_target"], "display_99");
    }

    /// The remaining ctl option-vocabulary parity pins (review round 4):
    /// the rewind handoff lists, the uncapped accessibility read, and
    /// memory project provenance all travel through the facade.
    #[test]
    fn parity_options_cover_rewind_lists_full_values_and_project() {
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "context",
                "rewind",
                "item-1",
                "noise",
                "the primer",
                "--preserve",
                "fact",
                "--discard",
                "dead end",
                "--artifact",
                "out.txt",
                "--next-step",
                "rerun tests",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["discard"], serde_json::json!(["dead end"]));
        assert_eq!(planned.args["artifacts"], serde_json::json!(["out.txt"]));
        assert_eq!(
            planned.args["next_steps"],
            serde_json::json!(["rerun tests"])
        );
        let planned =
            plan_for_meta("inspect", &argv(&["cu", "elements", "--full-values"])).unwrap();
        assert_eq!(planned.args["full_values"], serde_json::json!(true));
        let planned = plan_for_meta(
            "act",
            &argv(&["memory", "propose", "a fact", "--project", "intendant"]),
        )
        .unwrap();
        assert_eq!(planned.args["project"], "intendant");
    }

    /// The blocking-ask decision contract travels whole (review rounds
    /// 5-6): flat-form flags on a single question; the multi-question
    /// form carries per-question fields INSIDE each object, and mixing
    /// the flat twins with --questions is a plan error (the daemon
    /// would silently ignore them).
    #[test]
    fn ask_row_carries_the_full_decision_contract() {
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "ask",
                "which one?",
                "--option",
                "a",
                "--option",
                "b",
                "--pick-max",
                "2",
                "--no-free-text",
                "--consequence",
                "I proceed with a",
                "--wait",
                "600",
                "--expires",
                "2h",
            ]),
        )
        .unwrap();
        assert_eq!(planned.tool, "ask_user");
        assert_eq!(planned.args["question"], "which one?");
        assert_eq!(planned.args["pick_max"], 2);
        assert_eq!(planned.args["free_text"], serde_json::json!(false));
        assert_eq!(planned.args["consequence"], "I proceed with a");
        assert_eq!(planned.args["wait_seconds"], 600);
        assert_eq!(planned.args["expiry"], "2h");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "ask",
                "--questions",
                "[{\"question\":\"which?\",\"pick_max\":2,\"free_text\":false}]",
                "--wait",
                "600",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["questions"][0]["pick_max"], 2);
        assert!(planned.args.get("question").is_none(), "positional omitted");
        assert_eq!(
            planned.args["wait_seconds"], 600,
            "call-level fields stay flat"
        );
        let err = plan_for_meta(
            "act",
            &argv(&[
                "ask",
                "--questions",
                "[{\"question\":\"which?\"}]",
                "--pick-max",
                "2",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("per-question fields"), "{err}");
        let planned = plan_for_meta("act", &argv(&["ask", "ship it?", "--park"])).unwrap();
        assert_eq!(planned.args["question"], "ship it?");
        assert_eq!(planned.args["park"], serde_json::json!(true));
    }

    /// The ctl flag spellings for values the registry models as
    /// positionals plan identically (review round 14): a caller copying
    /// ctl help verbatim must not fail on argv shape.
    #[test]
    fn ctl_flag_spellings_alias_the_positionals() {
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "context",
                "rewind",
                "--item-id",
                "item-1",
                "--reason",
                "noise",
                "--primer",
                "the primer",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["anchor"]["item_id"], "item-1");
        assert_eq!(planned.args["reason"], "noise");
        assert_eq!(planned.args["primer"], "the primer");
        let planned = plan_for_meta(
            "act",
            &argv(&["cu", "actions", "--actions", "[{\"type\":\"screenshot\"}]"]),
        )
        .unwrap();
        assert_eq!(planned.args["actions"][0]["type"], "screenshot");
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "controller",
                "schedule",
                "--controller-id",
                "ctl-1",
                "--goal",
                "keep shipping",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["controller_id"], "ctl-1");
        assert_eq!(planned.args["north_star_goal"], "keep shipping");
        let planned = plan_for_meta(
            "authorize",
            &argv(&["agenda", "approve", "item-1", "--digest", "abc123"]),
        )
        .unwrap();
        assert_eq!(planned.args["digest"], "abc123");
    }

    /// ctl's primary invocation grammars plan verbatim (review round
    /// 15): remote start's trailing argv keeps word boundaries and its
    /// KEY=VALUE env pairs fold into the object, --allow-dirty flips
    /// require_clean, browser create takes the URL positionally with
    /// the ctl peer/session spellings, controller complete takes its
    /// required flags, and audio spawn accepts the whole tool object.
    #[test]
    fn ctl_primary_grammars_plan_verbatim() {
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "remote",
                "start",
                "--revision",
                "abc123",
                "--env",
                "RUST_LOG=debug",
                "--env",
                "CI=1",
                "--allow-dirty",
                "--timeout",
                "600",
                "--",
                "cargo",
                "check",
                "--all",
            ]),
        )
        .unwrap();
        assert_eq!(
            planned.args["argv"],
            serde_json::json!(["cargo", "check", "--all"]),
            "word boundaries preserved, flag-shaped words included"
        );
        assert_eq!(planned.args["expected_revision"], "abc123");
        assert_eq!(
            planned.args["env"],
            serde_json::json!({ "RUST_LOG": "debug", "CI": "1" })
        );
        assert_eq!(planned.args["require_clean"], serde_json::json!(false));
        assert_eq!(planned.args["timeout_s"], 600);
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "browser",
                "create",
                "https://example.com",
                "--session",
                "sess-2",
                "--peer",
                "peer-a",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["url"], "https://example.com");
        assert_eq!(planned.args["owner_session_id"], "sess-2");
        assert_eq!(planned.args["peer_id"], "peer-a");
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "controller",
                "complete",
                "--restart-id",
                "r-1",
                "--token",
                "tok",
                "--summary",
                "done",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["restart_id"], "r-1");
        assert_eq!(planned.args["turn_complete_token"], "tok");
        assert_eq!(planned.args["handoff_summary"], "done");
        let planned = plan_for_meta(
            "inspect",
            &argv(&["context", "inspect", "--item-id", "item-9"]),
        )
        .unwrap();
        assert_eq!(planned.args["item_id"], "item-9");
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "context",
                "claim-fission",
                "--group-id",
                "g-1",
                "--branch-session-id",
                "sess-b",
                "--expected-canonical-session-id",
                "sess-a",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["group_id"], "g-1");
        assert_eq!(planned.args["branch_session_id"], "sess-b");
        assert_eq!(planned.args["expected_canonical_session_id"], "sess-a");
        let planned =
            plan_for_meta("authorize", &argv(&["controller", "halt", "--one-shot"])).unwrap();
        assert_eq!(planned.args["persistent"], serde_json::json!(false));
        let planned = plan_for_meta(
            "act",
            &argv(&["browser", "release", "ws-1", "--holder", "sess-2"]),
        )
        .unwrap();
        assert_eq!(planned.args["holder_id"], "sess-2");
        let planned = plan_for_meta(
            "act",
            &argv(&["shared", "focus", "--region", "0.1,0.2,0.5,0.4"]),
        )
        .unwrap();
        assert_eq!(
            planned.args["region"],
            serde_json::json!({ "x": 0.1, "y": 0.2, "width": 0.5, "height": 0.4 })
        );
        assert!(
            plan_for_meta("act", &argv(&["shared", "focus", "--region", "0.1,0.2"])).is_err(),
            "a region needs four values"
        );
        // Round 21: the remaining ctl flag spellings — acquire's
        // --holder, shared show's CSV --focus, backout's --record-id.
        let planned = plan_for_meta(
            "act",
            &argv(&["browser", "acquire", "ws-1", "--holder", "sess-9"]),
        )
        .unwrap();
        assert_eq!(planned.args["holder_id"], "sess-9");
        let planned = plan_for_meta(
            "act",
            &argv(&["shared", "show", "--focus", "0.1,0.2,0.5,0.4"]),
        )
        .unwrap();
        assert_eq!(
            planned.args["focus_region"],
            serde_json::json!({ "x": 0.1, "y": 0.2, "width": 0.5, "height": 0.4 })
        );
        assert!(
            plan_for_meta(
                "act",
                &argv(&[
                    "shared",
                    "show",
                    "--focus",
                    "0.1,0.2,0.5,0.4",
                    "--focus-region",
                    "{\"x\":0.1,\"y\":0.2,\"width\":0.5,\"height\":0.4}",
                ]),
            )
            .is_err(),
            "one focus spelling at a time"
        );
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "context",
                "backout",
                "--record-id",
                "rec-1",
                "--mode",
                "fork",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["record_id"], "rec-1");
        assert_eq!(planned.args["mode"], "fork");
        // Round 22: ctl's WHEN/INTERVAL vocabulary on the schedule
        // family (`+2h`/`1d` instead of pre-converted ms), the
        // Label:description option split, and the [TYPE:]LOCATOR ref
        // grammar.
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "sweep",
                "--at",
                "+2h",
                "--every",
                "1d",
                "--until",
                "2026-09-01",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["fire_at_ms"], "__when:+2h");
        assert_eq!(planned.args["recurrence"]["every_ms"], 86_400_000u64);
        assert_eq!(planned.args["recurrence"]["until_ms"], "__when:2026-09-01");
        let err = plan_for_meta(
            "act",
            &argv(&[
                "agenda", "schedule", "item-1", "--goal", "g", "--at", "+2h", "--every", "soon",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("invalid interval"), "{err}");
        let planned =
            plan_for_meta("act", &argv(&["agenda", "patch", "item-1", "--due", "+1d"])).unwrap();
        assert_eq!(planned.args["patch"]["due_ms"], "__when:+1d");
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "add", "water the plants", "--due", "+1w"]),
        )
        .unwrap();
        assert_eq!(planned.args["due_ms"], "__when:+1w");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "ask",
                "Which database?",
                "--option",
                "postgres:Existing infra",
                "--option",
                "sqlite",
            ]),
        )
        .unwrap();
        assert_eq!(
            planned.args["options"],
            serde_json::json!([
                { "label": "postgres", "description": "Existing infra" },
                { "label": "sqlite" },
            ])
        );
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "ref", "item-1", "url:https://example.com/x"]),
        )
        .unwrap();
        assert_eq!(planned.args["ref_type"], "url");
        assert_eq!(planned.args["locator"], "https://example.com/x");
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "ref", "item-1", "https://example.com/y"]),
        )
        .unwrap();
        assert_eq!(planned.args["ref_type"], "url", "http(s) infers url");
        assert_eq!(planned.args["locator"], "https://example.com/y");
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "ref", "item-1", "memory:project_notes"]),
        )
        .unwrap();
        assert_eq!(planned.args["ref_type"], "memory");
        assert_eq!(planned.args["locator"], "project_notes");
        // An explicit --type overrides the prefix; the prefix still
        // leaves the locator (ctl's explicit.or(prefixed)).
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "ref",
                "item-1",
                "url:https://example.com/z",
                "--type",
                "session",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["ref_type"], "session");
        assert_eq!(planned.args["locator"], "https://example.com/z");
        let err =
            plan_for_meta("act", &argv(&["agenda", "ref", "item-1", "notes.md"])).unwrap_err();
        assert!(err.contains("cannot infer the ref type"), "{err}");
        let err = plan_for_meta(
            "act",
            &argv(&["agenda", "ref", "item-1", "x", "--type", "ftp"]),
        )
        .unwrap_err();
        assert!(err.contains("unknown ref type"), "{err}");
        // Round 23: ctl's --pick MIN[-MAX] / --no-free-text ask
        // grammar, and agenda start's full launch vocabulary.
        let planned = plan_for_meta(
            "act",
            &argv(&["ask", "Choose", "--pick", "0-2", "--no-free-text"]),
        )
        .unwrap();
        assert_eq!(planned.args["pick_min"], 0);
        assert_eq!(planned.args["pick_max"], 2);
        assert_eq!(planned.args["free_text"], serde_json::json!(false));
        let err =
            plan_for_meta("act", &argv(&["ask", "Choose", "--pick", "2", "--multi"])).unwrap_err();
        assert!(err.contains("--pick replaces --multi"), "{err}");
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "agenda",
                "start",
                "item-1",
                "--project",
                "/srv/proj",
                "--goal-run",
                "--agent",
                "codex",
                "--dial-autonomy",
                "high",
                "--dial-approve",
                "network=deny",
                "--dial-approve",
                "shell=allow",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["project_root"], "/srv/proj");
        assert_eq!(planned.args["interactive"], serde_json::json!(false));
        assert_eq!(planned.args["agent_config"]["agent"], "codex");
        assert_eq!(planned.args["agent_config"]["dial"]["autonomy"], "high");
        assert_eq!(
            planned.args["agent_config"]["dial"]["approvals"],
            serde_json::json!({ "network": "deny", "shell": "allow" })
        );
        let err = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--at",
                "+1h",
                "--dial-approve",
                "network",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("CATEGORY=RULE"), "{err}");
        // Round 24: display grant/revoke positional ids, the park-time
        // --ref gesture, and ctl's structured agenda ask flags.
        let planned = plan_for_meta("authorize", &argv(&["display", "grant-user", "2"])).unwrap();
        assert_eq!(planned.args["display_id"], 2);
        let planned = plan_for_meta(
            "authorize",
            &argv(&["display", "revoke-user", "3", "--note", "done"]),
        )
        .unwrap();
        assert_eq!(planned.args["display_id"], 3);
        assert_eq!(planned.args["note"], "done");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "add",
                "fix it",
                "--ref",
                "url:https://e.com/a",
                "--ref",
                "memory:notes",
                "--must-read",
                "--label",
                "ctx",
            ]),
        )
        .unwrap();
        assert_eq!(
            planned.args["refs"],
            serde_json::json!([
                { "ref_type": "url", "locator": "https://e.com/a", "must_read": true, "label": "ctx" },
                { "ref_type": "memory", "locator": "notes", "must_read": true, "label": "ctx" },
            ])
        );
        let err = plan_for_meta("act", &argv(&["agenda", "add", "fix it", "--label", "ctx"]))
            .unwrap_err();
        assert!(err.contains("pass --ref too"), "{err}");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "ask",
                "Choose",
                "--option",
                "A:First",
                "--option",
                "B",
                "--pick",
                "1",
                "--header",
                "Pick",
                "--consequence",
                "I pick A",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["op"], "ask");
        assert!(planned.args.get("kind").is_none());
        assert!(planned.args.get("title").is_none());
        assert_eq!(
            planned.args["questions"],
            serde_json::json!([{
                "question": "Choose",
                "header": "Pick",
                "options": [
                    { "label": "A", "description": "First" },
                    { "label": "B" },
                ],
                "pick_min": 1,
                "pick_max": 1,
                "consequence": "I pick A",
            }])
        );
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda", "ask", "Which?", "--option", "A", "--option", "B", "--multi",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["questions"][0]["pick_min"], 1);
        assert_eq!(planned.args["questions"][0]["pick_max"], 2);
        let err = plan_for_meta(
            "act",
            &argv(&["agenda", "ask", "Which?", "--header", "Pick"]),
        )
        .unwrap_err();
        assert!(err.contains("pass --option too"), "{err}");
        let err = plan_for_meta(
            "act",
            &argv(&["agenda", "ask", "Which?", "--consequence", "stalls"]),
        )
        .unwrap_err();
        assert!(err.contains("pass --option too"), "{err}");
        // Round 25: --note/--task kind shorthands and remote wait's
        // --for budget.
        let planned =
            plan_for_meta("act", &argv(&["agenda", "add", "remember this", "--note"])).unwrap();
        assert_eq!(planned.args["kind"], "note");
        let planned = plan_for_meta("act", &argv(&["agenda", "add", "do this", "--task"])).unwrap();
        assert_eq!(planned.args["kind"], "task");
        let err =
            plan_for_meta("act", &argv(&["agenda", "add", "x", "--note", "--task"])).unwrap_err();
        assert!(err.contains("pass --note or --task, not both"), "{err}");
        // ctl's precedence: an explicit --kind wins over a selector.
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "add", "x", "--kind", "QUESTION", "--note"]),
        )
        .unwrap();
        assert_eq!(planned.args["kind"], "question");
        let err =
            plan_for_meta("act", &argv(&["agenda", "add", "x", "--kind", "reminder"])).unwrap_err();
        assert!(err.contains("unknown kind"), "{err}");
        let planned = plan_for_meta(
            "authorize",
            &argv(&["remote", "wait", "job-1", "--for", "30"]),
        )
        .unwrap();
        assert_eq!(planned.args["wait_s"], 30);
        let err = plan_for_meta(
            "authorize",
            &argv(&["remote", "wait", "job-1", "--for", "300"]),
        )
        .unwrap_err();
        assert!(err.contains("one bounded"), "{err}");
        // Round 26: cu screenshot (ctl's display alias) and the list
        // lifecycle selectors.
        let planned = plan_for_meta(
            "inspect",
            &argv(&["cu", "screenshot", "--target", "display_2"]),
        )
        .unwrap();
        assert_eq!(planned.tool, "take_screenshot");
        assert_eq!(planned.args["display_target"], "display_2");
        let planned = plan_for_meta("inspect", &argv(&["agenda", "list", "--done"])).unwrap();
        assert_eq!(planned.args["status"], "done");
        let planned = plan_for_meta("inspect", &argv(&["agenda", "list", "--retired"])).unwrap();
        assert_eq!(planned.args["status"], "retired");
        let planned = plan_for_meta("inspect", &argv(&["agenda", "list", "--open"])).unwrap();
        assert_eq!(planned.args["status"], "open");
        // ctl's precedence: --all lifts the filter even beside --done.
        let planned =
            plan_for_meta("inspect", &argv(&["agenda", "list", "--all", "--done"])).unwrap();
        assert!(planned.args.get("status").is_none());
        // Round 27: input respond's --text spelling, and the sealed
        // binding-ref workflow named as a ctl-side exclusion.
        let planned = plan_for_meta("act", &argv(&["input", "respond", "--text", "yes"])).unwrap();
        assert_eq!(planned.args["text"], "yes");
        let err = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--at",
                "+1h",
                "--binding-ref",
                "file:/srv/plan.md",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("client-side"), "{err}");
        assert!(err.contains("--binding-refs"), "{err}");
        // Round 28: attest's file-pinning --ref is the same named
        // exclusion class.
        let err = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "attest",
                "item-1",
                "--occurrence",
                "occ-1",
                "--outcome",
                "partial",
                "--ref",
                "file:handoff.md",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("client-side"), "{err}");
        assert!(err.contains("--refs"), "{err}");
        // Round 29: the display frame alias, and relative file refs
        // refusing at plan time (ctl canonicalizes against ITS cwd).
        let planned = plan_for_meta("inspect", &argv(&["display", "frame", "latest"])).unwrap();
        assert_eq!(planned.tool, "read_frame");
        assert_eq!(planned.args["frame_id"], "latest");
        let err = plan_for_meta(
            "act",
            &argv(&["agenda", "add", "x", "--ref", "file:notes.md"]),
        )
        .unwrap_err();
        assert!(err.contains("absolute path"), "{err}");
        let err =
            plan_for_meta("act", &argv(&["agenda", "ref", "item-1", "file:notes.md"])).unwrap_err();
        assert!(err.contains("absolute path"), "{err}");
        let abs = abs_path("/srv/notes.md");
        let file_ref = format!("file:{abs}");
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "add", "x", "--ref", file_ref.as_str()]),
        )
        .unwrap();
        assert_eq!(planned.args["refs"][0]["locator"], abs.as_str());
        // Round 30: the shared-focus positional speaks the same CSV as
        // --region, and ctl's browser path aliases resolve.
        let planned = plan_for_meta("act", &argv(&["shared", "focus", "0,0,1,1"])).unwrap();
        assert_eq!(
            planned.args["region"],
            serde_json::json!({ "x": 0.0, "y": 0.0, "width": 1.0, "height": 1.0 })
        );
        let planned = plan_for_meta("inspect", &argv(&["browser", "ls"])).unwrap();
        assert_eq!(planned.tool, "list_browser_workspaces");
        let planned =
            plan_for_meta("act", &argv(&["browser", "open", "https://example.com"])).unwrap();
        assert_eq!(planned.tool, "create_browser_workspace");
        assert_eq!(planned.args["url"], "https://example.com");
        let planned = plan_for_meta("act", &argv(&["browser", "take", "ws-1"])).unwrap();
        assert_eq!(planned.tool, "acquire_browser_workspace");
        // Round 31: the alias TABLES — family spellings and ctl's
        // command aliases rewrite onto canonical rows (one sample per
        // class; the registry pin holds the tables coherent).
        let planned = plan_for_meta(
            "act",
            &argv(&["cu", "exec", "[{\"action\":\"screenshot\"}]"]),
        )
        .unwrap();
        assert_eq!(planned.tool, "execute_cu_actions");
        let planned = plan_for_meta("inspect", &argv(&["browsers", "ls"])).unwrap();
        assert_eq!(planned.tool, "list_browser_workspaces");
        let planned = plan_for_meta("inspect", &argv(&["approvals", "pending"])).unwrap();
        assert_eq!(planned.tool, "get_pending_approval");
        let planned = plan_for_meta("inspect", &argv(&["peers", "list"])).unwrap();
        assert_eq!(planned.tool, "list_peers");
        let planned =
            plan_for_meta("act", &argv(&["sessions", "note", "remember the port"])).unwrap();
        assert_eq!(planned.tool, "post_session_note");
        let planned = plan_for_meta("authorize", &argv(&["set", "autonomy", "high"])).unwrap();
        assert_eq!(planned.tool, "set_autonomy");
        let planned = plan_for_meta("act", &argv(&["shared-view", "show"])).unwrap();
        assert_eq!(planned.tool, "show_shared_view");
        let planned = plan_for_meta("act", &argv(&["agenda", "done", "item-1"])).unwrap();
        assert_eq!(planned.tool, "agenda_op");
        assert_eq!(planned.args["op"], "complete");
        let planned = plan_for_meta("inspect", &argv(&["memory", "show", "claim-1"])).unwrap();
        assert_eq!(planned.tool, "memory_read");
        let planned = plan_for_meta("inspect", &argv(&["display", "ready"])).unwrap();
        assert_eq!(planned.tool, "display_readiness");
        let planned = plan_for_meta("inspect", &argv(&["cu", "screenshot"])).unwrap();
        assert_eq!(planned.tool, "take_screenshot");
        let planned = plan_for_meta("authorize", &argv(&["shared", "request-input"])).unwrap();
        assert_eq!(planned.tool, "request_shared_view_input");
        // Round 32: ctl's inline preview cards translate; the
        // file-backed forms refuse by name.
        let planned = plan_for_meta(
            "act",
            &argv(&["ask", "Choose", "--preview-text", "A=inline details"]),
        )
        .unwrap();
        assert_eq!(
            planned.args["previews"],
            serde_json::json!([{ "label": "A", "text": "inline details" }])
        );
        let err = plan_for_meta(
            "act",
            &argv(&["ask", "Choose", "--preview-text", "no-separator"]),
        )
        .unwrap_err();
        assert!(err.contains("LABEL=VALUE"), "{err}");
        let err = plan_for_meta(
            "act",
            &argv(&["ask", "Choose", "--preview-html", "A=/srv/card.html"]),
        )
        .unwrap_err();
        assert!(err.contains("client-side"), "{err}");
        assert!(err.contains("--previews"), "{err}");
        // Round 33: an explicit flag beats the free-text tail (ctl's
        // precedence — the positional is silently ignored beside it),
        // and --schema is a named client-side exclusion.
        let planned = plan_for_meta(
            "act",
            &argv(&["task", "start", "positional", "task", "--task", "flag task"]),
        )
        .unwrap();
        assert_eq!(planned.args["task"], "flag task");
        let planned = plan_for_meta(
            "act",
            &argv(&["input", "respond", "positional", "--text", "flag text"]),
        )
        .unwrap();
        assert_eq!(planned.args["text"], "flag text");
        let err = plan_for_meta("act", &argv(&["ask", "--schema", "/srv/q.json"])).unwrap_err();
        assert!(err.contains("client-side"), "{err}");
        assert!(err.contains("--questions"), "{err}");
        // Round 34: session note's file-reading --image is the same
        // named exclusion class.
        let err = plan_for_meta(
            "act",
            &argv(&[
                "session",
                "note",
                "look at this",
                "--image",
                "/tmp/shot.png",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("client-side"), "{err}");
        assert!(err.contains("--images"), "{err}");
        // Round 35: overflowing time arithmetic refuses instead of
        // wrapping (checked mul/add in ctl's shared parsers).
        let err = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--at",
                "+1h",
                "--every",
                "30500568905w",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("too large"), "{err}");
        let mut planned = planned_with(
            &["agenda", "add"],
            serde_json::json!({ "due_ms": "__when:+30500568904w" }),
        );
        let err = substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap_err();
        assert!(err.contains("too far in the future"), "{err}");
        // Round 36: the flag beats its positional twin in EITHER order
        // (ctl reads the flag first), non-greedy included.
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "browser",
                "create",
                "--url",
                "https://flag.example",
                "https://positional.example",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["url"], "https://flag.example");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "browser",
                "create",
                "https://positional.example",
                "--url",
                "https://flag.example",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["url"], "https://flag.example");
        // Round 37: precedence is PATH-exact — a nested flag deep-merges
        // with a positional parent object (the flag's field stands, the
        // object's other fields land), and a sibling nested flag never
        // blocks a distinct positional path.
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "patch",
                "--title",
                "renamed",
                "item-1",
                "{\"body\":\"kept\",\"title\":\"clobber\"}",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["patch"]["title"], "renamed");
        assert_eq!(planned.args["patch"]["body"], "kept");
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "context",
                "rewind",
                "--position",
                "after",
                "item-9",
                "too deep",
                "primer text",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["anchor"]["item_id"], "item-9");
        assert_eq!(planned.args["anchor"]["position"], "after");
        assert_eq!(planned.args["reason"], "too deep");
        // Round 38: two synonymous scalar spellings refuse together
        // (ctl's or_else picks a fixed winner; last-wins could dispatch
        // the other value). Repeatable list twins still append.
        let err = plan_for_meta(
            "act",
            &argv(&[
                "task",
                "start",
                "work",
                "--session",
                "sess-a",
                "--session-id",
                "sess-b",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("not both"), "{err}");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "stamp",
                "fix-task",
                "--note",
                "first",
                "--annotation",
                "second",
            ]),
        )
        .unwrap();
        assert_eq!(
            planned.args["annotations"],
            serde_json::json!(["first", "second"])
        );
        // Round 40: a transformed boolean alias contradicting its
        // explicit twin refuses instead of silently overwriting.
        let err = plan_for_meta(
            "authorize",
            &argv(&[
                "remote",
                "start",
                "job",
                "--allow-dirty",
                "--require-clean",
                "true",
            ]),
        )
        .unwrap_err();
        assert!(err.contains("not both"), "{err}");
        let err = plan_for_meta(
            "authorize",
            &argv(&["controller", "halt", "--one-shot", "--persistent", "true"]),
        )
        .unwrap_err();
        assert!(err.contains("not both"), "{err}");
        // Round 42: --dial-approve merges per category into an
        // --agent-config approvals object — unspecified entries
        // survive, the specific spelling wins its own.
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "agenda",
                "start",
                "item-1",
                "--agent-config",
                "{\"dial\":{\"approvals\":{\"shell\":\"ask\",\"network\":\"allow\"}}}",
                "--dial-approve",
                "network=deny",
            ]),
        )
        .unwrap();
        assert_eq!(
            planned.args["agent_config"]["dial"]["approvals"],
            serde_json::json!({ "shell": "ask", "network": "deny" })
        );
        // Round 39: a full-object flag beside its nested flags is
        // order-independent — the specific spelling wins, the object
        // fills the rest (both orders pinned).
        for argv_case in [
            [
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--at",
                "+1h",
                "--agent",
                "codex",
                "--agent-config",
                "{\"agent\":\"claude\",\"dial\":{\"autonomy\":\"high\"}}",
            ],
            [
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--at",
                "+1h",
                "--agent-config",
                "{\"agent\":\"claude\",\"dial\":{\"autonomy\":\"high\"}}",
                "--agent",
                "codex",
            ],
        ] {
            let planned = plan_for_meta("act", &argv(&argv_case)).unwrap();
            assert_eq!(planned.args["agent_config"]["agent"], "codex");
            assert_eq!(planned.args["agent_config"]["dial"]["autonomy"], "high");
        }
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "peer",
                "task",
                "peer-a",
                "do the thing",
                "--context",
                "plain words",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["context"], "plain words");
        let planned = plan_for_meta(
            "authorize",
            &argv(&["peer", "task", "peer-a", "go", "--context", "{\"k\":1}"]),
        )
        .unwrap();
        assert_eq!(planned.args["context"]["k"], 1);
        let planned = plan_for_meta(
            "act",
            &argv(&["shared", "focus", "clear", "--reason", "done"]),
        )
        .unwrap();
        assert_eq!(planned.tool, "clear_shared_view_focus");
        assert_eq!(planned.args["reason"], "done");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "run the sweep",
                "--at",
                "1700000000000",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["goal"], "run the sweep");
        // --at is When-kind (round 22): the raw text rides to dispatch
        // and resolves through ctl's parse_due_ms there.
        assert_eq!(planned.args["fire_at_ms"], "__when:1700000000000");
        let mut planned = planned;
        substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap();
        assert_eq!(planned.args["fire_at_ms"], 1_700_000_000_000u64);
        let planned = plan_for_meta(
            "authorize",
            &argv(&["memory", "supersede", "abc123", "--with", "def456"]),
        )
        .unwrap();
        assert_eq!(planned.args["replacement"], "def456");
        let planned = plan_for_meta(
            "act",
            &argv(&["display", "request", "--reason", "please share"]),
        )
        .unwrap();
        assert_eq!(planned.args["reason"], "please share");
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "audio",
                "spawn",
                "--args",
                "{\"id\":\"a1\",\"provider\":\"openai\",\"playbook\":\"greet\"}",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["id"], "a1");
        assert_eq!(planned.args["provider"], "openai");
        assert_eq!(planned.args["playbook"], "greet");
    }

    /// ctl's agenda write vocabulary and the task-start mode pair plan
    /// verbatim (review round 19): the bare mode flags, the patch
    /// flags with explicit clears, the single-verb edge commands with
    /// --remove, the recurrence/trigger/launch-pin schedule flags, and
    /// the attest flags.
    #[test]
    fn ctl_agenda_vocabulary_plans_verbatim() {
        let planned =
            plan_for_meta("act", &argv(&["task", "start", "fix it", "--direct"])).unwrap();
        assert_eq!(planned.args["orchestrate"], serde_json::json!(false));
        let planned =
            plan_for_meta("act", &argv(&["task", "start", "fix it", "--orchestrate"])).unwrap();
        assert_eq!(planned.args["orchestrate"], serde_json::json!(true));
        assert!(plan_for_meta(
            "act",
            &argv(&["task", "start", "x", "--orchestrate", "--direct"])
        )
        .is_err());
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "patch",
                "item-1",
                "--title",
                "new title",
                "--clear-due",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["patch"]["title"], "new title");
        assert_eq!(planned.args["patch"]["due_ms"], serde_json::Value::Null);
        let planned =
            plan_for_meta("act", &argv(&["agenda", "patch", "item-1", "--clear-tags"])).unwrap();
        assert_eq!(planned.args["patch"]["tags"], serde_json::json!([]));
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "relies-on", "item-1", "item-2", "--remove"]),
        )
        .unwrap();
        assert_eq!(planned.args["op"], "remove_relies_on");
        let planned =
            plan_for_meta("act", &argv(&["agenda", "relies-on", "item-1", "item-2"])).unwrap();
        assert_eq!(planned.args["op"], "add_relies_on");
        assert!(
            plan_for_meta(
                "act",
                &argv(&[
                    "agenda",
                    "relates",
                    "item-1",
                    "item-2",
                    "--kind",
                    "duplicates",
                    "--remove"
                ])
            )
            .is_err(),
            "kind types the link being added"
        );
        let abs = abs_path("/srv/notes.md");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "ref",
                "item-1",
                abs.as_str(),
                "--type",
                "file",
                "--must-read",
                "--remove",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["op"], "remove_ref");
        assert!(
            planned.args.get("must_read").is_none(),
            "the remove op's strict shape drops add-only fields"
        );
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "1700000000000",
                "run the sweep",
                "--every",
                "86400000",
                "--agent",
                "codex",
                "--project",
                "/srv/proj",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["recurrence"]["every_ms"], 86_400_000u64);
        assert_eq!(planned.args["agent_config"]["agent"], "codex");
        assert_eq!(planned.args["project_root"], "/srv/proj");
        // Triggered schedules: --on-unblock with no instant arms on
        // approval (the __now sentinel fills at dispatch);
        // --on-item-match parses ctl's compact spec; cadence+trigger
        // and dual triggers refuse in ctl's own words (round 20).
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "gate the question",
                "--on-unblock",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["trigger"]["kind"], "on_unblock");
        assert_eq!(planned.args["fire_at_ms"], "__now");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "gate",
                "--on-item-match",
                "question:gate,urgent",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["trigger"]["kind"], "on_item_match");
        assert_eq!(planned.args["trigger"]["item_kind"], "question");
        assert_eq!(
            planned.args["trigger"]["tags"],
            serde_json::json!(["gate", "urgent"])
        );
        assert!(plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--every",
                "1000",
                "--on-unblock"
            ])
        )
        .is_err());
        assert!(
            plan_for_meta(
                "act",
                &argv(&["agenda", "schedule", "item-1", "--on-unblock"])
            )
            .is_err(),
            "goal is required"
        );
        // The one-command unplace: --remove sends the empty parent the
        // daemon resolves to the current placement (round 20).
        let planned =
            plan_for_meta("act", &argv(&["agenda", "place", "item-1", "--remove"])).unwrap();
        assert_eq!(planned.args["op"], "remove_part_of");
        assert_eq!(planned.args["parent_id"], "");
        assert!(plan_for_meta(
            "act",
            &argv(&["agenda", "place", "item-1", "hub-1", "--remove"])
        )
        .is_err());
        // Stamp carries the same launch pins (round 20).
        let planned = plan_for_meta(
            "act",
            &argv(&["agenda", "stamp", "fix-task", "--agent", "codex"]),
        )
        .unwrap();
        assert_eq!(planned.args["agent_config"]["agent"], "codex");
        // The dispatch clock sentinel becomes a number.
        let mut planned = planned_with(
            &["agenda", "schedule"],
            serde_json::json!({ "fire_at_ms": "__now" }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap();
        assert!(planned.args["fire_at_ms"].is_u64());
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "attest",
                "item-1",
                "--occurrence",
                "occ-1",
                "--outcome",
                "achieved",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["occurrence"], "occ-1");
        assert_eq!(planned.args["outcome"], "achieved");
    }

    /// Planning runs pre-auth in the ingress gate, so an oversized
    /// value must refuse cheaply — before any per-element collection —
    /// and must never be reflected near its own size (security
    /// review: the region CSV is byte-capped ahead of its Vec<f64>,
    /// and plan-error reflections are char-capped).
    #[test]
    fn oversized_values_refuse_cheaply_without_reflection() {
        let huge = "1,".repeat(1 << 20);
        let err = plan_for_meta(
            "act",
            &argv(&["shared", "focus", "--region", huge.as_str()]),
        )
        .unwrap_err();
        assert!(err.contains("bytes"), "{err}");
        assert!(err.len() < 200, "reflected {} bytes", err.len());
        let err = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "schedule",
                "item-1",
                "--goal",
                "g",
                "--at",
                "+1h",
                "--every",
                huge.as_str(),
            ]),
        )
        .unwrap_err();
        assert!(err.len() < 200, "reflected {} bytes", err.len());
        let err = plan_for_meta("act", &argv(&["ask", "q", "--wait", huge.as_str()])).unwrap_err();
        assert!(err.len() < 200, "reflected {} bytes", err.len());
    }

    /// Wrap raw args in a [`PlannedCall`] for a named registry row —
    /// the substitution tests exercise dispatch behavior for specific
    /// specs without re-deriving argv forms.
    fn planned_with(path: &[&str], args: serde_json::Value) -> PlannedCall {
        let spec = COMMANDS
            .iter()
            .find(|spec| spec.path == path)
            .expect("registry row");
        PlannedCall {
            tool: spec.tool,
            args,
            spec,
            caller_defaults: Vec::new(),
        }
    }

    /// The dispatcher fills seed-declared identity defaults with the
    /// caller's own identity, so two facade sessions never collide as
    /// the same lease holder (review round 10) — carried OUT-OF-BAND
    /// as `caller_defaults` (review round 25), so no input string is
    /// reserved: an explicit literal `"__caller"` is caller data and
    /// survives, as does any sentinel spelling under other keys.
    #[test]
    fn dispatch_sentinels_substitute_key_scoped() {
        // The defaulted form: planning strips the seed marker and
        // records the key; dispatch fills the identity.
        let planned = plan_for_meta("act", &argv(&["browser", "acquire", "ws-1"])).unwrap();
        assert_eq!(planned.caller_defaults, vec!["holder_id".to_string()]);
        assert!(planned.args.get("holder_id").is_none());
        let mut planned = planned;
        substitute_dispatch_sentinels(&mut planned, "sess-7").unwrap();
        assert_eq!(planned.args["holder_id"], "sess-7");
        // An explicit holder — even the literal sentinel spelling —
        // is caller data and passes untouched.
        let mut planned = plan_for_meta(
            "act",
            &argv(&["browser", "acquire", "ws-1", "--holder", "__caller"]),
        )
        .unwrap();
        assert!(planned.caller_defaults.is_empty());
        substitute_dispatch_sentinels(&mut planned, "sess-7").unwrap();
        assert_eq!(planned.args["holder_id"], "__caller");
        let mut planned = planned_with(
            &["browser", "acquire"],
            serde_json::json!({ "holder_id": "alice" }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-7").unwrap();
        assert_eq!(planned.args["holder_id"], "alice");
        // Literal sentinel spellings under other keys are caller data,
        // not sentinels — `notify __now` stays the text "__now".
        let mut planned = planned_with(
            &["browser", "acquire"],
            serde_json::json!({ "body": "__now", "reason": "__caller" }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-7").unwrap();
        assert_eq!(planned.args["body"], "__now");
        assert_eq!(planned.args["reason"], "__caller");
        // And the clock sentinel fills only its own key.
        let mut planned = planned_with(
            &["agenda", "schedule"],
            serde_json::json!({ "fire_at_ms": "__now" }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-7").unwrap();
        assert!(planned.args["fire_at_ms"].is_u64());
    }

    /// When-kind values ride to dispatch as `"__when:"` strings and
    /// resolve there through ctl's own `parse_due_ms` — only at the
    /// exact dotted paths the resolved row declares (review rounds
    /// 22-23), including nested ones (`recurrence.until_ms`) and ctl's
    /// 10-digit epoch-seconds heuristic; a bad form fails with ctl's
    /// wording; caller-owned opaque JSON is never walked, even when a
    /// key inside it shares a When name.
    #[test]
    fn when_sentinels_resolve_with_ctl_vocabulary_at_dispatch() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut planned = planned_with(
            &["agenda", "schedule"],
            serde_json::json!({
                "fire_at_ms": "__when:+2h",
                "recurrence": { "until_ms": "__when:1756400000" },
            }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap();
        let fire = planned.args["fire_at_ms"].as_u64().unwrap();
        assert!(fire >= before + 2 * 3_600_000);
        assert!(fire <= before + 2 * 3_600_000 + 60_000);
        // ctl's heuristic: 10-digit values are epoch seconds.
        assert_eq!(planned.args["recurrence"]["until_ms"], 1_756_400_000_000u64);
        let mut planned = planned_with(
            &["agenda", "patch"],
            serde_json::json!({ "patch": { "due_ms": "__when:bogus" } }),
        );
        let err = substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap_err();
        assert!(err.contains("could not parse due"), "{err}");
        // A "__when:" spelling outside the row's declared paths is
        // caller data — even under a same-named key inside opaque
        // JSON (`peer task --context '{"due_ms":"__when:+2h"}'`).
        let mut planned = planned_with(
            &["agenda", "schedule"],
            serde_json::json!({ "body": "__when:+2h" }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap();
        assert_eq!(planned.args["body"], "__when:+2h");
        let mut planned = planned_with(
            &["peer", "task"],
            serde_json::json!({ "context": { "due_ms": "__when:+2h", "note": "__when:bogus" } }),
        );
        substitute_dispatch_sentinels(&mut planned, "sess-1").unwrap();
        assert_eq!(planned.args["context"]["due_ms"], "__when:+2h");
        assert_eq!(planned.args["context"]["note"], "__when:bogus");
    }

    /// `agenda ask` mirrors ctl's own split (review round 10): plain
    /// TEXT parks an ordinary question item; --questions is the
    /// structured op:ask form; mixing or omitting both is a plan error.
    #[test]
    fn agenda_ask_splits_plain_and_structured_forms() {
        let planned =
            plan_for_meta("act", &argv(&["agenda", "ask", "When should this run?"])).unwrap();
        assert_eq!(planned.args["op"], "add");
        assert_eq!(planned.args["kind"], "question");
        assert_eq!(planned.args["title"], "When should this run?");
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "ask",
                "--questions",
                "[{\"question\":\"which?\",\"options\":[{\"label\":\"a\"}]}]",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["op"], "ask");
        assert!(planned.args.get("kind").is_none());
        assert!(planned.args.get("title").is_none());
        assert_eq!(planned.args["questions"][0]["question"], "which?");
        let err = plan_for_meta(
            "act",
            &argv(&["agenda", "ask", "text", "--questions", "[]"]),
        )
        .unwrap_err();
        assert!(err.contains("not both"), "{err}");
        let err = plan_for_meta("act", &argv(&["agenda", "ask"])).unwrap_err();
        assert!(err.contains("needs the question TEXT"), "{err}");
    }

    /// Session-note image attachments travel as literal JSON (review
    /// round 10) — the daemon reads no caller paths; base64 goes in the
    /// objects.
    #[test]
    fn session_note_carries_image_attachments() {
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "session",
                "note",
                "see this",
                "--images",
                "[{\"media_type\":\"image/png\",\"data\":\"aGk=\"}]",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["images"][0]["media_type"], "image/png");
    }

    /// Bare `agenda list` serves the useful default (open work) instead
    /// of the whole ledger, `--all` lifts the seed, and an explicit
    /// status overrides it (review round 9); the client-side ctl
    /// renders — the answered-union, --blocked/--frontier/--under — are
    /// deliberately NOT registry vocabulary (review round 11): a pure
    /// single-call planner cannot derive them, the row help says so,
    /// and this pin cements the exclusion as intent rather than
    /// omission.
    #[test]
    fn agenda_list_defaults_to_open_and_all_lifts_it() {
        let planned = plan_for_meta("inspect", &argv(&["agenda", "list"])).unwrap();
        assert_eq!(planned.args["status"], "open");
        let planned = plan_for_meta("inspect", &argv(&["agenda", "list", "--all"])).unwrap();
        assert!(planned.args.get("status").is_none());
        assert!(planned.args.get("__all_statuses").is_none());
        let planned =
            plan_for_meta("inspect", &argv(&["agenda", "list", "--status", "done"])).unwrap();
        assert_eq!(planned.args["status"], "done");
        for render_only in ["--blocked", "--frontier", "--under"] {
            assert!(
                plan_for_meta("inspect", &argv(&["agenda", "list", render_only])).is_err(),
                "{render_only} is a ctl client-side render, not registry vocabulary"
            );
        }
    }

    /// Park-time refs land atomically with the item (review round 11):
    /// add and ask both carry literal ref objects, so no observer ever
    /// sees a context-free item and no later failure strands one.
    #[test]
    fn agenda_parks_carry_refs_atomically() {
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "add",
                "fix the roof",
                "--refs",
                "[{\"ref_type\":\"file\",\"locator\":\"/srv/roof.md\",\"must_read\":true}]",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["refs"][0]["ref_type"], "file");
        assert_eq!(planned.args["refs"][0]["must_read"], true);
        let planned = plan_for_meta(
            "act",
            &argv(&[
                "agenda",
                "ask",
                "when?",
                "--refs",
                "[{\"ref_type\":\"url\",\"locator\":\"https://example.com\"}]",
            ]),
        )
        .unwrap();
        assert_eq!(planned.args["refs"][0]["ref_type"], "url");
    }

    /// ctl's default forms plan identically through the facade (review
    /// round 8): an omitted frame id reads the latest frame, an omitted
    /// holder gets facade provenance (overridable), and an omitted
    /// blocker id clears the sole live blocker (empty prefix — the
    /// daemon resolves it uniquely).
    #[test]
    fn optional_positionals_keep_ctl_default_forms() {
        let planned = plan_for_meta("inspect", &argv(&["display", "read-frame"])).unwrap();
        assert_eq!(planned.args["frame_id"], "latest");
        let planned =
            plan_for_meta("inspect", &argv(&["display", "read-frame", "frame-7"])).unwrap();
        assert_eq!(planned.args["frame_id"], "frame-7");
        let planned = plan_for_meta("act", &argv(&["browser", "acquire", "ws-1"])).unwrap();
        assert!(
            planned.args.get("holder_id").is_none()
                && planned.caller_defaults == vec!["holder_id".to_string()],
            "the identity default travels out-of-band; dispatch fills the caller"
        );
        let planned =
            plan_for_meta("act", &argv(&["browser", "acquire", "ws-1", "sess-4"])).unwrap();
        assert_eq!(planned.args["holder_id"], "sess-4");
        assert!(planned.caller_defaults.is_empty());
        let planned = plan_for_meta("act", &argv(&["agenda", "unblock", "item-1"])).unwrap();
        assert_eq!(planned.args["blocker_id"], "");
        let planned =
            plan_for_meta("act", &argv(&["agenda", "unblock", "item-1", "blk-2"])).unwrap();
        assert_eq!(planned.args["blocker_id"], "blk-2");
    }

    /// Target-session routing travels on notify and session note
    /// (review round 6).
    #[test]
    fn notify_and_note_carry_target_session_routing() {
        let planned =
            plan_for_meta("act", &argv(&["notify", "done", "--session", "sess-9"])).unwrap();
        assert_eq!(planned.args["session_id"], "sess-9");
        let planned = plan_for_meta(
            "act",
            &argv(&["session", "note", "hello", "--session", "sess-9"]),
        )
        .unwrap();
        assert_eq!(planned.args["session_id"], "sess-9");
    }

    /// Verdict seeds survive planning: the memory curation rows carry
    /// their verdict in the seed and the positionals land beside it.
    #[test]
    fn seeded_verdict_rows_plan_complete_args() {
        let planned = plan_for_meta(
            "authorize",
            &argv(&[
                "memory",
                "supersede",
                "abc12345",
                "def67890",
                "--reason",
                "newer",
            ]),
        )
        .unwrap();
        assert_eq!(planned.tool, "memory_judge");
        assert_eq!(planned.args["verdict"], "supersede");
        assert_eq!(planned.args["id"], "abc12345");
        assert_eq!(planned.args["replacement"], "def67890");
        assert_eq!(planned.args["reason"], "newer");
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
        assert_eq!(
            facade_gate_operation("authorize", &argv(&["approval", "approve", "7"])),
            Some(Op::Approval)
        );
        assert_eq!(
            facade_gate_operation("inspect", &argv(&["status"])),
            Some(Op::StatsRead)
        );
        // A PATH failure authorizes at the read floor: nothing executes —
        // dispatch re-plans, pressure-gates, and returns the parse error.
        assert_eq!(
            facade_gate_operation("inspect", &argv(&["nope"])),
            Some(Op::StatsRead)
        );
        // The gate never parses VALUES (security review: caller JSON is
        // parsed only after access.decision): a malformed value still
        // authorizes at the resolved command's operation, and dispatch
        // planning is where it fails.
        assert_eq!(
            facade_gate_operation("act", &argv(&["cu", "actions", "not json at all"])),
            Some(crate::mcp::mcp_tool_operation("execute_cu_actions"))
        );
        assert!(plan_for_meta("act", &argv(&["cu", "actions", "not json at all"])).is_err());
        assert!(facade_gate_operation("get_status", &serde_json::json!({})).is_none());
        assert_eq!(
            facade_gate_operation("help", &serde_json::json!({})),
            Some(Op::StatsRead)
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
        assert_eq!(facade_tool_advertised("events", read_only), Some(true));
        assert_eq!(facade_tool_advertised("inspect", read_only), Some(true));
        // A display.view principal legitimately passes act rows now: the
        // shared-view verbs mutate what the user WATCHES, and the daemon
        // has always classed them under the view operation — so act is
        // advertised. A principal holding only the pure-read ops still
        // sees no act.
        assert_eq!(facade_tool_advertised("act", read_only), Some(true));
        let stats_only = |op: Op| matches!(op, Op::StatsRead | Op::SessionInspect);
        assert_eq!(facade_tool_advertised("act", stats_only), Some(false));
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
        assert_eq!(
            facade_tool_advertised("events", approvals_only),
            Some(false),
            "events rides session.inspect, not the approval operation"
        );
        let nothing = |_: Op| false;
        assert_eq!(facade_tool_advertised("help", nothing), Some(false));
        assert_eq!(facade_tool_advertised("events", nothing), Some(false));
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
        // Support files derive from the BuiltinSkill manifest (review
        // round 4's P2): a skill that references its bundled files is a
        // complete package through the facade.
        let with_files = render_docs(&serde_json::json!({ "skill": "intendant-log-search" }));
        assert!(
            with_files.contains("Support files"),
            "manifest expected in skill fetch"
        );
        assert!(with_files.contains("references/query-recipes.md"));
        let file = render_docs(&serde_json::json!({
            "skill": "intendant-log-search",
            "file": "references/query-recipes.md",
        }));
        assert!(file.len() > 1000, "support file body expected");
        let unknown = render_docs(&serde_json::json!({
            "skill": "intendant-log-search",
            "file": "nope.md",
        }));
        assert!(unknown.contains("available:"), "{unknown}");
        assert!(
            render_docs(&serde_json::json!({ "file": "references/query-recipes.md" }))
                .contains("needs its skill")
        );
    }
}
