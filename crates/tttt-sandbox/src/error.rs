use thiserror::Error;

#[derive(Error, Debug)]
pub enum SandboxError {
    #[error("path not found: {0}")]
    PathNotFound(String),

    #[error("path canonicalization failed: {0}")]
    Canonicalize(#[from] std::io::Error),

    #[error("sandbox not supported on this platform")]
    NotSupported,

    #[error("sandbox apply failed: {0}")]
    ApplyFailed(String),
}

pub type Result<T> = std::result::Result<T, SandboxError>;
