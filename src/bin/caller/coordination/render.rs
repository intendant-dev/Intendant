//! Coordination-block renderer (Track C, C2; ruled §2.2–§2.5, §2.7).
//!
//! A pure function turns one space's radar snapshot into the ONE
//! bounded `[System] coordination v1` block a session may receive per
//! turn. Everything here is binding, ruled behavior:
//!
//! - **§2.2 schema, exactly**: line-oriented, one record per line —
//!   header, `sessions:` presence, `! overlap` ALERT lines,
//!   `messages:` existence-only (writer + ids, NEVER text),
//!   `invalid:`, `[truncated]`. Free text cannot appear.
//! - **§2.3 string safety**: every token passes its per-type validator
//!   (path grammar with the ≤120-char DISPLAY bound, middle-ellipsized
//!   — the parse grammar's 512 stays in `scan.rs`, C1 erratum 4;
//!   sanitize_key-idempotent ids shown as 8-char prefixes; closed
//!   backend enum; decimal counts). Failures COUNT into `invalid:` and
//!   never render. The `messages:` writer and ids render whole — §2.2
//!   makes them the retrieval coordinates for
//!   `$INTENDANT_COORDINATION_DIR/messages/<writer>/<id>.md`, so a
//!   truncated prefix would break the documented lazy read.
//! - **§2.4 caps**: rendered block ≤ 1536 bytes HARD (the loop
//!   truncates nothing downstream — the renderer is the only wall,
//!   R1); ≤ 3 session entries listed (counts stay exact); ≤ 8 overlap
//!   lines, ALERT lines kept first under byte pressure; ≤ 4 message
//!   lines; `[truncated]` marks every drop.
//! - **§2.5 dedup**: the caller keeps (hash, injected-at) per session;
//!   an identical render inside the 30-minute floor returns `None`; a
//!   NEW alert changes the hash and bypasses naturally.
//! - **§2.7 presence**: `None` unless at least one non-header line
//!   exists; the own session is excluded from its own radar lines (a
//!   session is not in conflict with itself — messages TO it still
//!   show).
//!
//! Zero-LLM and deterministic: identical inputs render byte-identical
//! blocks; every collection consumed is pre-sorted by the radar.
//!
//! The second product here is the **external ALERT steer line** (§2.8):
//! [`render_alert_steers`] renders one schema-rendered single line per
//! distinct overlap set per session pair (or per open PR) for the
//! supervised-external delivery lane. Ambient content — presence,
//! messages, invalid counts — has no path into that lane by
//! construction: the function consumes the snapshot's overlap
//! collections only (R8: ambient is never pushed at externals).

use std::collections::BTreeMap;

use super::radar::SpaceRadarSnapshot;
use super::scan;

/// §2.4 (R1): the rendered block's HARD byte cap.
pub(crate) const RENDERED_BLOCK_MAX_BYTES: usize = 1536;
/// §2.4: session entries listed on the presence line (counts exact).
pub(crate) const MAX_SESSIONS_LISTED: usize = 3;
/// §2.4: overlap lines rendered, ALERT first.
pub(crate) const MAX_OVERLAP_LINES: usize = 8;
/// §2.4: message lines rendered (one per writer).
pub(crate) const MAX_MESSAGE_LINES: usize = 4;
/// Ids listed per message line (the §2.2 example's shape; the line's
/// count stays exact).
pub(crate) const MAX_MESSAGE_IDS_LISTED: usize = 2;
/// §2.5: a never-changing block re-injects at most once per floor.
pub(crate) const REINJECT_FLOOR_MS: u64 = 30 * 60 * 1000;
/// §2.3: the path DISPLAY bound (middle-ellipsized; erratum 4).
pub(crate) const PATH_DISPLAY_MAX_CHARS: usize = 120;

/// One rendered block: the exact bytes to inject, the dedup hash the
/// caller stores, and whether any ALERT line rendered (the external
/// lane is ALERT-only, §2.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedBlock {
    pub text: String,
    pub hash: u64,
    pub has_alert: bool,
}

/// Render one session's coordination block from its space snapshot.
///
/// `own_writer_id` is the session's bus writer id
/// (`lifecycle::writer_id_for_session`); `last_hash`/`last_injected_ms`
/// are the caller's per-session dedup state from the previous `Some`
/// (pass `None`/`0` when nothing was ever injected). Returns `None`
/// when there is nothing to say (§2.7), or when the identical block
/// was already injected inside the §2.5 floor.
pub(crate) fn render_block(
    snapshot: &SpaceRadarSnapshot,
    own_writer_id: &str,
    last_hash: Option<u64>,
    last_injected_ms: u64,
    now_ms: u64,
) -> Option<RenderedBlock> {
    let candidates = build_candidates(snapshot, own_writer_id)?;
    let (text, has_alert) = assemble(&candidates)?;
    debug_assert!(text.len() <= RENDERED_BLOCK_MAX_BYTES);
    let hash = super::fnv1a_64(text.as_bytes());
    if last_hash == Some(hash) && now_ms.saturating_sub(last_injected_ms) < REINJECT_FLOOR_MS {
        return None; // §2.5: identical render, floor not yet reached
    }
    Some(RenderedBlock {
        text,
        hash,
        has_alert,
    })
}

/// Space-key labels as rendered in the header: the derived-key /
/// override-label grammar (`paths::resolve_space_dir`'s rule).
fn valid_space_key_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 96
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// §2.3 id/writer validator: sanitize_key-idempotent, ≤ 64.
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && super::sanitize_key(s) == s
}

/// §2.3 message-id validator. The ruled grammar says `[a-z0-9]{10,32}`
/// for ULIDs; C1 landed message ids as `m-<ulid>` (and radar notes are
/// `rn-<hash>`), so the rendered token is the retrievable filename
/// stem under the same §1.3 grammar, length-bounded 10..=32 — lex
/// posterior, reported as an erratum candidate.
fn valid_message_id(s: &str) -> bool {
    (10..=32).contains(&s.len()) && super::sanitize_key(s) == s
}

/// The `<id8>` display prefix (§2.3). Ids are grammar-checked ASCII,
/// so byte slicing is char-safe.
fn id8(s: &str) -> &str {
    &s[..s.len().min(8)]
}

/// §2.3 path display: grammar-valid, ≤ 120 chars middle-ellipsized.
fn display_path(p: &str) -> Option<String> {
    if !scan::valid_rel_path(p) {
        return None;
    }
    if p.len() <= PATH_DISPLAY_MAX_CHARS {
        return Some(p.to_string());
    }
    // Grammar paths are pure ASCII: byte offsets are char boundaries.
    let head = &p[..60];
    let tail = &p[p.len() - 59..];
    Some(format!("{head}…{tail}"))
}

/// The candidate lines for one session's block, per-kind caps applied,
/// every token validated. `None` only when the space key itself is
/// outside its grammar (nothing safe to say at all).
struct Candidates {
    header: String,
    sessions: Option<String>,
    overlaps: Vec<String>,
    messages: Vec<String>,
    invalid_line: Option<String>,
    /// Lines dropped by the per-kind count caps (before byte pressure).
    cap_dropped: bool,
}

fn build_candidates(snapshot: &SpaceRadarSnapshot, own_writer_id: &str) -> Option<Candidates> {
    if !valid_space_key_label(&snapshot.space_key) {
        return None;
    }
    let header = format!("[System] coordination v1 space={}", snapshot.space_key);
    let mut invalid: u64 = snapshot.invalid;

    // Presence: everyone but the own session (§2.7 — a space holding
    // only you says nothing). Actives list before stales; counts are
    // exact, the listing caps at MAX_SESSIONS_LISTED.
    let mut active: u64 = 0;
    let mut stale: u64 = 0;
    let mut listed: Vec<String> = Vec::new();
    for list_stale in [false, true] {
        for s in &snapshot.sessions {
            if s.stale != list_stale || s.writer_id == own_writer_id {
                continue;
            }
            if !valid_id(&s.writer_id) {
                invalid += 1;
                continue;
            }
            let backend = match s.backend.as_deref() {
                // A declaration without a backend field is an
                // unsupervised-grade writer: the closed enum's floor.
                None => "guest",
                Some(b) if scan::valid_backend(b) => b,
                Some(_) => {
                    invalid += 1;
                    continue;
                }
            };
            if s.stale {
                stale += 1;
            } else {
                active += 1;
            }
            if listed.len() < MAX_SESSIONS_LISTED {
                listed.push(format!("{}({backend})", id8(&s.writer_id)));
            }
        }
    }
    let sessions = (active + stale > 0).then(|| {
        format!(
            "sessions: {active} active, {stale} stale — {}",
            listed.join(", ")
        )
    });

    // Overlap ALERT lines involving this session (§2.7 own-exclusion:
    // the own id never renders; the counterparty does).
    let mut overlaps: Vec<String> = Vec::new();
    for o in &snapshot.pair_overlaps {
        let other = if o.a == own_writer_id {
            &o.b
        } else if o.b == own_writer_id {
            &o.a
        } else {
            continue; // someone else's collision — not this block's line
        };
        if !valid_id(other) {
            invalid += 1;
            continue;
        }
        let Some(path) = display_path(&o.path) else {
            invalid += 1;
            continue;
        };
        let sources = match (o.declared, o.git) {
            (true, true) => "declared+git",
            (true, false) => "declared",
            (false, true) => "git",
            (false, false) => {
                invalid += 1;
                continue;
            }
        };
        overlaps.push(format!(
            "! overlap {path} — with {} ({sources})",
            id8(other)
        ));
    }
    for o in &snapshot.pr_overlaps {
        if o.writer != own_writer_id {
            continue;
        }
        let Some(path) = display_path(&o.path) else {
            invalid += 1;
            continue;
        };
        overlaps.push(format!("! overlap {path} — pr#{}", o.pr));
    }
    let overlap_cap_dropped = overlaps.len() > MAX_OVERLAP_LINES;
    overlaps.truncate(MAX_OVERLAP_LINES);

    // Messages addressed to this session (or space-wide), grouped per
    // writer — existence and provenance only, never text (§2.2/§9).
    let mut per_writer: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in &snapshot.messages {
        if m.writer == own_writer_id {
            continue; // own outbox is not news
        }
        match m.to.as_deref() {
            None => {}
            Some(to) if to == own_writer_id => {}
            Some(_) => continue, // someone else's mail
        }
        if !valid_id(&m.writer) || !valid_message_id(&m.id) {
            invalid += 1;
            continue;
        }
        per_writer.entry(m.writer.as_str()).or_default().push(&m.id);
    }
    let mut messages: Vec<String> = Vec::new();
    let mut message_cap_dropped = false;
    for (writer, ids) in &per_writer {
        if messages.len() >= MAX_MESSAGE_LINES {
            message_cap_dropped = true;
            break;
        }
        let shown = ids
            .iter()
            .take(MAX_MESSAGE_IDS_LISTED)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        messages.push(format!(
            "messages: {} unread — from {writer}: {shown}",
            ids.len()
        ));
    }

    let invalid_line = (invalid > 0).then(|| format!("invalid: {invalid} entries ignored"));
    Some(Candidates {
        header,
        sessions,
        overlaps,
        messages,
        invalid_line,
        cap_dropped: overlap_cap_dropped || message_cap_dropped,
    })
}

/// Assemble the candidates into the final block under the §2.4 hard
/// byte cap. Under byte pressure the keep-priority is ALERT overlap
/// lines first, then presence, then messages, then the invalid count —
/// output stays in schema order regardless. `None` when no non-header
/// line exists (§2.7).
fn assemble(c: &Candidates) -> Option<(String, bool)> {
    let non_header = usize::from(c.sessions.is_some())
        + c.overlaps.len()
        + c.messages.len()
        + usize::from(c.invalid_line.is_some());
    if non_header == 0 {
        return None;
    }

    // The straightforward assembly first: everything the count caps
    // admitted, plus the marker when those caps dropped lines.
    let mut full = c.header.clone();
    if let Some(s) = &c.sessions {
        full.push('\n');
        full.push_str(s);
    }
    for line in &c.overlaps {
        full.push('\n');
        full.push_str(line);
    }
    for line in &c.messages {
        full.push('\n');
        full.push_str(line);
    }
    if let Some(line) = &c.invalid_line {
        full.push('\n');
        full.push_str(line);
    }
    if c.cap_dropped {
        full.push_str("\n[truncated]");
    }
    if full.len() <= RENDERED_BLOCK_MAX_BYTES {
        return Some((full, !c.overlaps.is_empty()));
    }

    // Over budget: reserve the marker, then admit lines by priority.
    const MARKER: &str = "\n[truncated]";
    let mut budget = RENDERED_BLOCK_MAX_BYTES.saturating_sub(c.header.len() + MARKER.len());
    let admit = |line: &str, budget: &mut usize| -> bool {
        let cost = 1 + line.len();
        if cost <= *budget {
            *budget -= cost;
            true
        } else {
            false
        }
    };
    let mut take_overlaps = 0usize;
    for line in &c.overlaps {
        if admit(line, &mut budget) {
            take_overlaps += 1;
        } else {
            break;
        }
    }
    let take_sessions = c.sessions.as_deref().is_some_and(|s| admit(s, &mut budget));
    let mut take_messages = 0usize;
    for line in &c.messages {
        if admit(line, &mut budget) {
            take_messages += 1;
        } else {
            break;
        }
    }
    let take_invalid = c
        .invalid_line
        .as_deref()
        .is_some_and(|s| admit(s, &mut budget));
    if take_overlaps == 0 && !take_sessions && take_messages == 0 && !take_invalid {
        return None; // nothing fit — unreachable with bounded lines, but never emit a bare header
    }

    let mut text = c.header.clone();
    if take_sessions {
        text.push('\n');
        text.push_str(c.sessions.as_deref().unwrap_or_default());
    }
    for line in &c.overlaps[..take_overlaps] {
        text.push('\n');
        text.push_str(line);
    }
    for line in &c.messages[..take_messages] {
        text.push('\n');
        text.push_str(line);
    }
    if take_invalid {
        text.push('\n');
        text.push_str(c.invalid_line.as_deref().unwrap_or_default());
    }
    text.push_str(MARKER);
    Some((text, take_overlaps > 0))
}

/// §2.8: byte bound for one external ALERT steer line (self-enforced —
/// nothing downstream truncates steer text either).
pub(crate) const STEER_LINE_MAX_BYTES: usize = 512;
/// Overlap paths listed on one steer line; the remainder renders as the
/// fixed `(+<n> more)` template token.
pub(crate) const STEER_MAX_PATHS_LISTED: usize = 4;

/// One external ALERT steer (§2.8): a schema-rendered single line for
/// one distinct overlap set, plus the set's canonical hash — the
/// identity the daemon-side [`super::radar::ExternalSteerLedger`] keys
/// its one-steer-per-set dedup on. The hash covers the counterparty and
/// the sorted path set, deliberately NOT the evidence sources, so a
/// declared→declared+git shift alone does not remint a steer (the
/// radar-note lane's canon, `messages::write_radar_note`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlertSteer {
    pub text: String,
    pub set_hash: u64,
}

/// Render the §2.8 external ALERT steer lines for one session: one per
/// counterparty session pair (paths grouped into the pair's set) and
/// one per open PR. Every token passes its §2.3 validator; invalid
/// tokens are skipped (a set with no valid path renders nothing — the
/// external lane has no `invalid:` channel, and the native block
/// already surfaces the counts). Empty when the session has no ALERT
/// overlaps — ambient snapshot content cannot reach this lane because
/// nothing here reads it.
pub(crate) fn render_alert_steers(
    snapshot: &SpaceRadarSnapshot,
    own_writer_id: &str,
) -> Vec<AlertSteer> {
    if !valid_space_key_label(&snapshot.space_key) {
        return Vec::new(); // nothing safe to say at all (§2.3)
    }
    let mut steers = Vec::new();

    // Pair sets: group this session's pair overlaps per counterparty.
    #[derive(Default)]
    struct PairSet<'a> {
        paths: std::collections::BTreeSet<&'a str>,
        declared: bool,
        git: bool,
    }
    let mut pairs: BTreeMap<&str, PairSet<'_>> = BTreeMap::new();
    for o in &snapshot.pair_overlaps {
        let other = if o.a == own_writer_id {
            &o.b
        } else if o.b == own_writer_id {
            &o.a
        } else {
            continue; // someone else's collision (§2.7 own-scope)
        };
        if !valid_id(other) || !scan::valid_rel_path(&o.path) {
            continue;
        }
        let entry = pairs.entry(other.as_str()).or_default();
        entry.paths.insert(o.path.as_str());
        entry.declared |= o.declared;
        entry.git |= o.git;
    }
    for (other, set) in &pairs {
        let sources = match (set.declared, set.git) {
            (true, true) => "declared+git",
            (true, false) => "declared",
            (false, true) => "git",
            (false, false) => continue, // sourceless set: nothing to claim
        };
        let tail = format!(" — with {} ({sources})", id8(other));
        if let Some(text) = assemble_steer_line(&snapshot.space_key, &set.paths, &tail) {
            steers.push(AlertSteer {
                text,
                set_hash: steer_set_hash(&format!("pair:{other}"), &set.paths),
            });
        }
    }

    // PR sets: group this session's PR overlaps per PR number.
    let mut prs: BTreeMap<u32, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for o in &snapshot.pr_overlaps {
        if o.writer != own_writer_id || !scan::valid_rel_path(&o.path) {
            continue;
        }
        prs.entry(o.pr).or_default().insert(o.path.as_str());
    }
    for (pr, paths) in &prs {
        let tail = format!(" — pr#{pr}");
        if let Some(text) = assemble_steer_line(&snapshot.space_key, paths, &tail) {
            steers.push(AlertSteer {
                text,
                set_hash: steer_set_hash(&format!("pr:{pr}"), paths),
            });
        }
    }
    steers
}

/// Canonical overlap-set identity for the ledger: counterparty (or PR)
/// plus the sorted path set; sources excluded (see [`AlertSteer`]).
fn steer_set_hash(counterparty: &str, paths: &std::collections::BTreeSet<&str>) -> u64 {
    let canonical = format!(
        "steer:{counterparty};paths={}",
        paths.iter().copied().collect::<Vec<_>>().join(",")
    );
    super::fnv1a_64(canonical.as_bytes())
}

/// Assemble one steer line under [`STEER_LINE_MAX_BYTES`]: the §2.2
/// header vocabulary + the ALERT marker + up to
/// [`STEER_MAX_PATHS_LISTED`] display-bounded paths + the fixed
/// `(+<n> more)` count for the rest + the pair/PR tail. Paths drop from
/// the back (into the count) under byte pressure; `None` only when no
/// path in the set survives its validator.
fn assemble_steer_line(
    space_key: &str,
    paths: &std::collections::BTreeSet<&str>,
    tail: &str,
) -> Option<String> {
    let displayed: Vec<String> = paths.iter().filter_map(|p| display_path(p)).collect();
    if displayed.is_empty() {
        return None;
    }
    let total = displayed.len();
    let mut listed = displayed.len().min(STEER_MAX_PATHS_LISTED);
    loop {
        let mut line = format!(
            "[System] coordination v1 space={space_key} ! overlap {}",
            displayed[..listed].join(", ")
        );
        if total > listed {
            line.push_str(&format!(" (+{} more)", total - listed));
        }
        line.push_str(tail);
        if line.len() <= STEER_LINE_MAX_BYTES {
            debug_assert!(!line.contains('\n'), "steer lines are single-line");
            return Some(line);
        }
        if listed == 1 {
            // A single display-bounded path always fits: header ≤ ~130
            // bytes, path ≤ 120 chars, tail ≤ ~40. Defensive fallback
            // rather than an unreachable!: drop the line, never emit an
            // over-bound steer.
            return None;
        }
        listed -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::radar::{
        compute_space_snapshot, ObservedSet, PrFileSet, RadarMessageMeta, RadarPairOverlap,
        RadarPrOverlap, RadarSessionPresence, RadarSpaceInputs,
    };

    const SPACE: &str = "test-space-0123456789abcdef";
    const OWN: &str = "s-me";

    fn base_snapshot() -> SpaceRadarSnapshot {
        SpaceRadarSnapshot {
            space_key: SPACE.to_string(),
            computed_ms: 0,
            sessions: Vec::new(),
            pair_overlaps: Vec::new(),
            pr_overlaps: Vec::new(),
            messages: Vec::new(),
            invalid: 0,
        }
    }

    fn presence(id: &str, backend: Option<&str>, stale: bool) -> RadarSessionPresence {
        RadarSessionPresence {
            writer_id: id.to_string(),
            backend: backend.map(str::to_string),
            stale,
        }
    }

    fn pair(path: &str, a: &str, b: &str, declared: bool, git: bool) -> RadarPairOverlap {
        RadarPairOverlap {
            path: path.to_string(),
            a: a.to_string(),
            b: b.to_string(),
            declared,
            git,
        }
    }

    fn message(writer: &str, id: &str, to: Option<&str>) -> RadarMessageMeta {
        RadarMessageMeta {
            writer: writer.to_string(),
            id: id.to_string(),
            to: to.map(str::to_string),
        }
    }

    fn render(snapshot: &SpaceRadarSnapshot) -> Option<RenderedBlock> {
        render_block(snapshot, OWN, None, 0, 0)
    }

    /// Every byte of a rendered block stays inside the schema: ASCII
    /// text plus the schema's em-dash and the ellipsis, line shapes
    /// from the §2.2 grammar only.
    fn assert_schema_clean(text: &str) {
        for ch in text.chars() {
            assert!(
                ch == '\n' || ch == '—' || ch == '…' || ch == ' ' || ch.is_ascii_graphic(),
                "byte outside the schema grammar: {ch:?} in {text:?}"
            );
        }
        for (i, line) in text.lines().enumerate() {
            if i == 0 {
                let key = line
                    .strip_prefix("[System] coordination v1 space=")
                    .unwrap_or_else(|| panic!("bad header: {line:?}"));
                assert!(super::valid_space_key_label(key), "{key:?}");
                continue;
            }
            assert!(
                line == "[truncated]"
                    || line.starts_with("sessions: ")
                    || line.starts_with("! overlap ")
                    || line.starts_with("messages: ")
                    || line.starts_with("invalid: "),
                "line outside the schema: {line:?}"
            );
        }
    }

    #[test]
    fn solo_space_renders_nothing() {
        let mut s = base_snapshot();
        s.sessions.push(presence(OWN, Some("native"), false));
        assert!(render(&s).is_none(), "§2.7: one session, nothing to say");
        assert!(render(&base_snapshot()).is_none(), "empty space");
    }

    #[test]
    fn presence_line_excludes_own_and_keeps_counts_exact() {
        let mut s = base_snapshot();
        s.sessions.push(presence(OWN, Some("native"), false));
        for i in 0..5 {
            s.sessions
                .push(presence(&format!("s-other-{i}"), Some("codex"), false));
        }
        s.sessions.push(presence("s-tired", None, true));
        let block = render(&s).expect("others exist");
        assert_schema_clean(&block.text);
        assert!(!block.has_alert);
        let sessions_line = block
            .text
            .lines()
            .find(|l| l.starts_with("sessions: "))
            .unwrap();
        assert!(
            sessions_line.starts_with("sessions: 5 active, 1 stale — "),
            "{sessions_line}"
        );
        // ≤3 listed, actives first; the backendless stale peer would
        // render as guest but the cap keeps it off the list.
        assert_eq!(sessions_line.matches("s-other-").count(), 3);
        assert!(!block.text.contains("s-me"), "own session never renders");
    }

    #[test]
    fn absent_backend_renders_as_guest() {
        let mut s = base_snapshot();
        s.sessions.push(presence("s-anon", None, false));
        let block = render(&s).unwrap();
        assert!(block.text.contains("s-anon(guest)"), "{}", block.text);
    }

    #[test]
    fn overlap_lines_follow_the_schema_and_exclude_foreign_pairs() {
        let mut s = base_snapshot();
        s.pair_overlaps
            .push(pair("src/hot.rs", OWN, "s-other", true, true));
        s.pair_overlaps
            .push(pair("src/mine2.rs", "s-aaa", OWN, false, true));
        s.pair_overlaps
            .push(pair("src/foreign.rs", "s-xx", "s-yy", true, false));
        s.pr_overlaps.push(RadarPrOverlap {
            path: "docs/mine.md".to_string(),
            writer: OWN.to_string(),
            pr: 566,
        });
        s.pr_overlaps.push(RadarPrOverlap {
            path: "docs/theirs.md".to_string(),
            writer: "s-other".to_string(),
            pr: 9,
        });
        let block = render(&s).unwrap();
        assert_schema_clean(&block.text);
        assert!(block.has_alert);
        let lines: Vec<&str> = block.text.lines().collect();
        assert_eq!(
            lines[1], "! overlap src/hot.rs — with s-other (declared+git)",
            "{lines:?}"
        );
        assert_eq!(lines[2], "! overlap src/mine2.rs — with s-aaa (git)");
        assert_eq!(lines[3], "! overlap docs/mine.md — pr#566");
        assert!(
            !block.text.contains("foreign") && !block.text.contains("theirs"),
            "other sessions' collisions are not this block's lines: {}",
            block.text
        );
    }

    #[test]
    fn messages_show_recipient_scoped_existence_only() {
        let mut s = base_snapshot();
        s.messages.push(message("s-peer", "m-0123456789ab", None));
        s.messages
            .push(message("s-peer", "m-0123456789ac", Some(OWN)));
        s.messages
            .push(message("s-peer", "m-0123456789ad", Some("s-third")));
        s.messages.push(message(OWN, "m-0123456789ae", None));
        s.messages
            .push(message("daemon", "rn-0011223344556677", Some(OWN)));
        let block = render(&s).unwrap();
        assert_schema_clean(&block.text);
        let lines: Vec<&str> = block.text.lines().collect();
        assert_eq!(
            lines[1], "messages: 1 unread — from daemon: rn-0011223344556677",
            "{lines:?}"
        );
        assert_eq!(
            lines[2],
            "messages: 2 unread — from s-peer: m-0123456789ab, m-0123456789ac"
        );
        assert!(
            !block.text.contains("m-0123456789ad") && !block.text.contains("m-0123456789ae"),
            "third-party and own mail never render"
        );
    }

    #[test]
    fn caps_bound_every_line_family_with_truncated_marker() {
        let mut s = base_snapshot();
        for i in 0..12 {
            s.pair_overlaps.push(pair(
                &format!("src/f{i:02}.rs"),
                OWN,
                "s-other",
                true,
                false,
            ));
        }
        for w in 0..6 {
            for k in 0..3 {
                s.messages.push(message(
                    &format!("s-writer-{w}"),
                    &format!("m-{w:06}00{k:04}"),
                    None,
                ));
            }
        }
        let block = render(&s).unwrap();
        assert_schema_clean(&block.text);
        let overlap_lines = block
            .text
            .lines()
            .filter(|l| l.starts_with("! overlap"))
            .count();
        assert_eq!(overlap_lines, MAX_OVERLAP_LINES);
        let message_lines: Vec<&str> = block
            .text
            .lines()
            .filter(|l| l.starts_with("messages: "))
            .collect();
        assert_eq!(message_lines.len(), MAX_MESSAGE_LINES);
        // Counts exact, ids listed capped.
        assert!(message_lines[0].starts_with("messages: 3 unread — "));
        assert_eq!(
            message_lines[0].matches("m-").count(),
            MAX_MESSAGE_IDS_LISTED
        );
        assert!(block.text.ends_with("\n[truncated]"), "{}", block.text);
        assert!(block.text.len() <= RENDERED_BLOCK_MAX_BYTES);
    }

    #[test]
    fn long_paths_are_middle_ellipsized_to_the_display_bound() {
        let mut s = base_snapshot();
        let long = format!("src/{}/deep.rs", "x".repeat(200));
        s.pair_overlaps
            .push(pair(&long, OWN, "s-other", true, false));
        let block = render(&s).unwrap();
        assert_schema_clean(&block.text);
        let line = block.text.lines().nth(1).unwrap();
        let path = line
            .strip_prefix("! overlap ")
            .unwrap()
            .split(" — ")
            .next()
            .unwrap();
        assert_eq!(path.chars().count(), PATH_DISPLAY_MAX_CHARS);
        assert!(path.contains('…'));
        assert!(path.starts_with("src/xxx") && path.ends_with("deep.rs"));
    }

    /// RULED binding (R6): adversarial strings on the rendered block —
    /// spaces, ANSI, newlines, RTL overrides, 4 KB names — become
    /// `invalid:` counts; not one hostile byte reaches the block.
    #[test]
    fn adversarial_snapshot_fields_render_as_counts_only() {
        let mut s = base_snapshot();
        s.sessions
            .push(presence("Bad Writer", Some("native"), false));
        s.sessions
            .push(presence("s-mystery", Some("botnet"), false));
        s.pair_overlaps
            .push(pair("evil path/with space.rs", OWN, "s-other", true, false));
        s.pair_overlaps
            .push(pair("ansi\u{1b}[31mred.rs", OWN, "s-other", true, false));
        s.pair_overlaps
            .push(pair("multi\nline.rs", OWN, "s-other", false, true));
        s.pair_overlaps
            .push(pair("rtl\u{202e}gnp.rs", OWN, "s-other", true, true));
        s.pair_overlaps
            .push(pair(&"a".repeat(4096), OWN, "s-other", true, false));
        s.pair_overlaps
            .push(pair("src/fine.rs", OWN, "UPPER CASE ID", true, false));
        s.pair_overlaps
            .push(pair("src/none.rs", OWN, "s-other", false, false));
        s.pr_overlaps.push(RadarPrOverlap {
            path: "-leading-dash.rs".to_string(),
            writer: OWN.to_string(),
            pr: 1,
        });
        s.messages
            .push(message("s-peer", "SHOUTING-NOT-AN-ID", Some(OWN)));
        s.messages
            .push(message("bad writer", "m-0123456789ab", None));
        s.messages.push(message("s-peer", "x", None)); // too short
        let block = render(&s).expect("the invalid count is a line");
        assert_schema_clean(&block.text);
        for hostile in [
            "evil path",
            "with space",
            "\u{1b}",
            "multi\nline",
            "\u{202e}",
            "aaaa",
            "UPPER",
            "SHOUTING",
            "bad writer",
            "botnet",
            "s-mystery",
            "-leading-dash",
        ] {
            assert!(
                !block.text.contains(hostile),
                "hostile token {hostile:?} leaked into {:?}",
                block.text
            );
        }
        assert!(!block.has_alert, "nothing valid to alert on");
        // 12 hostile tokens counted: 2 sessions + 7 overlap drops + 1
        // pr path + 3 message drops... the empty-sources line is a
        // count too. Pin the exact line.
        assert_eq!(
            block.text.lines().nth(1).unwrap(),
            "invalid: 13 entries ignored"
        );
    }

    /// RULED binding (R6): the same adversarial classes arriving
    /// through real bus files — scan → compute → render — become
    /// counts, never bytes.
    #[test]
    fn adversarial_bus_files_render_as_counts_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let sessions = space_dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // A well-formed neighbor with hostile dirty lines (spaces,
        // ANSI, RTL, dot-escape, a 4 KB name) — parse counts them.
        let hostile_dirty = format!(
            "---\nv: 1\nkind: session-declaration\nid: s-sly\nbackend: native\ncreated_ms: 1\n---\n\
             ## intent\nlooks legit\n\n## dirty\n- src/ok.rs\n- has space.rs\n- e\u{1b}[31m.rs\n- r\u{202e}tl.rs\n- ../../etc/passwd\n- {}\n",
            "k".repeat(4096)
        );
        std::fs::write(sessions.join("s-sly.md"), hostile_dirty).unwrap();
        // A malformed neighbor and a foreign entry: named scan rejects.
        std::fs::write(sessions.join("s-junk.md"), "not a document").unwrap();
        std::fs::write(sessions.join("stray.txt"), "x").unwrap();
        // Own declaration sharing the one valid path → a real ALERT.
        let own = format!(
            "---\nv: 1\nkind: session-declaration\nid: {OWN}\nbackend: native\ncreated_ms: 1\n---\n\
             ## intent\nme\n\n## dirty\n- src/ok.rs\n"
        );
        std::fs::write(sessions.join(format!("{OWN}.md")), own).unwrap();

        let now = crate::coordination::now_ms();
        let bus = crate::coordination::radar::read_space_bus(&space_dir, now).unwrap();
        let snapshot = compute_space_snapshot(
            &RadarSpaceInputs {
                space_key: SPACE,
                declarations: &bus.declarations,
                observed: &[],
                messages: &bus.messages,
                pr_files: &[],
                scan_invalid: bus.scan_invalid,
            },
            now,
        );
        let block = render_block(&snapshot, OWN, None, 0, now).unwrap();
        assert_schema_clean(&block.text);
        assert!(block.has_alert, "{}", block.text);
        assert!(block
            .text
            .contains("! overlap src/ok.rs — with s-sly (declared)"));
        // 5 hostile dirty lines + 2 scan rejects (junk doc, stray file).
        assert!(
            block.text.contains("invalid: 7 entries ignored"),
            "{}",
            block.text
        );
        for hostile in ["has space", "\u{1b}", "\u{202e}", "passwd", "kkkk"] {
            assert!(!block.text.contains(hostile), "{hostile:?} leaked");
        }
    }

    /// RULED binding (R1/R6): the 1536-byte cap AT the boundary —
    /// pre-cap renders of 1535/1536/1537 bytes prove the invariant.
    #[test]
    fn byte_cap_holds_at_the_boundary() {
        // 7 fixed-length overlap paths + 1 tunable path + presence +
        // one message line gives fine-grained control of the pre-cap
        // size; candidates are built by the real builder.
        let snapshot_for = |tail_len: usize| {
            let mut s = base_snapshot();
            s.sessions
                .push(presence("s-other-1", Some("native"), false));
            for i in 0..7 {
                s.pair_overlaps.push(pair(
                    &format!("src/{i}{}", "f".repeat(114)),
                    OWN,
                    "s-other-1",
                    true,
                    true,
                ));
            }
            s.pair_overlaps.push(pair(
                &format!("t/{}", "z".repeat(tail_len)),
                OWN,
                "s-other-1",
                true,
                true,
            ));
            s.messages
                .push(message("s-other-1", "m-0123456789ab", None));
            for (w, k) in [
                ("s-other-2", "ac"),
                ("s-other-2", "ad"),
                ("s-other-3", "ae"),
                ("s-other-3", "af"),
            ] {
                s.messages
                    .push(message(w, &format!("m-0123456789{k}"), None));
            }
            s
        };
        let precap_len = |tail_len: usize| {
            let c = build_candidates(&snapshot_for(tail_len), OWN).unwrap();
            assert!(!c.cap_dropped, "boundary probe must not be count-capped");
            let mut len = c.header.len();
            for line in c
                .sessions
                .iter()
                .map(String::as_str)
                .chain(c.overlaps.iter().map(String::as_str))
                .chain(c.messages.iter().map(String::as_str))
                .chain(c.invalid_line.iter().map(String::as_str))
            {
                len += 1 + line.len();
            }
            len
        };
        let base = precap_len(1);
        for target in [1535usize, 1536, 1537] {
            let tail_len = 1 + (target - base);
            assert!(
                tail_len <= 118,
                "probe path stays under the display bound (base={base})"
            );
            assert_eq!(precap_len(tail_len), target, "probe construction");
            let block = render_block(&snapshot_for(tail_len), OWN, None, 0, 0).unwrap();
            assert!(
                block.text.len() <= RENDERED_BLOCK_MAX_BYTES,
                "target {target}: rendered {} bytes",
                block.text.len()
            );
            assert_schema_clean(&block.text);
            assert!(block.has_alert, "alerts kept first under pressure");
            if target <= RENDERED_BLOCK_MAX_BYTES {
                assert_eq!(block.text.len(), target, "under the cap nothing is cut");
                assert!(!block.text.contains("[truncated]"));
            } else {
                assert!(block.text.ends_with("\n[truncated]"));
                assert!(
                    block
                        .text
                        .lines()
                        .filter(|l| l.starts_with("! overlap"))
                        .count()
                        >= 7,
                    "ALERT lines are the last to go: {}",
                    block.text
                );
            }
        }
    }

    /// RULED binding (R6): zero-LLM determinism — same snapshot, same
    /// bytes, every time.
    #[test]
    fn rendering_is_deterministic() {
        let mut s = base_snapshot();
        s.sessions.push(presence("s-other", Some("codex"), false));
        s.pair_overlaps
            .push(pair("src/a.rs", OWN, "s-other", true, true));
        s.messages.push(message("s-other", "m-0123456789ab", None));
        s.invalid = 3;
        let a = render(&s).unwrap();
        let b = render(&s).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.text, b.text, "byte-identical");
        assert_eq!(a.hash, super::super::fnv1a_64(a.text.as_bytes()));
    }

    /// §2.5: dedup by hash, the 30-minute reminder floor, and the
    /// natural new-alert bypass.
    #[test]
    fn dedup_floor_and_alert_bypass() {
        let mut s = base_snapshot();
        s.sessions.push(presence("s-other", Some("native"), false));
        let t0: u64 = 1_000_000;
        let first = render_block(&s, OWN, None, 0, t0).expect("first render lands");

        // Identical render inside the floor: suppressed.
        assert!(render_block(&s, OWN, Some(first.hash), t0, t0 + 1_000).is_none());
        assert!(render_block(&s, OWN, Some(first.hash), t0, t0 + REINJECT_FLOOR_MS - 1).is_none());
        // At the floor: re-injected as a reminder, identical bytes.
        let again = render_block(&s, OWN, Some(first.hash), t0, t0 + REINJECT_FLOOR_MS)
            .expect("reminder floor re-injects");
        assert_eq!(again.text, first.text);

        // A NEW alert changes the hash and bypasses the floor.
        s.pair_overlaps
            .push(pair("src/hot.rs", OWN, "s-other", false, true));
        let alerted = render_block(&s, OWN, Some(first.hash), t0, t0 + 1_000)
            .expect("new alert bypasses naturally");
        assert_ne!(alerted.hash, first.hash);
        assert!(alerted.has_alert);
    }

    #[test]
    fn hostile_space_key_renders_nothing() {
        let mut s = base_snapshot();
        s.space_key = "Weird Space!\u{1b}".to_string();
        s.sessions.push(presence("s-other", Some("native"), false));
        assert!(render(&s).is_none(), "nothing safe to say");
    }

    #[test]
    fn invalid_only_block_is_ambient_signal() {
        let mut s = base_snapshot();
        s.invalid = 4;
        let block = render(&s).unwrap();
        assert_schema_clean(&block.text);
        assert_eq!(
            block.text,
            format!("[System] coordination v1 space={SPACE}\ninvalid: 4 entries ignored")
        );
        assert!(!block.has_alert);
    }

    /// The compute → render pipeline stays deterministic end to end
    /// under shuffled inputs (the radar sorts, the renderer preserves).
    #[test]
    fn pipeline_is_order_independent() {
        let declarations = Vec::new();
        let observed_a = [
            ObservedSet {
                writer_id: OWN.to_string(),
                paths: ["src/x.rs".to_string(), "src/y.rs".to_string()]
                    .into_iter()
                    .collect(),
            },
            ObservedSet {
                writer_id: "s-peer".to_string(),
                paths: ["src/y.rs".to_string(), "src/x.rs".to_string()]
                    .into_iter()
                    .collect(),
            },
        ];
        let observed_b = [observed_a[1].clone(), observed_a[0].clone()];
        let prs = [PrFileSet {
            number: 5,
            paths: ["src/x.rs".to_string()].into_iter().collect(),
        }];
        let make = |observed: &[ObservedSet]| {
            let snapshot = compute_space_snapshot(
                &RadarSpaceInputs {
                    space_key: SPACE,
                    declarations: &declarations,
                    observed,
                    messages: &[],
                    pr_files: &prs,
                    scan_invalid: 0,
                },
                7,
            );
            render_block(&snapshot, OWN, None, 0, 7).unwrap().text
        };
        assert_eq!(make(&observed_a), make(&observed_b));
    }

    // ── §2.8 external ALERT steer lines ──

    /// RULED binding (R8): ambient content cannot reach the external
    /// steer lane. A snapshot rich in every ambient signal — presence,
    /// broadcast and addressed messages, invalid counts — but with no
    /// ALERT overlap renders ZERO steers; the native block for the same
    /// snapshot happily renders the ambient lines. Type-level backstop:
    /// `render_alert_steers` reads only the overlap collections, so
    /// there is no code path from ambient fields into a steer.
    #[test]
    fn ambient_content_never_reaches_the_steer_lane() {
        let mut s = base_snapshot();
        s.sessions.push(presence("s-other", Some("codex"), false));
        s.sessions.push(presence("s-tired", None, true));
        s.messages.push(message("s-peer", "m-0123456789ab", None));
        s.messages
            .push(message("s-peer", "m-0123456789ac", Some(OWN)));
        s.invalid = 7;
        assert!(
            render_block(&s, OWN, None, 0, 0).is_some(),
            "the native block DOES carry this ambient signal"
        );
        assert!(
            render_alert_steers(&s, OWN).is_empty(),
            "no ALERT ⇒ no steer, ever"
        );

        // Foreign pairs and foreign PR overlaps are someone else's
        // alerts — still nothing for this session's lane.
        s.pair_overlaps
            .push(pair("src/foreign.rs", "s-xx", "s-yy", true, false));
        s.pr_overlaps.push(RadarPrOverlap {
            path: "docs/theirs.md".to_string(),
            writer: "s-other".to_string(),
            pr: 9,
        });
        assert!(render_alert_steers(&s, OWN).is_empty());
    }

    /// One steer per distinct overlap set per session pair / per PR,
    /// schema-shaped, paths grouped and sorted, ambient tokens absent.
    #[test]
    fn steer_lines_group_sets_per_pair_and_pr() {
        let mut s = base_snapshot();
        s.sessions.push(presence("s-other-one", Some("codex"), false));
        s.messages.push(message("s-peer", "m-0123456789ab", None));
        s.pair_overlaps
            .push(pair("src/b.rs", OWN, "s-other-one", true, false));
        s.pair_overlaps
            .push(pair("src/a.rs", "s-other-one", OWN, false, true));
        s.pair_overlaps
            .push(pair("src/c.rs", OWN, "s-second", false, true));
        s.pr_overlaps.push(RadarPrOverlap {
            path: "docs/mine.md".to_string(),
            writer: OWN.to_string(),
            pr: 566,
        });
        let steers = render_alert_steers(&s, OWN);
        assert_eq!(steers.len(), 3, "{steers:?}");
        assert_eq!(
            steers[0].text,
            format!(
                "[System] coordination v1 space={SPACE} ! overlap src/a.rs, src/b.rs — with s-other- (declared+git)"
            )
        );
        assert_eq!(
            steers[1].text,
            format!("[System] coordination v1 space={SPACE} ! overlap src/c.rs — with s-second (git)")
        );
        assert_eq!(
            steers[2].text,
            format!("[System] coordination v1 space={SPACE} ! overlap docs/mine.md — pr#566")
        );
        for steer in &steers {
            assert!(!steer.text.contains('\n'), "single line");
            assert!(
                !steer.text.contains("messages:") && !steer.text.contains("sessions:"),
                "ambient vocabulary never rides a steer: {}",
                steer.text
            );
        }
        // Distinct sets ⇒ distinct hashes; a pure evidence shift on the
        // same set ⇒ the SAME hash (radar-note canon).
        assert_ne!(steers[0].set_hash, steers[1].set_hash);
        let mut shifted = base_snapshot();
        shifted
            .pair_overlaps
            .push(pair("src/b.rs", OWN, "s-other-one", false, true));
        shifted
            .pair_overlaps
            .push(pair("src/a.rs", "s-other-one", OWN, false, true));
        let shifted = render_alert_steers(&shifted, OWN);
        assert_eq!(shifted[0].set_hash, steers[0].set_hash);
        assert_ne!(shifted[0].text, steers[0].text, "sources still render");
        assert!(shifted[0].text.ends_with("(git)"), "{}", shifted[0].text);
    }

    /// Hostile tokens are skipped, never rendered; a set with no valid
    /// path renders nothing at all.
    #[test]
    fn steer_lines_drop_hostile_tokens() {
        let mut s = base_snapshot();
        s.pair_overlaps
            .push(pair("evil path.rs", OWN, "s-other", true, false));
        s.pair_overlaps
            .push(pair("ansi\u{1b}[31m.rs", OWN, "s-other", true, false));
        s.pair_overlaps
            .push(pair("src/fine.rs", OWN, "UPPER ID", true, false));
        s.pr_overlaps.push(RadarPrOverlap {
            path: "-dash.rs".to_string(),
            writer: OWN.to_string(),
            pr: 3,
        });
        assert!(
            render_alert_steers(&s, OWN).is_empty(),
            "nothing valid to say"
        );

        s.pair_overlaps
            .push(pair("src/ok.rs", OWN, "s-other", false, true));
        let steers = render_alert_steers(&s, OWN);
        assert_eq!(steers.len(), 1);
        assert!(steers[0].text.contains("src/ok.rs"));
        for hostile in ["evil path", "\u{1b}", "UPPER", "-dash.rs"] {
            assert!(!steers[0].text.contains(hostile), "{hostile:?} leaked");
        }

        let mut hostile_key = base_snapshot();
        hostile_key.space_key = "Bad Key!".to_string();
        hostile_key
            .pair_overlaps
            .push(pair("src/ok.rs", OWN, "s-other", true, true));
        assert!(render_alert_steers(&hostile_key, OWN).is_empty());
    }

    /// The steer line holds its own byte wall: many long paths list at
    /// most [`STEER_MAX_PATHS_LISTED`], the rest fold into the fixed
    /// `(+<n> more)` token, and byte pressure drops listed paths from
    /// the back rather than overflowing.
    #[test]
    fn steer_lines_hold_the_byte_cap() {
        let mut s = base_snapshot();
        let long_paths: Vec<String> = (0..12)
            .map(|i| format!("src/{i:02}{}", "p".repeat(110)))
            .collect();
        for p in &long_paths {
            s.pair_overlaps.push(pair(p, OWN, "s-other", true, true));
        }
        let steers = render_alert_steers(&s, OWN);
        assert_eq!(steers.len(), 1);
        let text = &steers[0].text;
        assert!(
            text.len() <= STEER_LINE_MAX_BYTES,
            "{} bytes: {text}",
            text.len()
        );
        assert!(text.contains(" more)"), "{text}");
        let listed = text.matches("src/").count();
        assert!(listed >= 1 && listed <= STEER_MAX_PATHS_LISTED, "{text}");
        assert!(
            text.contains(&format!("(+{} more)", 12 - listed)),
            "count stays exact: {text}"
        );
        assert!(text.ends_with("— with s-other (declared+git)"), "{text}");
    }

    /// Determinism: same snapshot, same steer bytes and hashes.
    #[test]
    fn steer_rendering_is_deterministic() {
        let mut s = base_snapshot();
        s.pair_overlaps
            .push(pair("src/y.rs", OWN, "s-other", true, false));
        s.pair_overlaps
            .push(pair("src/x.rs", OWN, "s-other", false, true));
        let a = render_alert_steers(&s, OWN);
        let b = render_alert_steers(&s, OWN);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1);
        assert!(a[0].text.contains("src/x.rs, src/y.rs"), "{}", a[0].text);
    }
}
