use thiserror::Error;

#[derive(Error, Debug)]
pub enum SchedulerError {
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),

    #[error("job not found: {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, SchedulerError>;
