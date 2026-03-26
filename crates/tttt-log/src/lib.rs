mod error;
mod event;
mod multi;
mod replay;
mod sqlite;
mod text;

pub use error::{LogError, Result};
pub use event::{Direction, LogEvent, SessionInfo};
pub use multi::MultiLogger;
pub use replay::SessionReplay;
pub use sqlite::{SharedSqliteLogSink, SqliteLogger};
pub use text::TextLogger;

/// Trait for log sinks that receive terminal I/O events.
pub trait LogSink: Send {
    /// Log a single event.
    fn log_event(&mut self, event: &LogEvent) -> Result<()>;

    /// Flush any buffered data to storage.
    fn flush(&mut self) -> Result<()>;
}
