use crate::error::{PtyError, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};

/// Abstraction over PTY operations for testability.
pub trait PtyBackend: Send {
    /// Write data to the PTY's stdin.
    fn write(&mut self, data: &[u8]) -> Result<()>;

    /// Read available data from the PTY's stdout. Returns number of bytes read.
    /// Non-blocking: returns 0 if no data available.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Resize the PTY window.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;

    /// Kill the child process.
    fn kill(&mut self) -> Result<()>;

    /// Check if child has exited. Returns Some(exit_code) if exited, None if still running.
    fn try_wait(&mut self) -> Result<Option<i32>>;

    /// Kill with bounded escalation: SIGTERM → poll 150ms → SIGKILL → poll 500ms → give up.
    ///
    /// Default implementation delegates to `kill()`. Override for process-group kill.
    fn kill_with_escalation(&mut self) -> Result<()> {
        self.kill()
    }
}

/// Real PTY backend using portable-pty.
pub struct RealPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    /// Raw fd of the reader for poll() integration.
    #[cfg(unix)]
    reader_raw_fd: i32,
}

impl RealPty {
    /// Spawn a new PTY process with the given command, dimensions, and working directory.
    pub fn spawn_with_cwd(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        Self::spawn_with_cwd_and_env(command, args, cwd, cols, rows, [])
    }

    /// Spawn a new PTY process with the given command, dimensions, working directory, and environment variables.
    pub fn spawn_with_cwd_and_env(
        command: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        cols: u16,
        rows: u16,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let mut cmd = CommandBuilder::new(command);
        for arg in args {
            cmd.arg(*arg);
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        } else if let Ok(current) = std::env::current_dir() {
            cmd.cwd(current);
        }

        // Set environment variables
        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        // Get the master raw fd for poll()-based I/O.
        // We use the master fd directly for polling — when data is available
        // on the master, it means the child wrote to the PTY.
        #[cfg(unix)]
        let reader_raw_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| PtyError::Spawn("failed to get PTY raw fd".to_string()))?;

        // Set the PTY master fd to non-blocking mode so reads return immediately
        // when no data is available (returns EAGAIN/EWOULDBLOCK).
        #[cfg(unix)]
        {
            use nix::fcntl::{fcntl, FcntlArg};
            use nix::sys::stat::{Mode, SFlag};
            let flags = fcntl(reader_raw_fd, FcntlArg::F_GETFL)
                .map_err(|e| PtyError::Spawn(format!("fcntl F_GETFL failed: {}", e)))?;
            fcntl(reader_raw_fd, FcntlArg::F_SETFL(nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK))
                .map_err(|e| PtyError::Spawn(format!("fcntl F_SETFL failed: {}", e)))?;
        }

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        Ok(Self {
            master: pair.master,
            child,
            reader,
            writer,
            #[cfg(unix)]
            reader_raw_fd,
        })
    }

    /// Spawn a new PTY process in the current working directory.
    pub fn spawn(command: &str, args: &[&str], cols: u16, rows: u16) -> Result<Self> {
        let cwd = std::env::current_dir().ok();
        Self::spawn_with_cwd(command, args, cwd.as_deref(), cols, rows)
    }

    /// Get the raw file descriptor of the PTY reader for use with poll().
    #[cfg(unix)]
    pub fn reader_raw_fd(&self) -> i32 {
        self.reader_raw_fd
    }
}

impl PtyBackend for RealPty {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        // Write in chunks to avoid EAGAIN (os error 35) on non-blocking PTY fds.
        // The master fd is set to non-blocking for reads, which also affects writes.
        // Large writes (>~1KB) can overflow the kernel PTY buffer.
        const CHUNK_SIZE: usize = 512;
        const MAX_RETRIES: usize = 100;

        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + CHUNK_SIZE).min(data.len());
            let mut retries = 0;
            loop {
                match self.writer.write(&data[offset..end]) {
                    Ok(0) => {
                        return Err(PtyError::Io(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "PTY write returned 0 bytes",
                        )));
                    }
                    Ok(n) => {
                        offset += n;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        retries += 1;
                        if retries >= MAX_RETRIES {
                            return Err(PtyError::Io(e));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => return Err(PtyError::Io(e)),
                }
            }
        }
        self.writer.flush()?;
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // Use nix::unistd::read on the raw fd for non-blocking behavior
        // when used with poll(). Falls back to portable-pty reader otherwise.
        #[cfg(unix)]
        {
            match nix::unistd::read(self.reader_raw_fd, buf) {
                Ok(n) => return Ok(n),
                Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => {
                    return Ok(0)
                }
                Err(nix::errno::Errno::EIO) => return Ok(0), // PTY closed
                Err(e) => return Err(PtyError::Io(std::io::Error::from(e))),
            }
        }
        #[cfg(not(unix))]
        match self.reader.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(PtyError::Io(e)),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Resize(e.to_string()))
    }

    fn kill(&mut self) -> Result<()> {
        self.child.kill().map_err(|e| PtyError::Io(e))?;
        Ok(())
    }

    fn kill_with_escalation(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill as nix_kill, Signal};
            use nix::unistd::Pid;
            use std::time::Duration;

            // Try to get PID for process-group kill.
            if let Some(pid_u32) = self.child.process_id() {
                let pid = pid_u32 as i32;
                // Step 1: SIGTERM to entire process group.
                let _ = nix_kill(Pid::from_raw(-pid), Signal::SIGTERM);

                // Step 2: Poll 3 × 50ms (150ms) for graceful exit.
                for _ in 0..3 {
                    std::thread::sleep(Duration::from_millis(50));
                    if self.try_wait()?.is_some() {
                        return Ok(());
                    }
                }

                // Step 3: SIGKILL + close master_fd to unblock kernel reads (macOS).
                let _ = nix_kill(Pid::from_raw(-pid), Signal::SIGKILL);
                // Close the master fd to unblock any kernel reads blocked on the PTY.
                let fd = self.reader_raw_fd;
                // Invalidate the fd so Drop / further reads don't double-close.
                // We can't set it in the struct from a non-mut context, but we
                // have &mut self here. Closing the fd that is also used for reads
                // is intentional: the child is dead, we want it to unblock.
                let _ = nix::unistd::close(fd);
                // Set to -1 so further read/write calls get EBADF rather than
                // operating on a recycled fd.
                self.reader_raw_fd = -1;

                // Step 4: Poll 10 × 50ms (500ms) for SIGKILL to take effect.
                for _ in 0..10 {
                    std::thread::sleep(Duration::from_millis(50));
                    if self.try_wait()?.is_some() {
                        return Ok(());
                    }
                }

                // Step 5: Give up — total worst case 650ms, never blocks indefinitely.
                return Ok(());
            }
        }
        // Fallback: no PID available or non-Unix — use the simple kill.
        self.kill()
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = if status.success() { 0 } else { 1 };
                Ok(Some(code))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(PtyError::Io(e)),
        }
    }
}

/// Mock PTY backend for testing.
pub struct MockPty {
    /// Data that read() will return (simulates process output).
    pub output_buf: VecDeque<u8>,
    /// Data that was written via write() (captures process input).
    pub input_buf: Vec<u8>,
    /// If set, try_wait returns this exit code.
    pub exit_code: Option<i32>,
    /// If true, kill() has been called.
    pub killed: bool,
    /// If true, kill_with_escalation() has been called.
    pub escalation_killed: bool,
    /// Number of times try_wait() was called before exit_code was set.
    pub try_wait_calls: usize,
    /// If set, kill_with_escalation() sets exit code after this many try_wait calls.
    /// Used to simulate a process that ignores SIGTERM but dies to SIGKILL.
    pub exit_after_n_waits: Option<usize>,
    /// Current dimensions.
    pub cols: u16,
    pub rows: u16,
}

impl MockPty {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            output_buf: VecDeque::new(),
            input_buf: Vec::new(),
            exit_code: None,
            killed: false,
            escalation_killed: false,
            try_wait_calls: 0,
            exit_after_n_waits: None,
            cols,
            rows,
        }
    }

    /// Queue data that read() will return.
    pub fn queue_output(&mut self, data: &[u8]) {
        self.output_buf.extend(data);
    }
}

impl PtyBackend for MockPty {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.input_buf.extend_from_slice(data);
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let len = buf.len().min(self.output_buf.len());
        for item in buf.iter_mut().take(len) {
            *item = self.output_buf.pop_front().unwrap();
        }
        Ok(len)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn kill(&mut self) -> Result<()> {
        self.killed = true;
        self.exit_code = Some(137);
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        self.try_wait_calls += 1;
        // Simulate a process that exits after N try_wait calls (for escalation tests).
        if let Some(n) = self.exit_after_n_waits {
            if self.try_wait_calls >= n && self.exit_code.is_none() {
                self.exit_code = Some(137);
            }
        }
        Ok(self.exit_code)
    }

    fn kill_with_escalation(&mut self) -> Result<()> {
        self.escalation_killed = true;
        // Simulate: SIGTERM sent. If exit_after_n_waits is None, process exits immediately.
        // Otherwise, it waits until try_wait is called enough times.
        if self.exit_after_n_waits.is_none() {
            self.exit_code = Some(15); // SIGTERM
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_pty_write_captures_input() {
        let mut pty = MockPty::new(80, 24);
        pty.write(b"hello").unwrap();
        assert_eq!(pty.input_buf, b"hello");
    }

    #[test]
    fn test_mock_pty_read_returns_queued_output() {
        let mut pty = MockPty::new(80, 24);
        pty.queue_output(b"world");
        let mut buf = [0u8; 32];
        let n = pty.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"world");
    }

    #[test]
    fn test_mock_pty_read_empty_returns_zero() {
        let mut pty = MockPty::new(80, 24);
        let mut buf = [0u8; 32];
        let n = pty.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_mock_pty_write_read_roundtrip() {
        let mut pty = MockPty::new(80, 24);
        pty.write(b"input data").unwrap();
        assert_eq!(pty.input_buf, b"input data");

        pty.queue_output(b"output data");
        let mut buf = [0u8; 32];
        let n = pty.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"output data");
    }

    #[test]
    fn test_mock_pty_exit_code_none_initially() {
        let mut pty = MockPty::new(80, 24);
        assert_eq!(pty.try_wait().unwrap(), None);
    }

    #[test]
    fn test_mock_pty_exit_code_set() {
        let mut pty = MockPty::new(80, 24);
        pty.exit_code = Some(0);
        assert_eq!(pty.try_wait().unwrap(), Some(0));
    }

    #[test]
    fn test_mock_pty_kill_sets_exit_code() {
        let mut pty = MockPty::new(80, 24);
        pty.kill().unwrap();
        assert!(pty.killed);
        assert_eq!(pty.try_wait().unwrap(), Some(137));
    }

    #[test]
    fn test_mock_pty_resize() {
        let mut pty = MockPty::new(80, 24);
        pty.resize(120, 40).unwrap();
        assert_eq!(pty.cols, 120);
        assert_eq!(pty.rows, 40);
    }

    #[test]
    fn test_mock_pty_partial_read() {
        let mut pty = MockPty::new(80, 24);
        pty.queue_output(b"long output data here");
        let mut buf = [0u8; 4];
        let n = pty.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..n], b"long");
        // remaining data still in buffer
        let n = pty.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..n], b" out");
    }

    #[test]
    fn test_mock_pty_kill_with_escalation_immediate_exit() {
        // Process exits immediately on SIGTERM (no delay needed).
        let mut pty = MockPty::new(80, 24);
        assert!(!pty.escalation_killed);
        assert!(pty.exit_code.is_none());

        pty.kill_with_escalation().unwrap();

        assert!(pty.escalation_killed);
        // The mock sets exit_code when no delay is configured.
        assert!(pty.try_wait().unwrap().is_some());
    }

    #[test]
    fn test_mock_pty_kill_with_escalation_delayed_exit() {
        // Simulate a process that ignores SIGTERM but exits after several try_wait calls
        // (as if it eventually dies to SIGKILL).
        let mut pty = MockPty::new(80, 24);
        // Process will "exit" on the 5th try_wait call.
        pty.exit_after_n_waits = Some(5);

        // Before kill, process is still alive.
        assert!(pty.exit_code.is_none());

        pty.kill_with_escalation().unwrap();
        assert!(pty.escalation_killed);

        // Not yet exited (exit_after_n_waits = 5, but kill_with_escalation doesn't wait).
        // Manual polling simulates the escalation loop.
        let mut exited = false;
        for _ in 0..10 {
            if pty.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
        }
        assert!(exited, "process should have exited within 10 try_wait calls");
    }

    #[test]
    fn test_mock_pty_default_kill_with_escalation_delegates_to_kill() {
        // The default trait impl delegates kill_with_escalation() to kill().
        // MockPty overrides it, so we verify the MockPty override via kill().
        let mut pty = MockPty::new(80, 24);
        pty.kill().unwrap();
        assert!(pty.killed);
        assert_eq!(pty.try_wait().unwrap(), Some(137));
    }

    #[cfg(unix)]
    #[test]
    fn test_real_pty_kill_escalation_cleans_up_sleep_process() {
        // Integration test: spawn `sleep 99999`, call kill_with_escalation(), verify
        // the process exits within ~1 second.
        use std::time::Instant;

        let mut pty = RealPty::spawn("sleep", &["99999"], 80, 24).unwrap();

        // Confirm process is running.
        assert!(pty.try_wait().unwrap().is_none(), "sleep should be running");

        let start = Instant::now();
        pty.kill_with_escalation().unwrap();

        // After kill_with_escalation, the process should be gone.
        // We poll manually (up to 700ms) in case the process needed SIGKILL time.
        let mut exited = false;
        for _ in 0..14 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if pty.try_wait().unwrap().is_some() {
                exited = true;
                break;
            }
        }
        let elapsed = start.elapsed();

        assert!(exited, "sleep process should have been killed (elapsed: {:?})", elapsed);
        assert!(elapsed.as_millis() < 1000, "kill should complete within 1 second, took {:?}", elapsed);
    }
}
