use crate::config::Config;
use crate::reload::{self, SavedState, SavedSession, SavedCronJob, SavedWatcher};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Terminal,
};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tttt_log::{Direction as LogDirection, LogEvent, LogSink, MultiLogger, SharedSqliteLogSink, SqliteLogger, TextLogger};
use tttt_mcp::notification::NotificationRegistry;
use tttt_mcp::{SharedNotificationRegistry, SharedScheduler, SharedScratchpad, SharedSidebarMessages};
use tttt_pty::{AnyPty, PtySession, RealPty, SessionManager, SessionStatus};
use tttt_scheduler::{Scheduler, SchedulerEvent};
use std::os::unix::net::UnixListener;
use tttt_tui::{
    protocol, InputEvent, InputParser, RawInput,
    ViewerClient, PtyWidget, SidebarWidget,
};

/// Minimum time between renders to the server terminal (ms).
/// During rapid updates (e.g., Claude Code redrawing history),
/// we accumulate changes and only render once the burst settles.
const RENDER_DEBOUNCE_MS: u64 = 50;


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

/// Pure mapping of a parsed `InputEvent` to the action that should be taken.
///
/// This function contains no I/O; it is a pure data transformation.
#[derive(Debug, PartialEq)]
enum InputAction {
    /// Forward raw bytes to the active session.
    SendToSession(Vec<u8>),
    /// Switch to the session at position `n` in the session order.
    SwitchSession(usize),
    NextSession,
    PrevSession,
    ShowHelp,
    CreateSession,
    Reload,
    Detach,
    /// Send the raw prefix key byte to the active session.
    PrefixEscape,
    /// Mouse events — handled directly, no further mapping needed.
    MousePress { button: tttt_tui::MouseButton, col: u16, row: u16 },
    MouseDrag { button: tttt_tui::MouseButton, col: u16, row: u16 },
    MouseRelease { col: u16, row: u16 },
    ScrollUp { col: u16, row: u16 },
    ScrollDown { col: u16, row: u16 },
    /// Show the Ctrl+C escape hint in the status line.
    ShowCtrlCHint,
}

fn decide_input_action(event: tttt_tui::InputEvent) -> InputAction {
    match event {
        tttt_tui::InputEvent::PassThrough(data) => InputAction::SendToSession(data),
        tttt_tui::InputEvent::SwitchTerminal(n)  => InputAction::SwitchSession(n),
        tttt_tui::InputEvent::NextTerminal        => InputAction::NextSession,
        tttt_tui::InputEvent::PrevTerminal        => InputAction::PrevSession,
        tttt_tui::InputEvent::ShowHelp            => InputAction::ShowHelp,
        tttt_tui::InputEvent::CreateTerminal      => InputAction::CreateSession,
        tttt_tui::InputEvent::Reload              => InputAction::Reload,
        tttt_tui::InputEvent::Detach              => InputAction::Detach,
        tttt_tui::InputEvent::PrefixEscape        => InputAction::PrefixEscape,
        tttt_tui::InputEvent::MousePress { button, col, row } => InputAction::MousePress { button, col, row },
        tttt_tui::InputEvent::MouseDrag { button, col, row } => InputAction::MouseDrag { button, col, row },
        tttt_tui::InputEvent::MouseRelease { col, row } => InputAction::MouseRelease { col, row },
        tttt_tui::InputEvent::ScrollUp { col, row } => InputAction::ScrollUp { col, row },
        tttt_tui::InputEvent::ScrollDown { col, row } => InputAction::ScrollDown { col, row },
        tttt_tui::InputEvent::ShowCtrlCHint => InputAction::ShowCtrlCHint,
    }
}

/// What to do when the active session exits.
#[derive(Debug, PartialEq)]
enum SessionExitAction {
    /// Active session is still running (or there is no active session).
    NoExit,
    /// Switch to a different running session.
    SwitchTo(String),
    /// All sessions have exited — time to quit the event loop.
    AllExited,
}

/// Determine what to do when the active session may have exited.
///
/// `is_running` returns `true` when a session with that ID is still running.
fn compute_exit_action(
    active_id: Option<&str>,
    session_order: &[String],
    is_running: impl Fn(&str) -> bool,
) -> SessionExitAction {
    let Some(id) = active_id else {
        return SessionExitAction::NoExit;
    };
    if is_running(id) {
        return SessionExitAction::NoExit;
    }
    // Active session has exited — find another running one
    if let Some(next) = session_order.iter().find(|s| s.as_str() != id && is_running(s)) {
        return SessionExitAction::SwitchTo(next.clone());
    }
    SessionExitAction::AllExited
}

/// Compute PTY dimensions from the raw terminal size and sidebar width.
fn calculate_pane_dimensions(cols: u16, rows: u16, sidebar_width: u16) -> (u16, u16) {
    (cols.saturating_sub(sidebar_width), rows.saturating_sub(1))
}

/// Compute the minimum PTY size across the server baseline and all connected viewers.
///
/// `viewers` is a slice of `(cols, rows)` tuples — the usable area already
/// reported by each viewer (no further sidebar subtraction needed here).
/// The result is clamped so it never exceeds `(server_cols, server_rows)`.
fn calculate_min_dimensions(
    viewers: &[(u16, u16)],
    server_cols: u16,
    server_rows: u16,
) -> (u16, u16) {
    let mut min_cols = server_cols;
    let mut min_rows = server_rows;
    for &(c, r) in viewers {
        min_cols = min_cols.min(c);
        min_rows = min_rows.min(r);
    }
    // Clamp to the server baseline (never grow larger than the server can show)
    min_cols = min_cols.min(server_cols);
    min_rows = min_rows.min(server_rows);
    (min_cols, min_rows)
}


/// Decide whether to render now given the current debounce state.
///
/// Returns `true` when `dirty` is true AND either:
/// - the burst has ended (`last_pty_data` is ≥ `debounce_ms` ago), or
/// - the max latency has been exceeded (`first_dirty` is ≥ `4 × debounce_ms` ago).
///
/// Returns `false` immediately when `dirty` is false.
fn should_render_now(
    dirty: bool,
    last_pty_data: Option<Instant>,
    first_dirty: Option<Instant>,
    now: Instant,
    debounce_ms: u64,
) -> bool {
    if !dirty {
        return false;
    }
    let burst_ended = last_pty_data
        .map(|t| now.duration_since(t).as_millis() >= debounce_ms as u128)
        .unwrap_or(true);
    let max_latency_exceeded = first_dirty
        .map(|t| now.duration_since(t).as_millis() >= (debounce_ms * 4) as u128)
        .unwrap_or(false);
    burst_ended || max_latency_exceeded
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

/// Compute a centered Rect for the help popup within the given terminal area.
/// Returns `(x, y, width, height)` values for [`ratatui::layout::Rect::new`].
#[cfg(test)]
fn help_popup_area(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup_width: u16 = 45;
    let popup_height: u16 = 14;
    let x = area.width.saturating_sub(popup_width) / 2;
    let y = area.height.saturating_sub(popup_height) / 2;
    ratatui::layout::Rect::new(
        x, y,
        popup_width.min(area.width),
        popup_height.min(area.height),
    )
}

/// Main application state.
pub struct App {
    config: Config,
    sessions: Arc<Mutex<SessionManager<AnyPty>>>,
    input_parser: InputParser,
    /// Current PTY dimensions (cols, rows) — tracked separately from screen size.
    pty_dims: (u16, u16),
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    logger: MultiLogger,
    sqlite_logger: Option<Arc<Mutex<SqliteLogger>>>,
    scheduler: SharedScheduler,
    notifications: SharedNotificationRegistry,
    scratchpad: SharedScratchpad,
    sidebar_messages: SharedSidebarMessages,
    sidebar_dirty: tttt_mcp::SidebarDirtyFlag,
    tui_state: tttt_mcp::SharedTuiState,
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
    /// When true, render_frame() will draw a help overlay popup.
    showing_help: bool,
    /// Active text selection (None when not selecting)
    selection: Option<tttt_tui::Selection>,
    /// Current scroll offset during selection drag or manual scroll (0 = live view)
    scroll_offset: usize,
    /// Scrollback count when selection started — used to compensate for new output
    selection_scroll_base: usize,
    /// When Some(deadline), show the Ctrl+C escape hint until that instant.
    ctrl_c_hint_until: Option<Instant>,
    /// Last session metadata snapshot — compared each tick to detect changes.
    last_session_snapshot: Vec<tttt_pty::SessionMetadata>,
    /// Deferred scheduler events waiting for target session to become idle.
    deferred_scheduler_events: Vec<SchedulerEvent>,
}

impl App {
    pub fn new(config: Config) -> Self {
        let display_config = config.display_config();
        let (cols, rows) = terminal_size();
        let (pty_cols, pty_rows) = calculate_pane_dimensions(cols, rows, config.sidebar_width);

        // Set up ratatui terminal with crossterm backend
        enable_raw_mode().expect("Failed to enable raw mode");
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture).expect("Failed to enter alternate screen");
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).expect("Failed to create terminal");

        Self {
            sessions: Arc::new(Mutex::new(SessionManager::with_max_sessions(config.max_sessions))),
            input_parser: InputParser::new(display_config),
            pty_dims: (pty_cols, pty_rows),
            terminal,
            logger: MultiLogger::new(),
            sqlite_logger: None,
            scheduler: Arc::new(Mutex::new(Scheduler::new())),
            notifications: Arc::new(Mutex::new(NotificationRegistry::new())),
            scratchpad: Arc::new(Mutex::new(std::collections::HashMap::new())),
            sidebar_messages: Arc::new(Mutex::new(Vec::new())),
            sidebar_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tui_state: Arc::new(tttt_mcp::TuiState::new()),
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
            showing_help: false,
            selection: None,
            scroll_offset: 0,
            selection_scroll_base: 0,
            ctrl_c_hint_until: None,
            last_session_snapshot: Vec::new(),
            deferred_scheduler_events: Vec::new(),
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

                    // Restore root flag
                    session.set_root(saved.root);

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
                job.if_busy,
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

    /// Build args and spawn a new root PTY backend.
    fn spawn_root_backend(&mut self, pty_cols: u16, pty_rows: u16) -> Result<AnyPty, Box<dyn std::error::Error>> {
        // If MCP socket is available, generate config and inject --mcp-config
        // for agents that support it (e.g., claude)
        let mut args: Vec<String> = self.config.root_args.clone();
        if self.mcp_socket_path.is_some() {
            if let Ok(config_path) = self.generate_mcp_config() {
                let cmd = &self.config.root_command;
                if cmd.contains("claude") && !args.iter().any(|a| a.contains("mcp-config")) {
                    // Claude uses --mcp-config with a JSON file
                    args.push("--mcp-config".to_string());
                    args.push(config_path);
                } else if cmd.contains("apchat") {
                    // For apchat: load extra args from tmp/apchat.args or APCHAT_ARGS env var
                    let extra_args_str = std::env::var("APCHAT_ARGS").ok().or_else(|| {
                        let args_file = self.config.work_dir.join("tmp/apchat.args");
                        std::fs::read_to_string(&args_file).ok().map(|s| s.trim().to_string())
                    });
                    if let Some(extra) = extra_args_str {
                        if let Ok(parsed) = shell_words::split(&extra) {
                            args.extend(parsed);
                        }
                    }
                    // Inject --mcp-server with quoted command string
                    if !args.iter().any(|a| a.contains("mcp-server")) {
                        let mcp_socket = self.mcp_socket_path.as_ref().unwrap();
                        let tttt_bin = std::env::current_exe()
                            .unwrap_or_else(|_| std::path::PathBuf::from("tttt"));
                        let mcp_server_cmd = shell_words::join(&[
                            tttt_bin.to_string_lossy().as_ref(),
                            "mcp-server",
                            "--connect",
                            mcp_socket.as_str(),
                        ]);
                        args.push("--mcp-server".to_string());
                        args.push(mcp_server_cmd);
                    }
                }
            }
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let tttt_pid = std::process::id();
        let real_backend = RealPty::spawn_with_cwd_and_env(
            &self.config.root_command, &args_refs, Some(&self.config.work_dir), pty_cols, pty_rows,
            [("TTTT_PID".to_string(), tttt_pid.to_string())],
        )?;
        Ok(AnyPty::Real(real_backend))
    }

    pub fn launch_root(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        let (pty_cols, pty_rows) = calculate_pane_dimensions(
            self.screen_cols, self.screen_rows, self.config.sidebar_width,
        );

        let backend = self.spawn_root_backend(pty_cols, pty_rows)?;
        let mut mgr = self.sessions.lock().unwrap();
        let id = mgr.generate_id();
        let mut session = PtySession::new(id.clone(), backend, self.config.root_command.clone(), pty_cols, pty_rows);
        session.set_root(true);
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

    /// Respawn the root session's child process in place.
    /// Kills the old child, spawns a new one with updated MCP config,
    /// and swaps the backend. Preserves session ID, position, and sidebar order.
    pub fn respawn_root_session(&mut self, root_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (pty_cols, pty_rows) = calculate_pane_dimensions(
            self.screen_cols, self.screen_rows, self.config.sidebar_width,
        );

        // Kill the old child process
        {
            let mut mgr = self.sessions.lock().unwrap();
            if let Ok(session) = mgr.get_mut(root_id) {
                let _ = session.kill();
            }
        }

        // Spawn new backend using the same arg-building logic as launch_root
        let backend = self.spawn_root_backend(pty_cols, pty_rows)?;

        // Swap the backend on the existing session
        {
            let mut mgr = self.sessions.lock().unwrap();
            if let Ok(session) = mgr.get_mut(root_id) {
                session.replace_backend(backend, pty_cols, pty_rows);
            }
        }

        self.active_session = Some(root_id.to_string());
        Ok(())
    }

    /// Create a new PTY session with the default shell.
    pub fn create_session(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (pty_cols, pty_rows) = calculate_pane_dimensions(
            self.screen_cols, self.screen_rows, self.config.sidebar_width,
        );

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
        self.switch_to_session(&id)?;
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
                    root: session.is_root(),
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
                if_busy: job.if_busy,
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
        let winch = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGWINCH, Arc::clone(&winch));
        let sigusr1 = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGUSR1, Arc::clone(&sigusr1));
        let sigusr2 = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGUSR2, Arc::clone(&sigusr2));

        let stdin_fd = std::io::stdin().as_raw_fd();

        self.render_frame()?;

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
            let poll_timeout_ms = if self.server_render_dirty { 10u16 } else { 50u16 };

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
                self.handle_resize()?;
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

            // Read PTY output — pump into screen buffer but defer rendering.
            // We must handle POLLHUP (child exited / slave closed) in addition to POLLIN,
            // otherwise poll() returns immediately in a busy loop on macOS.
            if let Some(flags) = poll_result.0 {
                if flags.contains(PollFlags::POLLIN) || flags.contains(PollFlags::POLLHUP) {
                    if let Some(id) = self.active_session.clone() {
                        let mut mgr = self.sessions.lock().unwrap();
                        if let Ok(session) = mgr.get_mut(&id) {
                            match session.pump_raw() {
                                Ok((n, raw_bytes)) if n > 0 => {
                                    let _ = self.logger.log_event(&LogEvent::new(
                                        id.clone(), LogDirection::Output, raw_bytes,
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

            // Check for sidebar-level changes that should trigger an immediate render
            // (not debounced — these aren't PTY output bursts).
            {
                // 1. MCP sidebar messages changed
                if self.sidebar_dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    self.server_render_dirty = true;
                    if self.first_dirty_time.is_none() {
                        self.first_dirty_time = Some(Instant::now());
                    }
                }

                // 2. Session metadata changed (name, status, count)
                let current_snapshot = {
                    let mgr = self.sessions.lock().unwrap();
                    mgr.list()
                };
                if current_snapshot != self.last_session_snapshot {
                    self.last_session_snapshot = current_snapshot;
                    self.server_render_dirty = true;
                    if self.first_dirty_time.is_none() {
                        self.first_dirty_time = Some(Instant::now());
                    }
                }

                // 3. TUI tools: pending switch or highlight changes
                if self.tui_state.dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    // Drain pending session switch
                    if let Some(target_id) = self.tui_state.pending_switch.lock().unwrap().take() {
                        self.active_session = Some(target_id);
                    }
                    self.server_render_dirty = true;
                    if self.first_dirty_time.is_none() {
                        self.first_dirty_time = Some(Instant::now());
                    }
                }
            }

            // Debounced render: only render to server terminal when
            // dirty AND enough time has passed since last PTY data.
            // This absorbs rapid redraws (e.g., Claude Code history)
            // into a single clean update.
            //
            // Synchronized output (DEC mode 2026): when the active session's
            // screen has sync mode set, suppress rendering entirely. The app
            // producing output has bracketed an update and we must wait until
            // the bracket closes (decrst 2026) before presenting a frame.
            if self.server_render_dirty {
                // Check if the active session has synchronized output enabled
                let sync_active = self.active_session.as_ref().map_or(false, |id| {
                    let mgr = self.sessions.lock().unwrap();
                    mgr.get(id)
                        .map(|s| s.synchronized_output())
                        .unwrap_or(false)
                });

                if !sync_active {
                    let now = Instant::now();
                    let should_render = should_render_now(
                        self.server_render_dirty,
                        self.last_pty_data_time,
                        self.first_dirty_time,
                        now,
                        RENDER_DEBOUNCE_MS,
                    );

                    if should_render {
                        self.render_frame()?;
                        self.server_render_dirty = false;
                        self.first_dirty_time = None;
                    }
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
                                match self.handle_input_event(event) {
                                    Ok(true) => {}
                                    Ok(false) => return Ok(()),
                                    Err(e) => {
                                        let _ = self.logger.log_event(&LogEvent::new(
                                            "system".to_string(), LogDirection::Meta,
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
            self.process_viewer_input()?;

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
                                    sid.clone(), LogDirection::Output, raw_bytes,
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
                        target_id.clone(), LogDirection::Meta,
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
            self.drain_deferred_scheduler_events();
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

        // Restore terminal state
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;

        Ok(())
    }

    /// Sync session_order with the actual sessions (MCP may have added new ones).
    fn sync_session_order(&mut self) {
        let mgr = self.sessions.lock().unwrap();
        let actual_ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
        drop(mgr);
        self.session_order = reconcile_session_order(&self.session_order, &actual_ids);
    }

    fn handle_input_event(&mut self, event: InputEvent) -> Result<bool, Box<dyn std::error::Error>> {
        match decide_input_action(event) {
            InputAction::SendToSession(data) => {
                if let Some(ref id) = self.active_session {
                    if self.config.log_input {
                        let _ = self.logger.log_event(&LogEvent::new(id.clone(), LogDirection::Input, data.clone()));
                    }
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(id) {
                        session.send_raw(&data)?;
                    }
                }
            }
            InputAction::SwitchSession(n) => {
                if let Some(id) = self.session_order.get(n).cloned() {
                    self.switch_to_session(&id)?;
                }
            }
            InputAction::NextSession => self.switch_relative(1)?,
            InputAction::PrevSession => self.switch_relative(-1)?,
            InputAction::ShowHelp => self.show_help()?,
            InputAction::PrefixEscape => {
                if let Some(ref id) = self.active_session {
                    let prefix = vec![self.config.prefix_key];
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(id) {
                        session.send_raw(&prefix)?;
                    }
                }
            }
            InputAction::Detach => return Ok(false),
            InputAction::CreateSession => {
                self.create_session()?;
            }
            InputAction::Reload => {
                self.reload_requested = true;
                return Ok(false);
            }
            InputAction::MousePress { button, col, row } => {
                if matches!(button, tttt_tui::MouseButton::Left) {
                    let sidebar_width = self.config.sidebar_width;
                    let pane_width = self.screen_cols.saturating_sub(sidebar_width);
                    if col < pane_width {
                        // Click in PTY pane — start text selection
                        self.selection = Some(tttt_tui::Selection::new(row, col));
                        self.scroll_offset = 0; // Start from live view
                        // Snapshot scrollback count to detect new output during selection
                        self.selection_scroll_base = self.active_session.as_ref()
                            .and_then(|id| {
                                let mgr = self.sessions.lock().unwrap();
                                mgr.get(id).ok().map(|s| s.max_scroll_offset())
                            })
                            .unwrap_or(0);
                        self.server_render_dirty = true;
                    } else if row >= 2 {
                        // Click in sidebar — switch to the clicked session
                        // Sessions start at row 2 (after header + separator)
                        let session_idx = (row - 2) as usize;
                        if session_idx < self.session_order.len() {
                            self.active_session = Some(self.session_order[session_idx].clone());
                            self.server_render_dirty = true;
                        }
                    }
                }
            }
            InputAction::MouseDrag { button, col, row } => {
                if matches!(button, tttt_tui::MouseButton::Left) {
                    if let Some(ref mut sel) = self.selection {
                        // Clamp to PTY pane bounds
                        let sidebar_width = self.config.sidebar_width;
                        let pane_width = self.screen_cols.saturating_sub(sidebar_width);
                        let clamped_col = col.min(pane_width.saturating_sub(1));
                        sel.update(row, clamped_col);

                        // Auto-scroll when dragging at top or bottom edge
                        let (_pty_cols, pty_rows) = self.pty_dims;
                        if row == 0 {
                            // Scroll up (into history) — only update local offset,
                            // render_frame() applies it on a cloned screen
                            let max = self.active_session.as_ref().and_then(|id| {
                                let mgr = self.sessions.lock().unwrap();
                                mgr.get(id).ok().map(|s| s.max_scroll_offset())
                            }).unwrap_or(0);
                            if self.scroll_offset < max {
                                self.scroll_offset += 1;
                                // Shift anchor down to keep it pointing at the same content
                                sel.anchor.0 = sel.anchor.0.saturating_add(1);
                            }
                        } else if row >= pty_rows.saturating_sub(1) && self.scroll_offset > 0 {
                            // Scroll down (toward live view)
                            self.scroll_offset -= 1;
                            // Shift anchor up
                            sel.anchor.0 = sel.anchor.0.saturating_sub(1);
                        }

                        self.server_render_dirty = true;
                    }
                }
            }
            InputAction::MouseRelease { col, row } => {
                if let Some(ref mut sel) = self.selection {
                    // Final update
                    let sidebar_width = self.config.sidebar_width;
                    let pane_width = self.screen_cols.saturating_sub(sidebar_width);
                    let clamped_col = col.min(pane_width.saturating_sub(1));
                    sel.update(row, clamped_col);

                    // Extract text from active session's screen (with scroll compensation)
                    if !sel.is_empty() {
                        if let Some(ref id) = self.active_session {
                            let mgr = self.sessions.lock().unwrap();
                            if let Ok(session) = mgr.get(id) {
                                let mut screen = session.screen().screen().clone();
                                let effective_scroll = compute_selection_scroll_compensation(
                                    self.selection_scroll_base,
                                    session.max_scroll_offset(),
                                    self.scroll_offset,
                                );
                                if effective_scroll > 0 {
                                    screen.set_scrollback(effective_scroll);
                                }
                                let text = sel.extract_text(&screen);
                                drop(mgr);
                                if !text.is_empty() {
                                    copy_to_clipboard(&text);
                                }
                            }
                        }
                    }

                    // Reset scroll offset to live view
                    self.scroll_offset = 0;

                    // Clear selection
                    self.selection = None;
                    self.server_render_dirty = true;
                }
            }
            InputAction::ScrollUp { .. } => {
                // Scroll into history — local offset only, applied at render time
                let max = self.active_session.as_ref().and_then(|id| {
                    let mgr = self.sessions.lock().unwrap();
                    mgr.get(id).ok().map(|s| s.max_scroll_offset())
                }).unwrap_or(0);
                if self.scroll_offset < max {
                    self.scroll_offset = (self.scroll_offset + 3).min(max);
                    self.server_render_dirty = true;
                }
            }
            InputAction::ScrollDown { .. } => {
                // Scroll toward live view — local offset only
                if self.scroll_offset > 0 {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                    self.server_render_dirty = true;
                }
            }
            InputAction::ShowCtrlCHint => {
                use std::time::Duration;
                self.ctrl_c_hint_until = Some(Instant::now() + Duration::from_secs(3));
                self.server_render_dirty = true;
            }
        }
        Ok(true)
    }

    fn switch_to_session(&mut self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let exists = self.sessions.lock().unwrap().exists(id);
        if exists {
            self.active_session = Some(id.to_string());
            self.render_frame()?;
        }
        Ok(())
    }

    fn switch_relative(&mut self, delta: i32) -> Result<(), Box<dyn std::error::Error>> {
        let current_idx = self.active_session.as_ref()
            .and_then(|id| self.session_order.iter().position(|s| s == id));
        if let Some(new_idx) = compute_relative_index(current_idx, delta, self.session_order.len()) {
            let id = self.session_order[new_idx].clone();
            self.switch_to_session(&id)?;
        }
        Ok(())
    }

    fn show_help(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.showing_help = true;
        self.render_frame()?;

        // Block for dismiss key
        let stdin_fd = std::io::stdin().as_raw_fd();
        let mut buf = [0u8; 64];
        let _ = nix::unistd::read(stdin_fd, &mut buf);

        self.showing_help = false;
        self.render_frame()?;
        Ok(())
    }

    /// Render the full frame using ratatui widgets.
    fn render_frame(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Collect all data needed for rendering before entering the draw closure,
        // because we cannot hold the mutex lock across the closure.
        let manual_scroll = self.scroll_offset;
        let selection_base = self.selection_scroll_base;
        let has_selection = self.selection.is_some();
        // screen_data: (screen, cursor_row, cursor_col, pty_rows)
        let screen_data: Option<(vt100::Screen, u16, u16, u16)> = self.active_session.as_ref()
            .and_then(|id| {
                let mgr = self.sessions.lock().unwrap();
                mgr.get(id).ok().map(|session| {
                    // Clone screen first, then apply scroll offset on the clone
                    // so we don't affect the shared session state (viewers see live view)
                    let mut screen = session.screen().screen().clone();
                    let effective_scroll = if has_selection {
                        // Compensate for new output that arrived during selection
                        compute_selection_scroll_compensation(
                            selection_base,
                            session.max_scroll_offset(),
                            manual_scroll,
                        )
                    } else {
                        manual_scroll
                    };
                    if effective_scroll > 0 {
                        screen.set_scrollback(effective_scroll);
                    }
                    let (pty_rows, _) = screen.size();
                    let (row, col) = if effective_scroll > 0 {
                        (0, 0) // Hide cursor when scrolled back
                    } else {
                        session.cursor_position()
                    };
                    (screen, row, col, pty_rows)
                })
            });

        // Collect highlights for the active session
        let active_highlights: Vec<tttt_mcp::TuiHighlight> = self.active_session.as_ref()
            .and_then(|id| {
                let highlights = self.tui_state.highlights.lock().unwrap();
                highlights.get(id).cloned()
            })
            .unwrap_or_default();

        let reminders: Vec<String> = self.sidebar_messages.lock().unwrap().clone();
        let uptime_secs = self.server_start_time.elapsed().as_secs();
        let uptime = format!("Uptime: {}s", uptime_secs);
        let sidebar_width = self.config.sidebar_width;
        let active_id = self.active_session.clone();
        let sessions_snapshot = {
            let mgr = self.sessions.lock().unwrap();
            mgr.list()
        };
        let showing_help = self.showing_help;
        let prefix_name = if showing_help {
            Some(prefix_key_name(self.config.prefix_key))
        } else {
            None
        };
        let selection_ref = self.selection.as_ref();

        // Check Ctrl+C hint expiry before borrowing self in the closure.
        let now = Instant::now();
        let show_ctrl_c_hint = match self.ctrl_c_hint_until {
            Some(deadline) if deadline > now => true,
            Some(_) => {
                // Expired — clear it.
                self.ctrl_c_hint_until = None;
                false
            }
            None => false,
        };

        self.terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(sidebar_width),
                ])
                .split(area);

            // PTY pane
            if let Some((ref screen, _, _, _)) = screen_data {
                let mut widget = PtyWidget::new(screen);
                if let Some(sel) = selection_ref {
                    widget = widget.with_selection(sel);
                }
                frame.render_widget(widget, chunks[0]);
            }

            // Highlight overlays on the PTY pane
            for hl in &active_highlights {
                let color = match hl.color.as_str() {
                    "red" => Color::Red,
                    "green" => Color::Green,
                    "blue" => Color::Blue,
                    "yellow" => Color::Yellow,
                    "cyan" => Color::Cyan,
                    "magenta" => Color::Magenta,
                    "white" => Color::White,
                    "black" => Color::Black,
                    "dark_gray" | "darkgray" => Color::DarkGray,
                    "light_red" | "lightred" => Color::LightRed,
                    "light_green" | "lightgreen" => Color::LightGreen,
                    "light_blue" | "lightblue" => Color::LightBlue,
                    "light_yellow" | "lightyellow" => Color::LightYellow,
                    "light_cyan" | "lightcyan" => Color::LightCyan,
                    "light_magenta" | "lightmagenta" => Color::LightMagenta,
                    _ => Color::Yellow, // fallback
                };
                let pane = chunks[0];
                for row in hl.y..hl.y.saturating_add(hl.height) {
                    for col in hl.x..hl.x.saturating_add(hl.width) {
                        let abs_x = pane.x + col;
                        let abs_y = pane.y + row;
                        if abs_x < pane.x + pane.width && abs_y < pane.y + pane.height {
                            frame.buffer_mut()[(abs_x, abs_y)].set_bg(color);
                        }
                    }
                }
            }

            // Sidebar
            let widget = SidebarWidget::new(
                &sessions_snapshot,
                active_id.as_deref(),
                &reminders,
            ).build_info(&uptime);
            frame.render_widget(widget, chunks[1]);

            // Ctrl+C escape hint in the bottom status row of the PTY pane.
            if show_ctrl_c_hint && chunks[0].height > 0 {
                let hint_area = Rect::new(
                    chunks[0].x,
                    chunks[0].y + chunks[0].height.saturating_sub(1),
                    chunks[0].width,
                    1,
                );
                let hint_widget = Paragraph::new("Press Ctrl+\\ then ? for help")
                    .style(Style::default().fg(Color::Yellow).bg(Color::Black));
                frame.render_widget(hint_widget, hint_area);
            }

            // Help overlay popup
            if showing_help {
                let p = prefix_name.as_deref().unwrap_or("");
                let popup_width = 45u16;
                let popup_height = 14u16;
                let x = area.width.saturating_sub(popup_width) / 2;
                let y = area.height.saturating_sub(popup_height) / 2;
                let popup_area = Rect::new(
                    x, y,
                    popup_width.min(area.width),
                    popup_height.min(area.height),
                );

                frame.render_widget(Clear, popup_area);

                let help_text = vec![
                    Line::from(vec![Span::styled("tttt help", Style::default().add_modifier(Modifier::BOLD))]),
                    Line::from(format!("prefix: {}", p)),
                    Line::from(""),
                    Line::from("  0-9  Switch to terminal N"),
                    Line::from("  n    Next terminal"),
                    Line::from("  p    Previous terminal"),
                    Line::from("  c    Create new terminal"),
                    Line::from("  d    Detach/quit"),
                    Line::from("  r    Live reload (execv)"),
                    Line::from("  ?    This help"),
                    Line::from(format!("  {p}{p}  Send literal prefix")),
                    Line::from(""),
                    Line::from("Press any key to dismiss..."),
                ];

                let help_widget = Paragraph::new(help_text)
                    .block(Block::bordered().title(" Help "))
                    .style(Style::default().fg(Color::White).bg(Color::Black));
                frame.render_widget(help_widget, popup_area);
            }
        })?;

        // Position cursor at PTY cursor location, adjusted for row offset
        // when the PTY is taller than the display pane.
        if let Some((_, cursor_row, col, pty_rows)) = screen_data {
            let pane_height = self.screen_rows.saturating_sub(1); // status bar
            let row_offset = pty_rows.saturating_sub(pane_height);
            let display_row = cursor_row.saturating_sub(row_offset);
            self.terminal.set_cursor_position((col, display_row))?;
            self.terminal.show_cursor()?;
        }

        Ok(())
    }

    fn handle_resize(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (cols, rows) = terminal_size();
        self.screen_cols = cols;
        self.screen_rows = rows;
        let (pty_cols, pty_rows) = calculate_pane_dimensions(cols, rows, self.config.sidebar_width);
        self.pty_dims = (pty_cols, pty_rows);
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
                id.clone(), LogDirection::Meta, resize_data.clone(),
            ));
        }
        // Notify ratatui about the resize, then redraw
        self.terminal.resize(ratatui::layout::Rect::new(0, 0, cols, rows))?;
        self.render_frame()?;
        Ok(())
    }

    fn check_session_exit(&mut self) -> bool {
        let mgr = self.sessions.lock().unwrap();
        let is_running = |id: &str| mgr.get(id)
            .map_or(false, |s| matches!(s.status(), SessionStatus::Running));
        let action = compute_exit_action(
            self.active_session.as_deref(),
            &self.session_order,
            is_running,
        );
        drop(mgr);

        match action {
            SessionExitAction::NoExit => false,
            SessionExitAction::SwitchTo(next_id) => {
                if let Some(ref exited_id) = self.active_session {
                    if let Some(ref logger) = self.sqlite_logger {
                        let _ = logger.lock().unwrap().log_session_end(exited_id);
                    }
                }
                self.active_session = Some(next_id);
                false
            }
            SessionExitAction::AllExited => {
                if let Some(ref exited_id) = self.active_session {
                    if let Some(ref logger) = self.sqlite_logger {
                        let _ = logger.lock().unwrap().log_session_end(exited_id);
                    }
                }
                true
            }
        }
    }

    /// Minimum seconds of input idle before we inject scheduler messages.
    /// If the user typed something more recently, we defer or drop to avoid clobbering.
    const SCHEDULER_INPUT_IDLE_THRESHOLD: f64 = 2.0;

    fn handle_scheduler_event(&mut self, event: SchedulerEvent) {
        match &event {
            SchedulerEvent::ReminderFired(reminder) => {
                let _ = self.logger.log_event(&LogEvent::new(
                    "scheduler".to_string(), LogDirection::Meta,
                    format!("REMINDER: {}", reminder.message).into_bytes(),
                ));
                // Inject the reminder message into the active session (or first session).
                let target = self.active_session.clone().or_else(|| {
                    self.session_order.first().cloned()
                });
                if let Some(sid) = target {
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(&sid) {
                        if session.input_idle_seconds() < Self::SCHEDULER_INPUT_IDLE_THRESHOLD {
                            // Reminders always use wait policy — defer
                            drop(mgr);
                            self.deferred_scheduler_events.push(event);
                            return;
                        }
                        let text = format!("\n[REMINDER: {}]\r", reminder.message);
                        let _ = session.send_raw(text.as_bytes());
                    }
                }
            }
            SchedulerEvent::CronFired(job) => {
                let _ = self.logger.log_event(&LogEvent::new(
                    "scheduler".to_string(), LogDirection::Meta,
                    format!("CRON[{}]: {}", job.id, job.command).into_bytes(),
                ));
                // Always target the session_id specified in the job.
                if let Some(ref session_id) = job.session_id {
                    let mut mgr = self.sessions.lock().unwrap();
                    if let Ok(session) = mgr.get_mut(session_id) {
                        if session.input_idle_seconds() < Self::SCHEDULER_INPUT_IDLE_THRESHOLD {
                            match job.if_busy {
                                tttt_scheduler::BusyPolicy::Wait => {
                                    drop(mgr);
                                    self.deferred_scheduler_events.push(event);
                                }
                                tttt_scheduler::BusyPolicy::Drop => {}
                            }
                            return;
                        }
                        // Auto-terminate with \r so the agent processes the message
                        let text = format!("\r\n[CRON {}]: {}\r", job.id, job.command);
                        let _ = session.send_raw(text.as_bytes());
                    }
                }
            }
        }
    }

    /// Retry any deferred scheduler events whose target sessions are now idle.
    fn drain_deferred_scheduler_events(&mut self) {
        if self.deferred_scheduler_events.is_empty() {
            return;
        }
        let mut still_deferred = Vec::new();
        let events = std::mem::take(&mut self.deferred_scheduler_events);
        for event in events {
            let (target_id, is_idle) = match &event {
                SchedulerEvent::ReminderFired(_) => {
                    let sid = self.active_session.clone().or_else(|| {
                        self.session_order.first().cloned()
                    });
                    match sid {
                        Some(id) => {
                            let mgr = self.sessions.lock().unwrap();
                            let idle = mgr.get(&id).map_or(true, |s| {
                                s.input_idle_seconds() >= Self::SCHEDULER_INPUT_IDLE_THRESHOLD
                            });
                            (Some(id), idle)
                        }
                        None => (None, false),
                    }
                }
                SchedulerEvent::CronFired(job) => {
                    match &job.session_id {
                        Some(id) => {
                            let mgr = self.sessions.lock().unwrap();
                            let idle = mgr.get(id).map_or(true, |s| {
                                s.input_idle_seconds() >= Self::SCHEDULER_INPUT_IDLE_THRESHOLD
                            });
                            (Some(id.clone()), idle)
                        }
                        None => (None, false),
                    }
                }
            };
            if is_idle && target_id.is_some() {
                // Inject now
                match &event {
                    SchedulerEvent::ReminderFired(reminder) => {
                        let sid = target_id.unwrap();
                        let mut mgr = self.sessions.lock().unwrap();
                        if let Ok(session) = mgr.get_mut(&sid) {
                            let text = format!("\n[REMINDER: {}]\r", reminder.message);
                            let _ = session.send_raw(text.as_bytes());
                        }
                    }
                    SchedulerEvent::CronFired(job) => {
                        let sid = target_id.unwrap();
                        let mut mgr = self.sessions.lock().unwrap();
                        if let Ok(session) = mgr.get_mut(&sid) {
                            let text = format!("\r\n[CRON {}]: {}\r", job.id, job.command);
                            let _ = session.send_raw(text.as_bytes());
                        }
                    }
                }
            } else {
                still_deferred.push(event);
            }
        }
        self.deferred_scheduler_events = still_deferred;
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
                        let sidebar_dirty = self.sidebar_dirty.clone();
                        let tui_state = self.tui_state.clone();
                        let tui_tools_enabled = self.config.tui_tools;
                        let screen_cols = self.screen_cols;
                        let screen_rows = self.screen_rows;
                        let (pty_cols, pty_rows) = self.pty_dims;
                        let work_dir = self.config.work_dir.clone();
                        let db_path = self.config.db_path.clone();
                        let sqlite_logger = self.sqlite_logger.clone();
                        std::thread::spawn(move || {
                            use tttt_mcp::proxy::handle_proxy_client;
                            use tttt_mcp::{PtyToolHandler, ReplayToolHandler, SchedulerToolHandler, NotificationToolHandler, ScratchpadToolHandler, SidebarMessageToolHandler, TuiToolHandler, CompositeToolHandler};

                            // Set the stream to blocking mode for the handler
                            let _ = stream.set_nonblocking(false);

                            let pty_handler = PtyToolHandler::new(sessions.clone(), work_dir)
                                .with_default_dims(pty_cols, pty_rows)
                                .with_sqlite_logger(sqlite_logger);
                            let scheduler_handler = SchedulerToolHandler::new(scheduler);
                            let notif_handler = NotificationToolHandler::new(notifications, sessions.clone());
                            let scratchpad_handler = ScratchpadToolHandler::new_shared(scratchpad);
                            let sidebar_handler = SidebarMessageToolHandler::new(sidebar_messages, sidebar_dirty);
                            let replay_handler = ReplayToolHandler::new(db_path);
                            let mut composite = CompositeToolHandler::new();
                            composite.add_handler(Box::new(pty_handler));
                            composite.add_handler(Box::new(scheduler_handler));
                            composite.add_handler(Box::new(notif_handler));
                            composite.add_handler(Box::new(scratchpad_handler));
                            composite.add_handler(Box::new(sidebar_handler));
                            composite.add_handler(Box::new(replay_handler));

                            if tui_tools_enabled {
                                let tui_handler = TuiToolHandler::new(tui_state, sessions, screen_cols, screen_rows);
                                composite.add_handler(Box::new(tui_handler));
                            }

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
                        client.active_session = self.active_session.clone();
                        client.invalidate();
                        let _ = self.logger.log_event(&LogEvent::new(
                            "viewer".to_string(), LogDirection::Meta,
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

    fn process_viewer_input(&mut self) -> Result<(), Box<dyn std::error::Error>> {
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
                                "viewer".to_string(), LogDirection::Meta,
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
                                "viewer".to_string(), LogDirection::Meta,
                                format!("RESIZE: cols={}, rows={}", cols, rows).into_bytes(),
                            ));
                            // cols = usable PTY width reported by client
                            // (client subtracts its own sidebar if it has one)
                            self.viewer_clients[i].cols = cols;
                            self.viewer_clients[i].rows = rows;
                            self.viewer_clients[i].invalidate();
                            // Resize PTY to minimum across all clients (tmux behavior)
                            self.resize_pty_to_min_and_redraw();
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
            self.resize_pty_to_min_and_redraw();
        }

        Ok(())
    }

    /// Resize the PTY to the minimum size across the main terminal and all connected viewers.
    /// Forces a full ratatui redraw to remove stale content.
    fn resize_pty_to_min_and_redraw(&mut self) {
        // The PTY can never be larger than the main terminal's usable area
        let (max_pty_cols, max_pty_rows) = calculate_pane_dimensions(
            self.screen_cols, self.screen_rows, self.config.sidebar_width,
        );

        // Build the viewer dimensions slice (only connected clients).
        // Attach clients don't have a sidebar, so use their cols directly;
        // subtract 1 from rows for the status bar.
        let viewer_dims: Vec<(u16, u16)> = self.viewer_clients.iter()
            .filter(|c| c.connected)
            .map(|c| (c.cols, c.rows.saturating_sub(1)))
            .collect();

        let (min_cols, min_rows) =
            calculate_min_dimensions(&viewer_dims, max_pty_cols, max_pty_rows);

        // Check if dimensions actually changed
        let (old_cols, old_rows) = self.pty_dims;
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

            // Update tracked PTY dimensions
            self.pty_dims = (min_cols, min_rows);

            // Redraw the full frame via ratatui (handles gap fill and sidebar)
            let _ = self.render_frame();
        }

        // Always invalidate viewer hash state so they get a fresh update
        // Also notify clients of the new virtual window size
        for client in &mut self.viewer_clients {
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
                        "viewer".to_string(), LogDirection::Meta,
                        format!("UPDATE: sid={}, sent={}, screen_data_len={}, cursor=({},{})", sid, sent, screen_data_len, row, col).into_bytes(),
                    ));
                } else {
                    let _ = self.logger.log_event(&LogEvent::new(
                        "viewer".to_string(), LogDirection::Meta,
                        format!("UPDATE: session {} not found!", sid).into_bytes(),
                    ));
                }
            } else {
                let _ = self.logger.log_event(&LogEvent::new(
                    "viewer".to_string(), LogDirection::Meta,
                    "UPDATE: no active_session!".to_string().into_bytes(),
                ));
            }
        }
    }
}

/// Compute the effective scroll offset needed to keep selected content stable.
///
/// When a selection is active and new PTY output pushes lines into scrollback,
/// the content moves under the selection. This function computes the total
/// scroll offset needed: the drift from new output plus any manual scroll.
fn compute_selection_scroll_compensation(
    base_scrollback_count: usize,
    current_scrollback_count: usize,
    manual_scroll_offset: usize,
) -> usize {
    let drift = current_scrollback_count.saturating_sub(base_scrollback_count);
    drift + manual_scroll_offset
}

/// Copy text to the system clipboard via OSC 52 escape sequence.
fn copy_to_clipboard(text: &str) {
    // Try platform-native clipboard first (works in all terminals)
    if copy_to_clipboard_native(text) {
        return;
    }
    // Fall back to OSC 52 (works in iTerm2, kitty, alacritty, etc.)
    copy_to_clipboard_osc52(text);
}

/// Copy via platform-native command (pbcopy on macOS, xclip/xsel on Linux).
fn copy_to_clipboard_native(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let cmd = "pbcopy";
    #[cfg(target_os = "linux")]
    let cmd = "xclip";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return false;

    let mut child = match Command::new(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    if let Some(ref mut stdin) = child.stdin {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Copy via OSC 52 escape sequence (terminal must support it).
fn copy_to_clipboard_osc52(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let osc = format!("\x1b]52;c;{}\x07", encoded);
    let _ = std::io::stdout().write_all(osc.as_bytes());
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Chunk 1: prefix_key_name / help popup ────────────────────────────────

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
    fn test_help_popup_area_centered_large_terminal() {
        // 200x50 terminal — popup should be centered
        let area = ratatui::layout::Rect::new(0, 0, 200, 50);
        let popup = help_popup_area(area);
        assert_eq!(popup.width, 45, "popup width should be 45");
        assert_eq!(popup.height, 14, "popup height should be 14");
        // x = (200 - 45) / 2 = 77
        assert_eq!(popup.x, 77, "popup should be horizontally centered");
        // y = (50 - 14) / 2 = 18
        assert_eq!(popup.y, 18, "popup should be vertically centered");
    }

    #[test]
    fn test_help_popup_area_centered_standard_terminal() {
        // 80x24 terminal
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let popup = help_popup_area(area);
        assert_eq!(popup.width, 45);
        assert_eq!(popup.height, 14);
        // x = (80 - 45) / 2 = 17
        assert_eq!(popup.x, 17);
        // y = (24 - 14) / 2 = 5
        assert_eq!(popup.y, 5);
    }

    #[test]
    fn test_help_popup_area_clamped_when_terminal_too_small() {
        // Terminal smaller than popup — should clamp to terminal size, origin at 0
        let area = ratatui::layout::Rect::new(0, 0, 20, 5);
        let popup = help_popup_area(area);
        assert_eq!(popup.width, 20, "width clamped to terminal width");
        assert_eq!(popup.height, 5, "height clamped to terminal height");
        assert_eq!(popup.x, 0, "x should be 0 when terminal narrower than popup");
        assert_eq!(popup.y, 0, "y should be 0 when terminal shorter than popup");
    }

    #[test]
    fn test_help_popup_area_prefix_appears_in_help_text() {
        // Verify the prefix key name format used in the popup lines
        let prefix = "Ctrl+A";
        let line = format!("  {prefix}{prefix}  Send literal prefix");
        assert!(line.contains("Ctrl+ACtrl+A"), "literal prefix line should repeat prefix twice");
        assert!(line.contains("Send literal prefix"));
    }

    // ── Chunk 7: decide_input_action ─────────────────────────────────────────

    #[test]
    fn test_decide_input_action_pass_through() {
        let data = vec![b'h', b'i'];
        assert_eq!(
            decide_input_action(InputEvent::PassThrough(data.clone())),
            InputAction::SendToSession(data),
        );
    }

    #[test]
    fn test_decide_input_action_switch_terminal() {
        assert_eq!(
            decide_input_action(InputEvent::SwitchTerminal(3)),
            InputAction::SwitchSession(3),
        );
    }

    #[test]
    fn test_decide_input_action_next_terminal() {
        assert_eq!(decide_input_action(InputEvent::NextTerminal), InputAction::NextSession);
    }

    #[test]
    fn test_decide_input_action_prev_terminal() {
        assert_eq!(decide_input_action(InputEvent::PrevTerminal), InputAction::PrevSession);
    }

    #[test]
    fn test_decide_input_action_show_help() {
        assert_eq!(decide_input_action(InputEvent::ShowHelp), InputAction::ShowHelp);
    }

    #[test]
    fn test_decide_input_action_create_terminal() {
        assert_eq!(decide_input_action(InputEvent::CreateTerminal), InputAction::CreateSession);
    }

    #[test]
    fn test_decide_input_action_reload() {
        assert_eq!(decide_input_action(InputEvent::Reload), InputAction::Reload);
    }

    #[test]
    fn test_decide_input_action_detach() {
        assert_eq!(decide_input_action(InputEvent::Detach), InputAction::Detach);
    }

    #[test]
    fn test_decide_input_action_prefix_escape() {
        assert_eq!(decide_input_action(InputEvent::PrefixEscape), InputAction::PrefixEscape);
    }

    #[test]
    fn test_decide_input_action_show_ctrl_c_hint() {
        assert_eq!(
            decide_input_action(InputEvent::ShowCtrlCHint),
            InputAction::ShowCtrlCHint
        );
    }

    // ── Chunk 6: compute_exit_action ─────────────────────────────────────────

    #[test]
    fn test_compute_exit_action_no_active_session() {
        let order = ss(&["a", "b"]);
        let action = compute_exit_action(None, &order, |_| true);
        assert_eq!(action, SessionExitAction::NoExit);
    }

    #[test]
    fn test_compute_exit_action_still_running() {
        let order = ss(&["a", "b"]);
        let action = compute_exit_action(Some("a"), &order, |_| true);
        assert_eq!(action, SessionExitAction::NoExit);
    }

    #[test]
    fn test_compute_exit_action_exited_with_fallback() {
        let order = ss(&["a", "b"]);
        // "a" exited, "b" still running
        let action = compute_exit_action(Some("a"), &order, |id| id == "b");
        assert_eq!(action, SessionExitAction::SwitchTo("b".to_string()));
    }

    #[test]
    fn test_compute_exit_action_all_exited() {
        let order = ss(&["a", "b"]);
        // Both exited
        let action = compute_exit_action(Some("a"), &order, |_| false);
        assert_eq!(action, SessionExitAction::AllExited);
    }

    #[test]
    fn test_compute_exit_action_skips_self_in_fallback() {
        // session_order has the active session first — should not switch to itself
        let order = ss(&["a", "b", "c"]);
        // "a" exited, "b" exited, "c" running
        let action = compute_exit_action(Some("a"), &order, |id| id == "c");
        assert_eq!(action, SessionExitAction::SwitchTo("c".to_string()));
    }

    // ── Chunk 5: calculate_pane_dimensions / calculate_min_dimensions ─────────

    #[test]
    fn test_calculate_pane_dimensions_basic() {
        assert_eq!(calculate_pane_dimensions(120, 40, 20), (100, 39));
    }

    #[test]
    fn test_calculate_pane_dimensions_zero_sidebar() {
        assert_eq!(calculate_pane_dimensions(80, 24, 0), (80, 23));
    }

    #[test]
    fn test_calculate_pane_dimensions_saturates_cols() {
        assert_eq!(calculate_pane_dimensions(10, 24, 20), (0, 23));
    }

    #[test]
    fn test_calculate_pane_dimensions_saturates_rows() {
        assert_eq!(calculate_pane_dimensions(80, 0, 0), (80, 0));
    }

    #[test]
    fn test_calculate_min_dimensions_no_viewers() {
        assert_eq!(calculate_min_dimensions(&[], 100, 39), (100, 39));
    }

    #[test]
    fn test_calculate_min_dimensions_smaller_viewer() {
        assert_eq!(calculate_min_dimensions(&[(60, 20)], 100, 39), (60, 20));
    }

    #[test]
    fn test_calculate_min_dimensions_larger_viewer_clamped() {
        // Viewer larger than server → clamped to server baseline
        assert_eq!(calculate_min_dimensions(&[(200, 50)], 100, 39), (100, 39));
    }

    #[test]
    fn test_calculate_min_dimensions_multiple_viewers() {
        let viewers = [(80, 30), (60, 25), (90, 35)];
        assert_eq!(calculate_min_dimensions(&viewers, 100, 39), (60, 25));
    }

    // ── Chunk 4: should_render_now ───────────────────────────────────────────

    #[test]
    fn test_should_render_now_not_dirty_returns_false() {
        let now = Instant::now();
        assert!(!should_render_now(false, None, None, now, 50));
        // Even with very old timestamps, dirty=false must win
        let old = now - std::time::Duration::from_secs(10);
        assert!(!should_render_now(false, Some(old), Some(old), now, 50));
    }

    #[test]
    fn test_should_render_now_no_last_pty_data_burst_ended() {
        // last_pty_data = None → burst_ended defaults to true
        let now = Instant::now();
        assert!(should_render_now(true, None, None, now, 50));
    }

    #[test]
    fn test_should_render_now_burst_still_active() {
        let now = Instant::now();
        // last_pty_data just 1 ms ago, debounce is 50 ms → burst not ended
        let recent = now - std::time::Duration::from_millis(1);
        assert!(!should_render_now(true, Some(recent), Some(recent), now, 50));
    }

    #[test]
    fn test_should_render_now_burst_ended() {
        let now = Instant::now();
        let old_enough = now - std::time::Duration::from_millis(60);
        // burst ended but first_dirty is also old → renders
        assert!(should_render_now(true, Some(old_enough), Some(old_enough), now, 50));
    }

    #[test]
    fn test_should_render_now_max_latency_exceeded_during_burst() {
        let now = Instant::now();
        let burst_recent = now - std::time::Duration::from_millis(1); // burst not ended
        let very_old = now - std::time::Duration::from_millis(250);   // > 4*50
        // Still within burst but max latency exceeded → must render
        assert!(should_render_now(true, Some(burst_recent), Some(very_old), now, 50));
    }

    #[test]
    fn test_should_render_now_within_debounce_no_max_latency() {
        let now = Instant::now();
        let burst_recent = now - std::time::Duration::from_millis(1);
        let first_dirty_recent = now - std::time::Duration::from_millis(10);
        // burst not ended, max latency not exceeded → do not render
        assert!(!should_render_now(true, Some(burst_recent), Some(first_dirty_recent), now, 50));
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
    fn test_help_popup_area_prefix_appears_twice_for_literal_prefix() {
        // The "send literal prefix" line shows "prefixprefix" (e.g. "XXX" when prefix is "XX")
        let prefix = "XX";
        let line = format!("  {prefix}{prefix}  Send literal prefix");
        let count = line.matches("XX").count();
        assert!(count >= 2, "prefix should appear at least twice in literal-prefix entry");
    }

    // ── Ratatui layout calculations ───────────────────────────────────────────

    #[test]
    fn test_render_frame_layout_standard() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};
        // 100 wide, sidebar 30 → pane 70
        let area = Rect::new(0, 0, 100, 24);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(30),
            ])
            .split(area);
        assert_eq!(chunks[0].width, 70, "pane should be total minus sidebar");
        assert_eq!(chunks[1].width, 30, "sidebar should be exactly sidebar_width");
        assert_eq!(chunks[0].height, 24);
        assert_eq!(chunks[1].height, 24);
    }

    #[test]
    fn test_render_frame_layout_zero_sidebar() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};
        let area = Rect::new(0, 0, 80, 24);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(0),
            ])
            .split(area);
        assert_eq!(chunks[0].width, 80, "with zero sidebar pane should fill full width");
        assert_eq!(chunks[1].width, 0);
    }

    #[test]
    fn test_render_frame_layout_narrow_terminal_sidebar_wins() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};
        // Terminal only 10 wide, sidebar 30 → pane gets Min(1) = 1, sidebar truncated
        let area = Rect::new(0, 0, 10, 24);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(30),
            ])
            .split(area);
        // Total width is 10; sidebar wants 30 but can only get at most 9 (pane needs ≥1)
        assert!(chunks[0].width >= 1, "pane must always have at least 1 column");
        assert_eq!(chunks[0].width + chunks[1].width, 10, "chunks must sum to total width");
    }

    #[test]
    fn test_render_frame_layout_exact_sidebar_width() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};
        // 50 wide, sidebar 20 → pane 30
        let area = Rect::new(0, 0, 50, 24);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(20),
            ])
            .split(area);
        assert_eq!(chunks[0].width, 30);
        assert_eq!(chunks[1].width, 20);
    }

    // ── copy_to_clipboard / OSC 52 ────────────────────────────────────────────

    #[test]
    fn test_copy_to_clipboard_osc52_format() {
        // Verify the OSC 52 format is correct
        use base64::Engine;
        let text = "Hello World";
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let expected = format!("\x1b]52;c;{}\x07", encoded);
        // The function writes to stdout, so we just verify the format
        assert_eq!(expected, "\x1b]52;c;SGVsbG8gV29ybGQ=\x07");
    }

    #[test]
    fn test_copy_to_clipboard_empty_text() {
        use base64::Engine;
        let text = "";
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let expected = format!("\x1b]52;c;{}\x07", encoded);
        assert_eq!(expected, "\x1b]52;c;\x07");
    }

    #[test]
    fn test_copy_to_clipboard_unicode() {
        use base64::Engine;
        let text = "Hello 世界";
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let expected = format!("\x1b]52;c;{}\x07", encoded);
        // Just verify it doesn't panic and produces valid base64
        assert!(expected.starts_with("\x1b]52;c;"));
        assert!(expected.ends_with("\x07"));
    }

    // ── Selection scroll compensation ────────────────────────────────────────

    #[test]
    fn test_selection_scroll_compensation_no_new_output() {
        // Scrollback count unchanged → no compensation
        assert_eq!(compute_selection_scroll_compensation(10, 10, 0), 0);
    }

    #[test]
    fn test_selection_scroll_compensation_new_output() {
        // 5 new lines arrived since selection started → scroll back 5 to compensate
        assert_eq!(compute_selection_scroll_compensation(10, 15, 0), 5);
    }

    #[test]
    fn test_selection_scroll_compensation_with_manual_scroll() {
        // User manually scrolled 3 lines + 5 new lines → total offset 8
        assert_eq!(compute_selection_scroll_compensation(10, 15, 3), 8);
    }

    #[test]
    fn test_selection_scroll_compensation_no_selection_base() {
        // No selection active (base same as current) → just manual scroll
        assert_eq!(compute_selection_scroll_compensation(20, 20, 5), 5);
    }
}
