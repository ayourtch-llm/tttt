use crate::error::Result;
use crate::event::LogEvent;
use crate::LogSink;
use rusqlite::Connection;
use std::path::Path;

/// Logs events to a SQLite database with timestamped chunks.
pub struct SqliteLogger {
    conn: Connection,
}

impl SqliteLogger {
    /// Create a new SQLite logger, creating the database and schema if needed.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                direction TEXT NOT NULL,
                data BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_session
                ON events(session_id, timestamp_ms);",
        )?;
        Ok(Self { conn })
    }

    /// Create an in-memory SQLite logger (for testing).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                direction TEXT NOT NULL,
                data BLOB NOT NULL
            );
            CREATE INDEX idx_events_session
                ON events(session_id, timestamp_ms);",
        )?;
        Ok(Self { conn })
    }

    /// Query events for a session, ordered by timestamp.
    pub fn query_events(&self, session_id: &str) -> Result<Vec<LogEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, timestamp_ms, direction, data FROM events
             WHERE session_id = ?1 ORDER BY timestamp_ms",
        )?;
        let events = stmt
            .query_map([session_id], |row| {
                let session_id: String = row.get(0)?;
                let timestamp_ms: u64 = row.get(1)?;
                let direction_str: String = row.get(2)?;
                let data: Vec<u8> = row.get(3)?;
                let direction = match direction_str.as_str() {
                    "input" => crate::event::Direction::Input,
                    "output" => crate::event::Direction::Output,
                    _ => crate::event::Direction::Meta,
                };
                Ok(LogEvent {
                    timestamp_ms,
                    session_id,
                    direction,
                    data,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    /// Count total events in the database.
    pub fn event_count(&self) -> Result<usize> {
        let count: usize =
            self.conn
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count)
    }
}

impl LogSink for SqliteLogger {
    fn log_event(&mut self, event: &LogEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (session_id, timestamp_ms, direction, data)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                event.session_id,
                event.timestamp_ms,
                event.direction.as_str(),
                event.data,
            ],
        )?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        // SQLite writes are immediate (no buffering needed)
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Direction;

    #[test]
    fn test_sqlite_in_memory() {
        let logger = SqliteLogger::in_memory().unwrap();
        assert_eq!(logger.event_count().unwrap(), 0);
    }

    #[test]
    fn test_sqlite_creates_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let _logger = SqliteLogger::new(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_sqlite_writes_and_queries() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let event = LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Input, b"hello".to_vec());
        logger.log_event(&event).unwrap();

        let events = logger.query_events("s1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].timestamp_ms, 1000);
        assert_eq!(events[0].session_id, "s1");
        assert_eq!(events[0].direction, Direction::Input);
        assert_eq!(events[0].data, b"hello");
    }

    #[test]
    fn test_sqlite_multiple_events() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        for i in 0..10 {
            let event = LogEvent::with_timestamp(
                i * 100,
                "s1".to_string(),
                Direction::Output,
                format!("chunk-{}", i).into_bytes(),
            );
            logger.log_event(&event).unwrap();
        }
        assert_eq!(logger.event_count().unwrap(), 10);
        let events = logger.query_events("s1").unwrap();
        assert_eq!(events.len(), 10);
        // verify ordering
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event.timestamp_ms, (i as u64) * 100);
        }
    }

    #[test]
    fn test_sqlite_multiple_sessions() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_event(&LogEvent::with_timestamp(1, "s1".to_string(), Direction::Input, b"a".to_vec())).unwrap();
        logger.log_event(&LogEvent::with_timestamp(2, "s2".to_string(), Direction::Input, b"b".to_vec())).unwrap();
        logger.log_event(&LogEvent::with_timestamp(3, "s1".to_string(), Direction::Output, b"c".to_vec())).unwrap();

        let s1_events = logger.query_events("s1").unwrap();
        assert_eq!(s1_events.len(), 2);
        let s2_events = logger.query_events("s2").unwrap();
        assert_eq!(s2_events.len(), 1);
        assert_eq!(logger.event_count().unwrap(), 3);
    }

    #[test]
    fn test_sqlite_binary_data() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let binary: Vec<u8> = (0..=255).collect();
        let event = LogEvent::with_timestamp(1, "s1".to_string(), Direction::Output, binary.clone());
        logger.log_event(&event).unwrap();

        let events = logger.query_events("s1").unwrap();
        assert_eq!(events[0].data, binary);
    }

    #[test]
    fn test_sqlite_empty_query() {
        let logger = SqliteLogger::in_memory().unwrap();
        let events = logger.query_events("nonexistent").unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_sqlite_flush_is_noop() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.flush().unwrap(); // should not error
    }

    #[test]
    fn test_sqlite_schema_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let mut logger = SqliteLogger::new(&path).unwrap();
            logger.log_event(&LogEvent::with_timestamp(1, "s1".to_string(), Direction::Input, b"a".to_vec())).unwrap();
        }
        // Re-open same DB — should not fail
        let logger = SqliteLogger::new(&path).unwrap();
        assert_eq!(logger.event_count().unwrap(), 1);
    }
}
