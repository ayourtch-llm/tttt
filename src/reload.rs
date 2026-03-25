//! Live reload support via execv().
//!
//! Saves application state to a temp file, clears CLOEXEC on PTY master FDs,
//! and exec's the new binary. On startup, detects the restore file and
//! reconstructs the application state from inherited FDs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use tttt_pty::SessionStatus;

use crate::config::Config;

/// Top-level state saved before execv().
#[derive(Serialize, Deserialize)]
pub struct SavedState {
    /// Format version for forward-compatibility.
    pub version: u32,

    /// Per-session state including PTY master FD numbers.
    pub sessions: Vec<SavedSession>,

    /// Currently active session ID.
    pub active_session: Option<String>,

    /// Ordered session list (tab order).
    pub session_order: Vec<String>,

    /// SessionManager's next_id counter.
    pub next_session_id: u64,

    /// Cron jobs to restore (reminders are ephemeral and lost).
    pub cron_jobs: Vec<SavedCronJob>,

    /// Notification watchers to restore.
    pub watchers: Vec<SavedWatcher>,

    /// Scratchpad key-value store.
    pub scratchpad: HashMap<String, String>,

    /// App configuration.
    pub config: Config,

    /// Terminal dimensions at time of save.
    pub screen_cols: u16,
    pub screen_rows: u16,

    /// If true, the root session should be killed and relaunched (SIGUSR2 reload).
    #[serde(default)]
    pub restart_root: bool,
}

/// Saved state for a single PTY session.
#[derive(Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    pub name: Option<String>,
    pub command: String,
    pub status: SessionStatus,
    pub cols: u16,
    pub rows: u16,
    /// Raw FD number of the PTY master. Must survive exec (CLOEXEC cleared).
    pub master_fd: RawFd,
    /// Child PID if known (for waitpid/kill after restore).
    pub child_pid: Option<i32>,
    /// Full screen contents with ANSI formatting, for replaying into a fresh vt100 parser.
    #[serde(with = "base64_bytes")]
    pub screen_contents_formatted: Vec<u8>,
}

/// Saved cron job.
#[derive(Serialize, Deserialize)]
pub struct SavedCronJob {
    pub id: String,
    pub expression: String,
    pub command: String,
    pub session_id: Option<String>,
}

/// Saved notification watcher.
#[derive(Serialize, Deserialize)]
pub struct SavedWatcher {
    pub id: String,
    pub watch_session_id: String,
    pub pattern: String,
    pub inject_text: String,
    pub inject_session_id: String,
    pub one_shot: bool,
}

/// Current format version.
pub const STATE_VERSION: u32 = 1;

/// Environment variable pointing to the restore state file.
pub const RESTORE_ENV_VAR: &str = "TTTT_RESTORE_FILE";

impl SavedState {
    /// Write this state to a temp file and return the path.
    pub fn write_to_file(&self) -> Result<String, Box<dyn std::error::Error>> {
        let path = format!("/tmp/tttt-restore-{}.json", std::process::id());
        let json = serde_json::to_string(self)?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Read and delete a saved state file.
    pub fn read_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let json = std::fs::read_to_string(path)?;
        let state: Self = serde_json::from_str(&json)?;
        let _ = std::fs::remove_file(path);
        if state.version > STATE_VERSION {
            return Err(format!(
                "saved state version {} is newer than supported version {}",
                state.version, STATE_VERSION
            )
            .into());
        }
        Ok(state)
    }
}

/// Clear CLOEXEC on a file descriptor so it survives execv().
pub fn clear_cloexec(fd: RawFd) -> Result<(), Box<dyn std::error::Error>> {
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )?;
    Ok(())
}

/// Perform execv() to replace the current process.
///
/// This function does not return on success.
pub fn exec_self() -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let exe_cstr = std::ffi::CString::new(exe.to_string_lossy().as_bytes())?;
    let args: Vec<std::ffi::CString> = std::env::args()
        .map(|a| std::ffi::CString::new(a).unwrap())
        .collect();
    let args_ref: Vec<&std::ffi::CStr> = args.iter().map(|a| a.as_c_str()).collect();
    nix::unistd::execv(&exe_cstr, &args_ref)?;
    unreachable!("execv returned")
}

/// Serde helper for Vec<u8> as base64 in JSON.
mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use base64::{Engine, engine::general_purpose::STANDARD};

    pub fn serialize<S: Serializer>(data: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        STANDARD.encode(data).serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_saved_state_roundtrip() {
        let state = SavedState {
            version: STATE_VERSION,
            sessions: vec![SavedSession {
                id: "pty-1".to_string(),
                name: Some("root".to_string()),
                command: "bash".to_string(),
                status: SessionStatus::Running,
                cols: 80,
                rows: 24,
                master_fd: 5,
                child_pid: Some(12345),
                screen_contents_formatted: b"\x1b[1mhello\x1b[0m world".to_vec(),
            }],
            active_session: Some("pty-1".to_string()),
            session_order: vec!["pty-1".to_string()],
            next_session_id: 2,
            cron_jobs: vec![SavedCronJob {
                id: "cron-1".to_string(),
                expression: "10s".to_string(),
                command: "check".to_string(),
                session_id: Some("pty-1".to_string()),
            }],
            watchers: vec![SavedWatcher {
                id: "notify-1".to_string(),
                watch_session_id: "pty-1".to_string(),
                pattern: r"❯\s*$".to_string(),
                inject_text: "[DONE]".to_string(),
                inject_session_id: "pty-1".to_string(),
                one_shot: true,
            }],
            scratchpad: {
                let mut m = HashMap::new();
                m.insert("key1".to_string(), "value1".to_string());
                m
            },
            config: Config::default(),
            screen_cols: 120,
            screen_rows: 40,
            restart_root: false,
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, STATE_VERSION);
        assert!(!restored.restart_root);
        assert_eq!(restored.sessions.len(), 1);
        assert_eq!(restored.sessions[0].id, "pty-1");
        assert_eq!(restored.sessions[0].master_fd, 5);
        assert_eq!(restored.sessions[0].child_pid, Some(12345));
        assert_eq!(
            restored.sessions[0].screen_contents_formatted,
            b"\x1b[1mhello\x1b[0m world"
        );
        assert_eq!(restored.active_session, Some("pty-1".to_string()));
        assert_eq!(restored.cron_jobs.len(), 1);
        assert_eq!(restored.watchers.len(), 1);
        assert_eq!(restored.scratchpad.get("key1").unwrap(), "value1");
    }

    #[test]
    fn test_saved_state_file_roundtrip() {
        let state = SavedState {
            version: STATE_VERSION,
            sessions: vec![],
            active_session: None,
            session_order: vec![],
            next_session_id: 1,
            cron_jobs: vec![],
            watchers: vec![],
            scratchpad: HashMap::new(),
            config: Config::default(),
            screen_cols: 80,
            screen_rows: 24,
            restart_root: false,
        };

        let path = state.write_to_file().unwrap();
        assert!(std::path::Path::new(&path).exists());

        let restored = SavedState::read_from_file(&path).unwrap();
        assert_eq!(restored.version, STATE_VERSION);
        // File should be deleted after read
        assert!(!std::path::Path::new(&path).exists());
    }
}
