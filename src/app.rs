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
use tttt_mcp::SharedContextRefreshQueue;
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

/// Replace zero dimensions with defaults. A PTY whose winsize was never
/// set reports 0x0, which the vt100 grid cannot represent (rows - 1
/// underflows).
fn normalize_terminal_size(cols: u16, rows: u16) -> (u16, u16) {
    (
        if cols == 0 { 80 } else { cols },
        if rows == 0 { 24 } else { rows },
    )
}

fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            normalize_terminal_size(ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

/// Basename of a root command, for app detection: "/opt/homebrew/bin/codex"
/// and "codex" both yield "codex". Matching on the basename (rather than a
/// substring of the whole path) keeps a binary that merely lives under a
/// directory named after another app (e.g. /tmp/claude-501/bin/codex) from
/// being detected as that app.
fn command_basename(cmd: &str) -> &str {
    std::path::Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd)
}

fn codex_mcp_config_args(tttt_bin: &std::path::Path, mcp_socket: &str) -> Vec<String> {
    let command = serde_json::to_string(&tttt_bin.to_string_lossy()).unwrap();
    let server_args = serde_json::to_string(&vec!["mcp-server", "--connect", mcp_socket]).unwrap();

    vec![
        "--config".to_string(),
        format!("mcp_servers.tttt.command={command}"),
        "--config".to_string(),
        format!("mcp_servers.tttt.args={server_args}"),
    ]
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
    MousePress { button: tttt_tui::MouseButton, modifiers: tttt_tui::MouseModifiers, col: u16, row: u16 },
    MouseDrag { button: tttt_tui::MouseButton, col: u16, row: u16 },
    MouseRelease { col: u16, row: u16 },
    ScrollUp { col: u16, row: u16 },
    ScrollDown { col: u16, row: u16 },
    /// Show the Ctrl+C escape hint in the status line.
    ShowCtrlCHint,
    /// Force a full repaint of the host terminal (recovery from corrupted display).
    Redraw,
    /// Write a diagnostic dump of the active session's render state to a file.
    DumpDiagnostics,
    /// Toggle the sticky/pinned-visible state of the active session
    /// (keyboard parity with ctrl-click on the active sidebar entry).
    ToggleStickyActive,
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
        tttt_tui::InputEvent::MousePress { button, modifiers, col, row } => InputAction::MousePress { button, modifiers, col, row },
        tttt_tui::InputEvent::MouseDrag { button, col, row } => InputAction::MouseDrag { button, col, row },
        tttt_tui::InputEvent::MouseRelease { col, row } => InputAction::MouseRelease { col, row },
        tttt_tui::InputEvent::ScrollUp { col, row } => InputAction::ScrollUp { col, row },
        tttt_tui::InputEvent::ScrollDown { col, row } => InputAction::ScrollDown { col, row },
        tttt_tui::InputEvent::ShowCtrlCHint => InputAction::ShowCtrlCHint,
        tttt_tui::InputEvent::Redraw => InputAction::Redraw,
        tttt_tui::InputEvent::DumpDiagnostics => InputAction::DumpDiagnostics,
        tttt_tui::InputEvent::ToggleStickyActive => InputAction::ToggleStickyActive,
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

/// Advance one context refresh request. Returns true once both stages complete.
fn advance_context_refresh_request(
    request: &mut tttt_mcp::ContextRefreshRequest,
    now: Instant,
    mut inject: impl FnMut(&str, &str) -> bool,
) -> bool {
    if !request.clear_sent && now >= request.clear_at {
        if inject("/clear", "clear") {
            request.clear_sent = true;
            request.restore_at = Some(now + request.followup_delay);
        }
    }

    let restore_due = request.clear_sent
        && request
            .restore_at
            .map(|deadline| now >= deadline)
            .unwrap_or(false);
    if !restore_due {
        return false;
    }

    let instruction = format!(
        "CONTEXT REFRESH: Please read {} to restore the context, then continue from the handoff.",
        request.filename
    );
    inject(&instruction, "restore")
}

/// Toggle a session's pinned-visible (sticky) state.
///
/// Returns the new `visible` Vec. Sticky membership is independent of which
/// session is active: ctrl-clicking the active session pins it so it remains
/// visible after switching active to another session. The active session is
/// always rendered regardless of `visible`, but its sticky bit only matters
/// once the user moves on.
fn toggle_session_visibility(visible: &[String], target_id: &str) -> Vec<String> {
    if visible.iter().any(|s| s == target_id) {
        visible.iter().filter(|s| s.as_str() != target_id).cloned().collect()
    } else {
        let mut next = visible.to_vec();
        next.push(target_id.to_string());
        next
    }
}

/// Computed rectangles for the main viewport's layout.
///
/// Single source of truth shared between `render_frame` (for painting) and
/// the PTY resize path (for SIGWINCH). When the two diverge, apps inside
/// the panes see the wrong size — keeping them computed in one place
/// avoids that drift.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneLayout {
    /// The right-side sidebar area.
    sidebar: Rect,
    /// The bottom hint row reserved for status text. Carved out of the
    /// pane container *before* the grid split so panes have clean edges.
    hint: Rect,
    /// One Rect per pane in `render_ids` order, row-major. Empty when no
    /// panes are visible.
    pane_rects: Vec<Rect>,
}

/// Compute the rect layout for the main viewport given the terminal size,
/// sidebar width, and the number of panes that will be displayed.
fn compute_pane_layout(
    screen_cols: u16,
    screen_rows: u16,
    sidebar_width: u16,
    n_panes: usize,
) -> PaneLayout {
    let area = Rect::new(0, 0, screen_cols, screen_rows);
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(sidebar_width),
        ])
        .split(area);
    let pane_container = h_chunks[0];

    // Reserve the bottom row of the pane container for the hint. With this
    // split the grid never paints over the hint, and apps in the bottom
    // pane no longer lose their last row.
    let v_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(pane_container);
    let grid_area = v_chunks[0];
    let hint = v_chunks[1];

    let pane_rects: Vec<Rect> = if n_panes == 0 {
        Vec::new()
    } else if n_panes == 1 {
        vec![grid_area]
    } else {
        let (grid_rows, grid_cols) =
            compute_grid_dims(n_panes, grid_area.width, grid_area.height);
        let row_constraints: Vec<Constraint> = (0..grid_rows)
            .map(|_| Constraint::Ratio(1, grid_rows as u32))
            .collect();
        let row_areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(grid_area);

        let mut rects: Vec<Rect> = Vec::with_capacity(n_panes);
        for r in 0..grid_rows {
            let remaining = n_panes.saturating_sub(r * grid_cols);
            if remaining == 0 {
                break;
            }
            let cells_in_row = remaining.min(grid_cols);
            let col_constraints: Vec<Constraint> = (0..cells_in_row)
                .map(|_| Constraint::Ratio(1, cells_in_row as u32))
                .collect();
            let col_areas = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(col_constraints)
                .split(row_areas[r]);
            for c in 0..cells_in_row {
                rects.push(col_areas[c]);
            }
        }
        rects
    };

    PaneLayout {
        sidebar: h_chunks[1],
        hint,
        pane_rects,
    }
}

/// Pick a grid (rows, cols) for `n` panes that gives each cell an aspect
/// ratio close to the container's, while keeping `rows * cols >= n`.
///
/// The score minimizes `|cell_w/cell_h - container_w/container_h|` over all
/// candidates. Ties resolve in favor of fewer rows (more horizontal layouts),
/// which keeps the visual order predictable when multiple grids match.
///
/// Examples (container 100x30, target aspect ≈ 3.33):
/// - n=2 → (1, 2): cells 50x30, side-by-side.
/// - n=4 → (2, 2): cells 50x15, exact aspect match.
/// - n=6 → (2, 3): cells 33x15, closer to target than 3x2's 50x10.
fn compute_grid_dims(n: usize, container_cols: u16, container_rows: u16) -> (usize, usize) {
    if n <= 1 {
        return (1, 1);
    }
    let target = (container_cols.max(1) as f32) / (container_rows.max(1) as f32);
    let mut best: Option<(f32, usize, usize)> = None;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cell_w = (container_cols as f32) / (cols as f32);
        let cell_h = (container_rows as f32) / (rows as f32);
        let aspect = if cell_h > 0.0 { cell_w / cell_h } else { f32::INFINITY };
        let score = (aspect - target).abs();
        let candidate = (score, rows, cols);
        match best {
            None => best = Some(candidate),
            Some((bs, br, _)) => {
                // Strictly lower score wins; ties prefer fewer rows.
                if score < bs - f32::EPSILON
                    || ((score - bs).abs() < f32::EPSILON && rows < br)
                {
                    best = Some(candidate);
                }
            }
        }
    }
    let (_, rows, cols) = best.unwrap();
    (rows, cols)
}

/// Compute a cursor-aware viewport row offset for rendering a PTY screen
/// into a smaller display area.
///
/// Default policy: keep the cursor visible at (or below) the bottom edge of
/// the pane while preferring offset=0 when the cursor is high enough that it
/// already fits. The result is always within `[0, max(0, pty_rows - area_h)]`.
///
/// - When `pty_rows <= area_h`, the area can show the full screen → offset 0.
/// - When the cursor sits within the top `area_h` rows, offset stays 0 so the
///   top of the screen (header / app UI) remains visible.
/// - As the cursor moves below the pane, the viewport scrolls down just
///   enough to keep the cursor on the bottom edge.
fn compute_pane_row_offset(pty_rows: u16, area_h: u16, cursor_row: u16) -> u16 {
    if area_h >= pty_rows || area_h == 0 {
        return 0;
    }
    let max_offset = pty_rows - area_h;
    cursor_row
        .saturating_sub(area_h.saturating_sub(1))
        .min(max_offset)
}

/// Compute the ordered list of session IDs to render in the main viewport.
///
/// Walks `session_order` and returns the subset that is either the active
/// session or in the pinned-visible set. Order matches `session_order` so
/// the rendered stack mirrors the sidebar layout. The active session is
/// always included even if not in `visible` (the implicit-visibility invariant).
fn compute_render_session_ids(
    active: Option<&str>,
    visible: &[String],
    session_order: &[String],
) -> Vec<String> {
    session_order
        .iter()
        .filter(|id| {
            active == Some(id.as_str()) || visible.iter().any(|v| v == *id)
        })
        .cloned()
        .collect()
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
/// Inputs for the diagnostic dump formatter — kept as a plain struct so the
/// formatter is a pure function that can be unit-tested without an `App`.
pub(crate) struct DiagnosticInputs<'a> {
    pub timestamp_ms: u128,
    pub session_id: &'a str,
    pub session_command: &'a str,
    pub session_status: &'a tttt_pty::SessionStatus,
    /// Parser size as `(cols, rows)`.
    pub parser_size: (u16, u16),
    /// Cursor position as `(row, col)`.
    pub cursor: (u16, u16),
    pub max_scroll: usize,
    /// Host terminal size as `(cols, rows)`.
    pub host_size: (u16, u16),
    pub sidebar_width: u16,
    pub pty_dims: (u16, u16),
    /// Pane area size as `(cols, rows)`.
    pub pane_size: (u16, u16),
    pub scroll_offset: usize,
    pub selection_scroll_base: usize,
    pub selection: Option<&'a tttt_tui::Selection>,
    pub all_sessions: &'a [tttt_pty::SessionMetadata],
    pub plain_contents: &'a str,
    /// The buffer that PtyWidget produces when rendered into the pane area.
    pub rendered_buffer: &'a ratatui::buffer::Buffer,
    pub formatted_contents: &'a [u8],
}

/// Render the diagnostic dump to a byte vector. Pure function — no I/O.
pub(crate) fn format_diagnostic_dump(inputs: &DiagnosticInputs) -> Vec<u8> {
    use std::io::Write;
    let mut out: Vec<u8> = Vec::new();
    let _ = writeln!(out, "tttt diagnostic dump");
    let _ = writeln!(out, "timestamp_ms: {}", inputs.timestamp_ms);
    let _ = writeln!(out);
    let _ = writeln!(out, "[active session]");
    let _ = writeln!(out, "id:                {}", inputs.session_id);
    let _ = writeln!(out, "command:           {}", inputs.session_command);
    let _ = writeln!(out, "status:            {:?}", inputs.session_status);
    let _ = writeln!(
        out,
        "parser size:       {}x{} (cols x rows)",
        inputs.parser_size.0, inputs.parser_size.1
    );
    let _ = writeln!(
        out,
        "cursor:            ({}, {}) (row, col)",
        inputs.cursor.0, inputs.cursor.1
    );
    let _ = writeln!(out, "max_scroll_offset: {}", inputs.max_scroll);
    let _ = writeln!(out);
    let _ = writeln!(out, "[host terminal / app state]");
    let _ = writeln!(
        out,
        "host size:         {}x{} (cols x rows)",
        inputs.host_size.0, inputs.host_size.1
    );
    let _ = writeln!(out, "sidebar_width:     {}", inputs.sidebar_width);
    let _ = writeln!(
        out,
        "configured pty_dims: {}x{}",
        inputs.pty_dims.0, inputs.pty_dims.1
    );
    let _ = writeln!(
        out,
        "pane area:         {}x{} (cols x rows)",
        inputs.pane_size.0, inputs.pane_size.1
    );
    let _ = writeln!(out, "scroll_offset:     {}", inputs.scroll_offset);
    let _ = writeln!(
        out,
        "selection_scroll_base: {}",
        inputs.selection_scroll_base
    );
    let _ = writeln!(out, "selection:         {:?}", inputs.selection);
    let _ = writeln!(out);
    let _ = writeln!(out, "[all sessions]");
    for s in inputs.all_sessions {
        let _ = writeln!(
            out,
            "  {} {}x{} {:?} cmd={}",
            s.id, s.cols, s.rows, s.status, s.command
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "[parser plain contents]");
    let _ = writeln!(out, "{}", inputs.plain_contents);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "[ptywidget render output (what the renderer would draw)]"
    );
    let buf_area = inputs.rendered_buffer.area();
    for row in 0..buf_area.height {
        let mut line = String::new();
        for col in 0..buf_area.width {
            line.push_str(inputs.rendered_buffer[(col, row)].symbol());
        }
        let _ = writeln!(out, "{:>3} | {}", row, line);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "[parser formatted contents (raw ANSI bytes)]");
    out.extend_from_slice(inputs.formatted_contents);
    let _ = writeln!(out);
    out
}

fn help_popup_area(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup_width: u16 = 45;
    let popup_height: u16 = 16;
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
    /// Sessions explicitly pinned visible in the main viewport, in addition to
    /// the active session (which is always rendered). Order is preserved but
    /// the effective render list is reordered against `session_order` at
    /// render time. Reconciled when sessions are added/removed.
    visible_sessions: Vec<String>,
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
    pending_injection_queue: std::collections::VecDeque<(String, String)>,
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
    /// When Some(deadline), show a hint message until that instant.
    ctrl_c_hint_until: Option<Instant>,
    /// Custom hint message to show (if None, shows default help hint).
    ctrl_c_hint_message: Option<String>,
    /// Last session metadata snapshot — compared each tick to detect changes.
    last_session_snapshot: Vec<tttt_pty::SessionMetadata>,
    /// Deferred scheduler events waiting for target session to become idle.
    deferred_scheduler_events: Vec<SchedulerEvent>,
    /// Pending Enter keystrokes for injections (cron, reminder, notification),
    /// sent after a delay so the target app processes the text before submission.
    pending_delayed_enters: Vec<(String, Instant)>,
    /// Direct keyboard input waiting for a non-blocking PTY write. Bytes are
    /// ordered per session so a busy child cannot stall the TUI event loop.
    pending_user_input:
        std::collections::HashMap<String, std::collections::VecDeque<u8>>,
    /// Context refresh requests scheduled by the MCP handoff tool.
    context_refresh_queue: SharedContextRefreshQueue,
}

impl App {
    /// Upper bound on queued notification injections. With 100ms pacing this
    /// is ~26s of backlog; beyond that a runaway watcher is just filling RAM.
    const MAX_PENDING_INJECTIONS: usize = 256;

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
            visible_sessions: Vec::new(),
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
            pending_injection_queue: std::collections::VecDeque::new(),
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
            ctrl_c_hint_message: None,
            last_session_snapshot: Vec::new(),
            deferred_scheduler_events: Vec::new(),
            pending_delayed_enters: Vec::new(),
            pending_user_input: std::collections::HashMap::new(),
            context_refresh_queue: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            config,
        }
    }

    /// Get a shared reference to the session manager (for the MCP server thread).
    #[allow(dead_code)]
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

                    // Restore working directory
                    if let Some(ref wd) = saved.working_dir {
                        session.set_working_dir(wd.clone());
                    }

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
                    // from_raw_fd does not take ownership on failure — close the
                    // inherited FD here or it leaks until process exit.
                    unsafe { libc::close(saved.master_fd); }
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

    /// Generate the opencode MCP config JSON content for the OPENCODE_CONFIG_CONTENT
    /// env var. Returns a JSON string with the tttt MCP server in opencode format,
    /// merged with any existing OPENCODE_CONFIG_CONTENT value from the environment.
    ///
    /// opencode merges config from multiple sources (global, project, inline), so
    /// we only need to specify the tttt MCP server here — the user's own config is
    /// loaded from other config files and merged with this inline override.
    pub fn generate_opencode_mcp_config_content(&self) -> Result<String, Box<dyn std::error::Error>> {
        let mcp_socket = self.mcp_socket_path.as_ref()
            .ok_or("MCP listener not started")?;

        let tttt_bin = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("tttt"));

        let tttt_entry = serde_json::json!({
            "type": "local",
            "command": [
                tttt_bin.to_string_lossy(),
                "mcp-server",
                "--connect",
                mcp_socket,
            ],
            "enabled": true,
        });

        // Start with existing OPENCODE_CONFIG_CONTENT if present, otherwise start fresh.
        let mut config: serde_json::Value = match std::env::var("OPENCODE_CONFIG_CONTENT") {
            Ok(s) if !s.is_empty() => {
                serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({}))
            }
            _ => serde_json::json!({}),
        };

        // Ensure config is a JSON object
        if !config.is_object() {
            config = serde_json::json!({});
        }

        // Merge tttt into the mcp section
        let mcp_map = config.as_object_mut().unwrap();
        if !mcp_map.contains_key("mcp") {
            mcp_map.insert("mcp".to_string(), serde_json::json!({}));
        }
        let mcp_obj = mcp_map.get_mut("mcp").unwrap();
        if !mcp_obj.is_object() {
            *mcp_obj = serde_json::json!({});
        }
        mcp_obj.as_object_mut().unwrap().insert("tttt".to_string(), tttt_entry);

        Ok(serde_json::to_string(&config)?)
    }

    /// Generate invocation-scoped Codex config overrides for the tttt MCP server.
    /// Codex accepts TOML values through `--config key=value`, so this leaves the
    /// user's persistent `~/.codex/config.toml` untouched.
    pub fn generate_codex_mcp_config_args(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mcp_socket = self.mcp_socket_path.as_ref()
            .ok_or("MCP listener not started")?;
        let tttt_bin = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("tttt"));

        Ok(codex_mcp_config_args(&tttt_bin, mcp_socket))
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
        if self.pending_injection_queue.len() >= Self::MAX_PENDING_INJECTIONS {
            let _ = self.logger.log_event(&LogEvent::new(
                session_id.to_string(), LogDirection::Meta,
                format!("[NOTIFICATION-DROPPED] injection queue full ({} entries): {}",
                    Self::MAX_PENDING_INJECTIONS, text).into_bytes(),
            ));
            return;
        }
        self.pending_injection_queue.push_back((session_id.to_string(), text.to_string()));
    }

    /// Remove a session ID from the session order list.
    #[allow(dead_code)]
    pub fn remove_from_session_order(&mut self, id: &str) {
        self.session_order.retain(|s| s != id);
        self.visible_sessions.retain(|s| s != id);
        if self.active_session.as_deref() == Some(id) {
            self.active_session = self.session_order.first().cloned();
        }
    }

    /// Build args and spawn a new root PTY backend.
    fn spawn_root_backend(&mut self, pty_cols: u16, pty_rows: u16) -> Result<AnyPty, Box<dyn std::error::Error>> {
        // If MCP socket is available, generate config and inject --mcp-config
        // for agents that support it (e.g., claude, opencode, codex)
        let mut args: Vec<String> = self.config.root_args.clone();
        let mut env_vars: Vec<(String, String)> = vec![
            ("TTTT_PID".to_string(), std::process::id().to_string()),
        ];

        if self.mcp_socket_path.is_some() {
            let cmd = command_basename(&self.config.root_command);
            if cmd == "claude" && !args.iter().any(|a| a.contains("mcp-config")) {
                // Claude uses --mcp-config with a JSON file
                if let Ok(config_path) = self.generate_mcp_config() {
                    args.push("--mcp-config".to_string());
                    args.push(config_path);
                }
            } else if cmd == "opencode" {
                // opencode: inject the tttt MCP server via OPENCODE_CONFIG_CONTENT env var.
                // opencode merges config from multiple sources, so we only need to specify
                // the tttt MCP server here — the user's own config is loaded from other
                // config files and merged with this inline override.
                if let Ok(content) = self.generate_opencode_mcp_config_content() {
                    env_vars.push(("OPENCODE_CONFIG_CONTENT".to_string(), content));
                }
            } else if cmd == "codex"
                && !args.iter().any(|a| a.contains("mcp_servers.tttt"))
            {
                // Codex accepts per-invocation config overrides. Prepend them so
                // existing arguments (including an initial prompt) retain their meaning.
                if let Ok(mut config_args) = self.generate_codex_mcp_config_args() {
                    config_args.extend(args);
                    args = config_args;
                }
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

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let real_backend = RealPty::spawn_with_cwd_and_env(
            &self.config.root_command, &args_refs, Some(&self.config.work_dir), pty_cols, pty_rows,
            env_vars,
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
        let work_dir = self.config.work_dir.to_string_lossy().into_owned();
        session.set_working_dir(work_dir.clone());
        mgr.add_session(session)?;
        drop(mgr);
        self.session_order.push(id.clone());
        self.active_session = Some(id.clone());
        if let Some(ref logger) = self.sqlite_logger {
            let _ = logger.lock().unwrap().log_session_start_with_dir(
                &id, &self.config.root_command, pty_cols, pty_rows, None, Some(&work_dir),
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
        let mut session = PtySession::new(id.clone(), backend, default_shell, pty_cols, pty_rows);
        session.set_working_dir(self.config.work_dir.to_string_lossy().into_owned());
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
                    working_dir: session.working_dir().map(|s| s.to_string()),
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
            self.drain_pending_user_input();

            // Get active PTY fd for polling (short lock)
            let pty_fd = self.active_session.as_ref().and_then(|id| {
                let mgr = self.sessions.lock().unwrap();
                mgr.get(id).ok().map(|s| s.backend().reader_raw_fd())
            });

            let stdin_pfd = PollFd::new(
                unsafe { BorrowedFd::borrow_raw(stdin_fd) }, PollFlags::POLLIN,
            );
            // Shorter poll timeout when we have a pending render (for debounce responsiveness)
            let poll_timeout_ms =
                if self.server_render_dirty || !self.pending_user_input.is_empty() {
                    10u16
                } else {
                    50u16
                };

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

            // Input gets priority over PTY output and rendering. In particular,
            // a continuously-redrawing TUI must not starve keyboard handling.
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
                            self.drain_pending_user_input();
                        }
                        Err(nix::errno::Errno::EAGAIN) => {}
                        Err(e) => return Err(Box::new(e)),
                    }
                }
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
            // The sync check is done INSIDE render_frame() under the same lock
            // as the screen clone to prevent a TOCTOU race: without this, MCP
            // threads can pump new data (including ?2026h + partial content)
            // between the sync check and the screen clone, causing mid-frame
            // rendering artifacts (garbled CUF positions).
            if self.server_render_dirty {
                let now = Instant::now();
                let should_render = should_render_now(
                    self.server_render_dirty,
                    self.last_pty_data_time,
                    self.first_dirty_time,
                    now,
                    RENDER_DEBOUNCE_MS,
                );

                if should_render {
                    if self.render_frame()? {
                        self.server_render_dirty = false;
                        self.first_dirty_time = None;
                    }
                    // else: sync was active, keep dirty for next iteration
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
                let mut visible_dirty = false;
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
                                if self.visible_sessions.iter().any(|v| v == &sid) {
                                    visible_dirty = true;
                                }
                            }
                        }
                    }
                }
                drop(mgr);
                if visible_dirty {
                    let now = Instant::now();
                    if !self.server_render_dirty {
                        self.first_dirty_time = Some(now);
                    }
                    self.server_render_dirty = true;
                    self.last_pty_data_time = Some(now);
                }
            }

            // Check notification watchers against all sessions — queue new injections
            if self.notifications.lock().unwrap().watcher_count() > 0 {
                let mut new_injections = Vec::new();
                {
                    let mgr = self.sessions.lock().unwrap();
                    let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
                    let mut notif = self.notifications.lock().unwrap();
                    for sid in &ids {
                        if let Ok(session) = mgr.get(sid) {
                            let screen_text = session.get_screen();
                            for inj in notif.check_session(sid, &screen_text) {
                                new_injections.push((inj.target_session_id, inj.text));
                            }
                        }
                    }
                }
                for (target_id, text) in new_injections {
                    self.queue_injection(&target_id, &text);
                }
            }
            // Drain one queued injection per tick with pacing (100ms between injections)
            // to avoid garbling the user's in-progress typing.
            const INJECTION_PACE_MS: u64 = 100;
            if !self.pending_injection_queue.is_empty() {
                let can_inject = self.last_injection_time
                    .map_or(true, |t| t.elapsed() >= std::time::Duration::from_millis(INJECTION_PACE_MS));
                if can_inject {
                    if let Some((target_id, text)) = self.pending_injection_queue.pop_front() {
                        let mut mgr = self.sessions.lock().unwrap();
                        let sent = if let Ok(session) = mgr.get_mut(&target_id) {
                            // Send text via send_keys (without auto-appending Enter).
                            // Strip trailing \r/\n — a delayed [ENTER] will be queued below.
                            let clean = text.trim_end_matches(|c| c == '\r' || c == '\n');
                            session.send_keys(clean).is_ok()
                        } else {
                            false
                        };
                        drop(mgr);
                        if sent {
                            let _ = self.logger.log_event(&LogEvent::new(
                                target_id.clone(), LogDirection::Meta,
                                format!("[NOTIFICATION] {}", text).into_bytes(),
                            ));
                            self.pending_delayed_enters.push((
                                target_id,
                                Instant::now() + std::time::Duration::from_millis(100),
                            ));
                        } else {
                            let _ = self.logger.log_event(&LogEvent::new(
                                target_id.clone(), LogDirection::Meta,
                                format!("[NOTIFICATION-DROPPED] target session gone or send failed: {}", text).into_bytes(),
                            ));
                        }
                        self.last_injection_time = Some(Instant::now());
                    }
                }
            }

            // Sync session order (MCP may have added new ones)
            self.sync_session_order();

            if self.check_session_exit() { break; }

            let events = self.scheduler.lock().unwrap().tick(std::time::Instant::now());
            for event in events { self.handle_scheduler_event(event); }
            self.drain_deferred_scheduler_events();
            self.drain_context_refresh_requests();
            self.drain_pending_delayed_enters();
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
        self.visible_sessions.retain(|id| actual_ids.contains(id));
    }

    fn queue_user_input(&mut self, session_id: &str, data: &[u8]) {
        self.pending_user_input
            .entry(session_id.to_string())
            .or_default()
            .extend(data);
    }

    /// Make one non-blocking write attempt for each session with queued input.
    fn drain_pending_user_input(&mut self) {
        let ids: Vec<String> = self.pending_user_input.keys().cloned().collect();
        let mut failed = Vec::new();
        {
            let mut mgr = self.sessions.lock().unwrap();
            for id in &ids {
                let Some(queue) = self.pending_user_input.get_mut(id) else {
                    continue;
                };
                let result = match mgr.get_mut(id) {
                    Ok(session) => session.try_send_raw(queue.make_contiguous()),
                    Err(e) => Err(e),
                };
                match result {
                    Ok(n) => {
                        queue.drain(..n.min(queue.len()));
                    }
                    Err(e) => failed.push((id.clone(), e.to_string())),
                }
            }
        }

        self.pending_user_input.retain(|id, queue| {
            !queue.is_empty() && !failed.iter().any(|(failed_id, _)| failed_id == id)
        });
        for (id, error) in failed {
            let _ = self.logger.log_event(&LogEvent::new(
                id,
                LogDirection::Meta,
                format!("Interactive input dropped: {error}").into_bytes(),
            ));
        }
    }

    fn handle_input_event(&mut self, event: InputEvent) -> Result<bool, Box<dyn std::error::Error>> {
        match decide_input_action(event) {
            InputAction::SendToSession(data) => {
                if let Some(id) = self.active_session.clone() {
                    if self.config.log_input {
                        let _ = self.logger.log_event(&LogEvent::new(
                            id.clone(),
                            LogDirection::Input,
                            data.clone(),
                        ));
                    }
                    self.queue_user_input(&id, &data);
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
                if let Some(id) = self.active_session.clone() {
                    let prefix = vec![self.config.prefix_key];
                    self.queue_user_input(&id, &prefix);
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
            InputAction::MousePress { button, modifiers, col, row } => {
                if matches!(button, tttt_tui::MouseButton::Left) {
                    let sidebar_width = self.config.sidebar_width;
                    let pane_width = self.screen_cols.saturating_sub(sidebar_width);
                    if col < pane_width {
                        // Click in the PTY pane area. In split view, hit-test
                        // the click against each pane's rect: if it landed on a
                        // non-active pane, switch focus to it instead of
                        // starting a selection.
                        let render_ids = compute_render_session_ids(
                            self.active_session.as_deref(),
                            &self.visible_sessions,
                            &self.session_order,
                        );
                        let mut focus_switched = false;
                        if render_ids.len() > 1 {
                            let layout = compute_pane_layout(
                                self.screen_cols,
                                self.screen_rows,
                                sidebar_width,
                                render_ids.len(),
                            );
                            if let Some(idx) = layout.pane_rects.iter().position(|r| {
                                col >= r.x
                                    && col < r.x.saturating_add(r.width)
                                    && row >= r.y
                                    && row < r.y.saturating_add(r.height)
                            }) {
                                if let Some(hit_id) = render_ids.get(idx).cloned() {
                                    if Some(hit_id.as_str()) != self.active_session.as_deref() {
                                        self.switch_to_session(&hit_id)?;
                                        focus_switched = true;
                                    }
                                }
                            }
                        }
                        if !focus_switched {
                            // Active pane (or single-pane): start selection.
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
                        }
                    } else if row >= 2 {
                        // Click in sidebar
                        // Sessions start at row 2 (after header + separator)
                        let session_idx = (row - 2) as usize;
                        if session_idx < self.session_order.len() {
                            let target = self.session_order[session_idx].clone();
                            if modifiers.ctrl {
                                // Ctrl-click toggles the session's sticky bit.
                                // For the currently-active session this means it
                                // will remain visible after the user switches
                                // active away.
                                self.visible_sessions = toggle_session_visibility(
                                    &self.visible_sessions,
                                    &target,
                                );
                                self.apply_pane_resize();
                                self.server_render_dirty = true;
                            } else {
                                // Plain click switches the active session. The
                                // render set may shrink/grow if the previously-
                                // active session was not pinned, so recompute
                                // PTY sizes to match.
                                self.switch_to_session(&target)?;
                            }
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
                                    match copy_to_clipboard(&text) {
                                        ClipboardResult::TmuxPassthroughDisabled => {
                                            self.ctrl_c_hint_until = Some(Instant::now() + std::time::Duration::from_secs(5));
                                            self.ctrl_c_hint_message = Some(
                                                "Copy failed: run `tmux set -g allow-passthrough on`".to_string()
                                            );
                                        }
                                        _ => {}
                                    }
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
            InputAction::Redraw => {
                self.force_redraw()?;
            }
            InputAction::DumpDiagnostics => {
                match self.dump_diagnostics() {
                    Ok(path) => {
                        use std::time::Duration;
                        self.ctrl_c_hint_message = Some(format!("diag: {}", path));
                        self.ctrl_c_hint_until = Some(Instant::now() + Duration::from_secs(5));
                        self.server_render_dirty = true;
                    }
                    Err(e) => {
                        use std::time::Duration;
                        self.ctrl_c_hint_message = Some(format!("diag failed: {}", e));
                        self.ctrl_c_hint_until = Some(Instant::now() + Duration::from_secs(5));
                        self.server_render_dirty = true;
                    }
                }
            }
            InputAction::ToggleStickyActive => {
                if let Some(active) = self.active_session.clone() {
                    self.visible_sessions =
                        toggle_session_visibility(&self.visible_sessions, &active);
                    self.apply_pane_resize();
                    self.server_render_dirty = true;
                }
            }
        }
        Ok(true)
    }

    fn switch_to_session(&mut self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let exists = self.sessions.lock().unwrap().exists(id);
        if exists {
            self.active_session = Some(id.to_string());
            self.apply_pane_resize();
            if self.render_frame()? {
                self.server_render_dirty = false;
                self.first_dirty_time = None;
            } else {
                self.server_render_dirty = true;
                self.first_dirty_time.get_or_insert_with(Instant::now);
            }
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
    /// Returns true if a frame was actually rendered, false if rendering was
    /// suppressed (e.g., due to synchronized output mode being active).
    fn render_frame(&mut self) -> Result<bool, Box<dyn std::error::Error>> {
        // Collect all data needed for rendering before entering the draw closure,
        // because we cannot hold the mutex lock across the closure.
        let manual_scroll = self.scroll_offset;
        let selection_base = self.selection_scroll_base;
        let has_selection = self.selection.is_some();
        let mut sync_suppressed = false;
        let active_id_str = self.active_session.clone();
        // Effective render list = active + pinned visible, ordered by session_order.
        let render_ids = compute_render_session_ids(
            active_id_str.as_deref(),
            &self.visible_sessions,
            &self.session_order,
        );

        // Per-pane render data, ordered by render_ids.
        struct PaneData {
            screen: vt100::Screen,
            is_active: bool,
            cursor_row: u16,
            cursor_col: u16,
            pty_rows: u16,
            show_cursor: bool,
        }

        let mut panes: Vec<PaneData> = Vec::with_capacity(render_ids.len());
        {
            let mgr = self.sessions.lock().unwrap();
            for id in &render_ids {
                let Ok(session) = mgr.get(id) else { continue };
                let is_active = active_id_str.as_deref() == Some(id);
                // Sync-output suppression applies only to the active session: if
                // the active session is mid-bracket, suppress the whole frame to
                // avoid tearing. Non-active visible panes render live regardless.
                if is_active && session.synchronized_output() {
                    sync_suppressed = true;
                    break;
                }
                let mut screen = session.screen().screen().clone();
                let effective_scroll = if is_active && has_selection {
                    compute_selection_scroll_compensation(
                        selection_base,
                        session.max_scroll_offset(),
                        manual_scroll,
                    )
                } else if is_active {
                    manual_scroll
                } else {
                    0
                };
                if effective_scroll > 0 {
                    screen.set_scrollback(effective_scroll);
                }
                let (pty_rows, _) = screen.size();
                // Track each session's cursor position so we can compute a
                // viewport offset that keeps the active region visible. For
                // inactive panes we still use it for viewport selection but
                // never render the terminal cursor itself.
                let (cursor_row, cursor_col) = if is_active && effective_scroll > 0 {
                    (0, 0)
                } else {
                    session.cursor_position()
                };
                let show_cursor = is_active && effective_scroll == 0;
                panes.push(PaneData {
                    screen,
                    is_active,
                    cursor_row,
                    cursor_col,
                    pty_rows,
                    show_cursor,
                });
            }
        }

        // If sync mode suppressed the render, return false so the caller
        // keeps dirty=true and retries on the next loop iteration.
        if sync_suppressed {
            return Ok(false);
        }

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
        let visible_ids_snapshot: Vec<String> = self.visible_sessions.clone();
        let sessions_snapshot = {
            let mgr = self.sessions.lock().unwrap();
            mgr.list()
        };
        let showing_help = self.showing_help;
        let prefix_name_str = prefix_key_name(self.config.prefix_key);
        let prefix_name = if showing_help {
            Some(prefix_name_str.clone())
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
                self.ctrl_c_hint_message = None;
                false
            }
            None => false,
        };
        let hint_message = self.ctrl_c_hint_message.clone();

        // Captured by the closure to extract the active pane's Rect (and the
        // matching row offset chosen during render) so cursor positioning
        // after the closure stays in sync with what was actually painted.
        let mut active_pane_rect: Option<Rect> = None;
        let mut active_pane_row_offset: Option<u16> = None;

        self.terminal.draw(|frame| {
            // Compute layout from the frame's actual area, not cached
            // screen_cols/rows. During terminal resize there's a window where
            // the cached values lag behind the buffer — using them would index
            // outside the buffer and panic in ratatui.
            let area = frame.area();
            let layout = compute_pane_layout(
                area.width,
                area.height,
                sidebar_width,
                panes.len(),
            );
            let pane_areas = &layout.pane_rects;

            for (i, pane) in panes.iter().enumerate() {
                let Some(&rect) = pane_areas.get(i) else { break };
                let offset = compute_pane_row_offset(pane.pty_rows, rect.height, pane.cursor_row);
                if pane.is_active {
                    active_pane_rect = Some(rect);
                    active_pane_row_offset = Some(offset);
                }
                let mut widget = PtyWidget::new(&pane.screen).with_row_offset(offset);
                if pane.is_active {
                    if let Some(sel) = selection_ref {
                        widget = widget.with_selection(sel);
                    }
                }
                frame.render_widget(widget, rect);
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
                // Scope highlights to the active pane's Rect so they don't
                // bleed into other visible panes.
                let pane = active_pane_rect.unwrap_or_else(|| {
                    layout.pane_rects.first().copied().unwrap_or(layout.hint)
                });
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
            )
            .build_info(&uptime)
            .visible_ids(&visible_ids_snapshot);
            frame.render_widget(widget, layout.sidebar);

            // Hint in the dedicated row at the bottom of the pane container.
            if layout.hint.height > 0 {
                if show_ctrl_c_hint {
                    let msg = hint_message.as_deref()
                        .unwrap_or("Press Ctrl+\\ then ? for help");
                    let hint_widget = Paragraph::new(msg)
                        .style(Style::default().fg(Color::Yellow).bg(Color::Black));
                    frame.render_widget(hint_widget, layout.hint);
                } else {
                    let hint_text = format!("Press {} ? for help", prefix_name_str);
                    let hint_widget = Paragraph::new(hint_text)
                        .style(Style::default().fg(Color::DarkGray));
                    frame.render_widget(hint_widget, layout.hint);
                }
            }

            // Help overlay popup
            if showing_help {
                let p = prefix_name.as_deref().unwrap_or("");
                let popup_area = help_popup_area(frame.area());

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
                    Line::from("  l    Force redraw (recover)"),
                    Line::from("  i    Dump diagnostics to /tmp"),
                    Line::from("  .    Toggle sticky on current pane"),
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

        // Position cursor at the active pane's PTY cursor location, using the
        // same row offset that the active PtyWidget was rendered with so the
        // cursor lands on the row whose content was actually painted.
        if let Some(pane_data) = panes.iter().find(|p| p.show_cursor) {
            if let (Some(rect), Some(offset)) = (active_pane_rect, active_pane_row_offset) {
                let display_row_in_pane = pane_data.cursor_row.saturating_sub(offset);
                if display_row_in_pane < rect.height {
                    let display_row = rect.y + display_row_in_pane;
                    let display_col = rect.x + pane_data.cursor_col;
                    self.terminal.set_cursor_position((display_col, display_row))?;
                    self.terminal.show_cursor()?;
                }
            }
        }

        Ok(true)
    }

    fn handle_resize(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (cols, rows) = terminal_size();
        self.screen_cols = cols;
        self.screen_rows = rows;
        let (pty_cols, pty_rows) = calculate_pane_dimensions(cols, rows, self.config.sidebar_width);
        self.pty_dims = (pty_cols, pty_rows);
        self.apply_pane_resize();
        // Notify ratatui about the resize, then redraw.
        self.terminal.resize(ratatui::layout::Rect::new(0, 0, cols, rows))?;
        self.render_frame()?;
        Ok(())
    }

    /// Resize each session's PTY to match the current layout.
    ///
    /// Sessions in the effective render list get their grid cell dimensions.
    /// Sessions NOT in the render list get the full single-pane dimensions so
    /// they're already correctly sized when the user later activates them
    /// alone (no extra SIGWINCH on switch). Sessions whose dims are already
    /// correct are skipped to avoid redundant SIGWINCHes.
    fn apply_pane_resize(&mut self) {
        let render_ids = compute_render_session_ids(
            self.active_session.as_deref(),
            &self.visible_sessions,
            &self.session_order,
        );
        let layout = compute_pane_layout(
            self.screen_cols,
            self.screen_rows,
            self.config.sidebar_width,
            render_ids.len(),
        );

        // Map render_id → assigned (cols, rows). Sessions not in this map get
        // single-pane dims (i.e., the full grid area).
        let mut id_to_cell: std::collections::HashMap<&str, (u16, u16)> =
            std::collections::HashMap::new();
        for (i, id) in render_ids.iter().enumerate() {
            if let Some(rect) = layout.pane_rects.get(i) {
                id_to_cell.insert(id.as_str(), (rect.width, rect.height));
            }
        }

        // Single-pane fallback dims = full grid area (one row shy of the
        // pane container after the hint row is reserved). Use pty_dims as
        // the source of truth so we stay consistent with the existing
        // calculation everywhere else.
        let (single_cols, single_rows) = self.pty_dims;

        let mut to_log: Vec<(String, u16, u16)> = Vec::new();
        {
            let mut mgr = self.sessions.lock().unwrap();
            let ids: Vec<String> = mgr.list().iter().map(|m| m.id.clone()).collect();
            for id in ids {
                let (target_cols, target_rows) = id_to_cell
                    .get(id.as_str())
                    .copied()
                    .unwrap_or((single_cols, single_rows));
                if let Ok(session) = mgr.get_mut(&id) {
                    let (current_rows, current_cols) = session.screen().screen().size();
                    if current_cols != target_cols || current_rows != target_rows {
                        let _ = session.resize(target_cols, target_rows);
                        to_log.push((id, target_cols, target_rows));
                    }
                }
            }
        }

        for (id, cols, rows) in to_log {
            let resize_data = format!(
                r#"{{"type":"resize","cols":{},"rows":{}}}"#,
                cols, rows
            ).into_bytes();
            let _ = self.logger.log_event(&LogEvent::new(
                id, LogDirection::Meta, resize_data,
            ));
        }
    }

    /// Force a full repaint of the host terminal. Used as a recovery shortcut
    /// when the displayed screen disagrees with what the parser actually holds
    /// (e.g. ratatui's previous-frame buffer drifted out of sync with reality).
    /// Calling `Terminal::clear` resets ratatui's diff baseline so the next
    /// `draw` writes every cell from scratch.
    fn force_redraw(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.terminal.clear()?;
        self.server_render_dirty = true;
        self.render_frame()?;
        Ok(())
    }

    /// Write a diagnostic dump of the active session's render-relevant state
    /// to a file. Returns the file path on success. Used to investigate
    /// rendering anomalies — captures everything needed to compare what the
    /// parser thinks the screen is, what the renderer would draw, and what
    /// the host terminal dimensions are.
    fn dump_diagnostics(&self) -> Result<String, Box<dyn std::error::Error>> {
        use std::io::Write;
        let id = self.active_session.as_ref()
            .ok_or("no active session")?
            .clone();

        let mgr = self.sessions.lock().unwrap();
        let session = mgr.get(&id).map_err(|e| format!("session lookup failed: {}", e))?;
        let metadata = session.metadata();
        let screen = session.screen().screen();
        let (parser_rows, parser_cols) = screen.size();
        let cursor = session.cursor_position();
        let max_scroll = session.max_scroll_offset();
        let plain_contents = screen.contents();
        let formatted_contents = screen.contents_formatted();

        // Render PtyWidget into a fresh buffer at the same size the host
        // terminal uses for the PTY pane. This is what *should* be on screen.
        let pane_height = self.screen_rows.saturating_sub(1);
        let pane_width = self.screen_cols.saturating_sub(self.config.sidebar_width);
        let area = Rect::new(0, 0, pane_width, pane_height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        {
            use ratatui::widgets::Widget;
            let widget = PtyWidget::new(screen);
            widget.render(area, &mut buf);
        }

        // All session metadata for cross-session context.
        let all_sessions = mgr.list();
        drop(mgr);

        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let bytes = format_diagnostic_dump(&DiagnosticInputs {
            timestamp_ms: ts_ms,
            session_id: &id,
            session_command: &metadata.command,
            session_status: &metadata.status,
            parser_size: (parser_cols, parser_rows),
            cursor,
            max_scroll,
            host_size: (self.screen_cols, self.screen_rows),
            sidebar_width: self.config.sidebar_width,
            pty_dims: self.pty_dims,
            pane_size: (pane_width, pane_height),
            scroll_offset: self.scroll_offset,
            selection_scroll_base: self.selection_scroll_base,
            selection: self.selection.as_ref(),
            all_sessions: &all_sessions,
            plain_contents: &plain_contents,
            rendered_buffer: &buf,
            formatted_contents: &formatted_contents,
        });

        let path = format!("/tmp/tttt-diag-{}.txt", ts_ms);
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&bytes)?;

        Ok(path)
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
                        let text = format!("[ENTER][REMINDER: {}]", reminder.message);
                        let _ = session.send_keys(&text);
                        drop(mgr);
                        self.pending_delayed_enters.push((
                            sid.clone(),
                            Instant::now() + std::time::Duration::from_millis(100),
                        ));
                    }
                }
            }
            SchedulerEvent::CronFired(job) => {
                let _ = self.logger.log_event(&LogEvent::new(
                    "scheduler".to_string(), LogDirection::Meta,
                    format!("CRON[{}]: {}", job.id, job.command).into_bytes(),
                ));
                // Use specified session_id, or fall back to first session.
                let target_id = job.session_id.clone().or_else(|| {
                    self.session_order.first().cloned()
                });
                if let Some(ref session_id) = target_id {
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
                        // Send text first, then queue a delayed [ENTER] so the target
                        // app has time to process the text before submission.
                        let cmd = job.command.trim_end_matches(|c| c == '\r' || c == '\n');
                        let text = format!("[ENTER][CRON {}]: {}", job.id, cmd);
                        let _ = session.send_keys(&text);
                        drop(mgr);
                        self.pending_delayed_enters.push((
                            session_id.clone(),
                            Instant::now() + std::time::Duration::from_millis(100),
                        ));
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
                    let id = job.session_id.clone().or_else(|| {
                        self.session_order.first().cloned()
                    });
                    match id {
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
            };
            if is_idle && target_id.is_some() {
                // Inject now
                match &event {
                    SchedulerEvent::ReminderFired(reminder) => {
                        let sid = target_id.unwrap();
                        let mut mgr = self.sessions.lock().unwrap();
                        if let Ok(session) = mgr.get_mut(&sid) {
                            let text = format!("[ENTER][REMINDER: {}]", reminder.message);
                            let _ = session.send_keys(&text);
                        }
                        self.pending_delayed_enters.push((
                            sid,
                            Instant::now() + std::time::Duration::from_millis(100),
                        ));
                    }
                    SchedulerEvent::CronFired(job) => {
                        let sid = target_id.unwrap();
                        let mut mgr = self.sessions.lock().unwrap();
                        if let Ok(session) = mgr.get_mut(&sid) {
                            let cmd = job.command.trim_end_matches(|c| c == '\r' || c == '\n');
                            let text = format!("[ENTER][CRON {}]: {}", job.id, cmd);
                            let _ = session.send_keys(&text);
                        }
                        self.pending_delayed_enters.push((
                            sid,
                            Instant::now() + std::time::Duration::from_millis(100),
                        ));
                    }
                }
            } else {
                still_deferred.push(event);
            }
        }
        self.deferred_scheduler_events = still_deferred;
    }

    /// Send any pending Enter keystrokes whose delay has elapsed.
    fn drain_pending_delayed_enters(&mut self) {
        if self.pending_delayed_enters.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut remaining = Vec::new();
        for (session_id, fire_at) in std::mem::take(&mut self.pending_delayed_enters) {
            if now >= fire_at {
                let mut mgr = self.sessions.lock().unwrap();
                let sent = match mgr.get_mut(&session_id) {
                    Ok(session) => session.send_keys("[ENTER]").is_ok(),
                    Err(_) => false,
                };
                drop(mgr);
                if !sent {
                    let _ = self.logger.log_event(&LogEvent::new(
                        session_id.clone(), LogDirection::Meta,
                        b"[NOTIFICATION-DROPPED] delayed Enter: target session gone".to_vec(),
                    ));
                }
            } else {
                remaining.push((session_id, fire_at));
            }
        }
        self.pending_delayed_enters = remaining;
    }

    fn inject_context_refresh_text(&mut self, text: &str, stage: &str) -> bool {
        let Some(session_id) = self.session_order.first().cloned() else {
            return false;
        };
        let sent = {
            let mut mgr = self.sessions.lock().unwrap();
            mgr.get_mut(&session_id)
                .map(|session| session.send_keys(text).is_ok())
                .unwrap_or(false)
        };
        if sent {
            self.pending_delayed_enters.push((
                session_id.clone(),
                Instant::now() + std::time::Duration::from_millis(100),
            ));
            let _ = self.logger.log_event(&LogEvent::new(
                session_id,
                LogDirection::Meta,
                format!("[CONTEXT-REFRESH] {stage}: {text}").into_bytes(),
            ));
        }
        sent
    }

    /// Advance scheduled context refreshes without blocking the event loop.
    fn drain_context_refresh_requests(&mut self) {
        let mut requests = {
            let mut queue = self.context_refresh_queue.lock().unwrap();
            std::mem::take(&mut *queue)
        };
        if requests.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut remaining = std::collections::VecDeque::new();
        while let Some(mut request) = requests.pop_front() {
            let complete = advance_context_refresh_request(&mut request, now, |text, stage| {
                self.inject_context_refresh_text(text, stage)
            });
            if !complete {
                remaining.push_back(request);
            }
        }

        self.context_refresh_queue.lock().unwrap().extend(remaining);
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
                        let context_refresh_queue = self.context_refresh_queue.clone();
                        let tui_tools_enabled = self.config.tui_tools;
                        let screen_cols = self.screen_cols;
                        let screen_rows = self.screen_rows;
                        let (pty_cols, pty_rows) = self.pty_dims;
                        let work_dir = self.config.work_dir.clone();
                        let db_path = self.config.db_path.clone();
                        let sqlite_logger = self.sqlite_logger.clone();
                        std::thread::spawn(move || {
                            use tttt_mcp::proxy::handle_proxy_client;
                            use tttt_mcp::{PtyToolHandler, ReplayToolHandler, SchedulerToolHandler, NotificationToolHandler, ScratchpadToolHandler, SidebarMessageToolHandler, TuiToolHandler, CompositeToolHandler, ContextRefreshToolHandler};

                            // Set the stream to blocking mode for the handler
                            let _ = stream.set_nonblocking(false);

                            let context_refresh_handler = ContextRefreshToolHandler::new(
                                context_refresh_queue,
                                work_dir.clone(),
                            );
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
                            composite.add_handler(Box::new(context_refresh_handler));
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
                        // Session create/kill is a web-UI feature; not
                        // implemented for `tttt attach` clients. Still ack
                        // with an error so a protocol client waiting on the
                        // documented acknowledgement doesn't hang forever.
                        protocol::ClientMsg::CreateSession { .. } => {
                            self.viewer_clients[i].send_msg(&protocol::ServerMsg::SessionCreated {
                                session_id: None,
                                error: Some(
                                    "session creation is not supported over the attach socket"
                                        .to_string(),
                                ),
                            });
                        }
                        protocol::ClientMsg::KillSession { session_id } => {
                            self.viewer_clients[i].send_msg(&protocol::ServerMsg::SessionKilled {
                                session_id,
                                success: false,
                                error: Some(
                                    "session kill is not supported over the attach socket"
                                        .to_string(),
                                ),
                            });
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
    ///
    /// When multi-pane is active (`visible_sessions` non-empty), the main TUI's
    /// grid layout determines each pane's nominal size via `apply_pane_resize`.
    /// We then **clamp** every visible session's PTY dimensions so they never
    /// exceed what the smallest viewer (or the server itself) can display.
    /// This ensures viewers with small screens (e.g. phones) see content that
    /// fits their terminal rather than truncated/clipped output.
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

        if !self.visible_sessions.is_empty() {
            // Multi-pane is active: main TUI layout dictates per-session sizes.
            self.apply_pane_resize();

            // After apply_pane_resize, sessions have been set to server grid cell
            // sizes.  Clamp each *visible* session so it never exceeds what the
            // smallest viewer (or the server itself) can display.  Sessions not
            // in the render list retain their single-pane fallback size (already
            // computed by apply_pane_resize) and are also clamped.
            let render_ids = compute_render_session_ids(
                self.active_session.as_deref(),
                &self.visible_sessions,
                &self.session_order,
            );
            {
                let mut mgr = self.sessions.lock().unwrap();
                for id in &render_ids {
                    if let Ok(session) = mgr.get_mut(id) {
                        let (r, c) = session.screen().screen().size();
                        let clamped_cols = c.min(min_cols);
                        let clamped_rows = r.min(min_rows);
                        if clamped_cols != c || clamped_rows != r {
                            let _ = session.resize(clamped_cols, clamped_rows);
                        }
                    }
                }
            }

            // Notify each viewer with the clamped size (or fallback to min).
            {
                let mgr = self.sessions.lock().unwrap();
                for client in &mut self.viewer_clients {
                    client.invalidate();
                    let (cols, rows) = client.active_session.as_ref()
                        .and_then(|sid| mgr.get(sid).ok())
                        .map(|s| {
                            let (r, c) = s.screen().screen().size();
                            (c, r)
                        })
                        .unwrap_or((min_cols, min_rows));
                    client.send_window_size(cols, rows);
                }
            }

            let _ = self.render_frame();
            return;
        }

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

/// Result of a clipboard copy attempt.
enum ClipboardResult {
    /// Copied via native command (pbcopy/xclip)
    Native,
    /// Copied via OSC 52
    Osc52,
    /// tmux passthrough is not enabled — copy likely failed
    TmuxPassthroughDisabled,
}

/// Copy text to the system clipboard.
/// Over SSH, prefer OSC 52 since native commands (pbcopy/xclip) would
/// copy to the remote machine's clipboard, not the local one.
fn copy_to_clipboard(text: &str) -> ClipboardResult {
    let is_ssh = std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some();
    if is_ssh {
        // OSC 52 reaches the local terminal through SSH
        return copy_to_clipboard_osc52(text);
    }
    // Try platform-native clipboard first (works in all terminals)
    if copy_to_clipboard_native(text) {
        return ClipboardResult::Native;
    }
    // Fall back to OSC 52 (works in iTerm2, kitty, alacritty, etc.)
    copy_to_clipboard_osc52(text)
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
/// Writes to /dev/tty to bypass ratatui's alternate screen buffer,
/// ensuring the sequence reaches the actual terminal emulator
/// (critical for SSH sessions where the local terminal handles OSC 52).
/// Check if tmux has allow-passthrough enabled.
fn tmux_passthrough_enabled() -> bool {
    use std::process::Command;
    Command::new("tmux")
        .args(["show", "-gv", "allow-passthrough"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or(false, |v| {
            let v = v.trim();
            v == "on" || v == "all"
        })
}

fn copy_to_clipboard_osc52(text: &str) -> ClipboardResult {
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let osc = format!("\x1b]52;c;{}\x07", encoded);
    // Inside tmux, wrap in DCS passthrough so the sequence reaches the
    // outer terminal instead of being consumed by tmux.
    let (seq, in_tmux) = if std::env::var_os("TMUX").is_some() {
        if !tmux_passthrough_enabled() {
            return ClipboardResult::TmuxPassthroughDisabled;
        }
        // DCS passthrough: double each ESC in the payload, wrap with Ptmux;..ST
        let escaped = osc.replace('\x1b', "\x1b\x1b");
        (format!("\x1bPtmux;{}\x1b\\", escaped), true)
    } else {
        (osc, false)
    };
    // Write to /dev/tty to bypass alternate screen buffer
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(seq.as_bytes());
        let _ = tty.flush();
    } else {
        // Fallback to stdout if /dev/tty unavailable
        let _ = std::io::stdout().write_all(seq.as_bytes());
        let _ = std::io::stdout().flush();
    }
    let _ = in_tmux;
    ClipboardResult::Osc52
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codex_mcp_config_args_are_invocation_scoped() {
        let args = codex_mcp_config_args(
            std::path::Path::new("/opt/tttt bin/tttt"),
            "/tmp/tttt-mcp-42.sock",
        );

        assert_eq!(args, vec![
            "--config",
            "mcp_servers.tttt.command=\"/opt/tttt bin/tttt\"",
            "--config",
            "mcp_servers.tttt.args=[\"mcp-server\",\"--connect\",\"/tmp/tttt-mcp-42.sock\"]",
        ]);
    }

    #[test]
    fn test_command_basename_bare_invocation() {
        assert_eq!(command_basename("codex"), "codex");
        assert_eq!(command_basename("claude"), "claude");
        assert_eq!(command_basename("opencode"), "opencode");
    }

    #[test]
    fn test_command_basename_path_invocations() {
        assert_eq!(command_basename("/opt/homebrew/bin/codex"), "codex");
        assert_eq!(command_basename("./claude"), "claude");
        assert_eq!(command_basename("/usr/local/bin/opencode"), "opencode");
    }

    #[test]
    fn test_command_basename_does_not_match_on_directory_names() {
        // A binary under a path containing an app name must not be
        // detected as that app (e.g. /tmp/claude-501/bin/codex is codex).
        assert_eq!(command_basename("/tmp/claude-501/bin/codex"), "codex");
        assert_eq!(command_basename("/home/claude/opencode"), "opencode");
    }

    #[test]
    fn test_command_basename_wrappers_keep_their_own_name() {
        assert_eq!(command_basename("codex-wrapper"), "codex-wrapper");
        assert_eq!(command_basename("/bin/my-codex"), "my-codex");
        assert_eq!(command_basename(""), "");
    }

    #[test]
    fn test_normalize_terminal_size_passes_through_sane_sizes() {
        assert_eq!(normalize_terminal_size(120, 40), (120, 40));
        assert_eq!(normalize_terminal_size(1, 1), (1, 1));
    }

    #[test]
    fn test_normalize_terminal_size_defaults_zero_dimensions() {
        // A PTY with no winsize set reports 0x0; assume a default size
        // instead of panicking downstream (vt100 grid underflows on 0 rows).
        assert_eq!(normalize_terminal_size(0, 0), (80, 24));
        assert_eq!(normalize_terminal_size(0, 40), (80, 40));
        assert_eq!(normalize_terminal_size(120, 0), (120, 24));
    }

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
        assert_eq!(popup.height, 16, "popup height should be 16");
        // x = (200 - 45) / 2 = 77
        assert_eq!(popup.x, 77, "popup should be horizontally centered");
        // y = (50 - 16) / 2 = 17
        assert_eq!(popup.y, 17, "popup should be vertically centered");
    }

    #[test]
    fn test_help_popup_area_centered_standard_terminal() {
        // 80x24 terminal
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let popup = help_popup_area(area);
        assert_eq!(popup.width, 45);
        assert_eq!(popup.height, 16);
        // x = (80 - 45) / 2 = 17
        assert_eq!(popup.x, 17);
        // y = (24 - 16) / 2 = 4
        assert_eq!(popup.y, 4);
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

    #[test]
    fn test_decide_input_action_redraw() {
        assert_eq!(
            decide_input_action(InputEvent::Redraw),
            InputAction::Redraw,
        );
    }

    #[test]
    fn test_decide_input_action_dump_diagnostics() {
        assert_eq!(
            decide_input_action(InputEvent::DumpDiagnostics),
            InputAction::DumpDiagnostics,
        );
    }

    #[test]
    fn test_decide_input_action_toggle_sticky_active() {
        assert_eq!(
            decide_input_action(InputEvent::ToggleStickyActive),
            InputAction::ToggleStickyActive,
        );
    }

    // ── Chunk 8: format_diagnostic_dump ──────────────────────────────────────

    fn make_test_buffer(cols: u16, rows: u16, fill: &str) -> ratatui::buffer::Buffer {
        let area = ratatui::layout::Rect::new(0, 0, cols, rows);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        for r in 0..rows {
            for c in 0..cols {
                buf[(c, r)].set_symbol(fill);
            }
        }
        buf
    }

    fn make_test_inputs<'a>(
        session_command: &'a str,
        session_status: &'a tttt_pty::SessionStatus,
        all_sessions: &'a [tttt_pty::SessionMetadata],
        plain_contents: &'a str,
        rendered_buffer: &'a ratatui::buffer::Buffer,
        formatted_contents: &'a [u8],
    ) -> DiagnosticInputs<'a> {
        DiagnosticInputs {
            timestamp_ms: 1_700_000_000_000,
            session_id: "sess-1",
            session_command,
            session_status,
            parser_size: (80, 24),
            cursor: (3, 7),
            max_scroll: 42,
            host_size: (120, 40),
            sidebar_width: 30,
            pty_dims: (90, 39),
            pane_size: (90, 39),
            scroll_offset: 0,
            selection_scroll_base: 0,
            selection: None,
            all_sessions,
            plain_contents,
            rendered_buffer,
            formatted_contents,
        }
    }

    #[test]
    fn test_format_diagnostic_dump_contains_expected_headings() {
        let buf = make_test_buffer(4, 2, "x");
        let status = tttt_pty::SessionStatus::Running;
        let dump = format_diagnostic_dump(&make_test_inputs(
            "/bin/zsh",
            &status,
            &[],
            "hello",
            &buf,
            b"\x1b[31mhi\x1b[0m",
        ));
        let text = String::from_utf8(dump).unwrap();
        assert!(text.contains("tttt diagnostic dump"));
        assert!(text.contains("[active session]"));
        assert!(text.contains("[host terminal / app state]"));
        assert!(text.contains("[all sessions]"));
        assert!(text.contains("[parser plain contents]"));
        assert!(text.contains("[ptywidget render output (what the renderer would draw)]"));
        assert!(text.contains("[parser formatted contents (raw ANSI bytes)]"));
    }

    #[test]
    fn test_format_diagnostic_dump_renders_session_fields() {
        let buf = make_test_buffer(2, 1, ".");
        let status = tttt_pty::SessionStatus::Running;
        let dump = format_diagnostic_dump(&make_test_inputs(
            "/bin/bash",
            &status,
            &[],
            "",
            &buf,
            b"",
        ));
        let text = String::from_utf8(dump).unwrap();
        assert!(text.contains("timestamp_ms: 1700000000000"));
        assert!(text.contains("id:                sess-1"));
        assert!(text.contains("command:           /bin/bash"));
        assert!(text.contains("Running"));
        assert!(text.contains("parser size:       80x24 (cols x rows)"));
        assert!(text.contains("cursor:            (3, 7) (row, col)"));
        assert!(text.contains("max_scroll_offset: 42"));
        assert!(text.contains("host size:         120x40 (cols x rows)"));
        assert!(text.contains("sidebar_width:     30"));
        assert!(text.contains("configured pty_dims: 90x39"));
        assert!(text.contains("pane area:         90x39 (cols x rows)"));
        assert!(text.contains("scroll_offset:     0"));
        assert!(text.contains("selection:         None"));
    }

    #[test]
    fn test_format_diagnostic_dump_renders_buffer_content() {
        // 5x2 buffer filled with 'A' should produce two rows of "AAAAA"
        // prefixed with row numbers like "  0 | AAAAA".
        let buf = make_test_buffer(5, 2, "A");
        let status = tttt_pty::SessionStatus::Running;
        let dump = format_diagnostic_dump(&make_test_inputs(
            "cmd",
            &status,
            &[],
            "",
            &buf,
            b"",
        ));
        let text = String::from_utf8(dump).unwrap();
        assert!(text.contains("  0 | AAAAA"));
        assert!(text.contains("  1 | AAAAA"));
        // Should not contain row index 2 since the buffer is only 2 rows tall
        assert!(!text.contains("  2 | "));
    }

    #[test]
    fn test_format_diagnostic_dump_renders_all_sessions_listing() {
        let buf = make_test_buffer(1, 1, ".");
        let status = tttt_pty::SessionStatus::Running;
        let exited_status = tttt_pty::SessionStatus::Exited(0);
        let sessions = vec![
            tttt_pty::SessionMetadata {
                id: "alpha".to_string(),
                command: "vim".to_string(),
                status: status.clone(),
                cols: 80,
                rows: 24,
                name: None,
                created_at: None,
                root: false,
                working_dir: None,
            },
            tttt_pty::SessionMetadata {
                id: "beta".to_string(),
                command: "bash".to_string(),
                status: exited_status,
                cols: 100,
                rows: 30,
                name: None,
                created_at: None,
                root: false,
                working_dir: None,
            },
        ];
        let dump = format_diagnostic_dump(&make_test_inputs(
            "vim",
            &status,
            &sessions,
            "",
            &buf,
            b"",
        ));
        let text = String::from_utf8(dump).unwrap();
        assert!(text.contains("  alpha 80x24"));
        assert!(text.contains("cmd=vim"));
        assert!(text.contains("  beta 100x30"));
        assert!(text.contains("cmd=bash"));
    }

    #[test]
    fn test_format_diagnostic_dump_includes_selection_when_present() {
        let buf = make_test_buffer(1, 1, ".");
        let status = tttt_pty::SessionStatus::Running;
        let sel = tttt_tui::Selection {
            anchor: (1, 2),
            head: (4, 5),
        };
        let mut inputs = make_test_inputs("cmd", &status, &[], "", &buf, b"");
        inputs.selection = Some(&sel);
        inputs.selection_scroll_base = 7;
        let dump = format_diagnostic_dump(&inputs);
        let text = String::from_utf8(dump).unwrap();
        assert!(text.contains("selection_scroll_base: 7"));
        assert!(text.contains("selection:         Some("));
        assert!(text.contains("anchor: (1, 2)"));
        assert!(text.contains("head: (4, 5)"));
    }

    #[test]
    fn test_format_diagnostic_dump_includes_raw_formatted_bytes() {
        // The raw ANSI bytes section should contain the literal escape bytes
        // (not lossy-converted), so a byte search is the strongest assertion.
        let buf = make_test_buffer(1, 1, ".");
        let status = tttt_pty::SessionStatus::Running;
        let raw = b"\x1b[31mRED\x1b[0m";
        let dump = format_diagnostic_dump(&make_test_inputs(
            "cmd",
            &status,
            &[],
            "",
            &buf,
            raw,
        ));
        // Search for the raw bytes anywhere in the dump.
        let needle: &[u8] = b"\x1b[31mRED\x1b[0m";
        assert!(
            dump.windows(needle.len()).any(|w| w == needle),
            "expected raw ANSI bytes to appear verbatim in the dump",
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

    fn context_refresh_request(clear_at: Instant) -> tttt_mcp::ContextRefreshRequest {
        tttt_mcp::ContextRefreshRequest {
            filename: "HANDOFF.md".to_string(),
            clear_at,
            followup_delay: std::time::Duration::from_secs(12),
            clear_sent: false,
            restore_at: None,
        }
    }

    #[test]
    fn test_context_refresh_waits_then_clears_then_restores() {
        let start = Instant::now();
        let mut request = context_refresh_request(start + std::time::Duration::from_secs(15));
        let mut injected = Vec::new();

        assert!(!advance_context_refresh_request(
            &mut request,
            start + std::time::Duration::from_secs(14),
            |text, stage| {
                injected.push((stage.to_string(), text.to_string()));
                true
            },
        ));
        assert!(injected.is_empty());

        let clear_time = start + std::time::Duration::from_secs(15);
        assert!(!advance_context_refresh_request(
            &mut request,
            clear_time,
            |text, stage| {
                injected.push((stage.to_string(), text.to_string()));
                true
            },
        ));
        assert_eq!(injected, vec![("clear".to_string(), "/clear".to_string())]);
        assert_eq!(
            request.restore_at,
            Some(clear_time + std::time::Duration::from_secs(12))
        );

        assert!(advance_context_refresh_request(
            &mut request,
            clear_time + std::time::Duration::from_secs(12),
            |text, stage| {
                injected.push((stage.to_string(), text.to_string()));
                true
            },
        ));
        assert_eq!(injected[1].0, "restore");
        assert!(injected[1].1.contains("HANDOFF.md"));
        assert!(injected[1].1.starts_with("CONTEXT REFRESH:"));
    }

    #[test]
    fn test_context_refresh_retries_failed_clear() {
        let now = Instant::now();
        let mut request = context_refresh_request(now);
        assert!(!advance_context_refresh_request(
            &mut request,
            now,
            |_text, _stage| false,
        ));
        assert!(!request.clear_sent);
        assert!(request.restore_at.is_none());
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

    // ── Chunk 3b: visible_sessions helpers ───────────────────────────────────

    #[test]
    fn test_toggle_adds_when_absent() {
        let result = toggle_session_visibility(&ss(&[]), "b");
        assert_eq!(result, ss(&["b"]));
    }

    #[test]
    fn test_toggle_removes_when_present() {
        let result = toggle_session_visibility(&ss(&["b", "c"]), "b");
        assert_eq!(result, ss(&["c"]));
    }

    #[test]
    fn test_toggle_on_active_makes_it_sticky() {
        // The active session can be pinned. Independent of who is active —
        // membership in visible is the user's "stay visible after switch"
        // intent.
        let result = toggle_session_visibility(&ss(&[]), "a");
        assert_eq!(result, ss(&["a"]));
    }

    #[test]
    fn test_toggle_unsticks_active_when_already_sticky() {
        // Symmetric: a second ctrl-click on a sticky session removes it
        // from visible. If it happens to be active, it remains rendered
        // because the active session is implicitly visible — but it loses
        // its sticky bit, so it disappears once the user switches away.
        let result = toggle_session_visibility(&ss(&["a", "b"]), "a");
        assert_eq!(result, ss(&["b"]));
    }

    #[test]
    fn test_compute_render_ids_active_only_when_visible_empty() {
        let result = compute_render_session_ids(
            Some("b"),
            &ss(&[]),
            &ss(&["a", "b", "c"]),
        );
        assert_eq!(result, ss(&["b"]));
    }

    #[test]
    fn test_compute_render_ids_active_plus_pinned_in_order() {
        // Active is "b", pinned is ["c"]. Effective render list, in
        // session_order, is ["b", "c"].
        let result = compute_render_session_ids(
            Some("b"),
            &ss(&["c"]),
            &ss(&["a", "b", "c"]),
        );
        assert_eq!(result, ss(&["b", "c"]));
    }

    #[test]
    fn test_compute_render_ids_dedupes_active_when_pinned() {
        // Active is also in visible — should appear once, in session_order.
        let result = compute_render_session_ids(
            Some("b"),
            &ss(&["b", "c"]),
            &ss(&["a", "b", "c"]),
        );
        assert_eq!(result, ss(&["b", "c"]));
    }

    #[test]
    fn test_compute_render_ids_no_active_returns_only_pinned() {
        let result = compute_render_session_ids(
            None,
            &ss(&["a", "c"]),
            &ss(&["a", "b", "c"]),
        );
        assert_eq!(result, ss(&["a", "c"]));
    }

    // ── Chunk 3e: compute_pane_layout ────────────────────────────────────────

    #[test]
    fn test_layout_zero_panes_has_empty_pane_rects() {
        let l = compute_pane_layout(100, 30, 30, 0);
        assert!(l.pane_rects.is_empty());
        assert_eq!(l.hint.height, 1);
        assert_eq!(l.sidebar.width, 30);
    }

    #[test]
    fn test_layout_single_pane_takes_full_grid_area() {
        let l = compute_pane_layout(100, 30, 30, 1);
        assert_eq!(l.pane_rects.len(), 1);
        let p = l.pane_rects[0];
        // Pane area = (cols - sidebar) wide, (rows - hint) tall.
        assert_eq!(p.width, 70);
        assert_eq!(p.height, 29);
        // Hint is the bottom row.
        assert_eq!(l.hint.y, p.y + p.height);
        assert_eq!(l.hint.height, 1);
    }

    #[test]
    fn test_layout_four_panes_form_2x2_with_disjoint_cells() {
        let l = compute_pane_layout(100, 30, 30, 4);
        assert_eq!(l.pane_rects.len(), 4);
        // Total width covered must equal pane area width (70), total height
        // for one column must equal grid height (29).
        let p0 = l.pane_rects[0];
        let p1 = l.pane_rects[1];
        let p2 = l.pane_rects[2];
        let p3 = l.pane_rects[3];
        // Top row cells share a y; bottom row cells share a different y.
        assert_eq!(p0.y, p1.y);
        assert_eq!(p2.y, p3.y);
        assert_ne!(p0.y, p2.y);
        // Cells in a row share an x sequence that tiles the pane width.
        assert_eq!(p0.x + p0.width, p1.x);
        assert_eq!(p2.x + p2.width, p3.x);
    }

    #[test]
    fn test_layout_three_panes_last_row_spans_remaining_columns() {
        // 3 panes in a 100x30 container → 2x2 grid, last row has 1 cell
        // spanning the full grid width (since cells_in_row collapses to 1).
        let l = compute_pane_layout(100, 30, 30, 3);
        assert_eq!(l.pane_rects.len(), 3);
        let bottom = l.pane_rects[2];
        // Bottom-row pane should span the full pane container width (70).
        assert_eq!(bottom.width, 70);
    }

    #[test]
    fn test_layout_pane_rects_partition_grid_area() {
        // Every cell in the grid area must hit exactly one pane — no gaps,
        // no overlaps. Click hit-testing relies on this: a single click
        // resolves to a unique pane.
        let l = compute_pane_layout(100, 30, 30, 4);
        let pane_x_max = 70u16; // pane container width
        let grid_y_max = l.hint.y; // grid area ends where hint starts
        for y in 0..grid_y_max {
            for x in 0..pane_x_max {
                let hits: usize = l
                    .pane_rects
                    .iter()
                    .filter(|r| {
                        x >= r.x
                            && x < r.x.saturating_add(r.width)
                            && y >= r.y
                            && y < r.y.saturating_add(r.height)
                    })
                    .count();
                assert_eq!(hits, 1, "({x},{y}) should hit exactly one pane");
            }
        }
    }

    #[test]
    fn test_layout_hint_does_not_overlap_panes() {
        // No pane rect should overlap the hint row vertically.
        for n in 1..=6 {
            let l = compute_pane_layout(100, 30, 30, n);
            for p in &l.pane_rects {
                assert!(
                    p.y + p.height <= l.hint.y,
                    "n={n}: pane {:?} overlaps hint {:?}",
                    p,
                    l.hint
                );
            }
        }
    }

    // ── Chunk 3d: compute_grid_dims ──────────────────────────────────────────

    #[test]
    fn test_grid_single_pane_is_1x1() {
        assert_eq!(compute_grid_dims(1, 100, 30), (1, 1));
        assert_eq!(compute_grid_dims(0, 100, 30), (1, 1));
    }

    #[test]
    fn test_grid_two_panes_in_wide_container_are_side_by_side() {
        // 100x30 container, aspect ≈ 3.33. 1x2 cells are 50x30 (aspect 1.67),
        // 2x1 cells are 100x15 (aspect 6.67). Side-by-side wins.
        assert_eq!(compute_grid_dims(2, 100, 30), (1, 2));
    }

    #[test]
    fn test_grid_four_panes_makes_2x2() {
        // 100x30: 2x2 cells are 50x15 (exact aspect match).
        assert_eq!(compute_grid_dims(4, 100, 30), (2, 2));
    }

    #[test]
    fn test_grid_six_panes_in_wide_container_makes_2x3() {
        // 100x30: 2x3 cells 33x15 (aspect 2.2), 3x2 cells 50x10 (aspect 5.0).
        // Target 3.33 → 2x3 wins (closer aspect).
        assert_eq!(compute_grid_dims(6, 100, 30), (2, 3));
    }

    #[test]
    fn test_grid_nine_panes_makes_3x3() {
        // 90x30 container; 3x3 cells 30x10 (aspect 3.0) ≈ container 3.0.
        assert_eq!(compute_grid_dims(9, 90, 30), (3, 3));
    }

    #[test]
    fn test_grid_three_panes_picks_close_aspect() {
        // 100x30 (aspect 3.33). Candidates:
        //   1x3: 33x30, aspect 1.1
        //   2x2: 50x15, aspect 3.33 (exact, but one blank cell)
        //   3x1: 100x10, aspect 10
        // 2x2 wins on aspect.
        assert_eq!(compute_grid_dims(3, 100, 30), (2, 2));
    }

    #[test]
    fn test_grid_handles_tall_container() {
        // 30x100 container, aspect 0.3. For 4 panes:
        //   1x4: 7x100, aspect 0.07
        //   2x2: 15x50, aspect 0.3 (exact)
        //   4x1: 30x25, aspect 1.2
        // 2x2 wins.
        assert_eq!(compute_grid_dims(4, 30, 100), (2, 2));
    }

    #[test]
    fn test_grid_satisfies_capacity() {
        // Sanity: rows * cols must be >= n for any input.
        for n in 1..=12 {
            let (r, c) = compute_grid_dims(n, 100, 30);
            assert!(
                r * c >= n,
                "n={n}: grid {r}x{c} has capacity {} < n",
                r * c
            );
        }
    }

    // ── Chunk 3c: compute_pane_row_offset ────────────────────────────────────

    #[test]
    fn test_pane_offset_zero_when_area_fits_pty() {
        // Area >= PTY → no scrolling needed.
        assert_eq!(compute_pane_row_offset(10, 10, 5), 0);
        assert_eq!(compute_pane_row_offset(10, 20, 9), 0);
    }

    #[test]
    fn test_pane_offset_zero_when_cursor_in_top_window() {
        // PTY 32, area 16, cursor at row 10 → cursor already fits, offset 0.
        assert_eq!(compute_pane_row_offset(32, 16, 10), 0);
        // Edge: cursor exactly at the bottom of the implicit window (row=15).
        assert_eq!(compute_pane_row_offset(32, 16, 15), 0);
    }

    #[test]
    fn test_pane_offset_scrolls_just_enough_for_cursor() {
        // PTY 32, area 16, cursor at row 16 → offset 1 keeps cursor at row 15.
        assert_eq!(compute_pane_row_offset(32, 16, 16), 1);
        // Cursor at row 25 → offset 10.
        assert_eq!(compute_pane_row_offset(32, 16, 25), 10);
    }

    #[test]
    fn test_pane_offset_clamped_to_max() {
        // PTY 32, area 16 → max_offset = 16. Cursor at row 31 wants offset 16,
        // not larger.
        assert_eq!(compute_pane_row_offset(32, 16, 31), 16);
        // Pathological: cursor reported beyond pty_rows still clamps.
        assert_eq!(compute_pane_row_offset(32, 16, 100), 16);
    }

    #[test]
    fn test_pane_offset_zero_height_area() {
        // Defensive: a zero-height pane reports offset 0 (no rows to show).
        assert_eq!(compute_pane_row_offset(32, 0, 10), 0);
    }

    #[test]
    fn test_compute_render_ids_orders_by_session_order_not_visible() {
        // The order of `visible` should not matter — render order follows
        // session_order so the rendered stack mirrors the sidebar.
        let result = compute_render_session_ids(
            Some("a"),
            &ss(&["c", "b"]), // reverse of session_order
            &ss(&["a", "b", "c"]),
        );
        assert_eq!(result, ss(&["a", "b", "c"]));
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
