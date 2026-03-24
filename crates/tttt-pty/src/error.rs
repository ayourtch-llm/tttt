use thiserror::Error;

#[derive(Error, Debug)]
pub enum PtyError {
    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("max sessions reached: {0}")]
    MaxSessionsReached(usize),

    #[error("session name already taken: {0}")]
    DuplicateName(String),

    #[error("session already exited")]
    SessionExited,

    #[error("pty I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("pty spawn error: {0}")]
    Spawn(String),

    #[error("resize error: {0}")]
    Resize(String),
}

pub type Result<T> = std::result::Result<T, PtyError>;
