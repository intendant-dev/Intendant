//! Track NS prose-export primitive (`intendant transcripts export` —
//! ruled R1/R8; commission `~/narrative-synthesis-kickoff-brief.md`,
//! design `~/narrative-synthesis-intake.md`).
//!
//! A keyless, daemonless read lane over the raw per-backend transcript
//! stores: enumerates sessions the way the indexer sweeps do, parses
//! through the SAME extractors (doctrine: no fifth parser family), applies
//! export-time redaction (`redact.rs`, default ON), and emits
//! deterministic session-delimited JSONL. It never touches the shard
//! store or its cursors — that store is a 14-day search cache
//! (`store.rs::RETENTION_MS`); this lane serves the full-history
//! narrative pyramid.
//!
//! Output contract (byte-stable for fixed inputs — pinned by test):
//! - export mode: per session, one `{"type":"session", …}` header row
//!   (spans, counts, per-class redactions), then one row per message —
//!   the [`MessageRecord`] serde shape verbatim, so every row carries the
//!   FROZEN [`super::record::Locator`] anchor the provenance pyramid
//!   cites. Supersession marks are deliberately not exported in v1:
//!   rewound prose is still history; search-view folding stays the query
//!   side's concern.
//! - `--list` mode: stat-only `{"type":"session-listing", …}` rows (no
//!   parse, no text) — cheap enough for the digest journal's daily diff
//!   over the whole 78 GB estate.
//!
//! Sessions are ordered by `(source, session_id)`; records ride in
//! extractor order. Wrapper sessions (external backends' native mirrors)
//! export nothing — their prose is canonical in the external store.
//! Lease-staging remnants (`SweepRoots::staged_entries`) are out of the
//! v1 lane: transient copies of leased homes, not this machine's history.

use super::indexer::{collect_suffix_files, resolve_production_roots, SweepRoots};
use super::record::{MessageRecord, Source};
use super::redact::redact_text;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What one export run should cover. `sources`/`session_keys` of `None`
/// mean "all"; times bound record `ts_ms` (export) and act as a coarse
/// newest-mtime prefilter (`since` only — an old `until` cannot excuse
/// skipping a still-growing session).
pub(crate) struct ExportOptions {
    pub sources: Option<Vec<Source>>,
    pub session_keys: Option<BTreeSet<String>>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub redact: bool,
    pub list_only: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            sources: None,
            session_keys: None,
            since_ms: None,
            until_ms: None,
            redact: true,
            list_only: false,
        }
    }
}

/// Where one session's emitted lines go. The stream sink serves stdout
/// and tests; the CLI adds a per-session-file sink for `--out DIR`.
pub(crate) trait ExportSink {
    fn session(
        &mut self,
        source: Source,
        session_id: &str,
        lines: &[String],
    ) -> std::io::Result<()>;
}

pub(crate) struct StreamSink<W: Write>(pub W);

impl<W: Write> ExportSink for StreamSink<W> {
    fn session(
        &mut self,
        _source: Source,
        _session_id: &str,
        lines: &[String],
    ) -> std::io::Result<()> {
        for line in lines {
            self.0.write_all(line.as_bytes())?;
            self.0.write_all(b"\n")?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct ExportStats {
    pub sessions_emitted: u64,
    pub sessions_skipped_wrapper: u64,
    pub sessions_skipped_empty: u64,
    pub records: u64,
    pub truncated_records: u64,
    pub redactions: BTreeMap<&'static str, u64>,
}

/// One enumerated session and everything extraction needs to parse it.
struct SessionEntry {
    source: Source,
    session_id: String,
    newest_mtime_ms: i64,
    total_bytes: u64,
    input: EntryInput,
}

enum EntryInput {
    Intendant {
        dir: PathBuf,
    },
    Codex {
        path: PathBuf,
    },
    Claude {
        main: PathBuf,
        agents: Vec<PathBuf>,
    },
    Kimi {
        location: crate::web_gateway::session_catalog::kimi_history::KimiSessionLocation,
    },
    Pi {
        location: crate::web_gateway::session_catalog::pi_history::PiSessionLocation,
    },
}

fn stat_ms_len(path: &Path) -> (i64, u64) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            (ms, meta.len())
        }
        Err(_) => (0, 0),
    }
}

fn wants_source(opts: &ExportOptions, source: Source) -> bool {
    opts.sources
        .as_ref()
        .map(|set| set.contains(&source))
        .unwrap_or(true)
}

fn session_key(source: Source, session_id: &str) -> String {
    format!("{}:{}", source.as_str(), session_id)
}

fn enumerate(roots: &SweepRoots, opts: &ExportOptions) -> Vec<SessionEntry> {
    let mut entries: Vec<SessionEntry> = Vec::new();

    if wants_source(opts, Source::Intendant) {
        if let Ok(dirs) = std::fs::read_dir(&roots.intendant_logs) {
            for dir in dirs.flatten() {
                let dir_path = dir.path();
                if !dir_path.is_dir() {
                    continue;
                }
                let log_path = dir_path.join("session.jsonl");
                if !log_path.is_file() {
                    continue;
                }
                let (mtime, bytes) = stat_ms_len(&log_path);
                let session_id = dir_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                entries.push(SessionEntry {
                    source: Source::Intendant,
                    session_id,
                    newest_mtime_ms: mtime,
                    total_bytes: bytes,
                    input: EntryInput::Intendant { dir: dir_path },
                });
            }
        }
    }

    if wants_source(opts, Source::Codex) {
        // Same id from several paths (sessions/ vs archived_sessions/, or
        // several roots): newest mtime wins, deterministically.
        let mut by_id: HashMap<String, (i64, u64, PathBuf)> = HashMap::new();
        for root in &roots.codex_roots {
            let mut files = Vec::new();
            collect_suffix_files(&root.join("sessions"), ".jsonl", 6, &mut files);
            collect_suffix_files(&root.join("archived_sessions"), ".jsonl", 6, &mut files);
            for path in files {
                let Some(session_id) =
                    crate::external_agent::codex::rollout::codex_session_file_id(&path)
                else {
                    continue;
                };
                let (mtime, bytes) = stat_ms_len(&path);
                let replace = by_id
                    .get(&session_id)
                    .map(|(current, _, _)| mtime > *current)
                    .unwrap_or(true);
                if replace {
                    by_id.insert(session_id, (mtime, bytes, path));
                }
            }
        }
        for (session_id, (mtime, bytes, path)) in by_id {
            entries.push(SessionEntry {
                source: Source::Codex,
                session_id,
                newest_mtime_ms: mtime,
                total_bytes: bytes,
                input: EntryInput::Codex { path },
            });
        }
    }

    if wants_source(opts, Source::ClaudeCode) {
        for root in &roots.claude_project_roots {
            // Mirrors the sweep's discovery (indexer.rs sweep_claude): one
            // pass builds mains and subagent files across every project
            // dir, because a subagent dir can live under a different
            // project dir than its main after a worktree relocation.
            let mut mains: HashMap<String, PathBuf> = HashMap::new();
            let mut subagents: HashMap<String, Vec<PathBuf>> = HashMap::new();
            let Ok(projects) = std::fs::read_dir(root) else {
                continue;
            };
            for project in projects.flatten() {
                let project_path = project.path();
                if !project_path.is_dir() {
                    continue;
                }
                let Ok(children) = std::fs::read_dir(&project_path) else {
                    continue;
                };
                for child in children.flatten() {
                    let child_path = child.path();
                    let name = child.file_name().to_string_lossy().to_string();
                    if child_path.is_file() {
                        if let Some(stem) = name.strip_suffix(".jsonl") {
                            mains.insert(stem.to_string(), child_path);
                        }
                    } else if child_path.is_dir() {
                        let mut agent_files = Vec::new();
                        collect_suffix_files(
                            &child_path.join("subagents"),
                            ".jsonl",
                            2,
                            &mut agent_files,
                        );
                        if !agent_files.is_empty() {
                            agent_files.sort();
                            subagents.entry(name).or_default().extend(agent_files);
                        }
                    }
                }
            }
            for (session_id, main_path) in mains {
                let mut agent_paths = subagents.remove(&session_id).unwrap_or_default();
                // Relocated project dirs can carry hardlinked twins of the
                // same subagent transcript — dedup by file identity, path
                // as the fallback (same rule as the sweep).
                agent_paths.sort();
                agent_paths.dedup();
                let mut seen_identities: Vec<crate::platform::FileIdentity> = Vec::new();
                agent_paths.retain(
                    |path| match crate::platform::FileIdentity::from_path(path) {
                        Ok(identity) if identity.is_reliable() => {
                            if seen_identities.contains(&identity) {
                                false
                            } else {
                                seen_identities.push(identity);
                                true
                            }
                        }
                        _ => true,
                    },
                );
                let (main_mtime, main_bytes) = stat_ms_len(&main_path);
                let mut newest = main_mtime;
                let mut bytes = main_bytes;
                for path in &agent_paths {
                    let (mtime, len) = stat_ms_len(path);
                    newest = newest.max(mtime);
                    bytes += len;
                }
                entries.push(SessionEntry {
                    source: Source::ClaudeCode,
                    session_id,
                    newest_mtime_ms: newest,
                    total_bytes: bytes,
                    input: EntryInput::Claude {
                        main: main_path,
                        agents: agent_paths,
                    },
                });
            }
        }
    }

    if wants_source(opts, Source::Kimi) {
        let mut by_id = HashMap::<
            String,
            crate::web_gateway::session_catalog::kimi_history::KimiSessionLocation,
        >::new();
        for root in &roots.kimi_roots {
            for location in crate::web_gateway::session_catalog::kimi_history::list_kimi_sessions_in(
                root,
                crate::web_gateway::session_catalog::kimi_history::KIMI_SESSION_SCAN_LIMIT,
            ) {
                let replace = by_id
                    .get(&location.session_id)
                    .map(|current| location.activity_mtime() > current.activity_mtime())
                    .unwrap_or(true);
                if replace {
                    by_id.insert(location.session_id.clone(), location);
                }
            }
        }
        for (session_id, location) in by_id {
            let bytes = location
                .all_dependency_paths()
                .map(|path| stat_ms_len(path).1)
                .sum();
            entries.push(SessionEntry {
                source: Source::Kimi,
                session_id,
                newest_mtime_ms: location.activity_mtime(),
                total_bytes: bytes,
                input: EntryInput::Kimi { location },
            });
        }
    }

    if wants_source(opts, Source::Pi) {
        let mut by_id = HashMap::<
            String,
            crate::web_gateway::session_catalog::pi_history::PiSessionLocation,
        >::new();
        for root in &roots.pi_roots {
            for location in crate::web_gateway::session_catalog::pi_history::list_pi_sessions_in(
                root,
                crate::web_gateway::session_catalog::pi_history::PI_SESSION_SCAN_LIMIT,
            ) {
                let replace = by_id
                    .get(&location.session_id)
                    .map(|current| location.updated_millis > current.updated_millis)
                    .unwrap_or(true);
                if replace {
                    by_id.insert(location.session_id.clone(), location);
                }
            }
        }
        for (session_id, location) in by_id {
            let (mtime, bytes) = stat_ms_len(&location.path);
            entries.push(SessionEntry {
                source: Source::Pi,
                session_id,
                newest_mtime_ms: mtime,
                total_bytes: bytes,
                input: EntryInput::Pi { location },
            });
        }
    }

    entries.retain(|entry| {
        if let Some(keys) = &opts.session_keys {
            if !keys.contains(&session_key(entry.source, &entry.session_id)) {
                return false;
            }
        }
        if let Some(since) = opts.since_ms {
            // newest_mtime < since ⇒ every record predates the window.
            if entry.newest_mtime_ms < since {
                return false;
            }
        }
        true
    });
    entries.sort_by(|a, b| {
        (a.source.as_str(), a.session_id.as_str()).cmp(&(b.source.as_str(), b.session_id.as_str()))
    });
    entries
}

fn extract_records(input: EntryInput) -> std::io::Result<Option<Vec<MessageRecord>>> {
    match input {
        EntryInput::Intendant { dir } => {
            let extraction = super::extract_intendant::extract_intendant_session(&dir)?;
            if extraction.wrapper {
                return Ok(None);
            }
            Ok(Some(extraction.shard.records))
        }
        EntryInput::Codex { path } => {
            let (shard, _) = super::extract_codex::extract_codex_session(&path, None, 0)?;
            Ok(Some(shard.records))
        }
        EntryInput::Claude { main, agents } => {
            let session_id = main
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_default();
            let (shard, _) =
                super::extract_claude::extract_claude_session(&session_id, &main, &agents)?;
            Ok(Some(shard.records))
        }
        EntryInput::Kimi { location } => {
            let (shard, _) = super::extract_kimi::extract_kimi_session(location)?;
            Ok(Some(shard.records))
        }
        EntryInput::Pi { location } => {
            let (shard, _) = super::extract_pi::extract_pi_session(location)?;
            Ok(Some(shard.records))
        }
    }
}

/// Run one export (or listing) over `roots` into `sink`.
pub(crate) fn run_export(
    roots: &SweepRoots,
    opts: &ExportOptions,
    sink: &mut dyn ExportSink,
) -> std::io::Result<ExportStats> {
    let mut stats = ExportStats::default();
    for entry in enumerate(roots, opts) {
        let key = session_key(entry.source, &entry.session_id);
        if opts.list_only {
            let line = serde_json::json!({
                "type": "session-listing",
                "source": entry.source.as_str(),
                "session_id": entry.session_id.as_str(),
                "session_key": key,
                "newest_mtime_ms": entry.newest_mtime_ms,
                "total_bytes": entry.total_bytes,
            })
            .to_string();
            sink.session(entry.source, &entry.session_id, &[line])?;
            stats.sessions_emitted += 1;
            continue;
        }

        let Some(mut records) = extract_records(entry.input)? else {
            stats.sessions_skipped_wrapper += 1;
            continue;
        };
        if let Some(since) = opts.since_ms {
            records.retain(|record| record.ts_ms >= since);
        }
        if let Some(until) = opts.until_ms {
            records.retain(|record| record.ts_ms <= until);
        }
        if records.is_empty() {
            stats.sessions_skipped_empty += 1;
            continue;
        }

        let mut session_redactions: BTreeMap<&'static str, u64> = BTreeMap::new();
        if opts.redact {
            for record in &mut records {
                let outcome = redact_text(&record.text);
                if !outcome.counts.is_empty() {
                    for (class, count) in &outcome.counts {
                        *session_redactions.entry(class).or_insert(0) += u64::from(*count);
                    }
                    record.text = outcome.text;
                }
            }
        }

        let truncated = records.iter().filter(|record| record.truncated).count() as u64;
        let first_ts_ms = records.iter().map(|record| record.ts_ms).min().unwrap_or(0);
        let last_ts_ms = records.iter().map(|record| record.ts_ms).max().unwrap_or(0);
        let mut lines = Vec::with_capacity(records.len() + 1);
        lines.push(
            serde_json::json!({
                "type": "session",
                "source": entry.source.as_str(),
                "session_id": entry.session_id.as_str(),
                "session_key": key,
                "first_ts_ms": first_ts_ms,
                "last_ts_ms": last_ts_ms,
                "records": records.len(),
                "truncated_records": truncated,
                "redactions": session_redactions,
            })
            .to_string(),
        );
        for record in &records {
            lines.push(serde_json::to_string(record).map_err(std::io::Error::other)?);
        }
        sink.session(entry.source, &entry.session_id, &lines)?;

        stats.sessions_emitted += 1;
        stats.records += records.len() as u64;
        stats.truncated_records += truncated;
        for (class, count) in session_redactions {
            *stats.redactions.entry(class).or_insert(0) += count;
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// CLI edge (`intendant transcripts …`) — the ONE place that reads the real
// environment (production roots); everything above takes explicit inputs.
// ---------------------------------------------------------------------------

const USAGE: &str = "usage: intendant transcripts export \
[--source intendant|codex|claude-code|kimi|pi]... [--session SOURCE:ID]... \
[--since WHEN] [--until WHEN] [--list] [--redact on|off] [--out DIR|-]
WHEN: epoch ms, YYYY-MM-DD (UTC midnight), or RFC3339";

fn parse_source(raw: &str) -> Option<Source> {
    [
        Source::Intendant,
        Source::Codex,
        Source::ClaudeCode,
        Source::Kimi,
        Source::Pi,
    ]
    .into_iter()
    .find(|source| source.as_str() == raw)
}

fn parse_when(raw: &str) -> Option<i64> {
    if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
        return raw.parse::<i64>().ok();
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        // UTC midnight, deliberately not local: byte-stable output must
        // not depend on the invoking shell's timezone.
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// `--out DIR`: one file per session under `DIR/<source>/<session_id>.jsonl`,
/// with each session's header echoed to stdout as a running index.
struct DirSink {
    root: PathBuf,
}

impl ExportSink for DirSink {
    fn session(
        &mut self,
        source: Source,
        session_id: &str,
        lines: &[String],
    ) -> std::io::Result<()> {
        let dir = self.root.join(source.as_str());
        std::fs::create_dir_all(&dir)?;
        let mut body = String::new();
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        std::fs::write(dir.join(format!("{session_id}.jsonl")), body)?;
        if let Some(header) = lines.first() {
            println!("{header}");
        }
        Ok(())
    }
}

pub(crate) fn transcripts_cli(argv: &[String]) -> i32 {
    if argv.first().map(String::as_str) != Some("export") {
        eprintln!("{USAGE}");
        return 2;
    }
    let mut opts = ExportOptions::default();
    let mut sources: Vec<Source> = Vec::new();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let mut out = "-".to_string();
    let mut args = argv[1..].iter();
    while let Some(arg) = args.next() {
        let mut take = |flag: &str| -> Option<String> {
            let value = args.next().cloned();
            if value.is_none() {
                eprintln!("error: {flag} needs a value\n{USAGE}");
            }
            value
        };
        match arg.as_str() {
            "--source" => match take("--source").as_deref().map(parse_source) {
                Some(Some(source)) => sources.push(source),
                Some(None) => {
                    eprintln!("error: unknown --source\n{USAGE}");
                    return 2;
                }
                None => return 2,
            },
            "--session" => match take("--session") {
                Some(key) => {
                    keys.insert(key);
                }
                None => return 2,
            },
            "--since" | "--until" => {
                let flag = arg.as_str();
                let Some(raw) = take(flag) else { return 2 };
                let Some(ms) = parse_when(&raw) else {
                    eprintln!("error: cannot parse {flag} value {raw:?}\n{USAGE}");
                    return 2;
                };
                if flag == "--since" {
                    opts.since_ms = Some(ms);
                } else {
                    opts.until_ms = Some(ms);
                }
            }
            "--list" => opts.list_only = true,
            "--redact" => match take("--redact").as_deref() {
                Some("on") => opts.redact = true,
                Some("off") => opts.redact = false,
                Some(_) => {
                    eprintln!("error: --redact takes on|off\n{USAGE}");
                    return 2;
                }
                None => return 2,
            },
            "--out" => match take("--out") {
                Some(path) => out = path,
                None => return 2,
            },
            other => {
                eprintln!("error: unknown flag {other:?}\n{USAGE}");
                return 2;
            }
        }
    }
    if !sources.is_empty() {
        opts.sources = Some(sources);
    }
    if !keys.is_empty() {
        opts.session_keys = Some(keys);
    }

    let roots = resolve_production_roots();
    let result = if out == "-" {
        let stdout = std::io::stdout();
        let mut sink = StreamSink(std::io::BufWriter::new(stdout.lock()));
        let result = run_export(&roots, &opts, &mut sink);
        result.and_then(|stats| sink.0.flush().map(|()| stats))
    } else {
        let mut sink = DirSink {
            root: PathBuf::from(&out),
        };
        run_export(&roots, &opts, &mut sink)
    };
    match result {
        Ok(stats) => {
            let redaction_total: u64 = stats.redactions.values().sum();
            eprintln!(
                "[transcripts] sessions={} records={} truncated={} redactions={} wrapper_skipped={} empty_skipped={}",
                stats.sessions_emitted,
                stats.records,
                stats.truncated_records,
                redaction_total,
                stats.sessions_skipped_wrapper,
                stats.sessions_skipped_empty,
            );
            0
        }
        Err(err) => {
            eprintln!("error: export failed: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(iso: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis()
    }

    fn write_jsonl(path: &Path, lines: &[serde_json::Value]) {
        let mut body = String::new();
        for line in lines {
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn native_fixture(logs: &Path) {
        // Real session: user prose (with a secret for the redaction pin),
        // assistant prose via sidecar span, plus diagnostics that must
        // never export.
        let dir = logs.join("11111111-aaaa-bbbb-cccc-000000000001");
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(
            dir.join("turns").join("turn_001_model.txt"),
            "NATIVE ASSISTANT PROSE",
        )
        .unwrap();
        write_jsonl(
            &dir.join("session.jsonl"),
            &[
                serde_json::json!({"ts":"10:00:00.000","ts_ms":1_000,"event":"session_started",
                    "data":{"session_id":"native-1","task":"native user prose"}}),
                serde_json::json!({"ts":"10:00:00.000","ts_ms":1_000,"event":"conversation_message",
                    "data":{"message_id":"mid-1","message_seq":1,"role":"user","provenance":"task",
                            "text":"native user prose with key sk-abcdefabcdefabcdef12 inside"}}),
                serde_json::json!({"ts":"10:00:01.000","ts_ms":2_000,"event":"conversation_message",
                    "file":"turns/turn_001_model.txt",
                    "data":{"message_id":"mid-2","message_seq":2,"role":"assistant","provenance":"assistant",
                            "model_offset":0,"model_bytes":22}}),
            ],
        );
        // Wrapper session: must export nothing (canonical in the external
        // backend's own store).
        let wrapper = logs.join("22222222-aaaa-bbbb-cccc-000000000002");
        std::fs::create_dir_all(&wrapper).unwrap();
        write_jsonl(
            &wrapper.join("session.jsonl"),
            &[
                serde_json::json!({"ts":"09:00:00.000","ts_ms":1,"event":"session_identity",
                "data":{"session_id":"wrap","source":"codex","backend_session_id":"abc"}}),
            ],
        );
    }

    const CLAUDE_SESSION: &str = "33333333-aaaa-bbbb-cccc-000000000003";

    fn claude_row(
        kind: &str,
        uuid: &str,
        iso: &str,
        content: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "userType": "external",
            "type": kind,
            "message": {"role": kind, "content": content},
            "uuid": uuid,
            "timestamp": iso,
            "sessionId": CLAUDE_SESSION,
            "version": "2.1.207",
        })
    }

    fn claude_fixture(projects: &Path) {
        let project = projects.join("-Users-test-proj");
        write_jsonl(
            &project.join(format!("{CLAUDE_SESSION}.jsonl")),
            &[
                claude_row(
                    "user",
                    "u-1",
                    "2026-07-10T10:00:00.000Z",
                    serde_json::json!("claude user prose"),
                ),
                claude_row(
                    "assistant",
                    "a-1",
                    "2026-07-10T10:00:05.000Z",
                    serde_json::json!([
                        {"type":"text","text":"claude assistant prose"},
                        {"type":"tool_use","id":"toolu_01","name":"Read","input":{"file_path":"/tmp/x"}},
                    ]),
                ),
                claude_row(
                    "user",
                    "u-2",
                    "2026-07-10T10:00:06.000Z",
                    serde_json::json!([
                        {"type":"tool_result","tool_use_id":"toolu_01","content":"secret tool payload"},
                    ]),
                ),
                // Harness plumbing that must never count as prose.
                claude_row(
                    "user",
                    "u-3",
                    "2026-07-10T10:00:07.000Z",
                    serde_json::json!(
                        "<system-reminder>injected context reminder</system-reminder>"
                    ),
                ),
                claude_row(
                    "user",
                    "u-4",
                    "2026-07-10T10:00:08.000Z",
                    serde_json::json!(
                        "<task-notification>background task done</task-notification>"
                    ),
                ),
            ],
        );
    }

    fn codex_fixture(codex_home: &Path) {
        write_jsonl(
            &codex_home
                .join("sessions")
                .join("2026")
                .join("07")
                .join("01")
                .join("rollout-2026-07-01T10-00-00-sess-codex-1.jsonl"),
            &[
                serde_json::json!({"timestamp":"2026-07-01T10:00:00.000Z","type":"session_meta",
                    "payload":{"id":"sess-codex-1",
                               "base_instructions":{"text":"You are Codex, harness config, never prose"}}}),
                serde_json::json!({"timestamp":"2026-07-01T10:00:01.000Z","type":"event_msg",
                    "payload":{"type":"user_message","message":"codex user prose",
                               "images":[],"local_images":[],"text_elements":[]}}),
                serde_json::json!({"timestamp":"2026-07-01T10:00:02.000Z","type":"response_item",
                    "payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"hidden reasoning"}]}}),
                serde_json::json!({"timestamp":"2026-07-01T10:00:03.000Z","type":"response_item",
                    "payload":{"type":"function_call","name":"shell",
                               "arguments":"{\"command\":[\"rg\",\"needle\"]}","call_id":"call-1"}}),
                serde_json::json!({"timestamp":"2026-07-01T10:00:04.000Z","type":"response_item",
                    "payload":{"type":"message","role":"assistant","id":"msg-a1",
                               "content":[{"type":"output_text","text":"codex assistant prose"}]}}),
            ],
        );
    }

    fn kimi_fixture(kimi_home: &Path) {
        let dir = kimi_home
            .join("sessions/wd_repo")
            .join("session_44444444-aaaa-bbbb-cccc-000000000004");
        std::fs::create_dir_all(dir.join("agents/main")).unwrap();
        std::fs::write(
            dir.join("state.json"),
            serde_json::json!({
                "createdAt":"2026-07-19T10:00:00.000Z",
                "updatedAt":"2026-07-19T10:01:00.000Z",
                "workDir":"/repo",
                "agents":{"main":{"type":"main","parentAgentId":null}}
            })
            .to_string(),
        )
        .unwrap();
        write_jsonl(
            &dir.join("agents/main/wire.jsonl"),
            &[
                serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"kimi user prose"}],
                    "origin":{"kind":"user"},"time":1_784_455_200_000i64}),
                serde_json::json!({"type":"context.append_loop_event",
                    "event":{"type":"content.part","uuid":"k-a1","part":{"type":"text","text":"kimi assistant prose"}},
                    "time":1_784_455_200_100i64}),
            ],
        );
    }

    fn fixture_roots(tmp: &Path) -> SweepRoots {
        let logs = tmp.join("intendant-logs");
        std::fs::create_dir_all(&logs).unwrap();
        native_fixture(&logs);
        let claude_projects = tmp.join("claude-projects");
        std::fs::create_dir_all(&claude_projects).unwrap();
        claude_fixture(&claude_projects);
        let codex_home = tmp.join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        codex_fixture(&codex_home);
        let kimi_home = tmp.join("kimi-code");
        std::fs::create_dir_all(&kimi_home).unwrap();
        kimi_fixture(&kimi_home);
        SweepRoots {
            store_root: tmp.join("unused-store"),
            intendant_logs: logs,
            codex_roots: vec![codex_home],
            claude_project_roots: vec![claude_projects],
            kimi_roots: vec![kimi_home],
            pi_roots: Vec::new(),
            staged_entries: Vec::new(),
        }
    }

    fn export_string(roots: &SweepRoots, opts: &ExportOptions) -> (String, ExportStats) {
        let mut sink = StreamSink(Vec::<u8>::new());
        let stats = run_export(roots, opts, &mut sink).unwrap();
        (String::from_utf8(sink.0).unwrap(), stats)
    }

    #[test]
    fn export_is_prose_only_deterministic_and_redacted() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let opts = ExportOptions::default();

        let (first, stats) = export_string(&roots, &opts);
        let (second, _) = export_string(&roots, &opts);
        assert_eq!(first, second, "byte-stable across runs");

        // The prose from all four backends is present…
        for expected in [
            "native user prose",
            "NATIVE ASSISTANT PROSE",
            "claude user prose",
            "claude assistant prose",
            "codex user prose",
            "codex assistant prose",
            "kimi user prose",
            "kimi assistant prose",
        ] {
            assert!(first.contains(expected), "missing prose: {expected}");
        }
        // …and zero tool traffic, harness config, or injected plumbing.
        for forbidden in [
            "tool_use",
            "tool_result",
            "function_call",
            "secret tool payload",
            "hidden reasoning",
            "base_instructions",
            "never prose",
            "system-reminder",
            "task-notification",
        ] {
            assert!(
                !first.contains(forbidden),
                "leaked into export: {forbidden}"
            );
        }
        // The wrapper session exported nothing.
        assert!(!first.contains("22222222-aaaa-bbbb-cccc-000000000002"));
        assert_eq!(stats.sessions_skipped_wrapper, 1);
        assert_eq!(stats.sessions_emitted, 4, "one session per backend");

        // Redaction: the native secret is scrubbed, counted in the header.
        assert!(!first.contains("sk-abcdefabcdefabcdef12"));
        assert!(first.contains("[REDACTED:provider-key]"));
        assert_eq!(stats.redactions.get("provider-key"), Some(&1));
        let native_header: serde_json::Value = first
            .lines()
            .find(|line| line.contains("\"type\":\"session\"") && line.contains("intendant:"))
            .map(|line| serde_json::from_str(line).unwrap())
            .expect("native session header");
        assert_eq!(native_header["redactions"]["provider-key"], 1);

        // Sessions ride sorted by (source, session_id).
        let order: Vec<String> = first
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|value| value["type"] == "session")
            .map(|value| value["session_key"].as_str().unwrap().to_string())
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
    }

    #[test]
    fn redact_off_is_the_operator_forensics_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let opts = ExportOptions {
            redact: false,
            ..ExportOptions::default()
        };
        let (output, stats) = export_string(&roots, &opts);
        assert!(output.contains("sk-abcdefabcdefabcdef12"));
        assert!(stats.redactions.is_empty());
    }

    #[test]
    fn since_until_bound_records_and_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());

        // until before the codex assistant row: only the user record stays.
        let opts = ExportOptions {
            sources: Some(vec![Source::Codex]),
            until_ms: Some(ms("2026-07-01T10:00:01.500Z")),
            ..ExportOptions::default()
        };
        let (output, stats) = export_string(&roots, &opts);
        assert!(output.contains("codex user prose"));
        assert!(!output.contains("codex assistant prose"));
        assert_eq!(stats.records, 1);

        // since after every record but before the file mtime ("now"): the
        // coarse prefilter keeps the session, the record filter empties
        // it, and the skip is counted.
        let opts = ExportOptions {
            sources: Some(vec![Source::Codex]),
            since_ms: Some(ms("2026-07-02T00:00:00.000Z")),
            ..ExportOptions::default()
        };
        let (output, stats) = export_string(&roots, &opts);
        assert!(output.is_empty());
        assert_eq!(stats.sessions_emitted, 0);
        assert_eq!(stats.sessions_skipped_empty, 1);

        // since far in the future: the mtime prefilter drops the session
        // before extraction ever runs.
        let opts = ExportOptions {
            sources: Some(vec![Source::Codex]),
            since_ms: Some(ms("2100-01-01T00:00:00.000Z")),
            ..ExportOptions::default()
        };
        let (output, stats) = export_string(&roots, &opts);
        assert!(output.is_empty());
        assert_eq!(stats.sessions_emitted, 0);
        assert_eq!(stats.sessions_skipped_empty, 0, "prefiltered, not parsed");
    }

    #[test]
    fn session_key_filter_selects_exactly_one() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let opts = ExportOptions {
            session_keys: Some(
                [session_key(Source::Codex, "sess-codex-1")]
                    .into_iter()
                    .collect(),
            ),
            ..ExportOptions::default()
        };
        let (output, stats) = export_string(&roots, &opts);
        assert_eq!(stats.sessions_emitted, 1);
        assert!(output.contains("codex user prose"));
        assert!(!output.contains("claude user prose"));
    }

    #[test]
    fn list_mode_is_stat_only() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let opts = ExportOptions {
            list_only: true,
            ..ExportOptions::default()
        };
        let (output, stats) = export_string(&roots, &opts);
        // Listing includes the wrapper session (no parse happens), so:
        // native real + native wrapper + claude + codex + kimi.
        assert_eq!(stats.sessions_emitted, 5);
        for line in output.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["type"], "session-listing");
            assert!(value.get("text").is_none());
            assert!(value["total_bytes"].as_u64().unwrap() > 0);
        }
        assert!(!output.contains("prose"), "no message text in listings");
    }

    #[test]
    fn locators_ride_every_exported_row() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let (output, _) = export_string(&roots, &ExportOptions::default());
        for line in output.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["type"] == "session" {
                continue;
            }
            assert!(
                value["locator"]["kind"].is_string(),
                "record without a locator anchor: {line}"
            );
            assert!(value["ts_ms"].is_i64() || value["ts_ms"].is_u64());
        }
    }

    #[test]
    fn parse_when_accepts_the_three_forms_utc() {
        assert_eq!(parse_when("1785000000000"), Some(1_785_000_000_000));
        assert_eq!(
            parse_when("2026-07-01"),
            Some(ms("2026-07-01T00:00:00.000Z")),
            "dates resolve at UTC midnight, never local"
        );
        assert_eq!(
            parse_when("2026-07-01T10:00:00.000Z"),
            Some(ms("2026-07-01T10:00:00.000Z"))
        );
        assert_eq!(parse_when("not-a-time"), None);
    }
}
