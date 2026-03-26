use chrono::DateTime;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction as LayoutDir, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell as RCell, Paragraph, Row, Table, TableState},
    Terminal,
};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};
use tttt_log::{SessionInfo, SessionReplay, SqliteLogger};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn run_replay(db_path: &Path, session_id: Option<&str>) -> Result<()> {
    let db = SqliteLogger::open_read_only(db_path)
        .map_err(|e| format!("Failed to open database {}: {}", db_path.display(), e))?;

    let sessions = build_session_list(&db)?;

    let mut app = ReplayApp::new(db, sessions, session_id)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Get session info from the sessions table, falling back to inferred info for orphans.
/// `pid` is used to filter events when inferring info for orphan sessions.
fn resolve_session_info(db: &SqliteLogger, session_id: &str, pid: Option<u32>) -> Result<Option<SessionInfo>> {
    if let Some(info) = db.get_session_info(session_id)? {
        return Ok(Some(info));
    }
    Ok(db.infer_session_info(session_id, pid)?)
}

/// Build the full session list: registered sessions + orphan sessions inferred from events.
/// NULL-pid orphan sessions are split by time gaps > 5 min into separate chunks.
/// Sorted by started_at_ms so orphans interleave correctly with registered sessions.
fn build_session_list(db: &SqliteLogger) -> Result<Vec<SessionListEntry>> {
    let mut sessions = Vec::new();

    for info in db.list_sessions()? {
        let event_count = db.query_events_with_pid(&info.session_id, info.pid).map(|e| e.len()).unwrap_or(0);
        sessions.push(SessionListEntry { info, event_count, load_strategy: LoadStrategy::ByPid });
    }

    for info in db.list_orphan_session_chunks()? {
        if info.pid.is_some() {
            let event_count = db.query_events_with_pid(&info.session_id, info.pid).map(|e| e.len()).unwrap_or(0);
            sessions.push(SessionListEntry { info, event_count, load_strategy: LoadStrategy::ByPid });
        } else {
            let to_ms = info.ended_at_ms.unwrap_or(i64::MAX as u64);
            let event_count = db.query_events_in_range(&info.session_id, info.started_at_ms, to_ms)
                .map(|e| e.len()).unwrap_or(0);
            sessions.push(SessionListEntry { info, event_count, load_strategy: LoadStrategy::ByTimeRange });
        }
    }

    sessions.sort_by_key(|s| s.info.started_at_ms);
    Ok(sessions)
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut ReplayApp,
) -> Result<()> {
    loop {
        terminal.draw(|f| {
            app.render(f);
        })?;

        let timeout = if app.is_playing() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key)?;
                }
            }
        } else if app.is_playing() {
            app.tick();
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// How to load events for a session list entry when the user opens it.
#[derive(Copy, Clone)]
enum LoadStrategy {
    /// Filter by (session_id, pid). pid=None means "all events" (legacy).
    ByPid,
    /// Filter by (session_id, NULL pid, timestamp range). Used for gap-split legacy chunks.
    ByTimeRange,
}

struct SessionListEntry {
    info: SessionInfo,
    event_count: usize,
    load_strategy: LoadStrategy,
}

/// Gap threshold for visual grouping separators: 1 hour.
const VISUAL_GAP_THRESHOLD_MS: u64 = 60 * 60 * 1_000;

/// Returns the session indices before which a separator row should appear.
/// A separator is inserted when the gap between the previous session's end
/// (or start if no end) and the next session's start exceeds the threshold.
fn compute_separator_positions(sessions: &[SessionListEntry]) -> Vec<usize> {
    let mut positions = Vec::new();
    for i in 1..sessions.len() {
        let prev_end = sessions[i - 1]
            .info
            .ended_at_ms
            .unwrap_or(sessions[i - 1].info.started_at_ms);
        let curr_start = sessions[i].info.started_at_ms;
        if curr_start.saturating_sub(prev_end) > VISUAL_GAP_THRESHOLD_MS {
            positions.push(i);
        }
    }
    positions
}

/// Convert session-based separator positions to visual row indices.
/// `sep_before[k]` = the session index before which separator k appears.
/// Returns the visual row indices (accounting for inserted separator rows).
fn separator_visual_indices(sep_before: &[usize]) -> Vec<usize> {
    sep_before
        .iter()
        .enumerate()
        .map(|(count, &session_idx)| session_idx + count)
        .collect()
}

/// Map a visual row index to a session index.
/// Returns `None` if the visual row is a separator.
fn visual_to_session_idx(visual: usize, sep_visual: &[usize]) -> Option<usize> {
    if sep_visual.contains(&visual) {
        return None;
    }
    let sep_before = sep_visual.iter().filter(|&&s| s < visual).count();
    Some(visual - sep_before)
}

struct SessionListState {
    table_state: TableState,
    /// Visual row indices (in the rendered table) that are separator rows.
    separator_indices: Vec<usize>,
}

impl SessionListState {
    fn new(sessions: &[SessionListEntry]) -> Self {
        let sep_before = compute_separator_positions(sessions);
        let sep_visual = separator_visual_indices(&sep_before);
        let mut table_state = TableState::default();
        if !sessions.is_empty() {
            table_state.select(Some(0));
        }
        SessionListState { table_state, separator_indices: sep_visual }
    }

    /// Total number of visual rows (sessions + separators).
    fn total_rows(&self, num_sessions: usize) -> usize {
        num_sessions + self.separator_indices.len()
    }

    /// Return the session index for the currently selected visual row,
    /// or None if nothing is selected or the selection is a separator.
    fn selected_session_idx(&self) -> Option<usize> {
        self.table_state
            .selected()
            .and_then(|v| visual_to_session_idx(v, &self.separator_indices))
    }

    /// Move selection down, skipping separator rows.
    fn move_down(&mut self, num_sessions: usize) {
        let total = self.total_rows(num_sessions);
        let current = self.table_state.selected().unwrap_or(0);
        let mut next = current + 1;
        while next < total && self.separator_indices.contains(&next) {
            next += 1;
        }
        if next < total {
            self.table_state.select(Some(next));
        }
    }

    /// Move selection up, skipping separator rows.
    fn move_up(&mut self) {
        let current = match self.table_state.selected() {
            Some(c) if c > 0 => c,
            _ => return,
        };
        let mut prev = current - 1;
        while prev > 0 && self.separator_indices.contains(&prev) {
            prev -= 1;
        }
        if !self.separator_indices.contains(&prev) {
            self.table_state.select(Some(prev));
        }
    }
}

struct ReplayViewState {
    replay: SessionReplay,
    session_info: SessionInfo,
    playing: bool,
    speed: f64,
    last_tick: Instant,
    base_timestamp: u64,
    playback_pos_ms: u64,
}

enum View {
    SessionList(SessionListState),
    Replay(ReplayViewState),
}

struct ReplayApp {
    db: SqliteLogger,
    sessions: Vec<SessionListEntry>,
    view: View,
    should_quit: bool,
}

impl ReplayApp {
    fn new(
        db: SqliteLogger,
        sessions: Vec<SessionListEntry>,
        initial_session: Option<&str>,
    ) -> Result<Self> {
        let view = if let Some(id) = initial_session {
            let info = resolve_session_info(&db, id, None)?
                .ok_or_else(|| format!("Session not found: {}", id))?;
            let events = db.query_events_with_pid(id, info.pid)?;
            let base_timestamp = events.first().map(|e| e.timestamp_ms).unwrap_or(0);
            let replay = SessionReplay::new(events, info.cols, info.rows);
            View::Replay(ReplayViewState {
                replay,
                session_info: info,
                playing: false,
                speed: 1.0,
                last_tick: Instant::now(),
                base_timestamp,
                playback_pos_ms: 0,
            })
        } else {
            View::SessionList(SessionListState::new(&sessions))
        };

        Ok(Self {
            db,
            sessions,
            view,
            should_quit: false,
        })
    }

    fn is_playing(&self) -> bool {
        if let View::Replay(state) = &self.view {
            state.playing
        } else {
            false
        }
    }

    fn tick(&mut self) {
        if let View::Replay(state) = &mut self.view {
            if state.playing {
                let elapsed = state.last_tick.elapsed();
                state.last_tick = Instant::now();
                let advance_ms = (elapsed.as_secs_f64() * 1000.0 * state.speed) as u64;
                state.playback_pos_ms = state.playback_pos_ms.saturating_add(advance_ms);
                let target_ts = state.base_timestamp.saturating_add(state.playback_pos_ms);
                state.replay.seek_to_timestamp(target_ts);
                if state.replay.current_index() >= state.replay.event_count() {
                    state.playing = false;
                }
            }
        }
    }

    /// Open a session from the sessions list by index, using its stored load strategy.
    fn open_entry(&mut self, idx: usize) -> Result<()> {
        let (info, load_strategy) = {
            let entry = &self.sessions[idx];
            (entry.info.clone(), entry.load_strategy)
        };
        let events = match load_strategy {
            LoadStrategy::ByPid => self.db.query_events_with_pid(&info.session_id, info.pid)?,
            LoadStrategy::ByTimeRange => {
                let to_ms = info.ended_at_ms.unwrap_or(i64::MAX as u64);
                self.db.query_events_in_range(&info.session_id, info.started_at_ms, to_ms)?
            }
        };
        let base_timestamp = events.first().map(|e| e.timestamp_ms).unwrap_or(0);
        let replay = SessionReplay::new(events, info.cols, info.rows);
        self.view = View::Replay(ReplayViewState {
            replay,
            session_info: info,
            playing: false,
            speed: 1.0,
            last_tick: Instant::now(),
            base_timestamp,
            playback_pos_ms: 0,
        });
        Ok(())
    }

    fn open_session(&mut self, session_id: &str, pid: Option<u32>) -> Result<()> {
        let info = resolve_session_info(&self.db, session_id, pid)?
            .ok_or_else(|| format!("Session not found: {}", session_id))?;
        let events = self.db.query_events_with_pid(session_id, info.pid)?;
        let base_timestamp = events.first().map(|e| e.timestamp_ms).unwrap_or(0);
        let replay = SessionReplay::new(events, info.cols, info.rows);
        self.view = View::Replay(ReplayViewState {
            replay,
            session_info: info,
            playing: false,
            speed: 1.0,
            last_tick: Instant::now(),
            base_timestamp,
            playback_pos_ms: 0,
        });
        Ok(())
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<()> {
        let num_sessions = self.sessions.len();
        match &mut self.view {
            View::SessionList(state) => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    state.move_down(num_sessions);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    state.move_up();
                }
                KeyCode::Enter => {
                    let maybe_idx = state.selected_session_idx();
                    if let Some(session_idx) = maybe_idx {
                        if session_idx < num_sessions {
                            self.open_entry(session_idx)?;
                        }
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.should_quit = true;
                }
                _ => {}
            },
            View::Replay(state) => match key.code {
                KeyCode::Char(' ') => {
                    state.playing = !state.playing;
                    if state.playing {
                        state.last_tick = Instant::now();
                    }
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    state.playing = false;
                    let next = state.replay.current_index() + 1;
                    state.replay.seek_to_index(next);
                    state.playback_pos_ms = state
                        .replay
                        .current_timestamp()
                        .saturating_sub(state.base_timestamp);
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    state.playing = false;
                    let cur = state.replay.current_index();
                    if cur > 0 {
                        state.replay.seek_to_index(cur - 1);
                    } else {
                        state.replay.seek_to_index(0);
                    }
                    state.playback_pos_ms = state
                        .replay
                        .current_timestamp()
                        .saturating_sub(state.base_timestamp);
                }
                KeyCode::Char(']') => {
                    state.playing = false;
                    let next = state.replay.current_index().saturating_add(10);
                    state.replay.seek_to_index(next);
                    state.playback_pos_ms = state
                        .replay
                        .current_timestamp()
                        .saturating_sub(state.base_timestamp);
                }
                KeyCode::Char('[') => {
                    state.playing = false;
                    let cur = state.replay.current_index();
                    let prev = cur.saturating_sub(10);
                    state.replay.seek_to_index(prev);
                    state.playback_pos_ms = state
                        .replay
                        .current_timestamp()
                        .saturating_sub(state.base_timestamp);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    state.playing = false;
                    state.replay.seek_to_index(0);
                    state.playback_pos_ms = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    state.playing = false;
                    let end = state.replay.event_count();
                    state.replay.seek_to_index(end);
                    state.playback_pos_ms = state
                        .replay
                        .current_timestamp()
                        .saturating_sub(state.base_timestamp);
                }
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    state.speed = (state.speed * 2.0).min(16.0);
                }
                KeyCode::Char('-') => {
                    state.speed = (state.speed / 2.0).max(0.125);
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    let new_state = SessionListState::new(&self.sessions);
                    self.view = View::SessionList(new_state);
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        match &mut self.view {
            View::SessionList(state) => {
                render_session_list(frame, &self.sessions, state);
            }
            View::Replay(state) => {
                render_replay(frame, state);
            }
        }
    }
}

fn render_session_list(
    frame: &mut ratatui::Frame,
    sessions: &[SessionListEntry],
    state: &mut SessionListState,
) {
    let area = frame.area();

    let sep_before = compute_separator_positions(sessions);
    let sep_style = Style::default().fg(Color::DarkGray);
    let mut sep_iter = sep_before.iter().peekable();

    let mut rows: Vec<Row> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        // Insert separator row before this session if a gap precedes it
        if sep_iter.peek() == Some(&&i) {
            sep_iter.next();
            let prev_end = sessions[i - 1]
                .info
                .ended_at_ms
                .unwrap_or(sessions[i - 1].info.started_at_ms);
            let gap_ms = s.info.started_at_ms.saturating_sub(prev_end);
            let gap_label = format!("── {} gap ──", format_duration_ms(Some(gap_ms)));
            rows.push(
                Row::new(vec![
                    RCell::from(gap_label),
                    RCell::from(""),
                    RCell::from(""),
                    RCell::from(""),
                    RCell::from(""),
                    RCell::from(""),
                    RCell::from(""),
                    RCell::from(""),
                ])
                .style(sep_style),
            );
        }

        let started = format_timestamp_ms(s.info.started_at_ms);
        let duration = format_duration_ms(
            s.info
                .ended_at_ms
                .map(|e| e.saturating_sub(s.info.started_at_ms)),
        );
        let size = format!("{}x{}", s.info.cols, s.info.rows);
        let name = s.info.name.as_deref().unwrap_or("-").to_string();
        let short_id = s.info.session_id.get(..8).unwrap_or(&s.info.session_id).to_string();
        let pid = s.info.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
        rows.push(Row::new(vec![
            RCell::from(short_id),
            RCell::from(pid),
            RCell::from(name),
            RCell::from(s.info.command.clone()),
            RCell::from(size),
            RCell::from(started),
            RCell::from(duration),
            RCell::from(s.event_count.to_string()),
        ]));
    }

    let widths = [
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Min(15),
        Constraint::Length(8),
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Length(8),
    ];

    let header = Row::new(vec![
        RCell::from("ID"),
        RCell::from("PID"),
        RCell::from("Name"),
        RCell::from("Command"),
        RCell::from("Size"),
        RCell::from("Started"),
        RCell::from("Duration"),
        RCell::from("Events"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(Block::default().title("Sessions (Enter: open | q: quit)"));

    frame.render_stateful_widget(table, area, &mut state.table_state);
}

fn render_replay(frame: &mut ratatui::Frame, state: &mut ReplayViewState) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(LayoutDir::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let screen = state.replay.screen();
    let lines = screen_to_lines(screen);
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, chunks[0]);

    let play_icon = if state.playing { ">" } else { "|" };
    let idx = state.replay.current_index();
    let total = state.replay.event_count();
    let ts_ms = state
        .replay
        .current_timestamp()
        .saturating_sub(state.base_timestamp);
    let ts_secs = ts_ms / 1000;
    let timestamp = format!(
        "{:02}:{:02}:{:02}",
        ts_secs / 3600,
        (ts_secs % 3600) / 60,
        ts_secs % 60
    );
    let speed = format!("{:.3}x", state.speed);
    let cmd = &state.session_info.command;
    let status = format!(
        "[{}] {}/{} | {} | {} | {} | Space:play h/l:step g/G:ends +-:speed q:back",
        play_icon, idx, total, timestamp, speed, cmd
    );

    let status_bar =
        Paragraph::new(status).style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(status_bar, chunks[1]);
}

fn screen_to_lines(screen: &vt100::Screen) -> Vec<Line<'static>> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);

    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_text = String::new();
        let mut current_style = Style::default();

        for col in 0..cols {
            let cell = match screen.cell(row, col) {
                Some(c) => c,
                None => {
                    current_text.push(' ');
                    continue;
                }
            };

            if cell.is_wide_continuation() {
                continue;
            }

            let cell_style = vt100_style_to_ratatui(cell);
            let contents = cell.contents();
            let text = if contents.is_empty() {
                " ".to_string()
            } else {
                contents
            };

            if cell_style == current_style {
                current_text.push_str(&text);
            } else {
                if !current_text.is_empty() {
                    spans.push(Span::styled(current_text.clone(), current_style));
                }
                current_text = text;
                current_style = cell_style;
            }
        }

        if !current_text.is_empty() {
            spans.push(Span::styled(current_text, current_style));
        }

        lines.push(Line::from(spans));
    }

    lines
}

fn vt100_style_to_ratatui(cell: &vt100::Cell) -> Style {
    let fg = vt100_color_to_ratatui(cell.fgcolor());
    let bg = vt100_color_to_ratatui(cell.bgcolor());

    let mut style = Style::default();
    if let Some(c) = fg {
        style = style.fg(c);
    }
    if let Some(c) = bg {
        style = style.bg(c);
    }
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }

    style
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn format_timestamp_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_duration_ms(ms: Option<u64>) -> String {
    match ms {
        None => "running".to_string(),
        Some(ms) => {
            let secs = ms / 1000;
            let mins = secs / 60;
            let secs = secs % 60;
            format!("{}m {}s", mins, secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tttt_log::{Direction, LogEvent, SessionInfo, SqliteLogger};

    fn make_parser(content: &[u8], cols: u16, rows: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(content);
        parser
    }

    fn make_db_with_session() -> SqliteLogger {
        use tttt_log::LogSink;
        let mut db = SqliteLogger::in_memory().unwrap();
        db.log_session_start("sess1", "bash", 80, 24, None).unwrap();
        db.log_event(&LogEvent::with_timestamp(
            1000,
            "sess1".to_string(),
            Direction::Output,
            b"hello".to_vec(),
        ))
        .unwrap();
        db.log_event(&LogEvent::with_timestamp(
            2000,
            "sess1".to_string(),
            Direction::Output,
            b" world".to_vec(),
        ))
        .unwrap();
        db
    }

    // --- vt100-to-ratatui conversion tests ---

    #[test]
    fn test_empty_screen_to_lines() {
        let parser = make_parser(b"", 80, 24);
        let lines = screen_to_lines(parser.screen());
        assert_eq!(lines.len(), 24, "blank screen should produce 24 lines");
    }

    #[test]
    fn test_plain_text_conversion() {
        let parser = make_parser(b"hello", 80, 24);
        let lines = screen_to_lines(parser.screen());
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with("hello"),
            "first line should start with 'hello', got: {:?}",
            &text[..text.len().min(20)]
        );
    }

    #[test]
    fn test_color_conversion_default() {
        assert_eq!(vt100_color_to_ratatui(vt100::Color::Default), None);
    }

    #[test]
    fn test_color_conversion_indexed() {
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Idx(3)),
            Some(Color::Indexed(3))
        );
    }

    #[test]
    fn test_color_conversion_rgb() {
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Rgb(255, 128, 0)),
            Some(Color::Rgb(255, 128, 0))
        );
    }

    #[test]
    fn test_bold_conversion() {
        // ESC[1m = bold on, then "X", ESC[0m = reset
        let parser = make_parser(b"\x1b[1mX\x1b[0m", 80, 24);
        let lines = screen_to_lines(parser.screen());
        let first = &lines[0];
        let bold_span = first.spans.iter().find(|s| s.content.contains('X'));
        assert!(bold_span.is_some(), "should have a span containing 'X'");
        let span = bold_span.unwrap();
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "'X' should be bold"
        );
    }

    #[test]
    fn test_italic_conversion() {
        // ESC[3m = italic on
        let parser = make_parser(b"\x1b[3mI\x1b[0m", 80, 24);
        let lines = screen_to_lines(parser.screen());
        let first = &lines[0];
        let span = first.spans.iter().find(|s| s.content.contains('I'));
        assert!(span.is_some());
        assert!(span.unwrap().style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn test_underline_conversion() {
        // ESC[4m = underline on
        let parser = make_parser(b"\x1b[4mU\x1b[0m", 80, 24);
        let lines = screen_to_lines(parser.screen());
        let first = &lines[0];
        let span = first.spans.iter().find(|s| s.content.contains('U'));
        assert!(span.is_some());
        assert!(span
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_wide_char_handling() {
        // "あ" is a wide char (2 columns wide)
        let parser = make_parser("あ".as_bytes(), 80, 24);
        let lines = screen_to_lines(parser.screen());
        let first = &lines[0];
        let text: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
        let count = text.matches('あ').count();
        assert_eq!(count, 1, "wide char should appear exactly once, not duplicated");
    }

    #[test]
    fn test_span_coalescing() {
        let parser = make_parser(b"hello world", 80, 24);
        let lines = screen_to_lines(parser.screen());
        let first = &lines[0];
        let text: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("hello world"));
        // 11 chars + trailing spaces = 80 cells; all same style → should be 1 span
        assert!(
            first.spans.len() < 11,
            "adjacent same-style cells should be coalesced, got {} spans",
            first.spans.len()
        );
    }

    // --- Utility function tests ---

    #[test]
    fn test_format_timestamp_ms_zero() {
        let s = format_timestamp_ms(0);
        assert!(s.contains("1970"), "epoch 0 should be 1970, got: {}", s);
    }

    #[test]
    fn test_format_timestamp_ms_nonzero() {
        // 2024-01-01 00:00:00 UTC = 1704067200 seconds
        let s = format_timestamp_ms(1_704_067_200_000);
        assert!(s.contains("2024"), "should be 2024, got: {}", s);
    }

    #[test]
    fn test_format_duration_ms_none() {
        assert_eq!(format_duration_ms(None), "running");
    }

    #[test]
    fn test_format_duration_ms_zero() {
        assert_eq!(format_duration_ms(Some(0)), "0m 0s");
    }

    #[test]
    fn test_format_duration_ms_ninety_seconds() {
        // 90000ms = 90s = 1m 30s
        assert_eq!(format_duration_ms(Some(90_000)), "1m 30s");
    }

    // --- ReplayApp logic tests ---

    #[test]
    fn test_session_list_entry_creation() {
        let info = SessionInfo {
            session_id: "test-id".to_string(),
            command: "bash".to_string(),
            cols: 80,
            rows: 24,
            started_at_ms: 1000,
            ended_at_ms: Some(2000),
            name: None,
            pid: None,
        };
        let entry = SessionListEntry {
            info,
            event_count: 42,
            load_strategy: LoadStrategy::ByPid,
        };
        assert_eq!(entry.event_count, 42);
        assert_eq!(entry.info.session_id, "test-id");
    }

    #[test]
    fn test_replay_app_new_empty_sessions() {
        let db = SqliteLogger::in_memory().unwrap();
        let app = ReplayApp::new(db, vec![], None).unwrap();
        assert!(!app.should_quit);
        assert!(matches!(app.view, View::SessionList(_)));
    }

    #[test]
    fn test_replay_app_new_starts_at_session_list_when_no_initial() {
        let db = make_db_with_session();
        let sessions = vec![SessionListEntry {
            info: db.list_sessions().unwrap().into_iter().next().unwrap(),
            event_count: 2,
            load_strategy: LoadStrategy::ByPid,
        }];
        let app = ReplayApp::new(db, sessions, None).unwrap();
        assert!(matches!(app.view, View::SessionList(_)));
    }

    #[test]
    fn test_replay_app_new_with_initial_session() {
        let db = make_db_with_session();
        let info = db.get_session_info("sess1").unwrap().unwrap();
        let event_count = db.query_events("sess1").unwrap().len();
        let sessions = vec![SessionListEntry { info, event_count, load_strategy: LoadStrategy::ByPid }];
        let app = ReplayApp::new(db, sessions, Some("sess1")).unwrap();
        assert!(matches!(app.view, View::Replay(_)));
    }

    #[test]
    fn test_replay_app_initial_not_playing() {
        let db = make_db_with_session();
        let app = ReplayApp::new(db, vec![], None).unwrap();
        assert!(!app.is_playing());
    }

    #[test]
    fn test_load_session_creates_replay_view() {
        let db = make_db_with_session();
        let info = db.get_session_info("sess1").unwrap().unwrap();
        let sessions = vec![SessionListEntry {
            info,
            event_count: 2,
            load_strategy: LoadStrategy::ByPid,
        }];
        let app = ReplayApp::new(db, sessions, Some("sess1")).unwrap();
        if let View::Replay(state) = &app.view {
            assert_eq!(state.replay.event_count(), 2);
            assert_eq!(state.session_info.command, "bash");
            assert!(!state.playing);
            assert_eq!(state.speed, 1.0);
        } else {
            panic!("expected Replay view");
        }
    }

    #[test]
    fn test_playback_advance_step_forward() {
        let events = vec![
            LogEvent::with_timestamp(
                1000,
                "s1".to_string(),
                Direction::Output,
                b"hello".to_vec(),
            ),
            LogEvent::with_timestamp(
                2000,
                "s1".to_string(),
                Direction::Output,
                b" world".to_vec(),
            ),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        assert_eq!(replay.current_index(), 0);
        replay.step_forward();
        assert_eq!(replay.current_index(), 1);
        assert!(replay.screen_contents().contains("hello"));
        replay.step_forward();
        assert_eq!(replay.current_index(), 2);
        assert!(replay.screen_contents().contains("hello world"));
    }

    #[test]
    fn test_speed_adjustment_max() {
        let mut speed = 1.0_f64;
        for _ in 0..10 {
            speed = (speed * 2.0).min(16.0);
        }
        assert_eq!(speed, 16.0);
    }

    #[test]
    fn test_speed_adjustment_min() {
        let mut speed = 1.0_f64;
        for _ in 0..10 {
            speed = (speed / 2.0).max(0.125);
        }
        assert_eq!(speed, 0.125);
    }

    #[test]
    fn test_speed_doubles_from_one() {
        let speed = (1.0_f64 * 2.0).min(16.0);
        assert_eq!(speed, 2.0);
    }

    #[test]
    fn test_speed_halves_from_one() {
        let speed = (1.0_f64 / 2.0).max(0.125);
        assert_eq!(speed, 0.5);
    }

    #[test]
    fn test_screen_size_matches_session_dimensions() {
        let events = vec![LogEvent::with_timestamp(
            1000,
            "s1".to_string(),
            Direction::Output,
            b"test".to_vec(),
        )];
        let replay = SessionReplay::new(events, 80, 24);
        let screen = replay.screen();
        assert_eq!(screen.size(), (24, 80));
    }

    #[test]
    fn test_replay_app_missing_session_errors() {
        let db = SqliteLogger::in_memory().unwrap();
        let result = ReplayApp::new(db, vec![], Some("nonexistent"));
        assert!(result.is_err());
    }

    // --- Orphan session tests ---

    #[test]
    fn test_build_session_list_no_orphans() {
        let mut db = SqliteLogger::in_memory().unwrap();
        db.log_session_start("s1", "bash", 80, 24, None).unwrap();
        {
            use tttt_log::LogSink;
            db.log_event(&LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Output, b"hi".to_vec())).unwrap();
        }
        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].info.command, "bash");
    }

    #[test]
    fn test_build_session_list_with_orphan() {
        let mut db = SqliteLogger::in_memory().unwrap();
        // Orphan: events only, no sessions entry
        {
            use tttt_log::LogSink;
            db.log_event(&LogEvent::with_timestamp(500, "orphan".to_string(), Direction::Output, b"old data".to_vec())).unwrap();
        }
        // Registered session
        db.log_session_start("registered", "bash", 80, 24, None).unwrap();

        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 2);
        let ids: Vec<&str> = sessions.iter().map(|s| s.info.session_id.as_str()).collect();
        assert!(ids.contains(&"orphan"));
        assert!(ids.contains(&"registered"));
    }

    #[test]
    fn test_build_session_list_orphan_inferred_info() {
        let mut db = SqliteLogger::in_memory().unwrap();
        {
            use tttt_log::LogSink;
            db.log_event(&LogEvent::with_timestamp(1000, "orphan".to_string(), Direction::Output, b"x".to_vec())).unwrap();
            db.log_event(&LogEvent::with_timestamp(5000, "orphan".to_string(), Direction::Output, b"y".to_vec())).unwrap();
        }
        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.info.session_id, "orphan");
        assert_eq!(s.info.command, "unknown");
        assert_eq!(s.info.cols, 80);
        assert_eq!(s.info.rows, 24);
        assert_eq!(s.info.started_at_ms, 1000);
        assert_eq!(s.info.ended_at_ms, Some(5000));
        assert_eq!(s.event_count, 2);
    }

    #[test]
    fn test_build_session_list_sorted_by_start_time() {
        let mut db = SqliteLogger::in_memory().unwrap();
        {
            use tttt_log::LogSink;
            // Orphan with early timestamp
            db.log_event(&LogEvent::with_timestamp(100, "early-orphan".to_string(), Direction::Output, b"a".to_vec())).unwrap();
            // Orphan with late timestamp
            db.log_event(&LogEvent::with_timestamp(9000, "late-orphan".to_string(), Direction::Output, b"b".to_vec())).unwrap();
        }
        // Registered session with middle timestamp (but log_session_start uses wall clock,
        // so we check relative ordering of orphans)
        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 2);
        // early-orphan (ts=100) should come before late-orphan (ts=9000)
        assert_eq!(sessions[0].info.session_id, "early-orphan");
        assert_eq!(sessions[1].info.session_id, "late-orphan");
    }

    #[test]
    fn test_resolve_session_info_registered() {
        let mut db = SqliteLogger::in_memory().unwrap();
        db.log_session_start("s1", "zsh", 120, 40, None).unwrap();
        let info = resolve_session_info(&db, "s1", None).unwrap().unwrap();
        assert_eq!(info.command, "zsh");
        assert_eq!(info.cols, 120);
    }

    #[test]
    fn test_resolve_session_info_orphan_fallback() {
        let mut db = SqliteLogger::in_memory().unwrap();
        {
            use tttt_log::LogSink;
            db.log_event(&LogEvent::with_timestamp(2000, "orphan".to_string(), Direction::Output, b"data".to_vec())).unwrap();
        }
        let info = resolve_session_info(&db, "orphan", None).unwrap().unwrap();
        assert_eq!(info.command, "unknown");
        assert_eq!(info.cols, 80);
        assert_eq!(info.started_at_ms, 2000);
    }

    #[test]
    fn test_resolve_session_info_missing() {
        let db = SqliteLogger::in_memory().unwrap();
        let info = resolve_session_info(&db, "nope", None).unwrap();
        assert!(info.is_none());
    }

    #[test]
    fn test_replay_app_can_open_orphan_session() {
        let mut db = SqliteLogger::in_memory().unwrap();
        {
            use tttt_log::LogSink;
            db.log_event(&LogEvent::with_timestamp(1000, "orphan".to_string(), Direction::Output, b"hello".to_vec())).unwrap();
            db.log_event(&LogEvent::with_timestamp(2000, "orphan".to_string(), Direction::Output, b" world".to_vec())).unwrap();
        }
        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 1);
        let app = ReplayApp::new(db, sessions, Some("orphan")).unwrap();
        if let View::Replay(state) = &app.view {
            assert_eq!(state.replay.event_count(), 2);
            assert_eq!(state.session_info.command, "unknown");
            assert_eq!(state.session_info.cols, 80);
            assert_eq!(state.session_info.rows, 24);
        } else {
            panic!("expected Replay view");
        }
    }

    // Helper: insert a NULL-pid event into an existing in-memory logger
    fn insert_null_pid_event_tui(db: &SqliteLogger, session_id: &str, ts: u64) {
        db.insert_raw_event_null_pid(session_id, ts).unwrap();
    }

    #[test]
    fn test_build_session_list_null_pid_orphan_not_split_when_continuous() {
        let db = SqliteLogger::in_memory().unwrap();
        // Three events well within 5 min of each other
        insert_null_pid_event_tui(&db, "pty-1", 1_000);
        insert_null_pid_event_tui(&db, "pty-1", 60_000);
        insert_null_pid_event_tui(&db, "pty-1", 120_000);

        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].info.session_id, "pty-1");
        assert_eq!(sessions[0].event_count, 3);
        assert!(matches!(sessions[0].load_strategy, LoadStrategy::ByTimeRange));
    }

    #[test]
    fn test_build_session_list_null_pid_orphan_split_on_gap() {
        let db = SqliteLogger::in_memory().unwrap();
        let gap = tttt_log::SqliteLogger::gap_threshold_ms() + 1;
        insert_null_pid_event_tui(&db, "pty-1", 1_000);
        insert_null_pid_event_tui(&db, "pty-1", 1_000 + gap);

        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].info.session_id, "pty-1");
        assert_eq!(sessions[1].info.session_id, "pty-1");
        // Each chunk has 1 event
        assert_eq!(sessions[0].event_count, 1);
        assert_eq!(sessions[1].event_count, 1);
        // Both use time-range loading
        assert!(matches!(sessions[0].load_strategy, LoadStrategy::ByTimeRange));
        assert!(matches!(sessions[1].load_strategy, LoadStrategy::ByTimeRange));
    }

    #[test]
    fn test_open_entry_time_range_loads_correct_chunk() {
        let db = SqliteLogger::in_memory().unwrap();
        let gap = tttt_log::SqliteLogger::gap_threshold_ms() + 1;
        // Chunk 1: ts 1_000 and 2_000
        insert_null_pid_event_tui(&db, "pty-1", 1_000);
        insert_null_pid_event_tui(&db, "pty-1", 2_000);
        // Chunk 2: ts 2_000+gap and 2_000+gap+5_000 (gap measured from last event of chunk 1)
        insert_null_pid_event_tui(&db, "pty-1", 2_000 + gap);
        insert_null_pid_event_tui(&db, "pty-1", 2_000 + gap + 5_000);

        let sessions = build_session_list(&db).unwrap();
        assert_eq!(sessions.len(), 2);

        // Open the first chunk (entries are sorted by started_at_ms)
        let mut app = ReplayApp::new(db, sessions, None).unwrap();
        app.open_entry(0).unwrap();
        if let View::Replay(state) = &app.view {
            // First chunk has 2 events (ts 1000 and 2000)
            assert_eq!(state.replay.event_count(), 2);
            assert_eq!(state.session_info.started_at_ms, 1_000);
        } else {
            panic!("expected Replay view");
        }

        // Open the second chunk
        app.open_entry(1).unwrap();
        if let View::Replay(state) = &app.view {
            assert_eq!(state.replay.event_count(), 2);
            assert_eq!(state.session_info.started_at_ms, 2_000 + gap);
        } else {
            panic!("expected Replay view");
        }
    }

    #[test]
    fn test_build_session_list_pid_and_null_pid_separate() {
        let mut db = SqliteLogger::in_memory().unwrap();
        // Pid-aware event (current run)
        {
            use tttt_log::LogSink;
            db.log_event(&LogEvent::with_timestamp(500, "pty-1".to_string(), Direction::Output, b"x".to_vec())).unwrap();
        }
        // NULL-pid legacy event, far away in time
        let gap = tttt_log::SqliteLogger::gap_threshold_ms() + 1;
        insert_null_pid_event_tui(&db, "pty-1", 500 + gap);

        let sessions = build_session_list(&db).unwrap();
        // Expect 2 entries: one ByPid, one ByTimeRange
        assert_eq!(sessions.len(), 2);
        let by_pid: Vec<_> = sessions.iter().filter(|s| matches!(s.load_strategy, LoadStrategy::ByPid)).collect();
        let by_range: Vec<_> = sessions.iter().filter(|s| matches!(s.load_strategy, LoadStrategy::ByTimeRange)).collect();
        assert_eq!(by_pid.len(), 1);
        assert_eq!(by_range.len(), 1);
    }

    // ── Visual grouping / separator tests ────────────────────────────────────

    /// Build a minimal SessionListEntry with only timestamps set.
    fn make_entry(started: u64, ended: Option<u64>) -> SessionListEntry {
        SessionListEntry {
            info: SessionInfo {
                session_id: "s".to_string(),
                command: "sh".to_string(),
                cols: 80,
                rows: 24,
                started_at_ms: started,
                ended_at_ms: ended,
                name: None,
                pid: None,
            },
            event_count: 0,
            load_strategy: LoadStrategy::ByPid,
        }
    }

    #[test]
    fn test_compute_separator_positions_empty() {
        let positions = compute_separator_positions(&[]);
        assert!(positions.is_empty());
    }

    #[test]
    fn test_compute_separator_positions_single() {
        let sessions = vec![make_entry(0, None)];
        let positions = compute_separator_positions(&sessions);
        assert!(positions.is_empty());
    }

    #[test]
    fn test_compute_separator_positions_no_gap() {
        // Two sessions 1 min apart — below threshold, no separator
        let sessions = vec![
            make_entry(0, Some(60_000)),
            make_entry(60_000, Some(120_000)),
        ];
        let positions = compute_separator_positions(&sessions);
        assert!(positions.is_empty());
    }

    #[test]
    fn test_compute_separator_positions_exact_threshold_not_split() {
        // Gap exactly == threshold: NOT > threshold, so no separator
        let sessions = vec![
            make_entry(0, Some(0)),
            make_entry(VISUAL_GAP_THRESHOLD_MS, None),
        ];
        let positions = compute_separator_positions(&sessions);
        assert!(positions.is_empty());
    }

    #[test]
    fn test_compute_separator_positions_gap_detected() {
        // Gap of threshold+1 → separator before index 1
        let sessions = vec![
            make_entry(0, Some(1_000)),
            make_entry(1_000 + VISUAL_GAP_THRESHOLD_MS + 1, None),
        ];
        let positions = compute_separator_positions(&sessions);
        assert_eq!(positions, vec![1]);
    }

    #[test]
    fn test_compute_separator_positions_gap_uses_started_when_no_end() {
        // Previous session has no ended_at_ms; gap is measured from started_at_ms
        let sessions = vec![
            make_entry(0, None),  // no ended_at_ms → use started_at_ms = 0
            make_entry(VISUAL_GAP_THRESHOLD_MS + 1, None),
        ];
        let positions = compute_separator_positions(&sessions);
        assert_eq!(positions, vec![1]);
    }

    #[test]
    fn test_compute_separator_positions_multiple_gaps() {
        let g = VISUAL_GAP_THRESHOLD_MS + 1;
        let sessions = vec![
            make_entry(0, Some(100)),
            make_entry(100 + g, Some(200 + g)),
            make_entry(200 + g * 2, None),
        ];
        let positions = compute_separator_positions(&sessions);
        assert_eq!(positions, vec![1, 2]);
    }

    #[test]
    fn test_separator_visual_indices_empty() {
        assert!(separator_visual_indices(&[]).is_empty());
    }

    #[test]
    fn test_separator_visual_indices_one() {
        // Separator before session 1: visual row = 1 + 0 = 1
        let vis = separator_visual_indices(&[1]);
        assert_eq!(vis, vec![1]);
    }

    #[test]
    fn test_separator_visual_indices_two() {
        // Seps before sessions 1 and 3:
        //   sep 0: visual = 1 + 0 = 1
        //   sep 1: visual = 3 + 1 = 4
        let vis = separator_visual_indices(&[1, 3]);
        assert_eq!(vis, vec![1, 4]);
    }

    #[test]
    fn test_visual_to_session_idx_no_seps() {
        assert_eq!(visual_to_session_idx(0, &[]), Some(0));
        assert_eq!(visual_to_session_idx(3, &[]), Some(3));
    }

    #[test]
    fn test_visual_to_session_idx_separator_returns_none() {
        // separator at visual 1
        let sep = vec![1];
        assert_eq!(visual_to_session_idx(1, &sep), None);
    }

    #[test]
    fn test_visual_to_session_idx_after_separator() {
        // visual layout: 0→s0, 1→sep, 2→s1, 3→s2, 4→sep, 5→s3
        let sep = vec![1, 4];
        assert_eq!(visual_to_session_idx(0, &sep), Some(0));
        assert_eq!(visual_to_session_idx(1, &sep), None);
        assert_eq!(visual_to_session_idx(2, &sep), Some(1));
        assert_eq!(visual_to_session_idx(3, &sep), Some(2));
        assert_eq!(visual_to_session_idx(4, &sep), None);
        assert_eq!(visual_to_session_idx(5, &sep), Some(3));
    }

    #[test]
    fn test_session_list_state_new_no_separators() {
        // Sessions close together → no separators, selection starts at 0
        let sessions = vec![
            make_entry(0, Some(1_000)),
            make_entry(1_001, Some(2_000)),
        ];
        let state = SessionListState::new(&sessions);
        assert!(state.separator_indices.is_empty());
        assert_eq!(state.table_state.selected(), Some(0));
    }

    #[test]
    fn test_session_list_state_new_with_separator() {
        let g = VISUAL_GAP_THRESHOLD_MS + 1;
        let sessions = vec![
            make_entry(0, Some(1_000)),
            make_entry(1_000 + g, None),
        ];
        let state = SessionListState::new(&sessions);
        // Separator before session 1 → visual index 1
        assert_eq!(state.separator_indices, vec![1]);
    }

    #[test]
    fn test_selected_session_idx_on_session_row() {
        let sessions = vec![make_entry(0, None), make_entry(1, None)];
        let mut state = SessionListState::new(&sessions);
        state.table_state.select(Some(0));
        assert_eq!(state.selected_session_idx(), Some(0));
    }

    #[test]
    fn test_selected_session_idx_on_separator_row() {
        let g = VISUAL_GAP_THRESHOLD_MS + 1;
        let sessions = vec![make_entry(0, Some(0)), make_entry(g + 1, None)];
        let mut state = SessionListState::new(&sessions);
        // Force-select the separator row (visual 1)
        state.table_state.select(Some(1));
        assert_eq!(state.selected_session_idx(), None);
    }

    #[test]
    fn test_move_down_skips_separator() {
        let g = VISUAL_GAP_THRESHOLD_MS + 1;
        // 3 sessions: sep before index 1
        let sessions = vec![
            make_entry(0, Some(100)),
            make_entry(100 + g, Some(200 + g)),
            make_entry(200 + g, None),
        ];
        let mut state = SessionListState::new(&sessions);
        // separator_indices should be [1] (visual row 1)
        assert_eq!(state.separator_indices, vec![1]);
        // Start at visual 0 (session 0)
        assert_eq!(state.table_state.selected(), Some(0));
        // Move down: skip separator at visual 1, land on visual 2 (session 1)
        state.move_down(3);
        assert_eq!(state.table_state.selected(), Some(2));
        // Move down again: land on visual 3 (session 2)
        state.move_down(3);
        assert_eq!(state.table_state.selected(), Some(3));
        // Move down past end: stay at 3
        state.move_down(3);
        assert_eq!(state.table_state.selected(), Some(3));
    }

    #[test]
    fn test_move_up_skips_separator() {
        let g = VISUAL_GAP_THRESHOLD_MS + 1;
        let sessions = vec![
            make_entry(0, Some(100)),
            make_entry(100 + g, None),
        ];
        let mut state = SessionListState::new(&sessions);
        // visual: 0→s0, 1→sep, 2→s1
        // Start at visual 2 (session 1)
        state.table_state.select(Some(2));
        // Move up: skip separator at visual 1, land on visual 0 (session 0)
        state.move_up();
        assert_eq!(state.table_state.selected(), Some(0));
        // Move up past start: stay at 0
        state.move_up();
        assert_eq!(state.table_state.selected(), Some(0));
    }

    #[test]
    fn test_enter_maps_visual_to_session_idx() {
        let g = VISUAL_GAP_THRESHOLD_MS + 1;
        let sessions = vec![
            make_entry(0, Some(100)),
            make_entry(100 + g, None),
        ];
        let state = SessionListState::new(&sessions);
        // visual 2 → session 1
        assert_eq!(
            visual_to_session_idx(2, &state.separator_indices),
            Some(1)
        );
    }
}
