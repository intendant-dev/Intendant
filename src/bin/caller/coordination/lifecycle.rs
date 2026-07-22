//! Declaration lifecycle glue (Track C, C1): the supervised-session
//! writer for the `sessions/` liveness lane.
//!
//! One guard per live supervised session, owned by the session's loop:
//! the loop declares at session start, heartbeats at its natural
//! boundaries (native glue: turn boundaries; wrapper glue: event-drain
//! ticks — ruled §1.5 cadence, no new timers), and the guard removes
//! the declaration on Drop (any orderly loop exit). A crash that skips
//! Drop is exactly the abandoned-declaration case the TTL sweep exists
//! for. Every operation is advisory liveness DATA for the radar —
//! declare errors are surfaced to the caller's session log and the
//! session proceeds undeclared; heartbeat trouble is swallowed (the
//! next beat retries, staleness stays honest by mtime).
//!
//! The guard is also where the writer-identity mapping lives:
//! [`writer_id_for_session`] is the single session-id → bus-writer-id
//! rule, shared by every kind so the C3 message lane cannot drift from
//! the declaration filename.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::declarations::{DeclarationInput, DeclarationSpace};
use super::{sanitize_key, scan, CoordinationError};

/// Minimum interval between heartbeat writes (§1.5): refreshes ride
/// natural boundaries, which for a busy wrapper can tick sub-second —
/// the throttle keeps the cost at one `utimes` a minute, plenty to
/// resolve the 45-minute staleness threshold.
pub(crate) const HEARTBEAT_MIN_INTERVAL_MS: u64 = 60 * 1000;

/// Intent truncation bound: §1.5 wants "one short paragraph" (advisory
/// body ≤ 8 KiB); a session goal is often a full task prompt, so the
/// glue keeps the head and marks the cut.
pub(crate) const MAX_INTENT_BYTES: usize = 1024;

/// Written when a session has no stated task text (attached externals,
/// resumed shells) — `write_own` refuses empty intent, and an honest
/// placeholder beats an invented one.
pub(crate) const FALLBACK_INTENT: &str = "(session started without a stated task)";

/// The one session-id → writer-id mapping for the bus (declaration
/// filename stem today; the C3 `messages/<writer>/` dir must reuse it).
/// `s-` marks supervised sessions apart from `guest-<ulid>` writers and
/// keeps the stem clear of the reserved `daemon` name by construction;
/// sanitizing the composed string keeps the result inside the §1.3
/// grammar (and its 64-char clamp) for any session id.
pub(crate) fn writer_id_for_session(session_id: &str) -> String {
    sanitize_key(&format!("s-{session_id}"))
}

/// Collapse a session goal into the declaration's `## intent` paragraph:
/// whitespace runs (newlines included) become single spaces — the body
/// section markers are line-anchored, so a one-line paragraph can never
/// open a `## dirty` section — then truncate at a char boundary with an
/// ellipsis. Empty input gets the honest fallback.
pub(crate) fn declaration_intent(raw: &str) -> String {
    let mut collapsed = String::with_capacity(raw.len().min(MAX_INTENT_BYTES + 8));
    let mut gap_pending = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            gap_pending = !collapsed.is_empty();
            continue;
        }
        if gap_pending {
            collapsed.push(' ');
            gap_pending = false;
        }
        collapsed.push(c);
        if collapsed.len() > MAX_INTENT_BYTES {
            break;
        }
    }
    if collapsed.is_empty() {
        return FALLBACK_INTENT.to_string();
    }
    if collapsed.len() > MAX_INTENT_BYTES {
        let mut cut = MAX_INTENT_BYTES;
        while !collapsed.is_char_boundary(cut) {
            cut -= 1;
        }
        collapsed.truncate(cut);
        collapsed.push('…');
    }
    collapsed
}

/// Everything a session loop knows at declare time. Paths and the space
/// key arrive RESOLVED (see `paths::resolve_space_dir`) — the guard
/// never reads env or a home dir, so tests inject tempdir spaces.
pub(crate) struct DeclareParams<'a> {
    pub space_dir: &'a Path,
    pub space_key: &'a str,
    /// The supervised session's id, verbatim (mapped through
    /// [`writer_id_for_session`] for the filename; recorded sanitized in
    /// the `session:` field).
    pub session_id: &'a str,
    /// One of the closed §1.5 backend set (`native`, `codex`,
    /// `claude-code`, `kimi`, `pi`).
    pub backend: &'a str,
    /// The session's checkout root; recorded as `root:` when it passes
    /// the absolute-path grammar, silently omitted otherwise (optional
    /// hint fields never sink the whole declaration).
    pub project_root: &'a Path,
    /// Checked-out branch when known; same optional-hint treatment.
    pub branch: Option<String>,
    /// Raw session goal/title; normalized via [`declaration_intent`].
    pub intent: &'a str,
}

/// RAII holder for one session's own declaration. Heartbeats take
/// `&self` (interior-mutable throttle clock) so shared drain contexts
/// can tick without a `&mut` thread through their call stacks.
pub(crate) struct SessionDeclarationGuard {
    space: DeclarationSpace,
    id: String,
    session_id: String,
    backend: String,
    root: Option<String>,
    branch: Option<String>,
    intent: String,
    last_beat_ms: AtomicU64,
}

impl SessionDeclarationGuard {
    /// Open the space's `sessions/` store and write this session's
    /// declaration. Errors are the caller's to log — the session runs
    /// undeclared rather than failing over bus trouble.
    pub(crate) fn declare(
        params: DeclareParams<'_>,
        now_ms: u64,
    ) -> Result<Self, CoordinationError> {
        let root_str = params.project_root.to_string_lossy();
        let guard = SessionDeclarationGuard {
            space: DeclarationSpace::open(params.space_dir, params.space_key)?,
            id: writer_id_for_session(params.session_id),
            session_id: params.session_id.to_string(),
            backend: params.backend.to_string(),
            root: scan::valid_abs_path(&root_str).then(|| root_str.into_owned()),
            branch: params
                .branch
                .filter(|b| super::declarations::valid_branch(b)),
            intent: declaration_intent(params.intent),
            last_beat_ms: AtomicU64::new(now_ms),
        };
        guard.space.write_own(&guard.input())?;
        Ok(guard)
    }

    /// Piggybacked liveness beat: an mtime touch, at most once per
    /// [`HEARTBEAT_MIN_INTERVAL_MS`]. A declaration that vanished under
    /// the guard (GC'd past TTL, tidied by hand) is re-declared — the
    /// session is demonstrably still live. Failures are swallowed: the
    /// beat window is claimed first, so trouble retries a minute later
    /// instead of on every tick.
    pub(crate) fn heartbeat(&self, now_ms: u64) {
        let last = self.last_beat_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < HEARTBEAT_MIN_INTERVAL_MS {
            return;
        }
        self.last_beat_ms.store(now_ms, Ordering::Relaxed);
        match self.space.touch_own(&self.id) {
            Ok(true) => {}
            Ok(false) => {
                let _ = self.space.write_own(&self.input());
            }
            Err(_) => {}
        }
    }

    /// [`Self::heartbeat`] on the process clock (the loop edges' form).
    pub(crate) fn heartbeat_now(&self) {
        self.heartbeat(super::now_ms());
    }

    /// The parsed view of what this guard last wrote (re-declare input).
    fn input(&self) -> DeclarationInput<'_> {
        DeclarationInput {
            id: &self.id,
            session: Some(&self.session_id),
            backend: Some(&self.backend),
            root: self.root.as_deref(),
            branch: self.branch.as_deref(),
            intent: &self.intent,
            dirty: &[],
        }
    }

    /// Read back the own declaration (tests + diagnostics).
    #[cfg(test)]
    pub(crate) fn read_back(&self) -> Option<super::declarations::SessionDeclaration> {
        self.space.read_own(&self.id).ok().flatten()
    }
}

impl Drop for SessionDeclarationGuard {
    fn drop(&mut self) {
        // Clean-end removal (§1.5). Best-effort: a failure here leaves
        // an abandoned declaration for the TTL sweep, never a panic in
        // an unwinding session.
        let _ = self.space.remove_own(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params<'a>(space_dir: &'a Path, intent: &'a str) -> DeclareParams<'a> {
        DeclareParams {
            space_dir,
            space_key: "test-space",
            session_id: "Sess_01ABC",
            backend: "native",
            project_root: Path::new("/tmp/proj"),
            branch: Some("feat/x".to_string()),
            intent,
        }
    }

    #[test]
    fn writer_id_mapping_is_grammar_safe_and_bounded() {
        assert_eq!(writer_id_for_session("abc"), "s-abc");
        assert_eq!(writer_id_for_session("Sess_01ABC"), "s-sess-01abc");
        let long = writer_id_for_session(&"x".repeat(200));
        assert!(long.len() <= 64);
        assert_eq!(sanitize_key(&long), long, "idempotent under the grammar");
        // The supervised prefix keeps the stem off the reserved name.
        assert_ne!(writer_id_for_session("daemon"), "daemon");
    }

    #[test]
    fn intent_normalization_collapses_truncates_and_falls_back() {
        assert_eq!(declaration_intent("  fix\nthe\tbus  "), "fix the bus");
        assert_eq!(declaration_intent("   \n\t  "), FALLBACK_INTENT);
        // A multi-line goal can never smuggle a line-anchored section
        // marker into the body.
        assert!(!declaration_intent("x\n## dirty\n- evil").contains('\n'));
        let long = declaration_intent(&"é".repeat(2 * MAX_INTENT_BYTES));
        assert!(long.len() <= MAX_INTENT_BYTES + '…'.len_utf8());
        assert!(long.ends_with('…'), "cut is marked");
    }

    #[test]
    fn declare_heartbeat_drop_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let space = tmp.path().join("space");
        let now = super::super::now_ms();
        let guard =
            SessionDeclarationGuard::declare(params(&space, "carve the glue"), now).unwrap();
        let file = tmp.path().join("space/sessions/s-sess-01abc.md");
        assert!(file.is_file());

        let decl = guard.read_back().expect("own declaration parses");
        assert_eq!(decl.backend.as_deref(), Some("native"));
        assert_eq!(decl.session.as_deref(), Some("sess-01abc"));
        assert_eq!(decl.root.as_deref(), Some("/tmp/proj"));
        assert_eq!(decl.branch.as_deref(), Some("feat/x"));
        assert_eq!(decl.intent, "carve the glue");
        assert!(
            decl.dirty.is_empty(),
            "glue declares no dirty paths (C2 radar unions git status)"
        );

        // Inside the throttle window: no write happens (removing the
        // file makes any write observable).
        std::fs::remove_file(&file).unwrap();
        guard.heartbeat(now + HEARTBEAT_MIN_INTERVAL_MS - 1);
        assert!(!file.exists(), "throttled beat writes nothing");

        // Past the window: the vanished declaration is re-declared.
        guard.heartbeat(now + HEARTBEAT_MIN_INTERVAL_MS + 1);
        assert!(file.is_file(), "vanished declaration re-declared");

        drop(guard);
        assert!(!file.exists(), "clean end removes the declaration");
    }

    #[test]
    fn invalid_hint_fields_are_omitted_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let space = tmp.path().join("space");
        let mut p = params(&space, "x");
        p.project_root = Path::new("relative/not/abs");
        p.branch = Some("-rf".to_string());
        let guard = SessionDeclarationGuard::declare(p, super::super::now_ms()).unwrap();
        let decl = guard.read_back().unwrap();
        assert_eq!(decl.root, None, "non-grammar root omitted");
        assert_eq!(decl.branch, None, "non-grammar branch omitted");
    }

    #[test]
    fn declare_refuses_only_on_real_write_trouble() {
        // A session id that sanitizes to a usable stem never fails; the
        // error path is the store's own (e.g. an unwritable space dir).
        let tmp = tempfile::tempdir().unwrap();
        let file_in_the_way = tmp.path().join("blocked");
        std::fs::write(&file_in_the_way, "x").unwrap();
        let p = DeclareParams {
            space_dir: &file_in_the_way,
            space_key: "blocked",
            session_id: "s1",
            backend: "native",
            project_root: Path::new("/p"),
            branch: None,
            intent: "x",
        };
        assert!(SessionDeclarationGuard::declare(p, 0).is_err());
    }
}
