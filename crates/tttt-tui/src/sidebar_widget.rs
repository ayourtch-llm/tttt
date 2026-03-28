use ratatui::prelude::*;
use ratatui::widgets::Widget;
use tttt_pty::{SessionMetadata, SessionStatus};

/// Ratatui widget that renders the session sidebar.
///
/// Replaces the ANSI-based `SidebarRenderer` with a proper ratatui widget
/// that writes directly into a `Buffer`.
pub struct SidebarWidget<'a> {
    sessions: &'a [SessionMetadata],
    active_id: Option<&'a str>,
    reminders: &'a [String],
    build_info: Option<&'a str>,
}

impl<'a> SidebarWidget<'a> {
    pub fn new(
        sessions: &'a [SessionMetadata],
        active_id: Option<&'a str>,
        reminders: &'a [String],
    ) -> Self {
        Self {
            sessions,
            active_id,
            reminders,
            build_info: None,
        }
    }

    pub fn build_info(mut self, info: &'a str) -> Self {
        self.build_info = Some(info);
        self
    }
}

/// Truncate a string to at most `max_len` characters. If truncation occurs and
/// `max_len > 3`, the result ends with `"..."`.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

impl<'a> Widget for SidebarWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // "| " prefix takes 2 chars; leave 1 char of right padding → 3 overhead.
        let usable_width = (area.width as usize).saturating_sub(3);

        let inactive_style = Style::default().fg(Color::White).bg(Color::Black);
        let active_style = Style::default().fg(Color::Black).bg(Color::White);

        // Helper: render a single prefixed line at the given relative row.
        let render_line = |row_offset: u16, text: &str, style: Style, buf: &mut Buffer| {
            let y = area.y + row_offset;
            if y >= area.y + area.height {
                return;
            }
            let x = area.x;
            // Fill the entire row with the style first (covers trailing space).
            for col in 0..area.width {
                buf[(x + col, y)].set_style(style);
            }
            // Write the "| " prefix.
            buf.set_string(x, y, "| ", style);
            // Write the content after the prefix.
            buf.set_string(x + 2, y, text, style);
        };

        // --- Row 0: Header ---
        let header_text = if let Some(info) = self.build_info {
            format!("TERMINALS ({}).", info)
        } else {
            "TERMINALS.".to_string()
        };
        let padded_header = format!("{:width$}", &header_text[..header_text.len().min(usable_width)], width = usable_width);
        render_line(0, &padded_header, inactive_style, buf);

        // --- Row 1: Separator ---
        let sep = format!("{:width$}", "=====", width = usable_width);
        render_line(1, &sep, inactive_style, buf);

        // --- Rows 2+: Session list ---
        let mut row: u16 = 2;
        for (i, session) in self.sessions.iter().enumerate() {
            if row >= area.height {
                break;
            }
            let is_active = self.active_id.map_or(false, |id| id == session.id);
            let style = if is_active { active_style } else { inactive_style };
            let status_char = match &session.status {
                SessionStatus::Running => '*',
                SessionStatus::Exited(code) => {
                    if *code == 0 { '.' } else { '!' }
                }
            };
            let display_name = session.name.as_deref().unwrap_or(&session.id);
            // "{i}{status_char} {name}" — the index and status_char take 2 chars, space takes 1 → 3 overhead
            let name_width = usable_width.saturating_sub(3);
            let truncated_name = truncate(display_name, name_width);
            let label = format!("{}{} {}", i, status_char, truncated_name);
            let padded_label = format!("{:width$}", &label[..label.len().min(usable_width)], width = usable_width);
            render_line(row, &padded_label, style, buf);
            row += 1;
        }

        // --- Blank line before reminders (if space allows) ---
        let reminders_section_height = if self.reminders.is_empty() {
            0u16
        } else {
            1 + self.reminders.len() as u16 // "REMINDERS" header + lines
        };
        let filler_start = row;
        let reminders_start = area.height.saturating_sub(reminders_section_height);

        // Fill rows between sessions and reminders with empty padded lines.
        let fill_end = if self.reminders.is_empty() {
            area.height
        } else {
            reminders_start
        };
        let mut r = filler_start;
        while r < fill_end {
            let empty = format!("{:width$}", "", width = usable_width);
            render_line(r, &empty, inactive_style, buf);
            r += 1;
        }

        // --- Reminders section ---
        if !self.reminders.is_empty() {
            row = reminders_start;
            if row < area.height {
                let rem_header = format!("{:width$}", "REMINDERS", width = usable_width);
                render_line(row, &rem_header, inactive_style, buf);
                row += 1;
                for reminder in self.reminders {
                    if row >= area.height {
                        break;
                    }
                    let text = truncate(reminder, usable_width);
                    let padded = format!("{:width$}", text, width = usable_width);
                    render_line(row, &padded, inactive_style, buf);
                    row += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper for tests: collect the symbol string for cells in a row between x
// offsets [x_start, x_start + len).
// ---------------------------------------------------------------------------
#[cfg(test)]
fn row_text(buf: &Buffer, area: Rect, row_offset: u16, x_start: u16, len: u16) -> String {
    let y = area.y + row_offset;
    (x_start..x_start + len)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn make_meta(id: &str, status: SessionStatus) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            command: "bash".to_string(),
            status,
            cols: 80,
            rows: 24,
            name: None,
            created_at: None,
        }
    }

    fn make_named_meta(id: &str, name: &str, status: SessionStatus) -> SessionMetadata {
        SessionMetadata {
            id: id.to_string(),
            command: "bash".to_string(),
            status,
            cols: 80,
            rows: 24,
            name: Some(name.to_string()),
            created_at: None,
        }
    }

    /// Collect the full text of a row (all columns) as a String.
    fn full_row(buf: &Buffer, area: Rect, row_offset: u16) -> String {
        row_text(buf, area, row_offset, area.x, area.width)
    }

    // ------------------------------------------------------------------
    // test_header_renders
    // ------------------------------------------------------------------
    #[test]
    fn test_header_renders() {
        let area = Rect::new(0, 0, 20, 10);
        let mut buf = Buffer::empty(area);
        let sessions: Vec<SessionMetadata> = vec![];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row0 = full_row(&buf, area, 0);
        assert!(row0.contains("TERMINALS"), "expected 'TERMINALS' in first row, got: {row0:?}");
    }

    // ------------------------------------------------------------------
    // test_header_with_build_info
    // ------------------------------------------------------------------
    #[test]
    fn test_header_with_build_info() {
        let area = Rect::new(0, 0, 30, 10);
        let mut buf = Buffer::empty(area);
        let sessions: Vec<SessionMetadata> = vec![];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders)
            .build_info("v1.0")
            .render(area, &mut buf);

        let row0 = full_row(&buf, area, 0);
        assert!(
            row0.contains("TERMINALS (v1.0)."),
            "expected 'TERMINALS (v1.0).' in first row, got: {row0:?}"
        );
    }

    // ------------------------------------------------------------------
    // test_separator_renders
    // ------------------------------------------------------------------
    #[test]
    fn test_separator_renders() {
        let area = Rect::new(0, 0, 20, 10);
        let mut buf = Buffer::empty(area);
        let sessions: Vec<SessionMetadata> = vec![];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row1 = full_row(&buf, area, 1);
        assert!(row1.contains("====="), "expected '=====' in row 1, got: {row1:?}");
    }

    // ------------------------------------------------------------------
    // test_session_list_running
    // ------------------------------------------------------------------
    #[test]
    fn test_session_list_running() {
        let area = Rect::new(0, 0, 25, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![make_meta("pty-1", SessionStatus::Running)];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row2 = full_row(&buf, area, 2);
        assert!(row2.contains('*'), "running session should show '*', got: {row2:?}");
    }

    // ------------------------------------------------------------------
    // test_session_list_exit_ok
    // ------------------------------------------------------------------
    #[test]
    fn test_session_list_exit_ok() {
        let area = Rect::new(0, 0, 25, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![make_meta("pty-1", SessionStatus::Exited(0))];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row2 = full_row(&buf, area, 2);
        assert!(row2.contains('.'), "exit-0 session should show '.', got: {row2:?}");
    }

    // ------------------------------------------------------------------
    // test_session_list_exit_fail
    // ------------------------------------------------------------------
    #[test]
    fn test_session_list_exit_fail() {
        let area = Rect::new(0, 0, 25, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![make_meta("pty-1", SessionStatus::Exited(1))];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row2 = full_row(&buf, area, 2);
        assert!(row2.contains('!'), "exit non-0 session should show '!', got: {row2:?}");
    }

    // ------------------------------------------------------------------
    // test_active_session_highlighted
    // ------------------------------------------------------------------
    #[test]
    fn test_active_session_highlighted() {
        let area = Rect::new(0, 0, 25, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![
            make_meta("pty-1", SessionStatus::Running),
            make_meta("pty-2", SessionStatus::Running),
        ];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, Some("pty-1"), &reminders).render(area, &mut buf);

        // Row 2 is the first session (pty-1), which is active.
        let cell = &buf[(area.x + 2, area.y + 2)]; // first content cell of row 2
        let style = cell.style();
        assert_eq!(
            style.fg,
            Some(Color::Black),
            "active session fg should be Black"
        );
        assert_eq!(
            style.bg,
            Some(Color::White),
            "active session bg should be White"
        );
    }

    // ------------------------------------------------------------------
    // test_inactive_session_style
    // ------------------------------------------------------------------
    #[test]
    fn test_inactive_session_style() {
        let area = Rect::new(0, 0, 25, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![
            make_meta("pty-1", SessionStatus::Running),
            make_meta("pty-2", SessionStatus::Running),
        ];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, Some("pty-1"), &reminders).render(area, &mut buf);

        // Row 3 is pty-2, which is inactive.
        let cell = &buf[(area.x + 2, area.y + 3)];
        let style = cell.style();
        assert_eq!(
            style.fg,
            Some(Color::White),
            "inactive session fg should be White"
        );
        assert_eq!(
            style.bg,
            Some(Color::Black),
            "inactive session bg should be Black"
        );
    }

    // ------------------------------------------------------------------
    // test_session_uses_name_when_available
    // ------------------------------------------------------------------
    #[test]
    fn test_session_uses_name_when_available() {
        let area = Rect::new(0, 0, 30, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![make_named_meta("pty-1", "my-session", SessionStatus::Running)];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row2 = full_row(&buf, area, 2);
        assert!(
            row2.contains("my-session"),
            "should display name 'my-session', got: {row2:?}"
        );
    }

    // ------------------------------------------------------------------
    // test_session_uses_id_when_no_name
    // ------------------------------------------------------------------
    #[test]
    fn test_session_uses_id_when_no_name() {
        let area = Rect::new(0, 0, 30, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![make_meta("pty-99", SessionStatus::Running)];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row2 = full_row(&buf, area, 2);
        assert!(
            row2.contains("pty-99"),
            "should display id 'pty-99' when name is None, got: {row2:?}"
        );
    }

    // ------------------------------------------------------------------
    // test_long_name_truncated
    // ------------------------------------------------------------------
    #[test]
    fn test_long_name_truncated() {
        // Width 15: "| " (2) + usable (12) = 14, but we set width=15 → usable = 12
        // name_width = 12 - 3 = 9
        let area = Rect::new(0, 0, 15, 10);
        let mut buf = Buffer::empty(area);
        let long_name = "averylongsessionname";
        let sessions = vec![make_named_meta("pty-1", long_name, SessionStatus::Running)];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row2 = full_row(&buf, area, 2);
        assert!(
            row2.contains("..."),
            "long name should be truncated with '...', got: {row2:?}"
        );
        assert!(
            !row2.contains(long_name),
            "full long name should not appear, got: {row2:?}"
        );
    }

    // ------------------------------------------------------------------
    // test_reminders_section
    // ------------------------------------------------------------------
    #[test]
    fn test_reminders_section() {
        let area = Rect::new(0, 0, 30, 15);
        let mut buf = Buffer::empty(area);
        let sessions: Vec<SessionMetadata> = vec![];
        let reminders = vec!["Check tests".to_string(), "Deploy v2".to_string()];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        // Collect all text across all rows.
        let all_text: String = (0..area.height)
            .map(|r| full_row(&buf, area, r))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            all_text.contains("REMINDERS"),
            "expected 'REMINDERS' header, got:\n{all_text}"
        );
        assert!(
            all_text.contains("Check tests"),
            "expected 'Check tests' reminder, got:\n{all_text}"
        );
        assert!(
            all_text.contains("Deploy v2"),
            "expected 'Deploy v2' reminder, got:\n{all_text}"
        );
    }

    // ------------------------------------------------------------------
    // test_empty_sessions
    // ------------------------------------------------------------------
    #[test]
    fn test_empty_sessions() {
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        let sessions: Vec<SessionMetadata> = vec![];
        let reminders: Vec<String> = vec![];

        // Should not panic.
        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        let row0 = full_row(&buf, area, 0);
        assert!(row0.contains("TERMINALS"), "header should still render for empty sessions");
        let row1 = full_row(&buf, area, 1);
        assert!(row1.contains("====="), "separator should still render for empty sessions");
    }

    // ------------------------------------------------------------------
    // test_filler_rows
    // ------------------------------------------------------------------
    #[test]
    fn test_filler_rows() {
        // 1 session in a 10-row area → rows 4..10 should be filler (non-empty "| " lines).
        let area = Rect::new(0, 0, 20, 10);
        let mut buf = Buffer::empty(area);
        let sessions = vec![make_meta("pty-1", SessionStatus::Running)];
        let reminders: Vec<String> = vec![];

        SidebarWidget::new(&sessions, None, &reminders).render(area, &mut buf);

        // Row 3 onwards should start with "| " (the prefix written by render_line).
        for r in 3..area.height {
            let prefix: String = row_text(&buf, area, r, area.x, 2);
            assert_eq!(
                prefix, "| ",
                "filler row {r} should start with '| ', got: {prefix:?}"
            );
        }
    }
}
