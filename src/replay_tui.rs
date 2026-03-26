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
/// Sorted by started_at_ms so orphans interleave correctly with registered sessions.
fn build_session_list(db: &SqliteLogger) -> Result<Vec<SessionListEntry>> {
    let mut sessions = Vec::new();

    for info in db.list_sessions()? {
        let event_count = db.query_events_with_pid(&info.session_id, info.pid).map(|e| e.len()).unwrap_or(0);
        sessions.push(SessionListEntry { info, event_count });
    }

    for (orphan_id, pid) in db.list_orphan_session_ids()? {
        if let Some(info) = db.infer_session_info(&orphan_id, pid)? {
            let event_count = db.query_events_with_pid(&orphan_id, pid).map(|e| e.len()).unwrap_or(0);
            sessions.push(SessionListEntry { info, event_count });
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

struct SessionListEntry {
    info: SessionInfo,
    event_count: usize,
}

struct SessionListState {
    table_state: TableState,
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
            let mut table_state = TableState::default();
            if !sessions.is_empty() {
                table_state.select(Some(0));
            }
            View::SessionList(SessionListState { table_state })
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
        match &mut self.view {
            View::SessionList(state) => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    let i = state.table_state.selected().unwrap_or(0);
                    if i + 1 < self.sessions.len() {
                        state.table_state.select(Some(i + 1));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let i = state.table_state.selected().unwrap_or(0);
                    if i > 0 {
                        state.table_state.select(Some(i - 1));
                    }
                }
                KeyCode::Enter => {
                    if let Some(i) = state.table_state.selected() {
                        if i < self.sessions.len() {
                            let session_id = self.sessions[i].info.session_id.clone();
                            let pid = self.sessions[i].info.pid;
                            self.open_session(&session_id, pid)?;
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
                    let mut table_state = TableState::default();
                    if !self.sessions.is_empty() {
                        table_state.select(Some(0));
                    }
                    self.view = View::SessionList(SessionListState { table_state });
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

    let rows: Vec<Row> = sessions
        .iter()
        .map(|s| {
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
            Row::new(vec![
                RCell::from(short_id),
                RCell::from(pid),
                RCell::from(name),
                RCell::from(s.info.command.clone()),
                RCell::from(size),
                RCell::from(started),
                RCell::from(duration),
                RCell::from(s.event_count.to_string()),
            ])
        })
        .collect();

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
        }];
        let app = ReplayApp::new(db, sessions, None).unwrap();
        assert!(matches!(app.view, View::SessionList(_)));
    }

    #[test]
    fn test_replay_app_new_with_initial_session() {
        let db = make_db_with_session();
        let info = db.get_session_info("sess1").unwrap().unwrap();
        let event_count = db.query_events("sess1").unwrap().len();
        let sessions = vec![SessionListEntry { info, event_count }];
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
}
