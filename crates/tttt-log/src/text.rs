use crate::error::Result;
use crate::event::LogEvent;
use crate::LogSink;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Logs events as JSON lines to per-session text files.
pub struct TextLogger {
    log_dir: PathBuf,
    files: std::collections::HashMap<String, File>,
}

impl TextLogger {
    /// Create a new text logger writing to the given directory.
    pub fn new(log_dir: impl AsRef<Path>) -> Result<Self> {
        let log_dir = log_dir.as_ref().to_path_buf();
        fs::create_dir_all(&log_dir)?;
        Ok(Self {
            log_dir,
            files: std::collections::HashMap::new(),
        })
    }

    /// Get or create the log file for a session.
    fn get_file(&mut self, session_id: &str) -> Result<&mut File> {
        if !self.files.contains_key(session_id) {
            let path = self.log_dir.join(format!("{}.jsonl", session_id));
            let file = File::options().create(true).append(true).open(path)?;
            self.files.insert(session_id.to_string(), file);
        }
        Ok(self.files.get_mut(session_id).unwrap())
    }

    /// Get the path where a session's log file would be written.
    pub fn log_path(&self, session_id: &str) -> PathBuf {
        self.log_dir.join(format!("{}.jsonl", session_id))
    }
}

impl LogSink for TextLogger {
    fn log_event(&mut self, event: &LogEvent) -> Result<()> {
        let file = self.get_file(&event.session_id)?;
        let json = serde_json::to_string(event)?;
        writeln!(file, "{}", json)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        for file in self.files.values_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Direction;

    #[test]
    fn test_text_logger_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("logs");
        let _logger = TextLogger::new(&sub).unwrap();
        assert!(sub.exists());
    }

    #[test]
    fn test_text_logger_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut logger = TextLogger::new(dir.path()).unwrap();
        let event = LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Input, b"hi".to_vec());
        logger.log_event(&event).unwrap();
        logger.flush().unwrap();
        let path = logger.log_path("s1");
        assert!(path.exists());
    }

    #[test]
    fn test_text_logger_writes_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut logger = TextLogger::new(dir.path()).unwrap();

        let e1 = LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Input, b"hello".to_vec());
        let e2 = LogEvent::with_timestamp(2000, "s1".to_string(), Direction::Output, b"world".to_vec());
        logger.log_event(&e1).unwrap();
        logger.log_event(&e2).unwrap();
        logger.flush().unwrap();

        let contents = fs::read_to_string(logger.log_path("s1")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed: LogEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.timestamp_ms, 1000);
        assert_eq!(parsed.direction, Direction::Input);
    }

    #[test]
    fn test_text_logger_separate_files_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut logger = TextLogger::new(dir.path()).unwrap();

        let e1 = LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Input, b"a".to_vec());
        let e2 = LogEvent::with_timestamp(2000, "s2".to_string(), Direction::Input, b"b".to_vec());
        logger.log_event(&e1).unwrap();
        logger.log_event(&e2).unwrap();
        logger.flush().unwrap();

        assert!(logger.log_path("s1").exists());
        assert!(logger.log_path("s2").exists());
    }

    #[test]
    fn test_text_logger_flush_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut logger = TextLogger::new(dir.path()).unwrap();
        let event = LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Input, b"data".to_vec());
        logger.log_event(&event).unwrap();
        logger.flush().unwrap();
        // File should be readable after flush
        let contents = fs::read_to_string(logger.log_path("s1")).unwrap();
        assert!(!contents.is_empty());
    }
}
