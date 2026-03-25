use crate::backend::PtyBackend;
use crate::error::{PtyError, Result};
use crate::keys::process_special_keys;
use crate::screen::ScreenBuffer;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
    last_output_time: Instant,
    last_input_time: Instant,
    capture_file: Option<std::fs::File>,
    capture_path: Option<String>,
    capture_bytes: u64,
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
            last_output_time: Instant::now(),
            last_input_time: Instant::now(),
            capture_file: None,
            capture_path: None,
            capture_bytes: 0,
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
            self.last_output_time = Instant::now();
            if let Some(ref mut file) = self.capture_file {
                file.write_all(&buf[..n])?;
                self.capture_bytes += n as u64;
            }
        }
        self.update_status()?;
        Ok((n, buf[..n].to_vec()))
    }

    /// Begin capturing raw PTY output to a temp file.
    /// Returns (capture_id, file_path). Only one capture per session is allowed at a time.
    pub fn start_capture(&mut self) -> Result<(String, String)> {
        if self.capture_file.is_some() {
            return Err(PtyError::CaptureAlreadyActive);
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let capture_id = format!("{}-{}", self.id, ts);
        let file_path = format!("/tmp/tttt-capture-{}.raw", capture_id);
        let file = std::fs::File::create(&file_path)?;
        self.capture_file = Some(file);
        self.capture_path = Some(file_path.clone());
        self.capture_bytes = 0;
        Ok((capture_id, file_path))
    }

    /// Stop capturing raw PTY output.
    /// Returns (file_path, bytes_written).
    pub fn stop_capture(&mut self) -> Result<(String, u64)> {
        if self.capture_file.is_none() {
            return Err(PtyError::NoCaptureActive);
        }
        let file_path = self.capture_path.take().unwrap_or_default();
        let bytes = self.capture_bytes;
        self.capture_file = None;
        self.capture_bytes = 0;
        Ok((file_path, bytes))
    }

    /// Send keys to the PTY, processing special key sequences.
    pub fn send_keys(&mut self, keys: &str) -> Result<()> {
        if self.status != SessionStatus::Running {
            return Err(PtyError::SessionExited);
        }
        self.last_input_time = Instant::now();
        let data = process_special_keys(keys);
        self.backend.write(&data)
    }

    /// Send raw bytes to the PTY without special key processing.
    pub fn send_raw(&mut self, data: &[u8]) -> Result<()> {
        if self.status != SessionStatus::Running {
            return Err(PtyError::SessionExited);
        }
        self.last_input_time = Instant::now();
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

    /// Feed raw bytes directly into the screen buffer (bypassing the PTY backend).
    /// Used to replay saved screen state after a live reload.
    pub fn inject_screen_data(&mut self, data: &[u8]) {
        self.screen.process(data);
    }

    /// Access the screen buffer directly.
    pub fn screen(&self) -> &ScreenBuffer {
        &self.screen
    }

    /// Access the PTY backend (for getting raw fd, etc.).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Access the PTY backend mutably (for testing and live-reload state injection).
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Get scrollback buffer contents (text that scrolled off the visible screen).
    pub fn get_scrollback(&self, max_lines: usize) -> Vec<String> {
        self.screen.get_scrollback(max_lines)
    }

    /// Set scrollback buffer depth.
    pub fn set_scrollback(&mut self, lines: usize) {
        self.screen.set_scrollback(lines);
    }

    /// Seconds since the session last produced any output.
    pub fn idle_seconds(&self) -> f64 {
        self.last_output_time.elapsed().as_secs_f64()
    }

    /// Seconds since the session last received any keyboard input.
    pub fn input_idle_seconds(&self) -> f64 {
        self.last_input_time.elapsed().as_secs_f64()
    }

    /// The last non-empty line of visible screen content, trimmed.
    /// Returns an empty string if the screen is blank.
    pub fn last_non_empty_line(&self) -> String {
        let contents = self.screen.contents();
        for line in contents.lines().rev() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        String::new()
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
    fn test_session_inject_screen_data() {
        let mut session = make_session();
        session.inject_screen_data(b"injected content");
        assert!(session.get_screen().contains("injected content"));
    }

    #[test]
    fn test_session_inject_screen_data_with_ansi() {
        let mut session = make_session();
        session.inject_screen_data(b"\x1b[1mBOLD\x1b[0m normal");
        let screen = session.get_screen();
        assert!(screen.contains("BOLD"));
        assert!(screen.contains("normal"));
    }

    #[test]
    fn test_start_capture_returns_capture_id_and_path() {
        let mut session = make_session();
        let (capture_id, file_path) = session.start_capture().unwrap();
        assert!(!capture_id.is_empty());
        assert!(file_path.starts_with("/tmp/tttt-capture-"));
        assert!(file_path.ends_with(".raw"));
        // cleanup
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_start_capture_twice_returns_error() {
        let mut session = make_session();
        let (_, file_path) = session.start_capture().unwrap();
        let result = session.start_capture();
        assert!(matches!(result, Err(PtyError::CaptureAlreadyActive)));
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_stop_capture_without_start_returns_error() {
        let mut session = make_session();
        let result = session.stop_capture();
        assert!(matches!(result, Err(PtyError::NoCaptureActive)));
    }

    #[test]
    fn test_capture_writes_pty_output() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"captured data");
        let mut session = PtySession::new("cap1".to_string(), mock, "bash".to_string(), 80, 24);
        let (_, file_path) = session.start_capture().unwrap();
        session.pump().unwrap();
        let (returned_path, bytes) = session.stop_capture().unwrap();
        assert_eq!(returned_path, file_path);
        assert_eq!(bytes, 13);
        let contents = std::fs::read(&file_path).unwrap();
        assert_eq!(contents, b"captured data");
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_capture_stops_writing_after_stop() {
        // Pump while capturing, stop, then pump again — only first bytes in file.
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"before");
        let mut session = PtySession::new("cap2".to_string(), mock, "bash".to_string(), 80, 24);
        let (_, file_path) = session.start_capture().unwrap();
        // Pump once — captures "before"
        session.pump().unwrap();
        let (_, bytes) = session.stop_capture().unwrap();
        assert_eq!(bytes, 6);
        // Queue more output and pump after stop — should NOT be written to file
        session.backend_mut().queue_output(b"after");
        session.pump().unwrap();
        let contents = std::fs::read(&file_path).unwrap();
        assert_eq!(contents, b"before");
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_capture_empty_session_zero_bytes() {
        let mut session = make_session();
        let (_, file_path) = session.start_capture().unwrap();
        session.pump().unwrap();
        let (_, bytes) = session.stop_capture().unwrap();
        assert_eq!(bytes, 0);
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_idle_seconds_initially_small() {
        let session = make_session();
        assert!(session.idle_seconds() < 1.0);
    }

    #[test]
    fn test_idle_seconds_resets_after_output() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"hello");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        std::thread::sleep(std::time::Duration::from_millis(20));
        session.pump().unwrap();
        // After pumping output, idle_seconds should be near zero
        assert!(session.idle_seconds() < 0.5);
    }

    #[test]
    fn test_last_non_empty_line_with_content() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"first line\r\nsecond line\r\n");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        session.pump().unwrap();
        assert_eq!(session.last_non_empty_line(), "second line");
    }

    #[test]
    fn test_last_non_empty_line_skips_trailing_empty() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"some text\r\n\r\n\r\n");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        session.pump().unwrap();
        assert_eq!(session.last_non_empty_line(), "some text");
    }

    #[test]
    fn test_last_non_empty_line_empty_screen() {
        let session = make_session();
        assert_eq!(session.last_non_empty_line(), "");
    }

    #[test]
    fn test_last_non_empty_line_single_line() {
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"prompt$ ");
        let mut session = PtySession::new("t1".to_string(), mock, "bash".to_string(), 80, 24);
        session.pump().unwrap();
        assert_eq!(session.last_non_empty_line(), "prompt$");
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
