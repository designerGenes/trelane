use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const MSG_TYPES: &[&str] = &[
    "question",
    "answer",
    "info",
    "claim-request",
    "claim-grant",
    "claim-deny",
    "help-request",
    "help-offer",
    "help-accept",
    "help-deny",
    "help-revoke",
    "submission",
    "review-result",
    "handoff",
    "system",
    // TMP v1.0 structured types (src/rules/trelane-message-protocol.schema.json).
    // The Squire must be able to decode every type an agent can send (R4).
    "park",
    "wake",
    "di_request",
    "di_approve",
    "di_deny",
    "claim",
    "bulletin",
    "domain_change_notice",
    "split_proposal_notice",
    "quiescence_notice",
    "custom",
];

/// TMP v1.0 protocol version stamped on every message (GAP-03).
pub const TMP_VERSION: &str = "1.0";

/// Message channels (TMP envelope). `direct` = inbox-addressed; `bulletin` =
/// domain-scoped board that never wakes anyone (R13).
pub const CHANNEL_DIRECT: &str = "direct";
pub const CHANNEL_BULLETIN: &str = "bulletin";

pub const URGENCIES: &[&str] = &["low", "normal", "high", "critical"];

pub const TRELANE_DIR: &str = ".trelane";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub agents: AgentConfig,
    pub launcher: LauncherConfig,
    /// The squire -- Trelane's dutiful assistant who restarts agents and
    /// keeps the workflow in motion.  `alias = "pump"` and `alias = "prop"`
    /// keep pre-0.3 config files loading without migration.
    #[serde(alias = "pump", alias = "prop")]
    pub squire: SquireConfig,
    pub claims: ClaimsConfig,
    #[serde(default)]
    pub di: DiConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub biplane: BiplaneConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
}

impl Default for Config {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        // Ready-to-use headless launcher profiles. Select one per agent with
        // `trelane add-agent <name> --launcher-agent <profile>`, or override
        // any of these in config.json.
        profiles.insert(
            "claude-code".to_string(),
            r#"claude -p "$(cat {prompt_file})" --permission-mode acceptEdits --allowedTools "Bash(trelane *)" --max-turns 50"#
                .to_string(),
        );
        profiles.insert(
            "opencode".to_string(),
            r#"opencode run "$(cat {prompt_file})""#.to_string(),
        );
        profiles.insert(
            "copilot".to_string(),
            r#"copilot -p "$(cat {prompt_file})" --allow-all-tools"#.to_string(),
        );
        Self {
            agents: AgentConfig::default(),
            launcher: LauncherConfig {
                template: r#"claude -p "$(cat {prompt_file})" --permission-mode acceptEdits --allowedTools "Bash(trelane *)" --max-turns 50"#
                    .to_string(),
                profiles,
            },
            squire: SquireConfig {
                interval_s: 20,
                // Compiled default simultaneous-execution ceiling. Conservative
                // on purpose (see `SquireConfig::max_concurrent` docs); raise it
                // to use more registered agents at once via config.json,
                // `trelane config set squire.max_concurrent N`, or
                // `trelane squire --max-concurrent N`.
                max_concurrent: 2,
                // F2: Default to 1 hour. Protects new installs against
                // enabled-but-silently-stuck counterparts without being
                // so aggressive that a legitimately slow agent gets
                // force-expired in normal operation.
                reply_timeout_s: Some(3600),
                breaker_escalation_count: default_breaker_escalation_count(),
                starvation_ticks: default_starvation_ticks(),
            },
            claims: ClaimsConfig {
                default_ttl_s: 900,
            },
            di: DiConfig::default(),
            retention: RetentionConfig::default(),
            ui: UiConfig::default(),
            biplane: BiplaneConfig::default(),
            workspace: WorkspaceConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub default: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    pub template: String,
    #[serde(default)]
    pub profiles: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SquireConfig {
    pub interval_s: u64,
    /// Maximum number of agents the squire runs *simultaneously*. This is a
    /// scheduling ceiling, NOT the number of registered agents: a swarm may
    /// have many more agents registered than this, and the squire will only
    /// ever have this many awake at once -- the rest are deferred to a later
    /// tick (which can look like "agents registered but idle"). The compiled
    /// default is 2. Override it in config.json (`"squire": {
    /// "max_concurrent": N }`), persistently with `trelane config set
    /// squire.max_concurrent N`, or for a single run with `trelane squire
    /// --max-concurrent N`. Inspect the effective value and live utilization
    /// with `trelane config explain squire.max_concurrent`.
    pub max_concurrent: usize,
    /// Maximum seconds a reply-wait park can sit unsatisfied before the
    /// squire declares it abandoned and wakes the waiting agent with an
    /// abandonment reason.  `None` (the default) disables timeout-based
    /// abandonment -- only `park_target_gone` (disabled/removed agent)
    /// triggers abandonment in that case.
    #[serde(default)]
    pub reply_timeout_s: Option<u64>,
    /// R24: how many times the same agent may be woken as designated breaker
    /// for the same wait-cycle before the cycle escalates (a different
    /// tie-break is tried, then the cycle is surfaced as needing a human).
    /// Default 3.
    #[serde(default = "default_breaker_escalation_count")]
    pub breaker_escalation_count: i64,
    /// R23: a candidate that has been valid but unchosen for this many
    /// consecutive ticks is guaranteed one of the next tick's capacity slots,
    /// ahead of ordinary ordering. Default 10 (at the default 20s interval,
    /// an agent waits at most ~3.3 minutes before its slot is guaranteed).
    #[serde(default = "default_starvation_ticks")]
    pub starvation_ticks: i64,
}

fn default_breaker_escalation_count() -> i64 {
    3
}

fn default_starvation_ticks() -> i64 {
    10
}

/// Slice 4A: domain-intrusion (DI) timing configuration. See R9, R25, R26.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiConfig {
    /// Seconds a non-owner approval must stand, unvetoed, before the request
    /// resolves to Approved (R9). Long enough for the owner to see the
    /// request on their next wake; short enough that a non-responsive owner
    /// doesn't block work for a full squire cycle. Default 300 (5 minutes).
    pub objection_window_s: u64,
    /// Seconds a DI request may sit with no approval and no veto before it
    /// transitions to Expired -- never silently Approved (R25). Default 3600.
    pub request_timeout_s: u64,
    /// Seconds a `claim-contested` park (an approved DI whose claim lost the
    /// lease race, R26) may sit before the contention is abandoned and the
    /// requester is woken. Default 1800 (30 minutes).
    pub claim_contested_timeout_s: u64,
}

impl Default for DiConfig {
    fn default() -> Self {
        Self {
            objection_window_s: 300,
            request_timeout_s: 3600,
            claim_contested_timeout_s: 1800,
        }
    }
}

/// Slice 4D: retention configuration (R15). Staleness demotes to a colder
/// tier; nothing is ever deleted unless `purge_days` is explicitly set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Messages untouched for longer than this many days are archived:
    /// excluded from default queries, fully readable under
    /// `--include-archived`. Default 30.
    pub hot_days: u64,
    /// A whole project with zero agent activity for this many days is
    /// flagged dormant (a marker only; no data is touched). Default 90.
    pub dormant_days: u64,
    /// Real deletion threshold in days. UNSET by default -- deletion only
    /// ever happens when this is explicitly configured (R15).
    pub purge_days: Option<u64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            hot_days: 30,
            dormant_days: 90,
            purge_days: None,
        }
    }
}

/// Deprecated name for [`SquireConfig`], kept so external code compiles.
pub type PumpConfig = SquireConfig;
pub type PropConfig = SquireConfig;

/// Session UI configuration: tmux key bindings and pane-navigation behaviour.
/// All keys use tmux key syntax (`F2`, `M-Left`, `C-b`, ...). Bindings land
/// in tmux's root key table, so they work without a prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub keys: UiKeys,
    /// Bind arrow keys to move focus between tmux panes. The binding lives at
    /// the tmux level, so it works in any terminal emulator. When
    /// `match_host_terminal` is set, Trelane tries to match the host
    /// terminal's own pane-navigation shortcuts (see below); otherwise it uses
    /// Alt+arrows.
    pub pane_navigation: bool,
    /// Try to match the host terminal's native pane-navigation keybindings.
    /// For Ghostty, Trelane reads `~/.config/ghostty/config` and mirrors any
    /// `goto_split` bindings whose modifiers tmux can actually receive
    /// (Alt/Ctrl/Shift). Cmd/Super-based bindings can't be forwarded to tmux
    /// on macOS, so those fall back to Alt+arrows. Other terminals use
    /// Alt+arrows directly.
    pub match_host_terminal: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            keys: UiKeys::default(),
            pane_navigation: true,
            match_host_terminal: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiKeys {
    /// Pop a diagnostic split showing `trelane status` for the session.
    pub diagnostics: String,
    /// Pop a diagnostic split showing the focused pane's agent inbox.
    pub inbox: String,
    /// Toggle verbose squire output for the session frame.
    pub verbose_toggle: String,
    /// Open the interactive diagnostic TUI (`trelane diagnostic`) in a split.
    pub diagnostic_view: String,
    /// Guaranteed, terminal-agnostic pane-navigation keys. Unlike the
    /// best-effort Alt/Ctrl+arrow bindings in `resolve_pane_nav_bindings`
    /// (which depend on per-terminal escape-sequence forwarding -- e.g.
    /// macOS Terminal.app only sends Option as Meta if the user has manually
    /// enabled that non-default preference), plain function keys are sent
    /// identically by every terminal with no configuration required, so
    /// these always work as a baseline regardless of terminal or settings.
    pub pane_left: String,
    pub pane_right: String,
    pub pane_up: String,
    pub pane_down: String,
    /// Per-session toggle: swap the focused agent pane between its live session
    /// view and a diagnostic view of that one agent (NOT the whole Trelane
    /// session -- that's `diagnostic_view`). Defaults to a function key for the
    /// same reason as the others: a global tmux root-table binding on a letter
    /// like `D` would swallow every `D` the user types into an agent's own
    /// terminal. Set it to `"D"` in config if you accept that tradeoff.
    pub session_diagnostic: String,
    /// Per-session key: show the focused (usually asleep) agent's message
    /// history, so the user can see why it parked. Same letter-vs-function-key
    /// tradeoff as `session_diagnostic`; set to `"M"` to match the design.
    pub message_history: String,
}

impl Default for UiKeys {
    fn default() -> Self {
        Self {
            diagnostics: "F2".to_string(),
            inbox: "F3".to_string(),
            verbose_toggle: "F4".to_string(),
            diagnostic_view: "F5".to_string(),
            pane_left: "F6".to_string(),
            pane_right: "F7".to_string(),
            pane_up: "F8".to_string(),
            pane_down: "F9".to_string(),
            session_diagnostic: "F10".to_string(),
            message_history: "F11".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimsConfig {
    pub default_ttl_s: u64,
}

/// Biplane configuration: controls re-analysis behaviour when the swarm
/// goes fully quiescent (all agents stopped, all inboxes empty, no
/// parked tasks).
///
/// Two independent behaviours:
/// - `detect_thematic_deadlock` (default true): automatically detect and
///   report stalled domains when the swarm goes quiescent. This is a
///   detection/reporting action only -- it surfaces the problem but does
///   not modify the session.
/// - `reanalyze_on_all_stop` (default false): additionally auto-register
///   agents for any emergent (uncovered) domains discovered during
///   reconciliation. This is a more consequential action that modifies
///   the session, so it remains opt-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BiplaneConfig {
    /// When true (default), the squire watch loop detects and reports
    /// stalled domains (thematic deadlock) when the swarm is quiescent.
    pub detect_thematic_deadlock: bool,
    /// When true (default false), the squire watch loop also auto-registers
    /// agents for emergent domains discovered during reconciliation.
    /// Additive-only: existing agents are never removed or re-assigned.
    pub reanalyze_on_all_stop: bool,
}

impl Default for BiplaneConfig {
    fn default() -> Self {
        Self {
            detect_thematic_deadlock: true,
            reanalyze_on_all_stop: false,
        }
    }
}

/// C5: workspace mode for delegated changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub mode: WorkspaceMode,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            mode: WorkspaceMode::Shared,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    Shared,
    Worktree,
}

impl Default for WorkspaceMode {
    fn default() -> Self {
        WorkspaceMode::Shared
    }
}

impl WorkspaceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkspaceMode::Shared => "shared",
            WorkspaceMode::Worktree => "worktree",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "shared" => WorkspaceMode::Shared,
            "worktree" => WorkspaceMode::Worktree,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------- C7 completion

/// Project completion status derived from durable work state.
#[derive(Debug, Clone)]
pub struct ProjectCompletionReport {
    pub eligible: bool,
    pub complete: bool,
    pub snapshot_fingerprint: String,
    pub attested_by: Option<String>,
    pub blockers: Vec<CompletionBlocker>,
}

#[derive(Debug, Clone)]
pub struct CompletionBlocker {
    pub kind: String,
    pub count: usize,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Domain {
    pub agent: String,
    #[serde(default)]
    pub description: String,
    pub writable: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_agent: Option<String>,
    #[serde(default)]
    pub forbidden_write: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchTarget {
    pub agent: String,
    pub adapter: String,
    pub target: String,
    pub command: String,
    pub tmux_target: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default = "default_urgency")]
    pub urgency: String,
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub created_at: String,
    pub schema: u32,
    pub sig: String,
    #[serde(skip)]
    pub processed_at: Option<String>,
    /// TMP envelope: `direct` (default) or `bulletin` (R12/R13).
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Bulletin scope: the domain this message is posted to. `None` for
    /// direct messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Id of the message this one replaces (bulletin updates, R12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// TMP protocol version.
    #[serde(default = "default_tmp_version")]
    pub tmp_version: String,
    /// R15/4D: last time this message was created, replied to, superseded,
    /// or otherwise touched. Drives retention archival.
    #[serde(default)]
    pub last_touched_at: String,
    /// R15: set when retention archives the message. `None` = hot tier.
    #[serde(skip)]
    pub archived_at: Option<String>,
}

fn default_channel() -> String {
    CHANNEL_DIRECT.to_string()
}

fn default_tmp_version() -> String {
    TMP_VERSION.to_string()
}

#[allow(clippy::too_many_arguments)]
impl Message {
    pub fn new(
        id: String,
        from: String,
        to: String,
        msg_type: String,
        urgency: String,
        subject: String,
        body: String,
        re: Option<String>,
        task: Option<String>,
        paths: Vec<String>,
        created_at: String,
    ) -> Self {
        let mut msg = Self {
            id,
            from,
            to,
            msg_type,
            urgency,
            subject,
            body,
            re,
            task,
            paths,
            created_at,
            schema: 1,
            sig: String::new(),
            processed_at: None,
            channel: default_channel(),
            scope: None,
            supersedes: None,
            tmp_version: default_tmp_version(),
            last_touched_at: String::new(),
            archived_at: None,
        };
        // last_touched_at defaults to created_at (R15: creation is a touch).
        msg.last_touched_at = msg.created_at.clone();
        msg
    }
}

// ------------------------------------------------------------------- tasks
//
// C1: the durable work ledger. Messages remain the notification / protocol
// channel; these types are the first-class record of what work exists, who
// owns it, its readiness and dependencies, and (via delegations and reviews)
// how cross-domain assistance is authorized and accepted. Everything here is
// additive -- existing park / claim / message flows are unchanged.

pub const TASK_STATES: &[&str] = &[
    "draft", "ready", "active", "blocked", "review", "done", "cancelled",
];

/// Lifecycle state of a task in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Draft,
    Ready,
    Active,
    Blocked,
    Review,
    Done,
    Cancelled,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Draft => "draft",
            TaskState::Ready => "ready",
            TaskState::Active => "active",
            TaskState::Blocked => "blocked",
            TaskState::Review => "review",
            TaskState::Done => "done",
            TaskState::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "draft" => TaskState::Draft,
            "ready" => TaskState::Ready,
            "active" => TaskState::Active,
            "blocked" => TaskState::Blocked,
            "review" => TaskState::Review,
            "done" => TaskState::Done,
            "cancelled" => TaskState::Cancelled,
            _ => return None,
        })
    }
    /// True for closed states (no longer schedulable or assistable).
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskState::Done | TaskState::Cancelled)
    }
    /// True for the success terminal state (satisfies a dependency).
    pub fn is_done(&self) -> bool {
        matches!(self, TaskState::Done)
    }
}

/// Whether a task may be assisted by non-owner agents. C2 enforces this; C1
/// only records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AssistPolicy {
    /// Any eligible idle agent may offer to help (subject to owner approval).
    #[default]
    Open,
    /// Owner-only: not open to cross-domain assistance.
    Solo,
}

impl AssistPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            AssistPolicy::Open => "open",
            AssistPolicy::Solo => "solo",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => AssistPolicy::Open,
            "solo" => AssistPolicy::Solo,
            _ => return None,
        })
    }
}

/// A first-class unit of work in the ledger.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub owner_agent: String,
    /// Owning domain name (usually the same as `owner_agent`).
    pub domain: String,
    pub parent_task: Option<String>,
    pub subject: String,
    pub body: String,
    pub state: TaskState,
    /// Urgency using the same vocabulary as messages (see [`URGENCIES`]).
    pub priority: String,
    pub assist_policy: AssistPolicy,
    /// How many agents may work this task at once (>= 1). Single-helper
    /// subtasks use 1.
    pub desired_parallelism: u32,
    /// Path globs this task is scoped to (a subset of the owner's domain).
    pub path_scope: Vec<String>,
    /// Acceptance criteria, human/agent-readable.
    pub acceptance: Vec<String>,
    /// Task ids that must be `done` before this task becomes ready.
    pub blocked_by: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Task {
    /// True when every dependency in `blocked_by` is present in the given set
    /// of completed (done) task ids. A task with no dependencies is always
    /// satisfied.
    pub fn deps_satisfied(&self, done_ids: &std::collections::HashSet<String>) -> bool {
        self.blocked_by.iter().all(|d| done_ids.contains(d))
    }
}

/// Role an agent plays on a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRole {
    Owner,
    Helper,
    Reviewer,
    Integrator,
}

impl TaskRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskRole::Owner => "owner",
            TaskRole::Helper => "helper",
            TaskRole::Reviewer => "reviewer",
            TaskRole::Integrator => "integrator",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "owner" => TaskRole::Owner,
            "helper" => TaskRole::Helper,
            "reviewer" => TaskRole::Reviewer,
            "integrator" => TaskRole::Integrator,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TaskAssignment {
    pub task_id: String,
    pub agent: String,
    pub role: TaskRole,
    /// Free-form lifecycle marker for the assignment (e.g. "active",
    /// "completed"). Kept as a string so the assistance protocol (C2) can
    /// extend the vocabulary without a migration.
    pub state: String,
    pub offer_id: Option<String>,
    pub delegation_id: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Status of a delegation capability. C2 drives the transitions; C1 records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationStatus {
    Offered,
    Active,
    Revoked,
    Expired,
    Submitted,
    Accepted,
    Rejected,
}

impl DelegationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DelegationStatus::Offered => "offered",
            DelegationStatus::Active => "active",
            DelegationStatus::Revoked => "revoked",
            DelegationStatus::Expired => "expired",
            DelegationStatus::Submitted => "submitted",
            DelegationStatus::Accepted => "accepted",
            DelegationStatus::Rejected => "rejected",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "offered" => DelegationStatus::Offered,
            "active" => DelegationStatus::Active,
            "revoked" => DelegationStatus::Revoked,
            "expired" => DelegationStatus::Expired,
            "submitted" => DelegationStatus::Submitted,
            "accepted" => DelegationStatus::Accepted,
            "rejected" => DelegationStatus::Rejected,
            _ => return None,
        })
    }
    /// True while the delegation can still authorize writes.
    pub fn is_live(&self) -> bool {
        matches!(self, DelegationStatus::Active)
    }
}

/// A signed, expiring, revocable, task-scoped grant of write authority from a
/// domain owner to a helper. C1 stores it; C2 issues and enforces it.
#[derive(Debug, Clone)]
pub struct Delegation {
    pub id: String,
    pub task_id: String,
    pub owner_agent: String,
    pub helper_agent: String,
    pub scope: Vec<String>,
    pub allowed_ops: Vec<String>,
    /// Opaque constraint object, stored as a raw JSON string.
    pub constraints_json: String,
    pub base_revision: Option<String>,
    pub offer_message: String,
    pub grant_message: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub status: DelegationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Accept,
    RequestChanges,
    Reject,
}

impl ReviewDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewDecision::Accept => "accept",
            ReviewDecision::RequestChanges => "request-changes",
            ReviewDecision::Reject => "reject",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "accept" => ReviewDecision::Accept,
            "request-changes" => ReviewDecision::RequestChanges,
            "reject" => ReviewDecision::Reject,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TaskReview {
    pub id: String,
    pub task_id: String,
    pub delegation_id: Option<String>,
    pub reviewer_agent: String,
    pub submission_ref: String,
    pub decision: ReviewDecision,
    pub notes: String,
    pub created_at: String,
}

fn default_urgency() -> String {
    "normal".to_string()
}

#[derive(Debug, Clone)]
pub struct ParkedTask {
    pub task: String,
    pub agent: String,
    pub wait_type: String,
    pub wait_re: Option<String>,
    pub wait_path: Option<String>,
    pub waiting_on: String,
    pub resume_hint: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub path: String,
    pub holder: String,
    pub task: Option<String>,
    pub grant: Option<String>,
    /// Delegation capability backing a cross-domain lease. `None` preserves
    /// ordinary in-domain leases and legacy rows.
    pub delegation_id: Option<String>,
    pub acquired_at: String,
    pub expires_at: f64,
    pub expires_human: String,
    pub contested: bool,
}

#[derive(Debug, Clone)]
pub struct RunningLock {
    pub agent: String,
    pub pid: i32,
    pub started_at: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub id: String,
    pub agent: String,
    pub paths: Vec<String>,
    pub at: String,
}

/// A helper's validated Git submission. Pending submissions are deliberately
/// separate from `TaskReview`: a submission is evidence presented for review,
/// while a review is an owner's/reviewer's decision about that evidence.
#[derive(Debug, Clone)]
pub struct TaskSubmission {
    pub id: String,
    pub task_id: String,
    pub delegation_id: String,
    pub helper_agent: String,
    pub commit_ref: String,
    pub base_revision: String,
    pub summary: String,
    pub tests: String,
    pub changed_paths: Vec<String>,
    /// Signed `submission` notification linked to this row.
    pub message_id: String,
    /// `pending`, `changes-requested`, `accepted`, or `rejected`.
    pub status: String,
    pub created_at: String,
    pub reviewed_at: Option<String>,
}

// ---------------------------------------------------------- C3 scheduling state
//
// Durable anti-churn state for bounded assist discovery and derived agent
// activity states. These are scheduler/observability primitives, not work
// items.

/// Per-helper durable state for assist-discovery anti-churn.
#[derive(Debug, Clone)]
pub struct AssistDiscoveryState {
    pub helper_agent: String,
    pub last_discovery_at: Option<String>,
    pub cooldown_until: Option<String>,
    pub last_offered_fingerprint: String,
    pub last_offer_id: Option<String>,
    pub updated_at: String,
}

/// Per (helper, owner) exponential rejection backoff so a denied offer does
/// not immediately re-fire on every tick.
#[derive(Debug, Clone)]
pub struct AssistRejectionBackoff {
    pub helper_agent: String,
    pub owner_agent: String,
    pub rejection_count: u32,
    pub last_rejected_at: Option<String>,
    pub retry_after: Option<String>,
}

/// Why an agent should be woken, in scheduler priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeKind {
    Inbox,
    AbandonedPark,
    ReadyPark,
    CycleBreak,
    OwnedTask,
    HelperAssignment,
    AssistDiscovery,
}

impl WakeKind {
    /// Lower is higher priority.
    pub fn rank(self) -> u8 {
        match self {
            WakeKind::Inbox => 0,
            WakeKind::AbandonedPark => 1,
            WakeKind::ReadyPark => 2,
            WakeKind::CycleBreak => 3,
            WakeKind::OwnedTask => 4,
            WakeKind::HelperAssignment => 5,
            WakeKind::AssistDiscovery => 6,
        }
    }
}

/// A single planned wake in the deterministic scheduler plan.
#[derive(Debug, Clone)]
pub struct WakeCandidate {
    pub agent: String,
    pub kind: WakeKind,
    pub reason: String,
    /// Urgency rank: critical=3, high=2, normal=1, low=0, unknown=1.
    pub urgency_rank: u8,
    pub task_id: Option<String>,
    pub delegation_id: Option<String>,
    pub discovery_fingerprint: Option<String>,
    pub discovery_task_id: Option<String>,
}

/// Derived, read-only explanation of why an agent is in its current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivityState {
    Running,
    Blocked,
    OwnedWorkReady,
    HelpAssignmentReady,
    AvailableToHelp,
    ProjectComplete,
    Disabled,
    Idle,
}

impl AgentActivityState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentActivityState::Running => "running",
            AgentActivityState::Blocked => "blocked",
            AgentActivityState::OwnedWorkReady => "owned-work-ready",
            AgentActivityState::HelpAssignmentReady => "help-assignment-ready",
            AgentActivityState::AvailableToHelp => "available-to-help",
            AgentActivityState::ProjectComplete => "project-complete",
            AgentActivityState::Disabled => "disabled",
            AgentActivityState::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentStatus {
    pub agent: String,
    pub state: AgentActivityState,
    pub reason: String,
    pub task_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistance_protocol_message_types_are_explicit() {
        for kind in [
            "help-request",
            "help-offer",
            "help-accept",
            "help-deny",
            "help-revoke",
            "submission",
            "review-result",
        ] {
            assert!(MSG_TYPES.contains(&kind), "missing message type {kind}");
        }
    }

    #[test]
    fn wake_kind_rank_is_in_priority_order() {
        assert!(WakeKind::Inbox.rank() < WakeKind::AbandonedPark.rank());
        assert!(WakeKind::AbandonedPark.rank() < WakeKind::ReadyPark.rank());
        assert!(WakeKind::ReadyPark.rank() < WakeKind::CycleBreak.rank());
        assert!(WakeKind::CycleBreak.rank() < WakeKind::OwnedTask.rank());
        assert!(WakeKind::OwnedTask.rank() < WakeKind::HelperAssignment.rank());
        assert!(WakeKind::HelperAssignment.rank() < WakeKind::AssistDiscovery.rank());
    }

    #[test]
    fn agent_activity_state_as_str_is_stable() {
        assert_eq!(AgentActivityState::Running.as_str(), "running");
        assert_eq!(AgentActivityState::AvailableToHelp.as_str(), "available-to-help");
        assert_eq!(AgentActivityState::ProjectComplete.as_str(), "project-complete");
    }

    #[test]
    fn ui_keys_default_pane_nav_uses_guaranteed_function_keys() {
        // Function keys are sent identically by every terminal with no
        // configuration required (unlike Alt/Option+arrow, which depends on
        // per-terminal, often non-default settings), so these are the
        // baseline that must always be present and always work.
        let keys = UiKeys::default();
        assert_eq!(keys.pane_left, "F6");
        assert_eq!(keys.pane_right, "F7");
        assert_eq!(keys.pane_up, "F8");
        assert_eq!(keys.pane_down, "F9");
    }

    #[test]
    fn ui_keys_deserializes_old_config_missing_pane_nav_fields() {
        // An existing user's config.json from before pane_left/right/up/down
        // existed must still parse cleanly, with the new fields silently
        // defaulting rather than failing to deserialize.
        let old_json = r#"{
            "diagnostics": "F2",
            "inbox": "F3",
            "verbose_toggle": "F4",
            "diagnostic_view": "F5"
        }"#;
        let keys: UiKeys = serde_json::from_str(old_json).unwrap();
        assert_eq!(keys.diagnostics, "F2");
        assert_eq!(keys.pane_left, "F6");
        assert_eq!(keys.pane_right, "F7");
        assert_eq!(keys.pane_up, "F8");
        assert_eq!(keys.pane_down, "F9");
    }
}
