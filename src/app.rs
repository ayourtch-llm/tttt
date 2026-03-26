use crate::config::Config;
use crate::reload::{self, SavedState, SavedSession, SavedCronJob, SavedWatcher};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tttt_log::{Direction, LogEvent, LogSink, MultiLogger, SharedSqliteLogSink, SqliteLogger, TextLogger};
use tttt_mcp::notification::NotificationRegistry;
use tttt_mcp::{SharedNotificationRegistry, SharedScheduler, SharedScratchpad, SharedSidebarMessages};
use tttt_pty::{AnyPty, PtySession, RealPty, SessionManager, SessionStatus};
use tttt_scheduler::{Scheduler, SchedulerEvent};
use std::os::unix::net::UnixListener;
use tttt_tui::{
    clear_screen, cursor_goto, protocol, InputEvent, InputParser, PaneRenderer, RawInput,
    SessionInfo, SidebarRenderer, ViewerClient,
};

/// Minimum time between renders to the server terminal (ms).
/// During rapid updates (e.g., Claude Code redrawing history),
/// we accumulate changes and only render once the burst settles.
const RENDER_DEBOUNCE_MS: u64 = 50;

/// Terminal state saved/restored around raw mode.
struct TerminalState {
    original_termios: Option<nix::sys::termios::Termios>,
}

impl TerminalState {
    fn enter_raw_mode() -> Self {
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
        Self { original_termios: original }
    }

    fn restore(&self) {
        if let Some(ref orig) = self.original_termios {
            let stdin = std::io::stdin();
            let _ = nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSAFLUSH, orig);
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        self.restore();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Clean up socket files on exit
        if let Some(ref path) = self.socket_path {
            let _ = std::fs::remove_file(path);
        }
        if let Some(ref path) = self.mcp_socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
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

fn write_all(fd: i32, data: &[u8]) -> nix::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        match nix::unistd::write(borrowed, &data[offset..]) {
            Ok(n) => offset += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reconcile the ordered session list against the ground-truth set.
///
/// - Preserves the relative order of IDs already in `current`.
/// - Appends any IDs in `actual` not yet present in `current`.
/// - Removes IDs that are no longer in `actual`.
fn reconcile_session_order(current: &[String], actual: &[String]) -> Vec<String> {
    let mut result: Vec<String> = current
        .iter()
        .filter(|id| actual.contains(id))
        .cloned()
        .collect();
    for id in actual {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    result
}

/// Compute the new session index after a relative navigation step.
///
/// Returns `None` when `total == 0`. `current_idx = None` is treated as 0.
/// Wraps around in both directions.
fn compute_relative_index(
    current_idx: Option<usize>,
    delta: i32,
    total: usize,
) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let cur = current_idx.unwrap_or(0) as i32;
    let len = total as i32;
    let new_idx = ((cur + delta) % len + len) % len;
    Some(new_idx as usize)
}

/// Maps a raw prefix-key byte to its human-readable name.
fn prefix_key_name(key: u8) -> String {
    match key {
        0x1c => "Ctrl+\\".to_string(),
        0x01 => "Ctrl+A".to_string(),
        0x02 => "Ctrl+B".to_string(),
        b    => format!("0x{:02x}", b),
    }
}

/// Builds the full help-screen string (with terminal escape sequences).
fn format_help_screen(prefix_name: &str) -> String {
    format!(
        "{}{}\x1b[1mtttt help\x1b[0m  (prefix: {})\
        {}  0-9  Switch to terminal N\
        {}  n    Next terminal\
        {}  p    Previous terminal\
        {}  d    Detach/quit\
        {}  r    Live reload (execv)\
        {}  ?    This help\
        {}  {p}{p}  Send literal prefix\
        {}Press any key to dismiss...",
        clear_screen(), cursor_goto(2, 4), prefix_name,
        cursor_goto(4, 4), cursor_goto(5, 4), cursor_goto(6, 4),
        cursor_goto(7, 4), cursor_goto(8, 4),
        cursor_goto(9, 4), cursor_goto(11, 4),
        cursor_goto(13, 4), p = prefix_name,
    )
}

/// Main application state.
pub struct App {
    config: Config,
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
    input_parser: InputParser,
    sidebar: SidebarRenderer,
    pane_renderer: PaneRenderer,
    logger: MultiLogger,
    sqlite_logger: Option<Arc<Mutex<SqliteLogger>>>,
    scheduler: SharedScheduler,
    notifications: SharedNotificationRegistry,
    scratchpad: SharedScratchpad,
    sidebar_messages: SharedSidebarMessages,
    active_session: Option<String>,
    session_order: Vec<String>,
    screen_cols: u16,
    screen_rows: u16,
    /// Unix socket listener for viewer connections.
    viewer_listener: Option<UnixListener>,
    /// Connected viewer clients.
    viewer_clients: Vec<ViewerClient>,
    /// Path to the viewer socket.
    pub socket_path: Option<String>,
    /// Unix socket listener for MCP proxy connections.
    mcp_listener: Option<UnixListener>,
    /// Path to the MCP proxy socket.
    pub mcp_socket_path: Option<String>,
    /// Whether the server terminal needs a render.
    server_render_dirty: bool,
    /// Last root session screen + status, captured on exit for diagnostics.
    pub last_root_screen: Option<(String, SessionStatus)>,
    /// When the current dirty burst started (for max latency cap).
    first_dirty_time: Option<Instant>,
    /// When the last PTY data was received (for burst-end detection).
    last_pty_data_time: Option<Instant>,
    /// Queued notification injections, drained one at a time to avoid garbling input.
    pending_injection_queue: Vec<(String, String)>,
    /// When the last injection was performed (for pacing).
    last_injection_time: Option<Instant>,
    /// Set to true when a live reload has been requested (prefix+R or SIGUSR1).
    pub reload_requested: bool,
    /// Set to true when a full reload (with root restart) has been requested (SIGUSR2).
    pub restart_root_requested: bool,
    /// When the server started (for uptime display).
    server_start_time: Instant,
}

impl App {
    pub fn new(config: Config) -> Self {
        let display_config = config.display_config();
        let (cols, rows) = terminal_size();
        let pty_cols = cols.saturating_sub(config.sidebar_width);
        let pty_rows = rows.saturating_sub(1);
        Self {
            sessions: Arc::new(Mutex::new(SessionManager::with_max_sessions(config.max_sessions))),
            input_parser: InputParser::new(display_config),
            sidebar: SidebarRenderer::new(config.sidebar_width),
            pane_renderer: PaneRenderer::new(pty_cols, pty_rows, 1, 1),
            logger: MultiLogger::new(),
            sqlite_logger: None,
            scheduler: Arc::new(Mutex::new(Scheduler::new())),
            notifications: Arc::new(Mutex::new(NotificationRegistry::new())),
            scratchpad: Arc::new(Mutex::new(std::collections::HashMap::new())),
            sidebar_messages: Arc::new(Mutex::new(Vec::new())),
            active_session: None,
            session_order: Vec::new(),
            screen_cols: cols,
            screen_rows: rows,
            viewer_listener: None,
            viewer_clients: Vec::new(),
            socket_path: None,
            mcp_listener: None,
            mcp_socket_path: None,
            server_render_dirty: false,
            last_root_screen: None,
            first_dirty_time: None,
            pending_injection_queue: Vec::new(),
            last_injection_time: None,
            last_pty_data_time: None,
            reload_requested: false,
            restart_root_requested: false,
            server_start_time: Instant::now(),
            config,
        }
    }

    /// Get a shared reference to the session manager (for the MCP server thread).
    pub fn shared_sessions(&self) -> Arc<Mutex<SessionManager<AnyPty>>> {
        self.sessions.clone()
    }

    /// Restore sessions from a SavedState (after execv reload).
    pub fn restore_sessions(&mut self, state: &SavedState) -> Result<(), Box<dyn std::error::Error>> {
        self.restore_sessions_filtered(state, |_| true)
    }

    /// Restore sessions from saved state, with a filter predicate.
    /// Sessions for which the predicate returns false are skipped.
    pub fn restore_sessions_filtered<F>(&mut self, state: &SavedState, mut should_restore: F) -> Result<(), Box<dyn std::error::Error>>
    where
        F: FnMut(&SavedSession) -> bool,
    {
        use tttt_pty::RestoredPty;

        let mut mgr = self.sessions.lock().unwrap();
        let mut errors = Vec::new();

        for saved in &state.sessions {
            // Only restore running sessions
            if saved.status != SessionStatus::Running {
                continue;
            }

            // Check filter predicate
            if !should_restore(saved) {
                // Close the inherited FD so it doesn't leak
                unsafe { libc::close(saved.master_fd); }
                continue;
            }

            match RestoredPty::from_raw_fd(saved.master_fd, saved.child_pid) {
                Ok(restored_backend) => {
                    let backend = AnyPty::Restored(restored_backend);
                    let mut session = PtySession::new(
                        saved.id.clone(),
                        backend,
                        saved.command.clone(),
                        saved.cols,
                        saved.rows,
                    );

                    // Replay formatted screen contents to restore visual state
                    if !saved.screen_contents_formatted.is_empty() {
                        session.inject_screen_data(&saved.screen_contents_formatted);
                    }

                    if let Some(ref name) = saved.name {
                        if let Err(e) = mgr.add_session_with_name(session, name.clone()) {
                            errors.push(format!("session {}: {}", saved.id, e));
                        }
                    } else if let Err(e) = mgr.add_session(session) {
                        errors.push(format!("session {}: {}", saved.id, e));
                    }
                }
                Err(e) => {
                    errors.push(format!("session {} (fd {}): {}", saved.id, saved.master_fd, e));
                }
            }
        }
        drop(mgr);

        // Restore session order and active session
        self.session_order = state.session_order.clone();
        self.active_session = state.active_session.clone();

        // Set the next_id counter to avoid ID collisions
        // We need to set it high enough that generate_id() won't collide
        {
            let mut mgr = self.sessions.lock().unwrap();
            // Generate IDs up to the saved next_id to advance the counter
            while mgr.next_id() < state.next_session_id {
                let _ = mgr.generate_id();
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; ").into())
        }
    }

    /// Restore cron jobs from saved state.
    pub fn restore_cron_jobs(&self, cron_jobs: &[reload::SavedCronJob]) {
        let mut sched = self.scheduler.lock().unwrap();
        let now = std::time::Instant::now();
        for job in cron_jobs {
            if let Err(e) = sched.add_cron(
                job.expression.clone(),
                job.command.clone(),
                job.session_id.clone(),
                now,
            ) {
                eprintln!("Warning: failed to restore cron job {}: {}", job.id, e);
            }
        }
    }

    /// Restore notification watchers from saved state.
    pub fn restore_watchers(&self, watchers: &[reload::SavedWatcher]) {
        let mut notif = self.notifications.lock().unwrap();
        for w in watchers {
            if let Err(e) = notif.add_watcher(
                w.watch_session_id.clone(),
                &w.pattern,
                w.inject_text.clone(),
                w.inject_session_id.clone(),
                w.one_shot,
            ) {
                eprintln!("Warning: failed to restore watcher {}: {}", w.id, e);
            }
        }
    }

    /// Restore scratchpad data from saved state.
    pub fn restore_scratchpad(&self, data: &std::collections::HashMap<String, String>) {
        let mut store = self.scratchpad.lock().unwrap();
        store.extend(data.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    /// Restore sidebar messages from saved state.
    pub fn restore_sidebar_messages(&self, messages: &[String]) {
        let mut msgs = self.sidebar_messages.lock().unwrap();
        msgs.extend_from_slice(messages);
    }

    /// Start listening for viewer connections on a Unix socket.
    /// Start the MCP proxy socket listener.
    /// Returns the socket path that `tttt mcp-server --connect` should use.
    pub fn start_mcp_listener(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let path = format!("/tmp/tttt-mcp-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        self.mcp_socket_path = Some(path.clone());
        self.mcp_listener = Some(listener);
        Ok(path)
    }

    pub fn start_viewer_listener(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let path = format!("/tmp/tttt-{}.sock", std::process::id());
        // Clean up stale socket
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        self.socket_path = Some(path.clone());
        self.viewer_listener = Some(listener);
        Ok(path)
    }

    /// Generate a temporary MCP config file for the root agent.
    /// Returns the path to the config file.
    pub fn generate_mcp_config(&self) -> Result<String, Box<dyn std::error::Error>> {
        let mcp_socket = self.mcp_socket_path.as_ref()
            .ok_or("MCP listener not started")?;

        // Find our own binary path
        let tttt_bin = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("tttt"));

        let config = serde_json::json!({
            "mcpServers": {
                "tttt": {
                    "command": tttt_bin.to_string_lossy(),
                    "args": ["mcp-server", "--connect", mcp_socket]
                }
            }
        });

        let config_path = format!("/tmp/tttt-mcp-config-{}.json", std::process::id());
        std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
        Ok(config_path)
    }

    pub fn init_loggers(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(&self.config.log_dir)?;
        if let Some(parent) = self.config.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text_logger = TextLogger::new(&self.config.log_dir)?;
        self.logger.add_sink(Box::new(text_logger));
        let sqlite_logger = Arc::new(Mutex::new(SqliteLogger::new(&self.config.db_path)?));
        self.logger.add_sink(Box::new(SharedSqliteLogSink(Arc::clone(&sqlite_logger))));
        self.sqlite_logger = Some(sqlite_logger);
        Ok(())
    }

    /// Set up a one-shot notification watcher that auto-injects "Continue from where
    /// you left off." when the root session shows Claude Code is fully ready.
    /// We match "? for shortcuts" which only appears when Claude is loaded and
    /// waiting for input, avoiding false triggers during startup rendering.
    pub fn setup_auto_continue(&self, root_session_id: &str) {
        let mut notif = self.notifications.lock().unwrap();
        if let Err(e) = notif.add_watcher(
            root_session_id.to_string(),
            r"\? for shortcuts",
            "Continue from where you left off.\n".to_string(),
            root_session_id.to_string(),
            true, // one-shot
        ) {
            eprintln!("Warning: failed to set up auto-continue watcher: {}", e);
        }
    }

    /// Queue a text injection into a session. Will be drained by the event loop.
    pub fn queue_injection(&mut self, session_id: &str, text: &str) {
        self.pending_injection_queue.push((session_id.to_string(), text.to_string()));
    }

    /// Remove a session ID from the session order list.
    pub fn remove_from_session_order(&mut self, id: &str) {
        self.session_order.retain(|s| s != id);
        if self.active_session.as_deref() == Some(id) {
            self.active_session = self.session_order.first().cloned();
        }
    }

    pub fn launch_root(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let pty_cols = self.screen_cols.saturating_sub(self.config.sidebar_width);
        let pty_rows = self.screen_rows.saturating_sub(1);

        // If MCP socket is available, generate config and inject --mcp-config
        // for agents that support it (e.g., claude)
        let mut args: Vec<String> = self.config.root_args.clone();
        let mut mcp_config_path: Option<String> = None;
        if self.mcp_socket_path.is_some() {
            if let Ok(config_path) = self.generate_mcp_config() {
                mcp_config_path = Some(config_path.clone());
                // Check if the command looks like claude and inject --mcp-config
                let cmd = &self.config.root_command;
                if cmd.contains("claude") && !args.iter().any(|a| a.contains("mcp-config")) {
                    args.push("--mcp-config".to_string());
                    args.push(config_path);
                }
            }
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let tttt_pid = std::process::id();
        let real_backend = RealPty::spawn_with_cwd_and_env(
            &self.config.root_command, &args_refs, Some(&self.config.work_dir), pty_cols, pty_rows,
            [("TTTT_PID".to_string(), tttt_pid.to_string())],
        )?;
        let backend = AnyPty::Real(real_backend);
        let mut mgr = self.sessions.lock().unwrap();
        let id = mgr.generate_id();
        let session = PtySession::new(id.clone(), backend, self.config.root_command.clone(), pty_cols, pty_rows);
        mgr.add_session(session)?;
        drop(mgr);
        self.session_order.push(id.clone());
        self.active_session = Some(id.clone());
        if let Some(ref logger) = self.sqlite_logger {
            let _ = logger.lock().unwrap().log_session_start(
                &id, &self.config.root_command, pty_cols, pty_rows, None,
            );
        }
        Ok(id)
    }

    /// Create a new PTY session with the default shell.
    pub fn create_session(&mut self, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let pty_cols = self.screen_cols.saturating_sub(self.config.sidebar_width);
        let pty_rows = self.screen_rows.saturating_sub(1);

        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let tttt_pid = std::process::id();
        let real_backend = RealPty::spawn_with_cwd_and_env(
            &default_shell, &[], Some(&self.config.work_dir), pty_cols, pty_rows,
            [("TTTT_PID".to_string(), tttt_pid.to_string())],
        )?;
        let backend = AnyPty::Real(real_backend);
        let mut mgr = self.sessions.lock().unwrap();
        let id = mgr.generate_id();
        let session = PtySession::new(id.clone(), backend, default_shell, pty_cols, pty_rows);
        mgr.add_session(session)?;
        drop(mgr);
        self.session_order.push(id.clone());
        self.switch_to_session(&id, stdout_fd)?;
        Ok(())
    }

    /// Build the SavedState for a live reload. Does NOT call execv.
    pub fn prepare_reload(&self) -> Result<SavedState, Box<dyn std::error::Error>> {
        let mgr = self.sessions.lock().unwrap();
        let mut sessions = Vec::new();

        for meta in mgr.list() {
            if let Ok(session) = mgr.get(&meta.id) {
                let master_fd = session.backend().reader_raw_fd();
                // Try to discover child PID via TIOCGPGRP
                let child_pid = {
                    let mut pgid: libc::pid_t = 0;
                    let ret = unsafe { libc::ioctl(master_fd, libc::TIOCGPGRP, &mut pgid) };
                    if ret == 0 && pgid > 0 { Some(pgid) } else { None }
                };
                let screen_contents_formatted = session.get_screen_formatted();

                sessions.push(SavedSession {
                    id: meta.id.clone(),
                    name: meta.name.clone(),
                    command: meta.command.clone(),
                    status: meta.status.clone(),
                    cols: meta.cols,
                    rows: meta.rows,
                    master_fd,
                    child_pid,
                    screen_contents_formatted,
                });
            }
        }
        drop(mgr);

        // Save cron jobs from scheduler
        let cron_jobs: Vec<SavedCronJob> = {
            let sched = self.scheduler.lock().unwrap();
            sched.list_cron().iter().map(|job| SavedCronJob {
                id: job.id.clone(),
                expression: job.expression.clone(),
                command: job.command.clone(),
                session_id: job.session_id.clone(),
            }).collect()
        };

        // Save notification watchers
        let watchers: Vec<SavedWatcher> = {
            let notif = self.notifications.lock().unwrap();
            notif.list_watchers().iter().map(|w| SavedWatcher {
                id: w.id.clone(),
                watch_session_id: w.watch_session_id.clone(),
                pattern: w.pattern.clone(),
                inject_text: w.inject_text.clone(),
                inject_session_id: w.inject_session_id.clone(),
                one_shot: w.one_shot,
            }).collect()
        };

        Ok(SavedState {
            version: reload::STATE_VERSION,
            sessions,
            active_session: self.active_session.clone(),
            session_order: self.session_order.clone(),
            next_session_id: self.sessions.lock().unwrap().next_id(),
            cron_jobs,
            watchers,
            scratchpad: self.scratchpad.lock().unwrap().clone(),
            sidebar_messages: self.sidebar_messages.lock().unwrap().clone(),
            config: self.config.clone(),
            screen_cols: self.screen_cols,
            screen_rows: self.screen_rows,
            restart_root: self.restart_root_requested,
        })
    }

    /// Perform the live reload: save state, clear CLOEXEC on PTY FDs, and execv.
    /// This function does not return on success.
    pub fn execute_reload(&self) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.prepare_reload()?;

        // Clear CLOEXEC on all PTY master FDs so they survive exec
        for session in &state.sessions {
            reload::clear_cloexec(session.master_fd)?;
        }

        // Write state file
        let path = state.write_to_file()?;
        std::env::set_var(reload::RESTORE_ENV_VAR, &path);

        // Close socket listeners (will be re-created after exec)
        // (They are dropped when App is dropped, but we want explicit cleanup)
        if let Some(ref socket_path) = self.socket_path {
            let _ = std::fs::remove_file(socket_path);
        }
        if let Some(ref mcp_socket_path) = self.mcp_socket_path {
            let _ = std::fs::remove_file(mcp_socket_path);
        }

        // execv replaces the process image — does not return on success
        reload::exec_self()?;
        unreachable!()
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let _terminal_state = TerminalState::enter_raw_mode();
        let winch = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGWINCH, Arc::clone(&winch));
        let sigusr1 = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGUSR1, Arc::clone(&sigusr1));
        let sigusr2 = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGUSR2, Arc::clone(&sigusr2));

        let stdout_fd = std::io::stdout().as_raw_fd();
        let stdin_fd = std::io::stdin().as_raw_fd();

        write_all(stdout_fd, clear_screen().as_bytes())?;
        self.render_sidebar(stdout_fd)?;

        loop {
            // Get active PTY fd for polling (short lock)
            let pty_fd = self.active_session.as_ref().and_then(|id| {
                let mgr = self.sessions.lock().unwrap();
                mgr.get(id).ok().map(|s| s.backend().reader_raw_fd())
            });

            let stdin_pfd = PollFd::new(
                unsafe { BorrowedFd::borrow_raw(stdin_fd) }, PollFlags::POLLIN,
            );
            // Shorter poll timeout when we have a pending render (for debounce responsiveness)
            let poll_timeout_ms = if self.server_render_dirty { 10u16 } else { 100u16 };

            let poll_result = if let Some(pty_raw_fd) = pty_fd {
                let pty_pfd = PollFd::new(
                    unsafe { BorrowedFd::borrow_raw(pty_raw_fd) }, PollFlags::POLLIN,
                );
                let mut fds = [pty_pfd, stdin_pfd];
                let _ = poll(&mut fds, PollTimeout::from(poll_timeout_ms));
                (fds[0].revents(), fds[1].revents())
            } else {
                let mut fds = [stdin_pfd];
                let _ = poll(&mut fds, PollTimeout::from(poll_timeout_ms));
                (None, fds[0].revents())
            };

            if winch.load(Ordering::Relaxed) {
                winch.store(false, Ordering::Relaxed);
                self.handle_resize(stdout_fd)?;
            }

            if sigusr1.load(Ordering::Relaxed) {
                sigusr1.store(false, Ordering::Relaxed);
                self.reload_requested = true;
                break;
            }

            if sigusr2.load(Ordering::Relaxed) {
                sigusr2.store(false, Ordering::Relaxed);
                self.reload_requested = true;
                self.restart_root_requested = true;
                break;
            }

            // Read PTY output — pump into screen buffer but defer rendering
            if let Some(flags) = poll_result.0 {
                if flags.contains(PollFlags::POLLIN) {
                    if let Some(id) = self.active_session.clone() {
                        let mut mgr = self.sessions.lock().unwrap();
                        if let Ok(session) = mgr.get_mut(&id) {
                            match session.pump_raw() {
                                Ok((n, raw_bytes)) if n > 0 => {
                                    let _ = self.logger.log_event(&LogEvent::new(
                                        id.clone(), Direction::Output, raw_bytes,
                                    ));
                                    let now = Instant::now();
                                    if !self.server_render_dirty {
                                        self.first_dirty_time = Some(now);
                                    }
                                    self.server_render_dirty = true;
                                    self.last_pty_data_time = Some(now);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            // Debounced render: only render to server terminal when
            // dirty AND enough time has passed since last PTY data.
            // This absorbs rapid redraws (e.g., Claude Code history)
            // into a single clean update.
            if self.server_render_dirty {
                let now = Instant::now();
                let burst_ended = self.last_pty_data_time
                    .map(|t| now.duration_since(t).as_millis() >= RENDER_DEBOUNCE_MS as u128)
                    .unwrap_or(true);
                let max_latency_exceeded = self.first_dirty_time
                    .map(|t| now.duration_since(t).as_millis() >= (RENDER_DEBOUNCE_MS * 4) as u128)
                    .unwrap_or(false);
                let should_render = burst_ended || max_latency_exceeded;

                if should_render {
                    if let Some(id) = self.active_session.clone() {
                        let render_data = {
                            let mgr = self.sessions.lock().unwrap();
                            if let Ok(session) = mgr.get(&id) {
                                let pane_output = self.pane_renderer.render(session.screen().screen());
                                let cursor = session.cursor_position();
                                Some((pane_output, cursor))
                            } else { None }
                        };
                        if let Some((pane_output, (row, col))) = render_data {
                            if !pane_output.is_empty() {
                                write_all(stdout_fd, &pane_output)?;
                                self.render_sidebar(stdout_fd)?;
                            }
                            let (tr, tc) = self.pane_renderer.cursor_terminal_position(row, col);
                            write_all(stdout_fd, cursor_goto(tr, tc).as_bytes())?;
                        }
                    }
                    self.server_render_dirty = false;
                    self.first_dirty_time = None;
                }
            }

            // Read stdin
            if let Some(flags) = poll_result.1 {
                if flags.contains(PollFlags::POLLIN) {
                    let mut buf = [0u8; 4096];
                    match nix::unistd::read(stdin_fd, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let raw = RawInput { bytes: buf[..n].to_vec() };
                            let events = self.input_parser.process(&raw);
                            for event in events {
                                match self.handle_input_event(event, stdout_fd) {
                                    Ok(true) => {}
                                    Ok(false) => return Ok(()),
                                    Err(e) => {
                                        let _ = self.logger.log_event(&LogEvent::new(
                                            "system".to_string(), Direction::Meta,
                                            format!("Input error: {}", e).into_bytes(),
                                        ));
                                    }
                                }
                            }
                        }
                        Err(nix::errno::Errno::EAGAIN) => {}
                        Err(e) => return Err(Box::new(e)),
                    }
                }
            }

            // Accept new MCP proxy connections (each runs in its own thread)
            self.accept_mcp_connections();

            // Accept new viewer connections
            self.accept_viewer_connections();

            // Process viewer client input
            self.process_viewer_input(stdout_fd)?;

            // Send screen updates to all viewers
            self.update_viewers();

            // Pump all non-active sessions to keep screens updated and log output
            {
                let active_id = self.active_session.clone();
                let mut mgr = self.sessions.lock().unwrap();
                let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
                for sid in ids {
                    if active_id.as_deref() == Some(&sid) {
                        continue; // already pumped above
                    }
                    if let Ok(session) = mgr.get_mut(&sid) {
                        if let Ok((n, raw_bytes)) = session.pump_raw() {
                            if n > 0 {
                                let _ = self.logger.log_event(&LogEvent::new(
                                    sid.clone(), Direction::Output, raw_bytes,
                                ));
                            }
                        }
                    }
                }
            }

            // Check notification watchers against all sessions — queue new injections
            {
                let mgr = self.sessions.lock().unwrap();
                let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
                let mut notif = self.notifications.lock().unwrap();
                for sid in &ids {
                    if let Ok(session) = mgr.get(sid) {
                        let screen_text = session.get_screen();
                        for inj in notif.check_session(sid, &screen_text) {
                            self.pending_injection_queue.push((inj.target_session_id, inj.text));
                        }
                    }
                }
            }
            // Drain one queued injection per tick with pacing (100ms between injections)
            // to avoid garbling the user's in-progress typing.
            const INJECTION_PACE_MS: u64 = 100;
            if !self.pending_injection_queue.is_empty() {
                let can_inject = self.last_injection_time
                    .map_or(true, |t| t.elapsed() >= std::time::Duration::from_millis(INJECTION_PACE_MS));
                if can_inject {
                    let (target_id, text) = self.pending_injection_queue.remove(0);
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(&target_id) {
                        let mut bytes = text.replace("[ENTER]", "\r").into_bytes();
                        // Always auto-submit: append \r if not already present
                        if bytes.last() != Some(&b'\r') {
                            bytes.push(b'\r');
                        }
                        let _ = session.send_raw(&bytes);
                    }
                    let _ = self.logger.log_event(&LogEvent::new(
                        target_id.clone(), Direction::Meta,
                        format!("[NOTIFICATION] {}", text).into_bytes(),
                    ));
                    self.last_injection_time = Some(Instant::now());
                }
            }

            // Sync session order (MCP may have added new ones)
            self.sync_session_order();

            if self.check_session_exit() { break; }

            let events = self.scheduler.lock().unwrap().tick(std::time::Instant::now());
            for event in events { self.handle_scheduler_event(event); }
        }

        // Capture last screen content from root session for diagnostics
        if let Some(ref id) = self.active_session {
            let mut mgr = self.sessions.lock().unwrap();
            if let Ok(session) = mgr.get_mut(id) {
                // Final pump to get any remaining output
                let _ = session.pump();
                let screen = session.get_screen();
                let status = session.status().clone();
                self.last_root_screen = Some((screen, status));
            }
        }

        Ok(())
    }

    /// Sync session_order with the actual sessions (MCP may have added new ones).
    fn sync_session_order(&mut self) {
        let mgr = self.sessions.lock().unwrap();
        let actual_ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
        drop(mgr);
        self.session_order = reconcile_session_order(&self.session_order, &actual_ids);
    }

    fn handle_input_event(&mut self, event: InputEvent, stdout_fd: i32) -> Result<bool, Box<dyn std::error::Error>> {
        match event {
            InputEvent::PassThrough(data) => {
                if let Some(ref id) = self.active_session {
                    if self.config.log_input {
                        let _ = self.logger.log_event(&LogEvent::new(id.clone(), Direction::Input, data.clone()));
                    }
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(id) {
                        session.send_raw(&data)?;
                    }
                }
            }
            InputEvent::SwitchTerminal(n) => {
                if let Some(id) = self.session_order.get(n).cloned() {
                    self.switch_to_session(&id, stdout_fd)?;
                }
            }
            InputEvent::NextTerminal => self.switch_relative(1, stdout_fd)?,
            InputEvent::PrevTerminal => self.switch_relative(-1, stdout_fd)?,
            InputEvent::ShowHelp => self.show_help(stdout_fd)?,
            InputEvent::PrefixEscape => {
                if let Some(ref id) = self.active_session {
                    let prefix = vec![self.config.prefix_key];
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(id) {
                        session.send_raw(&prefix)?;
                    }
                }
            }
            InputEvent::Detach => return Ok(false),
            InputEvent::CreateTerminal => {
                self.create_session(stdout_fd)?;
            }
            InputEvent::Reload => {
                self.reload_requested = true;
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn switch_to_session(&mut self, id: &str, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let exists = self.sessions.lock().unwrap().exists(id);
        if exists {
            self.active_session = Some(id.to_string());
            write_all(stdout_fd, clear_screen().as_bytes())?;
            self.pane_renderer.invalidate();
            let render_data = {
                let mut mgr = self.sessions.lock().unwrap();
                if let Ok(session) = mgr.get_mut(id) {
                    let pane_output = self.pane_renderer.render(session.screen().screen());
                    let cursor = session.cursor_position();
                    Some((pane_output, cursor))
                } else { None }
            };
            if let Some((pane_output, (row, col))) = render_data {
                write_all(stdout_fd, &pane_output)?;
                self.render_sidebar(stdout_fd)?;
                let (tr, tc) = self.pane_renderer.cursor_terminal_position(row, col);
                write_all(stdout_fd, cursor_goto(tr, tc).as_bytes())?;
            } else {
                self.render_sidebar(stdout_fd)?;
            }
        }
        Ok(())
    }

    fn switch_relative(&mut self, delta: i32, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let current_idx = self.active_session.as_ref()
            .and_then(|id| self.session_order.iter().position(|s| s == id));
        if let Some(new_idx) = compute_relative_index(current_idx, delta, self.session_order.len()) {
            let id = self.session_order[new_idx].clone();
            self.switch_to_session(&id, stdout_fd)?;
        }
        Ok(())
    }

    fn show_help(&mut self, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let prefix_name = prefix_key_name(self.config.prefix_key);
        let help = format_help_screen(&prefix_name);
        write_all(stdout_fd, help.as_bytes())?;
        let stdin_fd = std::io::stdin().as_raw_fd();
        let mut buf = [0u8; 64];
        let _ = nix::unistd::read(stdin_fd, &mut buf);
        write_all(stdout_fd, clear_screen().as_bytes())?;
        self.pane_renderer.invalidate();
        // Can't call render here because pane_renderer is &self — need to redraw on next loop
        self.render_sidebar(stdout_fd)?;
        Ok(())
    }

    fn render_sidebar(&self, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let mgr = self.sessions.lock().unwrap();
        let sessions = mgr.list();
        drop(mgr);
        let reminders: Vec<String> = self.sidebar_messages.lock().unwrap().clone();
        let uptime_secs = self.server_start_time.elapsed().as_secs();
        let uptime = format!("Uptime: {}s", uptime_secs);
        let lines = self.sidebar.render_with_build_info(
            &sessions, self.active_session.as_deref(),
            self.screen_cols, self.screen_rows, &reminders,
            Some(&uptime),
        );
        for line in &lines {
            write_all(stdout_fd, line.content.as_bytes())?;
        }
        Ok(())
    }

    fn handle_resize(&mut self, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let (cols, rows) = terminal_size();
        self.screen_cols = cols;
        self.screen_rows = rows;
        let pty_cols = cols.saturating_sub(self.config.sidebar_width);
        let pty_rows = rows.saturating_sub(1);
        self.pane_renderer.resize(pty_cols, pty_rows);
        let resized_ids: Vec<String> = {
            let mut mgr = self.sessions.lock().unwrap();
            let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
            for id in &ids {
                if let Ok(session) = mgr.get_mut(id) {
                    let _ = session.resize(pty_cols, pty_rows);
                }
            }
            ids
        };
        let resize_data = format!(
            r#"{{"type":"resize","cols":{},"rows":{}}}"#,
            pty_cols, pty_rows
        ).into_bytes();
        for id in &resized_ids {
            let _ = self.logger.log_event(&LogEvent::new(
                id.clone(), Direction::Meta, resize_data.clone(),
            ));
        }
        write_all(stdout_fd, clear_screen().as_bytes())?;
        if let Some(ref id) = self.active_session.clone() {
            let render_data = {
                let mut mgr = self.sessions.lock().unwrap();
                if let Ok(session) = mgr.get_mut(id) {
                    let pane_output = self.pane_renderer.render(session.screen().screen());
                    let cursor = session.cursor_position();
                    Some((pane_output, cursor))
                } else {
                    None
                }
            };
            if let Some((pane_output, (row, col))) = render_data {
                write_all(stdout_fd, &pane_output)?;
                self.render_sidebar(stdout_fd)?;
                let (tr, tc) = self.pane_renderer.cursor_terminal_position(row, col);
                write_all(stdout_fd, cursor_goto(tr, tc).as_bytes())?;
                return Ok(());
            }
        }
        self.render_sidebar(stdout_fd)?;
        Ok(())
    }

    fn check_session_exit(&mut self) -> bool {
        if let Some(ref id) = self.active_session {
            let mgr = self.sessions.lock().unwrap();
            let exited = mgr.get(id).map_or(false, |s| matches!(s.status(), SessionStatus::Exited(_)));
            if exited {
                let id_owned = id.clone();
                let next = self.session_order.iter()
                    .find(|s| *s != id && mgr.get(s).map_or(false, |sess| matches!(sess.status(), SessionStatus::Running)))
                    .cloned();
                drop(mgr);
                if let Some(ref logger) = self.sqlite_logger {
                    let _ = logger.lock().unwrap().log_session_end(&id_owned);
                }
                if let Some(next_id) = next {
                    self.active_session = Some(next_id);
                } else {
                    return true;
                }
            }
        }
        false
    }

    fn handle_scheduler_event(&mut self, event: SchedulerEvent) {
        match event {
            SchedulerEvent::ReminderFired(reminder) => {
                let _ = self.logger.log_event(&LogEvent::new(
                    "scheduler".to_string(), Direction::Meta,
                    format!("REMINDER: {}", reminder.message).into_bytes(),
                ));
                // Inject the reminder message into the active session (or first session).
                let target = self.active_session.clone().or_else(|| {
                    self.session_order.first().cloned()
                });
                if let Some(sid) = target {
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(&sid) {
                        let text = format!("\n[REMINDER: {}]\r", reminder.message);
                        let _ = session.send_raw(text.as_bytes());
                    }
                }
            }
            SchedulerEvent::CronFired(job) => {
                if let Some(ref session_id) = job.session_id {
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(session_id) {
                        let _ = session.send_keys(&job.command);
                    }
                }
            }
        }
    }

    // === MCP proxy management ===

    fn accept_mcp_connections(&mut self) {
        if let Some(ref listener) = self.mcp_listener {
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        // Each MCP proxy client gets its own thread with a
                        // CompositeToolHandler backed by the shared session manager.
                        let sessions = self.sessions.clone();
                        let notifications = self.notifications.clone();
                        let scheduler = self.scheduler.clone();
                        let scratchpad = self.scratchpad.clone();
                        let sidebar_messages = self.sidebar_messages.clone();
                        let work_dir = self.config.work_dir.clone();
                        let db_path = self.config.db_path.clone();
                        let sqlite_logger = self.sqlite_logger.clone();
                        std::thread::spawn(move || {
                            use tttt_mcp::proxy::handle_proxy_client;
                            use tttt_mcp::{PtyToolHandler, ReplayToolHandler, SchedulerToolHandler, NotificationToolHandler, ScratchpadToolHandler, SidebarMessageToolHandler, CompositeToolHandler};

                            // Set the stream to blocking mode for the handler
                            let _ = stream.set_nonblocking(false);

                            let pty_handler = PtyToolHandler::new(sessions.clone(), work_dir)
                                .with_sqlite_logger(sqlite_logger);
                            let scheduler_handler = SchedulerToolHandler::new(scheduler);
                            let notif_handler = NotificationToolHandler::new(notifications, sessions);
                            let scratchpad_handler = ScratchpadToolHandler::new_shared(scratchpad);
                            let sidebar_handler = SidebarMessageToolHandler::new(sidebar_messages);
                            let replay_handler = ReplayToolHandler::new(db_path);
                            let mut composite = CompositeToolHandler::new();
                            composite.add_handler(Box::new(pty_handler));
                            composite.add_handler(Box::new(scheduler_handler));
                            composite.add_handler(Box::new(notif_handler));
                            composite.add_handler(Box::new(scratchpad_handler));
                            composite.add_handler(Box::new(sidebar_handler));
                            composite.add_handler(Box::new(replay_handler));

                            if let Err(e) = handle_proxy_client(stream, &mut composite, "tttt") {
                                // Client disconnected or error — normal
                                let _ = e;
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }

    // === Viewer client management ===

    fn accept_viewer_connections(&mut self) {
        if let Some(ref listener) = self.viewer_listener {
            loop {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        // Get the PTY dimensions from the active session
                        let (pty_cols, pty_rows) = {
                            let mgr = self.sessions.lock().unwrap();
                            self.active_session
                                .as_ref()
                                .and_then(|id| mgr.get(id).ok())
                                .map(|s| {
                                    let m = s.metadata();
                                    (m.cols, m.rows)
                                })
                                .unwrap_or((
                                    self.screen_cols.saturating_sub(self.config.sidebar_width),
                                    self.screen_rows.saturating_sub(1),
                                ))
                        };

                        let mut client = ViewerClient::new(
                            stream,
                            pty_cols + self.config.sidebar_width, // total cols
                            pty_rows + 1, // total rows
                            self.config.sidebar_width,
                        );
                        // Override renderer to match actual PTY dimensions
                        client.renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
                        client.active_session = self.active_session.clone();
                        client.invalidate();
                        let _ = self.logger.log_event(&LogEvent::new(
                            "viewer".to_string(), Direction::Meta,
                            format!("ACCEPT: viewer connected, active_session={:?}, pty={}x{}", self.active_session, pty_cols, pty_rows).into_bytes(),
                        ));
                        self.viewer_clients.push(client);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }

    fn process_viewer_input(&mut self, _stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        for i in 0..self.viewer_clients.len() {
            if !self.viewer_clients[i].connected {
                continue;
            }
            self.viewer_clients[i].read_available();

            // Process all complete messages in the buffer
            loop {
                let buf = &self.viewer_clients[i].read_buf;
                if let Some((msg, consumed)) = protocol::decode_message::<protocol::ClientMsg>(buf)
                {
                    self.viewer_clients[i].read_buf.drain(..consumed);
                    match msg {
                        protocol::ClientMsg::KeyInput { bytes } => {
                            let _ = self.logger.log_event(&LogEvent::new(
                                "viewer".to_string(), Direction::Meta,
                                format!("INPUT: KeyInput len={}, active_session={:?}", bytes.len(), self.viewer_clients[i].active_session).into_bytes(),
                            ));
                            // Forward keystrokes to the viewer's active session
                            if let Some(ref sid) = self.viewer_clients[i].active_session.clone() {
                                let mut mgr = self.sessions.lock().unwrap();
                                if let Ok(session) = mgr.get_mut(sid) {
                                    let _ = session.send_raw(&bytes);
                                }
                            }
                        }
                        protocol::ClientMsg::SwitchSession { session_id } => {
                            if self.sessions.lock().unwrap().exists(&session_id) {
                                self.viewer_clients[i].active_session =
                                    Some(session_id);
                                self.viewer_clients[i].invalidate();
                            }
                        }
                        protocol::ClientMsg::Resize { cols, rows } => {
                            let _ = self.logger.log_event(&LogEvent::new(
                                "viewer".to_string(), Direction::Meta,
                                format!("RESIZE: cols={}, rows={}", cols, rows).into_bytes(),
                            ));
                            // cols = usable PTY width reported by client
                            // (client subtracts its own sidebar if it has one)
                            self.viewer_clients[i].cols = cols;
                            self.viewer_clients[i].rows = rows;
                            let pty_rows = rows.saturating_sub(1);
                            self.viewer_clients[i].renderer.resize(cols, pty_rows);
                            self.viewer_clients[i].invalidate();
                            // Resize PTY to minimum across all clients (tmux behavior)
                            self.resize_pty_to_min_and_redraw(_stdout_fd);
                        }
                        protocol::ClientMsg::Detach => {
                            self.viewer_clients[i].send_goodbye();
                        }
                    }
                } else {
                    break;
                }
            }
        }

        // Remove disconnected clients and resize if any were removed
        let count_before = self.viewer_clients.len();
        self.viewer_clients.retain(|c| c.connected);
        if self.viewer_clients.len() < count_before {
            // A client disconnected — resize PTY back up if possible
            self.resize_pty_to_min_and_redraw(_stdout_fd);
        }

        Ok(())
    }

    /// Resize the PTY to the minimum size across the main terminal and all connected viewers.
    /// Clears the main terminal and forces a full redraw to remove stale content.
    fn resize_pty_to_min_and_redraw(&mut self, stdout_fd: i32) {
        let sidebar = self.config.sidebar_width;
        // The PTY can never be larger than the main terminal's usable area
        let max_pty_cols = self.screen_cols.saturating_sub(sidebar);
        let max_pty_rows = self.screen_rows.saturating_sub(1);
        
        // Start with main terminal size as the baseline
        let mut min_cols = max_pty_cols;
        let mut min_rows = max_pty_rows;

        // Find minimum across all connected viewers.
        // Attach clients don't have a sidebar, so use their cols directly
        // as usable PTY width (no sidebar subtraction).
        for client in &self.viewer_clients {
            if !client.connected {
                continue;
            }
            let c = client.cols; // no sidebar on attach clients
            let r = client.rows.saturating_sub(1);
            min_cols = min_cols.min(c);
            min_rows = min_rows.min(r);
        }

        // Clamp to maximum PTY size (main terminal's usable area)
        // This ensures the PTY never grows larger than what the main window can display
        min_cols = min_cols.min(max_pty_cols);
        min_rows = min_rows.min(max_pty_rows);

        // Check if dimensions actually changed
        let (old_cols, old_rows) = self.pane_renderer.dimensions();
        let changed = min_cols != old_cols || min_rows != old_rows;

        if changed {
            // Resize all sessions (ScreenBuffer::resize is a no-op for same dimensions)
            let mut mgr = self.sessions.lock().unwrap();
            let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
            for id in ids {
                if let Ok(session) = mgr.get_mut(&id) {
                    let _ = session.resize(min_cols, min_rows);
                }
            }
            drop(mgr);

            // Resize main pane renderer and invalidate for full redraw
            self.pane_renderer.resize(min_cols, min_rows);

            // Clear main terminal to remove stale content from the old wider PTY
            let _ = write_all(stdout_fd, clear_screen().as_bytes());

            // Force full redraw of active session on main terminal
            {
                let mut mgr = self.sessions.lock().unwrap();
                if let Some(ref id) = self.active_session.clone() {
                    if let Ok(session) = mgr.get_mut(id) {
                        let pane_output = self.pane_renderer.render(session.screen().screen());
                        let _ = write_all(stdout_fd, &pane_output);
                    }
                }
            }
            // Fill gap between PTY area and sidebar with gray dots
            let max_pty_cols = self.screen_cols.saturating_sub(self.config.sidebar_width);
            if min_cols < max_pty_cols {
                let dot_attr = "\x1b[2;90m"; // dim + gray
                let reset = "\x1b[0m";
                let dots: String = ".".repeat((max_pty_cols - min_cols) as usize);
                for row in 0..min_rows {
                    let _ = write_all(
                        stdout_fd,
                        format!(
                            "\x1b[{};{}H{}{}{}",
                            row + 1,
                            min_cols + 1,
                            dot_attr,
                            dots,
                            reset
                        )
                        .as_bytes(),
                    );
                }
            }

            let _ = self.render_sidebar(stdout_fd);
        }

        // Always resize and invalidate viewer renderers so they get a fresh update
        // Also notify clients of the new virtual window size
        for client in &mut self.viewer_clients {
            client.renderer.resize(min_cols, min_rows);
            client.invalidate();
            // Send window size update to client
            client.send_window_size(min_cols, min_rows);
        }
    }

    fn update_viewers(&mut self) {
        let mgr = self.sessions.lock().unwrap();
        for client in &mut self.viewer_clients {
            if !client.connected {
                continue;
            }
            if let Some(ref sid) = client.active_session.clone() {
                if let Ok(session) = mgr.get(sid) {
                    let (row, col) = session.cursor_position();
                    let screen = session.screen().screen();
                    let screen_data_len = screen.contents_formatted().len();
                    let sent = client.send_screen_update(screen, row, col);
                    let _ = self.logger.log_event(&LogEvent::new(
                        "viewer".to_string(), Direction::Meta,
                        format!("UPDATE: sid={}, sent={}, screen_data_len={}, cursor=({},{})", sid, sent, screen_data_len, row, col).into_bytes(),
                    ));
                } else {
                    let _ = self.logger.log_event(&LogEvent::new(
                        "viewer".to_string(), Direction::Meta,
                        format!("UPDATE: session {} not found!", sid).into_bytes(),
                    ));
                }
            } else {
                let _ = self.logger.log_event(&LogEvent::new(
                    "viewer".to_string(), Direction::Meta,
                    "UPDATE: no active_session!".to_string().into_bytes(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Chunk 1: prefix_key_name / format_help_screen ────────────────────────

    #[test]
    fn test_prefix_key_name_ctrl_backslash() {
        assert_eq!(prefix_key_name(0x1c), "Ctrl+\\");
    }

    #[test]
    fn test_prefix_key_name_ctrl_a() {
        assert_eq!(prefix_key_name(0x01), "Ctrl+A");
    }

    #[test]
    fn test_prefix_key_name_ctrl_b() {
        assert_eq!(prefix_key_name(0x02), "Ctrl+B");
    }

    #[test]
    fn test_prefix_key_name_unknown_key() {
        assert_eq!(prefix_key_name(0x05), "0x05");
        assert_eq!(prefix_key_name(0xff), "0xff");
    }

    #[test]
    fn test_format_help_screen_contains_prefix() {
        let help = format_help_screen("Ctrl+A");
        assert!(help.contains("Ctrl+A"), "help screen should contain the prefix name");
    }

    #[test]
    fn test_format_help_screen_contains_keybindings() {
        let help = format_help_screen("Ctrl+\\");
        assert!(help.contains("Switch to terminal"), "should mention switching");
        assert!(help.contains("Next terminal"), "should mention next");
        assert!(help.contains("Previous terminal"), "should mention prev");
        assert!(help.contains("Detach/quit"), "should mention detach");
        assert!(help.contains("Live reload"), "should mention reload");
        assert!(help.contains("This help"), "should mention help");
        assert!(help.contains("Press any key"), "should mention dismiss");
    }

    // ── Chunk 3: reconcile_session_order ─────────────────────────────────────

    fn ss(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn test_reconcile_empty_current_returns_actual() {
        assert_eq!(reconcile_session_order(&ss(&[]), &ss(&["a", "b"])), ss(&["a", "b"]));
    }

    #[test]
    fn test_reconcile_empty_actual_returns_empty() {
        assert_eq!(reconcile_session_order(&ss(&["a", "b"]), &ss(&[])), ss(&[]));
    }

    #[test]
    fn test_reconcile_preserves_existing_order() {
        let result = reconcile_session_order(&ss(&["b", "a"]), &ss(&["a", "b"]));
        assert_eq!(result, ss(&["b", "a"]));
    }

    #[test]
    fn test_reconcile_appends_new_ids() {
        let result = reconcile_session_order(&ss(&["a"]), &ss(&["a", "b", "c"]));
        assert_eq!(result, ss(&["a", "b", "c"]));
    }

    #[test]
    fn test_reconcile_removes_stale_ids() {
        let result = reconcile_session_order(&ss(&["a", "b", "c"]), &ss(&["a", "c"]));
        assert_eq!(result, ss(&["a", "c"]));
    }

    #[test]
    fn test_reconcile_add_and_remove_simultaneously() {
        let result = reconcile_session_order(&ss(&["a", "b"]), &ss(&["b", "c"]));
        assert_eq!(result, ss(&["b", "c"]));
    }

    // ── Chunk 2: compute_relative_index ──────────────────────────────────────

    #[test]
    fn test_compute_relative_index_empty_returns_none() {
        assert_eq!(compute_relative_index(None, 1, 0), None);
        assert_eq!(compute_relative_index(Some(0), 1, 0), None);
    }

    #[test]
    fn test_compute_relative_index_single_element() {
        assert_eq!(compute_relative_index(Some(0), 1, 1), Some(0));
        assert_eq!(compute_relative_index(Some(0), -1, 1), Some(0));
    }

    #[test]
    fn test_compute_relative_index_forward() {
        assert_eq!(compute_relative_index(Some(0), 1, 3), Some(1));
        assert_eq!(compute_relative_index(Some(1), 1, 3), Some(2));
    }

    #[test]
    fn test_compute_relative_index_forward_wrap() {
        assert_eq!(compute_relative_index(Some(2), 1, 3), Some(0));
    }

    #[test]
    fn test_compute_relative_index_backward() {
        assert_eq!(compute_relative_index(Some(2), -1, 3), Some(1));
        assert_eq!(compute_relative_index(Some(1), -1, 3), Some(0));
    }

    #[test]
    fn test_compute_relative_index_backward_wrap() {
        assert_eq!(compute_relative_index(Some(0), -1, 3), Some(2));
    }

    #[test]
    fn test_compute_relative_index_none_current_treated_as_zero() {
        assert_eq!(compute_relative_index(None, 1, 3), Some(1));
        assert_eq!(compute_relative_index(None, -1, 3), Some(2));
    }

    #[test]
    fn test_format_help_screen_prefix_appears_twice_for_literal_prefix() {
        // The "send literal prefix" line shows "prefix prefix" (e.g. "Ctrl+A Ctrl+A")
        let help = format_help_screen("XX");
        let count = help.matches("XX").count();
        assert!(count >= 2, "prefix should appear at least twice (label + literal-prefix entry)");
    }
}
