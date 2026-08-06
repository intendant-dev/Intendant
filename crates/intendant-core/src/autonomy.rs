use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Global autonomy level controlling how much user approval is needed.
/// Serialized lowercase (`low`/`medium`/`high`/`full`) — the dial
/// vocabulary's closed form; an unknown word refuses at deserialization.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Ask for every category except file reads; Deny rules still gate.
    Low,
    /// Default. Apply category rules; arbitrary shell execution inherits
    /// the strictest rule of every effect the shell can reach.
    #[default]
    Medium,
    /// Auto-approve ordinary Ask rules; Deny rules and hard gates still gate.
    High,
    /// Auto-approve everything except HumanInput and LiveAudioSpawn.
    Full,
}

impl AutonomyLevel {
    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" | "l" | "0" => Self::Low,
            "medium" | "med" | "m" | "1" => Self::Medium,
            "high" | "h" | "2" => Self::High,
            "full" | "f" | "3" => Self::Full,
            _ => Self::Medium,
        }
    }
}

impl fmt::Display for AutonomyLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "Low"),
            Self::Medium => write!(f, "Medium"),
            Self::High => write!(f, "High"),
            Self::Full => write!(f, "Full"),
        }
    }
}

/// Categories of actions that the agent can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCategory {
    FileRead,
    FileWrite,
    #[allow(dead_code)]
    FileDelete,
    CommandExec,
    NetworkRequest,
    Destructive,
    HumanInput,
    /// Spawning an untrusted live audio sub-agent.
    LiveAudioSpawn,
    /// Accessing the user's session display (screenshot, mouse, keyboard).
    DisplayControl,
    /// An external agent invoking a tool / MCP call (e.g. Codex calling
    /// Intendant's own MCP server tools, or an MCP elicitation request).
    ToolCall,
}

impl ActionCategory {
    /// Return a severity score for display priority ordering.
    /// Higher = more severe. Used to show the most important category
    /// in approval prompts when multiple categories apply.
    pub fn severity(self) -> u8 {
        match self {
            Self::FileRead => 0,
            Self::ToolCall => 1,
            Self::NetworkRequest => 2,
            Self::FileWrite => 3,
            Self::FileDelete => 4,
            Self::Destructive => 5,
            // A shell can perform every ordinary runtime effect, even when
            // the best-effort classifier cannot recognize the spelling.
            Self::CommandExec => 6,
            Self::HumanInput => 7,
            Self::LiveAudioSpawn => 8,
            Self::DisplayControl => 9,
        }
    }
}

/// Parse error for an unrecognized [`ActionCategory`] name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownActionCategory;

impl fmt::Display for UnknownActionCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown action category")
    }
}

impl std::error::Error for UnknownActionCategory {}

/// Inverse of `Display`: parse the lowercase snake-case category name
/// back into a variant (`s.parse().ok()` for Option ergonomics). Used
/// by session-log replay to reconstruct `ApprovalRequired` events from
/// persisted approval rows.
impl std::str::FromStr for ActionCategory {
    type Err = UnknownActionCategory;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "file_read" => Ok(Self::FileRead),
            "file_write" => Ok(Self::FileWrite),
            "file_delete" => Ok(Self::FileDelete),
            "command_exec" => Ok(Self::CommandExec),
            "network" => Ok(Self::NetworkRequest),
            "destructive" => Ok(Self::Destructive),
            "human_input" => Ok(Self::HumanInput),
            "live_audio_spawn" => Ok(Self::LiveAudioSpawn),
            "display_control" => Ok(Self::DisplayControl),
            "tool_call" => Ok(Self::ToolCall),
            _ => Err(UnknownActionCategory),
        }
    }
}

impl fmt::Display for ActionCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileRead => write!(f, "file_read"),
            Self::FileWrite => write!(f, "file_write"),
            Self::FileDelete => write!(f, "file_delete"),
            Self::CommandExec => write!(f, "command_exec"),
            Self::NetworkRequest => write!(f, "network"),
            Self::Destructive => write!(f, "destructive"),
            Self::HumanInput => write!(f, "human_input"),
            Self::LiveAudioSpawn => write!(f, "live_audio_spawn"),
            Self::DisplayControl => write!(f, "display_control"),
            Self::ToolCall => write!(f, "tool_call"),
        }
    }
}

/// Per-category approval rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ApprovalRule {
    Auto,
    #[default]
    Ask,
    Deny,
}

impl ApprovalRule {
    /// Canonical lowercase string form, matching the TOML / serde
    /// representation (`auto` / `ask` / `deny`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    /// Parse a user-supplied rule string. Case-insensitive; returns `None`
    /// for anything that isn't `auto` / `ask` / `deny`.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    fn strictest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Ask, _) | (_, Self::Ask) => Self::Ask,
            (Self::Auto, Self::Auto) => Self::Auto,
        }
    }
}

/// Category-level approval rules parsed from intendant.toml [approval] section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalConfig {
    #[serde(default = "default_auto")]
    pub file_read: ApprovalRule,
    #[serde(default)]
    pub file_write: ApprovalRule,
    #[serde(default)]
    pub file_delete: ApprovalRule,
    #[serde(default = "default_auto")]
    pub command_exec: ApprovalRule,
    #[serde(default = "default_auto")]
    pub network: ApprovalRule,
    #[serde(default)]
    pub destructive: ApprovalRule,
    #[serde(default)]
    pub display_control: ApprovalRule,
    /// Controller-dispatched and external-agent tool / MCP calls. Defaults
    /// to `Ask`: external MCP servers are an untrusted instruction/data
    /// boundary, so Medium autonomy must not silently cross it.
    #[serde(default)]
    pub tool_call: ApprovalRule,
}

fn default_auto() -> ApprovalRule {
    ApprovalRule::Auto
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            file_read: ApprovalRule::Auto,
            file_write: ApprovalRule::Ask,
            file_delete: ApprovalRule::Ask,
            command_exec: ApprovalRule::Auto,
            network: ApprovalRule::Auto,
            destructive: ApprovalRule::Ask,
            display_control: ApprovalRule::Ask,
            tool_call: ApprovalRule::Ask,
        }
    }
}

impl ApprovalConfig {
    /// Effective rule for arbitrary shell execution.
    ///
    /// Shell syntax is an open-ended capability: variable indirection,
    /// command substitution, interpreters, and newly installed binaries make
    /// it impossible for substring classification to prove which effects a
    /// command can reach. Govern the capability by the strictest configured
    /// rule for its reachable effects instead. The classifier can still add
    /// useful labels, but an unrecognized spelling cannot weaken policy.
    fn shell_rule(&self) -> ApprovalRule {
        [
            self.command_exec,
            self.file_read,
            self.file_write,
            self.file_delete,
            self.network,
            self.destructive,
            self.display_control,
        ]
        .into_iter()
        .fold(ApprovalRule::Auto, ApprovalRule::strictest)
    }

    /// Set the rule for a category by its snake-case name (the
    /// `ApprovalConfig` field name / `ActionCategory` Display form).
    /// Categories that are always "ask" (`human_input`, `live_audio_spawn`)
    /// have no backing field and are ignored. Returns `true` if a field was
    /// updated.
    pub fn set_rule_by_name(&mut self, category: &str, rule: ApprovalRule) -> bool {
        match category {
            "file_read" => self.file_read = rule,
            "file_write" => self.file_write = rule,
            "file_delete" => self.file_delete = rule,
            "command_exec" => self.command_exec = rule,
            "network" => self.network = rule,
            "destructive" => self.destructive = rule,
            "display_control" => self.display_control = rule,
            "tool_call" => self.tool_call = rule,
            _ => return false,
        }
        true
    }

    pub fn rule_for(&self, category: ActionCategory) -> ApprovalRule {
        match category {
            ActionCategory::FileRead => self.file_read,
            ActionCategory::FileWrite => self.file_write,
            ActionCategory::FileDelete => self.file_delete,
            ActionCategory::CommandExec => self.shell_rule(),
            ActionCategory::NetworkRequest => self.network,
            ActionCategory::Destructive => self.destructive,
            ActionCategory::HumanInput => ApprovalRule::Ask, // always ask
            ActionCategory::LiveAudioSpawn => ApprovalRule::Ask, // always ask
            ActionCategory::DisplayControl => self.display_control,
            ActionCategory::ToolCall => self.tool_call,
        }
    }
}

/// Ask-propensity grade (Track AD, owner-ratified vocabulary). A TAUGHT
/// posture, not a permission wall: the daemon's hard parts are recording,
/// delivery, and routing — it cannot compel a model to ask. Each grade
/// includes the previous grade's asking classes:
/// `autopilot` — no discretionary questions, best-judgment throughout;
/// `escalate` (default) — owner-only decision classes park a non-blocking
/// question with a committed recommendation;
/// `checkpoint` — escalate + park before sealing named direction choices;
/// `confer` — checkpoint + a blocking ask (bounded wait) at direction
/// choices. S1 records and carries the grade; the per-grade teaching
/// assembly is S2.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AskGrade {
    Autopilot,
    #[default]
    Escalate,
    Checkpoint,
    Confer,
}

impl AskGrade {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Autopilot => "autopilot",
            Self::Escalate => "escalate",
            Self::Checkpoint => "checkpoint",
            Self::Confer => "confer",
        }
    }
}

/// Notification send-appetite (Track AD). A ceiling on the session's own
/// `notify_user` loudness, always UNDER owner delivery policy (quiet
/// hours, per-item reminder levels, nudge cooldowns stay owner-side):
/// `quiet` clamps every notification to info (still delivered, still in
/// the transcript — only loudness changes, and the clamp is named in the
/// tool response); `normal` is today's behavior verbatim; `eager` adds no
/// ceiling (its taught milestone-updates invitation is S2).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum NotifyAppetite {
    Quiet,
    #[default]
    Normal,
    Eager,
}

impl NotifyAppetite {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Normal => "normal",
            Self::Eager => "eager",
        }
    }
}

/// Per-category approval-rule overrides for one session — the
/// `ApprovalConfig` vocabulary with every field optional (`None` inherits
/// the live global rule at decision time). Field names mirror
/// `ApprovalConfig` exactly; `human_input` and `live_audio_spawn` remain
/// hard gates with no backing field at any scope. Unknown keys refuse the
/// whole dial (`deny_unknown_fields`): a declaration the daemon cannot
/// enforce must not ride an owner approval.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ApprovalOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_read: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_write: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_delete: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_exec: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_control: Option<ApprovalRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ApprovalRule>,
}

impl ApprovalOverrides {
    pub fn is_empty(&self) -> bool {
        let Self {
            file_read,
            file_write,
            file_delete,
            command_exec,
            network,
            destructive,
            display_control,
            tool_call,
        } = self;
        file_read.is_none()
            && file_write.is_none()
            && file_delete.is_none()
            && command_exec.is_none()
            && network.is_none()
            && destructive.is_none()
            && display_control.is_none()
            && tool_call.is_none()
    }
}

/// One field of the session-layer merge: the override wins over the live
/// global rule EXCEPT across a live global `deny` — the deny floor is
/// absolute and read at decision time, so a category deny the owner sets
/// bites every running session, dial-bearing ones included. Tightening
/// (any → deny, auto → ask) is always allowed.
fn apply_rule_override(base: ApprovalRule, over: Option<ApprovalRule>) -> ApprovalRule {
    match (base, over) {
        (ApprovalRule::Deny, _) => ApprovalRule::Deny,
        (base, None) => base,
        (_, Some(rule)) => rule,
    }
}

impl ApprovalConfig {
    /// The live global rules with one session's overrides applied,
    /// per-category most-specific-present, under the absolute deny floor
    /// ([`apply_rule_override`]). Shell composition (`shell_rule`) then
    /// runs over the MERGED per-category rules, so CommandExec's
    /// strictest-reachable-effect law sees the session's effective world.
    pub fn with_overrides(&self, overrides: &ApprovalOverrides) -> ApprovalConfig {
        ApprovalConfig {
            file_read: apply_rule_override(self.file_read, overrides.file_read),
            file_write: apply_rule_override(self.file_write, overrides.file_write),
            file_delete: apply_rule_override(self.file_delete, overrides.file_delete),
            command_exec: apply_rule_override(self.command_exec, overrides.command_exec),
            network: apply_rule_override(self.network, overrides.network),
            destructive: apply_rule_override(self.destructive, overrides.destructive),
            display_control: apply_rule_override(self.display_control, overrides.display_control),
            tool_call: apply_rule_override(self.tool_call, overrides.tool_call),
        }
    }
}

/// The scoped session-dial carrier (Track AD S1): four optional knobs, each
/// a closed typed vocabulary. `None` knobs inherit the LIVE global layer at
/// decision time; `Some` knobs are owner-signed sticky overrides pinned for
/// the session's life (mid-session re-dial is deliberately not vocabulary).
/// Hard gates (HumanInput, LiveAudioSpawn, the display-request rail,
/// question≠permission) stay outside the dial at every scope. TOML table
/// name at every scope: `[dial]`.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DialConfig {
    /// Autonomy-level override for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<AutonomyLevel>,
    /// Ask-propensity grade. Recorded and carried in S1; taught in S2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<AskGrade>,
    /// Per-category approval-rule overrides (the "per action" knob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<ApprovalOverrides>,
    /// Notification send-appetite ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyAppetite>,
}

impl DialConfig {
    /// True when no knob carries a value (an all-`None` dial binds
    /// nothing and is never recorded).
    pub fn is_empty(&self) -> bool {
        let Self {
            autonomy,
            ask,
            approvals,
            notify,
        } = self;
        autonomy.is_none()
            && ask.is_none()
            && approvals.as_ref().is_none_or(|a| a.is_empty())
            && notify.is_none()
    }
}

/// Short human description of a dial's set knobs (for launch/re-attach
/// log lines): `autonomy=full, approvals[network=deny], notify=quiet`.
pub fn describe_dial(dial: &DialConfig) -> String {
    let mut parts = Vec::new();
    if let Some(level) = dial.autonomy {
        parts.push(format!("autonomy={}", level.to_string().to_lowercase()));
    }
    if let Some(grade) = dial.ask {
        parts.push(format!("ask={}", grade.as_str()));
    }
    if let Some(overrides) = dial.approvals.as_ref() {
        let categories: [(&str, Option<ApprovalRule>); 8] = [
            ("file_read", overrides.file_read),
            ("file_write", overrides.file_write),
            ("file_delete", overrides.file_delete),
            ("command_exec", overrides.command_exec),
            ("network", overrides.network),
            ("destructive", overrides.destructive),
            ("display_control", overrides.display_control),
            ("tool_call", overrides.tool_call),
        ];
        let set: Vec<String> = categories
            .into_iter()
            .filter_map(|(name, rule)| rule.map(|rule| format!("{name}={}", rule.as_str())))
            .collect();
        if !set.is_empty() {
            parts.push(format!("approvals[{}]", set.join(", ")));
        }
    }
    if let Some(appetite) = dial.notify {
        parts.push(format!("notify={}", appetite.as_str()));
    }
    if parts.is_empty() {
        "empty".to_string()
    } else {
        parts.join(", ")
    }
}

/// Combined autonomy state shared between the agent loop and TUI.
#[derive(Debug, Clone)]
pub struct AutonomyState {
    pub level: AutonomyLevel,
    pub rules: ApprovalConfig,
    /// Session-level grant for the user's session display.
    /// Once true, `DisplayControl` actions skip the approval prompt.
    pub user_display_granted: bool,
    /// Approved action signatures, bucketed per session id (`None` session
    /// ids share the `""` bucket used by the single-session shapes). One
    /// autonomy state backs every native session of a daemon, so without
    /// the bucket an approval in one session would silence prompts in every
    /// other. Retries of the same action (e.g. with a different display
    /// param) skip the approval prompt; content-bearing signatures (see
    /// [`batch_dedup_source`]) carry a digest so changed content never
    /// rides an old approval.
    pub approved_commands: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Per-session dial overrides (Track AD S1), keyed by session id — the
    /// session OVERRIDE layer of the two-layer cascade. `level`/`rules`
    /// above remain the LIVE global layer: un-overridden knobs read them at
    /// decision time, overridden knobs stay owner-signed-pinned for the
    /// session's life. Recorded only from owner-signed gestures (an
    /// approved manifest, the owner's own create, the card's
    /// session-scoped approve-all) — sessions never write their own dial.
    /// In-memory like every other field; the durable copy lives in the
    /// session's `session_agent_config.json` overlay and re-attaches on
    /// resume/readopt.
    pub session_dials: std::collections::HashMap<String, DialConfig>,
}

impl Default for AutonomyState {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Medium,
            rules: ApprovalConfig::default(),
            user_display_granted: false,
            approved_commands: std::collections::HashMap::new(),
            session_dials: std::collections::HashMap::new(),
        }
    }
}

impl AutonomyState {
    pub fn new(level: AutonomyLevel, rules: ApprovalConfig) -> Self {
        Self {
            level,
            rules,
            user_display_granted: false,
            approved_commands: std::collections::HashMap::new(),
            session_dials: std::collections::HashMap::new(),
        }
    }

    /// The recorded dial override for `session`, if any. `None` session
    /// ids (the single-session shapes) never carry a dial — they are the
    /// whole daemon and read the global layer directly.
    pub fn session_dial(&self, session: Option<&str>) -> Option<&DialConfig> {
        self.session_dials.get(session?)
    }

    /// Record (or extend) a session's dial override layer. An empty dial
    /// is never recorded — absence and emptiness must stay the same state
    /// so dial-less sessions remain byte-identical to today.
    pub fn set_session_dial(&mut self, session: &str, dial: DialConfig) {
        if dial.is_empty() {
            return;
        }
        self.session_dials.insert(session.to_string(), dial);
    }

    /// Escalate exactly one session's autonomy override to Full (the
    /// session-scoped approve-all). Existing knobs on the session's dial
    /// are preserved; the global level and every other session are
    /// untouched.
    pub fn set_session_autonomy_full(&mut self, session: &str) {
        self.session_dials
            .entry(session.to_string())
            .or_default()
            .autonomy = Some(AutonomyLevel::Full);
    }

    /// The session's effective autonomy level: its sticky override when
    /// one was signed, else the LIVE global level. A live global tighten
    /// deliberately does not bite an overridden session (priced in the AD
    /// ruling — the owner's emergency lever is the per-category deny
    /// floor, which always bites; recourse for level is stop/re-dial).
    pub fn effective_level(&self, session: Option<&str>) -> AutonomyLevel {
        self.session_dial(session)
            .and_then(|dial| dial.autonomy)
            .unwrap_or(self.level)
    }

    /// The session's effective per-category rules: the LIVE global rules
    /// with the session's overrides applied under the absolute deny floor
    /// (see [`ApprovalConfig::with_overrides`]).
    pub fn effective_rules(&self, session: Option<&str>) -> ApprovalConfig {
        match self.session_dial(session).and_then(|d| d.approvals.as_ref()) {
            Some(overrides) => self.rules.with_overrides(overrides),
            None => self.rules.clone(),
        }
    }

    /// Effective rule for one category in one session (the deny-precheck
    /// form the consult sites read before [`Self::needs_approval`]).
    pub fn effective_rule_for(&self, session: Option<&str>, category: ActionCategory) -> ApprovalRule {
        self.effective_rules(session).rule_for(category)
    }

    /// The session's effective notify appetite (`normal` when un-dialed).
    pub fn effective_notify(&self, session: Option<&str>) -> NotifyAppetite {
        self.session_dial(session)
            .and_then(|dial| dial.notify)
            .unwrap_or_default()
    }

    /// The session's effective ask grade (`escalate` when un-dialed).
    /// Recorded vocabulary in S1; the grade's taught posture rides S2.
    pub fn effective_ask(&self, session: Option<&str>) -> AskGrade {
        self.session_dial(session)
            .and_then(|dial| dial.ask)
            .unwrap_or_default()
    }

    /// Generate a dedup key for an action.
    ///
    /// Keep the source byte-exact. Display selectors and `$NONCE[...]`
    /// references affect what a command targets, and trying to erase them
    /// without a complete shell parser lets executable syntax hide inside the
    /// ignored span. Structured runtime call nonces are removed earlier by
    /// [`batch_dedup_source`], where they are data rather than shell syntax.
    pub fn command_dedup_key(command: &str) -> String {
        command.to_owned()
    }

    /// Check if this action signature was already approved in this session.
    /// `session` is the local session id (`None` in the single-session
    /// shapes); approvals never carry across sessions.
    pub fn was_command_approved(&self, session: Option<&str>, dedup_source: &str) -> bool {
        self.approved_commands
            .get(session.unwrap_or(""))
            .is_some_and(|set| set.contains(&Self::command_dedup_key(dedup_source)))
    }

    /// Record an approved action signature for this session.
    pub fn record_approved_command(&mut self, session: Option<&str>, dedup_source: &str) {
        self.approved_commands
            .entry(session.unwrap_or("").to_string())
            .or_default()
            .insert(Self::command_dedup_key(dedup_source));
    }

    /// Determine whether approval is needed for a given action category,
    /// under `session`'s effective dial (session override layer over the
    /// live global layer; `None` and un-dialed sessions read pure global —
    /// today's exact behavior). Returns true if the user must be prompted.
    pub fn needs_approval(&self, session: Option<&str>, category: ActionCategory) -> bool {
        // HumanInput and LiveAudioSpawn always require human regardless of
        // autonomy level — hard gates outside the dial at every scope.
        if category == ActionCategory::HumanInput || category == ActionCategory::LiveAudioSpawn {
            return true;
        }

        let level = self.effective_level(session);

        // Full autonomy auto-approves everything except the hard gates above.
        if level == AutonomyLevel::Full {
            return false;
        }

        // DisplayControl: ask on first use, then session-grant takes over
        // (the grant is daemon-wide and not dial vocabulary).
        if category == ActionCategory::DisplayControl {
            return !self.user_display_granted;
        }

        let rules = self.effective_rules(session);

        // Low autonomy asks for everything except FileRead (unless Deny overrides)
        if level == AutonomyLevel::Low {
            let rule = rules.rule_for(category);
            if rule == ApprovalRule::Deny {
                return true;
            }
            return category != ActionCategory::FileRead;
        }

        // Check category-level rule (overrides the level)
        let rule = rules.rule_for(category);
        match rule {
            ApprovalRule::Auto => false,
            ApprovalRule::Deny => true, // deny acts like "ask" — will be denied
            ApprovalRule::Ask => {
                // Apply the effective autonomy level
                match level {
                    AutonomyLevel::Medium => {
                        // Ask for shell execution and Ask-ruled effects,
                        // including controller/external-agent tool calls.
                        matches!(
                            category,
                            ActionCategory::CommandExec
                                | ActionCategory::FileWrite
                                | ActionCategory::FileDelete
                                | ActionCategory::Destructive
                                | ActionCategory::NetworkRequest
                                | ActionCategory::ToolCall
                        )
                    }
                    AutonomyLevel::High => false,
                    _ => false, // Low and Full handled above
                }
            }
        }
    }

    /// Decide how to handle an approval request that an *external agent*
    /// (Codex / Gemini / Claude Code / Kimi Code) explicitly emitted.
    ///
    /// This is deliberately distinct from [`Self::needs_approval`], which
    /// classifies actions that intendant itself initiates. The external
    /// agent only sends an approval request when its own `approval_policy`
    /// has already decided the action warrants human review (e.g. Codex
    /// running with `on-request`). That escalation must not be silently
    /// swallowed by a category whose intendant-side default is `Auto`
    /// (`CommandExec`/`NetworkRequest` default to `Auto`) — the request
    /// arriving at all is the signal that a human should decide.
    ///
    /// Semantics (under `session`'s effective dial; `None` and un-dialed
    /// sessions read pure global):
    /// - `Full` autonomy → auto-approve (no human in the loop at all).
    /// - An explicit category `Deny` rule → reject.
    /// - Everything else → surface to the frontend `y/s/a/n` gate.
    ///
    /// The documented Full-before-deny caveat is inherited unchanged by
    /// per-session Full: the bypass then reaches that session only, and
    /// it remains the separately-tracked defect (docs/src/autonomy.md).
    pub fn external_approval_decision(
        &self,
        session: Option<&str>,
        category: ActionCategory,
    ) -> ExternalApprovalDecision {
        // Full autonomy keeps the human entirely out of the loop.
        if self.effective_level(session) == AutonomyLevel::Full {
            return ExternalApprovalDecision::AutoApprove;
        }

        // An explicit per-category Deny rule rejects outright.
        let rule = self.effective_rule_for(session, category);
        if rule == ApprovalRule::Deny {
            return ExternalApprovalDecision::Reject;
        }

        // Tool-call (MCP / computer-use) approvals honor an explicit `Auto`
        // rule so users can auto-allow Intendant's own tools without going
        // Full autonomy. Other categories keep prompting — their `Auto`
        // default is for intendant-initiated actions, not external-agent
        // escalations.
        if matches!(category, ActionCategory::ToolCall) && rule == ApprovalRule::Auto {
            return ExternalApprovalDecision::AutoApprove;
        }

        // The external agent asked: let the human decide.
        ExternalApprovalDecision::Ask
    }

    /// Decide how to handle a tool call the controller dispatches itself —
    /// outbound MCP client tools (`mcp__*`), `invoke_skill`,
    /// `spawn_sub_agent`, `workflow_checkpoint`. These never reach the
    /// runtime, so [`classify_command`] never sees them; the
    /// `[approval] tool_call` rule governs them here:
    /// - an explicit `Deny` rule refuses at every level, matching the
    ///   runtime batch consult (where a Deny rule is absolute);
    /// - otherwise [`Self::needs_approval`] for [`ActionCategory::ToolCall`]
    ///   decides: Low always prompts, the default `Ask` rule prompts at
    ///   Medium, High auto-approves ordinary Ask rules, and Full never asks.
    ///   Users who deliberately trust their configured servers can opt into
    ///   `tool_call = "auto"`.
    ///
    /// Session-aware like the other decision fns: `session`'s effective
    /// dial applies; `None` and un-dialed sessions read pure global.
    pub fn controller_tool_decision(&self, session: Option<&str>) -> ControllerToolDecision {
        let rule = self.effective_rule_for(session, ActionCategory::ToolCall);
        if rule == ApprovalRule::Deny {
            return ControllerToolDecision::Deny;
        }
        if self.needs_approval(session, ActionCategory::ToolCall) {
            return ControllerToolDecision::Ask;
        }
        ControllerToolDecision::AutoApprove
    }
}

/// Outcome of consulting autonomy for a controller-dispatched tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerToolDecision {
    /// Dispatch without prompting.
    AutoApprove,
    /// Refuse without prompting (explicit `tool_call = "deny"` rule).
    Deny,
    /// Surface an approval prompt before dispatch.
    Ask,
}

/// Outcome of routing an external-agent approval request through autonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalApprovalDecision {
    /// Auto-approve without prompting (Full autonomy).
    AutoApprove,
    /// Reject without prompting (explicit category `Deny`).
    Reject,
    /// Surface the request to the frontend approval gate.
    Ask,
}

/// Shared autonomy state wrapped in Arc<RwLock> for concurrent access.
pub type SharedAutonomy = Arc<RwLock<AutonomyState>>;

pub fn shared_autonomy(state: AutonomyState) -> SharedAutonomy {
    Arc::new(RwLock::new(state))
}

/// Hash a JSON value into `hasher`, order-independently for object keys,
/// so the same arguments produce the same digest regardless of field order.
fn hash_json_into(value: &serde_json::Value, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match value {
        serde_json::Value::Object(map) => {
            "obj".hash(hasher);
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for key in keys {
                key.hash(hasher);
                hash_json_into(&map[key.as_str()], hasher);
            }
        }
        serde_json::Value::Array(items) => {
            "arr".hash(hasher);
            for item in items {
                hash_json_into(item, hasher);
            }
        }
        other => other.to_string().hash(hasher),
    }
}

/// Digest of one runtime command minus its volatile `nonce`, so a retry of
/// the identical command dedups while any content change re-prompts.
fn command_content_digest(cmd: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match cmd {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for key in keys {
                if key == "nonce" {
                    continue;
                }
                std::hash::Hash::hash(key, &mut hasher);
                hash_json_into(&map[key.as_str()], &mut hasher);
            }
        }
        other => hash_json_into(other, &mut hasher),
    }
    hasher.finish()
}

/// Build the approval-dedup identity for a runtime command batch.
///
/// Distinct from the display preview on purpose: the preview for a
/// `writeFile`/`editFile` names only the path, and using it as the dedup
/// key made one approval cover *any* later content aimed at that path.
/// Here content-bearing mutations carry a digest of the full command
/// (minus the per-call `nonce`), while exec and the other commands keep
/// their exact-string semantics (with the display/nonce normalization
/// [`AutonomyState::command_dedup_key`] applies at record/consult time).
pub fn batch_dedup_source(json_str: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return json_str.to_string();
    };
    let Some(commands) = parsed.get("commands").and_then(|c| c.as_array()) else {
        return json_str.to_string();
    };
    let parts: Vec<String> = commands
        .iter()
        .map(|cmd| {
            let func = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("?");
            match func {
                "writeFile" | "editFile" => {
                    let path = cmd.get("file_path").and_then(|p| p.as_str()).unwrap_or("?");
                    format!("{}: {} #{:016x}", func, path, command_content_digest(cmd))
                }
                "execAsAgent" | "execPty" => {
                    let command = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                    format!("exec: {}", command)
                }
                "inspectPath" => {
                    let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    format!("inspect: {}", path)
                }
                "browse" => {
                    let url = cmd.get("url").and_then(|u| u.as_str()).unwrap_or("?");
                    format!("browse: {}", url)
                }
                _ => func.to_string(),
            }
        })
        .collect();
    parts.join(" | ")
}

/// Approval-dedup identity for a controller-dispatched tool call: the tool
/// name plus a digest of the full arguments, so a remembered approval
/// covers only the identical call.
pub fn controller_tool_dedup_source(tool_name: &str, args: &serde_json::Value) -> String {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_json_into(args, &mut hasher);
    format!("tool: {} #{:016x}", tool_name, hasher.finish())
}

/// Classify an agent command JSON into action categories.
pub fn classify_command(cmd: &serde_json::Value) -> Vec<ActionCategory> {
    let function = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("");

    let targets_user_display = cmd
        .get("display")
        .and_then(|d| d.as_i64())
        .is_some_and(|id| id <= 0);

    match function {
        "inspectPath" => vec![ActionCategory::FileRead],
        "writeFile" | "editFile" => vec![ActionCategory::FileWrite],
        "captureScreen" => {
            let mut cats = vec![ActionCategory::FileRead];
            if targets_user_display {
                cats.push(ActionCategory::DisplayControl);
            }
            cats
        }
        "askHuman" => vec![ActionCategory::HumanInput],
        "browse" => vec![ActionCategory::NetworkRequest],
        "execAsAgent" | "execPty" => {
            let command_str = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let mut cats = classify_shell_command(command_str);
            if targets_user_display {
                cats.push(ActionCategory::DisplayControl);
            }
            cats
        }
        _ => vec![ActionCategory::CommandExec],
    }
}

/// Split a compound shell command into individual sub-commands.
fn split_shell_commands(cmd: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    for line in cmd.split('\n') {
        // Split on &&, ||, and ; while preserving non-empty segments
        let mut remaining = line;
        while !remaining.is_empty() {
            if let Some(pos) = remaining.find("&&") {
                let part = remaining[..pos].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                remaining = remaining[pos + 2..].trim_start();
            } else if let Some(pos) = remaining.find("||") {
                let part = remaining[..pos].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                remaining = remaining[pos + 2..].trim_start();
            } else if let Some(pos) = remaining.find(';') {
                let part = remaining[..pos].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                remaining = remaining[pos + 1..].trim_start();
            } else {
                let part = remaining.trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                break;
            }
        }
    }
    parts
}

/// Classify a single shell sub-command into action categories.
///
/// Best-effort keyword matching for approval prompting — UX, not a security
/// boundary. String matching cannot see through variable indirection,
/// subshells, or novel spellings; the runtime's filesystem/exec sandbox is
/// what actually confines commands. Keep the common spellings covered
/// (long-form flags, absolute binary paths, `find -delete`) and accept that
/// a determined evasion only dodges the prompt, never the sandbox.
fn classify_single_command(cmd: &str, categories: &mut Vec<ActionCategory>) {
    let lower = cmd.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();

    // Compare binaries by basename so `/bin/rm` classifies like `rm`.
    fn base(token: &str) -> &str {
        token.rsplit('/').next().unwrap_or(token)
    }

    let first_token = tokens.first().copied().unwrap_or("");
    let is_sudo = base(first_token) == "sudo";
    // Skip leading `sudo` to classify the actual command
    let first = if is_sudo {
        tokens.get(1).copied().map(base).unwrap_or("")
    } else {
        base(first_token)
    };

    // Detect sudo usage as destructive (privilege escalation)
    if is_sudo {
        categories.push(ActionCategory::Destructive);
    }

    // Destructive commands
    let destructive_commands = [
        "rm", "rmdir", "kill", "killall", "pkill", "shutdown", "reboot", "mkfs", "dd",
    ];
    if destructive_commands.contains(&first)
        || lower.contains("rm -rf")
        || lower.contains("rm -r")
        || lower.contains("rm --recursive")
        || lower.contains("rm --force")
    {
        categories.push(ActionCategory::Destructive);
    }

    // `find` deletes without spelling `rm` as a command: `-delete`, or
    // `-exec`/`-execdir` handing matched paths to a destructive binary.
    if first == "find"
        && tokens.iter().enumerate().any(|(i, token)| {
            *token == "-delete"
                || ((*token == "-exec" || *token == "-execdir")
                    && tokens
                        .get(i + 1)
                        .copied()
                        .map(base)
                        .is_some_and(|next| destructive_commands.contains(&next)))
        })
    {
        categories.push(ActionCategory::Destructive);
    }

    // Network commands
    let network_commands = [
        "curl",
        "wget",
        "ssh",
        "scp",
        "rsync",
        "nc",
        "ncat",
        "ping",
        "traceroute",
        "dig",
        "nslookup",
        "git",
    ];
    if network_commands.contains(&first) || lower.contains("apt") || lower.contains("pip install") {
        categories.push(ActionCategory::NetworkRequest);
    }

    // File write indicators
    if lower.contains(" > ")
        || lower.contains(" >> ")
        || first == "tee"
        || first == "mv"
        || first == "cp"
    {
        categories.push(ActionCategory::FileWrite);
    }
}

/// Classify a shell command string into action categories.
/// Splits compound commands (&&, ||, ;, newlines) and classifies each part.
fn classify_shell_command(cmd: &str) -> Vec<ActionCategory> {
    let mut categories = vec![ActionCategory::CommandExec];
    for sub_cmd in split_shell_commands(cmd) {
        classify_single_command(sub_cmd, &mut categories);
    }
    categories.dedup();
    categories
}

/// Classify all commands in a JSON input batch.
pub fn classify_batch(json_str: &str) -> Vec<(usize, Vec<ActionCategory>)> {
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let commands = match value.get("commands").and_then(|c| c.as_array()) {
        Some(cmds) => cmds,
        None => return vec![],
    };

    commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| (i, classify_command(cmd)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_shell_rules_auto() -> ApprovalConfig {
        ApprovalConfig {
            file_read: ApprovalRule::Auto,
            file_write: ApprovalRule::Auto,
            file_delete: ApprovalRule::Auto,
            command_exec: ApprovalRule::Auto,
            network: ApprovalRule::Auto,
            destructive: ApprovalRule::Auto,
            display_control: ApprovalRule::Auto,
            tool_call: ApprovalRule::Auto,
        }
    }

    #[test]
    fn autonomy_level_display() {
        assert_eq!(AutonomyLevel::Low.to_string(), "Low");
        assert_eq!(AutonomyLevel::Medium.to_string(), "Medium");
        assert_eq!(AutonomyLevel::High.to_string(), "High");
        assert_eq!(AutonomyLevel::Full.to_string(), "Full");
    }

    #[test]
    fn autonomy_level_from_str() {
        assert_eq!(AutonomyLevel::from_str_loose("low"), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::from_str_loose("HIGH"), AutonomyLevel::High);
        assert_eq!(AutonomyLevel::from_str_loose("f"), AutonomyLevel::Full);
        assert_eq!(
            AutonomyLevel::from_str_loose("unknown"),
            AutonomyLevel::Medium
        );
        assert_eq!(AutonomyLevel::from_str_loose("0"), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::from_str_loose("3"), AutonomyLevel::Full);
    }

    #[test]
    fn action_category_display() {
        assert_eq!(ActionCategory::FileRead.to_string(), "file_read");
        assert_eq!(ActionCategory::FileWrite.to_string(), "file_write");
        assert_eq!(ActionCategory::Destructive.to_string(), "destructive");
        assert_eq!(ActionCategory::HumanInput.to_string(), "human_input");
    }

    #[test]
    fn approval_config_default_rules() {
        let config = ApprovalConfig::default();
        assert_eq!(config.file_read, ApprovalRule::Auto);
        assert_eq!(config.file_write, ApprovalRule::Ask);
        assert_eq!(config.file_delete, ApprovalRule::Ask);
        assert_eq!(config.command_exec, ApprovalRule::Auto);
        assert_eq!(config.network, ApprovalRule::Auto);
        assert_eq!(config.destructive, ApprovalRule::Ask);
        assert_eq!(config.tool_call, ApprovalRule::Ask);
        // The configured command-only field is Auto, but arbitrary shell
        // execution inherits the stricter reachable-effect defaults.
        assert_eq!(
            config.rule_for(ActionCategory::CommandExec),
            ApprovalRule::Ask
        );
    }

    #[test]
    fn shell_rule_is_the_strictest_reachable_rule() {
        let reachable = [
            "file_read",
            "file_write",
            "file_delete",
            "command_exec",
            "network",
            "destructive",
            "display_control",
        ];

        for category in reachable {
            let mut rules = all_shell_rules_auto();
            assert!(rules.set_rule_by_name(category, ApprovalRule::Ask));
            assert_eq!(
                rules.rule_for(ActionCategory::CommandExec),
                ApprovalRule::Ask,
                "{category} = ask must govern shell execution"
            );

            assert!(rules.set_rule_by_name(category, ApprovalRule::Deny));
            assert_eq!(
                rules.rule_for(ActionCategory::CommandExec),
                ApprovalRule::Deny,
                "{category} = deny must govern shell execution"
            );
        }

        let mut rules = all_shell_rules_auto();
        rules.tool_call = ApprovalRule::Deny;
        assert_eq!(
            rules.rule_for(ActionCategory::CommandExec),
            ApprovalRule::Auto,
            "controller tool policy is not reachable through the runtime shell"
        );
    }

    #[test]
    fn approval_config_from_toml() {
        let toml_str = r#"
file_read = "auto"
file_write = "deny"
file_delete = "deny"
command_exec = "ask"
network = "ask"
destructive = "deny"
"#;
        let config: ApprovalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.file_read, ApprovalRule::Auto);
        assert_eq!(config.file_write, ApprovalRule::Deny);
        assert_eq!(config.command_exec, ApprovalRule::Ask);
        assert_eq!(config.tool_call, ApprovalRule::Ask);
    }

    #[test]
    fn human_input_always_needs_approval() {
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::HumanInput));

        let state = AutonomyState::new(AutonomyLevel::High, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::HumanInput));
    }

    #[test]
    fn live_audio_always_needs_approval() {
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::LiveAudioSpawn));
    }

    #[test]
    fn full_autonomy_approves_everything_except_hard_gates() {
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(!state.needs_approval(ActionCategory::FileWrite));
        assert!(!state.needs_approval(ActionCategory::FileDelete));
        assert!(!state.needs_approval(ActionCategory::CommandExec));
        assert!(!state.needs_approval(ActionCategory::Destructive));
        assert!(!state.needs_approval(ActionCategory::NetworkRequest));
        assert!(state.needs_approval(ActionCategory::HumanInput));
        assert!(state.needs_approval(ActionCategory::LiveAudioSpawn));
    }

    #[test]
    fn low_autonomy_asks_for_everything_except_file_read() {
        let state = AutonomyState::new(AutonomyLevel::Low, ApprovalConfig::default());
        // FileRead is never gated even at Low
        assert!(!state.needs_approval(ActionCategory::FileRead));
        // Everything else needs approval at Low, regardless of Auto rules
        assert!(state.needs_approval(ActionCategory::CommandExec));
        assert!(state.needs_approval(ActionCategory::FileWrite));
        assert!(state.needs_approval(ActionCategory::Destructive));
        assert!(state.needs_approval(ActionCategory::NetworkRequest));
        assert!(state.needs_approval(ActionCategory::FileDelete));
    }

    #[test]
    fn medium_autonomy_asks_for_shell_writes_and_destructive() {
        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(state.needs_approval(ActionCategory::CommandExec));
        assert!(!state.needs_approval(ActionCategory::NetworkRequest));
        assert!(state.needs_approval(ActionCategory::FileWrite));
        assert!(state.needs_approval(ActionCategory::FileDelete));
        assert!(state.needs_approval(ActionCategory::Destructive));
    }

    #[test]
    fn medium_autonomy_asks_for_network_when_rule_asks() {
        let mut rules = ApprovalConfig::default();
        rules.network = ApprovalRule::Ask;
        let state = AutonomyState::new(AutonomyLevel::Medium, rules);
        assert!(state.needs_approval(ActionCategory::NetworkRequest));
    }

    #[test]
    fn high_autonomy_auto_approves_ordinary_categories() {
        let state = AutonomyState::new(AutonomyLevel::High, ApprovalConfig::default());
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(!state.needs_approval(ActionCategory::FileWrite));
        assert!(!state.needs_approval(ActionCategory::FileDelete));
        assert!(!state.needs_approval(ActionCategory::CommandExec));
        assert!(!state.needs_approval(ActionCategory::Destructive));
        assert!(state.needs_approval(ActionCategory::HumanInput));
        assert!(state.needs_approval(ActionCategory::LiveAudioSpawn));
    }

    #[test]
    fn high_autonomy_asks_for_destructive_when_rule_denies() {
        let mut rules = ApprovalConfig::default();
        rules.destructive = ApprovalRule::Deny;
        let state = AutonomyState::new(AutonomyLevel::High, rules);
        assert!(state.needs_approval(ActionCategory::Destructive));
    }

    #[test]
    fn command_dedup_key_keeps_effectful_targets_exact() {
        assert_ne!(
            AutonomyState::command_dedup_key("xdotool --display=1 key Return"),
            AutonomyState::command_dedup_key("xdotool --display=99 key Return")
        );
        assert_ne!(
            AutonomyState::command_dedup_key("xdotool display:1 key Return"),
            AutonomyState::command_dedup_key("xdotool display:99 key Return")
        );
        assert_ne!(
            AutonomyState::command_dedup_key("kill $NONCE[1]"),
            AutonomyState::command_dedup_key("kill $NONCE[2]")
        );
    }

    #[test]
    fn dedup_never_erases_executable_shell_syntax() {
        let mut state = AutonomyState::default();
        state.record_approved_command(None, "echo $NONCE[$(touch /tmp/approved)]");
        assert!(!state.was_command_approved(None, "echo $NONCE[$(touch /tmp/changed)]"));

        state.record_approved_command(None, "xdotool --display=$(printf 1) key Return");
        assert!(!state.was_command_approved(None, "xdotool --display=$(printf 2) key Return"));

        state.record_approved_command(None, "printf 'one two'");
        assert!(!state.was_command_approved(None, "printf 'one\ttwo'"));
    }

    #[test]
    fn approvals_are_scoped_per_session() {
        // One autonomy state backs every native session of a daemon; an
        // approval recorded in one session must not silence prompts in
        // another (or in the anonymous single-session bucket).
        let mut state = AutonomyState::default();
        state.record_approved_command(Some("sess-a"), "exec: cargo test");
        assert!(state.was_command_approved(Some("sess-a"), "exec: cargo test"));
        assert!(!state.was_command_approved(Some("sess-b"), "exec: cargo test"));
        assert!(!state.was_command_approved(None, "exec: cargo test"));

        state.record_approved_command(None, "exec: ls");
        assert!(state.was_command_approved(None, "exec: ls"));
        assert!(!state.was_command_approved(Some("sess-a"), "exec: ls"));
    }

    #[test]
    fn batch_dedup_source_tracks_write_content() {
        let write_v1 = r#"{"commands":[{"function":"editFile","nonce":1,"file_path":"/tmp/a.rs","content":"fn a() {}"}]}"#;
        let write_v1_retry = r#"{"commands":[{"function":"editFile","nonce":7,"file_path":"/tmp/a.rs","content":"fn a() {}"}]}"#;
        let write_v2 = r#"{"commands":[{"function":"editFile","nonce":1,"file_path":"/tmp/a.rs","content":"fn a() { std::process::exit(1) }"}]}"#;

        // Identical mutation with a fresh nonce dedups; changed content
        // at the same path re-prompts.
        assert_eq!(
            batch_dedup_source(write_v1),
            batch_dedup_source(write_v1_retry)
        );
        assert_ne!(batch_dedup_source(write_v1), batch_dedup_source(write_v2));

        // End to end through the session bucket: approving v1 never
        // covers v2.
        let mut state = AutonomyState::default();
        state.record_approved_command(Some("s"), &batch_dedup_source(write_v1));
        assert!(state.was_command_approved(Some("s"), &batch_dedup_source(write_v1_retry)));
        assert!(!state.was_command_approved(Some("s"), &batch_dedup_source(write_v2)));
    }

    #[test]
    fn batch_dedup_source_keeps_exec_exact_string_semantics() {
        let exec = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"cargo build"},{"function":"inspectPath","nonce":2,"path":"/tmp"}]}"#;
        assert_eq!(
            batch_dedup_source(exec),
            "exec: cargo build | inspect: /tmp"
        );
        // Unparseable input falls back to the raw string, like the preview.
        assert_eq!(batch_dedup_source("not json"), "not json");
    }

    #[test]
    fn controller_tool_dedup_source_tracks_args() {
        let a1: serde_json::Value = serde_json::from_str(r#"{"title":"x","body":"y"}"#).unwrap();
        // Same fields in a different order digest identically.
        let a1_reordered: serde_json::Value =
            serde_json::from_str(r#"{"body":"y","title":"x"}"#).unwrap();
        let a2: serde_json::Value =
            serde_json::from_str(r#"{"title":"x","body":"CHANGED"}"#).unwrap();

        let k1 = controller_tool_dedup_source("mcp__gh_create_issue", &a1);
        assert_eq!(
            k1,
            controller_tool_dedup_source("mcp__gh_create_issue", &a1_reordered)
        );
        assert_ne!(
            k1,
            controller_tool_dedup_source("mcp__gh_create_issue", &a2)
        );
        assert_ne!(k1, controller_tool_dedup_source("mcp__gh_close_issue", &a1));
    }

    #[test]
    fn controller_tool_decision_honors_rules_and_levels() {
        // The shipped Medium default prompts before crossing a controller
        // or external-agent tool boundary.
        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert_eq!(
            state.controller_tool_decision(),
            ControllerToolDecision::Ask
        );

        // Low also prompts.
        let state = AutonomyState::new(AutonomyLevel::Low, ApprovalConfig::default());
        assert_eq!(
            state.controller_tool_decision(),
            ControllerToolDecision::Ask
        );

        // Full never asks.
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert_eq!(
            state.controller_tool_decision(),
            ControllerToolDecision::AutoApprove
        );

        // Users can deliberately opt into auto dispatch at Medium.
        let mut rules = ApprovalConfig::default();
        rules.tool_call = ApprovalRule::Auto;
        let state = AutonomyState::new(AutonomyLevel::Medium, rules.clone());
        assert_eq!(
            state.controller_tool_decision(),
            ControllerToolDecision::AutoApprove
        );

        // The default Ask rule is auto-approved at High.
        rules.tool_call = ApprovalRule::Ask;
        let state = AutonomyState::new(AutonomyLevel::High, rules);
        assert_eq!(
            state.controller_tool_decision(),
            ControllerToolDecision::AutoApprove
        );

        // Deny refuses at every level — absolute, like the runtime batch
        // consult.
        let mut rules = ApprovalConfig::default();
        rules.tool_call = ApprovalRule::Deny;
        for level in [
            AutonomyLevel::Low,
            AutonomyLevel::Medium,
            AutonomyLevel::High,
            AutonomyLevel::Full,
        ] {
            let state = AutonomyState::new(level, rules.clone());
            assert_eq!(
                state.controller_tool_decision(),
                ControllerToolDecision::Deny,
                "tool_call = deny must refuse at {:?}",
                level
            );
        }
    }

    #[test]
    fn classifier_catches_cheap_evasions() {
        // Absolute-path binary names.
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "/bin/rm --recursive /tmp/x"
        });
        assert!(classify_command(&cmd).contains(&ActionCategory::Destructive));

        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "/usr/bin/curl https://example.com"
        });
        assert!(classify_command(&cmd).contains(&ActionCategory::NetworkRequest));

        // Long-form flags on a non-leading rm.
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo /tmp/x | xargs rm --recursive"
        });
        assert!(classify_command(&cmd).contains(&ActionCategory::Destructive));

        // find-based deletion.
        for command in [
            "find /tmp -name '*.log' -delete",
            "find . -type f -exec rm {} ;",
            "/usr/bin/find . -execdir rm -f {} ;",
        ] {
            let cmd: serde_json::Value = serde_json::json!({
                "function": "execAsAgent",
                "nonce": 1,
                "command": command
            });
            assert!(
                classify_command(&cmd).contains(&ActionCategory::Destructive),
                "{command:?} must classify destructive"
            );
        }

        // A plain find without deletion stays non-destructive.
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "find . -name '*.rs'"
        });
        assert!(!classify_command(&cmd).contains(&ActionCategory::Destructive));

        // sudo via absolute path counts as privilege escalation.
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "/usr/bin/sudo systemctl stop nginx"
        });
        assert!(classify_command(&cmd).contains(&ActionCategory::Destructive));
    }

    #[test]
    fn medium_asks_for_tool_call_by_default_and_auto_is_explicit() {
        // Default Ask: prompt at Medium.
        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::ToolCall));
        // Explicit auto rule: no prompt at Medium.
        let mut rules = ApprovalConfig::default();
        rules.tool_call = ApprovalRule::Auto;
        let state = AutonomyState::new(AutonomyLevel::Medium, rules);
        assert!(!state.needs_approval(ActionCategory::ToolCall));
    }

    #[test]
    fn low_autonomy_overrides_auto_rules() {
        let mut rules = ApprovalConfig::default();
        rules.file_write = ApprovalRule::Auto;
        let state = AutonomyState::new(AutonomyLevel::Low, rules);
        // Low overrides Auto — still asks for file_write
        assert!(state.needs_approval(ActionCategory::FileWrite));
    }

    #[test]
    fn classify_exec_command() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "ls -la /tmp"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::CommandExec));
        assert!(!cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn unrecognized_shell_effect_cannot_downgrade_default_policy() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "python -c \"__import__('os').unlink('/tmp/x')\""
        });
        let cats = classify_command(&cmd);
        assert_eq!(cats, vec![ActionCategory::CommandExec]);

        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::CommandExec));
    }

    #[test]
    fn classify_destructive_rm() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "rm -rf /tmp/test"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::CommandExec));
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_network_curl() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "curl https://example.com"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::NetworkRequest));
    }

    #[test]
    fn classify_file_write_redirect() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello > /tmp/out.txt"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::FileWrite));
    }

    #[test]
    fn classify_edit_file() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "editFile",
            "nonce": 1,
            "file": "/tmp/test.txt"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::FileWrite));
    }

    #[test]
    fn classify_ask_human() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "askHuman",
            "nonce": 1,
            "question": "Which database?"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::HumanInput));
    }

    #[test]
    fn classify_browse() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "browse",
            "nonce": 1,
            "url": "https://example.com"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::NetworkRequest));
    }

    #[test]
    fn classify_inspect_path() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "inspectPath",
            "nonce": 1,
            "path": "/tmp"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::FileRead));
    }

    #[test]
    fn classify_batch_multiple() {
        let json = r#"{"commands":[
            {"function":"execAsAgent","nonce":1,"command":"ls"},
            {"function":"editFile","nonce":2,"file":"/tmp/x"},
            {"function":"askHuman","nonce":3,"question":"ok?"}
        ]}"#;
        let result = classify_batch(json);
        assert_eq!(result.len(), 3);
        assert!(result[0].1.contains(&ActionCategory::CommandExec));
        assert!(result[1].1.contains(&ActionCategory::FileWrite));
        assert!(result[2].1.contains(&ActionCategory::HumanInput));
    }

    #[test]
    fn classify_batch_invalid_json() {
        let result = classify_batch("not json");
        assert!(result.is_empty());
    }

    #[test]
    fn classify_batch_no_commands() {
        let result = classify_batch(r#"{"commands":[]}"#);
        assert!(result.is_empty());
    }

    #[test]
    fn classify_multiline_rm() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello\nrm -rf /tmp/test"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::CommandExec));
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_chained_commands() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello && rm -rf /tmp/test"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_semicolon_separated() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello; curl https://example.com"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::NetworkRequest));
    }

    #[test]
    fn classify_or_chain() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "ls /nonexist || rm -rf /tmp/bad"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_bare_rm() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "rm file.txt"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn shared_autonomy_default() {
        let state = AutonomyState::default();
        assert_eq!(state.level, AutonomyLevel::Medium);
    }

    #[test]
    fn severity_ordering() {
        // Arbitrary shell execution outranks every ordinary effect because
        // the shell can reach all of them without a recognizable spelling.
        assert!(ActionCategory::CommandExec.severity() > ActionCategory::Destructive.severity());
        assert!(ActionCategory::Destructive.severity() > ActionCategory::FileDelete.severity());
        assert!(ActionCategory::FileDelete.severity() > ActionCategory::FileWrite.severity());
        assert!(ActionCategory::FileWrite.severity() > ActionCategory::NetworkRequest.severity());
        assert!(ActionCategory::NetworkRequest.severity() > ActionCategory::FileRead.severity());
    }

    #[test]
    fn human_input_highest_severity() {
        assert!(ActionCategory::HumanInput.severity() > ActionCategory::CommandExec.severity());
    }

    #[test]
    fn display_control_highest_severity() {
        assert!(
            ActionCategory::DisplayControl.severity() > ActionCategory::LiveAudioSpawn.severity()
        );
    }

    #[test]
    fn display_control_category_display() {
        assert_eq!(
            ActionCategory::DisplayControl.to_string(),
            "display_control"
        );
    }

    #[test]
    fn display_control_default_rule_is_ask() {
        let config = ApprovalConfig::default();
        assert_eq!(config.display_control, ApprovalRule::Ask);
        assert_eq!(
            config.rule_for(ActionCategory::DisplayControl),
            ApprovalRule::Ask
        );
    }

    #[test]
    fn display_control_needs_approval_when_not_granted() {
        // DisplayControl always needs approval until granted, at every autonomy level
        for level in [
            AutonomyLevel::Low,
            AutonomyLevel::Medium,
            AutonomyLevel::High,
        ] {
            let state = AutonomyState::new(level, ApprovalConfig::default());
            assert!(
                state.needs_approval(ActionCategory::DisplayControl),
                "DisplayControl should need approval at {:?} when not granted",
                level
            );
        }
        // Full autonomy auto-approves everything including DisplayControl
        let full = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(!full.needs_approval(ActionCategory::DisplayControl));
    }

    #[test]
    fn display_control_skips_approval_when_granted() {
        // Once granted, no approval needed at any level
        for level in [
            AutonomyLevel::Low,
            AutonomyLevel::Medium,
            AutonomyLevel::High,
            AutonomyLevel::Full,
        ] {
            let mut state = AutonomyState::new(level, ApprovalConfig::default());
            state.user_display_granted = true;
            assert!(
                !state.needs_approval(ActionCategory::DisplayControl),
                "DisplayControl should NOT need approval at {:?} when granted",
                level
            );
        }
    }

    #[test]
    fn classify_capture_screen_user_display() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "captureScreen",
            "nonce": 1,
            "display": 0
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::DisplayControl));
        assert!(cats.contains(&ActionCategory::FileRead));
    }

    #[test]
    fn classify_capture_screen_virtual_display() {
        // display: 99 should NOT trigger DisplayControl
        let cmd: serde_json::Value = serde_json::json!({
            "function": "captureScreen",
            "nonce": 1,
            "display": 99
        });
        let cats = classify_command(&cmd);
        assert!(!cats.contains(&ActionCategory::DisplayControl));
        assert!(cats.contains(&ActionCategory::FileRead));
    }

    #[test]
    fn classify_exec_user_display() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "xdotool key Return",
            "display": 0
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::DisplayControl));
        assert!(cats.contains(&ActionCategory::CommandExec));
    }

    #[test]
    fn classify_exec_no_display_no_control() {
        // No display field → no DisplayControl
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello"
        });
        let cats = classify_command(&cmd);
        assert!(!cats.contains(&ActionCategory::DisplayControl));
    }

    #[test]
    fn external_approval_asks_at_high_for_command_exec() {
        // Regression: an external agent (e.g. Codex on-request) explicitly
        // asking for command-exec approval must reach the frontend at High,
        // even though CommandExec's intendant-side default rule is Auto.
        let state = AutonomyState::new(AutonomyLevel::High, ApprovalConfig::default());
        assert_eq!(
            state.external_approval_decision(ActionCategory::CommandExec),
            ExternalApprovalDecision::Ask
        );
        assert_eq!(
            state.external_approval_decision(ActionCategory::FileWrite),
            ExternalApprovalDecision::Ask
        );
    }

    #[test]
    fn external_approval_asks_at_medium_and_low() {
        for level in [AutonomyLevel::Low, AutonomyLevel::Medium] {
            let state = AutonomyState::new(level, ApprovalConfig::default());
            assert_eq!(
                state.external_approval_decision(ActionCategory::CommandExec),
                ExternalApprovalDecision::Ask,
                "level {:?} should surface external command approvals",
                level
            );
        }
    }

    #[test]
    fn external_approval_full_auto_approves() {
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert_eq!(
            state.external_approval_decision(ActionCategory::CommandExec),
            ExternalApprovalDecision::AutoApprove
        );
        assert_eq!(
            state.external_approval_decision(ActionCategory::FileWrite),
            ExternalApprovalDecision::AutoApprove
        );
    }

    #[test]
    fn external_approval_deny_rule_rejects() {
        let mut rules = ApprovalConfig::default();
        rules.command_exec = ApprovalRule::Deny;
        let state = AutonomyState::new(AutonomyLevel::High, rules);
        assert_eq!(
            state.external_approval_decision(ActionCategory::CommandExec),
            ExternalApprovalDecision::Reject
        );
    }

    #[test]
    fn external_approval_tool_call_asks_by_default_and_honors_explicit_auto() {
        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert_eq!(state.rules.tool_call, ApprovalRule::Ask);
        assert_eq!(
            state.external_approval_decision(ActionCategory::ToolCall),
            ExternalApprovalDecision::Ask
        );

        let mut rules = ApprovalConfig::default();
        rules.tool_call = ApprovalRule::Auto;
        let state = AutonomyState::new(AutonomyLevel::Medium, rules);
        assert_eq!(
            state.external_approval_decision(ActionCategory::ToolCall),
            ExternalApprovalDecision::AutoApprove
        );
    }

    #[test]
    fn external_approval_command_exec_auto_still_asks() {
        // Regression guard: CommandExec's Auto default must NOT auto-approve
        // external-agent escalations below Full autonomy — only ToolCall does.
        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert_eq!(state.rules.command_exec, ApprovalRule::Auto);
        assert_eq!(
            state.external_approval_decision(ActionCategory::CommandExec),
            ExternalApprovalDecision::Ask
        );
    }

    #[test]
    fn external_approval_tool_call_deny_rejects() {
        let mut rules = ApprovalConfig::default();
        rules.tool_call = ApprovalRule::Deny;
        let state = AutonomyState::new(AutonomyLevel::Medium, rules);
        assert_eq!(
            state.external_approval_decision(ActionCategory::ToolCall),
            ExternalApprovalDecision::Reject
        );
    }

    #[test]
    fn external_approval_full_overrides_deny() {
        // Full keeps the human out of the loop entirely, even past a Deny rule.
        let mut rules = ApprovalConfig::default();
        rules.command_exec = ApprovalRule::Deny;
        let state = AutonomyState::new(AutonomyLevel::Full, rules);
        assert_eq!(
            state.external_approval_decision(ActionCategory::CommandExec),
            ExternalApprovalDecision::AutoApprove
        );
    }
}
