//! Client for `tttt attach` — connects to a running tttt instance.
//!
//! Uses client-side double-buffering: server ANSI data is applied to a local
//! vt100 screen, then PaneRenderer computes the minimal diff to the real
//! terminal. This prevents scroll floods when the inner program (e.g., Claude
//! Code) redraws its entire history.

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

    // Client-side double buffer:
    // 1. server_screen: vt100 parser that receives server's ANSI data
    // 2. client_renderer: PaneRenderer that computes minimal diff to real terminal
    //
    // This prevents scroll floods: even if the server sends a full screen redraw
    // (e.g., Claude Code redrawing history), we only update the cells that
    // actually changed from the client's perspective.
    let mut server_screen = vt100::Parser::new(term_rows, term_cols, 0);
    let mut client_renderer = PaneRenderer::new(term_cols, term_rows, 1, 1);

    let mut read_buf = Vec::new();
    let mut last_cursor = (1u16, 1u16);
    let mut last_server_cursor = (0u16, 0u16);

    loop {
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
                        if buf[..n].contains(&0x1c) {
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

        // Read from server
        if let Some(flags) = fds[1].revents() {
            if flags.contains(PollFlags::POLLIN) {
                let mut tmp = [0u8; 65536];
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        read_buf.extend_from_slice(&tmp[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
            if let Some(flags) = fds[1].revents() {
                if flags.contains(PollFlags::POLLHUP) {
                    break;
                }
            }
        }

        // Process received messages — batch all pending before rendering
        let mut needs_render = false;

        while let Some((msg, consumed)) = decode_message::<ServerMsg>(&read_buf) {
            read_buf.drain(..consumed);
            match msg {
                ServerMsg::ScreenUpdate {
                    screen_data,
                    cursor_row,
                    cursor_col,
                } => {
                    if !screen_data.is_empty() {
                        // Reset and replay: contents_formatted() is a full
                        // screen snapshot, so we create a fresh parser each
                        // time to avoid state accumulation.
                        server_screen = vt100::Parser::new(term_rows, term_cols, 0);
                        server_screen.process(&screen_data);
                        needs_render = true;
                    }
                    // Use server's cursor coords (0-indexed PTY coords)
                    last_server_cursor = (cursor_row, cursor_col);
                }
                ServerMsg::SessionList { .. } => {
                    // TODO: render sidebar
                }
                ServerMsg::Goodbye => {
                    break;
                }
            }
        }

        // Render: use PaneRenderer to compute minimal diff from server_screen
        // to real terminal. This is the key anti-flood mechanism.
        if needs_render {
            let output = client_renderer.render(server_screen.screen());
            if !output.is_empty() {
                write_fd(stdout_fd, &output);
            }
            // Position cursor: 0-indexed server coords → 1-indexed terminal
            let new_cursor = (last_server_cursor.0 + 1, last_server_cursor.1 + 1);
            if new_cursor != last_cursor {
                write_fd(
                    stdout_fd,
                    cursor_goto(new_cursor.0, new_cursor.1).as_bytes(),
                );
                last_cursor = new_cursor;
            }
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
        Self { original }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(ref orig) = self.original {
            let stdin = std::io::stdin();
            let _ = nix::sys::termios::tcsetattr(
                &stdin,
                nix::sys::termios::SetArg::TCSAFLUSH,
                orig,
            );
        }
    }
}
