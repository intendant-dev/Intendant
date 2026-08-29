//! The terminal tool family: request/response shell access for MCP
//! callers, sharing the dashboard's PTY pool (a shell opened over MCP is
//! attachable from a dashboard terminal tile and vice versa).
//!
//! Deliberate shape decisions (docs/design-mcp-control-lane.md, the
//! owner's terminal ruling):
//!
//! - **Per-tool operations stay honest.** The streaming tunnel's
//!   `terminal_open` frame does a stateful "attach = terminal.view,
//!   create = shell.spawn" split inside the handler; here the split is
//!   structural — `terminal_open` always demands `shell.spawn` (it
//!   creates when absent), while reads ride `terminal.view` and
//!   input/resize/close ride `terminal.write` — so the gate-level
//!   operation IS the tool's whole authority.
//! - **Polling, not streaming.** Output is read with a monotonic cursor
//!   over the scrollback ring ([`crate::terminal::Scrollback`]'s
//!   total-bytes-written space); a cursor that fell off the 256 KiB
//!   window reports `gap: true` — the polling analogue of the push
//!   lane's dropped-output marker.
//! - **Visibility is the registry's own model**: root-surface callers act
//!   as [`TerminalActor::Root`]; every scoped caller acts as its bound
//!   IAM principal and sees only its own and shared sessions. A missing
//!   principal id degrades to a principal that owns nothing.

use super::*;
use crate::peer::access_policy::FilesystemAccessPolicy;
use crate::terminal::{ShellSpawnPolicy, TerminalActor, TerminalKey};

/// Cap on one cursor read. Big enough for a full scrollback replay in
/// four calls, small enough to keep a tool result model-sized.
const TERMINAL_READ_MAX_BYTES: usize = 64 * 1024;
const TERMINAL_READ_DEFAULT_BYTES: usize = 16 * 1024;

/// Params for `terminal_list` (none).
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct TerminalListParams {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TerminalOpenParams {
    /// Terminal id to open or attach (creates the shell when absent).
    /// Omit to mint a fresh id.
    #[serde(default)]
    pub terminal_id: Option<String>,
    /// Initial columns (default 120).
    #[serde(default)]
    pub cols: Option<u16>,
    /// Initial rows (default 32).
    #[serde(default)]
    pub rows: Option<u16>,
    /// Create the shell as shared (visible to other principals). Default
    /// false.
    #[serde(default)]
    pub shared: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TerminalReadParams {
    /// The terminal id.
    pub terminal_id: String,
    /// Cursor from a previous read's `next_cursor` (0 = from the oldest
    /// retained output).
    #[serde(default)]
    pub cursor: Option<u64>,
    /// Max bytes to return (default 16384, cap 65536, floor 4 — a page
    /// never consumes a split UTF-8 sequence).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TerminalWriteParams {
    /// The terminal id.
    pub terminal_id: String,
    /// Bytes to write to the shell's stdin, verbatim.
    pub input: String,
    /// Append Enter (a carriage return — the key a terminal sends to
    /// submit a command line). Default true — pass false for raw
    /// keystrokes.
    #[serde(default)]
    pub enter: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TerminalResizeParams {
    /// The terminal id.
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TerminalCloseParams {
    /// The terminal id.
    pub terminal_id: String,
}

/// Root-surface callers act as root; every scoped caller acts as its
/// bound principal (a missing id owns nothing — fail closed, never
/// root).
fn terminal_actor(
    trust: ToolCallerTrust,
    actor: &crate::access::actor::ActorBinding,
) -> TerminalActor {
    match trust {
        ToolCallerTrust::OwnerSurface => TerminalActor::Root,
        ToolCallerTrust::Scoped => TerminalActor::Principal(
            actor
                .principal_id
                .clone()
                .unwrap_or_else(|| "principal:unattributed".to_string()),
        ),
    }
}

/// How much of `bytes` to deliver so the page never ends inside an
/// INCOMPLETE UTF-8 sequence: the whole page when everything after the
/// last genuinely INVALID sequence (binary output — lossy is honest) is
/// valid, trimmed to the last boundary when the tail is an incomplete
/// sequence a later page completes — invalid bytes earlier in the page
/// don't forfeit that trim (a binary page can still end in a split
/// character). An incomplete prefix is NEVER consumed — even when that
/// means an empty page with a parked cursor — because delivering it
/// lossily advances past bytes the caller can then never reconstruct.
/// Progress is guaranteed by the read floor (`max_bytes` ≥ 4, the
/// longest UTF-8 sequence) plus invalid sequences always being consumed:
/// the scan only ever holds back a tail the ring has not finished
/// receiving yet.
fn utf8_page_len(bytes: &[u8]) -> usize {
    // Walk the page the way the lossy decoder will: valid runs and
    // invalid sequences are delivered; only a trailing incomplete
    // sequence (`error_len() == None`) is held back. Each iteration
    // resumes past the previous error, so the page is scanned once.
    let mut offset = 0;
    loop {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return bytes.len(),
            Err(err) => match err.error_len() {
                Some(skip) => offset += err.valid_up_to() + skip,
                None => return offset + err.valid_up_to(),
            },
        }
    }
}

/// [`utf8_page_len`] while the shell lives; the whole page once it has
/// exited. A split sequence is held back only because a later page can
/// complete it — after exit no output can ever arrive, so holding a
/// truncated final sequence would park the cursor forever; delivering it
/// lossily is the honest end state. (On Windows the exit flag can beat
/// the reader thread's final drain by a moment, so a tail consumed in
/// that window may decay into replacement characters — a one-character
/// cost, against a permanently wedged cursor.)
fn page_keep_len(bytes: &[u8], alive: bool) -> usize {
    if alive {
        utf8_page_len(bytes)
    } else {
        bytes.len()
    }
}

fn no_registry() -> String {
    serde_json::json!({
        "ok": false,
        "error": "terminal registry unavailable on this server shape (bare stdio --mcp has no gateway PTY pool)",
    })
    .to_string()
}

fn no_visible(terminal_id: &str) -> String {
    serde_json::json!({
        "ok": false,
        "error": format!(
            "no visible terminal {terminal_id:?} — terminal_list enumerates yours, terminal_open creates one"
        ),
    })
    .to_string()
}

/// Whether the caller's CURRENT filesystem scope still matches the one
/// the session's shell was spawned under. The OS sandbox is fixed at
/// spawn, so when an operator narrows (or reissues) the owning
/// principal's scope, the old shell keeps enforcing the broader policy
/// — the session is refused as stale rather than serving authority the
/// grant no longer expresses (security review P1). Applies only to
/// sessions the CALLING principal owns: a shared session another
/// principal spawned runs under ITS owner's authority by design, and
/// root surfaces are never scope-bound.
fn scope_is_stale(
    trust: ToolCallerTrust,
    owned: bool,
    spawn_scope: Option<&FilesystemAccessPolicy>,
    current: Option<&FilesystemAccessPolicy>,
) -> bool {
    matches!(trust, ToolCallerTrust::Scoped) && owned && spawn_scope != current
}

fn stale_scope(terminal_id: &str) -> String {
    serde_json::json!({
        "ok": false,
        "error": format!(
            "your filesystem scope changed since terminal {terminal_id:?} was spawned and its sandbox still reflects the old scope — terminal_close it, then terminal_open a fresh shell under the current scope"
        ),
    })
    .to_string()
}

impl IntendantServer {
    pub(crate) async fn terminal_list_tool(
        &self,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
    ) -> String {
        let Some(registry) = self.terminal_registry().await else {
            return no_registry();
        };
        let rows: Vec<serde_json::Value> = registry
            .list_visible(&terminal_actor(trust, actor))
            .await
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "terminal_id": s.key.terminal_id,
                    "host_id": s.key.host_id,
                    "alive": s.alive,
                    "shared": s.shared,
                    "can_manage": s.can_manage,
                    "exit_status": s.exit_status,
                    "cols": s.size.map(|(c, _)| c),
                    "rows": s.size.map(|(_, r)| r),
                })
            })
            .collect();
        serde_json::json!({ "terminals": rows }).to_string()
    }

    pub(crate) async fn terminal_open_tool(
        &self,
        params: TerminalOpenParams,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
        fs_scope: Option<crate::peer::access_policy::FilesystemAccessPolicy>,
    ) -> String {
        let Some(registry) = self.terminal_registry().await else {
            return no_registry();
        };
        let terminal_id = params
            .terminal_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("mcp-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        let key = TerminalKey::local(&terminal_id);
        let acting = terminal_actor(trust, actor);
        let policy = ShellSpawnPolicy {
            // The gate already charged this call as shell.spawn — that is
            // the tool's whole operation, so creation is always permitted
            // here (attach-only callers use terminal_read/terminal_list,
            // which never spawn).
            may_spawn: true,
            shared: params.shared.unwrap_or(false),
            // The caller's grant-resolved filesystem scope, stated by the
            // ingress gate (dashboard-tunnel parity): a scoped grant's
            // shell is OS-sandboxed to its roots, and an ungated caller's
            // default is the empty scope. Environment secrecy does NOT
            // ride this scope — the registry derives it from the ACTOR,
            // so every principal-owned spawn (every Scoped caller,
            // including one whose grant carries no filesystem scope)
            // gets a cleared, secret-free environment and can never read
            // the daemon's provider keys through `env`.
            scope: fs_scope.clone(),
        };
        match registry
            .open_or_attach(
                key,
                params.cols.unwrap_or(120),
                params.rows.unwrap_or(32),
                &acting,
                policy,
            )
            .await
        {
            Ok((session, created)) => {
                // Attaching back to your own live shell whose sandbox
                // predates a scope change is refused up front — the
                // handle would only meet the same staleness refusal on
                // every read and write. (A fresh spawn is by definition
                // under the current scope.)
                if !created
                    && scope_is_stale(
                        trust,
                        session.managed_by(&acting),
                        session.spawn_scope(),
                        fs_scope.as_ref(),
                    )
                {
                    return stale_scope(&terminal_id);
                }
                // The current write high-water is the natural first
                // cursor: a fresh open starts reading at "now".
                let (_, cursor, _) = session.read_since(u64::MAX, 0);
                serde_json::json!({
                    "ok": true,
                    "terminal_id": terminal_id,
                    "created": created,
                    "alive": session.is_alive(),
                    "shared": session.shared(),
                    "cols": session.size().map(|(c, _)| c),
                    "rows": session.size().map(|(_, r)| r),
                    "read_cursor": cursor,
                })
                .to_string()
            }
            Err(err) => serde_json::json!({ "ok": false, "error": err.to_string() }).to_string(),
        }
    }

    pub(crate) async fn terminal_read_tool(
        &self,
        params: TerminalReadParams,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
        fs_scope: Option<FilesystemAccessPolicy>,
    ) -> String {
        let Some(registry) = self.terminal_registry().await else {
            return no_registry();
        };
        let key = TerminalKey::local(&params.terminal_id);
        let acting = terminal_actor(trust, actor);
        let Some(session) = registry.get_visible(&key, &acting).await else {
            return no_visible(&params.terminal_id);
        };
        if scope_is_stale(
            trust,
            session.managed_by(&acting),
            session.spawn_scope(),
            fs_scope.as_ref(),
        ) {
            return stale_scope(&params.terminal_id);
        }
        // Floor 4 (the longest UTF-8 sequence): with boundary-aligned
        // cursors and room for a whole sequence, the boundary trim below
        // can only hold back a tail the ring has not finished receiving.
        let max_bytes = params
            .max_bytes
            .unwrap_or(TERMINAL_READ_DEFAULT_BYTES)
            .clamp(4, TERMINAL_READ_MAX_BYTES);
        let (bytes, next_cursor, gap) = session.read_since(params.cursor.unwrap_or(0), max_bytes);
        // A multibyte UTF-8 sequence split at the page boundary must not
        // decay into replacement characters on both pages: while the
        // shell lives, hold the incomplete tail back (rewinding the
        // cursor to the boundary) so the next read re-delivers it whole;
        // once it has exited, deliver everything. Genuinely invalid
        // bytes mid-page (binary output) stay lossy — that is honest.
        let alive = session.is_alive();
        let kept = page_keep_len(&bytes, alive);
        let next_cursor = next_cursor - (bytes.len() - kept) as u64;
        serde_json::json!({
            "ok": true,
            "output": String::from_utf8_lossy(&bytes[..kept]),
            "next_cursor": next_cursor,
            "gap": gap,
            "alive": alive,
            "exit_status": session.exit_status(),
        })
        .to_string()
    }

    pub(crate) async fn terminal_write_tool(
        &self,
        params: TerminalWriteParams,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
        fs_scope: Option<FilesystemAccessPolicy>,
    ) -> String {
        let Some(registry) = self.terminal_registry().await else {
            return no_registry();
        };
        let key = TerminalKey::local(&params.terminal_id);
        let acting = terminal_actor(trust, actor);
        let Some(session) = registry.get_visible(&key, &acting).await else {
            return no_visible(&params.terminal_id);
        };
        if scope_is_stale(
            trust,
            session.managed_by(&acting),
            session.spawn_scope(),
            fs_scope.as_ref(),
        ) {
            return stale_scope(&params.terminal_id);
        }
        if !session.is_alive() {
            return serde_json::json!({
                "ok": false,
                "error": "shell already exited",
                "exit_status": session.exit_status(),
            })
            .to_string();
        }
        let mut input = params.input.into_bytes();
        if params.enter.unwrap_or(true) {
            // CR, not LF: the byte the Enter key actually sends. ConPTY
            // only submits a command line on CR (terminal.rs's PTY tests
            // pin this), and Unix line discipline maps CR to NL on input
            // (ICRNL), so CR is the correct submit byte everywhere.
            input.push(b'\r');
        }
        session.write_input(&input);
        serde_json::json!({
            "ok": true,
            "wrote_bytes": input.len(),
            "hint": "poll terminal_read with your cursor for the shell's response",
        })
        .to_string()
    }

    pub(crate) async fn terminal_resize_tool(
        &self,
        params: TerminalResizeParams,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
        fs_scope: Option<FilesystemAccessPolicy>,
    ) -> String {
        let Some(registry) = self.terminal_registry().await else {
            return no_registry();
        };
        let key = TerminalKey::local(&params.terminal_id);
        let acting = terminal_actor(trust, actor);
        let Some(session) = registry.get_visible(&key, &acting).await else {
            return no_visible(&params.terminal_id);
        };
        if scope_is_stale(
            trust,
            session.managed_by(&acting),
            session.spawn_scope(),
            fs_scope.as_ref(),
        ) {
            return stale_scope(&params.terminal_id);
        }
        session.resize(params.cols, params.rows);
        serde_json::json!({ "ok": true, "cols": params.cols, "rows": params.rows }).to_string()
    }

    pub(crate) async fn terminal_close_tool(
        &self,
        params: TerminalCloseParams,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
    ) -> String {
        let Some(registry) = self.terminal_registry().await else {
            return no_registry();
        };
        let key = TerminalKey::local(&params.terminal_id);
        let closed = registry
            .close_visible(&key, &terminal_actor(trust, actor))
            .await;
        serde_json::json!({ "ok": closed, "closed": closed }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multibyte sequence split at the page boundary is held back for
    /// the next page instead of decaying into replacement characters on
    /// both sides (review P2); binary garbage stays lossy, and a page
    /// smaller than one sequence still makes progress.
    #[test]
    fn utf8_page_len_holds_back_incomplete_tails() {
        let s = "ab\u{00e9}".as_bytes(); // 'é' = 2 bytes
        assert_eq!(utf8_page_len(&s[..3]), 2, "split tail held back");
        assert_eq!(utf8_page_len(s), 4, "complete page delivered whole");
        assert_eq!(
            utf8_page_len(&[0xff, 0x61]),
            2,
            "invalid mid-page stays lossy"
        );
        // Invalid bytes EARLIER in the page must not forfeit the tail
        // trim: the scan resumes past them and still holds back a
        // trailing incomplete sequence (review P2, round 3).
        assert_eq!(
            utf8_page_len(&[0xff, b'a', b'b', 0xc3]),
            3,
            "split tail held back even after an invalid byte"
        );
        assert_eq!(
            utf8_page_len(&[0xff, 0xc3, 0xa9]),
            3,
            "complete char after an invalid byte delivered whole"
        );
        assert_eq!(
            utf8_page_len(&[0xff]),
            1,
            "a lone invalid byte still makes progress"
        );
        // An incomplete PREFIX is never consumed — the cursor parks, and
        // the read floor (max_bytes ≥ 4) guarantees the next page has
        // room for the whole sequence.
        assert_eq!(
            utf8_page_len(&s[2..3]),
            0,
            "incomplete prefix held, not consumed"
        );
        assert_eq!(utf8_page_len(b""), 0);
    }

    /// A caller-owned session spawned under an older filesystem scope
    /// is refused once the grant's scope changes — the OS sandbox is
    /// fixed at spawn and must not keep serving authority the grant no
    /// longer expresses (security review P1). Shared sessions another
    /// principal owns ride that owner's authority, and root surfaces
    /// are never scope-bound.
    #[test]
    fn stale_scope_refuses_only_owned_scoped_mismatches() {
        let a = FilesystemAccessPolicy {
            read_roots: vec!["/srv/a".into()],
            write_roots: Vec::new(),
        };
        let b = FilesystemAccessPolicy {
            read_roots: vec!["/srv/b".into()],
            write_roots: Vec::new(),
        };
        assert!(scope_is_stale(
            ToolCallerTrust::Scoped,
            true,
            Some(&a),
            Some(&b)
        ));
        assert!(
            scope_is_stale(ToolCallerTrust::Scoped, true, None, Some(&a)),
            "a grant gaining a scope stales the old unscoped shell"
        );
        assert!(scope_is_stale(
            ToolCallerTrust::Scoped,
            true,
            Some(&a),
            None
        ));
        assert!(!scope_is_stale(
            ToolCallerTrust::Scoped,
            true,
            Some(&a),
            Some(&a)
        ));
        assert!(!scope_is_stale(ToolCallerTrust::Scoped, true, None, None));
        assert!(
            !scope_is_stale(ToolCallerTrust::Scoped, false, Some(&a), Some(&b)),
            "another owner's shared session rides its owner's authority"
        );
        assert!(
            !scope_is_stale(ToolCallerTrust::OwnerSurface, true, Some(&a), None),
            "root surfaces are never scope-bound"
        );
    }

    /// Once the shell has exited nothing can ever complete a split
    /// sequence, so a truncated final tail is delivered lossily instead
    /// of parking the cursor forever (review P2, round 4); while it
    /// lives the tail is held for the page that completes it.
    #[test]
    fn dead_sessions_consume_split_tails() {
        assert_eq!(page_keep_len(&[b'a', 0xc3], true), 1, "live: tail held");
        assert_eq!(
            page_keep_len(&[b'a', 0xc3], false),
            2,
            "dead: tail delivered lossily"
        );
        assert_eq!(page_keep_len(&[0xc3], false), 1, "dead: bare prefix too");
    }

    #[test]
    fn scoped_actor_without_principal_owns_nothing() {
        let unattributed = crate::access::actor::ActorBinding::unattributed();
        let actor = terminal_actor(ToolCallerTrust::Scoped, &unattributed);
        match actor {
            TerminalActor::Principal(id) => assert_eq!(id, "principal:unattributed"),
            TerminalActor::Root => panic!("scoped caller must never derive Root"),
        }
    }
}
