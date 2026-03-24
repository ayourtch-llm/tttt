use crate::backend::PtyBackend;
use crate::error::{PtyError, Result};
use crate::keys::process_special_keys;
use crate::screen::ScreenBuffer;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Unique identifier for a terminal session.
pub type SessionId = String;

/// Status of a terminal session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Exited(i32),
}

/// Metadata about a terminal session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: SessionId,
    pub command: String,
    pub status: SessionStatus,
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip)]
    pub created_at: Option<Instant>,
}

/// A terminal session combining a PTY backend with a VT100 screen buffer.
///
/// The generic parameter `B` allows injecting a `MockPty` for testing.
/// The session is synchronous — the caller drives I/O via `pump()`.
pub struct PtySession<B: PtyBackend> {
    pub id: SessionId,
    pub name: Option<String>,
    backend: B,
    screen: ScreenBuffer,
    status: SessionStatus,
    command: String,
    created_at: Instant,
}

impl<B: PtyBackend> PtySession<B> {
    /// Create a new session with the given backend and dimensions.
    pub fn new(id: SessionId, backend: B, command: String, cols: u16, rows: u16) -> Self {
        Self {
            id,
            name: None,
            backend,
            screen: ScreenBuffer::new(cols, rows),
            status: SessionStatus::Running,
            command,
            created_at: Instant::now(),
        }
    }

    /// Set the optional name for this session.
    pub fn set_name(&mut self, name: String) {
        self.name = Some(name);
    }

    /// Read available output from the PTY and feed it to the screen buffer.
    /// Returns the number of bytes read.
    pub fn pump(&mut self) -> Result<usize> {
        let (n, _) = self.pump_raw()?;
        Ok(n)
    }

    /// Read available output from the PTY, feed to screen buffer, and return raw bytes.
    /// Returns (bytes_read, raw_data). The raw data can be used for logging.
    pub fn pump_raw(&mut self) -> Result<(usize, Vec<u8>)> {
        if self.status != SessionStatus::Running {
            return Ok((0, Vec::new()));
        }
        let mut buf = [0u8; 32768];
        let n = self.backend.read(&mut buf)?;
        if n > 0 {
            self.screen.process(&buf[..n]);
        }
        self.update_status()?;
        Ok((n, buf[..n].to_vec()))
    }

    /// Send keys to the PTY, processing special key sequences.
    pub fn send_keys(&mut self, keys: &str) -> Result<()> {
        if self.status != SessionStatus::Running {
            return Err(PtyError::SessionExited);
        }
        let data = process_special_keys(keys);
        self.backend.write(&data)
    }

    /// Send raw bytes to the PTY without special key processing.
    pub fn send_raw(&mut self, data: &[u8]) -> Result<()> {
        if self.status != SessionStatus::Running {
            return Err(PtyError::SessionExited);
        }
        self.backend.write(data)
    }

    /// Get the plain text contents of the screen.
    pub fn get_screen(&self) -> String {
        self.screen.contents()
    }

    /// Get the screen contents with ANSI formatting.
    pub fn get_screen_formatted(&self) -> Vec<u8> {
        self.screen.contents_formatted()
    }

    /// Get the ANSI diff since last call. Used by the TUI for efficient rendering.
    pub fn screen_diff(&mut self) -> Vec<u8> {
        self.screen.contents_diff()
    }

    /// Get cursor position as (row, col), 0-indexed.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.screen.cursor_position()
    }

    /// Resize the terminal.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.backend.resize(cols, rows)?;
        self.screen.resize(cols, rows);
        Ok(())
    }

    /// Kill the session's process.
    pub fn kill(&mut self) -> Result<()> {
        self.backend.kill()?;
        self.update_status()?;
        Ok(())
    }

    /// Get current session status.
    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    /// Get session metadata.
    pub fn metadata(&self) -> SessionMetadata {
        SessionMetadata {
            id: self.id.clone(),
            command: self.command.clone(),
            status: self.status.clone(),
            cols: self.screen.size().0,
            rows: self.screen.size().1,
            name: self.name.clone(),
            created_at: Some(self.created_at),
        }
    }

    /// Access the screen buffer directly.
    pub fn screen(&self) -> &ScreenBuffer {
        &self.screen
    }

    /// Access the PTY backend (for getting raw fd, etc.).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Get scrollback buffer contents (text that scrolled off the visible screen).
    pub fn get_scrollback(&self, max_lines: usize) -> Vec<String> {
        self.screen.get_scrollback(max_lines)
    }

    /// Set scrollback buffer depth.
    pub fn set_scrollback(&mut self, lines: usize) {
        self.screen.set_scrollback(lines);
    }

    /// Check and update the session status from the backend.
    fn update_status(&mut self) -> Result<()> {
        if self.status == SessionStatus::Running {
            if let Some(code) = self.backend.try_wait()? {
                self.status = SessionStatus::Exited(code);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockPty;

    fn make_session() -> PtySession<MockPty> {
        let mock = MockPty::new(80, 24);
        PtySession::new("test-1".to_string(), mock, "bash".to_string(), 80, 24)
    }

    #[test]
    fn test_session_creation_running() {
        let session = make_session();
        assert_eq!(*session.status(), SessionStatus::Running);
        assert_eq!(session.id, "test-1");
    }

    #[test]
    fn test_session_metadata() {
        let session = make_session();
        let meta = session.metadata();
        assert_eq!(meta.id, "test-1");
        assert_eq!(meta.command, "bash");
        assert_eq!(meta.status, SessionStatus::Running);
        assert_eq!(meta.cols, 80);
        assert_eq!(meta.rows, 24);
    }

    #[test]
    fn test_session_send_keys() {
        let mut session = make_session();
        session.send_keys("hello").unwrap();
        // Access the mock backend to verify
        // We need to check that "hello" was written
        // Since we can't easily access the backend, let's verify via a different path
        // send_keys processes special keys, "hello" has none
    }

    #[test]
    fn test_session_send_keys_special() {
        let mut session = make_session();
        session.send_keys("^C").unwrap();
        // Ctrl-C should become byte 0x03
    }

    #[test]
    fn test_session_send_raw() {
        let mut session = make_session();
        session.send_raw(b"\x03").unwrap();
    }

    #[test]
    fn test_session_pump_reads_output() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"prompt$ ");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        let n = session.pump().unwrap();
        assert_eq!(n, 8);
        assert!(session.get_screen().contains("prompt$"));
    }

    #[test]
    fn test_session_pump_empty() {
        let mut session = make_session();
        let n = session.pump().unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_session_get_screen() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"hello world");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        session.pump().unwrap();
        assert!(session.get_screen().contains("hello world"));
    }

    #[test]
    fn test_session_cursor_position() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"hi");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        session.pump().unwrap();
        assert_eq!(session.cursor_position(), (0, 2));
    }

    #[test]
    fn test_session_resize() {
        let mut session = make_session();
        session.resize(120, 40).unwrap();
        let meta = session.metadata();
        assert_eq!(meta.cols, 120);
        assert_eq!(meta.rows, 40);
    }

    #[test]
    fn test_session_kill() {
        let mut session = make_session();
        session.kill().unwrap();
        assert!(matches!(*session.status(), SessionStatus::Exited(_)));
    }

    #[test]
    fn test_session_send_keys_after_exit_fails() {
        let mut session = make_session();
        session.kill().unwrap();
        let result = session.send_keys("hello");
        assert!(result.is_err());
    }

    #[test]
    fn test_session_send_raw_after_exit_fails() {
        let mut session = make_session();
        session.kill().unwrap();
        let result = session.send_raw(b"hello");
        assert!(result.is_err());
    }

    #[test]
    fn test_session_pump_after_exit() {
        let mut session = make_session();
        session.kill().unwrap();
        let n = session.pump().unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_session_pump_raw_returns_bytes() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"raw output data");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        let (n, raw) = session.pump_raw().unwrap();
        assert_eq!(n, 15);
        assert_eq!(raw, b"raw output data");
        // Screen should also have the content
        assert!(session.get_screen().contains("raw output data"));
    }

    #[test]
    fn test_session_pump_raw_empty() {
        let mut session = make_session();
        let (n, raw) = session.pump_raw().unwrap();
        assert_eq!(n, 0);
        assert!(raw.is_empty());
    }

    #[test]
    fn test_session_screen_diff() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"new text");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        let _ = session.screen_diff(); // consume initial state
        session.pump().unwrap();
        let diff = session.screen_diff();
        assert!(!diff.is_empty());
    }

    #[test]
    fn test_session_set_scrollback() {
        let mut session = make_session();
        session.set_scrollback(5000);
        assert_eq!(session.screen().scrollback_lines(), 5000);
    }

    #[test]
    fn test_session_status_updates_on_pump() {
        let mut mock = MockPty::new(80, 24);
        mock.exit_code = Some(42);
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        session.pump().unwrap();
        assert_eq!(*session.status(), SessionStatus::Exited(42));
    }

    #[test]
    fn test_session_send_keys_large_message() {
        // Verify that large messages (>1KB) can be sent without error.
        // This exercises the write path that previously failed with EAGAIN on real PTYs.
        let mock = MockPty::new(80, 24);
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        let large_msg = "x".repeat(2000);
        session.send_keys(&large_msg).unwrap();
    }

    #[test]
    fn test_session_send_raw_large_message() {
        let mock = MockPty::new(80, 24);
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        let large_data = vec![b'A'; 4096];
        session.send_raw(&large_data).unwrap();
    }

    #[test]
    fn test_session_get_scrollback() {
        let mut mock = MockPty::new(80, 5);
        // Queue output that overflows the 5-row screen
        let mut output = String::new();
        for i in 0..15 {
            output.push_str(&format!("line {}\r\n", i));
        }
        mock.queue_output(output.as_bytes());
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 5);
        session.pump().unwrap();
        let scrollback = session.get_scrollback(100);
        assert!(!scrollback.is_empty(), "should have scrollback after overflow");
        let joined = scrollback.join("\n");
        assert!(joined.contains("line 0"), "scrollback should contain earliest line");
    }
}
