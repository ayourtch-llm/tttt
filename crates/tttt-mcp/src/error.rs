use thiserror::Error;

#[derive(Error, Debug)]
pub enum McpError {
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PTY error: {0}")]
    Pty(#[from] tttt_pty::PtyError),

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("invalid params: {0}")]
    InvalidParams(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("scheduler error: {0}")]
    Scheduler(#[from] tttt_scheduler::SchedulerError),
}

pub type Result<T> = std::result::Result<T, McpError>;
