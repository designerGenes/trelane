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
    pub pump: PumpConfig,
    pub claims: ClaimsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: AgentConfig::default(),
            launcher: LauncherConfig {
                template: r#"claude -p "$(cat {prompt_file})" --permission-mode acceptEdits --allowedTools "Bash(trelane *)" --max-turns 50"#
                    .to_string(),
                profiles: HashMap::new(),
            },
            pump: PumpConfig {
                interval_s: 20,
                max_concurrent: 2,
            },
            claims: ClaimsConfig {
                default_ttl_s: 900,
            },
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
pub struct PumpConfig {
    pub interval_s: u64,
    pub max_concurrent: usize,
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
