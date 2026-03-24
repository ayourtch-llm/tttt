use crate::config::Config;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tttt_log::{Direction, LogEvent, LogSink, MultiLogger, SqliteLogger, TextLogger};
use tttt_mcp::notification::NotificationRegistry;
use tttt_mcp::{SharedNotificationRegistry, SharedScheduler};
use tttt_pty::{PtySession, RealPty, SessionManager, SessionStatus};
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

/// Main application state.
pub struct App {
    config: Config,
    sessions: Arc<Mutex<SessionManager<RealPty>>>,
    input_parser: InputParser,
    sidebar: SidebarRenderer,
    pane_renderer: PaneRenderer,
    logger: MultiLogger,
    scheduler: SharedScheduler,
    notifications: SharedNotificationRegistry,
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
            scheduler: Arc::new(Mutex::new(Scheduler::new())),
            notifications: Arc::new(Mutex::new(NotificationRegistry::new())),
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
            config,
        }
    }

    /// Get a shared reference to the session manager (for the MCP server thread).
    pub fn shared_sessions(&self) -> Arc<Mutex<SessionManager<RealPty>>> {
        self.sessions.clone()
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
        let sqlite_logger = SqliteLogger::new(&self.config.db_path)?;
        self.logger.add_sink(Box::new(sqlite_logger));
        Ok(())
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
        let backend = RealPty::spawn_with_cwd_and_env(
            &self.config.root_command, &args_refs, Some(&self.config.work_dir), pty_cols, pty_rows,
            [("TTTT_PID".to_string(), tttt_pid.to_string())],
        )?;
        let mut mgr = self.sessions.lock().unwrap();
        let id = mgr.generate_id();
        let session = PtySession::new(id.clone(), backend, self.config.root_command.clone(), pty_cols, pty_rows);
        mgr.add_session(session)?;
        drop(mgr);
        self.session_order.push(id.clone());
        self.active_session = Some(id.clone());
        Ok(id)
    }

    /// Create a new PTY session with the default shell.
    pub fn create_session(&mut self, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let pty_cols = self.screen_cols.saturating_sub(self.config.sidebar_width);
        let pty_rows = self.screen_rows.saturating_sub(1);

        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let tttt_pid = std::process::id();
        let backend = RealPty::spawn_with_cwd_and_env(
            &default_shell, &[], Some(&self.config.work_dir), pty_cols, pty_rows,
            [("TTTT_PID".to_string(), tttt_pid.to_string())],
        )?;
        let mut mgr = self.sessions.lock().unwrap();
        let id = mgr.generate_id();
        let session = PtySession::new(id.clone(), backend, default_shell, pty_cols, pty_rows);
        mgr.add_session(session)?;
        drop(mgr);
        self.session_order.push(id.clone());
        self.switch_to_session(&id, stdout_fd)?;
        Ok(())
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let _terminal_state = TerminalState::enter_raw_mode();
        let winch = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(libc::SIGWINCH, Arc::clone(&winch));

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
        let current_ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
        drop(mgr);

        // Add any new sessions not in our order
        for id in &current_ids {
            if !self.session_order.contains(id) {
                self.session_order.push(id.clone());
            }
        }
        // Remove any sessions that no longer exist
        self.session_order.retain(|id| current_ids.contains(id));
    }

    fn handle_input_event(&mut self, event: InputEvent, stdout_fd: i32) -> Result<bool, Box<dyn std::error::Error>> {
        match event {
            InputEvent::PassThrough(data) => {
                if let Some(ref id) = self.active_session {
                    let _ = self.logger.log_event(&LogEvent::new(id.clone(), Direction::Input, data.clone()));
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
        if self.session_order.is_empty() { return Ok(()); }
        let current_idx = self.active_session.as_ref()
            .and_then(|id| self.session_order.iter().position(|s| s == id))
            .unwrap_or(0);
        let len = self.session_order.len() as i32;
        let new_idx = ((current_idx as i32 + delta) % len + len) % len;
        let id = self.session_order[new_idx as usize].clone();
        self.switch_to_session(&id, stdout_fd)?;
        Ok(())
    }

    fn show_help(&mut self, stdout_fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let prefix_name = match self.config.prefix_key {
            0x1c => "Ctrl+\\".to_string(),
            0x01 => "Ctrl+A".to_string(),
            0x02 => "Ctrl+B".to_string(),
            b => format!("0x{:02x}", b),
        };
        let help = format!(
            "{}{}\x1b[1mtttt help\x1b[0m  (prefix: {})\
            {}  0-9  Switch to terminal N\
            {}  n    Next terminal\
            {}  p    Previous terminal\
            {}  d    Detach/quit\
            {}  ?    This help\
            {}  {p}{p}  Send literal prefix\
            {}Press any key to dismiss...",
            clear_screen(), cursor_goto(2, 4), prefix_name,
            cursor_goto(4, 4), cursor_goto(5, 4), cursor_goto(6, 4),
            cursor_goto(7, 4), cursor_goto(8, 4),
            cursor_goto(10, 4), cursor_goto(12, 4), p = prefix_name,
        );
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
        let reminders: Vec<String> = Vec::new();
        let lines = self.sidebar.render(
            &sessions, self.active_session.as_deref(),
            self.screen_cols, self.screen_rows, &reminders,
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
        {
            let mut mgr = self.sessions.lock().unwrap();
            let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
            for id in ids {
                if let Ok(session) = mgr.get_mut(&id) {
                    let _ = session.resize(pty_cols, pty_rows);
                }
            }
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
                let next = self.session_order.iter()
                    .find(|s| *s != id && mgr.get(s).map_or(false, |sess| matches!(sess.status(), SessionStatus::Running)))
                    .cloned();
                drop(mgr);
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
                        let work_dir = self.config.work_dir.clone();
                        std::thread::spawn(move || {
                            use tttt_mcp::proxy::handle_proxy_client;
                            use tttt_mcp::{PtyToolHandler, SchedulerToolHandler, NotificationToolHandler, ScratchpadToolHandler, CompositeToolHandler};

                            // Set the stream to blocking mode for the handler
                            let _ = stream.set_nonblocking(false);

                            let pty_handler = PtyToolHandler::new(sessions.clone(), work_dir);
                            let scheduler_handler = SchedulerToolHandler::new(scheduler);
                            let notif_handler = NotificationToolHandler::new(notifications, sessions);
                            let scratchpad_handler = ScratchpadToolHandler::new();
                            let mut composite = CompositeToolHandler::new();
                            composite.add_handler(Box::new(pty_handler));
                            composite.add_handler(Box::new(scheduler_handler));
                            composite.add_handler(Box::new(notif_handler));
                            composite.add_handler(Box::new(scratchpad_handler));

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
        // Start with main terminal size
        let mut min_cols = self.screen_cols.saturating_sub(sidebar);
        let mut min_rows = self.screen_rows.saturating_sub(1);

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
        for client in &mut self.viewer_clients {
            client.renderer.resize(min_cols, min_rows);
            client.invalidate();
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
