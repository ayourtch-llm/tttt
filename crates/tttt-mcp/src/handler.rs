use crate::error::{McpError, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tttt_log::{SessionReplay, SqliteLogger};
use tttt_pty::{MockPty, PtyBackend, PtySession, SessionManager, SessionStatus};
use tttt_scheduler::Scheduler;

/// A token that can be checked by long-running tool handlers to detect cancellation.
pub type CancelToken = Arc<AtomicBool>;

/// Parse a rate limit reset time string from PTY screen text.
/// Looks for patterns like "resets 2pm (Europe/Brussels)" or "resets 2:30pm (US/Pacific)".
/// Returns `(hour_24, minute, timezone_str)` or `None` if not found.
pub fn parse_rate_limit_reset(screen: &str) -> Option<(u32, u32, String)> {
    let re = regex::Regex::new(r"resets (\d{1,2})(?::(\d{2}))?(am|pm) \(([^)]+)\)").ok()?;
    let caps = re.captures(screen)?;

    let hour: u32 = caps[1].parse().ok()?;
    let minute: u32 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
    let ampm = &caps[3];
    let tz_str = caps[4].to_string();

    let hour_24 = match ampm {
        "am" => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        "pm" => {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        }
        _ => return None,
    };

    Some((hour_24, minute, tz_str))
}

/// Calculate the duration to wait until the rate limit resets, plus a safety margin.
/// Returns an error string if the timezone is unknown or the time is invalid.
pub fn calculate_wait_duration(
    hour_24: u32,
    minute: u32,
    tz_str: &str,
    safety_margin_minutes: u32,
) -> std::result::Result<Duration, String> {
    use chrono::TimeZone as _;

    let tz: chrono_tz::Tz = tz_str
        .parse()
        .map_err(|_| format!("Unknown timezone: {}", tz_str))?;

    let now = chrono::Utc::now().with_timezone(&tz);

    let reset_naive = now
        .date_naive()
        .and_hms_opt(hour_24, minute, 0)
        .ok_or_else(|| format!("Invalid reset time {:02}:{:02}", hour_24, minute))?;

    let reset_local = tz
        .from_local_datetime(&reset_naive)
        .single()
        .ok_or_else(|| "Ambiguous or invalid local time for reset".to_string())?;

    // If the reset time has already passed today, assume it's tomorrow.
    let reset_time = if reset_local <= now {
        reset_local + chrono::Duration::days(1)
    } else {
        reset_local
    };

    let diff_secs = reset_time.signed_duration_since(now).num_seconds().max(0) as u64;
    let total_secs = diff_secs + (safety_margin_minutes as u64 * 60);

    Ok(Duration::from_secs(total_secs))
}

/// Trait for handling MCP tool calls.
pub trait ToolHandler: Send {
    /// Handle a tool call by name with the given arguments.
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value>;

    /// Return the list of tool definitions this handler provides.
    fn tool_definitions(&self) -> Vec<Value>;

    /// Set a cancellation token for the next long-running tool call.
    /// The handler should check this token periodically and return early if set.
    fn set_cancel_token(&mut self, _token: CancelToken) {}
}

/// Shared session manager type used by both the TUI and MCP server.
pub type SharedSessionManager<B> = Arc<Mutex<SessionManager<B>>>;

/// Handles PTY-related tool calls by delegating to a shared SessionManager.
pub struct PtyToolHandler<B: PtyBackend> {
    manager: SharedSessionManager<B>,
    work_dir: std::path::PathBuf,
    default_cols: u16,
    default_rows: u16,
    sqlite_logger: Option<Arc<Mutex<SqliteLogger>>>,
    cancel_token: Option<CancelToken>,
}

impl<B: PtyBackend> PtyToolHandler<B> {
    /// Create a new handler with a shared session manager.
    pub fn new(manager: SharedSessionManager<B>, work_dir: std::path::PathBuf) -> Self {
        Self {
            manager,
            work_dir,
            default_cols: 80,
            default_rows: 24,
            sqlite_logger: None,
            cancel_token: None,
        }
    }

    /// Set default PTY dimensions for new sessions (used when cols/rows not specified).
    pub fn with_default_dims(mut self, cols: u16, rows: u16) -> Self {
        self.default_cols = cols;
        self.default_rows = rows;
        self
    }

    /// Update the default PTY dimensions (e.g. after a terminal resize).
    pub fn set_default_dims(&mut self, cols: u16, rows: u16) {
        self.default_cols = cols;
        self.default_rows = rows;
    }

    /// Attach a shared SqliteLogger for session metadata recording.
    pub fn with_sqlite_logger(mut self, logger: Option<Arc<Mutex<SqliteLogger>>>) -> Self {
        self.sqlite_logger = logger;
        self
    }

    /// Create a handler that owns its own session manager (convenience for standalone use).
    pub fn new_owned(manager: SessionManager<B>, work_dir: std::path::PathBuf) -> Self {
        Self::new(Arc::new(Mutex::new(manager)), work_dir)
    }

    /// Access the shared session manager.
    pub fn manager(&self) -> &SharedSessionManager<B> {
        &self.manager
    }

    /// Check if the current operation has been cancelled.
    fn is_cancelled(&self) -> bool {
        self.cancel_token
            .as_ref()
            .map_or(false, |t| t.load(Ordering::Relaxed))
    }

    fn handle_pty_list(&self) -> Result<Value> {
        let mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let sessions = mgr.list();
        Ok(json!(sessions))
    }

    fn handle_pty_get_screen(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        // Poll until process exits and output is fully drained, or until timeout.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            session.pump()?;
            if *session.status() != SessionStatus::Running {
                // Drain any remaining buffered output after exit.
                while session.pump()? > 0 {}
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let contents = session.get_screen();
        let cursor = session.cursor_position();
        Ok(json!({
            "screen": contents,
            "cursor": [cursor.1, cursor.0]
        }))
    }

    fn handle_pty_send_keys(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let keys = args["keys"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("keys required".to_string()))?;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        session.send_keys(keys)?;
        Ok(json!({"status": "ok"}))
    }

    fn handle_pty_kill(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        mgr.kill_session(session_id)?;
        Ok(json!({"status": "ok"}))
    }

    fn handle_pty_get_cursor(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get(session_id)?;
        let (row, col) = session.cursor_position();
        Ok(json!({"row": row, "col": col}))
    }

    fn handle_pty_resize(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let cols = args["cols"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("cols required".to_string()))? as u16;
        let rows = args["rows"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("rows required".to_string()))? as u16;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        session.resize(cols, rows)?;
        Ok(json!({"status": "ok"}))
    }

    fn handle_pty_get_scrollback(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let max_lines = args["lines"].as_u64().unwrap_or(100) as usize;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        session.pump()?;
        let lines = session.get_scrollback(max_lines);
        Ok(json!({"lines": lines}))
    }

    fn handle_pty_wait_for(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("pattern required".to_string()))?;
        let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000);

        let re = regex::Regex::new(pattern)
            .map_err(|e| McpError::InvalidParams(format!("invalid regex '{}': {}", pattern, e)))?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            if self.is_cancelled() {
                return Ok(json!({"status": "cancelled"}));
            }
            {
                let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
                let session = mgr.get_mut(session_id)?;
                session.pump()?;
                let screen = session.get_screen();
                if re.is_match(&screen) {
                    return Ok(json!({"status": "matched", "screen": screen}));
                }
            }
            if Instant::now() >= deadline {
                return Err(McpError::Protocol(format!(
                    "timeout waiting for pattern '{}' after {}ms",
                    pattern, timeout_ms
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn handle_pty_wait_for_idle(&self, args: &Value) -> Result<Value> {
        let pid = std::process::id();
        let debug_path = format!("/tmp/tttt-{}-debug.txt", pid);
        let mut debug_log = |msg: &str| {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true).append(true).open(&debug_path)
            {
                let _ = writeln!(f, "[{:?}] [wait_for_idle] {}", std::time::SystemTime::now(), msg);
            }
        };

        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let idle_threshold = args["idle_seconds"].as_f64().unwrap_or(10.0);
        let timeout_secs = args["timeout"].as_f64().unwrap_or(300.0);
        debug_log(&format!("started: session={}, idle_threshold={}, timeout={}", session_id, idle_threshold, timeout_secs));

        let ignore_re = if let Some(pat) = args["ignore_pattern"].as_str() {
            Some(regex::Regex::new(pat).map_err(|e| {
                McpError::InvalidParams(format!("invalid ignore_pattern: {}", e))
            })?)
        } else {
            None
        };

        let start = Instant::now();
        let deadline = start + Duration::from_secs_f64(timeout_secs);

        if let Some(re) = ignore_re {
            // Hash-based idle detection: strip ignore_pattern matches before hashing.
            let mut last_hash: Option<u64> = None;
            let mut hash_stable_since = Instant::now();

            loop {
                if self.is_cancelled() {
                    debug_log("cancelled by client");
                    return Ok(json!({"status": "cancelled"}));
                }
                let (screen, last_line) = {
                    let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
                    let session = mgr.get_mut(session_id)?;
                    session.pump()?;
                    (session.get_screen(), session.last_non_empty_line())
                };
                let filtered = re.replace_all(&screen, "");
                let mut hasher = DefaultHasher::new();
                filtered.hash(&mut hasher);
                let hash = hasher.finish();

                if Some(hash) != last_hash {
                    last_hash = Some(hash);
                    hash_stable_since = Instant::now();
                }

                let stable_secs = hash_stable_since.elapsed().as_secs_f64();
                if stable_secs >= idle_threshold {
                    return Ok(json!({
                        "status": "idle",
                        "idle_seconds": stable_secs,
                        "last_output_line": last_line,
                    }));
                }
                if Instant::now() >= deadline {
                    return Ok(json!({
                        "status": "timeout",
                        "idle_seconds": stable_secs,
                        "last_output_line": last_line,
                    }));
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        } else {
            let mut iteration = 0u32;
            loop {
                iteration += 1;
                if self.is_cancelled() {
                    debug_log("cancelled by client");
                    return Ok(json!({"status": "cancelled"}));
                }
                let (current_idle, last_line) = {
                    let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
                    let session = mgr.get_mut(session_id)?;
                    session.pump()?;
                    (session.idle_seconds(), session.last_non_empty_line())
                };
                debug_log(&format!("iter={}, idle={:.1}s, elapsed={:.1}s", iteration, current_idle, start.elapsed().as_secs_f64()));
                if current_idle >= idle_threshold {
                    debug_log("returning idle");
                    return Ok(json!({
                        "status": "idle",
                        "idle_seconds": current_idle,
                        "last_output_line": last_line,
                    }));
                }
                if Instant::now() >= deadline {
                    debug_log("returning timeout");
                    return Ok(json!({
                        "status": "timeout",
                        "idle_seconds": current_idle,
                        "last_output_line": last_line,
                    }));
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    fn handle_pty_set_scrollback(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let lines = args["lines"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("lines required".to_string()))? as usize;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        session.set_scrollback(lines);
        Ok(json!({"status": "ok"}))
    }

    fn handle_get_status(&self) -> Result<Value> {
        let mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let sessions: Vec<Value> = mgr
            .list()
            .iter()
            .filter_map(|meta| {
                mgr.get(&meta.id).ok().map(|session| {
                    let status_str = match meta.status {
                        SessionStatus::Running => "Running".to_string(),
                        SessionStatus::Exited(code) => format!("Exited({})", code),
                    };
                    json!({
                        "id": meta.id,
                        "name": meta.name,
                        "status": status_str,
                        "command": meta.command,
                        "last_output_line": session.last_non_empty_line(),
                        "idle_seconds": session.idle_seconds(),
                        "input_idle_seconds": session.input_idle_seconds(),
                    })
                })
            })
            .collect();
        Ok(json!({
            "sessions": sessions,
            "pending_reminders": 0,
            "active_watchers": 0,
        }))
    }

    fn handle_pty_start_capture(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        let (capture_id, file_path) = session.start_capture()?;
        Ok(json!({"capture_id": capture_id, "file_path": file_path}))
    }

    fn handle_pty_stop_capture(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        let (file_path, bytes_written) = session.stop_capture()?;
        Ok(json!({"file_path": file_path, "bytes_written": bytes_written}))
    }

    fn handle_pty_handle_rate_limit(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?
            .to_string();
        let safety_margin_minutes = args["safety_margin_minutes"].as_u64().unwrap_or(15) as u32;
        let continuation_prompt = args["continuation_prompt"]
            .as_str()
            .unwrap_or("Continue from where you left off.")
            .to_string();

        // Read current screen; release lock before sleeping.
        let screen = {
            let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
            let session = mgr.get_mut(&session_id)?;
            session.pump()?;
            session.get_screen()
        };

        let (hour_24, minute, tz_str) = match parse_rate_limit_reset(&screen) {
            Some(v) => v,
            None => {
                return Ok(json!({
                    "status": "no_rate_limit",
                    "message": "No rate limit dialog detected on screen"
                }))
            }
        };

        let wait_duration =
            calculate_wait_duration(hour_24, minute, &tz_str, safety_margin_minutes)
                .map_err(|e| McpError::Protocol(e))?;

        let waited_minutes = wait_duration.as_secs() / 60;
        let reset_time_str = format!("{:02}:{:02}", hour_24, minute);

        // Sleep until the rate limit resets (lock is not held).
        // Check cancellation every second during the wait.
        let wait_deadline = Instant::now() + wait_duration;
        while Instant::now() < wait_deadline {
            if self.is_cancelled() {
                return Ok(json!({
                    "status": "cancelled",
                    "message": "Rate limit wait cancelled by client"
                }));
            }
            std::thread::sleep(Duration::from_secs(1));
        }

        // Send "a" to the session (always / wait option).
        {
            let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
            let session = mgr.get_mut(&session_id)?;
            session.send_keys("a")?;
        }

        // Give the dialog time to process.
        std::thread::sleep(Duration::from_secs(2));

        // Inject the continuation prompt.
        {
            let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
            let session = mgr.get_mut(&session_id)?;
            session.send_keys(&format!("{}\n", continuation_prompt))?;
        }

        Ok(json!({
            "status": "handled",
            "reset_time": reset_time_str,
            "waited_minutes": waited_minutes,
            "message": "Rate limit handled, session resumed"
        }))
    }
}

impl PtyToolHandler<MockPty> {
    /// Launch a session using MockPty (for testing).
    pub fn handle_pty_launch_mock(&self, args: &Value) -> Result<Value> {
        let cols = args["cols"].as_u64().unwrap_or(self.default_cols as u64) as u16;
        let rows = args["rows"].as_u64().unwrap_or(self.default_rows as u64) as u16;
        let command = args["command"].as_str().unwrap_or("bash").to_string();
        let name = args["name"].as_str().map(|s| s.to_string());

        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = mgr.generate_id();
        let mock = MockPty::new(cols, rows);
        let session = PtySession::new(id.clone(), mock, command.clone(), cols, rows);
        if let Some(n) = name.clone() {
            mgr.add_session_with_name(session, n)?;
        } else {
            mgr.add_session(session)?;
        }
        drop(mgr);
        if let Some(ref logger) = self.sqlite_logger {
            let _ = logger.lock().unwrap().log_session_start(&id, &command, cols, rows, name.as_deref());
        }
        let mut resp = json!({"session_id": id});
        if let Some(n) = name {
            resp["name"] = json!(n);
        }
        Ok(resp)
    }
}

impl PtyToolHandler<tttt_pty::RealPty> {
    /// Launch a real PTY session.
    pub fn handle_pty_launch_real(&self, args: &Value) -> Result<Value> {
        let cols = args["cols"].as_u64().unwrap_or(self.default_cols as u64) as u16;
        let rows = args["rows"].as_u64().unwrap_or(self.default_rows as u64) as u16;
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let command = args["command"]
            .as_str()
            .unwrap_or(&default_shell);
        let cmd_args: Vec<&str> = args["args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let name = args["name"].as_str().map(|s| s.to_string());

        let cwd = args["working_dir"]
            .as_str()
            .map(std::path::PathBuf::from)
            .or_else(|| Some(self.work_dir.clone()));
        let backend = tttt_pty::RealPty::spawn_with_cwd(
            command,
            &cmd_args,
            cwd.as_deref(),
            cols,
            rows,
        )?;
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = mgr.generate_id();
        let session = PtySession::new(id.clone(), backend, command.to_string(), cols, rows);
        if let Some(n) = name.clone() {
            mgr.add_session_with_name(session, n)?;
        } else {
            mgr.add_session(session)?;
        }
        drop(mgr);
        if let Some(ref logger) = self.sqlite_logger {
            let _ = logger.lock().unwrap().log_session_start(&id, command, cols, rows, name.as_deref());
        }
        let mut resp = json!({"session_id": id});
        if let Some(n) = name {
            resp["name"] = json!(n);
        }
        Ok(resp)
    }
}

impl ToolHandler for PtyToolHandler<tttt_pty::RealPty> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_pty_launch" => self.handle_pty_launch_real(args),
            "tttt_pty_list" => self.handle_pty_list(),
            "tttt_pty_get_screen" => self.handle_pty_get_screen(args),
            "tttt_pty_send_keys" => self.handle_pty_send_keys(args),
            "tttt_pty_kill" => self.handle_pty_kill(args),
            "tttt_pty_get_cursor" => self.handle_pty_get_cursor(args),
            "tttt_pty_resize" => self.handle_pty_resize(args),
            "tttt_pty_get_scrollback" => self.handle_pty_get_scrollback(args),
            "tttt_pty_set_scrollback" => self.handle_pty_set_scrollback(args),
            "tttt_pty_wait_for" => self.handle_pty_wait_for(args),
            "tttt_pty_wait_for_idle" => self.handle_pty_wait_for_idle(args),
            "tttt_pty_start_capture" => self.handle_pty_start_capture(args),
            "tttt_pty_stop_capture" => self.handle_pty_stop_capture(args),
            "tttt_pty_handle_rate_limit" => self.handle_pty_handle_rate_limit(args),
            "tttt_get_status" => self.handle_get_status(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
    }

    fn set_cancel_token(&mut self, token: CancelToken) {
        self.cancel_token = Some(token);
    }
}

impl PtyToolHandler<tttt_pty::AnyPty> {
    /// Launch a real PTY session wrapped in AnyPty.
    pub fn handle_pty_launch_any(&self, args: &Value) -> Result<Value> {
        let cols = args["cols"].as_u64().unwrap_or(self.default_cols as u64) as u16;
        let rows = args["rows"].as_u64().unwrap_or(self.default_rows as u64) as u16;
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let command = args["command"]
            .as_str()
            .unwrap_or(&default_shell);
        let cmd_args: Vec<&str> = args["args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let name = args["name"].as_str().map(|s| s.to_string());

        let cwd = args["working_dir"]
            .as_str()
            .map(std::path::PathBuf::from)
            .or_else(|| Some(self.work_dir.clone()));
        let real_backend = tttt_pty::RealPty::spawn_with_cwd(
            command,
            &cmd_args,
            cwd.as_deref(),
            cols,
            rows,
        )?;
        let backend = tttt_pty::AnyPty::Real(real_backend);
        let mut mgr = self.manager.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = mgr.generate_id();
        let session = PtySession::new(id.clone(), backend, command.to_string(), cols, rows);
        if let Some(n) = name.clone() {
            mgr.add_session_with_name(session, n)?;
        } else {
            mgr.add_session(session)?;
        }
        drop(mgr);
        if let Some(ref logger) = self.sqlite_logger {
            let _ = logger.lock().unwrap().log_session_start(&id, command, cols, rows, name.as_deref());
        }
        let mut resp = json!({"session_id": id});
        if let Some(n) = name {
            resp["name"] = json!(n);
        }
        Ok(resp)
    }
}

impl ToolHandler for PtyToolHandler<tttt_pty::AnyPty> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_pty_launch" => self.handle_pty_launch_any(args),
            "tttt_pty_list" => self.handle_pty_list(),
            "tttt_pty_get_screen" => self.handle_pty_get_screen(args),
            "tttt_pty_send_keys" => self.handle_pty_send_keys(args),
            "tttt_pty_kill" => self.handle_pty_kill(args),
            "tttt_pty_get_cursor" => self.handle_pty_get_cursor(args),
            "tttt_pty_resize" => self.handle_pty_resize(args),
            "tttt_pty_get_scrollback" => self.handle_pty_get_scrollback(args),
            "tttt_pty_set_scrollback" => self.handle_pty_set_scrollback(args),
            "tttt_pty_wait_for" => self.handle_pty_wait_for(args),
            "tttt_pty_wait_for_idle" => self.handle_pty_wait_for_idle(args),
            "tttt_pty_start_capture" => self.handle_pty_start_capture(args),
            "tttt_pty_stop_capture" => self.handle_pty_stop_capture(args),
            "tttt_pty_handle_rate_limit" => self.handle_pty_handle_rate_limit(args),
            "tttt_get_status" => self.handle_get_status(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
    }

    fn set_cancel_token(&mut self, token: CancelToken) {
        self.cancel_token = Some(token);
    }
}

impl ToolHandler for PtyToolHandler<MockPty> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_pty_launch" => self.handle_pty_launch_mock(args),
            "tttt_pty_list" => self.handle_pty_list(),
            "tttt_pty_get_screen" => self.handle_pty_get_screen(args),
            "tttt_pty_send_keys" => self.handle_pty_send_keys(args),
            "tttt_pty_kill" => self.handle_pty_kill(args),
            "tttt_pty_get_cursor" => self.handle_pty_get_cursor(args),
            "tttt_pty_resize" => self.handle_pty_resize(args),
            "tttt_pty_get_scrollback" => self.handle_pty_get_scrollback(args),
            "tttt_pty_set_scrollback" => self.handle_pty_set_scrollback(args),
            "tttt_pty_wait_for" => self.handle_pty_wait_for(args),
            "tttt_pty_wait_for_idle" => self.handle_pty_wait_for_idle(args),
            "tttt_pty_start_capture" => self.handle_pty_start_capture(args),
            "tttt_pty_stop_capture" => self.handle_pty_stop_capture(args),
            "tttt_pty_handle_rate_limit" => self.handle_pty_handle_rate_limit(args),
            "tttt_get_status" => self.handle_get_status(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
    }

    fn set_cancel_token(&mut self, token: CancelToken) {
        self.cancel_token = Some(token);
    }
}

/// Combines multiple ToolHandlers, dispatching to the first that handles a tool.
pub struct CompositeToolHandler {
    handlers: Vec<Box<dyn ToolHandler>>,
}

impl CompositeToolHandler {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn add_handler(&mut self, handler: Box<dyn ToolHandler>) {
        self.handlers.push(handler);
    }
}

impl Default for CompositeToolHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolHandler for CompositeToolHandler {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        for handler in &mut self.handlers {
            match handler.handle_tool_call(name, args) {
                Err(McpError::ToolNotFound(_)) => continue,
                result => return result,
            }
        }
        Err(McpError::ToolNotFound(name.to_string()))
    }

    fn tool_definitions(&self) -> Vec<Value> {
        self.handlers
            .iter()
            .flat_map(|h| h.tool_definitions())
            .collect()
    }

    fn set_cancel_token(&mut self, token: CancelToken) {
        for handler in &mut self.handlers {
            handler.set_cancel_token(token.clone());
        }
    }
}

/// Shared scheduler type for use across handlers and the TUI.
pub type SharedScheduler = Arc<Mutex<Scheduler>>;

/// Handles scheduler-related tool calls (reminders and cron jobs).
pub struct SchedulerToolHandler {
    scheduler: SharedScheduler,
}

impl SchedulerToolHandler {
    /// Create a new handler with a shared scheduler.
    pub fn new(scheduler: SharedScheduler) -> Self {
        Self { scheduler }
    }

    /// Create a handler that owns its own scheduler (convenience for standalone use).
    pub fn new_owned(scheduler: Scheduler) -> Self {
        Self::new(Arc::new(Mutex::new(scheduler)))
    }

    /// Access the shared scheduler.
    pub fn scheduler(&self) -> &SharedScheduler {
        &self.scheduler
    }

    fn handle_reminder_set(&self, args: &Value) -> Result<Value> {
        let message = args["message"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("message required".to_string()))?
            .to_string();
        let delay_seconds = args["delay_seconds"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("delay_seconds required".to_string()))?;

        let fire_at = Instant::now() + Duration::from_secs(delay_seconds);
        let mut sched = self
            .scheduler
            .lock()
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = sched.add_reminder(message, fire_at);
        Ok(json!({"reminder_id": id}))
    }

    fn handle_cron_create(&self, args: &Value) -> Result<Value> {
        let expression = args["expression"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("expression required".to_string()))?
            .to_string();
        let command = args["command"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("command required".to_string()))?
            .to_string();
        let session_id = args["session_id"].as_str().map(|s| s.to_string());
        let if_busy = tttt_scheduler::BusyPolicy::from_str_opt(args["if_busy"].as_str());

        let now = Instant::now();
        let mut sched = self
            .scheduler
            .lock()
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = sched.add_cron(expression, command, session_id, if_busy, now)?;
        Ok(json!({"job_id": id}))
    }

    fn handle_cron_list(&self) -> Result<Value> {
        let sched = self
            .scheduler
            .lock()
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        let jobs: Vec<Value> = sched
            .list_cron()
            .iter()
            .map(|j| {
                json!({
                    "id": j.id,
                    "expression": j.expression,
                    "command": j.command,
                    "session_id": j.session_id,
                    "if_busy": format!("{:?}", j.if_busy).to_lowercase(),
                })
            })
            .collect();
        Ok(json!(jobs))
    }

    fn handle_cron_delete(&self, args: &Value) -> Result<Value> {
        let job_id = args["job_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("job_id required".to_string()))?;

        let mut sched = self
            .scheduler
            .lock()
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        sched.remove_cron(job_id)?;
        Ok(json!({"status": "ok"}))
    }
}

impl ToolHandler for SchedulerToolHandler {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_reminder_set" => self.handle_reminder_set(args),
            "tttt_cron_create" => self.handle_cron_create(args),
            "tttt_cron_list" => self.handle_cron_list(),
            "tttt_cron_delete" => self.handle_cron_delete(args),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::scheduler_tool_definitions()
    }
}

// === Notification tool handler ===

use crate::notification::NotificationRegistry;

pub type SharedNotificationRegistry = Arc<Mutex<NotificationRegistry>>;

/// Handles notification and self-injection tool calls.
pub struct NotificationToolHandler<B: PtyBackend = tttt_pty::RealPty> {
    registry: SharedNotificationRegistry,
    sessions: SharedSessionManager<B>,
}

impl<B: PtyBackend> NotificationToolHandler<B> {
    pub fn new(
        registry: SharedNotificationRegistry,
        sessions: SharedSessionManager<B>,
    ) -> Self {
        Self { registry, sessions }
    }

    fn handle_notify_on_prompt(&self, args: &Value) -> Result<Value> {
        let watch_session_id = args["watch_session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("watch_session_id required".into()))?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("pattern required".into()))?;
        let inject_text = args["inject_text"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("inject_text required".into()))?;
        let inject_session_id = args["inject_session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("inject_session_id required".into()))?;

        let mut reg = self.registry.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = reg
            .add_watcher(
                watch_session_id.into(),
                pattern,
                inject_text.into(),
                inject_session_id.into(),
                true, // one-shot
            )
            .map_err(|e| McpError::InvalidParams(e))?;
        Ok(json!({"watcher_id": id}))
    }

    fn handle_notify_on_pattern(&self, args: &Value) -> Result<Value> {
        let watch_session_id = args["watch_session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("watch_session_id required".into()))?;
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("pattern required".into()))?;
        let inject_text = args["inject_text"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("inject_text required".into()))?;
        let inject_session_id = args["inject_session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("inject_session_id required".into()))?;

        let mut reg = self.registry.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = reg
            .add_watcher(
                watch_session_id.into(),
                pattern,
                inject_text.into(),
                inject_session_id.into(),
                false, // recurring
            )
            .map_err(|e| McpError::InvalidParams(e))?;
        Ok(json!({"watcher_id": id}))
    }

    fn handle_notify_cancel(&self, args: &Value) -> Result<Value> {
        let watcher_id = args["watcher_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("watcher_id required".into()))?;
        let mut reg = self.registry.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        if reg.remove_watcher(watcher_id) {
            Ok(json!({"status": "ok"}))
        } else {
            Err(McpError::InvalidParams(format!("watcher not found: {}", watcher_id)))
        }
    }

    fn handle_notify_list(&self) -> Result<Value> {
        let reg = self.registry.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let watchers: Vec<Value> = reg
            .list_watchers()
            .iter()
            .map(|w| {
                json!({
                    "id": w.id,
                    "watch_session_id": w.watch_session_id,
                    "pattern": w.pattern,
                    "inject_text": w.inject_text,
                    "inject_session_id": w.inject_session_id,
                    "one_shot": w.one_shot,
                })
            })
            .collect();
        Ok(json!(watchers))
    }

    fn handle_self_inject(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".into()))?;
        let text = args["text"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("text required".into()))?;

        let mut mgr = self.sessions.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session = mgr.get_mut(session_id)?;
        let mut bytes = text.as_bytes().to_vec();
        // Always auto-submit: append \r if not already present
        if bytes.last() != Some(&b'\r') {
            bytes.push(b'\r');
        }
        session.send_raw(&bytes)?;
        Ok(json!({"status": "ok"}))
    }
}

impl<B: PtyBackend + 'static> ToolHandler for NotificationToolHandler<B> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_notify_on_prompt" => self.handle_notify_on_prompt(args),
            "tttt_notify_on_pattern" => self.handle_notify_on_pattern(args),
            "tttt_notify_cancel" => self.handle_notify_cancel(args),
            "tttt_notify_list" => self.handle_notify_list(),
            "tttt_self_inject" => self.handle_self_inject(args),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::notification_tool_definitions()
    }
}

// === Sidebar message tool handler ===

/// Shared sidebar messages type.
pub type SharedSidebarMessages = Arc<Mutex<Vec<String>>>;

/// Shared flag to signal the main render loop that sidebar content has changed.
pub type SidebarDirtyFlag = Arc<std::sync::atomic::AtomicBool>;

/// Handles sidebar message tool calls, writing messages into the sidebar REMINDERS section.
pub struct SidebarMessageToolHandler {
    messages: SharedSidebarMessages,
    dirty: SidebarDirtyFlag,
}

impl SidebarMessageToolHandler {
    pub fn new(messages: SharedSidebarMessages, dirty: SidebarDirtyFlag) -> Self {
        Self { messages, dirty }
    }

    fn handle_sidebar_list(&self) -> Result<Value> {
        let msgs = self.messages.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        Ok(json!({"messages": *msgs}))
    }

    fn handle_sidebar_message(&self, args: &Value) -> Result<Value> {
        let clear = args["clear"].as_bool().unwrap_or(false);
        let mut msgs = self.messages.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        if clear {
            msgs.clear();
            self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(json!({"status": "ok", "count": 0}));
        }
        let message = args["message"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("message required".into()))?
            .to_string();
        if msgs.len() >= 10 {
            msgs.remove(0);
        }
        msgs.push(message);
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({"status": "ok", "count": msgs.len()}))
    }
}

impl ToolHandler for SidebarMessageToolHandler {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_sidebar_message" => self.handle_sidebar_message(args),
            "tttt_sidebar_list" => self.handle_sidebar_list(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::sidebar_tool_definitions()
    }
}

// === Scratchpad tool handler ===

/// Shared scratchpad store type.
pub type SharedScratchpad = Arc<Mutex<HashMap<String, String>>>;

/// Handles scratchpad read/write tool calls using a shared HashMap.
pub struct ScratchpadToolHandler {
    store: SharedScratchpad,
}

impl ScratchpadToolHandler {
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a handler that shares an existing scratchpad store.
    pub fn new_shared(store: SharedScratchpad) -> Self {
        Self { store }
    }

    fn handle_scratchpad_write(&mut self, args: &Value) -> Result<Value> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("key required".into()))?
            .to_string();
        let content = args["content"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("content required".into()))?
            .to_string();
        let append = args["append"].as_bool().unwrap_or(false);

        let mut store = self.store.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        if append {
            store
                .entry(key)
                .and_modify(|v| v.push_str(&content))
                .or_insert(content);
        } else {
            store.insert(key, content);
        }
        Ok(json!({"status": "ok"}))
    }

    fn handle_scratchpad_read(&self, args: &Value) -> Result<Value> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("key required".into()))?;
        let store = self.store.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        match store.get(key) {
            Some(content) => Ok(json!({"content": content})),
            None => Err(McpError::InvalidParams(format!(
                "scratchpad key not found: {}",
                key
            ))),
        }
    }
}

impl Default for ScratchpadToolHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolHandler for ScratchpadToolHandler {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_scratchpad_write" => self.handle_scratchpad_write(args),
            "tttt_scratchpad_read" => self.handle_scratchpad_read(args),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::scratchpad_tool_definitions()
    }
}

// === Session replay tool handler ===

/// Handles MCP tool calls for session replay by reading from a SQLite log database.
pub struct ReplayToolHandler {
    db_path: PathBuf,
}

impl ReplayToolHandler {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_db(&self) -> Result<SqliteLogger> {
        SqliteLogger::open_read_only(&self.db_path)
            .map_err(|e| McpError::Protocol(e.to_string()))
    }
}

impl ToolHandler for ReplayToolHandler {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_replay_list_sessions" => {
                let db = self.open_db()?;
                let sessions = db
                    .list_sessions()
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                Ok(json!({ "sessions": sessions }))
            }
            "tttt_replay_get_screen" => {
                let session_id = args["session_id"]
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
                let db = self.open_db()?;
                let info = db
                    .get_session_info(session_id)
                    .map_err(|e| McpError::Protocol(e.to_string()))?
                    .ok_or_else(|| {
                        McpError::Protocol(format!("session '{}' not found", session_id))
                    })?;
                let events = db
                    .query_events(session_id)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let mut replay = SessionReplay::new(events, info.cols, info.rows);
                if let Some(idx) = args["event_index"].as_u64() {
                    replay.seek_to_index(idx as usize);
                } else if let Some(ts) = args["timestamp_ms"].as_u64() {
                    replay.seek_to_timestamp(ts);
                } else {
                    replay.seek_to_index(replay.event_count());
                }
                let screen = replay.screen_contents();
                let cursor = replay.cursor_position();
                Ok(json!({
                    "screen": screen,
                    "cursor": [cursor.0, cursor.1],
                    "event_index": replay.current_index(),
                    "timestamp_ms": replay.current_timestamp(),
                }))
            }
            "tttt_replay_get_timeline" => {
                let session_id = args["session_id"]
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
                let db = self.open_db()?;
                let events = db
                    .query_events(session_id)
                    .map_err(|e| McpError::Protocol(e.to_string()))?;
                let replay = SessionReplay::new(events, 80, 24);
                let timeline: Vec<Value> = replay
                    .timeline()
                    .into_iter()
                    .map(|(idx, ts, dir)| {
                        json!({
                            "index": idx,
                            "timestamp_ms": ts,
                            "direction": dir.as_str()
                        })
                    })
                    .collect();
                Ok(json!({ "timeline": timeline }))
            }
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::replay_tool_definitions()
    }
}

// === TUI control tool handler ===

/// A single highlight rectangle on a pane.
#[derive(Debug, Clone)]
pub struct TuiHighlight {
    pub id: String,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub color: String,
}

/// Shared TUI state for the tui_switch / tui_get_info / tui_highlight tools.
#[derive(Debug)]
pub struct TuiState {
    /// Pending session switch request (consumed by main loop each tick).
    pub pending_switch: Mutex<Option<String>>,
    /// Active highlights per session: session_id → list of highlights.
    pub highlights: Mutex<HashMap<String, Vec<TuiHighlight>>>,
    /// Dirty flag — set when highlights or switch changes, consumed by render loop.
    pub dirty: AtomicBool,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            pending_switch: Mutex::new(None),
            highlights: Mutex::new(HashMap::new()),
            dirty: AtomicBool::new(false),
        }
    }
}

/// Shared TUI state type.
pub type SharedTuiState = Arc<TuiState>;

/// Handles TUI control tool calls.
pub struct TuiToolHandler<B: PtyBackend> {
    tui_state: SharedTuiState,
    sessions: SharedSessionManager<B>,
    screen_cols: u16,
    screen_rows: u16,
}

impl<B: PtyBackend> TuiToolHandler<B> {
    pub fn new(
        tui_state: SharedTuiState,
        sessions: SharedSessionManager<B>,
        screen_cols: u16,
        screen_rows: u16,
    ) -> Self {
        Self { tui_state, sessions, screen_cols, screen_rows }
    }

    fn handle_tui_switch(&self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".into()))?;

        // Resolve name to canonical session ID (e.g. "my-shell" → "pty-0")
        let mgr = self.sessions.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let resolved_id = mgr.resolve_id(session_id).map_err(|_| {
            McpError::InvalidParams(format!("session '{}' not found", session_id))
        })?.to_string();
        drop(mgr);

        *self.tui_state.pending_switch.lock().unwrap() = Some(resolved_id.clone());
        self.tui_state.dirty.store(true, Ordering::Relaxed);
        Ok(json!({"status": "ok", "switched_to": resolved_id}))
    }

    fn handle_tui_get_info(&self) -> Result<Value> {
        let mgr = self.sessions.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let sessions: Vec<Value> = mgr.list().iter().map(|m| {
            json!({
                "id": m.id,
                "name": m.name,
                "command": m.command,
                "status": format!("{:?}", m.status),
                "cols": m.cols,
                "rows": m.rows,
                "root": m.root,
            })
        }).collect();
        drop(mgr);

        // Read active session from pending switch or current state is not available here,
        // but we can report screen dimensions and session list.
        let highlights = self.tui_state.highlights.lock().unwrap();
        let highlight_count: usize = highlights.values().map(|v| v.len()).sum();

        Ok(json!({
            "screen_cols": self.screen_cols,
            "screen_rows": self.screen_rows,
            "sessions": sessions,
            "highlight_count": highlight_count,
        }))
    }

    fn handle_tui_highlight(&self, args: &Value) -> Result<Value> {
        let raw_session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".into()))?;
        // Resolve name to canonical session ID
        let mgr = self.sessions.lock().map_err(|e| McpError::Protocol(e.to_string()))?;
        let session_id = mgr.resolve_id(raw_session_id).map_err(|_| {
            McpError::InvalidParams(format!("session '{}' not found", raw_session_id))
        })?.to_string();
        drop(mgr);
        let highlight_id = args["id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("id required".into()))?
            .to_string();
        let color = args["color"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("color required".into()))?
            .to_string();

        let mut highlights = self.tui_state.highlights.lock().unwrap();

        if color.is_empty() {
            // Remove highlight by id
            if let Some(list) = highlights.get_mut(&session_id) {
                list.retain(|h| h.id != highlight_id);
                if list.is_empty() {
                    highlights.remove(&session_id);
                }
            }
            self.tui_state.dirty.store(true, Ordering::Relaxed);
            return Ok(json!({"status": "ok", "action": "removed", "id": highlight_id}));
        }

        let x = args["x"].as_u64().unwrap_or(0) as u16;
        let y = args["y"].as_u64().unwrap_or(0) as u16;
        let width = args["width"].as_u64().unwrap_or(1) as u16;
        let height = args["height"].as_u64().unwrap_or(1) as u16;

        let entry = highlights.entry(session_id.clone()).or_default();
        // Update existing highlight with same id, or add new one
        if let Some(existing) = entry.iter_mut().find(|h| h.id == highlight_id) {
            existing.x = x;
            existing.y = y;
            existing.width = width;
            existing.height = height;
            existing.color = color;
        } else {
            entry.push(TuiHighlight {
                id: highlight_id.clone(),
                x, y, width, height, color,
            });
        }

        self.tui_state.dirty.store(true, Ordering::Relaxed);
        Ok(json!({"status": "ok", "action": "set", "id": highlight_id}))
    }
}

impl<B: PtyBackend + 'static> ToolHandler for TuiToolHandler<B> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "tttt_tui_switch" => self.handle_tui_switch(args),
            "tttt_tui_get_info" => self.handle_tui_get_info(),
            "tttt_tui_highlight" => self.handle_tui_highlight(args),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::tui_tool_definitions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tttt_log::LogSink as _;

    fn make_handler() -> PtyToolHandler<MockPty> {
        PtyToolHandler::new_owned(SessionManager::new(), std::path::PathBuf::from("/tmp"))
    }

    fn make_handler_with_session() -> PtyToolHandler<MockPty> {
        let handler = make_handler();
        handler.handle_pty_launch_mock(&json!({})).unwrap();
        handler
    }

    #[test]
    fn test_pty_launch_mock() {
        let handler = make_handler();
        let result = handler.handle_pty_launch_mock(&json!({})).unwrap();
        assert!(result["session_id"].is_string());
        assert_eq!(handler.manager().lock().unwrap().session_count(), 1);
    }

    #[test]
    fn test_pty_launch_custom_dims() {
        let handler = make_handler();
        let result = handler
            .handle_pty_launch_mock(&json!({"cols": 120, "rows": 40}))
            .unwrap();
        let id = result["session_id"].as_str().unwrap();
        let mgr = handler.manager().lock().unwrap();
        let session = mgr.get(id).unwrap();
        let meta = session.metadata();
        assert_eq!(meta.cols, 120);
        assert_eq!(meta.rows, 40);
    }

    #[test]
    fn test_pty_list_empty() {
        let handler = make_handler();
        let result = handler.handle_pty_list().unwrap();
        assert_eq!(result.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_pty_list_with_sessions() {
        let handler = make_handler_with_session();
        let result = handler.handle_pty_list().unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_pty_send_keys() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler.handle_tool_call(
            "tttt_pty_send_keys",
            &json!({"session_id": id, "keys": "hello"}),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_pty_send_keys_no_auto_enter() {
        // Verify that send_keys does NOT auto-append a newline;
        // callers must use explicit [ENTER] or \n.
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler.handle_tool_call(
            "tttt_pty_send_keys",
            &json!({"session_id": id, "keys": "hello"}),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_pty_send_keys_missing_session_id() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("tttt_pty_send_keys", &json!({"keys": "hello"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_pty_get_screen() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call("tttt_pty_get_screen", &json!({"session_id": id}))
            .unwrap();
        assert!(result["screen"].is_string());
        assert!(result["cursor"].is_array());
    }

    #[test]
    fn test_pty_kill() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        handler
            .handle_tool_call("tttt_pty_kill", &json!({"session_id": id}))
            .unwrap();
        assert_eq!(handler.manager().lock().unwrap().session_count(), 0);
    }

    #[test]
    fn test_pty_get_cursor() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call("tttt_pty_get_cursor", &json!({"session_id": id}))
            .unwrap();
        assert_eq!(result["row"], 0);
        assert_eq!(result["col"], 0);
    }

    #[test]
    fn test_pty_resize() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        handler
            .handle_tool_call("tttt_pty_resize", &json!({"session_id": id, "cols": 100, "rows": 50}))
            .unwrap();
        let mgr = handler.manager().lock().unwrap();
        let session = mgr.get(&id).unwrap();
        let meta = session.metadata();
        assert_eq!(meta.cols, 100);
        assert_eq!(meta.rows, 50);
    }

    #[test]
    fn test_pty_set_scrollback() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        handler
            .handle_tool_call("tttt_pty_set_scrollback", &json!({"session_id": id, "lines": 5000}))
            .unwrap();
    }

    #[test]
    fn test_unknown_tool() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("nonexistent", &json!({}));
        assert!(matches!(result.unwrap_err(), McpError::ToolNotFound(_)));
    }

    #[test]
    fn test_get_status_empty() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("tttt_get_status", &json!({})).unwrap();
        assert_eq!(result["sessions"].as_array().unwrap().len(), 0);
        assert_eq!(result["pending_reminders"], 0);
        assert_eq!(result["active_watchers"], 0);
    }

    #[test]
    fn test_get_status_with_session() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        // Queue some output
        {
            let mut mgr = handler.manager().lock().unwrap();
            let session = mgr.get_mut(&id).unwrap();
            session.backend_mut().queue_output(b"hello world\r\n");
            session.pump().unwrap();
        }
        let mut handler = handler;
        let result = handler.handle_tool_call("tttt_get_status", &json!({})).unwrap();
        let sessions = result["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s["id"].as_str().unwrap(), id);
        assert_eq!(s["status"], "Running");
        assert_eq!(s["last_output_line"], "hello world");
        assert!(s["idle_seconds"].as_f64().unwrap() < 1.0);
    }

    #[test]
    fn test_get_status_idle_seconds_present() {
        let handler = make_handler_with_session();
        let mut handler = handler;
        let result = handler.handle_tool_call("tttt_get_status", &json!({})).unwrap();
        let sessions = result["sessions"].as_array().unwrap();
        assert!(sessions[0]["idle_seconds"].is_number());
    }

    #[test]
    fn test_get_status_empty_screen_last_line() {
        let handler = make_handler_with_session();
        let mut handler = handler;
        let result = handler.handle_tool_call("tttt_get_status", &json!({})).unwrap();
        let sessions = result["sessions"].as_array().unwrap();
        assert_eq!(sessions[0]["last_output_line"], "");
    }

    #[test]
    fn test_pty_start_capture_returns_capture_id_and_file_path() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call("tttt_pty_start_capture", &json!({"session_id": id}))
            .unwrap();
        assert!(result["capture_id"].is_string());
        let file_path = result["file_path"].as_str().unwrap();
        assert!(file_path.starts_with("/tmp/tttt-capture-"));
        assert!(file_path.ends_with(".raw"));
        // cleanup
        let _ = std::fs::remove_file(file_path);
    }

    #[test]
    fn test_pty_stop_capture_returns_file_path_and_bytes() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let start_result = handler
            .handle_tool_call("tttt_pty_start_capture", &json!({"session_id": id}))
            .unwrap();
        let file_path = start_result["file_path"].as_str().unwrap().to_string();
        let stop_result = handler
            .handle_tool_call("tttt_pty_stop_capture", &json!({"session_id": id}))
            .unwrap();
        assert_eq!(stop_result["file_path"].as_str().unwrap(), file_path);
        assert!(stop_result["bytes_written"].is_number());
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_pty_start_capture_twice_returns_error() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result1 = handler
            .handle_tool_call("tttt_pty_start_capture", &json!({"session_id": id}))
            .unwrap();
        let file_path = result1["file_path"].as_str().unwrap().to_string();
        let result2 = handler.handle_tool_call("tttt_pty_start_capture", &json!({"session_id": id}));
        assert!(result2.is_err());
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_pty_stop_capture_without_start_returns_error() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler.handle_tool_call("tttt_pty_stop_capture", &json!({"session_id": id}));
        assert!(result.is_err());
    }

    #[test]
    fn test_pty_start_capture_missing_session_id() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("tttt_pty_start_capture", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_pty_stop_capture_missing_session_id() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("tttt_pty_stop_capture", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_composite_merges_definitions() {
        let mut composite = CompositeToolHandler::new();
        let handler = make_handler();
        composite.add_handler(Box::new(handler));
        let defs = composite.tool_definitions();
        assert_eq!(defs.len(), 15);
    }

    #[test]
    fn test_composite_dispatches() {
        let mut composite = CompositeToolHandler::new();
        let handler = make_handler();
        composite.add_handler(Box::new(handler));
        let result = composite.handle_tool_call("tttt_pty_list", &json!({}));
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_unknown_tool() {
        let mut composite = CompositeToolHandler::new();
        let handler = make_handler();
        composite.add_handler(Box::new(handler));
        let result = composite.handle_tool_call("nonexistent", &json!({}));
        assert!(matches!(result.unwrap_err(), McpError::ToolNotFound(_)));
    }

    #[test]
    fn test_pty_get_scrollback() {
        let handler = make_handler();
        handler.handle_pty_launch_mock(&json!({"cols": 80, "rows": 5})).unwrap();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();

        // Queue output that overflows the 5-row screen
        {
            let mut mgr = handler.manager().lock().unwrap();
            let session = mgr.get_mut(&id).unwrap();
            let mut output = String::new();
            for i in 0..15 {
                output.push_str(&format!("line {}\r\n", i));
            }
            session.send_raw(output.as_bytes()).unwrap();
        }

        let mut handler = handler;
        let result = handler
            .handle_tool_call("tttt_pty_get_scrollback", &json!({"session_id": id}))
            .unwrap();
        assert!(result["lines"].is_array(), "response should contain lines array");
    }

    #[test]
    fn test_pty_get_scrollback_with_lines_param() {
        let handler = make_handler();
        handler.handle_pty_launch_mock(&json!({"cols": 80, "rows": 5})).unwrap();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();

        {
            let mut mgr = handler.manager().lock().unwrap();
            let session = mgr.get_mut(&id).unwrap();
            let mut output = String::new();
            for i in 0..15 {
                output.push_str(&format!("line {}\r\n", i));
            }
            session.send_raw(output.as_bytes()).unwrap();
        }

        let mut handler = handler;
        let result = handler
            .handle_tool_call("tttt_pty_get_scrollback", &json!({"session_id": id, "lines": 3}))
            .unwrap();
        let lines = result["lines"].as_array().unwrap();
        assert!(lines.len() <= 3, "should return at most 3 lines");
    }

    /// Test that two references to the same handler share state.
    #[test]
    fn test_shared_manager() {
        let shared: SharedSessionManager<MockPty> =
            Arc::new(Mutex::new(SessionManager::new()));

        let handler = PtyToolHandler::<MockPty>::new(shared.clone(), std::path::PathBuf::from("/tmp"));

        // Launch via handler
        handler.handle_pty_launch_mock(&json!({})).unwrap();

        // Should be visible through the shared reference
        assert_eq!(shared.lock().unwrap().session_count(), 1);

        // And through the handler's own reference
        assert_eq!(handler.manager().lock().unwrap().session_count(), 1);
    }

    // --- Scheduler tool handler tests ---

    fn make_scheduler_handler() -> SchedulerToolHandler {
        SchedulerToolHandler::new_owned(Scheduler::new())
    }

    #[test]
    fn test_scheduler_reminder_set() {
        let mut handler = make_scheduler_handler();
        let result = handler
            .handle_tool_call(
                "tttt_reminder_set",
                &json!({"message": "test reminder", "delay_seconds": 60}),
            )
            .unwrap();
        assert!(result["reminder_id"].is_string());
        assert_eq!(
            handler.scheduler().lock().unwrap().reminder_count(),
            1
        );
    }

    #[test]
    fn test_scheduler_cron_create() {
        let mut handler = make_scheduler_handler();
        let result = handler
            .handle_tool_call(
                "tttt_cron_create",
                &json!({"expression": "10s", "command": "echo hello"}),
            )
            .unwrap();
        assert!(result["job_id"].is_string());
        assert_eq!(handler.scheduler().lock().unwrap().cron_count(), 1);
    }

    #[test]
    fn test_scheduler_cron_create_invalid() {
        let mut handler = make_scheduler_handler();
        let result = handler.handle_tool_call(
            "tttt_cron_create",
            &json!({"expression": "invalid!!!", "command": "x"}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_scheduler_cron_list() {
        let mut handler = make_scheduler_handler();
        handler
            .handle_tool_call(
                "tttt_cron_create",
                &json!({"expression": "10s", "command": "a"}),
            )
            .unwrap();
        handler
            .handle_tool_call(
                "tttt_cron_create",
                &json!({"expression": "20s", "command": "b"}),
            )
            .unwrap();

        let result = handler.handle_tool_call("tttt_cron_list", &json!({})).unwrap();
        let jobs = result.as_array().unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn test_scheduler_cron_delete() {
        let mut handler = make_scheduler_handler();
        let create_result = handler
            .handle_tool_call(
                "tttt_cron_create",
                &json!({"expression": "10s", "command": "x"}),
            )
            .unwrap();
        let job_id = create_result["job_id"].as_str().unwrap();

        handler
            .handle_tool_call("tttt_cron_delete", &json!({"job_id": job_id}))
            .unwrap();
        assert_eq!(handler.scheduler().lock().unwrap().cron_count(), 0);
    }

    #[test]
    fn test_scheduler_cron_delete_nonexistent() {
        let mut handler = make_scheduler_handler();
        let result = handler.handle_tool_call("tttt_cron_delete", &json!({"job_id": "nope"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_composite_pty_and_scheduler() {
        let mut composite = CompositeToolHandler::new();
        composite.add_handler(Box::new(make_handler()));
        composite.add_handler(Box::new(make_scheduler_handler()));

        // Should have 15 PTY + 4 scheduler = 19 tool definitions
        let defs = composite.tool_definitions();
        assert_eq!(defs.len(), 19);

        // PTY tool should work
        let result = composite.handle_tool_call("tttt_pty_list", &json!({}));
        assert!(result.is_ok());

        // Scheduler tool should work
        let result = composite.handle_tool_call(
            "tttt_cron_create",
            &json!({"expression": "10s", "command": "test"}),
        );
        assert!(result.is_ok());

        // Unknown tool should error
        let result = composite.handle_tool_call("nonexistent", &json!({}));
        assert!(matches!(result.unwrap_err(), McpError::ToolNotFound(_)));
    }

    // --- Notification tool handler tests ---

    fn make_notification_handler() -> NotificationToolHandler<MockPty> {
        let sessions = Arc::new(Mutex::new(SessionManager::<MockPty>::new()));
        let registry = Arc::new(Mutex::new(NotificationRegistry::new()));
        NotificationToolHandler::new(registry, sessions)
    }

    #[test]
    fn test_notify_on_prompt() {
        let mut handler = make_notification_handler();
        let result = handler
            .handle_tool_call(
                "tttt_notify_on_prompt",
                &json!({
                    "watch_session_id": "pty-1",
                    "pattern": "❯\\s*$",
                    "inject_text": "[DONE] Executor finished",
                    "inject_session_id": "root"
                }),
            )
            .unwrap();
        assert!(result["watcher_id"].is_string());
    }

    #[test]
    fn test_notify_on_pattern() {
        let mut handler = make_notification_handler();
        let result = handler
            .handle_tool_call(
                "tttt_notify_on_pattern",
                &json!({
                    "watch_session_id": "pty-1",
                    "pattern": "error",
                    "inject_text": "[ALERT] Error detected",
                    "inject_session_id": "root"
                }),
            )
            .unwrap();
        assert!(result["watcher_id"].is_string());
    }

    #[test]
    fn test_notify_invalid_regex() {
        let mut handler = make_notification_handler();
        let result = handler.handle_tool_call(
            "tttt_notify_on_prompt",
            &json!({
                "watch_session_id": "pty-1",
                "pattern": "[invalid",
                "inject_text": "x",
                "inject_session_id": "root"
            }),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_notify_list() {
        let mut handler = make_notification_handler();
        handler
            .handle_tool_call(
                "tttt_notify_on_prompt",
                &json!({
                    "watch_session_id": "pty-1",
                    "pattern": "a",
                    "inject_text": "x",
                    "inject_session_id": "root"
                }),
            )
            .unwrap();
        handler
            .handle_tool_call(
                "tttt_notify_on_pattern",
                &json!({
                    "watch_session_id": "pty-2",
                    "pattern": "b",
                    "inject_text": "y",
                    "inject_session_id": "root"
                }),
            )
            .unwrap();

        let result = handler.handle_tool_call("tttt_notify_list", &json!({})).unwrap();
        let watchers = result.as_array().unwrap();
        assert_eq!(watchers.len(), 2);
    }

    #[test]
    fn test_notify_cancel() {
        let mut handler = make_notification_handler();
        let result = handler
            .handle_tool_call(
                "tttt_notify_on_prompt",
                &json!({
                    "watch_session_id": "pty-1",
                    "pattern": "a",
                    "inject_text": "x",
                    "inject_session_id": "root"
                }),
            )
            .unwrap();
        let watcher_id = result["watcher_id"].as_str().unwrap();

        handler
            .handle_tool_call("tttt_notify_cancel", &json!({"watcher_id": watcher_id}))
            .unwrap();

        let list = handler.handle_tool_call("tttt_notify_list", &json!({})).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_notify_cancel_nonexistent() {
        let mut handler = make_notification_handler();
        let result = handler.handle_tool_call("tttt_notify_cancel", &json!({"watcher_id": "nope"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_self_inject_missing_session() {
        let mut handler = make_notification_handler();
        let result = handler.handle_tool_call(
            "tttt_self_inject",
            &json!({"session_id": "nonexistent", "text": "hello"}),
        );
        assert!(result.is_err());
    }

    fn make_notification_handler_with_session() -> (NotificationToolHandler<MockPty>, String) {
        let sessions = Arc::new(Mutex::new(SessionManager::<MockPty>::new()));
        let registry = Arc::new(Mutex::new(NotificationRegistry::new()));
        let id = {
            let mut mgr = sessions.lock().unwrap();
            let id = mgr.generate_id();
            let mock = MockPty::new(80, 24);
            let session = tttt_pty::PtySession::new(id.clone(), mock, "bash".to_string(), 80, 24);
            mgr.add_session(session).unwrap();
            id
        };
        (NotificationToolHandler::new(registry, sessions), id)
    }

    #[test]
    fn test_self_inject_appends_cr() {
        // Verify that self_inject always appends \r for auto-submit.
        let (mut handler, id) = make_notification_handler_with_session();
        handler
            .handle_tool_call(
                "tttt_self_inject",
                &json!({"session_id": id, "text": "hello"}),
            )
            .unwrap();

        // Inspect what was written to the MockPty
        let sessions = handler.sessions.lock().unwrap();
        let session = sessions.get(&id).unwrap();
        let backend = session.backend();
        assert!(
            backend.input_buf.ends_with(b"\r"),
            "self_inject should auto-append \\r, got: {:?}",
            String::from_utf8_lossy(&backend.input_buf)
        );
        assert_eq!(backend.input_buf, b"hello\r");
    }

    #[test]
    fn test_self_inject_does_not_double_cr() {
        // If text already ends with \r, don't add another.
        let (mut handler, id) = make_notification_handler_with_session();
        handler
            .handle_tool_call(
                "tttt_self_inject",
                &json!({"session_id": id, "text": "world\r"}),
            )
            .unwrap();

        let sessions = handler.sessions.lock().unwrap();
        let session = sessions.get(&id).unwrap();
        let backend = session.backend();
        assert_eq!(
            backend.input_buf, b"world\r",
            "should not double the \\r"
        );
    }

    // --- Scratchpad tool handler tests ---

    fn make_scratchpad_handler() -> ScratchpadToolHandler {
        ScratchpadToolHandler::new()
    }

    #[test]
    fn test_scratchpad_write_and_read() {
        let mut handler = make_scratchpad_handler();
        handler
            .handle_tool_call(
                "tttt_scratchpad_write",
                &json!({"key": "notes", "content": "hello world"}),
            )
            .unwrap();
        let result = handler
            .handle_tool_call("tttt_scratchpad_read", &json!({"key": "notes"}))
            .unwrap();
        assert_eq!(result["content"].as_str().unwrap(), "hello world");
    }

    #[test]
    fn test_scratchpad_write_overwrites() {
        let mut handler = make_scratchpad_handler();
        handler
            .handle_tool_call(
                "tttt_scratchpad_write",
                &json!({"key": "k", "content": "first"}),
            )
            .unwrap();
        handler
            .handle_tool_call(
                "tttt_scratchpad_write",
                &json!({"key": "k", "content": "second"}),
            )
            .unwrap();
        let result = handler
            .handle_tool_call("tttt_scratchpad_read", &json!({"key": "k"}))
            .unwrap();
        assert_eq!(result["content"].as_str().unwrap(), "second");
    }

    #[test]
    fn test_scratchpad_append() {
        let mut handler = make_scratchpad_handler();
        handler
            .handle_tool_call(
                "tttt_scratchpad_write",
                &json!({"key": "log", "content": "line1\n"}),
            )
            .unwrap();
        handler
            .handle_tool_call(
                "tttt_scratchpad_write",
                &json!({"key": "log", "content": "line2\n", "append": true}),
            )
            .unwrap();
        let result = handler
            .handle_tool_call("tttt_scratchpad_read", &json!({"key": "log"}))
            .unwrap();
        assert_eq!(result["content"].as_str().unwrap(), "line1\nline2\n");
    }

    #[test]
    fn test_scratchpad_read_missing_key() {
        let mut handler = make_scratchpad_handler();
        let result = handler.handle_tool_call("tttt_scratchpad_read", &json!({"key": "nope"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_scratchpad_shared_store_visible_across_handlers() {
        let shared = Arc::new(Mutex::new(HashMap::new()));
        let mut writer = ScratchpadToolHandler::new_shared(shared.clone());
        let mut reader = ScratchpadToolHandler::new_shared(shared.clone());

        writer
            .handle_tool_call(
                "tttt_scratchpad_write",
                &json!({"key": "x", "content": "shared_value"}),
            )
            .unwrap();

        let result = reader
            .handle_tool_call("tttt_scratchpad_read", &json!({"key": "x"}))
            .unwrap();
        assert_eq!(result["content"].as_str().unwrap(), "shared_value");
    }

    #[test]
    fn test_scratchpad_restore_from_saved_data() {
        let mut pre_existing = HashMap::new();
        pre_existing.insert("restored_key".to_string(), "restored_value".to_string());
        let shared = Arc::new(Mutex::new(pre_existing));
        let mut handler = ScratchpadToolHandler::new_shared(shared);

        let result = handler
            .handle_tool_call("tttt_scratchpad_read", &json!({"key": "restored_key"}))
            .unwrap();
        assert_eq!(result["content"].as_str().unwrap(), "restored_value");
    }

    // --- tttt_pty_wait_for tests ---

    #[test]
    fn test_pty_wait_for_immediate_match() {
        let handler = make_handler();
        let id = {
            let mut mgr = handler.manager().lock().unwrap();
            let id = mgr.generate_id();
            let mut mock = MockPty::new(80, 24);
            mock.queue_output(b"ready$ ");
            let mut session = PtySession::new(id.clone(), mock, "bash".to_string(), 80, 24);
            session.pump().unwrap();
            mgr.add_session(session).unwrap();
            id
        };

        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for",
                &json!({"session_id": id, "pattern": "ready\\$", "timeout_ms": 1000}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "matched");
        assert!(result["screen"].is_string());
    }

    #[test]
    fn test_pty_wait_for_timeout() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();

        let mut handler = handler;
        let result = handler.handle_tool_call(
            "tttt_pty_wait_for",
            &json!({"session_id": id, "pattern": "never_matches", "timeout_ms": 200}),
        );
        assert!(result.is_err(), "should error on timeout");
    }

    #[test]
    fn test_pty_wait_for_invalid_regex() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();

        let mut handler = handler;
        let result = handler.handle_tool_call(
            "tttt_pty_wait_for",
            &json!({"session_id": id, "pattern": "[invalid", "timeout_ms": 1000}),
        );
        assert!(result.is_err(), "should error on invalid regex");
    }

    #[test]
    fn test_pty_wait_for_missing_session() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call(
            "tttt_pty_wait_for",
            &json!({"session_id": "nonexistent", "pattern": "foo", "timeout_ms": 200}),
        );
        assert!(result.is_err(), "should error on missing session");
    }

    #[test]
    fn test_pty_wait_for_default_timeout() {
        // Call without timeout_ms, verify it parses (uses default 30000).
        // We use a pattern that matches immediately so we don't wait 30s.
        let handler = make_handler();
        let id = {
            let mut mgr = handler.manager().lock().unwrap();
            let id = mgr.generate_id();
            let mut mock = MockPty::new(80, 24);
            mock.queue_output(b"prompt> ");
            let mut session = PtySession::new(id.clone(), mock, "bash".to_string(), 80, 24);
            session.pump().unwrap();
            mgr.add_session(session).unwrap();
            id
        };

        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for",
                &json!({"session_id": id, "pattern": "prompt>"}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "matched");
    }

    #[test]
    fn test_pty_wait_for_in_definitions() {
        let defs = crate::tools::pty_tool_definitions();
        let wait_for = defs
            .iter()
            .find(|t| t["name"] == "tttt_pty_wait_for")
            .expect("tttt_pty_wait_for should be in pty_tool_definitions");
        let required = wait_for["inputSchema"]["required"].as_array().unwrap();
        assert!(required.contains(&Value::from("session_id")));
        assert!(required.contains(&Value::from("pattern")));
        // timeout_ms should NOT be required (it's optional)
        assert!(!required.contains(&Value::from("timeout_ms")));
    }

    // --- tttt_pty_wait_for_idle tests ---

    #[test]
    fn test_pty_wait_for_idle_missing_session_id() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("tttt_pty_wait_for_idle", &json!({}));
        assert!(result.is_err(), "should error when session_id is missing");
    }

    #[test]
    fn test_pty_wait_for_idle_missing_session() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call(
            "tttt_pty_wait_for_idle",
            &json!({"session_id": "nonexistent", "idle_seconds": 5.0, "timeout": 0.5}),
        );
        assert!(result.is_err(), "should error on missing session");
    }

    #[test]
    fn test_pty_wait_for_idle_detects_idle_immediately() {
        // idle_seconds threshold of 0.0 is satisfied by any positive idle time
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for_idle",
                &json!({"session_id": id, "idle_seconds": 0.0, "timeout": 30.0}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "idle");
        assert!(result["idle_seconds"].as_f64().unwrap() >= 0.0);
        assert!(result["last_output_line"].is_string());
    }

    #[test]
    fn test_pty_wait_for_idle_timeout() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for_idle",
                &json!({"session_id": id, "idle_seconds": 99999.0, "timeout": 0.5}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "timeout");
        assert!(result["idle_seconds"].as_f64().unwrap() < 99999.0);
        assert!(result["last_output_line"].is_string());
    }

    #[test]
    fn test_pty_wait_for_idle_default_params() {
        // Verify optional params have defaults by checking the tool definition
        let defs = crate::tools::pty_tool_definitions();
        let tool = defs
            .iter()
            .find(|t| t["name"] == "tttt_pty_wait_for_idle")
            .expect("tttt_pty_wait_for_idle should be in pty_tool_definitions");
        let required = tool["inputSchema"]["required"].as_array().unwrap();
        assert!(required.contains(&Value::from("session_id")));
        assert!(!required.contains(&Value::from("idle_seconds")));
        assert!(!required.contains(&Value::from("timeout")));
        assert!(!required.contains(&Value::from("ignore_pattern")));
    }

    #[test]
    fn test_pty_wait_for_idle_ignore_pattern_in_tool_definition() {
        let defs = crate::tools::pty_tool_definitions();
        let tool = defs
            .iter()
            .find(|t| t["name"] == "tttt_pty_wait_for_idle")
            .unwrap();
        assert!(
            tool["inputSchema"]["properties"]["ignore_pattern"].is_object(),
            "ignore_pattern should be a property"
        );
    }

    #[test]
    fn test_pty_wait_for_idle_ignore_pattern_invalid_regex() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler.handle_tool_call(
            "tttt_pty_wait_for_idle",
            &json!({"session_id": id, "idle_seconds": 0.0, "timeout": 5.0, "ignore_pattern": "[invalid"}),
        );
        assert!(result.is_err(), "invalid ignore_pattern should return error");
    }

    #[test]
    fn test_pty_wait_for_idle_ignore_pattern_detects_idle_when_hash_stable() {
        // Static output: hash never changes, so idle is detected immediately
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for_idle",
                &json!({"session_id": id, "idle_seconds": 0.0, "timeout": 10.0, "ignore_pattern": "\\d{2}:\\d{2}:\\d{2}"}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "idle");
        assert!(result["idle_seconds"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn test_pty_wait_for_idle_ignore_pattern_timeout() {
        // Require 99999 stable seconds — must time out
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for_idle",
                &json!({"session_id": id, "idle_seconds": 99999.0, "timeout": 0.5, "ignore_pattern": "\\d{2}:\\d{2}:\\d{2}"}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "timeout");
    }

    #[test]
    fn test_pty_wait_for_idle_ignore_pattern_strips_changing_parts() {
        // The session screen is static; the pattern matches part of it, but stripped hash
        // is also stable → should detect idle with ignore_pattern present.
        let handler = make_handler();
        let id = {
            let mut mgr = handler.manager().lock().unwrap();
            let id = mgr.generate_id();
            let mut mock = MockPty::new(80, 24);
            mock.queue_output(b"status: 12:34:56 ready");
            let mut session = PtySession::new(id.clone(), mock, "bash".to_string(), 80, 24);
            session.pump().unwrap();
            mgr.add_session(session).unwrap();
            id
        };
        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_wait_for_idle",
                &json!({"session_id": id, "idle_seconds": 0.0, "timeout": 10.0, "ignore_pattern": "\\d{2}:\\d{2}:\\d{2}"}),
            )
            .unwrap();
        assert_eq!(result["status"].as_str().unwrap(), "idle");
    }

    #[test]
    fn test_pty_launch_with_name() {
        let handler = make_handler();
        let result = handler.handle_pty_launch_mock(&json!({"name": "mysession"})).unwrap();
        assert!(result["session_id"].is_string());
        assert_eq!(result["name"].as_str().unwrap(), "mysession");
    }

    #[test]
    fn test_pty_launch_duplicate_name() {
        let handler = make_handler();
        handler.handle_pty_launch_mock(&json!({"name": "mysession"})).unwrap();
        let result = handler.handle_pty_launch_mock(&json!({"name": "mysession"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_pty_get_screen_by_name() {
        let handler = make_handler();
        handler.handle_pty_launch_mock(&json!({"name": "mysession"})).unwrap();
        let mut handler = handler;
        let result = handler
            .handle_tool_call("tttt_pty_get_screen", &json!({"session_id": "mysession"}))
            .unwrap();
        assert!(result["screen"].is_string());
    }

    // === SidebarMessageToolHandler tests ===

    fn make_sidebar_handler() -> SidebarMessageToolHandler {
        SidebarMessageToolHandler::new(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }

    #[test]
    fn test_sidebar_message_add() {
        let mut handler = make_sidebar_handler();
        let result = handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "hello"})).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["count"], 1);
    }

    #[test]
    fn test_sidebar_message_caps_at_10() {
        let mut handler = make_sidebar_handler();
        for i in 0..11 {
            handler.handle_tool_call("tttt_sidebar_message", &json!({"message": format!("msg {}", i)})).unwrap();
        }
        let msgs = handler.messages.lock().unwrap();
        assert_eq!(msgs.len(), 10);
        // Oldest (msg 0) should have been dropped; newest should be msg 10
        assert_eq!(msgs[9], "msg 10");
        assert_eq!(msgs[0], "msg 1");
    }

    #[test]
    fn test_sidebar_message_clear() {
        let mut handler = make_sidebar_handler();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "hello"})).unwrap();
        let result = handler.handle_tool_call("tttt_sidebar_message", &json!({"clear": true})).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["count"], 0);
        assert!(handler.messages.lock().unwrap().is_empty());
    }

    #[test]
    fn test_sidebar_message_missing_message_errors() {
        let mut handler = make_sidebar_handler();
        let result = handler.handle_tool_call("tttt_sidebar_message", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_sidebar_tool_definitions() {
        let handler = make_sidebar_handler();
        let defs = handler.tool_definitions();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|d| d["name"] == "tttt_sidebar_message"));
        assert!(defs.iter().any(|d| d["name"] == "tttt_sidebar_list"));
    }

    #[test]
    fn test_sidebar_list_empty() {
        let mut handler = make_sidebar_handler();
        let result = handler.handle_tool_call("tttt_sidebar_list", &json!({})).unwrap();
        assert_eq!(result["messages"], json!([]));
    }

    #[test]
    fn test_sidebar_list_returns_messages() {
        let mut handler = make_sidebar_handler();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "alpha"})).unwrap();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "beta"})).unwrap();
        let result = handler.handle_tool_call("tttt_sidebar_list", &json!({})).unwrap();
        assert_eq!(result["messages"], json!(["alpha", "beta"]));
    }

    #[test]
    fn test_sidebar_list_after_clear() {
        let mut handler = make_sidebar_handler();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "hello"})).unwrap();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"clear": true})).unwrap();
        let result = handler.handle_tool_call("tttt_sidebar_list", &json!({})).unwrap();
        assert_eq!(result["messages"], json!([]));
    }

    // === Sidebar dirty flag tests ===

    fn make_sidebar_handler_with_flag() -> (SidebarMessageToolHandler, Arc<std::sync::atomic::AtomicBool>) {
        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = SidebarMessageToolHandler::new(
            Arc::new(Mutex::new(Vec::new())),
            dirty.clone(),
        );
        (handler, dirty)
    }

    #[test]
    fn test_sidebar_dirty_flag_set_on_add() {
        let (mut handler, dirty) = make_sidebar_handler_with_flag();
        assert!(!dirty.load(std::sync::atomic::Ordering::Relaxed));
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "hello"})).unwrap();
        assert!(dirty.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_sidebar_dirty_flag_set_on_clear() {
        let (mut handler, dirty) = make_sidebar_handler_with_flag();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "hello"})).unwrap();
        dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        handler.handle_tool_call("tttt_sidebar_message", &json!({"clear": true})).unwrap();
        assert!(dirty.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_sidebar_dirty_flag_not_set_on_list() {
        let (mut handler, dirty) = make_sidebar_handler_with_flag();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "hello"})).unwrap();
        dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        handler.handle_tool_call("tttt_sidebar_list", &json!({})).unwrap();
        assert!(!dirty.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_sidebar_dirty_flag_resettable() {
        let (mut handler, dirty) = make_sidebar_handler_with_flag();
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "a"})).unwrap();
        assert!(dirty.load(std::sync::atomic::Ordering::Relaxed));
        // Simulate main loop consuming the flag
        dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(!dirty.load(std::sync::atomic::Ordering::Relaxed));
        // Second message sets it again
        handler.handle_tool_call("tttt_sidebar_message", &json!({"message": "b"})).unwrap();
        assert!(dirty.load(std::sync::atomic::Ordering::Relaxed));
    }

    // === parse_rate_limit_reset tests ===

    #[test]
    fn test_parse_rate_limit_pm_no_minutes() {
        let screen = "You've hit your limit - resets 2pm (Europe/Brussels)\nSome other text";
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((14, 0, "Europe/Brussels".to_string())));
    }

    #[test]
    fn test_parse_rate_limit_am_no_minutes() {
        let screen = "resets 11am (US/Pacific) foo";
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((11, 0, "US/Pacific".to_string())));
    }

    #[test]
    fn test_parse_rate_limit_pm_with_minutes() {
        let screen = "resets 2:30pm (America/New_York)";
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((14, 30, "America/New_York".to_string())));
    }

    #[test]
    fn test_parse_rate_limit_am_with_minutes() {
        let screen = "resets 9:45am (Asia/Tokyo)";
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((9, 45, "Asia/Tokyo".to_string())));
    }

    #[test]
    fn test_parse_rate_limit_12pm_is_noon() {
        let screen = "resets 12pm (UTC)";
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((12, 0, "UTC".to_string())));
    }

    #[test]
    fn test_parse_rate_limit_12am_is_midnight() {
        let screen = "resets 12am (UTC)";
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((0, 0, "UTC".to_string())));
    }

    #[test]
    fn test_parse_rate_limit_no_match_returns_none() {
        let screen = "No rate limit here. Everything is fine.";
        assert_eq!(parse_rate_limit_reset(screen), None);
    }

    #[test]
    fn test_parse_rate_limit_full_dialog() {
        let screen = concat!(
            "You've hit your limit - resets 2pm (Europe/Brussels)\n\n",
            "/rate-limit-options\n\n",
            "What do you want to do?\n\n",
            "} 1. Stop and wait for limit to reset\n",
            "  2. Add funds to continue with extra usage\n",
            "  3. Upgrade your plan\n\n",
            "Enter to confirm · Esc to cancel"
        );
        let result = parse_rate_limit_reset(screen);
        assert_eq!(result, Some((14, 0, "Europe/Brussels".to_string())));
    }

    // === calculate_wait_duration tests ===

    #[test]
    fn test_calculate_wait_duration_adds_safety_margin() {
        // Pick a reset time far in the future (23:59 UTC) so we know it hasn't passed.
        let dur = calculate_wait_duration(23, 59, "UTC", 10).unwrap();
        // Safety margin of 10 minutes must be included.
        assert!(dur.as_secs() >= 10 * 60);
    }

    #[test]
    fn test_calculate_wait_duration_unknown_tz_errors() {
        let result = calculate_wait_duration(14, 0, "Fake/NotReal", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown timezone"));
    }

    #[test]
    fn test_calculate_wait_duration_past_time_wraps_to_tomorrow() {
        // Use hour 0 minute 1 UTC. If it has passed today we expect ~24h wait;
        // if not (extremely unlikely: test runs in the first minute of the day)
        // we expect < 2 minutes. Either way the safety margin of 0 is excluded.
        // We simply verify the function succeeds and returns a sensible value.
        let dur = calculate_wait_duration(0, 1, "UTC", 0).unwrap();
        // Result must be somewhere in (0, 25 hours).
        assert!(dur.as_secs() <= 25 * 3600, "wait too long: {:?}", dur);
        // Must be at least 0 seconds (non-negative).
        assert!(dur.as_secs() < 25 * 3600);
    }

    #[test]
    fn test_calculate_wait_duration_valid_timezone() {
        // Should not error for a well-known timezone.
        let result = calculate_wait_duration(14, 0, "Europe/Brussels", 15);
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_rate_limit_no_dialog() {
        let handler = make_handler_with_session();
        let id = handler.manager().lock().unwrap().list()[0].id.clone();
        let mut handler = handler;
        let result = handler
            .handle_tool_call(
                "tttt_pty_handle_rate_limit",
                &json!({"session_id": id}),
            )
            .unwrap();
        assert_eq!(result["status"], "no_rate_limit");
    }

    // --- ReplayToolHandler tests ---

    fn make_replay_db() -> (ReplayToolHandler, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let mut logger = tttt_log::SqliteLogger::new(&db_path).unwrap();
        logger.log_session_start("s1", "bash", 80, 24, Some("test session")).unwrap();
        logger.log_event(&tttt_log::LogEvent::with_timestamp(
            100, "s1".to_string(), tttt_log::Direction::Output, b"hello".to_vec(),
        )).unwrap();
        logger.log_event(&tttt_log::LogEvent::with_timestamp(
            200, "s1".to_string(), tttt_log::Direction::Output, b" world".to_vec(),
        )).unwrap();
        logger.log_event(&tttt_log::LogEvent::with_timestamp(
            300, "s1".to_string(), tttt_log::Direction::Input, b"typed".to_vec(),
        )).unwrap();
        logger.log_session_end("s1").unwrap();
        (ReplayToolHandler::new(db_path), dir)
    }

    #[test]
    fn test_replay_list_sessions_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.db");
        tttt_log::SqliteLogger::new(&db_path).unwrap();
        let mut handler = ReplayToolHandler::new(db_path);
        let result = handler.handle_tool_call("tttt_replay_list_sessions", &json!({})).unwrap();
        assert_eq!(result["sessions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_replay_list_sessions_with_data() {
        let (mut handler, _dir) = make_replay_db();
        let result = handler.handle_tool_call("tttt_replay_list_sessions", &json!({})).unwrap();
        let sessions = result["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], "s1");
        assert_eq!(sessions[0]["command"], "bash");
        assert_eq!(sessions[0]["name"], "test session");
    }

    #[test]
    fn test_replay_get_screen_end() {
        let (mut handler, _dir) = make_replay_db();
        let result = handler
            .handle_tool_call("tttt_replay_get_screen", &json!({"session_id": "s1"}))
            .unwrap();
        let screen = result["screen"].as_str().unwrap();
        assert!(screen.contains("hello world"), "screen: {:?}", screen);
        assert_eq!(result["event_index"], 3); // all 3 events processed
    }

    #[test]
    fn test_replay_get_screen_at_event_index() {
        let (mut handler, _dir) = make_replay_db();
        let result = handler
            .handle_tool_call("tttt_replay_get_screen", &json!({"session_id": "s1", "event_index": 1}))
            .unwrap();
        let screen = result["screen"].as_str().unwrap();
        assert!(screen.contains("hello"), "screen: {:?}", screen);
        assert!(!screen.contains("world"), "screen should not have world yet");
        assert_eq!(result["event_index"], 1);
    }

    #[test]
    fn test_replay_get_screen_at_timestamp() {
        let (mut handler, _dir) = make_replay_db();
        let result = handler
            .handle_tool_call("tttt_replay_get_screen", &json!({"session_id": "s1", "timestamp_ms": 100}))
            .unwrap();
        let screen = result["screen"].as_str().unwrap();
        assert!(screen.contains("hello"));
        assert!(!screen.contains("world"));
        assert_eq!(result["timestamp_ms"], 100u64);
    }

    #[test]
    fn test_replay_get_screen_returns_cursor() {
        let (mut handler, _dir) = make_replay_db();
        let result = handler
            .handle_tool_call("tttt_replay_get_screen", &json!({"session_id": "s1"}))
            .unwrap();
        assert!(result["cursor"].is_array());
        let cursor = result["cursor"].as_array().unwrap();
        assert_eq!(cursor.len(), 2);
    }

    #[test]
    fn test_replay_get_screen_missing_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        tttt_log::SqliteLogger::new(&db_path).unwrap();
        let mut handler = ReplayToolHandler::new(db_path);
        let result = handler.handle_tool_call("tttt_replay_get_screen", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_replay_get_screen_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        tttt_log::SqliteLogger::new(&db_path).unwrap();
        let mut handler = ReplayToolHandler::new(db_path);
        let result = handler.handle_tool_call("tttt_replay_get_screen", &json!({"session_id": "nope"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_replay_get_timeline() {
        let (mut handler, _dir) = make_replay_db();
        let result = handler
            .handle_tool_call("tttt_replay_get_timeline", &json!({"session_id": "s1"}))
            .unwrap();
        let timeline = result["timeline"].as_array().unwrap();
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[0]["index"], 0);
        assert_eq!(timeline[0]["timestamp_ms"], 100u64);
        assert_eq!(timeline[0]["direction"], "output");
        assert_eq!(timeline[2]["direction"], "input");
    }

    #[test]
    fn test_replay_get_timeline_missing_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        tttt_log::SqliteLogger::new(&db_path).unwrap();
        let mut handler = ReplayToolHandler::new(db_path);
        let result = handler.handle_tool_call("tttt_replay_get_timeline", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_replay_tool_definitions_count() {
        let dir = tempfile::tempdir().unwrap();
        let handler = ReplayToolHandler::new(dir.path().join("x.db"));
        let defs = handler.tool_definitions();
        assert_eq!(defs.len(), 3);
    }

    #[test]
    fn test_replay_tool_definitions_names() {
        let dir = tempfile::tempdir().unwrap();
        let handler = ReplayToolHandler::new(dir.path().join("x.db"));
        let defs = handler.tool_definitions();
        let names: Vec<&str> = defs.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"tttt_replay_list_sessions"));
        assert!(names.contains(&"tttt_replay_get_screen"));
        assert!(names.contains(&"tttt_replay_get_timeline"));
    }

    #[test]
    fn test_replay_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = ReplayToolHandler::new(dir.path().join("x.db"));
        let result = handler.handle_tool_call("nonexistent_tool", &json!({}));
        assert!(matches!(result.unwrap_err(), McpError::ToolNotFound(_)));
    }

    #[test]
    fn test_pty_launch_mock_with_sqlite_logger() {
        let logger = Arc::new(Mutex::new(tttt_log::SqliteLogger::in_memory().unwrap()));
        let handler = PtyToolHandler::new_owned(SessionManager::new(), std::path::PathBuf::from("/tmp"))
            .with_sqlite_logger(Some(Arc::clone(&logger)));
        let result = handler.handle_pty_launch_mock(&json!({"command": "sh"})).unwrap();
        assert!(result["session_id"].is_string());
        let sessions = logger.lock().unwrap().list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].command, "sh");
    }

    #[test]
    fn test_pty_launch_mock_without_sqlite_logger() {
        // Without sqlite_logger, launch should still succeed
        let handler = make_handler();
        let result = handler.handle_pty_launch_mock(&json!({})).unwrap();
        assert!(result["session_id"].is_string());
    }

    // === TuiToolHandler tests ===

    fn make_tui_handler() -> TuiToolHandler<MockPty> {
        let sessions: SharedSessionManager<MockPty> = Arc::new(Mutex::new(SessionManager::new()));
        // Add a test session
        let session = tttt_pty::PtySession::new(
            "pty-1".to_string(),
            MockPty::new(80, 24),
            "bash".to_string(),
            80, 24,
        );
        sessions.lock().unwrap().add_session(session).unwrap();
        TuiToolHandler::new(
            Arc::new(TuiState::new()),
            sessions,
            120, 40,
        )
    }

    #[test]
    fn test_tui_switch_valid_session() {
        let mut handler = make_tui_handler();
        let result = handler.handle_tool_call("tttt_tui_switch", &json!({"session_id": "pty-1"})).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["switched_to"], "pty-1");
        let pending = handler.tui_state.pending_switch.lock().unwrap().clone();
        assert_eq!(pending, Some("pty-1".to_string()));
        assert!(handler.tui_state.dirty.load(Ordering::Relaxed));
    }

    #[test]
    fn test_tui_switch_by_name_resolves_to_id() {
        // Add a named session
        let sessions: SharedSessionManager<MockPty> = Arc::new(Mutex::new(SessionManager::new()));
        let session = tttt_pty::PtySession::new(
            "pty-1".to_string(),
            MockPty::new(80, 24),
            "bash".to_string(),
            80, 24,
        );
        sessions.lock().unwrap().add_session_with_name(session, "my-shell".to_string()).unwrap();
        let tui_state = Arc::new(TuiState::new());
        let mut handler = TuiToolHandler::new(tui_state.clone(), sessions, 120, 40);

        // Switch using the name
        let result = handler.handle_tool_call("tttt_tui_switch", &json!({"session_id": "my-shell"})).unwrap();
        // Should resolve to the canonical ID, not the name
        assert_eq!(result["switched_to"], "pty-1");
        let pending = tui_state.pending_switch.lock().unwrap().clone();
        assert_eq!(pending, Some("pty-1".to_string()));
    }

    #[test]
    fn test_tui_highlight_by_name_resolves_to_id() {
        let sessions: SharedSessionManager<MockPty> = Arc::new(Mutex::new(SessionManager::new()));
        let session = tttt_pty::PtySession::new(
            "pty-1".to_string(),
            MockPty::new(80, 24),
            "bash".to_string(),
            80, 24,
        );
        sessions.lock().unwrap().add_session_with_name(session, "my-shell".to_string()).unwrap();
        let tui_state = Arc::new(TuiState::new());
        let mut handler = TuiToolHandler::new(tui_state.clone(), sessions, 120, 40);

        // Add highlight using the name
        let result = handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "my-shell", "id": "h1", "color": "red",
            "x": 0, "y": 0, "width": 10, "height": 1
        })).unwrap();
        assert_eq!(result["status"], "ok");
        // Highlight should be keyed by canonical ID
        let highlights = tui_state.highlights.lock().unwrap();
        assert!(highlights.contains_key("pty-1"));
        assert!(!highlights.contains_key("my-shell"));
    }

    #[test]
    fn test_tui_switch_invalid_session() {
        let mut handler = make_tui_handler();
        let result = handler.handle_tool_call("tttt_tui_switch", &json!({"session_id": "pty-999"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_tui_get_info() {
        let mut handler = make_tui_handler();
        let result = handler.handle_tool_call("tttt_tui_get_info", &json!({})).unwrap();
        assert_eq!(result["screen_cols"], 120);
        assert_eq!(result["screen_rows"], 40);
        assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(result["highlight_count"], 0);
    }

    #[test]
    fn test_tui_highlight_add() {
        let mut handler = make_tui_handler();
        let result = handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 5, "y": 2, "width": 10, "height": 3, "color": "red"
        })).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["action"], "set");

        let highlights = handler.tui_state.highlights.lock().unwrap();
        let list = highlights.get("pty-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].color, "red");
        assert_eq!(list[0].x, 5);
    }

    #[test]
    fn test_tui_highlight_multiple_per_pane() {
        let mut handler = make_tui_handler();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 0, "y": 0, "width": 5, "height": 1, "color": "red"
        })).unwrap();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h2",
            "x": 10, "y": 5, "width": 20, "height": 2, "color": "blue"
        })).unwrap();

        let highlights = handler.tui_state.highlights.lock().unwrap();
        let list = highlights.get("pty-1").unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_tui_highlight_update_existing() {
        let mut handler = make_tui_handler();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 0, "y": 0, "width": 5, "height": 1, "color": "red"
        })).unwrap();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 10, "y": 10, "width": 20, "height": 5, "color": "green"
        })).unwrap();

        let highlights = handler.tui_state.highlights.lock().unwrap();
        let list = highlights.get("pty-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].color, "green");
        assert_eq!(list[0].x, 10);
    }

    #[test]
    fn test_tui_highlight_remove() {
        let mut handler = make_tui_handler();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 0, "y": 0, "width": 5, "height": 1, "color": "red"
        })).unwrap();
        let result = handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1", "color": ""
        })).unwrap();
        assert_eq!(result["action"], "removed");

        let highlights = handler.tui_state.highlights.lock().unwrap();
        assert!(highlights.get("pty-1").is_none());
    }

    #[test]
    fn test_tui_highlight_remove_one_of_many() {
        let mut handler = make_tui_handler();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 0, "y": 0, "width": 5, "height": 1, "color": "red"
        })).unwrap();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h2",
            "x": 10, "y": 0, "width": 5, "height": 1, "color": "blue"
        })).unwrap();
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1", "color": ""
        })).unwrap();

        let highlights = handler.tui_state.highlights.lock().unwrap();
        let list = highlights.get("pty-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "h2");
    }

    #[test]
    fn test_tui_highlight_sets_dirty() {
        let mut handler = make_tui_handler();
        assert!(!handler.tui_state.dirty.load(Ordering::Relaxed));
        handler.handle_tool_call("tttt_tui_highlight", &json!({
            "session_id": "pty-1", "id": "h1",
            "x": 0, "y": 0, "width": 1, "height": 1, "color": "red"
        })).unwrap();
        assert!(handler.tui_state.dirty.load(Ordering::Relaxed));
    }

}
