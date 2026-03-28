/// Tracks a text selection between two screen coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Anchor point (where mouse press started) — (row, col), 0-indexed
    pub anchor: (u16, u16),
    /// Head point (current mouse position) — (row, col), 0-indexed
    pub head: (u16, u16),
}

impl Selection {
    /// Create a new selection with anchor at the given position.
    pub fn new(row: u16, col: u16) -> Self {
        Self {
            anchor: (row, col),
            head: (row, col),
        }
    }

    /// Update the head (drag) position.
    pub fn update(&mut self, row: u16, col: u16) {
        self.head = (row, col);
    }

    /// Get the normalized start and end positions (start <= end in reading order).
    /// Returns ((start_row, start_col), (end_row, end_col)).
    pub fn range(&self) -> ((u16, u16), (u16, u16)) {
        let (ar, ac) = self.anchor;
        let (hr, hc) = self.head;
        if ar < hr {
            ((ar, ac), (hr, hc))
        } else if ar > hr {
            ((hr, hc), (ar, ac))
        } else {
            // Same row: order by col
            if ac <= hc {
                ((ar, ac), (hr, hc))
            } else {
                ((hr, hc), (ar, ac))
            }
        }
    }

    /// Check if a cell at (row, col) is within the selection.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let ((start_row, start_col), (end_row, end_col)) = self.range();
        if row < start_row || row > end_row {
            return false;
        }
        if start_row == end_row {
            // Single line
            col >= start_col && col <= end_col
        } else if row == start_row {
            // First line of multi-line
            col >= start_col
        } else if row == end_row {
            // Last line of multi-line
            col <= end_col
        } else {
            // Middle row
            true
        }
    }

    /// Check if the selection is empty (anchor == head, same cell).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Extract the selected text from a vt100 screen.
    pub fn extract_text(&self, screen: &vt100::Screen) -> String {
        let ((start_row, start_col), (end_row, end_col)) = self.range();
        let screen_cols = screen.size().1;

        if start_row == end_row {
            // Single line
            let line = extract_line(screen, start_row, start_col, end_col);
            line.trim_end().to_string()
        } else {
            // Multi-line
            let mut lines = Vec::new();

            // First line: from start_col to end of screen
            let first = extract_line(screen, start_row, start_col, screen_cols.saturating_sub(1));
            lines.push(first.trim_end().to_string());

            // Middle lines: full width
            for row in (start_row + 1)..end_row {
                let mid = extract_line(screen, row, 0, screen_cols.saturating_sub(1));
                lines.push(mid.trim_end().to_string());
            }

            // Last line: from col 0 to end_col
            let last = extract_line(screen, end_row, 0, end_col);
            lines.push(last.trim_end().to_string());

            lines.join("\n")
        }
    }
}

/// Extract text from a single screen line between start_col and end_col (inclusive).
fn extract_line(screen: &vt100::Screen, row: u16, start_col: u16, end_col: u16) -> String {
    let mut result = String::new();
    let screen_cols = screen.size().1;
    let actual_end = end_col.min(screen_cols.saturating_sub(1));
    if start_col > actual_end {
        return result;
    }
    for col in start_col..=actual_end {
        if let Some(cell) = screen.cell(row, col) {
            if cell.is_wide_continuation() {
                continue;
            }
            let contents = cell.contents();
            if contents.is_empty() {
                result.push(' ');
            } else {
                result.push_str(&contents);
            }
        } else {
            result.push(' ');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_screen(input: &[u8], cols: u16, rows: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(input);
        parser
    }

    #[test]
    fn test_new_selection() {
        let sel = Selection::new(5, 10);
        assert_eq!(sel.anchor, (5, 10));
        assert_eq!(sel.head, (5, 10));
    }

    #[test]
    fn test_update_head() {
        let mut sel = Selection::new(5, 10);
        sel.update(7, 15);
        assert_eq!(sel.head, (7, 15));
        assert_eq!(sel.anchor, (5, 10)); // unchanged
    }

    #[test]
    fn test_range_forward() {
        let mut sel = Selection::new(2, 5);
        sel.update(4, 10);
        assert_eq!(sel.range(), ((2, 5), (4, 10)));
    }

    #[test]
    fn test_range_backward() {
        let mut sel = Selection::new(4, 10);
        sel.update(2, 5);
        assert_eq!(sel.range(), ((2, 5), (4, 10)));
    }

    #[test]
    fn test_range_same_row_forward() {
        let mut sel = Selection::new(3, 2);
        sel.update(3, 8);
        assert_eq!(sel.range(), ((3, 2), (3, 8)));
    }

    #[test]
    fn test_range_same_row_backward() {
        let mut sel = Selection::new(3, 8);
        sel.update(3, 2);
        assert_eq!(sel.range(), ((3, 2), (3, 8)));
    }

    #[test]
    fn test_contains_single_line() {
        let mut sel = Selection::new(3, 2);
        sel.update(3, 6);
        assert!(sel.contains(3, 2));
        assert!(sel.contains(3, 4));
        assert!(sel.contains(3, 6));
        assert!(!sel.contains(3, 1));
        assert!(!sel.contains(3, 7));
        assert!(!sel.contains(2, 4));
        assert!(!sel.contains(4, 4));
    }

    #[test]
    fn test_contains_multi_line() {
        let mut sel = Selection::new(2, 5);
        sel.update(4, 3);
        // First line: col >= 5
        assert!(sel.contains(2, 5));
        assert!(sel.contains(2, 10));
        assert!(!sel.contains(2, 4));
        // Middle line: any col
        assert!(sel.contains(3, 0));
        assert!(sel.contains(3, 50));
        // Last line: col <= 3
        assert!(sel.contains(4, 0));
        assert!(sel.contains(4, 3));
        assert!(!sel.contains(4, 4));
        // Outside
        assert!(!sel.contains(1, 5));
        assert!(!sel.contains(5, 0));
    }

    #[test]
    fn test_extract_text_single_line() {
        let parser = make_screen(b"Hello World!", 20, 3);
        let mut sel = Selection::new(0, 0);
        sel.update(0, 4);
        assert_eq!(sel.extract_text(parser.screen()), "Hello");
    }

    #[test]
    fn test_extract_text_multi_line() {
        // Two lines: "Hello" and "World"
        let parser = make_screen(b"Hello\r\nWorld", 20, 3);
        let mut sel = Selection::new(0, 3);
        sel.update(1, 4);
        // First line from col 3 to end: "lo" (trimmed)
        // Last line from 0 to col 4: "World"
        assert_eq!(sel.extract_text(parser.screen()), "lo\nWorld");
    }

    #[test]
    fn test_extract_text_trailing_whitespace_trimmed() {
        let parser = make_screen(b"Hi   ", 10, 2);
        let mut sel = Selection::new(0, 0);
        sel.update(0, 9);
        assert_eq!(sel.extract_text(parser.screen()), "Hi");
    }

    #[test]
    fn test_is_empty() {
        let sel = Selection::new(3, 5);
        assert!(sel.is_empty());
        let mut sel2 = Selection::new(3, 5);
        sel2.update(3, 6);
        assert!(!sel2.is_empty());
    }

    #[test]
    fn test_extract_text_empty_selection() {
        let parser = make_screen(b"Hello", 20, 3);
        let sel = Selection::new(0, 2);
        // Single cell selection
        assert_eq!(sel.extract_text(parser.screen()), "l");
    }

    #[test]
    fn test_extract_text_wide_char() {
        // "Hello中World": H(0)e(1)l(2)l(3)o(4)中(5,6)W(7)o(8)r(9)l(10)d(11)
        // 中 is a wide char occupying cols 5 and 6 (continuation)
        // "World" ends at col 11, so select 0..=11 to capture all
        let parser = make_screen("Hello中World".as_bytes(), 20, 3);
        let mut sel = Selection::new(0, 0);
        sel.update(0, 11);
        let text = sel.extract_text(parser.screen());
        assert!(text.contains("Hello"));
        assert!(text.contains("中"));
        assert!(text.contains("World"));
    }
}
