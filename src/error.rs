use thiserror::Error;

#[derive(Debug, Error)]
pub enum TrelaneError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("glob pattern error: {0}")]
    Glob(#[from] globset::Error),

    #[error("{0}")]
    Msg(String),
}

impl TrelaneError {
    pub fn msg<S: Into<String>>(s: S) -> Self {
        TrelaneError::Msg(s.into())
    }

    /// The safety refusal returned when an agent has no explicitly-assigned
    /// launcher (model/profile) and would otherwise silently fall back to the
    /// built-in default launcher template -- which may be a real, paid CLI
    /// (e.g. Anthropic's `claude`) that bills the user's own account. Trelane
    /// must never spend a user's money without their explicit, per-agent
    /// choice, so this is refused rather than defaulted.
    pub fn launcher_not_configured(agent: &str) -> Self {
        TrelaneError::Msg(format!(
            "launcher-not-configured: agent '{agent}' has no launcher model assigned. \
             Refusing to auto-launch with the built-in default launcher, since it may \
             invoke a paid CLI and unintentionally bill your account. Assign a model for \
             this agent -- via the Biplane UI's model selector ('m' on its row), or \
             `trelane add-agent {agent} --launcher-agent <profile-or-model>` -- then retry."
        ))
    }

    /// True if this is the launcher-not-configured safety refusal above, as
    /// opposed to a genuine failure. Callers that process multiple agents
    /// (like squire's wake loop) use this to skip just the affected agent
    /// rather than aborting the whole batch.
    pub fn is_launcher_not_configured(&self) -> bool {
        matches!(self, TrelaneError::Msg(s) if s.starts_with("launcher-not-configured:"))
    }
}

pub type Result<T> = std::result::Result<T, TrelaneError>;
