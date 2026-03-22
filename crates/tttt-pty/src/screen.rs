/// VT100 terminal screen buffer wrapping the vt100 crate.
///
/// Maintains a virtual terminal grid and interprets ANSI escape sequences
/// from PTY output to track the terminal state.
pub struct ScreenBuffer {
    parser: vt100::Parser,
    prev_screen: vt100::Screen,
    cols: u16,
    rows: u16,
    scrollback_lines: usize,
}

impl ScreenBuffer {
    /// Create a new screen buffer with the given dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, 1000)
    }

    /// Create a new screen buffer with custom scrollback depth.
    pub fn with_scrollback(cols: u16, rows: u16, scrollback: usize) -> Self {
        let parser = vt100::Parser::new(rows, cols, scrollback);
        let prev_screen = parser.screen().clone();
        Self {
            parser,
            prev_screen,
            cols,
            rows,
            scrollback_lines: scrollback,
        }
    }

    /// Feed raw bytes from PTY output into the parser.
    pub fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// Get the plain text contents of the visible screen.
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Get the screen contents with ANSI formatting codes.
    pub fn contents_formatted(&self) -> Vec<u8> {
        self.parser.screen().contents_formatted()
    }

    /// Get the ANSI diff between current screen and last snapshot.
    /// This is the key optimization for efficient rendering — only
    /// the changed bytes need to be written to the real terminal.
    pub fn contents_diff(&mut self) -> Vec<u8> {
        let diff = self.parser.screen().contents_diff(&self.prev_screen);
        self.prev_screen = self.parser.screen().clone();
        diff
    }

    /// Get current cursor position as (row, col), 0-indexed.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Get screen dimensions.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Resize the screen buffer. Resets the parser state.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.parser = vt100::Parser::new(rows, cols, self.scrollback_lines);
        self.prev_screen = self.parser.screen().clone();
    }

    /// Set scrollback buffer depth. Recreates the parser.
    pub fn set_scrollback(&mut self, lines: usize) {
        self.scrollback_lines = lines;
        self.parser = vt100::Parser::new(self.rows, self.cols, lines);
        self.prev_screen = self.parser.screen().clone();
    }

    /// Get the scrollback buffer depth.
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback_lines
    }

    /// Access the underlying vt100 screen for advanced operations.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_correct_dimensions() {
        let buf = ScreenBuffer::new(80, 24);
        assert_eq!(buf.size(), (80, 24));
    }

    #[test]
    fn test_with_scrollback() {
        let buf = ScreenBuffer::with_scrollback(80, 24, 5000);
        assert_eq!(buf.scrollback_lines(), 5000);
    }

    #[test]
    fn test_process_output_updates_contents() {
        let mut buf = ScreenBuffer::new(80, 24);
        buf.process(b"hello world");
        let contents = buf.contents();
        assert!(contents.contains("hello world"));
    }

    #[test]
    fn test_cursor_position_initial() {
        let buf = ScreenBuffer::new(80, 24);
        assert_eq!(buf.cursor_position(), (0, 0));
    }

    #[test]
    fn test_cursor_position_after_text() {
        let mut buf = ScreenBuffer::new(80, 24);
        buf.process(b"hello");
        assert_eq!(buf.cursor_position(), (0, 5));
    }

    #[test]
    fn test_cursor_position_after_newline() {
        let mut buf = ScreenBuffer::new(80, 24);
        buf.process(b"hello\r\nworld");
        assert_eq!(buf.cursor_position(), (1, 5));
    }

    #[test]
    fn test_resize() {
        let mut buf = ScreenBuffer::new(80, 24);
        buf.process(b"some content");
        buf.resize(120, 40);
        assert_eq!(buf.size(), (120, 40));
        // screen is reset after resize
        assert_eq!(buf.cursor_position(), (0, 0));
    }

    #[test]
    fn test_contents_diff_empty_on_no_change() {
        let mut buf = ScreenBuffer::new(80, 24);
        let diff = buf.contents_diff();
        // initial diff may contain cursor positioning; subsequent should be empty
        let diff2 = buf.contents_diff();
        assert!(diff2.is_empty(), "second diff with no changes should be empty, got {} bytes", diff2.len());
        let _ = diff; // suppress unused warning
    }

    #[test]
    fn test_contents_diff_nonempty_after_change() {
        let mut buf = ScreenBuffer::new(80, 24);
        let _ = buf.contents_diff(); // consume initial state
        buf.process(b"new text");
        let diff = buf.contents_diff();
        assert!(!diff.is_empty(), "diff should contain changes");
    }

    #[test]
    fn test_contents_formatted_includes_ansi() {
        let mut buf = ScreenBuffer::new(80, 24);
        // Send text with bold attribute
        buf.process(b"\x1b[1mBOLD\x1b[0m");
        let formatted = buf.contents_formatted();
        // Should contain ESC sequence
        assert!(formatted.windows(2).any(|w| w == b"\x1b["), "formatted output should contain ANSI codes");
    }

    #[test]
    fn test_set_scrollback() {
        let mut buf = ScreenBuffer::new(80, 24);
        buf.set_scrollback(2000);
        assert_eq!(buf.scrollback_lines(), 2000);
    }

    #[test]
    fn test_contents_diff_resets_baseline() {
        let mut buf = ScreenBuffer::new(80, 24);
        let _ = buf.contents_diff();
        buf.process(b"first");
        let diff1 = buf.contents_diff();
        assert!(!diff1.is_empty());
        // second diff without new input should be empty
        let diff2 = buf.contents_diff();
        assert!(diff2.is_empty());
        // new input should produce diff again
        buf.process(b"second");
        let diff3 = buf.contents_diff();
        assert!(!diff3.is_empty());
    }

    #[test]
    fn test_screen_access() {
        let buf = ScreenBuffer::new(80, 24);
        let screen = buf.screen();
        assert_eq!(screen.size(), (24, 80));
    }
}
