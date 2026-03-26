use crate::error::Result;
use crate::event::{LogEvent, SessionInfo};
use crate::LogSink;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
                ON events(session_id, timestamp_ms);
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                command TEXT NOT NULL,
                cols INTEGER NOT NULL,
                rows INTEGER NOT NULL,
                started_at_ms INTEGER NOT NULL,
                ended_at_ms INTEGER,
                name TEXT
            );",
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
                ON events(session_id, timestamp_ms);
            CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                command TEXT NOT NULL,
                cols INTEGER NOT NULL,
                rows INTEGER NOT NULL,
                started_at_ms INTEGER NOT NULL,
                ended_at_ms INTEGER,
                name TEXT
            );",
        )?;
        Ok(Self { conn })
    }

    /// Open a read-only connection to an existing SQLite database (for replay).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
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

    /// Record the start of a session.
    pub fn log_session_start(
        &mut self,
        session_id: &str,
        command: &str,
        cols: u16,
        rows: u16,
        name: Option<&str>,
    ) -> Result<()> {
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (session_id, command, cols, rows, started_at_ms, ended_at_ms, name)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
            rusqlite::params![session_id, command, cols, rows, started_at_ms, name],
        )?;
        Ok(())
    }

    /// Record the end of a session, setting ended_at_ms to the current time.
    pub fn log_session_end(&mut self, session_id: &str) -> Result<()> {
        let ended_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.conn.execute(
            "UPDATE sessions SET ended_at_ms = ?1 WHERE session_id = ?2",
            rusqlite::params![ended_at_ms, session_id],
        )?;
        Ok(())
    }

    /// List all sessions ordered by start time.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, command, cols, rows, started_at_ms, ended_at_ms, name
             FROM sessions ORDER BY started_at_ms",
        )?;
        let sessions = stmt
            .query_map([], |row| {
                Ok(SessionInfo {
                    session_id: row.get(0)?,
                    command: row.get(1)?,
                    cols: row.get::<_, u16>(2)?,
                    rows: row.get::<_, u16>(3)?,
                    started_at_ms: row.get(4)?,
                    ended_at_ms: row.get(5)?,
                    name: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(sessions)
    }

    /// Get info for a specific session by ID.
    pub fn get_session_info(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, command, cols, rows, started_at_ms, ended_at_ms, name
             FROM sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map([session_id], |row| {
            Ok(SessionInfo {
                session_id: row.get(0)?,
                command: row.get(1)?,
                cols: row.get::<_, u16>(2)?,
                rows: row.get::<_, u16>(3)?,
                started_at_ms: row.get(4)?,
                ended_at_ms: row.get(5)?,
                name: row.get(6)?,
            })
        })?;
        match rows.next() {
            Some(Ok(info)) => Ok(Some(info)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
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

/// A thread-safe wrapper around `SqliteLogger` that implements `LogSink`.
pub struct SharedSqliteLogSink(pub Arc<Mutex<SqliteLogger>>);

impl LogSink for SharedSqliteLogSink {
    fn log_event(&mut self, event: &LogEvent) -> Result<()> {
        self.0.lock().unwrap().log_event(event)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.lock().unwrap().flush()
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

    // --- Session metadata tests ---

    #[test]
    fn test_session_start_and_query() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_session_start("s1", "bash", 80, 24, None).unwrap();

        let info = logger.get_session_info("s1").unwrap().unwrap();
        assert_eq!(info.session_id, "s1");
        assert_eq!(info.command, "bash");
        assert_eq!(info.cols, 80);
        assert_eq!(info.rows, 24);
        assert!(info.started_at_ms > 0);
        assert!(info.ended_at_ms.is_none());
        assert!(info.name.is_none());
    }

    #[test]
    fn test_session_end_sets_timestamp() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_session_start("s1", "zsh", 100, 30, None).unwrap();
        let info_before = logger.get_session_info("s1").unwrap().unwrap();
        assert!(info_before.ended_at_ms.is_none());

        logger.log_session_end("s1").unwrap();
        let info_after = logger.get_session_info("s1").unwrap().unwrap();
        assert!(info_after.ended_at_ms.is_some());
        assert!(info_after.ended_at_ms.unwrap() >= info_after.started_at_ms);
    }

    #[test]
    fn test_list_sessions_empty() {
        let logger = SqliteLogger::in_memory().unwrap();
        let sessions = logger.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_multiple() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_session_start("s1", "bash", 80, 24, None).unwrap();
        logger.log_session_start("s2", "zsh", 120, 40, Some("my shell")).unwrap();
        logger.log_session_start("s3", "fish", 60, 20, None).unwrap();

        let sessions = logger.list_sessions().unwrap();
        assert_eq!(sessions.len(), 3);
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains(&"s1"));
        assert!(ids.contains(&"s2"));
        assert!(ids.contains(&"s3"));
    }

    #[test]
    fn test_session_name_field() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_session_start("s1", "bash", 80, 24, Some("dev session")).unwrap();

        let info = logger.get_session_info("s1").unwrap().unwrap();
        assert_eq!(info.name, Some("dev session".to_string()));
    }

    #[test]
    fn test_get_session_info_nonexistent() {
        let logger = SqliteLogger::in_memory().unwrap();
        let info = logger.get_session_info("nope").unwrap();
        assert!(info.is_none());
    }

    #[test]
    fn test_sessions_schema_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let mut logger = SqliteLogger::new(&path).unwrap();
            logger.log_session_start("s1", "bash", 80, 24, None).unwrap();
        }
        // Re-open — should not fail, sessions table still exists
        let logger = SqliteLogger::new(&path).unwrap();
        let sessions = logger.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
    }

    #[test]
    fn test_open_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.db");
        {
            let mut logger = SqliteLogger::new(&path).unwrap();
            logger.log_session_start("s1", "bash", 80, 24, Some("test")).unwrap();
            logger.log_event(&LogEvent::with_timestamp(
                1, "s1".to_string(), Direction::Output, b"hi".to_vec(),
            )).unwrap();
        }
        let ro = SqliteLogger::open_read_only(&path).unwrap();
        let sessions = ro.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        let events = ro.query_events("s1").unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_shared_sink_log_event() {
        let logger = Arc::new(Mutex::new(SqliteLogger::in_memory().unwrap()));
        let mut sink = SharedSqliteLogSink(Arc::clone(&logger));
        let event = LogEvent::with_timestamp(42, "s1".to_string(), Direction::Output, b"data".to_vec());
        sink.log_event(&event).unwrap();
        sink.flush().unwrap();

        let count = logger.lock().unwrap().event_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_shared_sink_multiple_events() {
        let logger = Arc::new(Mutex::new(SqliteLogger::in_memory().unwrap()));
        let mut sink = SharedSqliteLogSink(Arc::clone(&logger));
        for i in 0..5u64 {
            sink.log_event(&LogEvent::with_timestamp(
                i * 10, "s1".to_string(), Direction::Output, vec![i as u8],
            )).unwrap();
        }
        let count = logger.lock().unwrap().event_count().unwrap();
        assert_eq!(count, 5);
    }
}
