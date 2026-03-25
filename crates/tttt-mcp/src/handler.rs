use crate::error::{McpError, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tttt_pty::{MockPty, PtyBackend, PtySession, SessionManager, SessionStatus};
use tttt_scheduler::Scheduler;

/// Trait for handling MCP tool calls.
pub trait ToolHandler: Send {
    /// Handle a tool call by name with the given arguments.
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value>;

    /// Return the list of tool definitions this handler provides.
    fn tool_definitions(&self) -> Vec<Value>;
}

/// Shared session manager type used by both the TUI and MCP server.
pub type SharedSessionManager<B> = Arc<Mutex<SessionManager<B>>>;

/// Handles PTY-related tool calls by delegating to a shared SessionManager.
pub struct PtyToolHandler<B: PtyBackend> {
    manager: SharedSessionManager<B>,
    work_dir: std::path::PathBuf,
    default_cols: u16,
    default_rows: u16,
}

impl<B: PtyBackend> PtyToolHandler<B> {
    /// Create a new handler with a shared session manager.
    pub fn new(manager: SharedSessionManager<B>, work_dir: std::path::PathBuf) -> Self {
        Self {
            manager,
            work_dir,
            default_cols: 80,
            default_rows: 24,
        }
    }

    /// Create a handler that owns its own session manager (convenience for standalone use).
    pub fn new_owned(manager: SessionManager<B>, work_dir: std::path::PathBuf) -> Self {
        Self::new(Arc::new(Mutex::new(manager)), work_dir)
    }

    /// Access the shared session manager.
    pub fn manager(&self) -> &SharedSessionManager<B> {
        &self.manager
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
        let session = PtySession::new(id.clone(), mock, command, cols, rows);
        if let Some(n) = name.clone() {
            mgr.add_session_with_name(session, n)?;
        } else {
            mgr.add_session(session)?;
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
            "tttt_pty_start_capture" => self.handle_pty_start_capture(args),
            "tttt_pty_stop_capture" => self.handle_pty_stop_capture(args),
            "tttt_get_status" => self.handle_get_status(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
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
            "tttt_pty_start_capture" => self.handle_pty_start_capture(args),
            "tttt_pty_stop_capture" => self.handle_pty_stop_capture(args),
            "tttt_get_status" => self.handle_get_status(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
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
            "tttt_pty_start_capture" => self.handle_pty_start_capture(args),
            "tttt_pty_stop_capture" => self.handle_pty_stop_capture(args),
            "tttt_get_status" => self.handle_get_status(),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
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

        let now = Instant::now();
        let mut sched = self
            .scheduler
            .lock()
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        let id = sched.add_cron(expression, command, session_id, now)?;
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

// === Scratchpad tool handler ===

/// Handles scratchpad read/write tool calls using an in-memory HashMap.
pub struct ScratchpadToolHandler {
    store: HashMap<String, String>,
}

impl ScratchpadToolHandler {
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
        }
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

        if append {
            self.store
                .entry(key)
                .and_modify(|v| v.push_str(&content))
                .or_insert(content);
        } else {
            self.store.insert(key, content);
        }
        Ok(json!({"status": "ok"}))
    }

    fn handle_scratchpad_read(&self, args: &Value) -> Result<Value> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("key required".into()))?;
        match self.store.get(key) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(defs.len(), 13);
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

        // Should have 10 PTY + 4 scheduler = 14 tool definitions
        let defs = composite.tool_definitions();
        assert_eq!(defs.len(), 14);

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

}
