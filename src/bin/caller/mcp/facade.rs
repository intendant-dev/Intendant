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
    /// A literal JSON value (object/array/scalar) parsed from the argv
    /// string at plan time — a parse failure is a plan error, never a
    /// dispatch. The argv value is the JSON text itself; the CLI-side
    /// `@file`/stdin expansions stay client-side as ever.
    Json,
    /// JSON when the value parses as JSON, otherwise the literal string
    /// — ctl's `JSON|TEXT` contract (peer task context).
    JsonOrText,
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
        flags: &[],
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
                "questions",
                "questions",
                Json,
                "multi-question form: JSON array of question objects (omit QUESTION)"
            ),
            flag!("previews", "previews", Json, "preview cards (JSON array)"),
            flag!("pick-min", "pick_min", U64, "minimum selections (0 = optional)"),
            flag!("pick-max", "pick_max", U64, "maximum selections (default 1)"),
            flag!("multi", "multi_select", Bool, "allow multiple selections"),
            flag!(
                "free-text",
                "free_text",
                Json,
                "true (default) or false — false requires an option pick"
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
            flag!(
                "orchestrate",
                "orchestrate",
                Json,
                "true forces orchestration, false forces a direct session; omit for automatic selection"
            ),
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
            flag!("kind", "kind", Str, "note|task|question (default task, matching ctl)"),
            flag!("due-ms", "due_ms", U64, "reminder instant, ms since epoch"),
            flag!("source", "source", Str, "self-described caller label"),
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
        positionals: &[],
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
        positionals: &[],
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
            flag!("profile-dir", "profile_dir", Str, "browser profile dir"),
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
        positionals: &[p_json("REGION", "region", false)],
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
        help: "Focus the shared view on a normalized region (REGION JSON or --region x,y,w,h)",
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
        flags: &[flag!("wait-s", "wait_s", U64, "one bounded wait, 1-60s")],
        help: "Wait one bounded chunk for a remote job (chunk longer waits client-side)",
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
            p_json("PATCH", "patch", true),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Merge-patch an item (absent = keep, null = clear)",
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
            flag!("due-ms", "due_ms", U64, "reminder instant, ms since epoch"),
            flag!("source", "source", Str, "self-described caller label"),
            flag!(
                "questions",
                "__ask_questions",
                Json,
                "structured option-bearing form: JSON array of question objects (omit TEXT)"
            ),
            flag!(
                "refs",
                "refs",
                Json,
                "source pointers, atomically with the park: [{ref_type, locator, must_read?, label?}]"
            ),
        ],
        help: "Park a durable question (plain TEXT, or --questions for the structured form — ctl's own split)",
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
            p_str("UNDER", "under", true, false),
        ],
        flags: &[flag!(
            "source",
            "source",
            Str,
            "self-described caller label"
        )],
        help: "Re-parent an item under a hub",
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
            flag!("note", "note", Str, "attestation note"),
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
            p_u64("FIRE_AT_MS", "fire_at_ms"),
            p_str("GOAL", "goal", true, true),
        ],
        flags: &[
            flag!("goal", "goal", Str, "ctl-spelling alias for GOAL"),
            flag!(
                "at",
                "fire_at_ms",
                U64,
                "fire instant in epoch ms (ctl's human --at vocabulary is converted client-side)"
            ),
            flag!("fire-at-ms", "fire_at_ms", U64, "fire instant, ms since epoch"),
            flag!("orchestrate", "orchestrate", Bool, "orchestrated session"),
            flag!("interactive", "interactive", Bool, "interactive session"),
            flag!("recurrence", "recurrence", Json, "{\"every_ms\":..}"),
            flag!("agent-config", "agent_config", Json, "agent launch pins"),
            flag!("trigger", "trigger", Json, "{\"kind\":\"on_unblock\"} etc."),
            flag!("project-root", "project_root", Str, "project root"),
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
            flag!("project-root", "project_root", Str, "project root"),
            flag!("fire-at-ms", "fire_at_ms", U64, "first fire instant"),
            flag!("every-ms", "every_ms", U64, "recurrence period"),
            flag!(
                "suspend-after",
                "suspend_after",
                U64,
                "suspend after N failures"
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
            flag!("project-root", "project_root", Str, "project root"),
            flag!("agent-config", "agent_config", Json, "agent launch pins"),
            flag!(
                "interactive",
                "interactive",
                Json,
                "true (default) or false — false is the autonomous goal-run"
            ),
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
            ValueKind::Json => serde_json::from_str(raw)
                .map_err(|e| format!("{key}: expected literal JSON ({e})"))?,
            ValueKind::JsonOrText => serde_json::from_str(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
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
            insert_value(&mut obj, flag.json_key, flag.kind, &raw)?;
        } else if let Some(pos) = spec.positionals.get(positional_index) {
            if pos.greedy {
                greedy_key = Some(pos.json_key);
                greedy_kind = pos.kind;
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
        obj.insert(key.to_string(), value);
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
    // ctl's `--allow-dirty` is the negative of the tool's require_clean.
    if obj.remove("__allow_dirty").is_some() {
        obj.insert("require_clean".to_string(), serde_json::Value::Bool(false));
    }
    // ctl's `--one-shot` is the negative of the halt's persistent field.
    if obj.remove("__one_shot").is_some() {
        obj.insert("persistent".to_string(), serde_json::Value::Bool(false));
    }
    // ctl's `--region x,y,width,height` (normalized 0-1 floats) becomes
    // the tool's region object.
    if let Some(csv) = obj.remove("__region_csv") {
        if obj.contains_key("region") {
            return Err("pass one region — either the REGION positional or --region".to_string());
        }
        let text = csv.as_str().unwrap_or_default();
        let parts: Vec<f64> = text
            .split(',')
            .map(|p| p.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|_| format!("--region expects x,y,width,height (got {text:?})"))?;
        let [x, y, width, height] = parts.as_slice() else {
            return Err(format!("--region expects four values (got {text:?})"));
        };
        obj.insert(
            "region".to_string(),
            serde_json::json!({ "x": x, "y": y, "width": width, "height": height }),
        );
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
    // The `agenda list --all` shape: lift the seeded open-status default
    // so the whole ledger (every status) comes back.
    if obj.remove("__all_statuses").is_some() {
        obj.remove("status");
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
    Ok(serde_json::Value::Object(obj))
}

/// Replace the `"__caller"` identity sentinel in a planned call's
/// top-level string values with the dispatching caller's identity. The
/// planner is pure and cannot know who is calling, but a CONSTANT
/// default identity would make two different facade sessions collide as
/// the "same" holder (the browser-lease registry rejects only a
/// DIFFERENT holder id, so a constant silently hands one session's
/// exclusive lease to another). The dispatcher substitutes after
/// resolution; the gate's resolution ignores argument values, so
/// authorization is unaffected.
pub(crate) fn substitute_caller_identity(args: &mut serde_json::Value, identity: &str) {
    if let Some(obj) = args.as_object_mut() {
        for value in obj.values_mut() {
            if value.as_str() == Some("__caller") {
                *value = serde_json::Value::String(identity.to_string());
            }
        }
    }
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
/// through to the fixed per-tool map). `Some(op)`: authorize this
/// operation, then dispatch. An executor call whose argv fails to parse
/// authorizes at the harmless read floor: nothing will execute — dispatch
/// re-plans, applies the rewind-only pressure gate first, and returns the
/// parse error as a tool result — and the error's content is registry
/// shape, the same disclosure class as `help`.
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
            plan_for_meta(name, args)
                .map(|planned| crate::mcp::mcp_tool_operation(planned.tool))
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
        // The task-start mode override is tri-state: true forces
        // orchestration, false forces direct (ctl's --direct), omitted
        // keeps automatic selection (review round 2).
        let planned = plan_for_meta(
            "act",
            &argv(&["task", "start", "fix it", "--orchestrate", "false"]),
        )
        .unwrap();
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
                "--free-text",
                "false",
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

    /// The dispatcher replaces the identity sentinel with the caller's
    /// own identity, so two facade sessions never collide as the same
    /// lease holder (review round 10); explicit values pass untouched.
    #[test]
    fn caller_identity_sentinel_substitutes_at_dispatch() {
        let mut args = serde_json::json!({ "holder_id": "__caller", "workspace_id": "ws-1" });
        substitute_caller_identity(&mut args, "sess-7");
        assert_eq!(args["holder_id"], "sess-7");
        assert_eq!(args["workspace_id"], "ws-1");
        let mut args = serde_json::json!({ "holder_id": "alice" });
        substitute_caller_identity(&mut args, "sess-7");
        assert_eq!(args["holder_id"], "alice");
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
        assert_eq!(
            planned.args["holder_id"], "__caller",
            "the identity sentinel travels to dispatch, which substitutes the caller"
        );
        let planned =
            plan_for_meta("act", &argv(&["browser", "acquire", "ws-1", "sess-4"])).unwrap();
        assert_eq!(planned.args["holder_id"], "sess-4");
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
        // A parse failure authorizes at the read floor: nothing executes —
        // dispatch re-plans, pressure-gates, and returns the parse error.
        assert_eq!(
            facade_gate_operation("inspect", &argv(&["nope"])),
            Some(Op::StatsRead)
        );
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
