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
pub(crate) enum ValueKind {
    Str,
    U64,
    Bool,
    /// Repeatable string flag collected into a JSON array.
    StrList,
    /// A literal JSON value (object/array/scalar) parsed from the argv
    /// string at plan time — a parse failure is a plan error, never a
    /// dispatch. The argv value is the JSON text itself; the CLI-side
    /// `@file`/stdin expansions stay client-side as ever.
    Json,
    /// JSON when the value parses as JSON, otherwise the literal string
    /// — ctl's `JSON|TEXT` contract (peer task context).
    JsonOrText,
    /// ctl's WHEN vocabulary (`+2h`, epoch digits — 10-digit values are
    /// seconds — RFC3339, `YYYY-MM-DD [HH:MM]`), resolved to epoch ms by
    /// `ctl::parse_due_ms`. The relative and calendar forms need the
    /// clock and local timezone, which planning may not read, so the raw
    /// text rides to dispatch as a key-scoped `"__when:<raw>"` sentinel
    /// and `substitute_dispatch_sentinels` resolves it (the daemon's
    /// clock is the one schedules fire on, so it is also the right one
    /// to parse against).
    When,
    /// ctl's INTERVAL vocabulary (`45m`, `2h`, `7d`, `1w`, or raw ms),
    /// parsed at plan time by `ctl::parse_duration_ms` — durations are
    /// clock-free, so planning stays pure.
    Interval,
}

#[derive(Debug)]
pub(crate) struct PositionalSpec {
    pub(crate) name: &'static str,
    pub(crate) json_key: &'static str,
    pub(crate) kind: ValueKind,
    pub(crate) required: bool,
    /// A greedy positional joins every remaining non-flag token with
    /// spaces (free-text tails like task descriptions). Must be last.
    pub(crate) greedy: bool,
}

#[derive(Debug)]
pub(crate) struct FlagSpec {
    /// Flag name without the leading dashes.
    pub(crate) name: &'static str,
    pub(crate) json_key: &'static str,
    pub(crate) kind: ValueKind,
    pub(crate) help: &'static str,
}

#[derive(Debug)]
pub(crate) struct CommandSpec {
    /// Command path segments, e.g. `["approval", "approve"]`.
    pub(crate) path: &'static [&'static str],
    pub(crate) lane: RiskLane,
    /// The underlying MCP tool this command executes as. Its IAM operation
    /// (via `mcp_tool_operation`) is the command's authorization target.
    pub(crate) tool: &'static str,
    /// JSON object merged into the arguments first (op tags, defaults).
    /// Positionals and flags overwrite seed keys.
    pub(crate) seed: &'static str,
    pub(crate) positionals: &'static [PositionalSpec],
    pub(crate) flags: &'static [FlagSpec],
    pub(crate) help: &'static str,
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

const fn p_u64_opt(name: &'static str, json_key: &'static str) -> PositionalSpec {
    PositionalSpec {
        name,
        json_key,
        kind: ValueKind::U64,
        required: false,
        greedy: false,
    }
}

const fn p_json(name: &'static str, json_key: &'static str, required: bool) -> PositionalSpec {
    PositionalSpec {
        name,
        json_key,
        kind: ValueKind::Json,
        required,
        greedy: false,
    }
}

/// Greedy TOKEN LIST: the remaining argv tokens land as a JSON string
/// array, word boundaries preserved — the shape a command argv needs
/// (a greedy Str joins with spaces and would destroy quoting).
const fn p_rest(name: &'static str, json_key: &'static str) -> PositionalSpec {
    PositionalSpec {
        name,
        json_key,
        kind: ValueKind::StrList,
        required: true,
        greedy: true,
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
        flags: &[flag!("text", "text", Str, "the response text (ctl spelling)")],
        help: "Answer the pending askHuman question",
    },
    CommandSpec {
        path: &["ask"],
        lane: RiskLane::Act,
        tool: "ask_user",
        seed: "{}",
        positionals: &[p_str("QUESTION", "question", false, true)],
        flags: &[
            flag!("header", "header", Str, "short topic chip"),
            flag!(
                "option",
                "__options",
                StrList,
                "choice label (repeatable, max 4)"
            ),
            flag!(
                "schema",
                "__schema_client",
                Str,
                "ctl-side only: ctl reads the schema file/stdin — here pass --questions"
            ),
            flag!(
                "questions",
                "questions",
                Json,
                "multi-question form: JSON array of question objects (omit QUESTION)"
            ),
            flag!(
                "preview-text",
                "__preview_text",
                StrList,
                "LABEL=VALUE inline preview card (repeatable, ctl spelling)"
            ),
            flag!(
                "preview-html",
                "__preview_file_client",
                StrList,
                "ctl-side only: ctl reads the HTML file — here pass --previews"
            ),
            flag!(
                "preview-image",
                "__preview_file_client",
                StrList,
                "ctl-side only: ctl reads the image file — here pass --previews"
            ),
            flag!("previews", "previews", Json, "preview cards (JSON array)"),
            flag!(
                "pick",
                "__pick",
                Str,
                "selection bounds MIN[-MAX] (ctl spelling; replaces --multi)"
            ),
            flag!("pick-min", "pick_min", U64, "minimum selections (0 = optional)"),
            flag!("pick-max", "pick_max", U64, "maximum selections (default 1)"),
            flag!("multi", "multi_select", Bool, "allow multiple selections"),
            flag!(
                "free-text",
                "free_text",
                Bool,
                "document that a typed answer is welcome (the rail always accepts one)"
            ),
            flag!(
                "no-free-text",
                "__no_free_text",
                Bool,
                "require an option pick (free_text: false)"
            ),
            flag!(
                "consequence",
                "consequence",
                Str,
                "what happens if the question lapses unanswered"
            ),
            flag!("wait", "wait_seconds", U64, "block seconds (default 300, max 900)"),
            flag!(
                "expires",
                "expiry",
                Str,
                "decision window (\"2h\", \"tomorrow 9am\", RFC3339)"
            ),
            flag!("park", "park", Bool, "file as a durable agenda question instead of blocking"),
            flag!("session", "session_id", Str, "ask as another session"),
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
            flag!("session", "session_id", Str, "notify as another session"),
        ],
        help: "Fire-and-forget notification to the user",
    },
    CommandSpec {
        path: &["session", "note"],
        lane: RiskLane::Act,
        tool: "post_session_note",
        seed: "{}",
        positionals: &[p_str("TEXT", "text", true, true)],
        flags: &[
            flag!("source", "source", Str, "short source label"),
            flag!("session", "session_id", Str, "post into another session"),
            flag!(
                "image",
                "__image_file_client",
                StrList,
                "ctl-side only: ctl reads and encodes the file — here pass --images"
            ),
            flag!(
                "images",
                "images",
                Json,
                "JSON array of {media_type, data (base64), name?} attachments"
            ),
        ],
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
            flag!("task", "task", Str, "ctl-spelling alias for TASK"),
            flag!("session-id", "session_id", Str, "ctl-spelling alias for --session"),
            flag!(
                "orchestrate",
                "__orchestrate",
                Bool,
                "force orchestration mode (omit both mode flags for automatic selection)"
            ),
            flag!("direct", "__direct", Bool, "force a direct session"),
            flag!(
                "frame",
                "reference_frame_ids",
                StrList,
                "reference frame id (repeatable; routes to the CU runner)"
            ),
            flag!(
                "display-target",
                "display_target",
                Str,
                "display target for a computer-use task"
            ),
        ],
        help: "Start an agent task (or queue a follow-up into a session)",
    },
    CommandSpec {
        path: &["agenda", "list"],
        lane: RiskLane::Inspect,
        tool: "agenda_list",
        seed: r#"{"status":"open"}"#,
        positionals: &[],
        flags: &[
            flag!(
                "status",
                "status",
                Str,
                "open (default) | done | retired"
            ),
            flag!("all", "__all_statuses", Bool, "the whole ledger, every status"),
            flag!("open", "__list_open", Bool, "open items (the default, explicit — ctl selector)"),
            flag!("done", "__list_done", Bool, "done items (ctl selector)"),
            flag!("retired", "__list_retired", Bool, "retired items (ctl selector)"),
            flag!("q", "q", Str, "server-side search"),
        ],
        help: "List agenda items (default: open work). ctl's --blocked/--frontier/--under and its bare-list answered-union are client-side renders over this read — fetch and derive",
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
        seed: r#"{"op":"add","kind":"task"}"#,
        positionals: &[p_str("TITLE", "title", true, true)],
        flags: &[
            flag!("body", "body", Str, "markdown body"),
            flag!("tag", "tags", StrList, "tag (repeatable)"),
            flag!(
                "kind",
                "__kind_explicit",
                Str,
                "note|task|question (default task, matching ctl)"
            ),
            flag!("note", "__kind_note", Bool, "park a note (ctl shorthand)"),
            flag!("task", "__kind_task", Bool, "park a task (ctl shorthand, the default)"),
            flag!(
                "due",
                "due_ms",
                When,
                "reminder instant — ctl's WHEN vocabulary: +2h, epoch, RFC3339, YYYY-MM-DD [HH:MM]"
            ),
            flag!("due-ms", "due_ms", U64, "reminder instant, ms since epoch"),
            flag!("source", "source", Str, "self-described caller label"),
            flag!(
                "ref",
                "__park_refs",
                StrList,
                "[TYPE:]LOCATOR source pointer (repeatable, ctl spelling)"
            ),
            flag!(
                "must-read",
                "__park_must_read",
                Bool,
                "mark every --ref of this park must-read"
            ),
            flag!("label", "__park_label", Str, "label every --ref of this park"),
            flag!(
                "refs",
                "refs",
                Json,
                "source pointers, atomically with the park: [{ref_type, locator, must_read?, label?}]"
            ),
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
            flag!("project", "project", Str, "project provenance"),
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
    // The terminal family (owner-ruled): reads on inspect; resize/close on
    // act; open (shell.spawn) and write (command execution in a live
    // shell) on authorize — writing into a shell IS running commands, the
    // same class as spawning one.
    CommandSpec {
        path: &["terminal", "list"],
        lane: RiskLane::Inspect,
        tool: "terminal_list",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "List the shell sessions visible to the caller",
    },
    CommandSpec {
        path: &["terminal", "open"],
        lane: RiskLane::Authorize,
        tool: "terminal_open",
        seed: "{}",
        positionals: &[p_str("TERMINAL_ID", "terminal_id", false, false)],
        flags: &[
            flag!("cols", "cols", U64, "initial columns (default 120)"),
            flag!("rows", "rows", U64, "initial rows (default 32)"),
            flag!("shared", "shared", Bool, "visible to other principals"),
        ],
        help: "Open or create a shell PTY session (shell-spawn class)",
    },
    CommandSpec {
        path: &["terminal", "read"],
        lane: RiskLane::Inspect,
        tool: "terminal_read",
        seed: "{}",
        positionals: &[p_str("TERMINAL_ID", "terminal_id", true, false)],
        flags: &[
            flag!(
                "cursor",
                "cursor",
                U64,
                "from a previous read's next_cursor"
            ),
            flag!(
                "max-bytes",
                "max_bytes",
                U64,
                "cap one read (default 16384)"
            ),
        ],
        help: "Cursor-paged read of a shell session's output",
    },
    CommandSpec {
        path: &["terminal", "write"],
        lane: RiskLane::Authorize,
        tool: "terminal_write",
        seed: "{}",
        positionals: &[
            p_str("TERMINAL_ID", "terminal_id", true, false),
            p_str("INPUT", "input", true, true),
        ],
        flags: &[flag!(
            "no-enter",
            "__no_enter",
            Bool,
            "raw keystrokes, no Enter"
        )],
        help: "Write a command line (or raw input) into a live shell",
    },
    CommandSpec {
        path: &["terminal", "resize"],
        lane: RiskLane::Act,
        tool: "terminal_resize",
        seed: "{}",
        positionals: &[
            p_str("TERMINAL_ID", "terminal_id", true, false),
            p_u64("COLS", "cols"),
            p_u64("ROWS", "rows"),
        ],
        flags: &[],
        help: "Resize a shell session's PTY",
    },
    CommandSpec {
        path: &["terminal", "close"],
        lane: RiskLane::Act,
        tool: "terminal_close",
        seed: "{}",
        positionals: &[p_str("TERMINAL_ID", "terminal_id", true, false)],
        flags: &[],
        help: "Close a shell session",
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
            flag!("item-id", "item_id", Str, "ctl-spelling alias for ITEM_ID"),
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
            flag!("item-id", "anchor.item_id", Str, "ctl-spelling alias for ITEM_ID"),
            flag!("reason", "reason", Str, "ctl-spelling alias for REASON"),
            flag!("primer", "primer", Str, "ctl-spelling alias for PRIMER"),
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
            flag!(
                "discard",
                "discard",
                StrList,
                "dead end to record as discarded (repeatable)"
            ),
            flag!(
                "artifact",
                "artifacts",
                StrList,
                "produced artifact to carry across (repeatable)"
            ),
            flag!(
                "next-step",
                "next_steps",
                StrList,
                "recommended continuation step (repeatable)"
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
            flag!("record-id", "record_id", Str, "rewind record (ctl spelling)"),
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
            flag!(
                "full-values",
                "full_values",
                Bool,
                "uncapped element values/titles (long URLs, document text)"
            ),
        ],
        help: "Read the accessibility element tree",
    },
    CommandSpec {
        path: &["cu", "actions"],
        lane: RiskLane::Act,
        tool: "execute_cu_actions",
        seed: "{}",
        positionals: &[p_json("ACTIONS", "actions", true)],
        flags: &[
            flag!("actions", "actions", Json, "ctl-spelling alias for ACTIONS"),
            flag!("target", "display_target", Str, "display target"),
            flag!(
                "coordinate-space",
                "coordinate_space",
                Str,
                "pixel (default) or normalized_1000"
            ),
            flag!("observe", "observe", Str, "pixels|ax|auto|none"),
            flag!("annotate", "annotate", Bool, "draw click crosshairs"),
            flag!("settle", "settle", Json, "true/false or settle cap ms"),
        ],
        help: "Execute a JSON array of computer-use actions",
    },
    CommandSpec {
        path: &["cu", "proof"], lane: RiskLane::Act, tool: "external_cu_proof", seed: "{}",
        positionals: &[], flags: &[flag!("request", "request", Str, "duplicate-key-safe proof request JSON")],
        help: "Drive a bounded, model-free external proof session",
    },
    CommandSpec {
        path: &["cu", "task"],
        lane: RiskLane::Act,
        tool: "run_bounded_cu_task",
        seed: "{}",
        positionals: &[p_str("TASK", "task", true, true)],
        flags: &[
            flag!("mode", "mode", Str, "stage or attest"),
            flag!("attempt", "attempt_id", Str, "exact Scout attempt id"),
            flag!("workspace", "workspace_id", Str, "exact browser workspace id"),
            flag!("display-id", "display_id", U64, "daemon virtual display id"),
            flag!("target", "display_target", Str, "canonical display_N target"),
            flag!(
                "capture-generation",
                "capture_generation",
                Str,
                "opaque display generation"
            ),
            flag!(
                "prior-receipt",
                "prior_receipt_id",
                Str,
                "stage receipt continued by attest"
            ),
        ],
        help: "Run a bounded CU-only stage or read-only attestation task",
    },
    CommandSpec {
        path: &["display", "create"],
        lane: RiskLane::Act,
        tool: "create_virtual_display",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("width", "width", U64, "width px (default 1920)"),
            flag!("height", "height", U64, "height px (default 1080)"),
        ],
        help: "Create a virtual display",
    },
    CommandSpec {
        path: &["display", "destroy"],
        lane: RiskLane::Act,
        tool: "destroy_virtual_display",
        seed: "{}",
        positionals: &[
            p_u64("DISPLAY_ID", "display_id"),
            p_str("CAPTURE_GENERATION", "capture_generation", false, false),
        ],
        flags: &[flag!("note", "note", Str, "why the display is destroyed")],
        help: "Destroy the exact generation returned by display create",
    },
    CommandSpec {
        path: &["display", "frames"],
        lane: RiskLane::Inspect,
        tool: "list_frames",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("stream", "stream", Str, "stream filter, e.g. display_99"),
            flag!("count", "count", U64, "max frames (default 20)"),
        ],
        help: "List captured display frames",
    },
    CommandSpec {
        path: &["display", "read-frame"],
        lane: RiskLane::Inspect,
        tool: "read_frame",
        seed: r#"{"frame_id":"latest"}"#,
        positionals: &[p_str("FRAME_ID", "frame_id", false, false)],
        flags: &[flag!(
            "stream",
            "stream",
            Str,
            "stream filter when FRAME_ID is \"latest\""
        )],
        help: "Read one captured frame (\"latest\" works)",
    },
    CommandSpec {
        path: &["display", "take"],
        lane: RiskLane::Act,
        tool: "take_display",
        seed: "{}",
        positionals: &[p_u64("DISPLAY_ID", "display_id")],
        flags: &[],
        help: "Claim a display for this agent",
    },
    CommandSpec {
        path: &["display", "release"],
        lane: RiskLane::Act,
        tool: "release_display",
        seed: "{}",
        positionals: &[p_u64("DISPLAY_ID", "display_id")],
        flags: &[flag!("note", "note", Str, "why control is released")],
        help: "Release a claimed display",
    },
    CommandSpec {
        path: &["display", "grant-user"],
        lane: RiskLane::Authorize,
        tool: "grant_user_display",
        seed: "{}",
        positionals: &[p_u64_opt("DISPLAY_ID", "display_id")],
        flags: &[flag!(
            "display-id",
            "display_id",
            U64,
            "user-session display (omit = primary)"
        )],
        help: "Mint agent access to the user's session display",
    },
    CommandSpec {
        path: &["display", "revoke-user"],
        lane: RiskLane::Authorize,
        tool: "revoke_user_display",
        seed: "{}",
        positionals: &[p_u64_opt("DISPLAY_ID", "display_id")],
        flags: &[
            flag!("display-id", "display_id", U64, "omit = primary"),
            flag!("note", "note", Str, "why revoked"),
        ],
        help: "Revoke agent access to the user's session display",
    },
    CommandSpec {
        path: &["display", "request"],
        lane: RiskLane::Act,
        tool: "request_user_display",
        seed: "{}",
        positionals: &[p_str("REASON", "reason", true, true)],
        flags: &[
            flag!("reason", "reason", Str, "ctl-spelling alias for REASON"),
            flag!(
                "access",
                "access",
                Str,
                "view (default) or view_and_control"
            ),
            flag!("wait", "wait_seconds", U64, "wait for decision (cap 600)"),
            flag!("session", "session_id", Str, "attribution session id"),
        ],
        help: "Ask the user to share their display (asks only; blocks)",
    },
    CommandSpec {
        path: &["browser", "providers"],
        lane: RiskLane::Inspect,
        tool: "browser_workspace_providers",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "List browser workspace providers",
    },
    CommandSpec {
        path: &["browser", "list"],
        lane: RiskLane::Inspect,
        tool: "list_browser_workspaces",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "List browser workspaces",
    },
    CommandSpec {
        path: &["browser", "create"],
        lane: RiskLane::Act,
        tool: "create_browser_workspace",
        seed: "{}",
        positionals: &[p_str("URL", "url", false, false)],
        flags: &[
            flag!("url", "url", Str, "URL to open (omit = about:blank)"),
            flag!("label", "label", Str, "dashboard label"),
            flag!(
                "provider",
                "provider",
                Str,
                "auto|cdp|system_cdp|playwright|agent_browser|stream"
            ),
            flag!("peer", "peer_id", Str, "federation peer (ctl spelling)"),
            flag!("session", "owner_session_id", Str, "owning session (ctl spelling)"),
            flag!("owner-session", "owner_session_id", Str, "owning session"),
            flag!(
                "display-target",
                "display_target",
                Str,
                "daemon-created virtual display"
            ),
            flag!("profile-dir", "profile_dir", Str, "browser profile dir"),
            flag!(
                "extension-archive",
                "extension_archive_path",
                Str,
                "absolute immutable extension zip"
            ),
            flag!(
                "extension-sha256",
                "extension_archive_sha256",
                Str,
                "expected lowercase extension archive sha256"
            ),
            flag!(
                "extension-bytes",
                "extension_archive_byte_length",
                U64,
                "expected extension archive byte length"
            ),
            flag!(
                "extension-manifest-version",
                "extension_manifest_version",
                U64,
                "expected manifest_version"
            ),
            flag!(
                "extension-version",
                "extension_version",
                Str,
                "expected extension version"
            ),
        ],
        help: "Create a browser workspace",
    },
    CommandSpec {
        path: &["browser", "close"],
        lane: RiskLane::Act,
        tool: "close_browser_workspace",
        seed: "{}",
        positionals: &[p_str("WORKSPACE_ID", "workspace_id", true, false)],
        flags: &[flag!("reason", "reason", Str, "why closed")],
        help: "Close a browser workspace",
    },
    CommandSpec {
        path: &["browser", "acquire"],
        lane: RiskLane::Act,
        tool: "acquire_browser_workspace",
        seed: r#"{"holder_id":"__caller"}"#,
        positionals: &[
            p_str("WORKSPACE_ID", "workspace_id", true, false),
            p_str("HOLDER_ID", "holder_id", false, false),
        ],
        flags: &[
            flag!("holder", "holder_id", Str, "holder taking the lease (ctl spelling)"),
            flag!("holder-id", "holder_id", Str, "holder taking the lease"),
            flag!("holder-kind", "holder_kind", Str, "holder kind"),
            flag!("note", "note", Str, "lease note"),
            flag!("force", "force", Bool, "steal a live lease"),
        ],
        help: "Acquire a browser workspace lease",
    },
    CommandSpec {
        path: &["browser", "release"],
        lane: RiskLane::Act,
        tool: "release_browser_workspace",
        seed: "{}",
        positionals: &[p_str("WORKSPACE_ID", "workspace_id", true, false)],
        flags: &[
            flag!("holder", "holder_id", Str, "holder releasing (ctl spelling)"),
            flag!("holder-id", "holder_id", Str, "holder releasing"),
            flag!("note", "note", Str, "release note"),
        ],
        help: "Release a browser workspace lease",
    },
    CommandSpec {
        path: &["shared", "show"],
        lane: RiskLane::Act,
        tool: "show_shared_view",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("target", "display_target", Str, "display target"),
            flag!("display-id", "display_id", U64, "numeric display id"),
            flag!("reason", "reason", Str, "why the user should watch"),
            flag!(
                "focus",
                "__focus_csv",
                Str,
                "x,y,width,height normalized 0-1 (ctl spelling)"
            ),
            flag!(
                "focus-region",
                "focus_region",
                Json,
                "{\"x\":..,\"y\":..,\"width\":..,\"height\":..} normalized 0-1"
            ),
        ],
        help: "Show the shared view of a display to the user",
    },
    CommandSpec {
        path: &["shared", "focus"],
        lane: RiskLane::Act,
        tool: "focus_shared_view",
        seed: "{}",
        // ctl parses the positional and --region through the same CSV
        // grammar; both spell x,y,width,height.
        positionals: &[p_str("REGION", "__region_csv", false, false)],
        flags: &[
            flag!(
                "region",
                "__region_csv",
                Str,
                "x,y,width,height normalized 0-1 (ctl spelling)"
            ),
            flag!("target", "display_target", Str, "display target"),
            flag!("display-id", "display_id", U64, "numeric display id"),
            flag!("note", "note", Str, "short label"),
        ],
        help: "Focus the shared view on a normalized region (REGION or --region, x,y,w,h)",
    },
    CommandSpec {
        path: &["shared", "focus-clear"],
        lane: RiskLane::Act,
        tool: "clear_shared_view_focus",
        seed: "{}",
        positionals: &[],
        flags: &[flag!("reason", "reason", Str, "why focus cleared")],
        help: "Clear the shared-view focus region",
    },
    CommandSpec {
        // ctl's three-segment spelling of the same retraction; the
        // resolver prefers the longest matching path, so this wins over
        // `shared focus` whenever the third token is literally "clear".
        path: &["shared", "focus", "clear"],
        lane: RiskLane::Act,
        tool: "clear_shared_view_focus",
        seed: "{}",
        positionals: &[],
        flags: &[flag!("reason", "reason", Str, "why focus cleared")],
        help: "Clear the shared-view focus region (ctl spelling)",
    },
    CommandSpec {
        path: &["shared", "input"],
        lane: RiskLane::Authorize,
        tool: "request_shared_view_input",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("target", "display_target", Str, "display target"),
            flag!("display-id", "display_id", U64, "numeric display id"),
            flag!("reason", "reason", Str, "why input authority is wanted"),
        ],
        help: "Ask the user for shared-view input authority",
    },
    CommandSpec {
        path: &["shared", "hide"],
        lane: RiskLane::Act,
        tool: "hide_shared_view",
        seed: "{}",
        positionals: &[],
        flags: &[flag!("reason", "reason", Str, "why dismissed")],
        help: "Hide the shared view",
    },
    CommandSpec {
        path: &["shared", "capture"],
        lane: RiskLane::Inspect,
        tool: "capture_shared_view_frame",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!("target", "display_target", Str, "display target"),
            flag!("display-id", "display_id", U64, "numeric display id"),
            flag!("reason", "reason", Str, "note in the shared-view banner"),
        ],
        help: "Capture one shared-view frame",
    },
    CommandSpec {
        path: &["settings", "autonomy"],
        lane: RiskLane::Authorize,
        tool: "set_autonomy",
        seed: "{}",
        positionals: &[p_str("LEVEL", "level", true, false)],
        flags: &[],
        help: "Set the global autonomy level (low|medium|high|full)",
    },
    CommandSpec {
        path: &["settings", "verbosity"],
        lane: RiskLane::Act,
        tool: "set_verbosity",
        seed: "{}",
        positionals: &[p_str("LEVEL", "level", true, false)],
        flags: &[],
        help: "Set output verbosity (quiet|normal|verbose|debug)",
    },
    CommandSpec {
        path: &["remote", "start"],
        lane: RiskLane::Authorize,
        tool: "remote_command",
        seed: r#"{"op":"start"}"#,
        positionals: &[p_rest("COMMAND", "argv")],
        flags: &[
            flag!("host", "host", Str, "remote host selector"),
            flag!("branch", "branch", Str, "branch to run on"),
            flag!("cwd", "cwd", Str, "working directory"),
            flag!("env", "__env_kv", StrList, "KEY=VALUE (repeatable, ctl spelling)"),
            flag!(
                "source",
                "source",
                Str,
                "git_revision (default) or working_tree"
            ),
            flag!(
                "revision",
                "expected_revision",
                Str,
                "revision guard (ctl spelling)"
            ),
            flag!(
                "expected-revision",
                "expected_revision",
                Str,
                "revision guard"
            ),
            flag!("cache", "cache", Str, "none (default) or durable_sccache"),
            flag!("timeout", "timeout_s", U64, "1-3600 seconds (ctl spelling)"),
            flag!("timeout-s", "timeout_s", U64, "1-3600 (default 900)"),
            flag!(
                "allow-dirty",
                "__allow_dirty",
                Bool,
                "allow a dirty working tree (ctl spelling)"
            ),
            flag!(
                "require-clean",
                "require_clean",
                Json,
                "true (default) or false"
            ),
        ],
        help: "Start a remote command: remote start [flags] -- CMD ARGS… (word boundaries preserved)",
    },
    CommandSpec {
        path: &["remote", "status"],
        lane: RiskLane::Authorize,
        tool: "remote_command",
        seed: r#"{"op":"status"}"#,
        positionals: &[p_str("JOB_ID", "job_id", true, false)],
        flags: &[],
        help: "Remote job status (read semantics; rides the shell-spawn op)",
    },
    CommandSpec {
        path: &["remote", "wait"],
        lane: RiskLane::Authorize,
        tool: "remote_command",
        seed: r#"{"op":"wait"}"#,
        positionals: &[p_str("JOB_ID", "job_id", true, false)],
        flags: &[
            flag!(
                "for",
                "__wait_for",
                U64,
                "wait budget in seconds (ctl spelling; one bounded chunk here, max 60)"
            ),
            flag!("wait-s", "wait_s", U64, "one bounded wait, 1-60s"),
        ],
        help: "Wait one bounded chunk for a remote job (ctl's --for loops longer budgets client-side)",
    },
    CommandSpec {
        path: &["remote", "cancel"],
        lane: RiskLane::Authorize,
        tool: "remote_command",
        seed: r#"{"op":"cancel"}"#,
        positionals: &[p_str("JOB_ID", "job_id", true, false)],
        flags: &[],
        help: "Cancel a remote job",
    },
    CommandSpec {
        path: &["agenda", "patch"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"patch"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_json("PATCH", "patch", false),
        ],
        flags: &[
            flag!("title", "patch.title", Str, "new title (ctl spelling)"),
            flag!("body", "patch.body", Str, "new body (ctl spelling)"),
            flag!(
                "tag",
                "patch.tags",
                StrList,
                "replacement tag (repeatable, ctl spelling)"
            ),
            flag!(
                "due",
                "patch.due_ms",
                When,
                "new due instant — ctl's WHEN vocabulary: +2h, epoch, RFC3339, YYYY-MM-DD [HH:MM]"
            ),
            flag!("due-ms", "patch.due_ms", U64, "new due instant, ms since epoch"),
            flag!("clear-tags", "__clear_tags", Bool, "empty the tag list"),
            flag!("clear-due", "__clear_due", Bool, "clear the due instant"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Merge-patch an item: PATCH JSON, or the ctl flags (absent = keep, clears explicit)",
    },
    CommandSpec {
        path: &["agenda", "reopen"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"reopen"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Reopen a completed/retired item",
    },
    CommandSpec {
        path: &["agenda", "retire"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"retire"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Retire an item without completing it",
    },
    CommandSpec {
        path: &["agenda", "pickup"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"pick_up"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Pick up an item to work it",
    },
    CommandSpec {
        path: &["agenda", "acknowledge"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"acknowledge_answer"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Acknowledge an answered question",
    },
    CommandSpec {
        path: &["agenda", "ask"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add","kind":"question"}"#,
        positionals: &[p_str("TEXT", "title", false, true)],
        flags: &[
            flag!("body", "body", Str, "markdown body"),
            flag!("tag", "tags", StrList, "tag (repeatable)"),
            flag!(
                "due",
                "due_ms",
                When,
                "reminder instant — ctl's WHEN vocabulary: +2h, epoch, RFC3339, YYYY-MM-DD [HH:MM]"
            ),
            flag!("due-ms", "due_ms", U64, "reminder instant, ms since epoch"),
            flag!("source", "source", Str, "self-described caller label"),
            flag!(
                "option",
                "__agenda_options",
                StrList,
                "decision-card choice \"Label[:description]\" (repeatable; makes the structured form)"
            ),
            flag!(
                "pick",
                "__agenda_pick",
                Str,
                "selection bounds MIN[-MAX] (replaces --multi)"
            ),
            flag!(
                "multi",
                "__agenda_multi",
                Bool,
                "any subset of the options may be picked"
            ),
            flag!("header", "__agenda_header", Str, "short topic chip"),
            flag!(
                "consequence",
                "__agenda_consequence",
                Str,
                "what happens if the question lapses unanswered"
            ),
            flag!(
                "ref",
                "__park_refs",
                StrList,
                "[TYPE:]LOCATOR source pointer (repeatable, ctl spelling)"
            ),
            flag!(
                "must-read",
                "__park_must_read",
                Bool,
                "mark every --ref of this park must-read"
            ),
            flag!("label", "__park_label", Str, "label every --ref of this park"),
            flag!(
                "questions",
                "__ask_questions",
                Json,
                "structured multi-question form: JSON array of question objects (omit TEXT)"
            ),
            flag!(
                "refs",
                "refs",
                Json,
                "source pointers, atomically with the park: [{ref_type, locator, must_read?, label?}]"
            ),
        ],
        help: "Park a durable question (plain TEXT; --option flags make ctl's decision card; --questions is the raw form)",
    },
    CommandSpec {
        path: &["agenda", "block"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"set_blocker"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("CRITERION", "criterion", true, true),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Set a blocker criterion on an item",
    },
    CommandSpec {
        path: &["agenda", "unblock"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"clear_blocker","blocker_id":""}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("BLOCKER_ID", "blocker_id", false, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Clear a blocker",
    },
    CommandSpec {
        // ctl's spelling: one verb, --remove flips the edge off.
        path: &["agenda", "relies-on"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_relies_on"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("TARGET_ID", "target_id", true, false),
        ],
        flags: &[
            flag!("remove", "__remove_relies", Bool, "remove the edge instead"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Add (or with --remove, drop) a relies-on edge — ctl's spelling",
    },
    CommandSpec {
        path: &["agenda", "relates"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_relates_to"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("TARGET_ID", "target_id", true, false),
        ],
        flags: &[
            flag!("kind", "link_kind", Str, "closed relates-to vocabulary (add only)"),
            flag!("remove", "__remove_relates", Bool, "remove the link instead"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Add (or with --remove, drop) a relates-to link — ctl's spelling",
    },
    CommandSpec {
        path: &["agenda", "ref"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_ref"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("LOCATOR", "__ref_locator", true, false),
        ],
        flags: &[
            flag!(
                "type",
                "ref_type",
                Str,
                "file|dir|memory|session|url (overrides a TYPE: prefix)"
            ),
            flag!("must-read", "must_read", Bool, "mark the ref must-read"),
            flag!("label", "label", Str, "ref label"),
            flag!("remove", "__remove_ref", Bool, "detach the ref instead"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Attach (or with --remove, detach) a ref — [TYPE:]LOCATOR, ctl's grammar (url inferred from http(s); other types need the prefix or --type)",
    },
    CommandSpec {
        path: &["agenda", "relies-add"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_relies_on"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("TARGET_ID", "target_id", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Add a relies-on edge",
    },
    CommandSpec {
        path: &["agenda", "relies-remove"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"remove_relies_on"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("TARGET_ID", "target_id", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Remove a relies-on edge",
    },
    CommandSpec {
        path: &["agenda", "part-add"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_part_of"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("PARENT_ID", "parent_id", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Add a part-of placement",
    },
    CommandSpec {
        path: &["agenda", "part-remove"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"remove_part_of"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("PARENT_ID", "parent_id", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Remove a part-of placement",
    },
    CommandSpec {
        path: &["agenda", "place"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"place"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("UNDER", "under", false, false),
        ],
        flags: &[
            flag!("under", "under", Str, "ctl-spelling alias for UNDER"),
            flag!(
                "remove",
                "__unplace",
                Bool,
                "remove the CURRENT placement (the daemon resolves the parent)"
            ),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Re-parent an item under a hub, or --remove its current placement",
    },
    CommandSpec {
        path: &["agenda", "relates-add"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_relates_to"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("TARGET_ID", "target_id", true, false),
        ],
        flags: &[
            flag!(
                "link-kind",
                "link_kind",
                Str,
                "closed relates-to vocabulary"
            ),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Add a relates-to link",
    },
    CommandSpec {
        path: &["agenda", "relates-remove"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"remove_relates_to"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("TARGET_ID", "target_id", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Remove a relates-to link",
    },
    CommandSpec {
        path: &["agenda", "ref-add"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"add_ref"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("REF_TYPE", "ref_type", true, false),
            p_str("LOCATOR", "locator", true, false),
        ],
        flags: &[
            flag!("must-read", "must_read", Bool, "mark the ref must-read"),
            flag!("label", "label", Str, "ref label"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Attach a ref (file|dir|memory|session|url + locator)",
    },
    CommandSpec {
        path: &["agenda", "ref-remove"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"remove_ref"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("REF_TYPE", "ref_type", true, false),
            p_str("LOCATOR", "locator", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Detach a ref",
    },
    CommandSpec {
        path: &["agenda", "attest"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"attest"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("OCCURRENCE", "occurrence", true, false),
            p_str("OUTCOME", "outcome", true, false),
        ],
        flags: &[
            flag!(
                "occurrence",
                "occurrence",
                Str,
                "ctl-spelling alias for OCCURRENCE"
            ),
            flag!("outcome", "outcome", Str, "ctl-spelling alias for OUTCOME"),
            flag!("note", "note", Str, "attestation note"),
            flag!(
                "ref",
                "__attest_ref_client",
                StrList,
                "ctl-side only: ctl hashes the file into the pin — here pass --refs"
            ),
            flag!(
                "refs",
                "refs",
                Json,
                "binding refs [{\"locator\":..,\"sha256\":..}] (digests computed client-side)"
            ),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Attest an occurrence outcome",
    },
    CommandSpec {
        path: &["agenda", "schedule"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"propose_effect"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_u64_opt("FIRE_AT_MS", "fire_at_ms"),
            p_str("GOAL", "goal", false, true),
        ],
        flags: &[
            flag!("goal", "goal", Str, "ctl-spelling alias for GOAL"),
            flag!(
                "on-item-match",
                "__on_item_match",
                Str,
                "fire when a matching item parks: KIND:TAG[,TAG...] (e.g. question:gate)"
            ),
            flag!(
                "at",
                "fire_at_ms",
                When,
                "fire instant — ctl's WHEN vocabulary: +2h, epoch, RFC3339, YYYY-MM-DD [HH:MM]"
            ),
            flag!("fire-at-ms", "fire_at_ms", U64, "fire instant, ms since epoch"),
            flag!("orchestrate", "orchestrate", Bool, "orchestrated session"),
            flag!("interactive", "interactive", Bool, "interactive session"),
            flag!(
                "every",
                "recurrence.every_ms",
                Interval,
                "recurrence period — ctl's INTERVAL vocabulary: 45m, 2h, 7d, 1w, or ms"
            ),
            flag!(
                "until",
                "recurrence.until_ms",
                When,
                "recurrence end (WHEN vocabulary)"
            ),
            flag!(
                "max-occurrences",
                "recurrence.max_occurrences",
                U64,
                "occurrence cap"
            ),
            flag!(
                "suspend-after",
                "recurrence.suspend_after_failures",
                U64,
                "suspend after N failures"
            ),
            flag!(
                "on-unblock",
                "__on_unblock",
                Bool,
                "fire when the item's prerequisites clear"
            ),
            flag!("recurrence", "recurrence", Json, "{\"every_ms\":..}"),
            flag!("agent", "agent_config.agent", Str, "launch pin: backend"),
            flag!(
                "claude-model",
                "agent_config.claude_model",
                Str,
                "launch pin"
            ),
            flag!(
                "claude-effort",
                "agent_config.claude_effort",
                Str,
                "launch pin"
            ),
            flag!("codex-model", "agent_config.codex_model", Str, "launch pin"),
            flag!(
                "codex-reasoning-effort",
                "agent_config.codex_reasoning_effort",
                Str,
                "launch pin"
            ),
            flag!("kimi-model", "agent_config.kimi_model", Str, "launch pin"),
            flag!(
                "kimi-thinking",
                "agent_config.kimi_thinking",
                Str,
                "launch pin"
            ),
            flag!(
                "dial-autonomy",
                "agent_config.dial.autonomy",
                Str,
                "session dial"
            ),
            flag!("dial-ask", "agent_config.dial.ask", Str, "session dial"),
            flag!(
                "dial-notify",
                "agent_config.dial.notify",
                Str,
                "session dial"
            ),
            flag!(
                "dial-approve",
                "__dial_approve",
                StrList,
                "CATEGORY=RULE (repeatable, e.g. network=deny)"
            ),
            flag!(
                "agent-config",
                "agent_config",
                Json,
                "the full launch-pin object (the flags above cover the common pins)"
            ),
            flag!("trigger", "trigger", Json, "{\"kind\":\"on_item_match\",..} etc."),
            flag!("project", "project_root", Str, "project root (ctl spelling)"),
            flag!("project-root", "project_root", Str, "project root"),
            flag!(
                "binding-ref",
                "__binding_ref_client",
                StrList,
                "ctl-side only: ctl reads and digests the file — here pass --binding-refs"
            ),
            flag!(
                "binding-refs",
                "binding_refs",
                Json,
                "[{\"locator\":..,\"sha256\":..}] (digests computed client-side)"
            ),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Propose a scheduled effect (owner approval binds it later)",
    },
    CommandSpec {
        path: &["agenda", "stamp"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"stamp"}"#,
        positionals: &[p_str("DEFINITION", "definition", true, false)],
        flags: &[
            flag!("project", "project_root", Str, "project root (ctl spelling)"),
            flag!("project-root", "project_root", Str, "project root"),
            flag!(
                "at",
                "fire_at_ms",
                When,
                "first fire instant — ctl's WHEN vocabulary: +2h, epoch, RFC3339, YYYY-MM-DD [HH:MM]"
            ),
            flag!("fire-at-ms", "fire_at_ms", U64, "first fire instant"),
            flag!(
                "every",
                "every_ms",
                Interval,
                "recurrence period — ctl's INTERVAL vocabulary: 45m, 2h, 7d, 1w, or ms"
            ),
            flag!("every-ms", "every_ms", U64, "recurrence period"),
            flag!(
                "suspend-after",
                "suspend_after",
                U64,
                "suspend after N failures"
            ),
            flag!("note", "annotations", StrList, "annotation (ctl spelling)"),
            flag!("agent", "agent_config.agent", Str, "launch pin: backend"),
            flag!(
                "claude-model",
                "agent_config.claude_model",
                Str,
                "launch pin"
            ),
            flag!(
                "claude-effort",
                "agent_config.claude_effort",
                Str,
                "launch pin"
            ),
            flag!("codex-model", "agent_config.codex_model", Str, "launch pin"),
            flag!(
                "codex-reasoning-effort",
                "agent_config.codex_reasoning_effort",
                Str,
                "launch pin"
            ),
            flag!("kimi-model", "agent_config.kimi_model", Str, "launch pin"),
            flag!(
                "kimi-thinking",
                "agent_config.kimi_thinking",
                Str,
                "launch pin"
            ),
            flag!(
                "dial-autonomy",
                "agent_config.dial.autonomy",
                Str,
                "session dial"
            ),
            flag!("dial-ask", "agent_config.dial.ask", Str, "session dial"),
            flag!(
                "dial-notify",
                "agent_config.dial.notify",
                Str,
                "session dial"
            ),
            flag!(
                "dial-approve",
                "__dial_approve",
                StrList,
                "CATEGORY=RULE (repeatable, e.g. network=deny)"
            ),
            flag!("agent-config", "agent_config", Json, "agent launch pins"),
            flag!(
                "annotation",
                "annotations",
                StrList,
                "annotation (repeatable)"
            ),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Stamp a sealed automation definition",
    },
    CommandSpec {
        path: &["agenda", "request-occurrence"],
        lane: RiskLane::Authorize,
        tool: "agenda_op",
        seed: r#"{"op":"request_occurrence"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[],
        help: "Fire an out-of-schedule run of an approved recurring effect (authority-class)",
    },
    CommandSpec {
        path: &["agenda", "approve"],
        lane: RiskLane::Authorize,
        tool: "agenda_op",
        seed: r#"{"op":"approve_effect"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("DIGEST", "digest", true, false),
        ],
        flags: &[flag!("digest", "digest", Str, "ctl-spelling alias for DIGEST")],
        help: "Bind owner approval to a scheduled effect's manifest digest. Review first: agenda show ID surfaces effects[].manifest and its digest (ctl's digestless review is that read, client-rendered) — approving echoes the digest so what you approve is what you read",
    },
    CommandSpec {
        path: &["agenda", "revoke-schedule"],
        lane: RiskLane::Authorize,
        tool: "agenda_op",
        seed: r#"{"op":"revoke_effect"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[],
        help: "Revoke an approved scheduled effect",
    },
    CommandSpec {
        path: &["agenda", "withdraw"],
        lane: RiskLane::Act,
        tool: "agenda_op",
        seed: r#"{"op":"withdraw_effect"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[
            flag!("reason", "reason", Str, "why withdrawn"),
            flag!("source", "source", Str, "self-described caller label"),
        ],
        help: "Withdraw a proposed scheduled effect",
    },
    CommandSpec {
        path: &["agenda", "start"],
        lane: RiskLane::Authorize,
        tool: "agenda_op",
        seed: r#"{"op":"start_now"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[
            flag!("goal", "goal", Str, "goal override"),
            flag!("project", "project_root", Str, "project root (ctl spelling)"),
            flag!("project-root", "project_root", Str, "project root"),
            flag!(
                "goal-run",
                "__goal_run",
                Bool,
                "autonomous shape (ctl spelling; default is interactive)"
            ),
            flag!(
                "interactive",
                "interactive",
                Json,
                "true (default) or false — false is the autonomous goal-run"
            ),
            flag!("agent", "agent_config.agent", Str, "launch pin: backend"),
            flag!(
                "claude-model",
                "agent_config.claude_model",
                Str,
                "launch pin"
            ),
            flag!(
                "claude-effort",
                "agent_config.claude_effort",
                Str,
                "launch pin"
            ),
            flag!("codex-model", "agent_config.codex_model", Str, "launch pin"),
            flag!(
                "codex-reasoning-effort",
                "agent_config.codex_reasoning_effort",
                Str,
                "launch pin"
            ),
            flag!("kimi-model", "agent_config.kimi_model", Str, "launch pin"),
            flag!(
                "kimi-thinking",
                "agent_config.kimi_thinking",
                Str,
                "launch pin"
            ),
            flag!(
                "dial-autonomy",
                "agent_config.dial.autonomy",
                Str,
                "session dial"
            ),
            flag!("dial-ask", "agent_config.dial.ask", Str, "session dial"),
            flag!(
                "dial-notify",
                "agent_config.dial.notify",
                Str,
                "session dial"
            ),
            flag!(
                "dial-approve",
                "__dial_approve",
                StrList,
                "CATEGORY=RULE (repeatable, e.g. network=deny)"
            ),
            flag!("agent-config", "agent_config", Json, "agent launch pins"),
        ],
        help: "Mint, approve, and fire an item's session in one act (owner surface)",
    },
    CommandSpec {
        path: &["memory", "accept"],
        lane: RiskLane::Authorize,
        tool: "memory_judge",
        seed: r#"{"verdict":"accept"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!("reason", "reason", Str, "rationale")],
        help: "Accept a proposed memory claim (owner curation)",
    },
    CommandSpec {
        path: &["memory", "dispute"],
        lane: RiskLane::Authorize,
        tool: "memory_judge",
        seed: r#"{"verdict":"dispute"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!("reason", "reason", Str, "rationale")],
        help: "Dispute a memory claim (owner curation)",
    },
    CommandSpec {
        path: &["memory", "retire"],
        lane: RiskLane::Authorize,
        tool: "memory_judge",
        seed: r#"{"verdict":"retire"}"#,
        positionals: &[p_str("ID", "id", true, false)],
        flags: &[flag!("reason", "reason", Str, "rationale")],
        help: "Retire a memory claim (owner curation)",
    },
    CommandSpec {
        path: &["memory", "supersede"],
        lane: RiskLane::Authorize,
        tool: "memory_judge",
        seed: r#"{"verdict":"supersede"}"#,
        positionals: &[
            p_str("ID", "id", true, false),
            p_str("REPLACEMENT", "replacement", true, false),
        ],
        flags: &[
            flag!("with", "replacement", Str, "ctl-spelling alias for REPLACEMENT"),
            flag!("reason", "reason", Str, "rationale"),
        ],
        help: "Supersede a memory claim with a replacement (owner curation)",
    },
    CommandSpec {
        path: &["controller", "status"],
        lane: RiskLane::Inspect,
        tool: "get_controller_loop_status",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "Controller loop status",
    },
    CommandSpec {
        path: &["controller", "restart-status"],
        lane: RiskLane::Inspect,
        tool: "get_restart_status",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "Scheduled-restart status",
    },
    CommandSpec {
        path: &["controller", "halt"],
        lane: RiskLane::Authorize,
        tool: "request_controller_loop_halt",
        seed: "{}",
        positionals: &[],
        flags: &[
            flag!(
                "one-shot",
                "__one_shot",
                Bool,
                "halt one cycle only (ctl spelling)"
            ),
            flag!(
                "persistent",
                "persistent",
                Json,
                "true (default) or false — false halts one cycle only"
            ),
        ],
        help: "Halt the controller loop",
    },
    CommandSpec {
        path: &["controller", "clear-halt"],
        lane: RiskLane::Authorize,
        tool: "clear_controller_loop_halt",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "Clear a controller loop halt",
    },
    CommandSpec {
        path: &["controller", "intervene"],
        lane: RiskLane::Authorize,
        tool: "intervene_controller_loop",
        seed: "{}",
        positionals: &[p_str("MODE", "mode", true, false)],
        flags: &[],
        help: "Intervene in the controller loop (stop|abort)",
    },
    CommandSpec {
        path: &["controller", "schedule"],
        lane: RiskLane::Authorize,
        tool: "schedule_controller_restart",
        seed: "{}",
        positionals: &[
            p_str("CONTROLLER_ID", "controller_id", true, false),
            p_str("GOAL", "north_star_goal", true, true),
        ],
        flags: &[
            flag!(
                "controller-id",
                "controller_id",
                Str,
                "ctl-spelling alias for CONTROLLER_ID"
            ),
            flag!("goal", "north_star_goal", Str, "ctl-spelling alias for GOAL"),
            flag!("reason", "reason", Str, "why restart"),
            flag!("after", "restart_after", Str, "turn_end (default) or now"),
            flag!(
                "command",
                "restart_command",
                Str,
                "restart command override"
            ),
            flag!("auto-start", "auto_start_task", Bool, "auto-start the task"),
            flag!("max-attempts", "max_attempts", U64, "default 1"),
            flag!("cooldown-sec", "cooldown_sec", U64, "default 30"),
        ],
        help: "Schedule a controller restart",
    },
    CommandSpec {
        path: &["controller", "cancel"],
        lane: RiskLane::Authorize,
        tool: "cancel_controller_restart",
        seed: "{}",
        positionals: &[],
        flags: &[flag!(
            "restart-id",
            "restart_id",
            Str,
            "guard: reject on mismatch"
        )],
        help: "Cancel a scheduled controller restart",
    },
    CommandSpec {
        path: &["controller", "complete"],
        lane: RiskLane::Authorize,
        tool: "controller_turn_complete",
        seed: "{}",
        positionals: &[
            p_str("RESTART_ID", "restart_id", true, false),
            p_str("TOKEN", "turn_complete_token", true, false),
        ],
        flags: &[
            flag!(
                "restart-id",
                "restart_id",
                Str,
                "ctl-spelling alias for RESTART_ID"
            ),
            flag!(
                "token",
                "turn_complete_token",
                Str,
                "ctl-spelling alias for TOKEN"
            ),
            flag!("status", "status", Str, "completion status"),
            flag!("summary", "handoff_summary", Str, "handoff summary (ctl spelling)"),
            flag!("handoff-summary", "handoff_summary", Str, "handoff summary"),
        ],
        help: "Report a controller turn complete",
    },
    CommandSpec {
        path: &["audio", "spawn"],
        lane: RiskLane::Authorize,
        tool: "spawn_live_audio",
        seed: "{}",
        positionals: &[
            p_str("ID", "id", false, false),
            p_str("PROVIDER", "provider", false, false),
        ],
        flags: &[
            flag!(
                "args",
                "__audio_args",
                Json,
                "the whole tool object, ctl's form: {id, provider, playbook, response_schema, …}"
            ),
            flag!(
                "playbook",
                "playbook",
                Str,
                "system prompt / goal (required by the tool)"
            ),
            flag!(
                "response-schema",
                "response_schema",
                Json,
                "{\"fields\":[..]} (required by the tool)"
            ),
            flag!(
                "timeout-secs",
                "timeout_secs",
                U64,
                "hard timeout (default 300)"
            ),
            flag!("voice", "voice", Str, "voice name"),
            flag!("model", "model", Str, "model override"),
            flag!("initial-message", "initial_message", Str, "text sent first"),
        ],
        help: "Spawn a live audio session (openai|gemini)",
    },
    CommandSpec {
        path: &["peer", "list"],
        lane: RiskLane::Inspect,
        tool: "list_peers",
        seed: "{}",
        positionals: &[],
        flags: &[],
        help: "List federated peers",
    },
    CommandSpec {
        path: &["peer", "message"],
        lane: RiskLane::Act,
        tool: "peer_send_message",
        seed: "{}",
        positionals: &[
            p_str("PEER_ID", "peer_id", true, false),
            p_str("MESSAGE", "message", true, true),
        ],
        flags: &[flag!("session", "session", Str, "peer-side session scope")],
        help: "Send a message to a peer daemon",
    },
    CommandSpec {
        path: &["peer", "task"],
        lane: RiskLane::Authorize,
        tool: "peer_delegate_task",
        seed: "{}",
        positionals: &[
            p_str("PEER_ID", "peer_id", true, false),
            p_str("INSTRUCTIONS", "instructions", true, true),
        ],
        flags: &[flag!(
            "context",
            "context",
            JsonOrText,
            "JSON when it parses, else the literal text (ctl's JSON|TEXT contract)"
        )],
        help: "Delegate an autonomous task to a peer daemon",
    },
    CommandSpec {
        path: &["context", "claim-fission"],
        lane: RiskLane::Authorize,
        tool: "claim_fission_canonical",
        seed: "{}",
        positionals: &[
            p_str("GROUP_ID", "group_id", true, false),
            p_str("BRANCH_SESSION_ID", "branch_session_id", true, false),
        ],
        flags: &[
            flag!("group-id", "group_id", Str, "ctl-spelling alias for GROUP_ID"),
            flag!(
                "branch-session-id",
                "branch_session_id",
                Str,
                "ctl-spelling alias for BRANCH_SESSION_ID"
            ),
            flag!(
                "expected-canonical-session-id",
                "expected_canonical_session_id",
                Str,
                "CAS guard (ctl spelling)"
            ),
            flag!(
                "expected-canonical",
                "expected_canonical_session_id",
                Str,
                "CAS guard"
            ),
        ],
        help: "Claim a fission branch as canonical (CAS over lineage)",
    },
];

/// ctl's family spellings (`ctl.rs::run`'s multi-pattern arms), applied
/// to the first argv token before path resolution. One table instead of
/// duplicated rows: an alias resolves to the canonical family's own
/// rows, so its vocabulary cannot drift ("derive, don't mirror").
pub(crate) const FAMILY_ALIASES: &[(&str, &str)] = &[
    ("browsers", "browser"),
    ("shared-view", "shared"),
    ("approvals", "approval"),
    ("sessions", "session"),
    ("peers", "peer"),
    ("set", "settings"),
];

/// ctl's command-path aliases (the family dispatchers' multi-pattern
/// arms), rewritten whole-path to the canonical row before resolution —
/// the same no-copies rule as [`FAMILY_ALIASES`]. `cu screenshot` is
/// the one cross-family entry: ctl routes it through the display
/// screenshot handler by that name.
pub(crate) const COMMAND_ALIASES: &[(&[&str], &[&str])] = &[
    (&["browser", "ls"], &["browser", "list"]),
    (&["browser", "open"], &["browser", "create"]),
    (&["browser", "take"], &["browser", "acquire"]),
    (&["cu", "exec"], &["cu", "actions"]),
    (&["cu", "read-screen"], &["cu", "elements"]),
    (&["cu", "screenshot"], &["display", "screenshot"]),
    (&["display", "frame"], &["display", "read-frame"]),
    (&["display", "ready"], &["display", "status"]),
    (&["display", "readiness"], &["display", "status"]),
    (&["display", "grant_user"], &["display", "grant-user"]),
    (
        &["display", "grant-user-display"],
        &["display", "grant-user"],
    ),
    (
        &["display", "grant_user_display"],
        &["display", "grant-user"],
    ),
    (&["display", "revoke_user"], &["display", "revoke-user"]),
    (
        &["display", "revoke-user-display"],
        &["display", "revoke-user"],
    ),
    (
        &["display", "revoke_user_display"],
        &["display", "revoke-user"],
    ),
    (&["display", "request-user"], &["display", "request"]),
    (&["display", "request_user"], &["display", "request"]),
    (
        &["display", "request_user_display"],
        &["display", "request"],
    ),
    (&["shared", "request-input"], &["shared", "input"]),
    (&["agenda", "ls"], &["agenda", "list"]),
    (&["agenda", "done"], &["agenda", "complete"]),
    (&["agenda", "pick-up"], &["agenda", "pickup"]),
    (&["agenda", "ack"], &["agenda", "acknowledge"]),
    (&["agenda", "needs"], &["agenda", "relies-on"]),
    (&["agenda", "relates-to"], &["agenda", "relates"]),
    (&["agenda", "edit"], &["agenda", "patch"]),
    (&["context", "inspect-anchor"], &["context", "inspect"]),
    (&["memory", "list"], &["memory", "search"]),
    (&["memory", "ls"], &["memory", "search"]),
    (&["memory", "show"], &["memory", "read"]),
    (&["memory", "add"], &["memory", "propose"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The alias tables stay coherent with the registry: every command
    /// alias rewrites onto a REGISTERED canonical path, no alias source
    /// shadows a real row (an alias must never change what a registered
    /// spelling means), sources are unique, and every family alias
    /// points at a family that actually has rows.
    #[test]
    fn alias_tables_target_registered_rows_and_shadow_nothing() {
        for (alias, canonical) in COMMAND_ALIASES {
            assert!(
                COMMANDS.iter().any(|spec| spec.path == *canonical),
                "alias {alias:?} rewrites to unregistered {canonical:?}"
            );
            assert!(
                !COMMANDS.iter().any(|spec| spec.path == *alias),
                "alias {alias:?} shadows a registered row"
            );
        }
        for (i, (a, _)) in COMMAND_ALIASES.iter().enumerate() {
            for (b, _) in &COMMAND_ALIASES[i + 1..] {
                assert_ne!(a, b, "duplicate command alias source");
            }
        }
        for (alias, family) in FAMILY_ALIASES {
            assert!(
                COMMANDS.iter().any(|spec| spec.path[0] == *family),
                "family alias {alias:?} points at rowless family {family:?}"
            );
            assert!(
                !COMMANDS.iter().any(|spec| spec.path[0] == *alias),
                "family alias {alias:?} shadows a registered family"
            );
        }
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
            Op::TerminalView,
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
}
