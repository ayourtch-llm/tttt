use crate::ansi;

/// Renders a vt100::Screen into a sub-region of the real terminal.
///
/// Instead of using contents_diff() (which assumes the terminal is the same
/// width as the PTY), we read cells from the screen and position them
/// explicitly on the real terminal. This is the approach zellij uses.
pub struct PaneRenderer {
    /// Previous screen content for dirty-tracking (plain text per cell).
    prev_contents: Vec<Vec<String>>,
    /// Previous attribute state per cell.
    prev_attrs: Vec<Vec<CellAttrs>>,
    /// Size of the PTY/pane.
    pane_cols: u16,
    pane_rows: u16,
    /// Offset on the real terminal where the pane starts (1-indexed).
    offset_row: u16,
    offset_col: u16,
    /// Whether to force a full redraw on next render.
    force_redraw: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct CellAttrs {
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    fgcolor: vt100::Color,
    bgcolor: vt100::Color,
}

impl CellAttrs {
    fn from_cell(cell: &vt100::Cell) -> Self {
        Self {
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
            fgcolor: cell.fgcolor(),
            bgcolor: cell.bgcolor(),
        }
    }

    fn write_ansi(&self, buf: &mut Vec<u8>) {
        use std::io::Write;

        let _ = write!(buf, "\x1b[0");
        if self.bold {
            let _ = write!(buf, ";1");
        }
        if self.italic {
            let _ = write!(buf, ";3");
        }
        if self.underline {
            let _ = write!(buf, ";4");
        }
        if self.inverse {
            let _ = write!(buf, ";7");
        }
        write_color_fg(buf, self.fgcolor);
        write_color_bg(buf, self.bgcolor);
        let _ = write!(buf, "m");
    }
}

fn write_color_fg(buf: &mut Vec<u8>, color: vt100::Color) {
    use std::io::Write;
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => {
            let _ = write!(buf, ";{}", 30 + i);
        }
        vt100::Color::Idx(i) if i < 16 => {
            let _ = write!(buf, ";{}", 90 + i - 8);
        }
        vt100::Color::Idx(i) => {
            let _ = write!(buf, ";38;5;{}", i);
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(buf, ";38;2;{};{};{}", r, g, b);
        }
    }
}

fn write_color_bg(buf: &mut Vec<u8>, color: vt100::Color) {
    use std::io::Write;
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => {
            let _ = write!(buf, ";{}", 40 + i);
        }
        vt100::Color::Idx(i) if i < 16 => {
            let _ = write!(buf, ";{}", 100 + i - 8);
        }
        vt100::Color::Idx(i) => {
            let _ = write!(buf, ";48;5;{}", i);
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(buf, ";48;2;{};{};{}", r, g, b);
        }
    }
}

impl PaneRenderer {
    /// Create a new pane renderer.
    ///
    /// `offset_row` and `offset_col` are 1-indexed terminal coordinates
    /// where the top-left of the pane should be drawn.
    pub fn new(pane_cols: u16, pane_rows: u16, offset_row: u16, offset_col: u16) -> Self {
        Self {
            prev_contents: vec![vec![" ".to_string(); pane_cols as usize]; pane_rows as usize],
            prev_attrs: vec![vec![CellAttrs::default(); pane_cols as usize]; pane_rows as usize],
            pane_cols,
            pane_rows,
            offset_row,
            offset_col,
            force_redraw: true, // first render is always full
        }
    }

    /// Render the screen into the pane area, returning bytes to write to the terminal.
    /// Only emits changes since the last render (dirty-tracking).
    pub fn render(&mut self, screen: &vt100::Screen) -> Vec<u8> {
        let mut output = Vec::new();
        let mut last_written_attrs = CellAttrs::default();
        let mut any_output = false;

        for row in 0..self.pane_rows {
            for col in 0..self.pane_cols {
                let cell = screen.cell(row, col);
                let (contents, attrs) = match cell {
                    Some(cell) => {
                        let c = cell.contents();
                        let c = if c.is_empty() { " ".to_string() } else { c };
                        (c, CellAttrs::from_cell(cell))
                    }
                    None => (" ".to_string(), CellAttrs::default()),
                };

                let r = row as usize;
                let c = col as usize;
                let changed = self.force_redraw
                    || self.prev_contents[r][c] != contents
                    || self.prev_attrs[r][c] != attrs;

                if changed {
                    // Position cursor
                    let term_row = self.offset_row + row;
                    let term_col = self.offset_col + col;
                    output.extend_from_slice(
                        ansi::cursor_goto(term_row, term_col).as_bytes(),
                    );

                    // Set attributes if different from last written
                    if attrs != last_written_attrs || !any_output {
                        let mut attr_buf = Vec::new();
                        attrs.write_ansi(&mut attr_buf);
                        output.extend_from_slice(&attr_buf);
                        last_written_attrs = attrs.clone();
                    }

                    // Write cell contents
                    output.extend_from_slice(contents.as_bytes());
                    any_output = true;

                    self.prev_contents[r][c] = contents;
                    self.prev_attrs[r][c] = attrs;
                }
            }
        }

        self.force_redraw = false;

        // Reset attributes after rendering
        if any_output {
            output.extend_from_slice(ansi::set_attribute(ansi::Attribute::Reset).as_bytes());
        }

        output
    }

    /// Force a full redraw on next render.
    pub fn invalidate(&mut self) {
        self.force_redraw = true;
    }

    /// Resize the pane renderer.
    pub fn resize(&mut self, pane_cols: u16, pane_rows: u16) {
        self.pane_cols = pane_cols;
        self.pane_rows = pane_rows;
        self.prev_contents =
            vec![vec![" ".to_string(); pane_cols as usize]; pane_rows as usize];
        self.prev_attrs =
            vec![vec![CellAttrs::default(); pane_cols as usize]; pane_rows as usize];
        self.force_redraw = true;
    }

    /// Get the terminal position for a PTY cursor position.
    /// Converts 0-indexed PTY coords to 1-indexed terminal coords.
    pub fn cursor_terminal_position(&self, pty_row: u16, pty_col: u16) -> (u16, u16) {
        (self.offset_row + pty_row, self.offset_col + pty_col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_display(output: &[u8], rows: u16, cols: u16) -> vt100::Parser {
        let mut display = vt100::Parser::new(rows, cols, 0);
        display.process(output);
        display
    }

    #[test]
    fn test_pane_renderer_simple() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"hello");

        let mut renderer = PaneRenderer::new(20, 5, 1, 1);
        let output = renderer.render(parser.screen());

        // Verify by parsing on a display
        let display = render_to_display(&output, 5, 20);
        let contents = display.screen().contents();
        assert!(
            contents.contains("hello"),
            "display should contain 'hello': {:?}",
            contents
        );
    }

    #[test]
    fn test_pane_renderer_dirty_tracking() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"hello");

        let mut renderer = PaneRenderer::new(20, 5, 1, 1);
        let output1 = renderer.render(parser.screen());
        assert!(!output1.is_empty());

        // Second render with no changes should produce nothing
        let output2 = renderer.render(parser.screen());
        assert!(
            output2.is_empty(),
            "second render with no changes should be empty, got {} bytes",
            output2.len()
        );
    }

    #[test]
    fn test_pane_renderer_incremental_update() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"hello");

        let mut renderer = PaneRenderer::new(20, 5, 1, 1);
        let output1 = renderer.render(parser.screen());

        // Apply to display
        let mut display = vt100::Parser::new(5, 20, 0);
        display.process(&output1);
        assert!(display.screen().contents().contains("hello"));

        // Add more text
        parser.process(b" world");
        let output2 = renderer.render(parser.screen());

        // output2 should be smaller than output1 (only the changed cells)
        assert!(
            output2.len() < output1.len(),
            "incremental should be smaller: {} vs {}",
            output2.len(),
            output1.len()
        );

        display.process(&output2);
        assert!(
            display.screen().contents().contains("hello world"),
            "after incremental: {:?}",
            display.screen().contents()
        );
    }

    #[test]
    fn test_pane_renderer_offset() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"X");

        // Pane starts at terminal row 3, col 5
        let mut renderer = PaneRenderer::new(20, 5, 3, 5);
        let output = renderer.render(parser.screen());

        // Verify on a wider display
        let display = render_to_display(&output, 10, 30);
        // 'X' should be at row 2 (0-indexed), col 4 (0-indexed) = (3-1, 5-1)
        let cell = display.screen().cell(2, 4);
        assert_eq!(
            cell.map(|c| c.contents()),
            Some("X".to_string()),
            "X should be at offset position"
        );
    }

    #[test]
    fn test_pane_renderer_cursor_position() {
        let renderer = PaneRenderer::new(50, 10, 1, 1);
        assert_eq!(renderer.cursor_terminal_position(0, 0), (1, 1));
        assert_eq!(renderer.cursor_terminal_position(5, 10), (6, 11));
    }

    #[test]
    fn test_pane_renderer_cursor_position_with_offset() {
        let renderer = PaneRenderer::new(50, 10, 3, 5);
        assert_eq!(renderer.cursor_terminal_position(0, 0), (3, 5));
        assert_eq!(renderer.cursor_terminal_position(2, 7), (5, 12));
    }

    #[test]
    fn test_pane_renderer_invalidate() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"hello");

        let mut renderer = PaneRenderer::new(20, 5, 1, 1);
        let _ = renderer.render(parser.screen());

        // Second render: no changes
        let output2 = renderer.render(parser.screen());
        assert!(output2.is_empty());

        // After invalidate, should re-render everything
        renderer.invalidate();
        let output3 = renderer.render(parser.screen());
        assert!(!output3.is_empty(), "after invalidate should re-render");
    }

    #[test]
    fn test_pane_renderer_newlines() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"line1\r\nline2");

        let mut renderer = PaneRenderer::new(20, 5, 1, 1);
        let output = renderer.render(parser.screen());

        let display = render_to_display(&output, 5, 20);
        let contents = display.screen().contents();
        assert!(contents.contains("line1"), "should have line1: {:?}", contents);
        assert!(contents.contains("line2"), "should have line2: {:?}", contents);
    }

    #[test]
    fn test_pane_renderer_wider_display() {
        let pty_cols: u16 = 40;
        let pty_rows: u16 = 5;

        let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);
        parser.process(b"$ ls\r\nfile1  file2\r\n$ ");

        let mut renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
        let output = renderer.render(parser.screen());

        // Parse on a WIDER terminal
        let display = render_to_display(&output, pty_rows, 80);
        let contents = display.screen().contents();
        assert!(contents.contains("$ ls"), "should show '$ ls': {:?}", contents);
        assert!(contents.contains("file1  file2"), "should show files: {:?}", contents);
    }

    #[test]
    fn test_pane_renderer_sequential_updates() {
        let pty_cols: u16 = 40;
        let pty_rows: u16 = 5;
        let display_cols: u16 = 80;

        let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);
        let mut renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
        let mut display = vt100::Parser::new(pty_rows, display_cols, 0);

        // Step 1: prompt
        parser.process(b"$ ");
        let output1 = renderer.render(parser.screen());
        display.process(&output1);
        assert!(display.screen().contents().contains("$ "), "step 1");

        // Step 2: command + output
        parser.process(b"ls\r\nfile1\r\n$ ");
        let output2 = renderer.render(parser.screen());
        display.process(&output2);

        let contents = display.screen().contents();
        assert!(contents.contains("$ ls"), "step 2 '$ ls': {:?}", contents);
        assert!(contents.contains("file1"), "step 2 'file1': {:?}", contents);
    }

    #[test]
    fn test_pane_renderer_with_colors() {
        let mut parser = vt100::Parser::new(5, 40, 0);
        parser.process(b"\x1b[32mgreen\x1b[0m normal");

        let mut renderer = PaneRenderer::new(40, 5, 1, 1);
        let output = renderer.render(parser.screen());

        // Verify the output contains color sequences
        let s = String::from_utf8_lossy(&output);
        assert!(s.contains(";32m"), "should have green color: {:?}", s);

        // Verify on display
        let display = render_to_display(&output, 5, 40);
        let contents = display.screen().contents();
        assert!(contents.contains("green"), "display: {:?}", contents);
        assert!(contents.contains("normal"), "display: {:?}", contents);
    }

    #[test]
    fn test_pane_renderer_resize() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"hello");

        let mut renderer = PaneRenderer::new(20, 5, 1, 1);
        let _ = renderer.render(parser.screen());

        renderer.resize(30, 8);
        // After resize, force_redraw is true
        parser.set_size(8, 30);
        let output = renderer.render(parser.screen());
        assert!(!output.is_empty(), "resize should trigger redraw");
    }
}
