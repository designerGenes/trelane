use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const MSG_TYPES: &[&str] = &[
    "question",
    "answer",
    "info",
    "claim-request",
    "claim-grant",
    "claim-deny",
    "handoff",
    "system",
];

pub const URGENCIES: &[&str] = &["low", "normal", "high", "critical"];

pub const TRELANE_DIR: &str = ".trelane";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub agents: AgentConfig,
    pub launcher: LauncherConfig,
    /// The prop (formerly "pump"). `alias = "pump"` keeps pre-0.3 config
    /// files loading without migration.
    #[serde(alias = "pump")]
    pub prop: PropConfig,
    pub claims: ClaimsConfig,
    #[serde(default)]
    pub ui: UiConfig,
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
            prop: PropConfig {
                interval_s: 20,
                max_concurrent: 2,
            },
            claims: ClaimsConfig {
                default_ttl_s: 900,
            },
            ui: UiConfig::default(),
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
pub struct PropConfig {
    pub interval_s: u64,
    pub max_concurrent: usize,
}

/// Deprecated name for [`PropConfig`], kept so external code compiles.
pub type PumpConfig = PropConfig;

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
    /// Toggle verbose prop output for the session frame.
    pub verbose_toggle: String,
}

impl Default for UiKeys {
    fn default() -> Self {
        Self {
            diagnostics: "F2".to_string(),
            inbox: "F3".to_string(),
            verbose_toggle: "F4".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimsConfig {
    pub default_ttl_s: u64,
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
        Self {
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
        }
    }
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
