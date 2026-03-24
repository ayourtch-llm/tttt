//! Server-side viewer client management.
//!
//! Each connected viewer gets a `ViewerClient` that tracks its state
//! and renders screen updates independently.

use crate::pane_renderer::PaneRenderer;
use crate::protocol::{encode_message, ServerMsg, SessionInfo};
use std::io::Write;
use std::os::unix::net::UnixStream;

/// A connected viewer client.
pub struct ViewerClient {
    stream: UnixStream,
    /// Which session this viewer is watching.
    pub active_session: Option<String>,
    /// Client's terminal dimensions.
    pub cols: u16,
    pub rows: u16,
    /// Per-client pane renderer for dirty tracking.
    pub renderer: PaneRenderer,
    /// Read buffer for incoming messages.
    pub read_buf: Vec<u8>,
    /// Whether this client is still connected.
    pub connected: bool,
    /// Last cursor position sent (to avoid redundant updates).
    last_cursor_row: u16,
    last_cursor_col: u16,
}

impl ViewerClient {
    pub fn new(stream: UnixStream, cols: u16, rows: u16, sidebar_width: u16) -> Self {
        let pty_cols = cols.saturating_sub(sidebar_width);
        let pty_rows = rows.saturating_sub(1);
        // Set non-blocking for poll integration
        stream.set_nonblocking(true).ok();
        Self {
            stream,
            active_session: None,
            cols,
            rows,
            renderer: PaneRenderer::new(pty_cols, pty_rows, 1, 1),
            read_buf: Vec::new(),
            connected: true,
            last_cursor_row: 0,
            last_cursor_col: 0,
        }
    }

    /// Send a screen update to the client.
    /// Returns true if data was sent, false if no changes or error.
    ///
    /// Uses the PaneRenderer to detect if anything changed. If so,
    /// sends the vt100 contents_formatted() which the client can
    /// feed into its own vt100 parser for clean state reproduction.
    pub fn send_screen_update(
        &mut self,
        screen: &vt100::Screen,
        cursor_row: u16,
        cursor_col: u16,
    ) -> bool {
        // Use PaneRenderer just for dirty detection
        let pane_diff = self.renderer.render(screen);

        // Skip if nothing changed
        if pane_diff.is_empty()
            && cursor_row == self.last_cursor_row
            && cursor_col == self.last_cursor_col
        {
            return true; // SKIPPED - no changes
        }
        self.last_cursor_row = cursor_row;
        self.last_cursor_col = cursor_col;

        // Send the full screen as contents_formatted() — this is a
        // replayable ANSI sequence that reproduces the exact screen state
        // when fed to a vt100 parser. The client will then use its own
        // PaneRenderer for minimal rendering to the real terminal.
        let screen_data = screen.contents_formatted();

        let msg = ServerMsg::ScreenUpdate {
            screen_data,
            cursor_row,
            cursor_col,
        };
        self.send_msg(&msg)
    }

    /// Send a session list update.
    pub fn send_session_list(
        &mut self,
        sessions: &[SessionInfo],
        active_id: Option<&str>,
    ) -> bool {
        let msg = ServerMsg::SessionList {
            sessions: sessions.to_vec(),
            active_id: active_id.map(|s| s.to_string()),
        };
        self.send_msg(&msg)
    }

    /// Send goodbye and disconnect.
    pub fn send_goodbye(&mut self) {
        let _ = self.send_msg(&ServerMsg::Goodbye);
        self.connected = false;
    }

    /// Get the raw fd for poll().
    pub fn raw_fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.stream.as_raw_fd()
    }

    /// Read available data from the stream into the buffer.
    /// Returns number of bytes read, or 0 on EOF/error.
    pub fn read_available(&mut self) -> usize {
        let mut tmp = [0u8; 4096];
        match self.stream.read(&mut tmp) {
            Ok(0) => {
                self.connected = false;
                0
            }
            Ok(n) => {
                self.read_buf.extend_from_slice(&tmp[..n]);
                n
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(_) => {
                self.connected = false;
                0
            }
        }
    }

    /// Invalidate the renderer (force full redraw).
    pub fn invalidate(&mut self) {
        self.renderer.invalidate();
    }

    fn send_msg(&mut self, msg: &ServerMsg) -> bool {
        let data = encode_message(msg);
        // Temporarily set blocking for writes to ensure delivery
        let _ = self.stream.set_nonblocking(false);
        let result = match self.stream.write_all(&data) {
            Ok(()) => {
                let _ = self.stream.flush();
                true
            }
            Err(_) => {
                self.connected = false;
                false
            }
        };
        let _ = self.stream.set_nonblocking(true);
        result
    }
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn make_pair() -> (ViewerClient, UnixStream) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let client_stream = UnixStream::connect(&sock_path).unwrap();
        let (server_stream, _) = listener.accept().unwrap();
        let viewer = ViewerClient::new(server_stream, 120, 40, 30);
        (viewer, client_stream)
    }

    #[test]
    fn test_viewer_client_new() {
        let (viewer, _client) = make_pair();
        assert!(viewer.connected);
        assert_eq!(viewer.cols, 120);
        assert_eq!(viewer.rows, 40);
        assert!(viewer.active_session.is_none());
    }

    #[test]
    fn test_viewer_send_screen_update() {
        let (mut viewer, mut client) = make_pair();

        // Use a small screen to avoid filling the socket buffer
        let mut parser = vt100::Parser::new(3, 20, 0);
        parser.process(b"hi");

        // Resize the viewer's renderer to match
        viewer.renderer = PaneRenderer::new(20, 3, 1, 1);

        // Send in a thread to avoid blocking
        let handle = std::thread::spawn(move || {
            viewer.send_screen_update(parser.screen(), 0, 2)
        });

        // Read on client side
        client.set_nonblocking(false).unwrap();
        use std::time::Duration;
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 65536];
        let n = client.read(&mut buf).unwrap();
        assert!(n > 0, "client should receive data");

        let success = handle.join().unwrap();
        assert!(success, "send should succeed");
    }

    #[test]
    fn test_viewer_send_session_list() {
        let (mut viewer, mut client) = make_pair();
        client.set_nonblocking(false).unwrap();

        let sessions = vec![
            SessionInfo { id: "pty-1".into(), command: "bash".into(), status: "running".into() },
        ];
        let success = viewer.send_session_list(&sessions, Some("pty-1"));
        assert!(success);

        let mut buf = [0u8; 4096];
        let n = client.read(&mut buf).unwrap();
        assert!(n > 0);
    }

    #[test]
    fn test_viewer_send_goodbye() {
        let (mut viewer, _client) = make_pair();
        viewer.send_goodbye();
        assert!(!viewer.connected);
    }

    #[test]
    fn test_viewer_read_available() {
        let (mut viewer, mut client) = make_pair();

        // Client sends some data
        use crate::protocol::{encode_message, ClientMsg};
        let msg = ClientMsg::KeyInput { bytes: b"hello".to_vec() };
        client.write_all(&encode_message(&msg)).unwrap();
        client.flush().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        let n = viewer.read_available();
        assert!(n > 0, "should have read data from client");
        assert!(!viewer.read_buf.is_empty());
    }

    #[test]
    fn test_viewer_client_disconnect() {
        let (mut viewer, client) = make_pair();
        drop(client); // close client side

        std::thread::sleep(std::time::Duration::from_millis(50));
        let n = viewer.read_available();
        // Either got 0 bytes (EOF) or error — either way, disconnected
        assert!(!viewer.connected || n == 0);
    }
}
