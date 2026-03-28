//! Client for `tttt attach` — connects to a running tttt instance.
//!
//! Uses a virtual screen approach to prevent scroll floods:
//! - All server updates are applied to a virtual vt100 screen immediately
//! - The real terminal is only updated when the socket has no more pending data
//! - This means rapid redraws (e.g., Claude Code redrawing history) are absorbed
//!   into the virtual screen, and only the final state is rendered

use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use tttt_tui::protocol::{decode_message, encode_message, ClientMsg, ServerMsg};
use tttt_tui::PtyWidget;

/// Maximum number of reconnect attempts for the attach client.
const ATTACH_MAX_RECONNECT_ATTEMPTS: u32 = 30;

/// Base delay between attach reconnect attempts (doubles each retry, capped at 2s).
const ATTACH_RECONNECT_BASE_DELAY_MS: u64 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PasteMode {
    None,
    StartEsc,
    StartBracket,
    Start2,
    Start0,
    InPaste,
    EndEsc,
    EndBracket,
    End2,
    End0,
    End1,
}

fn process_paste_bytes(bytes: &[u8], paste_mode: &mut PasteMode, contains_detach_key: &mut bool) {
    *contains_detach_key = false;
    for &byte in bytes {
        match *paste_mode {
            PasteMode::None => {
                if byte == 0x1b {
                    *paste_mode = PasteMode::StartEsc;
                } else if byte == 0x1c {
                    *contains_detach_key = true;
                }
            }
            PasteMode::StartEsc => {
                if byte == b'[' {
                    *paste_mode = PasteMode::StartBracket;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::StartBracket => {
                if byte == b'2' {
                    *paste_mode = PasteMode::Start2;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::Start2 => {
                if byte == b'0' {
                    *paste_mode = PasteMode::Start0;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::Start0 => {
                if byte == b'0' {
                    *paste_mode = PasteMode::InPaste;
                } else if byte == b'1' {
                    *paste_mode = PasteMode::None;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::InPaste => {
                if byte == 0x1b {
                    *paste_mode = PasteMode::EndEsc;
                }
            }
            PasteMode::EndEsc => {
                if byte == b'[' {
                    *paste_mode = PasteMode::EndBracket;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::EndBracket => {
                if byte == b'2' {
                    *paste_mode = PasteMode::End2;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::End2 => {
                if byte == b'0' {
                    *paste_mode = PasteMode::End0;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::End0 => {
                if byte == b'1' {
                    *paste_mode = PasteMode::End1;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::End1 => {
                if byte == b'~' {
                    *paste_mode = PasteMode::None;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_key_triggers_detach() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);

        assert!(contains_detach_key, "Prefix key should trigger detach");
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_prefix_key_in_text_triggers_detach() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        let input = b"hello\x1cworld";
        process_paste_bytes(input, &mut paste_mode, &mut contains_detach_key);

        assert!(
            contains_detach_key,
            "Prefix key in text should trigger detach"
        );
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_prefix_key_in_bracketed_paste_ignored() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Bracketed paste start: \x1b[200~
        let paste_start = [0x1b, b'[', b'2', b'0', b'0', b'~'];
        process_paste_bytes(&paste_start, &mut paste_mode, &mut contains_detach_key);

        assert!(
            !contains_detach_key,
            "Paste start should not trigger detach"
        );
        assert_eq!(
            paste_mode,
            PasteMode::InPaste,
            "Should be in paste mode after start marker"
        );

        // Now send the prefix key - should NOT trigger detach
        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);

        assert!(
            !contains_detach_key,
            "Prefix key in paste should NOT trigger detach"
        );

        // Bracketed paste end: \x1b[201~
        let paste_end = [0x1b, b'[', b'2', b'0', b'1', b'~'];
        process_paste_bytes(&paste_end, &mut paste_mode, &mut contains_detach_key);

        assert!(!contains_detach_key, "Paste end should not trigger detach");
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_prefix_key_after_bracketed_paste_triggers_detach() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Send bracketed paste start and end
        let paste_sequence = [
            0x1b, b'[', b'2', b'0', b'0', b'~', // start
            b't', b'e', b'x', b't', // content
            0x1b, b'[', b'2', b'0', b'1', b'~', // end
        ];
        process_paste_bytes(&paste_sequence, &mut paste_mode, &mut contains_detach_key);

        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);

        // Now send the prefix key - should trigger detach
        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);

        assert!(
            contains_detach_key,
            "Prefix key after paste should trigger detach"
        );
    }

    #[test]
    fn test_bracketed_paste_across_multiple_reads() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // First read: \x1b[200
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::Start0);

        // Second read: ~text\x1c
        process_paste_bytes(
            &[b'0', b'~', b't', b'e', b'x', b't', 0x1c],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert!(
            !contains_detach_key,
            "Prefix key in paste should NOT trigger detach"
        );
        assert_eq!(paste_mode, PasteMode::InPaste);

        // Third read: \x1b[201~
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'1', b'~'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_normal_input_no_paste() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        let input = b"hello world";
        process_paste_bytes(input, &mut paste_mode, &mut contains_detach_key);

        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_incomplete_escape_sequence() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Send just ESC
        process_paste_bytes(&[0x1b], &mut paste_mode, &mut contains_detach_key);
        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::StartEsc);

        // Send regular character, should reset
        process_paste_bytes(&[b'a'], &mut paste_mode, &mut contains_detach_key);
        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);

        // Now send prefix key, should trigger detach
        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);
        assert!(contains_detach_key);
    }

    #[test]
    fn test_incomplete_bracketed_paste() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Send \x1b[200
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert_eq!(paste_mode, PasteMode::Start0);

        // Send something that completes the paste start
        process_paste_bytes(&[b'0'], &mut paste_mode, &mut contains_detach_key);
        assert_eq!(paste_mode, PasteMode::InPaste);

        // Send prefix key, should NOT trigger detach
        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);
        assert!(
            !contains_detach_key,
            "Prefix key in incomplete paste should NOT trigger detach"
        );

        // Send escape to start end sequence
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'1'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::End1);

        // Finally complete with ~ to exit paste mode
        process_paste_bytes(&[b'~'], &mut paste_mode, &mut contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_real_world_paste_scenario() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Simulate pasting: \x1b[200~echo "test\x1c"\x1b[201~
        // This contains the prefix key inside the paste
        let paste = [
            0x1b, b'[', b'2', b'0', b'0', b'~', // start
            b'e', b'c', b'h', b'o', b' ', b'"', b't', b'e', b's', b't', 0x1c,
            b'"', // content with prefix key
            0x1b, b'[', b'2', b'0', b'1', b'~', // end
        ];

        process_paste_bytes(&paste, &mut paste_mode, &mut contains_detach_key);

        assert!(
            !contains_detach_key,
            "Prefix key inside paste should NOT trigger detach"
        );
        assert_eq!(paste_mode, PasteMode::None);
    }

    #[test]
    fn test_invalid_bracketed_end_sequence() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Invalid: \x1b[201x where x != ~
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'1', b'x'],
            &mut paste_mode,
            &mut contains_detach_key,
        );

        assert!(!contains_detach_key);
        assert_eq!(
            paste_mode,
            PasteMode::None,
            "Invalid end sequence should not leave us in paste mode"
        );
    }

    #[test]
    fn test_malformed_start_sequence() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Malformed: \x1b[201x while not in paste mode
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'1', b'x'],
            &mut paste_mode,
            &mut contains_detach_key,
        );

        assert!(!contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);

        // Prefix key should still trigger detach
        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);
        assert!(contains_detach_key);
    }

    #[test]
    fn test_invalid_end_sequence_during_paste() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Start paste: \x1b[200~
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'0', b'~'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert_eq!(paste_mode, PasteMode::InPaste);

        // Invalid end sequence: \x1b[201x (x != ~)
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'1', b'x'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert_eq!(
            paste_mode,
            PasteMode::InPaste,
            "Should still be in paste mode after invalid end sequence"
        );

        // Prefix key should NOT trigger detach
        process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);
        assert!(
            !contains_detach_key,
            "Prefix key in paste should not trigger detach"
        );
    }

    #[test]
    fn test_end_sequence_split_across_reads() {
        let mut paste_mode = PasteMode::None;
        let mut contains_detach_key = false;

        // Start paste
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'0', b'~'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert_eq!(paste_mode, PasteMode::InPaste);

        // End sequence split: \x1b[201 in one read
        process_paste_bytes(
            &[0x1b, b'[', b'2', b'0', b'1'],
            &mut paste_mode,
            &mut contains_detach_key,
        );
        assert_eq!(paste_mode, PasteMode::End1);

        // ~ in next read
        process_paste_bytes(&[b'~'], &mut paste_mode, &mut contains_detach_key);
        assert_eq!(paste_mode, PasteMode::None);
    }
}

/// Try to connect to the viewer socket, retrying with backoff.
fn attach_connect_with_retry(
    socket_path: &str,
    max_attempts: u32,
) -> Result<UnixStream, std::io::Error> {
    let mut delay_ms = ATTACH_RECONNECT_BASE_DELAY_MS;
    for attempt in 0..max_attempts {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(_) if attempt + 1 < max_attempts => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(2000);
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "max reconnect attempts exceeded",
    ))
}

/// Reason the inner event loop exited.
enum AttachLoopExit {
    /// User requested detach or stdin EOF — stop entirely.
    Detach,
    /// Server disconnected — try to reconnect.
    Disconnected,
}

fn cleanup_terminal() {
    let _ = std::io::stdout().write_all(b"\x1b[?2004l");
    let _ = std::io::stdout().flush();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Run the attach client.
pub fn run_attach(socket_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Enter raw terminal mode BEFORE any output
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Enable bracketed paste mode
    stdout.write_all(b"\x1b[?2004h")?;
    stdout.flush()?;

    let stdin_fd = std::io::stdin().as_raw_fd();

    // Register SIGWINCH handler for terminal resize
    let winch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _ = signal_hook::flag::register(libc::SIGWINCH, std::sync::Arc::clone(&winch));

    let mut paste_mode = PasteMode::None;

    // Create the ratatui terminal once; pass it into the loop so it persists across reconnects.
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            cleanup_terminal();
            return Err(e.into());
        }
    };

    loop {
        let stream = match attach_connect_with_retry(socket_path, ATTACH_MAX_RECONNECT_ATTEMPTS) {
            Ok(s) => s,
            Err(_) => {
                cleanup_terminal();
                eprintln!("\r\n[tttt attach] Could not connect.");
                return Ok(());
            }
        };

        let result = run_attach_loop(stream, stdin_fd, &winch, &mut paste_mode, &mut terminal);

        match result {
            AttachLoopExit::Detach => {
                cleanup_terminal();
                return Ok(());
            }
            AttachLoopExit::Disconnected => {
                eprintln!("\r\n[tttt attach] Disconnected. Reconnecting...");
                // Loop back to reconnect
                continue;
            }
        }
    }
}

/// Inner event loop for the attach client. Returns the reason it exited.
fn run_attach_loop(
    mut stream: UnixStream,
    stdin_fd: i32,
    winch: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    paste_mode: &mut PasteMode,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> AttachLoopExit {
    let _ = stream.set_nonblocking(true);
    let stream_fd = stream.as_raw_fd();

    // Get our terminal size
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // Clear screen
    let _ = terminal.clear();

    // Tell server our terminal size immediately so PTY can be resized
    {
        let msg = ClientMsg::Resize {
            cols: term_cols,
            rows: term_rows,
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.write_all(&encode_message(&msg));
        let _ = stream.set_nonblocking(true);
    }

    let mut cur_cols = term_cols;
    let mut cur_rows = term_rows;

    // Virtual screen: absorbs all server updates instantly.
    // Only flushed to real terminal when socket is idle.
    let mut virtual_screen = vt100::Parser::new(cur_rows, cur_cols, 0);
    let mut last_cursor = (0u16, 0u16);
    let mut virtual_dirty = false;

    let mut read_buf = Vec::new();

    // Last time we rendered to the real terminal (for max latency cap)
    let mut last_render_time = std::time::Instant::now();
    // Maximum time to wait before forcing a render, even if data is still arriving
    const RENDER_FORCE_MS: u64 = 100;
    // Debug: count messages received
    let mut msg_count = 0;

    loop {
        // Handle terminal resize
        if winch.load(std::sync::atomic::Ordering::Relaxed) {
            winch.store(false, std::sync::atomic::Ordering::Relaxed);
            let (new_cols, new_rows) = crossterm::terminal::size().unwrap_or((80, 24));
            if new_cols != cur_cols || new_rows != cur_rows {
                cur_cols = new_cols;
                cur_rows = new_rows;
                // Resize virtual screen
                virtual_screen = vt100::Parser::new(cur_rows, cur_cols, 0);
                virtual_dirty = true;
                // Tell server about new size
                let msg = ClientMsg::Resize {
                    cols: cur_cols,
                    rows: cur_rows,
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.write_all(&encode_message(&msg));
                let _ = stream.set_nonblocking(true);
                // Clear screen
                let _ = terminal.clear();
            }
        }
        let stdin_pfd = PollFd::new(
            unsafe { BorrowedFd::borrow_raw(stdin_fd) },
            PollFlags::POLLIN,
        );
        let stream_pfd = PollFd::new(
            unsafe { BorrowedFd::borrow_raw(stream_fd) },
            PollFlags::POLLIN,
        );
        let mut fds = [stdin_pfd, stream_pfd];
        let _ = poll(&mut fds, PollTimeout::from(100u16));

        // Read from stdin → send to server
        if let Some(flags) = fds[0].revents() {
            if flags.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 4096];
                match nix::unistd::read(stdin_fd, &mut buf) {
                    Ok(0) => return AttachLoopExit::Detach,
                    Ok(n) => {
                        let mut contains_detach_key = false;
                        process_paste_bytes(&buf[..n], paste_mode, &mut contains_detach_key);
                        if contains_detach_key {
                            let msg = ClientMsg::Detach;
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.write_all(&encode_message(&msg));
                            let _ = stream.set_nonblocking(true);
                            return AttachLoopExit::Detach;
                        }
                        let msg = ClientMsg::KeyInput {
                            bytes: buf[..n].to_vec(),
                        };
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.write_all(&encode_message(&msg));
                        let _ = stream.set_nonblocking(true);
                    }
                    Err(nix::errno::Errno::EAGAIN) => {}
                    Err(_) => return AttachLoopExit::Detach,
                }
            }
        }

        // Read ALL available data from server into buffer
        let mut got_server_data = false;
        if let Some(flags) = fds[1].revents() {
            if flags.contains(PollFlags::POLLIN) {
                // Drain the socket completely
                loop {
                    let mut tmp = [0u8; 65536];
                    match stream.read(&mut tmp) {
                        Ok(0) => {
                            // EOF — server disconnected
                            return AttachLoopExit::Disconnected;
                        }
                        Ok(n) => {
                            read_buf.extend_from_slice(&tmp[..n]);
                            got_server_data = true;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => return AttachLoopExit::Disconnected,
                    }
                }
            }
            if let Some(flags) = fds[1].revents() {
                if flags.contains(PollFlags::POLLHUP) {
                    return AttachLoopExit::Disconnected;
                }
            }
        }

        // Process ALL pending messages into virtual screen
        while let Some((msg, consumed)) = decode_message::<ServerMsg>(&read_buf) {
            read_buf.drain(..consumed);
            msg_count += 1;
            tracing::trace!("[CLIENT] msg #{}: {:?}", msg_count, std::mem::discriminant(&msg));
            match msg {
                ServerMsg::ScreenUpdate {
                    screen_data,
                    cursor_row,
                    cursor_col,
                } => {
                    tracing::trace!("[CLIENT] ScreenUpdate: data_len={}, cursor=({},{})", screen_data.len(), cursor_row, cursor_col);
                    if !screen_data.is_empty() {
                        // Apply to virtual screen (fresh parser for clean state)
                        // Use current terminal dimensions, not initial ones
                        let (vrows, vcols) = virtual_screen.screen().size();
                        let virt_cols = cur_cols.min(vcols);
                        let virt_rows = cur_rows.min(vrows);
                        virtual_screen = vt100::Parser::new(virt_rows, virt_cols, 0);
                        virtual_screen.process(&screen_data);
                        virtual_dirty = true;
                    }
                    last_cursor = (cursor_row, cursor_col);
                }
                ServerMsg::SessionList { .. } => {
                    tracing::trace!("[CLIENT] SessionList received");
                }
                ServerMsg::WindowSize { cols, rows } => {
                    tracing::trace!("[CLIENT] WindowSize: cols={}, rows={}", cols, rows);
                    // Resize virtual screen to match the server's PTY dimensions
                    virtual_screen = vt100::Parser::new(rows, cols, 0);
                    virtual_dirty = true;
                }
                ServerMsg::Goodbye => return AttachLoopExit::Disconnected,
            }
        }

        // Only flush virtual screen to real terminal when:
        // 1. Virtual screen has changes AND
        // 2. No more data waiting on the socket (we've drained it)
        //
        // This is the key insight: if data is still arriving, we keep
        // updating the virtual screen and skip the real terminal render.
        // Only when the socket goes quiet do we render the final state.
        //
        // FORCE RENDER: If it's been RENDER_FORCE_MS since last render and we have data,
        // force a render to avoid blank screen issues.
        let should_force_render = virtual_dirty
            && last_render_time.elapsed().as_millis() >= RENDER_FORCE_MS as u128;

        if virtual_dirty && (!got_server_data || should_force_render) {
            tracing::trace!("[CLIENT] Rendering: got_server_data={}, should_force={}", got_server_data, should_force_render);
            let screen = virtual_screen.screen();
            let _ = terminal.draw(|frame| {
                frame.render_widget(PtyWidget::new(screen), frame.area());
            });
            let (cursor_row, cursor_col) = last_cursor;
            let _ = terminal.set_cursor_position((cursor_col, cursor_row));
            let _ = terminal.show_cursor();
            virtual_dirty = false;
            last_render_time = std::time::Instant::now();
        }
    }
}
