//! Instant vitals hydration — the disk-recovery side of the session
//! vitals hub (`session_vitals.rs`, at its size budget, keeps only the
//! live producers).
//!
//! After a resume or daemon restart the Model/effort and permission
//! chips used to wait on the backend's first wire echo (seconds — or
//! forever, for restored sessions nobody resumes), and the context
//! meter waited on the first live usage snapshot. The ground truth
//! already sits on disk: the wrapper log dir's recorded launch config
//! (`session_agent_config.json`) and the backend's own transcript
//! (Claude Code stamps model / top-level effort / permissionMode /
//! usage / cwd on its `~/.claude/projects` records). This module reads
//! both and feeds the hub — under a strict precedence:
//!
//! **wire echo > launch config > disk snapshot**, per field, by
//! construction: the hydrator merges its two disk layers itself
//! (recorded launch config beats the transcript) and the hub folds the
//! result **fill-if-absent** ([`SessionVitalsHub::apply_recovered_facts`]
//! never overwrites a field a live producer stated), while every live
//! fold overwrites recovered values and clears their `recovered_at`
//! stamps. Races between the hydrator and a spawning backend are
//! harmless in either order.
//!
//! Recovered fields carry provenance stamps (`model_recovered_at_epoch`,
//! `permission_recovered_at_epoch`, the cache/context sections'
//! `recovered_at_epoch`) so frontends can caveat the chip ("from the
//! session log as of …"); disk-sourced permission facts keep
//! `permission_echoed: false`.
//!
//! Two call sites hydrate: the boot restore scan (same candidate walk
//! and newest-N cap as the git-target restore — [`restored_session_candidates`])
//! and the resume/attach lanes (`session_supervisor/launch.rs`, beside
//! `seed_resumed_git_vitals_locus`). Both emit
//! `AppEvent::SessionRecoveredFacts` — hub-internal, no outbound twin,
//! never persisted — and everything downstream (session-log vitals
//! rows, `session_state_lines` bootstrap, tunnel, Station, peers)
//! inherits from the hub's ordinary `SessionVitals` emission.
//!
//! This module also owns the hub's account rate-limit window
//! persistence: the per-account store (account truth that outlives any
//! session) survives restarts in a small versioned state file, and the
//! existing `observed_at_epoch` / reset-rollover degradation keeps a
//! stale restore honest. Reported percentages persist as reported;
//! nothing is ever synthesized.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::event::{AppEvent, EventBus};
use crate::external_agent::AgentBackend;
use crate::session_config::SessionAgentConfig;
use crate::session_vitals::{GitVitalsTargets, SessionVitalsHub};
use crate::types::{
    SessionCacheVitals, SessionConfigVitals, SessionContextVitals, SessionLimitWindow,
};

// ---------------------------------------------------------------------------
// Recovered facts + the hub's fill-if-absent fold
// ---------------------------------------------------------------------------

/// One session's disk-recovered vitals facts, pre-merged across the two
/// disk layers (recorded launch config over transcript snapshot) with
/// every provenance stamp already baked in. Carried by
/// `AppEvent::SessionRecoveredFacts`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveredSessionFacts {
    /// Backend source ("claude-code", "codex", "kimi", "pi") when known — the
    /// membership key that lets a restored session inherit the account's
    /// persisted rate-limit windows.
    pub source: Option<String>,
    /// Config-section facts; absent fields claim nothing. Recovered
    /// stamps ride the fields themselves.
    pub config: SessionConfigVitals,
    /// Prompt-cache receipt recovered from the transcript's last usage
    /// record (`recovered_at_epoch` set; `last_activity_epoch` is the
    /// record's own time, so the client-side TTL countdown reads
    /// honestly stale).
    pub cache: Option<SessionCacheVitals>,
    /// Context footprint recovered from the transcript's last usage
    /// record (`recovered_at_epoch` set).
    pub context: Option<SessionContextVitals>,
}

impl RecoveredSessionFacts {
    /// Nothing to state: the hydrator skips the emission entirely.
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.config == SessionConfigVitals::default()
            && self.cache.is_none()
            && self.context.is_none()
    }
}

impl SessionVitalsHub {
    /// Fold disk-recovered facts into a session's vitals, fill-if-absent
    /// per field: a value any live producer already stated — launch
    /// emission, wire echo, usage fold — is never overwritten, so the
    /// hydrator can lose every race safely (live folds conversely
    /// overwrite recovered values and clear their stamps; see
    /// `apply_config_facts` and the usage listener).
    ///
    /// A known `source` additionally records account membership and
    /// mirrors the account's known rate-limit windows into the session —
    /// how a restored session inherits the persisted account state
    /// without waiting for a live `SessionIdentity`.
    pub(crate) fn apply_recovered_facts(&self, session_id: &str, facts: RecoveredSessionFacts) {
        if facts.is_empty() {
            return;
        }
        let RecoveredSessionFacts {
            source,
            config,
            cache,
            context,
        } = facts;
        if config != SessionConfigVitals::default() || cache.is_some() || context.is_some() {
            self.apply(session_id, |vitals| {
                if config != SessionConfigVitals::default() {
                    let section = vitals.config.get_or_insert_with(Default::default);
                    if section.model.is_none() && config.model.is_some() {
                        section.model = config.model.clone();
                        section.model_recovered_at_epoch = config.model_recovered_at_epoch;
                    }
                    if section.effort.is_none() && config.effort.is_some() {
                        section.effort = config.effort.clone();
                    }
                    if section.permission_mode.is_none() && config.permission_mode.is_some() {
                        section.permission_mode = config.permission_mode.clone();
                        section.permission_kind = config.permission_kind.clone();
                        // Disk facts are never a backend's own voucher.
                        section.permission_echoed = false;
                        section.permission_recovered_at_epoch =
                            config.permission_recovered_at_epoch;
                    }
                }
                if vitals.cache.is_none() {
                    vitals.cache = cache.clone();
                }
                if vitals.context.is_none() {
                    vitals.context = context.clone();
                }
            });
        }
        // Membership last, so the mirrored account-limit emission (if
        // any) already carries the sections filled above.
        if let Some(source) = source.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let canonical = self.resolve(session_id);
            if !canonical.is_empty() {
                self.session_sources
                    .lock()
                    .expect("vitals source lock")
                    .insert(canonical, source.to_string());
                // An empty report mirrors the account's known windows into
                // the newly joined member (the `link_identity` pattern).
                self.apply_rate_limit_windows(session_id, Vec::new());
            }
        }
    }

    /// Persist the account rate-limit window store to the hub's state
    /// file, if one is configured. Serialization happens under the lock
    /// (the store is a handful of windows); the write itself is handed to
    /// the blocking pool when a runtime is present, falling back to an
    /// inline write in plain-sync callers (tests).
    pub(crate) fn persist_account_limits(&self) {
        let Some(path) = self.limit_store.clone() else {
            return;
        };
        let snapshot = self
            .account_limits
            .lock()
            .expect("vitals account lock")
            .clone();
        let write = move || {
            if let Err(err) = persist_account_limit_store(&path, &snapshot) {
                eprintln!("Session vitals: account limit store write failed: {err}");
            }
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::spawn_blocking(write);
        } else {
            write();
        }
    }
}

// ---------------------------------------------------------------------------
// Account rate-limit window persistence
// ---------------------------------------------------------------------------

/// Store format version this daemon writes. Readers accept exactly this
/// major shape; unknown FIELDS anywhere are ignored (additive evolution
/// needs no bump), while a future breaking shape bumps the version and
/// this reader fails closed to an empty store (windows re-learn on the
/// next provider report — honest, never corrupting).
const ACCOUNT_LIMIT_STORE_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct AccountLimitStoreFile {
    version: u32,
    /// Backend source → that account's windows (label-keyed map flattened
    /// to a list on disk; labels re-key on load).
    accounts: BTreeMap<String, Vec<SessionLimitWindow>>,
}

/// Production location of the account rate-limit window store.
pub(crate) fn account_limit_store_path() -> PathBuf {
    crate::platform::intendant_home().join("account-rate-limits.json")
}

/// Load the persisted per-account window store. Missing file, parse
/// failures, and future-versioned files all read as empty — the store is
/// a warm-start convenience, never load-bearing state.
pub(crate) fn load_account_limit_store(
    path: &Path,
) -> HashMap<String, BTreeMap<String, SessionLimitWindow>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(file) = serde_json::from_str::<AccountLimitStoreFile>(&raw) else {
        return HashMap::new();
    };
    if file.version != ACCOUNT_LIMIT_STORE_VERSION {
        return HashMap::new();
    }
    file.accounts
        .into_iter()
        .map(|(source, windows)| {
            let store: BTreeMap<String, SessionLimitWindow> = windows
                .into_iter()
                .filter(|window| !window.label.trim().is_empty())
                .map(|window| (window.label.trim().to_string(), window))
                .collect();
            (source, store)
        })
        .filter(|(source, store)| !source.trim().is_empty() && !store.is_empty())
        .collect()
}

/// Atomically write the account window store (temp file + rename via the
/// shared helper). Windows persist exactly as reported — `used_pct`
/// stays absent when the provider never stated one.
pub(crate) fn persist_account_limit_store(
    path: &Path,
    accounts: &HashMap<String, BTreeMap<String, SessionLimitWindow>>,
) -> Result<(), String> {
    let file = AccountLimitStoreFile {
        version: ACCOUNT_LIMIT_STORE_VERSION,
        accounts: accounts
            .iter()
            .map(|(source, store)| (source.clone(), store.values().cloned().collect()))
            .collect(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::file_watcher::atomic_write(path, json.as_bytes()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Claude Code transcript tail extractor
// ---------------------------------------------------------------------------

/// The last usage entry a transcript records, in the fields the vitals
/// sections need (verified live against Claude Code 2.1.215–2.1.217
/// records: `message.usage` with the per-TTL `cache_creation` split).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordedUsage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    /// Per-TTL cache-write split (`cache_creation.ephemeral_{5m,1h}_input_tokens`)
    /// — the cache chip's TTL flavor.
    pub ephemeral_5m_tokens: u64,
    pub ephemeral_1h_tokens: u64,
    /// Epoch seconds of the record carrying this usage, when it stated a
    /// parseable `timestamp`.
    pub observed_at_epoch: Option<u64>,
}

impl RecordedUsage {
    fn footprint_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens + self.output_tokens
    }
}

/// Facts one bounded read of a Claude Code transcript yields — the
/// last-recorded value per field. Absent fields make no claim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordedSessionFacts {
    /// `message.model` of the last main-thread assistant record.
    pub model: Option<String>,
    /// Top-level `effort` of the last main-thread assistant record
    /// (transcript-only in 2.1.217 — live stream envelopes don't carry
    /// it; absent = no claim, never inferred).
    pub effort: Option<String>,
    /// Top-level `permissionMode` (user records and the dedicated
    /// `permission-mode` records).
    pub permission_mode: Option<String>,
    /// Last main-thread usage entry with any tokens in it.
    pub usage: Option<RecordedUsage>,
    /// Top-level `cwd` of the last record stating one (any record type —
    /// identical scope to `latest_recorded_cwd`, which resume-locus
    /// seeding already trusts).
    pub cwd: Option<String>,
    /// Epoch seconds of the newest record that contributed any fact —
    /// the "as of" the recovered chips are caveated with.
    pub as_of_epoch: Option<u64>,
}

impl RecordedSessionFacts {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold one JSONL record, last-writer-wins per field. Sidechain
    /// records (in-band Task children) are skipped for the session facts
    /// — a child can run a different model, and its usage is the child's
    /// context, not the main thread's — but still count for `cwd`.
    fn fold_record(&mut self, obj: &serde_json::Value) {
        let record_epoch = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_record_epoch);
        if let Some(cwd) = obj
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            self.cwd = Some(cwd.to_string());
            self.as_of_epoch = record_epoch.or(self.as_of_epoch);
        }
        if obj.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            return;
        }
        let mut contributed = false;
        if let Some(mode) = obj
            .get("permissionMode")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            self.permission_mode = Some(mode.to_string());
            contributed = true;
        }
        if obj.get("type").and_then(|t| t.as_str()) == Some("assistant") {
            if let Some(model) = obj
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|m| !m.is_empty())
            {
                self.model = Some(model.to_string());
                contributed = true;
            }
            if let Some(effort) = obj
                .get("effort")
                .and_then(|e| e.as_str())
                .map(str::trim)
                .filter(|e| !e.is_empty())
            {
                self.effort = Some(effort.to_string());
                contributed = true;
            }
            if let Some(usage) = obj
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|usage| recorded_usage_from_value(usage, record_epoch))
            {
                self.usage = Some(usage);
                contributed = true;
            }
        }
        if contributed {
            self.as_of_epoch = record_epoch.or(self.as_of_epoch);
        }
    }

    /// Whether every field the transcript can state is already known —
    /// the head-window fallback is only read for the remainder.
    fn saturated(&self) -> bool {
        self.model.is_some()
            && self.effort.is_some()
            && self.permission_mode.is_some()
            && self.usage.is_some()
            && self.cwd.is_some()
    }
}

/// A `message.usage` object → the recovered-usage fields. All-zero usage
/// carries no information (synthetic records) and claims nothing.
fn recorded_usage_from_value(
    usage: &serde_json::Value,
    observed_at_epoch: Option<u64>,
) -> Option<RecordedUsage> {
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let ephemeral = |key: &str| {
        usage
            .get("cache_creation")
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    let recorded = RecordedUsage {
        input_tokens: read("input_tokens"),
        cache_read_tokens: read("cache_read_input_tokens"),
        cache_creation_tokens: read("cache_creation_input_tokens"),
        output_tokens: read("output_tokens"),
        ephemeral_5m_tokens: ephemeral("ephemeral_5m_input_tokens"),
        ephemeral_1h_tokens: ephemeral("ephemeral_1h_input_tokens"),
        observed_at_epoch,
    };
    (recorded.footprint_tokens() > 0).then_some(recorded)
}

/// ISO-8601 record timestamp → epoch seconds.
fn parse_record_epoch(timestamp: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(timestamp.trim())
        .ok()
        .map(|dt| dt.timestamp())
        .filter(|secs| *secs > 0)
        .map(|secs| secs as u64)
}

/// Byte windows for the bounded transcript read — the
/// `latest_recorded_cwd` shape (tail nearly always answers; the head is
/// the fallback for facts a tail of cwd-less oversized records missed).
const FACTS_TAIL_BYTES: u64 = 256 * 1024;
const FACTS_HEAD_BYTES: u64 = 64 * 1024;

/// The most recent session facts a Claude Code transcript records, in
/// two bounded reads (tail, then a head fallback for still-unknown
/// fields) — a resume must not pay a full parse of a multi-megabyte
/// transcript. `None` when the file can't be read or no scanned window
/// states any fact.
pub(crate) fn latest_recorded_session_facts(path: &Path) -> Option<RecordedSessionFacts> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let tail_start = len.saturating_sub(FACTS_TAIL_BYTES);
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    // Lossy: a mid-file seek can split a multi-byte character; the
    // replacement bytes only ever corrupt the partial line the scan
    // skips anyway.
    let tail = String::from_utf8_lossy(&tail);
    let mut facts = fold_facts_in_lines(&tail, tail_start > 0);
    if tail_start > 0 && !facts.saturated() {
        let mut head = vec![0u8; FACTS_HEAD_BYTES.min(tail_start) as usize];
        file.seek(SeekFrom::Start(0)).ok()?;
        file.read_exact(&mut head).ok()?;
        let head = String::from_utf8_lossy(&head);
        let head_facts = fold_facts_in_lines(&head, false);
        // The tail is newer: head facts only fill fields the tail left
        // unknown, and never advance the as-of stamp on their own.
        if facts.model.is_none() {
            facts.model = head_facts.model;
        }
        if facts.effort.is_none() {
            facts.effort = head_facts.effort;
        }
        if facts.permission_mode.is_none() {
            facts.permission_mode = head_facts.permission_mode;
        }
        if facts.usage.is_none() {
            facts.usage = head_facts.usage;
        }
        if facts.cwd.is_none() {
            facts.cwd = head_facts.cwd;
        }
        if facts.as_of_epoch.is_none() {
            facts.as_of_epoch = head_facts.as_of_epoch;
        }
    }
    (!facts.is_empty()).then_some(facts)
}

/// Fold the complete JSONL lines of one chunk. `skip_first_line` drops
/// the leading partial line of a mid-file read; a trailing partial line
/// simply fails its JSON parse.
fn fold_facts_in_lines(chunk: &str, skip_first_line: bool) -> RecordedSessionFacts {
    let mut facts = RecordedSessionFacts::default();
    let mut lines = chunk.lines();
    if skip_first_line {
        let _ = lines.next();
    }
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
            facts.fold_record(&obj);
        }
    }
    facts
}

// ---------------------------------------------------------------------------
// Layer merge: recorded launch config over transcript snapshot
// ---------------------------------------------------------------------------

/// Merge the two disk layers into one recovered-facts payload for a
/// session of `backend`. Field precedence inside the hydrator: the
/// wrapper's recorded launch config (what Intendant asked the backend to
/// run — the same layer a live spawn emission states) beats the
/// transcript snapshot; the live-wire layer wins later at the hub by
/// fill-if-absent. Cache/context only ever come from the transcript's
/// usage record.
///
/// Stamps state where a field's value actually came from:
/// transcript-sourced fields carry the record's as-of epoch;
/// launch-config-sourced fields carry `launch_config_as_of_epoch` — the
/// config file's mtime on the boot lane, and `None` on the resume lane
/// (the effective config computed this instant is current intent, the
/// same claim the spawn's own launch emission makes moments later, so
/// no caveat is owed).
pub(crate) fn recovered_facts_from_layers(
    backend: &AgentBackend,
    launch_config: Option<&SessionAgentConfig>,
    launch_config_as_of_epoch: Option<u64>,
    recorded: Option<&RecordedSessionFacts>,
) -> RecoveredSessionFacts {
    let recorded_as_of = recorded.and_then(|facts| facts.as_of_epoch);
    let (config_model, config_effort, config_mode): (
        Option<&String>,
        Option<&String>,
        Option<&String>,
    ) = match (backend, launch_config) {
        (AgentBackend::ClaudeCode, Some(config)) => (
            config.claude_model.as_ref(),
            config.claude_effort.as_ref(),
            config.claude_permission_mode.as_ref(),
        ),
        (AgentBackend::Codex, Some(config)) => (
            config.codex_model.as_ref(),
            config.codex_reasoning_effort.as_ref(),
            None, // sandbox·approval pair handled below
        ),
        (AgentBackend::Kimi, Some(config)) => (
            config.kimi_model.as_ref(),
            config.kimi_thinking.as_ref(),
            config.kimi_permission_mode.as_ref(),
        ),
        (AgentBackend::Pi, Some(config)) => {
            (config.pi_model.as_ref(), config.pi_thinking.as_ref(), None)
        }
        (_, None) => (None, None, None),
    };

    let mut config = SessionConfigVitals::default();
    let stamp_for = |from_config: bool| {
        if from_config {
            launch_config_as_of_epoch
        } else {
            recorded_as_of
        }
    };

    let recorded_model = recorded.and_then(|facts| facts.model.as_ref());
    if let Some(model) = config_model.or(recorded_model) {
        config.model = Some(model.clone());
        config.model_recovered_at_epoch = stamp_for(config_model.is_some());
    }
    let recorded_effort = recorded.and_then(|facts| facts.effort.as_ref());
    if let Some(effort) = config_effort.or(recorded_effort) {
        config.effort = Some(effort.clone());
    }

    // Permission mode + display kind, per backend vocabulary.
    match backend {
        AgentBackend::ClaudeCode => {
            let recorded_mode = recorded.and_then(|facts| facts.permission_mode.as_ref());
            if let Some(mode) = config_mode.or(recorded_mode) {
                config.permission_mode = Some(mode.clone());
                config.permission_kind =
                    intendant_core::vitals::claude_permission_kind(mode).map(str::to_string);
                config.permission_recovered_at_epoch = stamp_for(config_mode.is_some());
            }
        }
        AgentBackend::Codex => {
            // The launch pair renders exactly like the live launch-facts
            // emission ("sandbox · approval"); no transcript layer here
            // (the Codex rollout extractor is a flagged follow-up).
            let sandbox = launch_config
                .and_then(|c| c.codex_sandbox.as_deref())
                .map(str::trim)
                .unwrap_or_default();
            let approval = launch_config
                .and_then(|c| c.codex_approval_policy.as_deref())
                .map(str::trim)
                .unwrap_or_default();
            let mode = match (sandbox.is_empty(), approval.is_empty()) {
                (true, true) => None,
                (false, true) => Some(sandbox.to_string()),
                (true, false) => Some(approval.to_string()),
                (false, false) => Some(format!("{sandbox} · {approval}")),
            };
            if let Some(mode) = mode {
                config.permission_mode = Some(mode);
                config.permission_kind =
                    intendant_core::vitals::codex_permission_kind(approval, sandbox)
                        .map(str::to_string);
                config.permission_recovered_at_epoch = stamp_for(true);
            }
        }
        AgentBackend::Kimi => {
            if let Some(mode) = config_mode {
                config.permission_mode = Some(mode.clone());
                config.permission_kind =
                    crate::external_agent::kimi_code::kimi_permission_kind(mode)
                        .map(str::to_string);
                config.permission_recovered_at_epoch = stamp_for(true);
            }
        }
        AgentBackend::Pi => {
            // Pi's provider/tool process is always supervised through
            // Intendant's approval extension; unlike the other external
            // engines this permission vocabulary is an adapter invariant,
            // not a user-selectable launch field.
            config.permission_mode =
                Some(crate::external_agent::pi::PI_PERMISSION_MODE.to_string());
            config.permission_kind = Some(intendant_core::vitals::PERMISSION_KIND_ASK.to_string());
            config.permission_recovered_at_epoch = launch_config_as_of_epoch.or(recorded_as_of);
        }
    }

    // Cache + context from the transcript's last usage record.
    let usage = recorded.and_then(|facts| facts.usage.as_ref());
    let (cache, context) = usage
        .map(|usage| {
            let model = config.model.as_deref().unwrap_or_default();
            (
                recovered_cache_vitals(usage),
                recovered_context_vitals(usage, model),
            )
        })
        .unwrap_or((None, None));

    RecoveredSessionFacts {
        source: Some(backend_source_name(backend).to_string()),
        config,
        cache,
        context,
    }
}

fn backend_source_name(backend: &AgentBackend) -> &'static str {
    match backend {
        AgentBackend::ClaudeCode => "claude-code",
        AgentBackend::Codex => "codex",
        AgentBackend::Kimi => "kimi",
        AgentBackend::Pi => "pi",
    }
}

/// Cache receipt from a recorded usage entry — the
/// `cache_vitals_from_usage` formula over transcript fields. The TTL
/// flavor comes from the per-TTL split when it states one; a flat write
/// or a bare read falls back to the Anthropic 5-minute default (the
/// transcript is always an Anthropic backend). `last_activity_epoch` is
/// the record's own time — the client countdown then reads honestly
/// stale/cold instead of restarting.
fn recovered_cache_vitals(usage: &RecordedUsage) -> Option<SessionCacheVitals> {
    let sample_total = usage.cache_read_tokens + usage.cache_creation_tokens + usage.input_tokens;
    if sample_total == 0 {
        return None;
    }
    // Floor, never round — 100 must mean fully cache-served.
    let hit_pct = ((usage.cache_read_tokens * 100) / sample_total).min(100) as u8;
    let ttl_seconds = if usage.ephemeral_1h_tokens > 0 {
        Some(3600)
    } else if usage.ephemeral_5m_tokens > 0
        || usage.cache_creation_tokens > 0
        || usage.cache_read_tokens > 0
    {
        // A stated 5m split, a flat write, and a bare read all resolve to
        // the Anthropic 5-minute default.
        Some(300)
    } else {
        None
    };
    let observed = usage.observed_at_epoch?;
    Some(SessionCacheVitals {
        hit_pct: Some(hit_pct),
        last_activity_epoch: observed,
        ttl_seconds,
        recovered_at_epoch: Some(observed),
    })
}

/// Context footprint from a recorded usage entry — the live formula
/// (tokens = input + cache reads + cache writes + output; pct clamped by
/// the effective window) against the model family's known window, so a
/// 1M-window session never reads >100% against the 200k default.
fn recovered_context_vitals(usage: &RecordedUsage, model: &str) -> Option<SessionContextVitals> {
    let tokens_used = usage.footprint_tokens();
    if tokens_used == 0 {
        return None;
    }
    let window = crate::external_agent::claude_code::claude_model_context_window(model)
        .unwrap_or(crate::external_agent::claude_code::DEFAULT_CONTEXT_WINDOW);
    let effective_window = window.max(tokens_used);
    let usage_pct = (tokens_used as f64 / effective_window as f64) * 100.0;
    let observed = usage.observed_at_epoch?;
    Some(SessionContextVitals {
        tokens_used,
        context_window: effective_window,
        usage_pct,
        observed_at_epoch: observed,
        recovered_at_epoch: Some(observed),
    })
}

// ---------------------------------------------------------------------------
// Hydration call sites
// ---------------------------------------------------------------------------

/// One restored-session candidate from the on-disk session store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestoredSessionCandidate {
    pub session_id: String,
    /// Effective probe root: the worktree checkout for worktree
    /// sessions, else the recorded project root.
    pub root: PathBuf,
}

/// Cap on boot-time restoration. A long-lived store accumulates
/// thousands of non-ended session dirs (a real store measured ~2.1k
/// across ~100 distinct roots); the newest N by meta mtime cover the
/// session windows a dashboard realistically shows, and older idle
/// sessions regain their chips the moment they are resumed. Both boot
/// restore lanes — git-target registration and vitals hydration — run
/// over exactly this candidate list, so their coverage can't drift.
pub(crate) const RESTORED_TARGET_CAP: usize = 64;

/// Walk the session store for restore candidates: non-ended sessions
/// (scope mirrors the `SessionEnded` prune — a `completed` meta ended
/// before the restart), newest [`RESTORED_TARGET_CAP`] by meta mtime
/// (re-stamped on every lifecycle transition), worktree sessions keyed
/// by their checkout. Synchronous filesystem walk — call it from a
/// blocking context.
pub(crate) fn restored_session_candidates(home: &Path) -> Vec<RestoredSessionCandidate> {
    let logs_dir = crate::platform::intendant_home_in(home).join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return Vec::new();
    };
    let mut restored: Vec<(std::time::SystemTime, RestoredSessionCandidate)> = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path().join("session_meta.json");
        let Ok(raw) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<crate::session_log::SessionMeta>(&raw) else {
            continue;
        };
        // Parity with the SessionEnded prune: completed = ended.
        if meta.status.as_deref() == Some("completed") {
            continue;
        }
        let root = meta
            .worktree
            .as_ref()
            .map(|worktree| worktree.path.clone())
            .or(meta.project_root);
        let Some(root) = root.filter(|root| !root.trim().is_empty()) else {
            continue;
        };
        let session_id = meta.session_id.trim().to_string();
        if session_id.is_empty() {
            continue;
        }
        let mtime = meta_path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        restored.push((
            mtime,
            RestoredSessionCandidate {
                session_id,
                root: PathBuf::from(root),
            },
        ));
    }
    // Newest first; the cap keeps probe work and emission fan-out bounded.
    restored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    restored
        .into_iter()
        .take(RESTORED_TARGET_CAP)
        .map(|(_, candidate)| candidate)
        .collect()
}

/// The boot restore pass: one candidate walk feeding both restore lanes
/// — git-target registration (insert-if-absent, exactly
/// `register_restored_session_targets`) and vitals hydration for each
/// candidate's wrapper dir. Returns (targets registered, sessions
/// hydrated). Synchronous — the boot call sites run it on the blocking
/// pool.
pub(crate) fn restore_session_vitals_at_boot(
    home: &Path,
    registry: &GitVitalsTargets,
    bus: &EventBus,
) -> (usize, usize) {
    let candidates = restored_session_candidates(home);
    let registered = candidates
        .iter()
        .filter(|candidate| {
            registry.register_restored(&candidate.session_id, candidate.root.clone())
        })
        .count();
    let mut hydrated = 0usize;
    for candidate in &candidates {
        let facts = recover_wrapper_session_facts(home, &candidate.session_id);
        let Some((facts, cwd)) = facts else {
            continue;
        };
        // First-hand locus seed, exactly like the resume lane: the
        // transcript's recorded cwd is where the session actually works
        // (a worktree the registered root knows nothing about).
        if let Some(cwd) = cwd.as_deref() {
            registry.seed_locus(&candidate.session_id, Path::new(cwd));
        }
        if !facts.is_empty() {
            hydrated += 1;
            bus.send(AppEvent::SessionRecoveredFacts {
                session_id: Some(candidate.session_id.clone()),
                facts: Box::new(facts),
            });
        }
    }
    (registered, hydrated)
}

/// Disk recovery for one wrapper session dir: the recorded launch config
/// (`session_agent_config.json`), the wrapper-index mapping to the
/// backend conversation, and — for Claude Code — the transcript tail.
/// Returns the merged facts plus the transcript's recorded cwd (the
/// boot locus seed). `None` when the wrapper resolves to no external
/// backend (native sessions have no disk layer beyond what the daemon
/// already replays).
fn recover_wrapper_session_facts(
    home: &Path,
    wrapper_session_id: &str,
) -> Option<(RecoveredSessionFacts, Option<String>)> {
    let log_dir = crate::platform::intendant_home_in(home)
        .join("logs")
        .join(wrapper_session_id);
    let launch_config = crate::session_config::read_log_dir_config(&log_dir);
    let config_as_of = launch_config.as_ref().and_then(|_| {
        log_dir
            .join(crate::session_config::SESSION_AGENT_CONFIG_FILE)
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|age| age.as_secs())
    });
    let conversation =
        crate::external_wrapper_index::conversation_for_wrapper(home, wrapper_session_id);
    let source = conversation
        .as_ref()
        .map(|(source, _)| source.clone())
        .or_else(|| {
            launch_config
                .as_ref()
                .and_then(|config| config.source.clone())
        });
    let backend = crate::external_agent::AgentBackend::from_str_loose(source.as_deref()?)?;
    let recorded = match (&backend, conversation.as_ref()) {
        (AgentBackend::ClaudeCode, Some((_, backend_session_id))) => {
            crate::web_gateway::find_claude_session_file(home, backend_session_id)
                .and_then(|transcript| latest_recorded_session_facts(&transcript))
        }
        _ => None,
    };
    let cwd = recorded.as_ref().and_then(|facts| facts.cwd.clone());
    let facts = recovered_facts_from_layers(
        &backend,
        launch_config.as_ref(),
        config_as_of,
        recorded.as_ref(),
    );
    Some((facts, cwd))
}

/// The resume/attach hydration lane, called beside
/// `seed_resumed_git_vitals_locus` with the just-computed effective
/// launch config: reads the backend transcript tail (Claude Code) off
/// the blocking pool and emits the merged recovered facts keyed exactly
/// like the lane's git-vitals registration. Fire-and-forget — the live
/// spawn's own launch emission and echoes win any race by construction.
pub(crate) fn hydrate_resumed_session_vitals(
    bus: EventBus,
    home: PathBuf,
    vitals_session_id: String,
    backend: AgentBackend,
    resume_token: String,
    launch_config: Option<SessionAgentConfig>,
) {
    if vitals_session_id.trim().is_empty() {
        return;
    }
    let work = move || {
        let recorded = match &backend {
            AgentBackend::ClaudeCode => {
                let token = resume_token.trim();
                if token.is_empty() {
                    None
                } else {
                    crate::web_gateway::find_claude_session_file(&home, token)
                        .and_then(|transcript| latest_recorded_session_facts(&transcript))
                }
            }
            // Codex/Kimi/Pi: launch-config layer only (transcript extractors
            // are a flagged follow-up).
            _ => None,
        };
        let facts = recovered_facts_from_layers(
            &backend,
            launch_config.as_ref(),
            // The effective config was computed this instant — current
            // intent, not a disk snapshot: no stamp (see
            // `recovered_facts_from_layers`).
            None,
            recorded.as_ref(),
        );
        if !facts.is_empty() {
            bus.send(AppEvent::SessionRecoveredFacts {
                session_id: Some(vitals_session_id),
                facts: Box::new(facts),
            });
        }
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(work);
    } else {
        work();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_vitals::spawn_cache_vitals_listener;

    // Fixture lines shaped from live Claude Code 2.1.215–2.1.217 records
    // (synthetic values — no real transcript bytes committed).
    fn assistant_line(model: &str, effort: &str, ts: &str, sidechain: bool) -> String {
        format!(
            concat!(
                r#"{{"parentUuid":"p1","isSidechain":{sidechain},"type":"assistant","effort":"{effort}","#,
                r#""cwd":"/repo","sessionId":"cc-1","version":"2.1.217","gitBranch":"main","#,
                r#""uuid":"u1","timestamp":"{ts}","requestId":"req_1","#,
                r#""message":{{"id":"msg_1","type":"message","role":"assistant","model":"{model}","#,
                r#""content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn","#,
                r#""usage":{{"input_tokens":2,"cache_creation_input_tokens":3240,"cache_read_input_tokens":55796,"#,
                r#""output_tokens":521,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":3240}}}}}}}}"#,
            ),
            sidechain = sidechain,
            effort = effort,
            ts = ts,
            model = model,
        )
    }

    fn user_line(mode: &str, cwd: &str, ts: &str) -> String {
        format!(
            concat!(
                r#"{{"parentUuid":null,"isSidechain":false,"type":"user","permissionMode":"{mode}","#,
                r#""cwd":"{cwd}","sessionId":"cc-1","version":"2.1.217","timestamp":"{ts}","#,
                r#""message":{{"role":"user","content":"do the thing"}}}}"#,
            ),
            mode = mode,
            cwd = cwd,
            ts = ts,
        )
    }

    /// The extractor folds model / top-level effort / permissionMode /
    /// last usage (with the per-TTL split) / cwd / record timestamp,
    /// last-writer-wins, skipping sidechain records for session facts.
    #[test]
    fn extractor_reads_the_live_record_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut lines = Vec::new();
        lines.push(user_line("default", "/repo", "2026-07-22T07:00:00.000Z"));
        lines.push(assistant_line(
            "claude-haiku-4-5",
            "low",
            "2026-07-22T07:10:00.000Z",
            false,
        ));
        // Dedicated permission-mode record (no timestamp on the live shape).
        lines.push(
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"cc-1"}"#
                .to_string(),
        );
        // A sidechain Task child on another model must not claim the facts.
        lines.push(assistant_line(
            "claude-haiku-4-5-child",
            "high",
            "2026-07-22T07:20:00.000Z",
            true,
        ));
        // Malformed line: skipped, never aborts the fold.
        lines.push("{not json".to_string());
        lines.push(assistant_line(
            "claude-fable-5",
            "xhigh",
            "2026-07-22T07:53:52.991Z",
            false,
        ));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let facts = latest_recorded_session_facts(&path).expect("facts");
        assert_eq!(facts.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(facts.effort.as_deref(), Some("xhigh"));
        assert_eq!(facts.permission_mode.as_deref(), Some("bypassPermissions"));
        assert_eq!(facts.cwd.as_deref(), Some("/repo"));
        let usage = facts.usage.as_ref().expect("usage");
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.cache_read_tokens, 55_796);
        assert_eq!(usage.cache_creation_tokens, 3_240);
        assert_eq!(usage.output_tokens, 521);
        assert_eq!(usage.ephemeral_1h_tokens, 3_240);
        assert_eq!(usage.ephemeral_5m_tokens, 0);
        let as_of = facts.as_of_epoch.expect("as-of stamp");
        assert_eq!(usage.observed_at_epoch, Some(as_of));
        let parsed = chrono::DateTime::parse_from_rfc3339("2026-07-22T07:53:52.991Z").unwrap();
        assert_eq!(as_of, parsed.timestamp() as u64);
    }

    /// Absent fields claim nothing: a transcript with no effort / usage /
    /// mode yields None for exactly those fields, and an empty or missing
    /// file yields no facts at all.
    #[test]
    fn extractor_absent_fields_make_no_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            concat!(
                // Assistant record without effort/usage (older CLIs).
                r#"{"type":"assistant","cwd":"/repo","timestamp":"2026-07-22T07:00:00Z","#,
                r#""message":{"role":"assistant","model":"claude-haiku-4-5","content":[]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let facts = latest_recorded_session_facts(&path).expect("facts");
        assert_eq!(facts.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(facts.effort, None, "no effort recorded — no claim");
        assert_eq!(facts.permission_mode, None);
        assert_eq!(facts.usage, None);

        std::fs::write(&path, "").unwrap();
        assert_eq!(latest_recorded_session_facts(&path), None);
        assert_eq!(
            latest_recorded_session_facts(&dir.path().join("gone.jsonl")),
            None
        );
    }

    /// The head window fills only fields a fact-less oversized tail
    /// missed (the `latest_recorded_cwd` shape).
    #[test]
    fn extractor_scans_tail_then_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut contents = user_line("plan", "/repo", "2026-07-22T06:00:00Z");
        contents.push('\n');
        contents.push_str(&format!(
            "{{\"type\":\"attachment\",\"blob\":\"{}\"}}\n",
            "x".repeat((FACTS_TAIL_BYTES + 4096) as usize)
        ));
        std::fs::write(&path, contents).unwrap();
        let facts = latest_recorded_session_facts(&path).expect("facts");
        assert_eq!(
            facts.permission_mode.as_deref(),
            Some("plan"),
            "head fallback covers a fact-less tail window"
        );
        assert_eq!(facts.cwd.as_deref(), Some("/repo"));
    }

    /// Layer precedence inside the hydrator: the recorded launch config
    /// beats the transcript per field; the transcript fills the rest;
    /// stamps state each field's actual source (config mtime vs record
    /// time), and cache/context only ever derive from the transcript.
    #[test]
    fn layer_merge_config_beats_transcript_per_field() {
        let recorded = RecordedSessionFacts {
            model: Some("claude-haiku-4-5".into()),
            effort: Some("low".into()),
            permission_mode: Some("default".into()),
            usage: Some(RecordedUsage {
                input_tokens: 2,
                cache_read_tokens: 55_796,
                cache_creation_tokens: 3_240,
                output_tokens: 521,
                ephemeral_5m_tokens: 0,
                ephemeral_1h_tokens: 3_240,
                observed_at_epoch: Some(1_000_000),
            }),
            cwd: Some("/repo".into()),
            as_of_epoch: Some(1_000_000),
        };
        // Config states model + mode; effort comes from the transcript.
        let config = SessionAgentConfig {
            claude_model: Some("claude-fable-5".into()),
            claude_permission_mode: Some("bypassPermissions".into()),
            ..Default::default()
        };
        let facts = recovered_facts_from_layers(
            &AgentBackend::ClaudeCode,
            Some(&config),
            Some(2_000_000),
            Some(&recorded),
        );
        assert_eq!(facts.source.as_deref(), Some("claude-code"));
        assert_eq!(facts.config.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(
            facts.config.model_recovered_at_epoch,
            Some(2_000_000),
            "config-sourced model stamps with the config's as-of"
        );
        assert_eq!(facts.config.effort.as_deref(), Some("low"));
        assert_eq!(
            facts.config.permission_mode.as_deref(),
            Some("bypassPermissions")
        );
        assert_eq!(
            facts.config.permission_kind.as_deref(),
            Some(intendant_core::vitals::PERMISSION_KIND_BYPASS)
        );
        assert!(!facts.config.permission_echoed);
        assert_eq!(facts.config.permission_recovered_at_epoch, Some(2_000_000));

        // Cache: 55796 / (55796+3240+2) floor = 94%; 1h split → 3600.
        let cache = facts.cache.as_ref().expect("cache");
        assert_eq!(cache.hit_pct, Some(94));
        assert_eq!(cache.ttl_seconds, Some(3600));
        assert_eq!(cache.last_activity_epoch, 1_000_000);
        assert_eq!(cache.recovered_at_epoch, Some(1_000_000));

        // Context: fable-5 → 1M window from the model-family table.
        let context = facts.context.as_ref().expect("context");
        assert_eq!(context.tokens_used, 2 + 55_796 + 3_240 + 521);
        assert_eq!(context.context_window, 1_000_000);
        assert!(context.usage_pct > 5.9 && context.usage_pct < 6.0);
        assert_eq!(context.observed_at_epoch, 1_000_000);
        assert_eq!(context.recovered_at_epoch, Some(1_000_000));

        // Transcript-only merge (no config): transcript fields stamped
        // with the record time.
        let facts =
            recovered_facts_from_layers(&AgentBackend::ClaudeCode, None, None, Some(&recorded));
        assert_eq!(facts.config.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(facts.config.model_recovered_at_epoch, Some(1_000_000));
        assert_eq!(facts.config.permission_mode.as_deref(), Some("default"));
        assert_eq!(
            facts.config.permission_kind.as_deref(),
            Some(intendant_core::vitals::PERMISSION_KIND_ASK)
        );

        // Resume lane (config as-of None): config-sourced fields carry no
        // stamp — current intent, not a disk snapshot.
        let facts = recovered_facts_from_layers(
            &AgentBackend::ClaudeCode,
            Some(&config),
            None,
            Some(&recorded),
        );
        assert_eq!(facts.config.model_recovered_at_epoch, None);
        assert_eq!(facts.config.effort.as_deref(), Some("low"));
    }

    /// A footprint above the family window clamps by widening the
    /// effective window (the live meter's rule) — never >100%.
    #[test]
    fn recovered_context_clamps_against_the_family_window() {
        let usage = RecordedUsage {
            input_tokens: 250_000,
            output_tokens: 1_000,
            observed_at_epoch: Some(5),
            ..Default::default()
        };
        let context = recovered_context_vitals(&usage, "claude-haiku-4-5").expect("context");
        assert_eq!(context.context_window, 251_000, "window widened to fit");
        assert!((context.usage_pct - 100.0).abs() < f64::EPSILON);
        // Unknown model family: the 200k default applies.
        let small = RecordedUsage {
            input_tokens: 50_000,
            observed_at_epoch: Some(5),
            ..Default::default()
        };
        let context = recovered_context_vitals(&small, "mystery-model").expect("context");
        assert_eq!(context.context_window, 200_000);
        assert!((context.usage_pct - 25.0).abs() < 0.01);
    }

    /// Codex/Kimi/Pi launch-config layers map their own vocabulary
    /// (sandbox·approval join, thinking-as-effort, Pi's fixed Intendant
    /// approval gate) with recovered stamps; no transcript layer exists
    /// for them yet.
    #[test]
    fn layer_merge_codex_kimi_and_pi_config_layers() {
        let config = SessionAgentConfig {
            codex_model: Some("gpt-5.5-codex".into()),
            codex_reasoning_effort: Some("high".into()),
            codex_sandbox: Some("workspace-write".into()),
            codex_approval_policy: Some("on-request".into()),
            ..Default::default()
        };
        let facts = recovered_facts_from_layers(&AgentBackend::Codex, Some(&config), Some(7), None);
        assert_eq!(facts.source.as_deref(), Some("codex"));
        assert_eq!(facts.config.model.as_deref(), Some("gpt-5.5-codex"));
        assert_eq!(facts.config.effort.as_deref(), Some("high"));
        assert_eq!(
            facts.config.permission_mode.as_deref(),
            Some("workspace-write · on-request")
        );
        assert_eq!(
            facts.config.permission_kind.as_deref(),
            Some(intendant_core::vitals::PERMISSION_KIND_AUTO_SANDBOXED)
        );
        assert_eq!(facts.config.permission_recovered_at_epoch, Some(7));
        assert!(facts.cache.is_none());
        assert!(facts.context.is_none());

        let config = SessionAgentConfig {
            kimi_model: Some("kimi-k2.5".into()),
            kimi_thinking: Some("on".into()),
            kimi_permission_mode: Some("yolo".into()),
            ..Default::default()
        };
        let facts = recovered_facts_from_layers(&AgentBackend::Kimi, Some(&config), Some(9), None);
        assert_eq!(facts.source.as_deref(), Some("kimi"));
        assert_eq!(facts.config.model.as_deref(), Some("kimi-k2.5"));
        assert_eq!(facts.config.effort.as_deref(), Some("on"));
        assert_eq!(
            facts.config.permission_kind.as_deref(),
            Some(intendant_core::vitals::PERMISSION_KIND_BYPASS)
        );

        let config = SessionAgentConfig {
            pi_model: Some("openai-codex/gpt-5.6-sol".into()),
            pi_thinking: Some("high".into()),
            ..Default::default()
        };
        let facts = recovered_facts_from_layers(&AgentBackend::Pi, Some(&config), Some(11), None);
        assert_eq!(facts.source.as_deref(), Some("pi"));
        assert_eq!(
            facts.config.model.as_deref(),
            Some("openai-codex/gpt-5.6-sol")
        );
        assert_eq!(facts.config.effort.as_deref(), Some("high"));
        assert_eq!(
            facts.config.permission_mode.as_deref(),
            Some(crate::external_agent::pi::PI_PERMISSION_MODE)
        );
        assert_eq!(
            facts.config.permission_kind.as_deref(),
            Some(intendant_core::vitals::PERMISSION_KIND_ASK)
        );
        assert!(!facts.config.permission_echoed);
        assert_eq!(facts.config.model_recovered_at_epoch, Some(11));
        assert_eq!(facts.config.permission_recovered_at_epoch, Some(11));

        // No layers at all: only the source remains, and the emission is
        // still useful (account-limit membership).
        let facts = recovered_facts_from_layers(&AgentBackend::Codex, None, None, None);
        assert!(!facts.is_empty());
        assert_eq!(facts.source.as_deref(), Some("codex"));
        assert_eq!(facts.config, SessionConfigVitals::default());
    }

    fn recovered_cc_facts(as_of: u64) -> RecoveredSessionFacts {
        RecoveredSessionFacts {
            source: Some("claude-code".into()),
            config: SessionConfigVitals {
                model: Some("claude-fable-5".into()),
                effort: Some("xhigh".into()),
                permission_mode: Some("bypassPermissions".into()),
                permission_kind: Some(intendant_core::vitals::PERMISSION_KIND_BYPASS.into()),
                permission_echoed: false,
                model_recovered_at_epoch: Some(as_of),
                permission_recovered_at_epoch: Some(as_of),
            },
            cache: Some(SessionCacheVitals {
                hit_pct: Some(94),
                last_activity_epoch: as_of,
                ttl_seconds: Some(3600),
                recovered_at_epoch: Some(as_of),
            }),
            context: Some(SessionContextVitals {
                tokens_used: 59_559,
                context_window: 1_000_000,
                usage_pct: 5.9559,
                observed_at_epoch: as_of,
                recovered_at_epoch: Some(as_of),
            }),
        }
    }

    /// Recovered-then-live: the hydrator fills empty sections (stamped),
    /// and every later live fold overwrites its field and clears the
    /// stamp — the race is harmless in this order.
    #[tokio::test]
    async fn recovered_then_live_overwrites_and_clears_stamps() {
        let bus = EventBus::new();
        let hub = SessionVitalsHub::new(bus.clone());
        let _listener = spawn_cache_vitals_listener(bus.clone(), hub.clone());
        let mut rx = bus.subscribe();
        let deadline = std::time::Duration::from_secs(5);

        async fn wait_vitals(
            rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
        ) -> crate::types::SessionVitals {
            loop {
                if let Ok(AppEvent::SessionVitals { vitals, .. }) = rx.recv().await {
                    return vitals;
                }
            }
        }

        bus.send(AppEvent::SessionRecoveredFacts {
            session_id: Some("cc-1".into()),
            facts: Box::new(recovered_cc_facts(1_000_000)),
        });
        let vitals = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("recovered emission");
        let config = vitals.config.as_ref().expect("config");
        assert_eq!(config.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(config.model_recovered_at_epoch, Some(1_000_000));
        assert_eq!(config.permission_recovered_at_epoch, Some(1_000_000));
        assert!(!config.permission_echoed);
        assert_eq!(
            vitals.context.as_ref().and_then(|c| c.recovered_at_epoch),
            Some(1_000_000)
        );

        // Live model echo: overwrites the model, clears ITS stamp only.
        bus.send(AppEvent::SessionConfigFacts {
            session_id: Some("cc-1".into()),
            facts: SessionConfigVitals {
                model: Some("claude-mythos-5".into()),
                ..Default::default()
            },
        });
        let vitals = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("model echo emission");
        let config = vitals.config.as_ref().expect("config");
        assert_eq!(config.model.as_deref(), Some("claude-mythos-5"));
        assert_eq!(config.model_recovered_at_epoch, None, "live fold clears");
        assert_eq!(
            config.permission_recovered_at_epoch,
            Some(1_000_000),
            "untouched fields keep their stamps"
        );

        // Live permission echo: clears the permission stamp and vouches.
        bus.send(AppEvent::SessionConfigFacts {
            session_id: Some("cc-1".into()),
            facts: SessionConfigVitals {
                permission_mode: Some("bypassPermissions".into()),
                permission_kind: Some(intendant_core::vitals::PERMISSION_KIND_BYPASS.into()),
                permission_echoed: true,
                ..Default::default()
            },
        });
        let vitals = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("mode echo emission");
        let config = vitals.config.as_ref().expect("config");
        assert!(config.permission_echoed);
        assert_eq!(config.permission_recovered_at_epoch, None);

        // Live usage: overwrites cache AND context, clearing both stamps.
        bus.send(AppEvent::UsageSnapshot {
            session_id: Some("cc-1".into()),
            main: crate::frontend::ModelUsageSnapshot {
                provider: "anthropic".into(),
                model: "claude-mythos-5".into(),
                tokens_used: 70_000,
                context_window: 1_000_000,
                hard_context_window: Some(1_000_000),
                usage_pct: 7.0,
                prompt_tokens: 69_000,
                completion_tokens: 1_000,
                cached_tokens: 60_000,
                cache_creation_tokens: 2_000,
                last_cache_read_tokens: 60_000,
                last_cache_creation_tokens: 2_000,
                last_uncached_input_tokens: 7_000,
                cache_ttl_seconds: Some(300),
                limits: Vec::new(),
            },
            presence: None,
        });
        let vitals = tokio::time::timeout(deadline, wait_vitals(&mut rx))
            .await
            .expect("usage emission");
        let context = vitals.context.as_ref().expect("context");
        assert_eq!(context.tokens_used, 70_000);
        assert_eq!(context.recovered_at_epoch, None, "live usage clears");
        assert!((context.usage_pct - 7.0).abs() < f64::EPSILON);
        assert_eq!(
            vitals.cache.as_ref().and_then(|c| c.recovered_at_epoch),
            None,
            "live cache fold clears"
        );
    }

    /// Live-then-recovered: everything a live producer stated survives
    /// the hydrator untouched (fill-if-absent) — the race is harmless in
    /// this order too. Only genuinely absent fields fill.
    #[tokio::test]
    async fn live_then_recovered_fills_only_absent_fields() {
        let bus = EventBus::new();
        let hub = SessionVitalsHub::new(bus.clone());

        // Live launch facts: model + mode (no effort — CC 2.1.2xx echoes
        // none), live usage context.
        hub.apply_config_facts(
            "cc-1",
            SessionConfigVitals {
                model: Some("claude-mythos-5".into()),
                permission_mode: Some("plan".into()),
                permission_kind: Some(intendant_core::vitals::PERMISSION_KIND_PLAN.into()),
                permission_echoed: true,
                ..Default::default()
            },
        );
        hub.apply("cc-1", |vitals| {
            vitals.context = Some(SessionContextVitals {
                tokens_used: 70_000,
                context_window: 1_000_000,
                usage_pct: 7.0,
                observed_at_epoch: 2_000_000,
                recovered_at_epoch: None,
            });
        });

        hub.apply_recovered_facts("cc-1", recovered_cc_facts(1_000_000));

        let vitals = hub
            .sessions
            .lock()
            .expect("vitals state lock")
            .get("cc-1")
            .cloned()
            .expect("entry");
        let config = vitals.config.as_ref().expect("config");
        assert_eq!(
            config.model.as_deref(),
            Some("claude-mythos-5"),
            "live model survives the hydrator"
        );
        assert_eq!(config.model_recovered_at_epoch, None);
        assert_eq!(config.permission_mode.as_deref(), Some("plan"));
        assert!(config.permission_echoed, "live voucher survives");
        assert_eq!(config.permission_recovered_at_epoch, None);
        // Effort was absent live → the recovered value fills.
        assert_eq!(config.effort.as_deref(), Some("xhigh"));
        // Context was live → recovered context ignored.
        let context = vitals.context.as_ref().expect("context");
        assert_eq!(context.tokens_used, 70_000);
        assert_eq!(context.recovered_at_epoch, None);
        // Cache was absent → recovered cache fills, stamped.
        assert_eq!(
            vitals.cache.as_ref().and_then(|c| c.recovered_at_epoch),
            Some(1_000_000)
        );
    }

    /// The account limit store round-trips through its state file,
    /// tolerates unknown fields (additive forward-compat), fails closed
    /// on a future version, and never invents percentages.
    #[test]
    fn limit_store_round_trip_and_forward_compat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-rate-limits.json");
        let mut accounts: HashMap<String, BTreeMap<String, SessionLimitWindow>> = HashMap::new();
        let mut store = BTreeMap::new();
        store.insert(
            "5h".to_string(),
            SessionLimitWindow {
                label: "5h".into(),
                used_pct: None,
                resets_at_epoch: Some(1_784_503_200),
                status: Some("allowed_warning".into()),
                observed_at_epoch: Some(1_784_499_000),
            },
        );
        accounts.insert("claude-code".to_string(), store);
        persist_account_limit_store(&path, &accounts).expect("persist");

        let loaded = load_account_limit_store(&path);
        assert_eq!(loaded, accounts, "round-trip is lossless");
        let window = &loaded["claude-code"]["5h"];
        assert_eq!(window.used_pct, None, "unreported percentage stays absent");
        assert_eq!(window.observed_at_epoch, Some(1_784_499_000));

        // Additive forward-compat: unknown fields anywhere are ignored.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value["future_top_level"] = serde_json::json!({"x": 1});
        value["accounts"]["claude-code"][0]["futureField"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        let loaded = load_account_limit_store(&path);
        assert_eq!(loaded["claude-code"]["5h"].label, "5h");

        // A future breaking version fails closed to empty.
        value["version"] = serde_json::json!(2);
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        assert!(load_account_limit_store(&path).is_empty());

        // Missing / corrupt files read as empty.
        assert!(load_account_limit_store(&dir.path().join("gone.json")).is_empty());
        std::fs::write(&path, "{").unwrap();
        assert!(load_account_limit_store(&path).is_empty());
    }

    /// End-to-end persistence: a report through one hub lands in the
    /// state file; a second hub restores it at construction; recovered
    /// facts with a source mirror the restored account view into the
    /// session — and the restored windows keep their honest stamps.
    #[tokio::test]
    async fn limit_store_survives_a_hub_restart_and_mirrors_to_recovered_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-rate-limits.json");
        let bus = EventBus::new();
        let hub = SessionVitalsHub::with_limit_store(bus.clone(), Some(path.clone()));
        // Session of the claude-code account reports a window.
        hub.apply_recovered_facts(
            "cc-1",
            RecoveredSessionFacts {
                source: Some("claude-code".into()),
                ..Default::default()
            },
        );
        let window = SessionLimitWindow {
            label: "7d".into(),
            used_pct: None,
            resets_at_epoch: Some(1_784_900_000),
            status: Some("allowed".into()),
            observed_at_epoch: Some(1_784_499_000),
        };
        hub.apply_rate_limit_windows("cc-1", vec![window.clone()]);
        // The persist may ride the blocking pool: wait for the file.
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(path.exists(), "report persists the store");

        // Restart: a fresh hub restores the account view; a restored
        // session inherits it via recovered-facts membership.
        let bus2 = EventBus::new();
        let hub2 = SessionVitalsHub::with_limit_store(bus2.clone(), Some(path.clone()));
        hub2.apply_recovered_facts(
            "cc-restored",
            RecoveredSessionFacts {
                source: Some("claude-code".into()),
                ..Default::default()
            },
        );
        let vitals = hub2
            .sessions
            .lock()
            .expect("vitals state lock")
            .get("cc-restored")
            .cloned()
            .expect("restored entry");
        assert_eq!(vitals.limits, vec![window]);
    }

    /// Boot restore: both lanes (git-target registration and vitals
    /// hydration) run over one candidate walk — identical scope and cap —
    /// and a Claude Code candidate hydrates from its recorded launch
    /// config + transcript, seeding the git locus from the recorded cwd.
    #[tokio::test]
    async fn boot_restore_registers_and_hydrates_over_one_capped_walk() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");

        // The transcript the wrapper maps to, in the real store shape.
        let project_dir = home.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(&project_dir).unwrap();
        // Recorded cwd points at a checkout (worktree) the meta root
        // doesn't know about.
        let worktree = home.path().join("repo-wt");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let transcript = project_dir.join("backend-cc-9.jsonl");
        std::fs::write(
            &transcript,
            [
                user_line(
                    "acceptEdits",
                    worktree.to_str().unwrap(),
                    "2026-07-22T07:00:00Z",
                ),
                assistant_line("claude-fable-5", "max", "2026-07-22T07:10:00Z", false).replace(
                    "\"cwd\":\"/repo\"",
                    &format!("\"cwd\":\"{}\"", worktree.display()),
                ),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();

        // Wrapper session dir: meta + launch config + index mapping.
        let wrapper_dir = logs.join("wrap-cc");
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrap-cc",
                "created_at": "now",
                "project_root": home.path().join("repo").to_str().unwrap(),
                "status": "idle",
            })
            .to_string(),
        )
        .unwrap();
        crate::session_config::write_log_dir_config(
            &wrapper_dir,
            &SessionAgentConfig {
                source: Some("claude-code".into()),
                claude_model: Some("claude-fable-5".into()),
                claude_permission_mode: Some("acceptEdits".into()),
                ..Default::default()
            },
        )
        .expect("write config");
        crate::external_wrapper_index::upsert(
            home.path(),
            "claude-code",
            "backend-cc-9",
            "wrap-cc",
            &wrapper_dir,
            None,
        )
        .expect("index upsert");

        // A completed session: excluded by both lanes.
        let done_dir = logs.join("wrap-done");
        std::fs::create_dir_all(&done_dir).unwrap();
        std::fs::write(
            done_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrap-done",
                "created_at": "now",
                "project_root": "/x",
                "status": "completed",
            })
            .to_string(),
        )
        .unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let registry = GitVitalsTargets::default();
        let (registered, hydrated) = restore_session_vitals_at_boot(home.path(), &registry, &bus);
        assert_eq!(registered, 1, "completed sessions stay out");
        assert_eq!(hydrated, 1);

        // Parity with the registration-only wrapper over the same walk.
        let twin = GitVitalsTargets::default();
        assert_eq!(
            crate::session_vitals::register_restored_session_targets(home.path(), &twin),
            registered
        );
        let mut ids: Vec<String> = registry.snapshot().into_iter().map(|(id, _)| id).collect();
        let mut twin_ids: Vec<String> = twin.snapshot().into_iter().map(|(id, _)| id).collect();
        ids.sort();
        twin_ids.sort();
        assert_eq!(ids, twin_ids, "both lanes cover the same sessions");
        assert_eq!(
            registry.snapshot()[0].1,
            worktree,
            "recorded cwd seeds the probe locus at boot"
        );

        let event = rx.try_recv().expect("hydration event");
        let AppEvent::SessionRecoveredFacts { session_id, facts } = event else {
            panic!("unexpected event: {event:?}");
        };
        assert_eq!(session_id.as_deref(), Some("wrap-cc"));
        assert_eq!(facts.source.as_deref(), Some("claude-code"));
        assert_eq!(facts.config.model.as_deref(), Some("claude-fable-5"));
        assert!(
            facts.config.model_recovered_at_epoch.is_some(),
            "boot-lane config facts are disk-stamped"
        );
        assert_eq!(facts.config.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(
            facts.config.effort.as_deref(),
            Some("max"),
            "transcript fills effort"
        );
        assert!(
            facts.context.is_some(),
            "transcript usage recovers the meter"
        );
    }

    /// Boot-cap parity: past the cap, both lanes restore exactly the same
    /// (newest) candidate set.
    #[test]
    fn boot_walk_cap_applies_to_both_lanes() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");
        let total = RESTORED_TARGET_CAP + 3;
        for index in 0..total {
            let dir = logs.join(format!("sess-{index:03}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("session_meta.json"),
                serde_json::json!({
                    "session_id": format!("sess-{index:03}"),
                    "created_at": "now",
                    "project_root": "/x",
                    "status": "idle",
                })
                .to_string(),
            )
            .unwrap();
        }
        let candidates = restored_session_candidates(home.path());
        assert_eq!(candidates.len(), RESTORED_TARGET_CAP);
        let bus = EventBus::new();
        let registry = GitVitalsTargets::default();
        let (registered, hydrated) = restore_session_vitals_at_boot(home.path(), &registry, &bus);
        assert_eq!(registered, RESTORED_TARGET_CAP);
        assert_eq!(hydrated, 0, "native sessions have no disk layer to hydrate");
        let twin = GitVitalsTargets::default();
        assert_eq!(
            crate::session_vitals::register_restored_session_targets(home.path(), &twin),
            RESTORED_TARGET_CAP
        );
    }

    /// The resume hydration lane: transcript + effective config merge and
    /// emit under the lane's vitals id (sync context exercises the
    /// no-runtime fallback path). Config-layer fields arrive unstamped
    /// (current intent); transcript-sourced fields keep their stamps.
    #[test]
    fn resume_hydration_emits_merged_facts_under_the_given_id() {
        let home = tempfile::tempdir().unwrap();
        let project_dir = home.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("backend-cc-7.jsonl"),
            assistant_line("claude-fable-5", "xhigh", "2026-07-22T07:00:00Z", false) + "\n",
        )
        .unwrap();

        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        hydrate_resumed_session_vitals(
            bus.clone(),
            home.path().to_path_buf(),
            "backend-cc-7".into(),
            AgentBackend::ClaudeCode,
            "backend-cc-7".into(),
            Some(SessionAgentConfig {
                source: Some("claude-code".into()),
                claude_effort: Some("low".into()),
                ..Default::default()
            }),
        );
        let event = rx.try_recv().expect("hydration event");
        let AppEvent::SessionRecoveredFacts { session_id, facts } = event else {
            panic!("unexpected event: {event:?}");
        };
        assert_eq!(session_id.as_deref(), Some("backend-cc-7"));
        assert_eq!(
            facts.config.effort.as_deref(),
            Some("low"),
            "effective config beats the transcript's effort"
        );
        assert_eq!(facts.config.model.as_deref(), Some("claude-fable-5"));
        assert!(
            facts.config.model_recovered_at_epoch.is_some(),
            "transcript-sourced model keeps its record stamp"
        );
        assert!(facts.cache.is_some());
        assert!(facts.context.is_some());
    }
}
