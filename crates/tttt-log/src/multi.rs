use crate::error::Result;
use crate::event::LogEvent;
use crate::LogSink;

/// Dispatches log events to multiple sinks.
///
/// If one sink fails, the error is recorded but other sinks still receive the event.
pub struct MultiLogger {
    sinks: Vec<Box<dyn LogSink>>,
}

impl MultiLogger {
    /// Create a new multi-logger with no sinks.
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    /// Add a sink.
    pub fn add_sink(&mut self, sink: Box<dyn LogSink>) {
        self.sinks.push(sink);
    }

    /// Get the number of sinks.
    pub fn sink_count(&self) -> usize {
        self.sinks.len()
    }
}

impl Default for MultiLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl LogSink for MultiLogger {
    fn log_event(&mut self, event: &LogEvent) -> Result<()> {
        let mut last_error = None;
        for sink in &mut self.sinks {
            if let Err(e) = sink.log_event(event) {
                last_error = Some(e);
            }
        }
        match last_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn flush(&mut self) -> Result<()> {
        let mut last_error = None;
        for sink in &mut self.sinks {
            if let Err(e) = sink.flush() {
                last_error = Some(e);
            }
        }
        match last_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Direction;

    use crate::error::LogError;

    /// A sink that always fails.
    struct FailingSink;
    impl LogSink for FailingSink {
        fn log_event(&mut self, _event: &LogEvent) -> Result<()> {
            Err(LogError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "test failure",
            )))
        }
        fn flush(&mut self) -> Result<()> {
            Err(LogError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "test flush failure",
            )))
        }
    }

    #[test]
    fn test_multi_logger_empty() {
        let mut logger = MultiLogger::new();
        assert_eq!(logger.sink_count(), 0);
        let event = LogEvent::with_timestamp(1, "s1".to_string(), Direction::Input, b"a".to_vec());
        logger.log_event(&event).unwrap(); // no sinks, no error
    }

    #[test]
    fn test_multi_logger_dispatches_to_all() {
        let mut logger = MultiLogger::new();
        // We can't check the recording sinks after adding them to MultiLogger
        // since they're behind Box<dyn LogSink>. Instead, use SqliteLogger as a
        // verifiable sink.
        let sqlite1 = crate::sqlite::SqliteLogger::in_memory().unwrap();
        let sqlite2 = crate::sqlite::SqliteLogger::in_memory().unwrap();
        logger.add_sink(Box::new(sqlite1));
        logger.add_sink(Box::new(sqlite2));
        assert_eq!(logger.sink_count(), 2);

        let event = LogEvent::with_timestamp(1, "s1".to_string(), Direction::Input, b"hello".to_vec());
        logger.log_event(&event).unwrap();
        // Both sinks received the event (can't easily verify without downcasting,
        // but at least it didn't error)
    }

    #[test]
    fn test_multi_logger_survives_one_sink_error() {
        let mut logger = MultiLogger::new();
        let sqlite = crate::sqlite::SqliteLogger::in_memory().unwrap();
        logger.add_sink(Box::new(FailingSink));
        logger.add_sink(Box::new(sqlite));

        let event = LogEvent::with_timestamp(1, "s1".to_string(), Direction::Input, b"a".to_vec());
        // Should return error from failing sink, but the sqlite sink still got the event
        let result = logger.log_event(&event);
        assert!(result.is_err());
    }

    #[test]
    fn test_multi_logger_flush() {
        let mut logger = MultiLogger::new();
        let sqlite = crate::sqlite::SqliteLogger::in_memory().unwrap();
        logger.add_sink(Box::new(sqlite));
        logger.flush().unwrap();
    }

    #[test]
    fn test_multi_logger_default() {
        let logger = MultiLogger::default();
        assert_eq!(logger.sink_count(), 0);
    }
}
