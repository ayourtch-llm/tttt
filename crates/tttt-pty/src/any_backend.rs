//! Unified PTY backend enum supporting both freshly-spawned and restored sessions.
//!
//! This allows a single `SessionManager<AnyPty>` to hold sessions created
//! during normal startup alongside sessions restored after a live reload.

use crate::backend::{PtyBackend, RealPty};
use crate::error::Result;
use crate::restored::RestoredPty;

/// A PTY backend that is either a freshly-spawned RealPty or a restored one.
pub enum AnyPty {
    Real(RealPty),
    Restored(RestoredPty),
}

impl AnyPty {
    /// Get the raw file descriptor for poll() integration.
    #[cfg(unix)]
    pub fn reader_raw_fd(&self) -> i32 {
        match self {
            AnyPty::Real(pty) => pty.reader_raw_fd(),
            AnyPty::Restored(pty) => pty.reader_raw_fd(),
        }
    }
}

impl PtyBackend for AnyPty {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        match self {
            AnyPty::Real(pty) => pty.write(data),
            AnyPty::Restored(pty) => pty.write(data),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self {
            AnyPty::Real(pty) => pty.read(buf),
            AnyPty::Restored(pty) => pty.read(buf),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        match self {
            AnyPty::Real(pty) => pty.resize(cols, rows),
            AnyPty::Restored(pty) => pty.resize(cols, rows),
        }
    }

    fn kill(&mut self) -> Result<()> {
        match self {
            AnyPty::Real(pty) => pty.kill(),
            AnyPty::Restored(pty) => pty.kill(),
        }
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        match self {
            AnyPty::Real(pty) => pty.try_wait(),
            AnyPty::Restored(pty) => pty.try_wait(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::PtySession;
    use crate::manager::SessionManager;

    #[test]
    fn test_any_pty_restored_in_session_manager() {
        // Create a real PTY pair via libc
        let mut master: std::os::unix::io::RawFd = 0;
        let mut slave: std::os::unix::io::RawFd = 0;
        let ret = unsafe {
            libc::openpty(
                &mut master, &mut slave,
                std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, 0);

        // Wrap in RestoredPty, then AnyPty
        let restored = RestoredPty::from_raw_fd(master, None).unwrap();
        let any = AnyPty::Restored(restored);

        // Use in a PtySession and SessionManager
        let session = PtySession::new("pty-1".to_string(), any, "bash".to_string(), 80, 24);
        let mut mgr: SessionManager<AnyPty> = SessionManager::new();
        mgr.add_session(session).unwrap();

        assert_eq!(mgr.session_count(), 1);
        assert!(mgr.exists("pty-1"));

        // Write from slave side, read from session
        let msg = b"hello\n";
        unsafe { libc::write(slave, msg.as_ptr() as *const _, msg.len()) };
        std::thread::sleep(std::time::Duration::from_millis(50));

        let session = mgr.get_mut("pty-1").unwrap();
        let n = session.pump().unwrap();
        assert!(n > 0, "should have read data from restored PTY");
        assert!(session.get_screen().contains("hello"));

        unsafe { libc::close(slave); }
    }
}
