use crate::ansi;
use tttt_pty::SessionMetadata;

/// Renders the right-side sidebar showing session list and status.
pub struct SidebarRenderer {
    width: u16,
}

/// A line of sidebar content ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarLine {
    pub row: u16,
    pub content: String,
}

impl SidebarRenderer {
    pub fn new(width: u16) -> Self {
        Self { width }
    }

    /// Render the sidebar content for the given terminal state.
    ///
    /// Returns a list of positioned sidebar lines, each containing
    /// the ANSI sequences needed to draw at the correct position.
    pub fn render(
        &self,
        sessions: &[SessionMetadata],
        active_id: Option<&str>,
        screen_cols: u16,
        screen_rows: u16,
        reminders: &[String],
    ) -> Vec<SidebarLine> {
        let mut lines = Vec::new();
        let sidebar_col = screen_cols.saturating_sub(self.width) + 1;
        let usable_width = (self.width as usize).saturating_sub(3); // "| " prefix + padding

        // Header
        lines.push(self.make_line(1, sidebar_col, &self.pad("TERMINALS", usable_width), false));

        // Separator
        lines.push(self.make_line(2, sidebar_col, &self.pad(&"-".repeat(usable_width), usable_width), false));

        // Session list
        let mut row = 3u16;
        for (i, session) in sessions.iter().enumerate() {
            if row >= screen_rows {
                break;
            }
            let is_active = active_id.map_or(false, |id| id == session.id);
            let status_char = match &session.status {
                tttt_pty::SessionStatus::Running => '*',
                tttt_pty::SessionStatus::Exited(code) => {
                    if *code == 0 {
                        '.'
                    } else {
                        '!'
                    }
                }
            };
            let display_name = session.name.as_deref().unwrap_or(&session.id);
            let label = format!("{}{} {}", i, status_char, truncate(display_name, usable_width.saturating_sub(3)));
            lines.push(self.make_line(row, sidebar_col, &self.pad(&label, usable_width), is_active));
            row += 1;
        }

        // Empty space
        if row < screen_rows.saturating_sub(reminders.len() as u16 + 2) {
            row += 1; // blank line
        }

        // Reminders section
        if !reminders.is_empty() && row < screen_rows {
            lines.push(self.make_line(row, sidebar_col, &self.pad("REMINDERS", usable_width), false));
            row += 1;
            for reminder in reminders {
                if row >= screen_rows {
                    break;
                }
                let text = truncate(reminder, usable_width);
                lines.push(self.make_line(row, sidebar_col, &self.pad(&text, usable_width), false));
                row += 1;
            }
        }

        // Fill remaining rows with empty sidebar
        while row < screen_rows {
            lines.push(self.make_line(row, sidebar_col, &self.pad("", usable_width), false));
            row += 1;
        }

        lines
    }

    fn make_line(&self, row: u16, col: u16, content: &str, highlighted: bool) -> SidebarLine {
        let attr = if highlighted {
            ansi::set_attribute(ansi::Attribute::Colors { fg: 30, bg: 47 }) // black on white
        } else {
            ansi::set_attribute(ansi::Attribute::Colors { fg: 37, bg: 40 }) // white on black
        };
        let reset = ansi::set_attribute(ansi::Attribute::Reset);

        SidebarLine {
            row,
            content: format!(
                "{}{}| {}{}{}",
                ansi::cursor_goto(row, col),
                attr,
                content,
                ansi::clear_to_eol(),
                reset,
            ),
        }
    }

    fn pad(&self, s: &str, width: usize) -> String {
        if s.len() >= width {
            s[..width].to_string()
        } else {
            format!("{:width$}", s, width = width)
        }
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tttt_pty::{SessionMetadata, SessionStatus};

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

    #[test]
    fn test_sidebar_render_empty() {
        let renderer = SidebarRenderer::new(30);
        let lines = renderer.render(&[], None, 120, 10, &[]);
        // Should have header + separator + filler lines
        assert!(!lines.is_empty());
        // First line is TERMINALS header
        assert!(lines[0].content.contains("TERMINALS"));
    }

    #[test]
    fn test_sidebar_render_sessions() {
        let renderer = SidebarRenderer::new(30);
        let sessions = vec![
            make_meta("pty-1", SessionStatus::Running),
            make_meta("pty-2", SessionStatus::Exited(0)),
        ];
        let lines = renderer.render(&sessions, Some("pty-1"), 120, 10, &[]);
        let combined: String = lines.iter().map(|l| l.content.clone()).collect();
        assert!(combined.contains("pty-1"));
        assert!(combined.contains("pty-2"));
    }

    #[test]
    fn test_sidebar_active_highlighted() {
        let renderer = SidebarRenderer::new(30);
        let sessions = vec![
            make_meta("pty-1", SessionStatus::Running),
            make_meta("pty-2", SessionStatus::Running),
        ];
        let lines = renderer.render(&sessions, Some("pty-1"), 120, 10, &[]);
        // Active session should have the highlight attribute (black on white: 30;47)
        let active_line = lines.iter().find(|l| l.content.contains("pty-1")).unwrap();
        assert!(active_line.content.contains("\x1b[30;47m"));
    }

    #[test]
    fn test_sidebar_truncates_long_names() {
        let renderer = SidebarRenderer::new(20);
        let sessions = vec![make_meta(
            "very-long-session-name-that-exceeds-width",
            SessionStatus::Running,
        )];
        let lines = renderer.render(&sessions, None, 100, 10, &[]);
        let session_line = lines.iter().find(|l| l.content.contains("very")).unwrap();
        // Should be truncated with "..."
        assert!(session_line.content.contains("...") || session_line.content.len() < 100);
    }

    #[test]
    fn test_sidebar_render_reminders() {
        let renderer = SidebarRenderer::new(30);
        let reminders = vec!["Check tests".to_string(), "Deploy v2".to_string()];
        let lines = renderer.render(&[], None, 120, 15, &reminders);
        let combined: String = lines.iter().map(|l| l.content.clone()).collect();
        assert!(combined.contains("REMINDERS"));
        assert!(combined.contains("Check tests"));
        assert!(combined.contains("Deploy v2"));
    }

    #[test]
    fn test_sidebar_status_chars() {
        let renderer = SidebarRenderer::new(30);
        let sessions = vec![
            make_meta("running", SessionStatus::Running),
            make_meta("ok", SessionStatus::Exited(0)),
            make_meta("fail", SessionStatus::Exited(1)),
        ];
        let lines = renderer.render(&sessions, None, 120, 10, &[]);
        let combined: String = lines.iter().map(|l| l.content.clone()).collect();
        assert!(combined.contains("0*"), "running should have * marker");
        assert!(combined.contains("1."), "exit 0 should have . marker");
        assert!(combined.contains("2!"), "exit non-0 should have ! marker");
    }

    #[test]
    fn test_sidebar_lines_positioned_correctly() {
        let renderer = SidebarRenderer::new(30);
        let lines = renderer.render(&[], None, 120, 5, &[]);
        // All lines should have sequential row numbers
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line.row, (i + 1) as u16);
        }
    }

    #[test]
    fn test_truncate_function() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("this is long text", 10), "this is...");
        assert_eq!(truncate("ab", 2), "ab");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
