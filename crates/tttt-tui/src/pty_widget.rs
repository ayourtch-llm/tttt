use crate::selection::Selection;
use crate::vt100_style;
use ratatui::prelude::*;
use ratatui::widgets::Widget;

/// Widget that renders a vt100::Screen into a ratatui Buffer.
/// Cells beyond the PTY dimensions are filled with dim gray dots.
pub struct PtyWidget<'a> {
    screen: &'a vt100::Screen,
    selection: Option<&'a Selection>,
}

impl<'a> PtyWidget<'a> {
    pub fn new(screen: &'a vt100::Screen) -> Self {
        Self { screen, selection: None }
    }

    /// Set the active selection for visual highlighting.
    pub fn with_selection(mut self, sel: &'a Selection) -> Self {
        self.selection = Some(sel);
        self
    }
}

impl<'a> Widget for PtyWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (pty_rows, pty_cols) = self.screen.size();
        let gap_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);

        // When the PTY has more rows than the display area, show the bottom
        // portion (where the cursor and latest output are) instead of the top.
        let row_offset = pty_rows.saturating_sub(area.height);

        for row in 0..area.height {
            for col in 0..area.width {
                let x = area.x + col;
                let y = area.y + row;
                let pty_row = row + row_offset;

                if pty_row < pty_rows && col < pty_cols {
                    if let Some(cell) = self.screen.cell(pty_row, col) {
                        if cell.is_wide_continuation() {
                            continue; // skip continuation cells
                        }
                        let contents = cell.contents();
                        let symbol = if contents.is_empty() { " " } else { &contents };
                        let mut style = vt100_style::cell_style(cell);

                        // Highlight selected cells
                        if let Some(sel) = &self.selection {
                            if sel.contains(pty_row, col) {
                                style = style.add_modifier(Modifier::REVERSED);
                            }
                        }

                        buf[(x, y)].set_symbol(symbol).set_style(style);
                    }
                } else {
                    // Gap fill: dim gray dot (never highlighted)
                    buf[(x, y)].set_symbol(".").set_style(gap_style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selection;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn make_screen(input: &[u8], cols: u16, rows: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(input);
        parser
    }

    #[test]
    fn test_renders_plain_text() {
        let parser = make_screen(b"Hello", 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "H");
        assert_eq!(buf[(1, 0)].symbol(), "e");
        assert_eq!(buf[(2, 0)].symbol(), "l");
        assert_eq!(buf[(3, 0)].symbol(), "l");
        assert_eq!(buf[(4, 0)].symbol(), "o");
    }

    #[test]
    fn test_renders_bold_text() {
        // ESC[1m sets bold
        let parser = make_screen(b"\x1b[1mBold", 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "B");
        assert!(
            buf[(0, 0)].style().add_modifier.contains(Modifier::BOLD),
            "Expected BOLD modifier on 'B'"
        );
    }

    #[test]
    fn test_renders_colored_text() {
        // ESC[31m sets red foreground (index 1)
        let parser = make_screen(b"\x1b[31mRed", 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "R");
        // ANSI 31 maps to Indexed(1) in vt100
        assert_eq!(buf[(0, 0)].style().fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn test_gap_fill_width() {
        // PTY 5 cols, area 10 cols → columns 5-9 filled with gray dots
        let parser = make_screen(b"Hello", 5, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // Inside PTY area
        assert_eq!(buf[(4, 0)].symbol(), "o");
        // Gap fill: columns 5-9 in row 0
        for col in 5..10u16 {
            assert_eq!(
                buf[(col, 0)].symbol(),
                ".",
                "Expected '.' at col {col} (gap fill)"
            );
            assert_eq!(
                buf[(col, 0)].style().fg,
                Some(Color::DarkGray),
                "Expected DarkGray fg at col {col}"
            );
        }
    }

    #[test]
    fn test_gap_fill_height() {
        // PTY 2 rows, area 5 rows → rows 2-4 filled with gray dots
        let parser = make_screen(b"AB", 5, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 5, 5);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // Row 0 should have content
        assert_eq!(buf[(0, 0)].symbol(), "A");
        // Rows 2-4 should be gap fill
        for row in 2..5u16 {
            assert_eq!(
                buf[(0, row)].symbol(),
                ".",
                "Expected '.' at row {row} (gap fill)"
            );
            assert_eq!(
                buf[(0, row)].style().fg,
                Some(Color::DarkGray),
                "Expected DarkGray fg at row {row}"
            );
        }
    }

    #[test]
    fn test_area_narrower_than_pty() {
        // PTY 10x2 wraps "Hello World" across two rows; area 3x2 clips to
        // the leftmost 3 columns. Heights match so no row_offset is involved
        // and the assertions exercise pure column clipping.
        let parser = make_screen(b"Hello World", 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);
        // Should not panic
        widget.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "H");
        assert_eq!(buf[(1, 0)].symbol(), "e");
        assert_eq!(buf[(2, 0)].symbol(), "l");
    }

    #[test]
    fn test_area_shorter_than_pty_shows_bottom_rows() {
        // PTY 5x4, area 5x2 — when the PTY is taller than the area we render
        // the bottom area.height rows (so the cursor / latest output stays
        // visible). With four lines of text the bottom two rows are "C" and
        // "D".
        let parser = make_screen(b"A\r\nB\r\nC\r\nD", 5, 4);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // The bottom two PTY rows (rows 2 and 3) should appear at area
        // rows 0 and 1.
        assert_eq!(buf[(0, 0)].symbol(), "C", "area row 0 should be PTY row 2");
        assert_eq!(buf[(0, 1)].symbol(), "D", "area row 1 should be PTY row 3");
        // Top two PTY rows ("A" and "B") should not appear anywhere in
        // the rendered buffer.
        for row in 0..area.height {
            for col in 0..area.width {
                let sym = buf[(col, row)].symbol();
                assert_ne!(sym, "A", "PTY row 0 should be clipped off the top");
                assert_ne!(sym, "B", "PTY row 1 should be clipped off the top");
            }
        }
    }

    #[test]
    fn test_empty_screen() {
        // Empty screen (no input) should render spaces for PTY cells
        let parser = make_screen(b"", 5, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // All PTY cells should be spaces (empty cell contents → " ")
        for col in 0..5u16 {
            assert_eq!(
                buf[(col, 0)].symbol(),
                " ",
                "Expected space at col {col} for empty screen"
            );
        }
    }

    #[test]
    fn test_wide_character() {
        // "中" is a wide character spanning 2 columns; continuation cell is skipped
        let parser = make_screen("中".as_bytes(), 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // Column 0 should have the wide character
        assert_eq!(buf[(0, 0)].symbol(), "中");
        // Column 1 is the wide continuation — it should remain as initialized (space)
        // since we skip it with `continue`
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn test_renders_with_offset_area() {
        // Area starting at (5, 3) should still render correctly
        let parser = make_screen(b"Hi", 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(5, 3, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // Buffer coordinates are offset: x=5, y=3 for the first cell
        assert_eq!(buf[(5, 3)].symbol(), "H");
        assert_eq!(buf[(6, 3)].symbol(), "i");
    }

    #[test]
    fn test_selection_highlights_cells() {
        let parser = make_screen(b"Hello World", 20, 2);
        let mut sel = Selection::new(0, 0);
        // Select first 5 chars (cols 0-4 inclusive)
        sel.head = (0, 4);

        let widget = PtyWidget::new(parser.screen()).with_selection(&sel);
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        // Selected cells (cols 0-4, row 0) should have REVERSED modifier
        assert!(
            buf[(0, 0)].style().add_modifier.contains(Modifier::REVERSED),
            "col 0 should be REVERSED"
        );
        assert!(
            buf[(4, 0)].style().add_modifier.contains(Modifier::REVERSED),
            "col 4 should be REVERSED"
        );
        // Unselected cell (col 5) should NOT have REVERSED
        assert!(
            !buf[(5, 0)].style().add_modifier.contains(Modifier::REVERSED),
            "col 5 should NOT be REVERSED"
        );
    }

    #[test]
    fn test_no_selection_no_highlight() {
        let parser = make_screen(b"Hello", 10, 2);
        let widget = PtyWidget::new(parser.screen());
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        // No cell should have REVERSED modifier
        assert!(
            !buf[(0, 0)].style().add_modifier.contains(Modifier::REVERSED),
            "No selection: col 0 should NOT be REVERSED"
        );
    }

    #[test]
    fn test_selection_does_not_affect_gap_fill() {
        // PTY is 5 cols wide, area is 10 cols — cols 5-9 are gap fill
        let parser = make_screen(b"Hi", 5, 2);
        let mut sel = Selection::new(0, 0);
        sel.head = (0, 9); // extends into gap area (beyond PTY cols)

        let widget = PtyWidget::new(parser.screen()).with_selection(&sel);
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        // Gap fill cells (cols 5-9) should NOT be highlighted
        for col in 5..10u16 {
            assert!(
                !buf[(col, 0)].style().add_modifier.contains(Modifier::REVERSED),
                "Gap fill col {col} should NOT be REVERSED"
            );
        }
    }
}
