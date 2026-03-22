//! Client for `tttt attach` — connects to a running tttt instance.
//!
//! Uses a virtual screen approach to prevent scroll floods:
//! - All server updates are applied to a virtual vt100 screen immediately
//! - The real terminal is only updated when the socket has no more pending data
//! - This means rapid redraws (e.g., Claude Code redrawing history) are absorbed
//!   into the virtual screen, and only the final state is rendered

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use tttt_tui::protocol::{decode_message, encode_message, ClientMsg, ServerMsg};
use tttt_tui::{clear_screen, cursor_goto, PaneRenderer};

/// Run the attach client.
pub fn run_attach(socket_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_nonblocking(true)?;

    // Enter raw terminal mode BEFORE any output
    let _raw = RawMode::enter();

    let stdout_fd = std::io::stdout().as_raw_fd();
    let stdin_fd = std::io::stdin().as_raw_fd();
    let stream_fd = stream.as_raw_fd();

    // Get our terminal size
    let (term_cols, term_rows) = terminal_size();

    // Clear screen
    write_fd(stdout_fd, clear_screen().as_bytes());

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

    // Register SIGWINCH handler for terminal resize
    let winch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _ = signal_hook::flag::register(libc::SIGWINCH, std::sync::Arc::clone(&winch));

    let mut cur_cols = term_cols;
    let mut cur_rows = term_rows;

    // Virtual screen: absorbs all server updates instantly.
    // Only flushed to real terminal when socket is idle.
    let mut virtual_screen = vt100::Parser::new(cur_rows, cur_cols, 0);
    let mut renderer = PaneRenderer::new(cur_cols, cur_rows, 1, 1);
    let mut last_cursor = (0u16, 0u16);
    let mut virtual_dirty = false;

    let mut read_buf = Vec::new();

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

    fn process_paste_bytes(
        bytes: &[u8],
        paste_mode: &mut PasteMode,
        contains_detach_key: &mut bool,
    ) {
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
                        *paste_mode = PasteMode::End1;
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

    let mut paste_mode = PasteMode::None;

    loop {
        // Handle terminal resize
        if winch.load(std::sync::atomic::Ordering::Relaxed) {
            winch.store(false, std::sync::atomic::Ordering::Relaxed);
            let (new_cols, new_rows) = terminal_size();
            if new_cols != cur_cols || new_rows != cur_rows {
                cur_cols = new_cols;
                cur_rows = new_rows;
                // Resize virtual screen and renderer
                virtual_screen = vt100::Parser::new(cur_rows, cur_cols, 0);
                renderer = PaneRenderer::new(cur_cols, cur_rows, 1, 1);
                renderer.invalidate();
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
                write_fd(stdout_fd, clear_screen().as_bytes());
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
                    Ok(0) => break,
                    Ok(n) => {
                        let mut contains_detach_key = false;
                        process_paste_bytes(&buf[..n], &mut paste_mode, &mut contains_detach_key);
                        if contains_detach_key {
                            let msg = ClientMsg::Detach;
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.write_all(&encode_message(&msg));
                            let _ = stream.set_nonblocking(true);
                            break;
                        }
                        let msg = ClientMsg::KeyInput {
                            bytes: buf[..n].to_vec(),
                        };
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.write_all(&encode_message(&msg));
                        let _ = stream.set_nonblocking(true);
                    }
                    Err(nix::errno::Errno::EAGAIN) => {}
                    Err(_) => break,
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
                            return Ok(());
                        }
                        Ok(n) => {
                            read_buf.extend_from_slice(&tmp[..n]);
                            got_server_data = true;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => return Ok(()),
                    }
                }
            }
            if let Some(flags) = fds[1].revents() {
                if flags.contains(PollFlags::POLLHUP) {
                    return Ok(());
                }
            }
        }

        // Process ALL pending messages into virtual screen
        while let Some((msg, consumed)) = decode_message::<ServerMsg>(&read_buf) {
            read_buf.drain(..consumed);
            match msg {
                ServerMsg::ScreenUpdate {
                    screen_data,
                    cursor_row,
                    cursor_col,
                } => {
                    if !screen_data.is_empty() {
                        // Apply to virtual screen (fresh parser for clean state)
                        virtual_screen = vt100::Parser::new(term_rows, term_cols, 0);
                        virtual_screen.process(&screen_data);
                        virtual_dirty = true;
                    }
                    last_cursor = (cursor_row, cursor_col);
                }
                ServerMsg::SessionList { .. } => {}
                ServerMsg::Goodbye => return Ok(()),
            }
        }

        // Only flush virtual screen to real terminal when:
        // 1. Virtual screen has changes AND
        // 2. No more data waiting on the socket (we've drained it)
        //
        // This is the key insight: if data is still arriving, we keep
        // updating the virtual screen and skip the real terminal render.
        // Only when the socket goes quiet do we render the final state.
        if virtual_dirty && !got_server_data {
            // Render PTY cells via PaneRenderer (minimal diff).
            let output = renderer.render(virtual_screen.screen());
            if !output.is_empty() {
                write_fd(stdout_fd, &output);
            }

            // Fill right margin with gray dots if PTY is narrower than terminal
            let (vrows, vcols) = virtual_screen.screen().size();
            if vcols < cur_cols {
                // Gray foreground (dim), dot character
                let dot_attr = "\x1b[2;90m"; // dim + bright black (gray)
                let reset = "\x1b[0m";
                let dots: String = ".".repeat((cur_cols - vcols) as usize);
                for row in 0..vrows.min(cur_rows) {
                    write_fd(
                        stdout_fd,
                        format!(
                            "\x1b[{};{}H{}{}{}",
                            row + 1,
                            vcols + 1,
                            dot_attr,
                            dots,
                            reset
                        )
                        .as_bytes(),
                    );
                }
            }

            let new_cursor = (last_cursor.0 + 1, last_cursor.1 + 1);
            write_fd(
                stdout_fd,
                cursor_goto(new_cursor.0, new_cursor.1).as_bytes(),
            );
            virtual_dirty = false;
        }
    }

    Ok(())
}

fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

fn write_fd(fd: i32, data: &[u8]) {
    let mut offset = 0;
    while offset < data.len() {
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        match nix::unistd::write(borrowed, &data[offset..]) {
            Ok(n) => offset += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
}

struct RawMode {
    original: Option<nix::sys::termios::Termios>,
}

impl RawMode {
    fn enter() -> Self {
        use nix::sys::termios::*;
        let stdin = std::io::stdin();
        let original = tcgetattr(&stdin).ok();
        if let Some(ref orig) = original {
            let mut raw: Termios = orig.clone();
            raw.local_flags.remove(LocalFlags::ICANON);
            raw.local_flags.remove(LocalFlags::ECHO);
            raw.local_flags.remove(LocalFlags::ISIG);
            raw.local_flags.remove(LocalFlags::IEXTEN);
            raw.input_flags.remove(InputFlags::IXON);
            raw.input_flags.remove(InputFlags::ICRNL);
            raw.input_flags.remove(InputFlags::BRKINT);
            raw.input_flags.remove(InputFlags::INPCK);
            raw.input_flags.remove(InputFlags::ISTRIP);
            raw.output_flags.remove(OutputFlags::OPOST);
            raw.control_flags.remove(ControlFlags::CSIZE);
            raw.control_flags.insert(ControlFlags::CS8);
            raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
            raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
            let _ = tcsetattr(&stdin, SetArg::TCSAFLUSH, &raw);
        }
        // Enable bracketed paste mode (xterm DEC private mode 2004)
        // This causes the terminal to wrap pasted content in \x1b[200~ markers
        // so we can distinguish paste events from regular keystrokes
        let _ = std::io::stdout().write_all(b"\x1b[?2004h");
        let _ = std::io::stdout().flush();
        Self { original }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Disable bracketed paste mode before restoring original settings
        let _ = std::io::stdout().write_all(b"\x1b[?2004l");
        let _ = std::io::stdout().flush();

        if let Some(ref orig) = self.original {
            let stdin = std::io::stdin();
            let _ =
                nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSAFLUSH, orig);
        }
    }
}
