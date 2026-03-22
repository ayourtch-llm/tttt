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
    /// Spawn a new PTY process with the given command and dimensions.
    pub fn spawn(command: &str, args: &[&str], cols: u16, rows: u16) -> Result<Self> {
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
}

impl RealPty {
    /// Get the raw file descriptor of the PTY reader for use with poll().
    #[cfg(unix)]
    pub fn reader_raw_fd(&self) -> i32 {
        self.reader_raw_fd
    }
}

impl PtyBackend for RealPty {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
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
        Ok(self.exit_code)
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
}
