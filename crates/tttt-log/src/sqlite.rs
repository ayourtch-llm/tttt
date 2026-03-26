use crate::error::Result;
use crate::event::{LogEvent, SessionInfo};
use crate::LogSink;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Check whether a named column exists in a table.
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({})", table);
    conn.prepare(&sql)
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(1))
                .ok()
                .map(|iter| iter.filter_map(|r| r.ok()).any(|n| n == column))
        })
        .unwrap_or(false)
}

/// Logs events to a SQLite database with timestamped chunks.
pub struct SqliteLogger {
    conn: Connection,
    /// PID of the current process. None for read-only connections.
    pid: Option<u32>,
    /// Whether the events table has a pid column (migration applied).
    events_has_pid: bool,
    /// Whether the sessions table has a pid column (migration applied).
    sessions_has_pid: bool,
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
        // Schema migrations: add pid column if absent.
        if !column_exists(&conn, "events", "pid") {
            let _ = conn.execute("ALTER TABLE events ADD COLUMN pid INTEGER", []);
        }
        if !column_exists(&conn, "sessions", "pid") {
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN pid INTEGER", []);
        }
        Ok(Self {
            conn,
            pid: Some(std::process::id()),
            events_has_pid: true,
            sessions_has_pid: true,
        })
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
                data BLOB NOT NULL,
                pid INTEGER
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
                name TEXT,
                pid INTEGER
            );",
        )?;
        Ok(Self {
            conn,
            pid: Some(std::process::id()),
            events_has_pid: true,
            sessions_has_pid: true,
        })
    }

    /// Open a read-only connection to an existing SQLite database (for replay).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let events_has_pid = column_exists(&conn, "events", "pid");
        let sessions_has_pid = column_exists(&conn, "sessions", "pid");
        Ok(Self {
            conn,
            pid: None,
            events_has_pid,
            sessions_has_pid,
        })
    }

    /// Query events for a session, ordered by timestamp.
    pub fn query_events(&self, session_id: &str) -> Result<Vec<LogEvent>> {
        self.query_events_with_pid(session_id, None)
    }

    /// Query events for a session filtered by optional PID, ordered by timestamp.
    /// If pid is None, returns all events for the session_id (legacy behaviour).
    pub fn query_events_with_pid(
        &self,
        session_id: &str,
        pid: Option<u32>,
    ) -> Result<Vec<LogEvent>> {
        if let Some(p) = pid {
            if self.events_has_pid {
                let mut stmt = self.conn.prepare(
                    "SELECT session_id, timestamp_ms, direction, data FROM events
                     WHERE session_id = ?1 AND pid = ?2 ORDER BY timestamp_ms",
                )?;
                let events = stmt.query_map(rusqlite::params![session_id, p], Self::map_event_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                return Ok(events);
            }
            // pid column absent – fall through to unfiltered query
        }
        let mut stmt = self.conn.prepare(
            "SELECT session_id, timestamp_ms, direction, data FROM events
             WHERE session_id = ?1 ORDER BY timestamp_ms",
        )?;
        let events = stmt.query_map([session_id], Self::map_event_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(events)
    }

    fn map_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogEvent> {
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
            "INSERT OR REPLACE INTO sessions (session_id, command, cols, rows, started_at_ms, ended_at_ms, name, pid)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7)",
            rusqlite::params![session_id, command, cols, rows, started_at_ms, name, self.pid],
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

    /// Returns true if the sessions table exists in this database.
    fn has_sessions_table(&self) -> bool {
        self.conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='sessions'",
                [],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// List all sessions ordered by start time.
    /// Returns an empty Vec if the sessions table does not exist.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        if !self.has_sessions_table() {
            return Ok(Vec::new());
        }
        if self.sessions_has_pid {
            let mut stmt = self.conn.prepare(
                "SELECT session_id, command, cols, rows, started_at_ms, ended_at_ms, name, pid
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
                        pid: row.get(7)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(sessions)
        } else {
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
                        pid: None,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(sessions)
        }
    }

    /// List (session_id, pid) pairs that have events but no entry in the sessions table.
    /// If the sessions table does not exist, returns ALL distinct (session_id, pid) pairs from events.
    /// Different PIDs with the same session_id appear as separate entries.
    pub fn list_orphan_session_ids(&self) -> Result<Vec<(String, Option<u32>)>> {
        if self.events_has_pid {
            let sql = if self.has_sessions_table() {
                "SELECT session_id, pid FROM events
                 WHERE session_id NOT IN (SELECT session_id FROM sessions)
                 GROUP BY session_id, pid
                 ORDER BY session_id, pid"
            } else {
                "SELECT session_id, pid FROM events
                 GROUP BY session_id, pid
                 ORDER BY session_id, pid"
            };
            let mut stmt = self.conn.prepare(sql)?;
            let pairs = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<u32>>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(pairs)
        } else {
            // Legacy DB without pid column – return session_id with None pid
            let sql = if self.has_sessions_table() {
                "SELECT DISTINCT session_id FROM events
                 WHERE session_id NOT IN (SELECT session_id FROM sessions)
                 ORDER BY session_id"
            } else {
                "SELECT DISTINCT session_id FROM events ORDER BY session_id"
            };
            let mut stmt = self.conn.prepare(sql)?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(ids.into_iter().map(|id| (id, None)).collect())
        }
    }

    /// Build a synthetic SessionInfo for an orphan session from its events.
    /// Returns None if the session has no events matching the given pid filter.
    /// Pass pid=None to match all events (legacy behaviour).
    pub fn infer_session_info(
        &self,
        session_id: &str,
        pid: Option<u32>,
    ) -> Result<Option<SessionInfo>> {
        let row: Option<(u64, u64)> = if let Some(p) = pid {
            if self.events_has_pid {
                self.conn
                    .query_row(
                        "SELECT MIN(timestamp_ms), MAX(timestamp_ms) FROM events WHERE session_id = ?1 AND pid = ?2",
                        rusqlite::params![session_id, p],
                        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
                    )
                    .ok()
            } else {
                self.conn
                    .query_row(
                        "SELECT MIN(timestamp_ms), MAX(timestamp_ms) FROM events WHERE session_id = ?1",
                        [session_id],
                        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
                    )
                    .ok()
            }
        } else {
            self.conn
                .query_row(
                    "SELECT MIN(timestamp_ms), MAX(timestamp_ms) FROM events WHERE session_id = ?1",
                    [session_id],
                    |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
                )
                .ok()
        };
        match row {
            None => Ok(None),
            Some((min_ts, max_ts)) => Ok(Some(SessionInfo {
                session_id: session_id.to_string(),
                command: "unknown".to_string(),
                cols: 80,
                rows: 24,
                started_at_ms: min_ts,
                ended_at_ms: Some(max_ts),
                name: None,
                pid,
            })),
        }
    }

    /// Time gap threshold used to split NULL-pid orphan sessions: 1 hour.
    const GAP_THRESHOLD_MS: u64 = 60 * 60 * 1000;

    /// Expose the gap threshold for tests in other crates.
    pub fn gap_threshold_ms() -> u64 {
        Self::GAP_THRESHOLD_MS
    }

    /// Insert an event with an explicit NULL pid.  Intended for test code that needs to
    /// simulate legacy data recorded before the pid column was added.
    #[doc(hidden)]
    pub fn insert_raw_event_null_pid(
        &self,
        session_id: &str,
        timestamp_ms: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (session_id, timestamp_ms, direction, data, pid)
             VALUES (?1, ?2, 'output', X'68', NULL)",
            rusqlite::params![session_id, timestamp_ms],
        )?;
        Ok(())
    }

    /// Split a single (session_id, NULL-pid) orphan into chunks separated by gaps
    /// longer than `GAP_THRESHOLD_MS`.  Returns one `SessionInfo` per chunk.
    fn split_null_pid_orphan_into_chunks(&self, session_id: &str) -> Result<Vec<SessionInfo>> {
        let sql = if self.events_has_pid {
            "SELECT timestamp_ms FROM events
             WHERE session_id = ?1 AND pid IS NULL
             ORDER BY timestamp_ms"
        } else {
            "SELECT timestamp_ms FROM events
             WHERE session_id = ?1
             ORDER BY timestamp_ms"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let timestamps: Vec<u64> = stmt
            .query_map([session_id], |row| row.get::<_, u64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if timestamps.is_empty() {
            return Ok(Vec::new());
        }

        let mut chunks: Vec<(u64, u64)> = Vec::new();
        let mut chunk_start = timestamps[0];
        let mut chunk_end = timestamps[0];

        for &ts in &timestamps[1..] {
            if ts.saturating_sub(chunk_end) > Self::GAP_THRESHOLD_MS {
                chunks.push((chunk_start, chunk_end));
                chunk_start = ts;
            }
            chunk_end = ts;
        }
        chunks.push((chunk_start, chunk_end));

        Ok(chunks
            .into_iter()
            .map(|(start_ms, end_ms)| SessionInfo {
                session_id: session_id.to_string(),
                command: "unknown".to_string(),
                cols: 80,
                rows: 24,
                started_at_ms: start_ms,
                ended_at_ms: Some(end_ms),
                name: None,
                pid: None,
            })
            .collect())
    }

    /// Build the full list of orphan session chunks, ready for display.
    ///
    /// - Orphans with a non-NULL pid → one `SessionInfo` per (session_id, pid) pair.
    /// - Orphans with NULL pid → split by time gaps > 5 min; one `SessionInfo` per chunk.
    pub fn list_orphan_session_chunks(&self) -> Result<Vec<SessionInfo>> {
        let mut result = Vec::new();
        for (session_id, pid) in self.list_orphan_session_ids()? {
            if pid.is_some() {
                if let Some(info) = self.infer_session_info(&session_id, pid)? {
                    result.push(info);
                }
            } else {
                let chunks = self.split_null_pid_orphan_into_chunks(&session_id)?;
                result.extend(chunks);
            }
        }
        Ok(result)
    }

    /// Query events for a session whose pid is NULL, restricted to a timestamp range.
    /// Used to replay individual time-gap-split chunks of legacy NULL-pid orphan sessions.
    /// The bounds are inclusive on both ends.
    /// `to_ms` is capped at `i64::MAX` to stay within SQLite's signed integer range.
    pub fn query_events_in_range(
        &self,
        session_id: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Vec<LogEvent>> {
        let to_ms = to_ms.min(i64::MAX as u64);
        if self.events_has_pid {
            let mut stmt = self.conn.prepare(
                "SELECT session_id, timestamp_ms, direction, data FROM events
                 WHERE session_id = ?1 AND pid IS NULL
                   AND timestamp_ms >= ?2 AND timestamp_ms <= ?3
                 ORDER BY timestamp_ms",
            )?;
            let events = stmt
                .query_map(rusqlite::params![session_id, from_ms, to_ms], Self::map_event_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(events)
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT session_id, timestamp_ms, direction, data FROM events
                 WHERE session_id = ?1
                   AND timestamp_ms >= ?2 AND timestamp_ms <= ?3
                 ORDER BY timestamp_ms",
            )?;
            let events = stmt
                .query_map(rusqlite::params![session_id, from_ms, to_ms], Self::map_event_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(events)
        }
    }

    /// Get info for a specific session by ID.
    /// Returns None if the sessions table does not exist or the session is not found.
    pub fn get_session_info(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        if !self.has_sessions_table() {
            return Ok(None);
        }
        if self.sessions_has_pid {
            let mut stmt = self.conn.prepare(
                "SELECT session_id, command, cols, rows, started_at_ms, ended_at_ms, name, pid
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
                    pid: row.get(7)?,
                })
            })?;
            match rows.next() {
                Some(Ok(info)) => Ok(Some(info)),
                Some(Err(e)) => Err(e.into()),
                None => Ok(None),
            }
        } else {
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
                    pid: None,
                })
            })?;
            match rows.next() {
                Some(Ok(info)) => Ok(Some(info)),
                Some(Err(e)) => Err(e.into()),
                None => Ok(None),
            }
        }
    }
}

impl LogSink for SqliteLogger {
    fn log_event(&mut self, event: &LogEvent) -> Result<()> {
        if self.events_has_pid {
            self.conn.execute(
                "INSERT INTO events (session_id, timestamp_ms, direction, data, pid)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    event.session_id,
                    event.timestamp_ms,
                    event.direction.as_str(),
                    event.data,
                    self.pid,
                ],
            )?;
        } else {
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
        }
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
        // pid should be Some (current process)
        assert!(info.pid.is_some());
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

    // --- Legacy DB helper (events table only, no sessions table, no pid column) ---

    fn legacy_in_memory() -> SqliteLogger {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                direction TEXT NOT NULL,
                data BLOB NOT NULL
            );
            CREATE INDEX idx_events_session ON events(session_id, timestamp_ms);",
        )
        .unwrap();
        SqliteLogger { conn, pid: None, events_has_pid: false, sessions_has_pid: false }
    }

    // --- has_sessions_table tests ---

    #[test]
    fn test_has_sessions_table_true() {
        let logger = SqliteLogger::in_memory().unwrap();
        assert!(logger.has_sessions_table());
    }

    #[test]
    fn test_has_sessions_table_false() {
        let logger = legacy_in_memory();
        assert!(!logger.has_sessions_table());
    }

    // --- Graceful fallback on legacy DB ---

    #[test]
    fn test_list_sessions_legacy_db_returns_empty() {
        let logger = legacy_in_memory();
        let sessions = logger.list_sessions().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_get_session_info_legacy_db_returns_none() {
        let logger = legacy_in_memory();
        let info = logger.get_session_info("any-id").unwrap();
        assert!(info.is_none());
    }

    #[test]
    fn test_list_orphan_session_ids_legacy_db_returns_all() {
        let mut logger = legacy_in_memory();
        logger.log_event(&LogEvent::with_timestamp(1, "s1".to_string(), Direction::Output, b"a".to_vec())).unwrap();
        logger.log_event(&LogEvent::with_timestamp(2, "s2".to_string(), Direction::Output, b"b".to_vec())).unwrap();
        let orphans = logger.list_orphan_session_ids().unwrap();
        assert_eq!(orphans.len(), 2);
        let ids: Vec<&str> = orphans.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"s1"));
        assert!(ids.contains(&"s2"));
        // Legacy DB has no pid column, so pids should all be None
        assert!(orphans.iter().all(|(_, p)| p.is_none()));
    }

    #[test]
    fn test_list_orphan_session_ids_legacy_db_empty_events() {
        let logger = legacy_in_memory();
        let orphans = logger.list_orphan_session_ids().unwrap();
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_infer_session_info_legacy_db() {
        let mut logger = legacy_in_memory();
        logger.log_event(&LogEvent::with_timestamp(1000, "orphan".to_string(), Direction::Output, b"hi".to_vec())).unwrap();
        let info = logger.infer_session_info("orphan", None).unwrap().unwrap();
        assert_eq!(info.session_id, "orphan");
        assert_eq!(info.command, "unknown");
        assert_eq!(info.started_at_ms, 1000);
    }

    #[test]
    fn test_open_read_only_legacy_db_list_sessions_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        // Create a DB with only the events table (simulating pre-sessions-table era)
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (
                    id INTEGER PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL,
                    data BLOB NOT NULL
                );",
            ).unwrap();
            conn.execute(
                "INSERT INTO events (session_id, timestamp_ms, direction, data) VALUES ('s1', 1000, 'output', X'68656c6c6f')",
                [],
            ).unwrap();
        }
        let ro = SqliteLogger::open_read_only(&path).unwrap();
        // Should not crash
        let sessions = ro.list_sessions().unwrap();
        assert!(sessions.is_empty());
        let info = ro.get_session_info("s1").unwrap();
        assert!(info.is_none());
        let orphans = ro.list_orphan_session_ids().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].0, "s1");
        assert_eq!(orphans[0].1, None);
    }

    // --- Orphan session tests ---

    #[test]
    fn test_list_orphan_session_ids_none() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_session_start("s1", "bash", 80, 24, None).unwrap();
        logger.log_event(&LogEvent::with_timestamp(1, "s1".to_string(), Direction::Output, b"hi".to_vec())).unwrap();
        // s1 has a sessions entry, so no orphans
        let orphans = logger.list_orphan_session_ids().unwrap();
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_list_orphan_session_ids_found() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        // s1 has events but no sessions entry
        logger.log_event(&LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Output, b"hello".to_vec())).unwrap();
        logger.log_event(&LogEvent::with_timestamp(2000, "s1".to_string(), Direction::Output, b" world".to_vec())).unwrap();
        let orphans = logger.list_orphan_session_ids().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].0, "s1");
        // pid should be Some (current process wrote these events)
        assert!(orphans[0].1.is_some());
    }

    #[test]
    fn test_list_orphan_session_ids_mixed() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        // registered session
        logger.log_session_start("registered", "bash", 80, 24, None).unwrap();
        logger.log_event(&LogEvent::with_timestamp(1, "registered".to_string(), Direction::Output, b"ok".to_vec())).unwrap();
        // orphan sessions
        logger.log_event(&LogEvent::with_timestamp(10, "orphan-a".to_string(), Direction::Output, b"a".to_vec())).unwrap();
        logger.log_event(&LogEvent::with_timestamp(20, "orphan-b".to_string(), Direction::Output, b"b".to_vec())).unwrap();
        let orphans = logger.list_orphan_session_ids().unwrap();
        assert_eq!(orphans.len(), 2);
        let ids: Vec<&str> = orphans.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"orphan-a"));
        assert!(ids.contains(&"orphan-b"));
    }

    #[test]
    fn test_list_orphan_session_ids_empty_db() {
        let logger = SqliteLogger::in_memory().unwrap();
        let orphans = logger.list_orphan_session_ids().unwrap();
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_infer_session_info_no_events() {
        let logger = SqliteLogger::in_memory().unwrap();
        let info = logger.infer_session_info("nonexistent", None).unwrap();
        assert!(info.is_none());
    }

    #[test]
    fn test_infer_session_info_basic() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_event(&LogEvent::with_timestamp(1000, "orphan".to_string(), Direction::Output, b"hello".to_vec())).unwrap();
        logger.log_event(&LogEvent::with_timestamp(3000, "orphan".to_string(), Direction::Output, b"bye".to_vec())).unwrap();

        let info = logger.infer_session_info("orphan", None).unwrap().unwrap();
        assert_eq!(info.session_id, "orphan");
        assert_eq!(info.command, "unknown");
        assert_eq!(info.cols, 80);
        assert_eq!(info.rows, 24);
        assert_eq!(info.started_at_ms, 1000);
        assert_eq!(info.ended_at_ms, Some(3000));
        assert!(info.name.is_none());
    }

    #[test]
    fn test_infer_session_info_single_event() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        logger.log_event(&LogEvent::with_timestamp(5000, "solo".to_string(), Direction::Input, b"x".to_vec())).unwrap();

        let info = logger.infer_session_info("solo", None).unwrap().unwrap();
        assert_eq!(info.started_at_ms, 5000);
        assert_eq!(info.ended_at_ms, Some(5000));
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

    // --- PID deduplication tests ---

    #[test]
    fn test_pid_stored_in_events() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let pid = logger.pid.unwrap();
        logger.log_event(&LogEvent::with_timestamp(1, "s1".to_string(), Direction::Output, b"x".to_vec())).unwrap();

        // Query the raw pid stored in the event
        let stored_pid: Option<u32> = logger.conn.query_row(
            "SELECT pid FROM events WHERE session_id = 's1'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(stored_pid, Some(pid));
    }

    #[test]
    fn test_pid_stored_in_sessions() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let pid = logger.pid.unwrap();
        logger.log_session_start("s1", "bash", 80, 24, None).unwrap();

        let info = logger.get_session_info("s1").unwrap().unwrap();
        assert_eq!(info.pid, Some(pid));
    }

    #[test]
    fn test_query_events_with_pid_filters_by_pid() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let current_pid = logger.pid.unwrap();

        // Inject events as if from a different pid
        let other_pid: u32 = current_pid.wrapping_add(1);
        logger.conn.execute(
            "INSERT INTO events (session_id, timestamp_ms, direction, data, pid) VALUES ('s1', 100, 'output', X'61', ?1)",
            [other_pid],
        ).unwrap();
        // Write a real event from current pid
        logger.log_event(&LogEvent::with_timestamp(200, "s1".to_string(), Direction::Output, b"b".to_vec())).unwrap();

        // query_events returns all
        assert_eq!(logger.query_events("s1").unwrap().len(), 2);
        // query_events_with_pid(current) returns only 1
        assert_eq!(logger.query_events_with_pid("s1", Some(current_pid)).unwrap().len(), 1);
        // query_events_with_pid(other) returns only 1
        assert_eq!(logger.query_events_with_pid("s1", Some(other_pid)).unwrap().len(), 1);
    }

    #[test]
    fn test_orphan_dedup_across_pids() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let current_pid = logger.pid.unwrap();
        let other_pid: u32 = current_pid.wrapping_add(999);

        // Two runs both use session_id "pty-1"
        logger.log_event(&LogEvent::with_timestamp(100, "pty-1".to_string(), Direction::Output, b"run1".to_vec())).unwrap();
        logger.conn.execute(
            "INSERT INTO events (session_id, timestamp_ms, direction, data, pid) VALUES ('pty-1', 200, 'output', X'72756e32', ?1)",
            [other_pid],
        ).unwrap();

        let orphans = logger.list_orphan_session_ids().unwrap();
        // Should see two separate entries for the same session_id but different pids
        assert_eq!(orphans.len(), 2);
        assert!(orphans.iter().all(|(id, _)| id == "pty-1"));
        let pids: Vec<Option<u32>> = orphans.iter().map(|(_, p)| *p).collect();
        assert!(pids.contains(&Some(current_pid)));
        assert!(pids.contains(&Some(other_pid)));
    }

    #[test]
    fn test_infer_session_info_filters_by_pid() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let current_pid = logger.pid.unwrap();
        let other_pid: u32 = current_pid.wrapping_add(999);

        logger.log_event(&LogEvent::with_timestamp(1000, "pty-1".to_string(), Direction::Output, b"run1".to_vec())).unwrap();
        logger.conn.execute(
            "INSERT INTO events (session_id, timestamp_ms, direction, data, pid) VALUES ('pty-1', 5000, 'output', X'72756e32', ?1)",
            [other_pid],
        ).unwrap();

        let info_current = logger.infer_session_info("pty-1", Some(current_pid)).unwrap().unwrap();
        assert_eq!(info_current.started_at_ms, 1000);
        assert_eq!(info_current.ended_at_ms, Some(1000));

        let info_other = logger.infer_session_info("pty-1", Some(other_pid)).unwrap().unwrap();
        assert_eq!(info_other.started_at_ms, 5000);
        assert_eq!(info_other.ended_at_ms, Some(5000));
    }

    #[test]
    fn test_migration_adds_pid_column_to_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pre_pid.db");
        // Create DB without pid column (simulates old schema)
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (
                    id INTEGER PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL,
                    data BLOB NOT NULL
                );
                CREATE TABLE sessions (
                    session_id TEXT PRIMARY KEY,
                    command TEXT NOT NULL,
                    cols INTEGER NOT NULL,
                    rows INTEGER NOT NULL,
                    started_at_ms INTEGER NOT NULL,
                    ended_at_ms INTEGER,
                    name TEXT
                );",
            ).unwrap();
            conn.execute(
                "INSERT INTO events (session_id, timestamp_ms, direction, data) VALUES ('s1', 1, 'output', X'61')",
                [],
            ).unwrap();
        }
        // Open with SqliteLogger::new — should run migration
        let logger = SqliteLogger::new(&path).unwrap();
        assert!(logger.events_has_pid);
        assert!(logger.sessions_has_pid);
        // Old event should be queryable; its pid column is NULL
        let events = logger.query_events("s1").unwrap();
        assert_eq!(events.len(), 1);
    }

    // --- Gap-based chunk splitting tests ---

    /// Helper: insert a NULL-pid event (simulates legacy data).
    fn insert_null_pid_event(logger: &SqliteLogger, session_id: &str, ts: u64) {
        logger.insert_raw_event_null_pid(session_id, ts).unwrap();
    }

    #[test]
    fn test_no_split_when_continuous() {
        let logger = SqliteLogger::in_memory().unwrap();
        // Three events within 5 minutes of each other
        insert_null_pid_event(&logger, "pty-1", 1_000);
        insert_null_pid_event(&logger, "pty-1", 100_000);
        insert_null_pid_event(&logger, "pty-1", 200_000);

        let chunks = logger.split_null_pid_orphan_into_chunks("pty-1").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].started_at_ms, 1_000);
        assert_eq!(chunks[0].ended_at_ms, Some(200_000));
    }

    #[test]
    fn test_chunk_splitting_on_time_gap() {
        let logger = SqliteLogger::in_memory().unwrap();
        // Two clusters separated by > 5 minutes
        let gap = SqliteLogger::GAP_THRESHOLD_MS + 1;
        insert_null_pid_event(&logger, "pty-1", 1_000);
        insert_null_pid_event(&logger, "pty-1", 60_000);
        insert_null_pid_event(&logger, "pty-1", 60_000 + gap);
        insert_null_pid_event(&logger, "pty-1", 60_000 + gap + 10_000);

        let chunks = logger.split_null_pid_orphan_into_chunks("pty-1").unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].started_at_ms, 1_000);
        assert_eq!(chunks[0].ended_at_ms, Some(60_000));
        assert_eq!(chunks[1].started_at_ms, 60_000 + gap);
        assert_eq!(chunks[1].ended_at_ms, Some(60_000 + gap + 10_000));
        // All chunks have pid=None
        assert!(chunks.iter().all(|c| c.pid.is_none()));
    }

    #[test]
    fn test_chunk_splitting_exact_gap_boundary_not_split() {
        let logger = SqliteLogger::in_memory().unwrap();
        // Gap exactly equal to threshold — should NOT split
        insert_null_pid_event(&logger, "pty-1", 0);
        insert_null_pid_event(&logger, "pty-1", SqliteLogger::GAP_THRESHOLD_MS);

        let chunks = logger.split_null_pid_orphan_into_chunks("pty-1").unwrap();
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_chunk_splitting_multiple_gaps() {
        let logger = SqliteLogger::in_memory().unwrap();
        let g = SqliteLogger::GAP_THRESHOLD_MS + 1;
        insert_null_pid_event(&logger, "s1", 0);
        insert_null_pid_event(&logger, "s1", g);
        insert_null_pid_event(&logger, "s1", g * 2);

        let chunks = logger.split_null_pid_orphan_into_chunks("s1").unwrap();
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn test_chunk_splitting_single_event() {
        let logger = SqliteLogger::in_memory().unwrap();
        insert_null_pid_event(&logger, "s1", 5_000);

        let chunks = logger.split_null_pid_orphan_into_chunks("s1").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].started_at_ms, 5_000);
        assert_eq!(chunks[0].ended_at_ms, Some(5_000));
    }

    #[test]
    fn test_chunk_splitting_empty() {
        let logger = SqliteLogger::in_memory().unwrap();
        let chunks = logger.split_null_pid_orphan_into_chunks("no-events").unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_list_orphan_session_chunks_pid_aware_unchanged() {
        // A pid-aware orphan should produce exactly one chunk (the pid-based inferred info)
        let mut logger = SqliteLogger::in_memory().unwrap();
        let pid = logger.pid.unwrap();
        logger.log_event(&LogEvent::with_timestamp(1000, "pty-1".to_string(), Direction::Output, b"x".to_vec())).unwrap();

        let chunks = logger.list_orphan_session_chunks().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].pid, Some(pid));
    }

    #[test]
    fn test_list_orphan_session_chunks_null_pid_split() {
        let logger = SqliteLogger::in_memory().unwrap();
        let g = SqliteLogger::GAP_THRESHOLD_MS + 1;
        insert_null_pid_event(&logger, "pty-1", 1_000);
        insert_null_pid_event(&logger, "pty-1", 1_000 + g);

        let chunks = logger.list_orphan_session_chunks().unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.pid.is_none()));
    }

    #[test]
    fn test_query_events_in_range() {
        let logger = SqliteLogger::in_memory().unwrap();
        insert_null_pid_event(&logger, "s1", 1_000);
        insert_null_pid_event(&logger, "s1", 5_000);
        insert_null_pid_event(&logger, "s1", 10_000);

        let events = logger.query_events_in_range("s1", 1_000, 5_000).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].timestamp_ms, 1_000);
        assert_eq!(events[1].timestamp_ms, 5_000);
    }

    #[test]
    fn test_query_events_in_range_inclusive_bounds() {
        let logger = SqliteLogger::in_memory().unwrap();
        insert_null_pid_event(&logger, "s1", 100);
        insert_null_pid_event(&logger, "s1", 200);
        insert_null_pid_event(&logger, "s1", 300);

        // Exact boundary match
        let events = logger.query_events_in_range("s1", 100, 100).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].timestamp_ms, 100);
    }

    #[test]
    fn test_query_events_in_range_excludes_pid_events() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        // One event from this pid (non-null pid)
        logger.log_event(&LogEvent::with_timestamp(5_000, "s1".to_string(), Direction::Output, b"pid".to_vec())).unwrap();
        // One event with null pid
        insert_null_pid_event(&logger, "s1", 5_000);

        let events = logger.query_events_in_range("s1", 0, i64::MAX as u64).unwrap();
        // Only the NULL-pid event should appear
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_query_events_in_range_empty() {
        let logger = SqliteLogger::in_memory().unwrap();
        let events = logger.query_events_in_range("no-such", 0, i64::MAX as u64).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_null_pid_chunks_not_mixed_with_pid_chunks() {
        let mut logger = SqliteLogger::in_memory().unwrap();
        let current_pid = logger.pid.unwrap();
        let g = SqliteLogger::GAP_THRESHOLD_MS + 1;

        // Pid-aware event in run A
        logger.log_event(&LogEvent::with_timestamp(1_000, "pty-1".to_string(), Direction::Output, b"a".to_vec())).unwrap();
        // Two NULL-pid events forming separate chunks
        insert_null_pid_event(&logger, "pty-1", 2_000_000);
        insert_null_pid_event(&logger, "pty-1", 2_000_000 + g);

        let chunks = logger.list_orphan_session_chunks().unwrap();
        // 1 pid-aware chunk + 2 null-pid chunks = 3 total
        assert_eq!(chunks.len(), 3);
        let pid_chunks: Vec<_> = chunks.iter().filter(|c| c.pid == Some(current_pid)).collect();
        let null_chunks: Vec<_> = chunks.iter().filter(|c| c.pid.is_none()).collect();
        assert_eq!(pid_chunks.len(), 1);
        assert_eq!(null_chunks.len(), 2);
    }
}
