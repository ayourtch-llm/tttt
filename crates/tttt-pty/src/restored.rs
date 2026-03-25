//! Restored PTY backend for sessions inherited across execv().
//!
//! After a live reload, we have raw file descriptors for PTY masters
//! but no portable-pty handles. This backend wraps those raw FDs
//! and uses direct syscalls for process management.

use crate::error::{PtyError, Result};
use crate::backend::PtyBackend;
use std::os::unix::io::RawFd;

/// PTY backend reconstructed from a raw file descriptor inherited across execv().
///
/// Unlike RealPty (which spawns a child via portable-pty), this wraps an
/// already-open master FD whose child process is still running.
pub struct RestoredPty {
    master_fd: RawFd,
    child_pid: Option<nix::unistd::Pid>,
}

impl RestoredPty {
    /// Wrap an inherited PTY master file descriptor.
    ///
    /// `master_fd` must be a valid, open file descriptor for a PTY master.
    /// `child_pid` is the PID of the child process (if known).
    ///
    /// Sets the FD to non-blocking mode and re-enables CLOEXEC.
    pub fn from_raw_fd(master_fd: RawFd, child_pid: Option<i32>) -> Result<Self> {
        // Validate the FD is open
        nix::fcntl::fcntl(master_fd, nix::fcntl::FcntlArg::F_GETFD)
            .map_err(|e| PtyError::Spawn(format!("inherited FD {} is invalid: {}", master_fd, e)))?;

        // Set non-blocking mode
        let flags = nix::fcntl::fcntl(master_fd, nix::fcntl::FcntlArg::F_GETFL)
            .map_err(|e| PtyError::Spawn(format!("fcntl F_GETFL on fd {}: {}", master_fd, e)))?;
        nix::fcntl::fcntl(
            master_fd,
            nix::fcntl::FcntlArg::F_SETFL(
                nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
            ),
        )
        .map_err(|e| PtyError::Spawn(format!("fcntl F_SETFL on fd {}: {}", master_fd, e)))?;

        // Re-enable CLOEXEC (was cleared before exec, restore it now)
        nix::fcntl::fcntl(
            master_fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )
        .map_err(|e| PtyError::Spawn(format!("fcntl F_SETFD on fd {}: {}", master_fd, e)))?;

        let pid = child_pid.map(nix::unistd::Pid::from_raw);

        Ok(Self {
            master_fd,
            child_pid: pid,
        })
    }

    /// Get the raw file descriptor.
    pub fn reader_raw_fd(&self) -> i32 {
        self.master_fd
    }

    /// Try to discover the foreground process group of the PTY.
    /// This can be used to find the child PID if it wasn't provided.
    pub fn discover_foreground_pgid(&self) -> Option<i32> {
        // TIOCGPGRP returns the foreground process group ID
        let mut pgid: libc::pid_t = 0;
        let ret = unsafe { libc::ioctl(self.master_fd, libc::TIOCGPGRP, &mut pgid) };
        if ret == 0 && pgid > 0 {
            Some(pgid)
        } else {
            None
        }
    }
}

impl PtyBackend for RestoredPty {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        use std::os::unix::io::BorrowedFd;

        const CHUNK_SIZE: usize = 512;
        const MAX_RETRIES: usize = 100;

        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + CHUNK_SIZE).min(data.len());
            let mut retries = 0;
            loop {
                let borrowed = unsafe { BorrowedFd::borrow_raw(self.master_fd) };
                match nix::unistd::write(borrowed, &data[offset..end]) {
                    Ok(n) => {
                        offset += n;
                        break;
                    }
                    Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => {
                        retries += 1;
                        if retries >= MAX_RETRIES {
                            return Err(PtyError::Io(std::io::Error::new(
                                std::io::ErrorKind::WouldBlock,
                                "PTY write timed out after retries",
                            )));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => return Err(PtyError::Io(std::io::Error::from(e))),
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match nix::unistd::read(self.master_fd, buf) {
            Ok(n) => Ok(n),
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => Ok(0),
            Err(nix::errno::Errno::EIO) => Ok(0), // PTY closed
            Err(e) => Err(PtyError::Io(std::io::Error::from(e))),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ret = unsafe { libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws) };
        if ret == 0 {
            Ok(())
        } else {
            Err(PtyError::Resize(format!(
                "TIOCSWINSZ failed: {}",
                std::io::Error::last_os_error()
            )))
        }
    }

    fn kill(&mut self) -> Result<()> {
        if let Some(pid) = self.child_pid {
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM)
                .map_err(|e| PtyError::Io(std::io::Error::from(e)))?;
            Ok(())
        } else if let Some(pgid) = self.discover_foreground_pgid() {
            // Kill the process group
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pgid),
                nix::sys::signal::Signal::SIGTERM,
            )
            .map_err(|e| PtyError::Io(std::io::Error::from(e)))?;
            Ok(())
        } else {
            Err(PtyError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "cannot kill: child PID unknown",
            )))
        }
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        if let Some(pid) = self.child_pid {
            match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => Ok(Some(code)),
                Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => Ok(Some(128)),
                Ok(nix::sys::wait::WaitStatus::StillAlive) => Ok(None),
                Ok(_) => Ok(None),
                Err(nix::errno::Errno::ECHILD) => {
                    // Child was already reaped or doesn't exist — check if PTY is still alive
                    // by trying a zero-byte read
                    let mut probe = [0u8; 0];
                    match nix::unistd::read(self.master_fd, &mut probe) {
                        Err(nix::errno::Errno::EIO) => Ok(Some(0)), // PTY closed
                        _ => Ok(None), // Assume still running
                    }
                }
                Err(e) => Err(PtyError::Io(std::io::Error::from(e))),
            }
        } else {
            // No PID — probe the PTY
            let mut probe = [0u8; 1];
            match nix::unistd::read(self.master_fd, &mut probe) {
                Err(nix::errno::Errno::EIO) => Ok(Some(0)), // PTY slave closed
                _ => Ok(None), // Still open (EAGAIN = no data but alive)
            }
        }
    }
}

impl Drop for RestoredPty {
    fn drop(&mut self) {
        let _ = nix::unistd::close(self.master_fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_restored_pty_invalid_fd() {
        // FD 9999 should not be open
        let result = RestoredPty::from_raw_fd(9999, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_restored_pty_from_real_pty() {
        // Create a real PTY pair via libc, then wrap it in RestoredPty
        use std::os::unix::io::RawFd;

        let mut master: RawFd = 0;
        let mut slave: RawFd = 0;
        let ret = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if ret != 0 {
            panic!("openpty failed");
        }

        // Wrap the master FD
        let mut restored = RestoredPty::from_raw_fd(master, None).unwrap();

        // Write something to the slave side, read from master
        let msg = b"hello from slave\n";
        let written = unsafe { libc::write(slave, msg.as_ptr() as *const _, msg.len()) };
        assert!(written > 0);

        // Give it a moment to arrive
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut buf = [0u8; 256];
        let n = restored.read(&mut buf).unwrap();
        assert!(n > 0);

        // Write to master, it should appear on slave
        restored.write(b"hello from master\n").unwrap();

        // Resize should work
        restored.resize(120, 40).unwrap();

        // try_wait with no child should return None (PTY still open)
        assert_eq!(restored.try_wait().unwrap(), None);

        // Clean up slave
        unsafe { libc::close(slave); }
        // After slave closes, try_wait should eventually detect it
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Note: EIO detection on try_wait may not work immediately on all platforms
    }
}
