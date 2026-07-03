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
}

pub type Result<T> = std::result::Result<T, TrelaneError>;
