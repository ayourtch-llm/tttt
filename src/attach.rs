//! Client for `tttt attach` — connects to a running tttt instance.

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use tttt_tui::protocol::{decode_message, encode_message, ClientMsg, ServerMsg};
use tttt_tui::{clear_screen, cursor_goto};

/// Run the attach client.
pub fn run_attach(socket_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_nonblocking(true)?;

    eprintln!("Connected to tttt at {}", socket_path);

    // Enter raw terminal mode
    let _raw = RawMode::enter();

    let stdout_fd = std::io::stdout().as_raw_fd();
    let stdin_fd = std::io::stdin().as_raw_fd();
    let stream_fd = stream.as_raw_fd();

    // Clear screen
    write_fd(stdout_fd, clear_screen().as_bytes());

    let mut read_buf = Vec::new();

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
                        // Check for detach sequence (Ctrl+\ d)
                        if buf[..n].contains(&0x1c) {
                            // Simplified: any Ctrl+\ in the stream means detach for now
                            // A proper implementation would use InputParser
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

        // Read from server → display
        if let Some(flags) = fds[1].revents() {
            if flags.contains(PollFlags::POLLIN) {
                let mut tmp = [0u8; 65536];
                match stream.read(&mut tmp) {
                    Ok(0) => {
                        eprintln!("\nServer disconnected.");
                        break;
                    }
                    Ok(n) => {
                        read_buf.extend_from_slice(&tmp[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
            if let Some(flags) = fds[1].revents() {
                if flags.contains(PollFlags::POLLHUP) {
                    eprintln!("\nServer disconnected.");
                    break;
                }
            }
        }

        // Process received messages
        while let Some((msg, consumed)) = decode_message::<ServerMsg>(&read_buf) {
            read_buf.drain(..consumed);
            match msg {
                ServerMsg::ScreenUpdate {
                    ansi_data,
                    cursor_row,
                    cursor_col,
                } => {
                    if !ansi_data.is_empty() {
                        write_fd(stdout_fd, &ansi_data);
                    }
                    write_fd(
                        stdout_fd,
                        cursor_goto(cursor_row, cursor_col).as_bytes(),
                    );
                }
                ServerMsg::SessionList { .. } => {
                    // TODO: render sidebar
                }
                ServerMsg::Goodbye => {
                    eprintln!("\nServer shutting down.");
                    break;
                }
            }
        }
    }

    Ok(())
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
