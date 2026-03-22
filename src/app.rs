use crate::config::Config;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tttt_log::{Direction, LogEvent, LogSink, MultiLogger, SqliteLogger, TextLogger};
use tttt_pty::{PtySession, RealPty, SessionManager, SessionStatus};
use tttt_scheduler::{Scheduler, SchedulerEvent};
use std::os::unix::net::UnixListener;
use tttt_tui::{
    clear_screen, cursor_goto, protocol, InputEvent, InputParser, PaneRenderer, RawInput,
    SessionInfo, SidebarRenderer, ViewerClient,
};

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
    scheduler: Scheduler,
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
            scheduler: Scheduler::new(),
            active_session: None,
            session_order: Vec::new(),
            screen_cols: cols,
            screen_rows: rows,
            viewer_listener: None,
            viewer_clients: Vec::new(),
            socket_path: None,
            config,
        }
    }

    /// Get a shared reference to the session manager (for the MCP server thread).
    pub fn shared_sessions(&self) -> Arc<Mutex<SessionManager<RealPty>>> {
        self.sessions.clone()
    }

    /// Start listening for viewer connections on a Unix socket.
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
        let args: Vec<&str> = self.config.root_args.iter().map(|s| s.as_str()).collect();
        let backend = RealPty::spawn_with_cwd(
            &self.config.root_command, &args, Some(&self.config.work_dir), pty_cols, pty_rows,
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
            let poll_result = if let Some(pty_raw_fd) = pty_fd {
                let pty_pfd = PollFd::new(
                    unsafe { BorrowedFd::borrow_raw(pty_raw_fd) }, PollFlags::POLLIN,
                );
                let mut fds = [pty_pfd, stdin_pfd];
                let _ = poll(&mut fds, PollTimeout::from(100u16));
                (fds[0].revents(), fds[1].revents())
            } else {
                let mut fds = [stdin_pfd];
                let _ = poll(&mut fds, PollTimeout::from(100u16));
                (None, fds[0].revents())
            };

            if winch.load(Ordering::Relaxed) {
                winch.store(false, Ordering::Relaxed);
                self.handle_resize(stdout_fd)?;
            }

            // Read PTY output and render
            if let Some(flags) = poll_result.0 {
                if flags.contains(PollFlags::POLLIN) {
                    if let Some(id) = self.active_session.clone() {
                        let render_data = {
                            let mut mgr = self.sessions.lock().unwrap();
                            if let Ok(session) = mgr.get_mut(&id) {
                                match session.pump_raw() {
                                    Ok((n, raw_bytes)) if n > 0 => {
                                        // Log the raw output
                                        let _ = self.logger.log_event(&LogEvent::new(
                                            id.clone(), Direction::Output, raw_bytes,
                                        ));
                                        let pane_output = self.pane_renderer.render(session.screen().screen());
                                        let cursor = session.cursor_position();
                                        // Always return cursor position — even if no cells changed
                                        // (e.g., arrow keys move cursor without changing content)
                                        Some((pane_output, cursor))
                                    }
                                    _ => None,
                                }
                            } else { None }
                        };
                        if let Some((pane_output, (row, col))) = render_data {
                            if !pane_output.is_empty() {
                                write_all(stdout_fd, &pane_output)?;
                                self.render_sidebar(stdout_fd)?;
                            }
                            // Always restore cursor to correct position
                            let (tr, tc) = self.pane_renderer.cursor_terminal_position(row, col);
                            write_all(stdout_fd, cursor_goto(tr, tc).as_bytes())?;
                        }
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

            // Sync session order (MCP may have added new ones)
            self.sync_session_order();

            if self.check_session_exit() { break; }

            let events = self.scheduler.tick(std::time::Instant::now());
            for event in events { self.handle_scheduler_event(event); }
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
            let mut mgr = self.sessions.lock().unwrap();
            if let Ok(session) = mgr.get_mut(id) {
                let pane_output = self.pane_renderer.render(session.screen().screen());
                drop(mgr);
                write_all(stdout_fd, &pane_output)?;
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
                            self.viewer_clients[i].cols = cols;
                            self.viewer_clients[i].rows = rows;
                            let pty_cols = cols.saturating_sub(self.config.sidebar_width);
                            let pty_rows = rows.saturating_sub(1);
                            self.viewer_clients[i].renderer.resize(pty_cols, pty_rows);
                            self.viewer_clients[i].invalidate();
                            // Resize PTY to minimum across all clients (tmux behavior)
                            self.resize_pty_to_min();
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

        // Remove disconnected clients
        self.viewer_clients.retain(|c| c.connected);

        Ok(())
    }

    /// Resize the PTY to the minimum size across the main terminal and all connected viewers.
    /// This is the tmux behavior: all clients see the same (smallest) size.
    fn resize_pty_to_min(&mut self) {
        let sidebar = self.config.sidebar_width;
        // Start with main terminal size
        let mut min_cols = self.screen_cols.saturating_sub(sidebar);
        let mut min_rows = self.screen_rows.saturating_sub(1);

        // Find minimum across all connected viewers
        for client in &self.viewer_clients {
            if !client.connected {
                continue;
            }
            let c = client.cols.saturating_sub(sidebar);
            let r = client.rows.saturating_sub(1);
            min_cols = min_cols.min(c);
            min_rows = min_rows.min(r);
        }

        // Resize all sessions
        let mut mgr = self.sessions.lock().unwrap();
        let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
        for id in ids {
            if let Ok(session) = mgr.get_mut(&id) {
                let _ = session.resize(min_cols, min_rows);
            }
        }
        drop(mgr);

        // Resize main pane renderer
        self.pane_renderer.resize(min_cols, min_rows);

        // Resize all viewer renderers and invalidate
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
                    client.send_screen_update(session.screen().screen(), row, col);
                }
            }
        }
    }
}
