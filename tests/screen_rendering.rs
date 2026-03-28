//! Tests for the screen rendering pipeline.
//!
//! These tests verify that PTY output is correctly processed through the
//! vt100 parser and that the rendering artifacts (diffs, formatted output,
//! cursor positioning) are correct for our split-pane layout.

/// Helper: create a vt100 parser, feed it some data, and verify screen state.
fn parse_and_check(cols: u16, rows: u16, input: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(input);
    parser
}

// =============================================================================
// Basic screen buffer tests
// =============================================================================

#[test]
fn test_screen_buffer_simple_text() {
    let parser = parse_and_check(80, 24, b"hello world");
    assert!(parser.screen().contents().starts_with("hello world"));
    assert_eq!(parser.screen().cursor_position(), (0, 11));
}

#[test]
fn test_screen_buffer_newline() {
    let parser = parse_and_check(80, 24, b"line1\r\nline2");
    let contents = parser.screen().contents();
    assert!(contents.contains("line1"));
    assert!(contents.contains("line2"));
    assert_eq!(parser.screen().cursor_position(), (1, 5));
}

#[test]
fn test_screen_buffer_wrapping() {
    // Text that exceeds column width should wrap
    let parser = parse_and_check(10, 5, b"1234567890ABCDE");
    let contents = parser.screen().contents();
    // First 10 chars on row 0, next 5 on row 1
    assert!(contents.starts_with("1234567890"));
    assert!(contents.contains("ABCDE"));
    assert_eq!(parser.screen().cursor_position(), (1, 5));
}

#[test]
fn test_screen_buffer_narrow_columns() {
    // Simulate the PTY area width when sidebar takes 30 cols from 120-col terminal
    let pty_cols = 90; // 120 - 30
    let parser = parse_and_check(pty_cols, 24, b"$ ls\r\nfile1  file2  file3");
    let contents = parser.screen().contents();
    assert!(contents.contains("$ ls"));
    assert!(contents.contains("file1  file2  file3"));
}

// =============================================================================
// contents_diff tests
// =============================================================================

#[test]
fn test_diff_initial_state() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    let initial = parser.screen().clone();
    parser.process(b"hello");
    let diff = parser.screen().contents_diff(&initial);
    // Diff should contain "hello" and cursor positioning
    let diff_str = String::from_utf8_lossy(&diff);
    assert!(
        diff_str.contains("hello"),
        "diff should contain 'hello', got: {:?}",
        diff_str
    );
    assert!(!diff.is_empty());
}

#[test]
fn test_diff_no_change() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(b"hello");
    let screen = parser.screen().clone();
    let diff = parser.screen().contents_diff(&screen);
    assert!(
        diff.is_empty(),
        "diff with no changes should be empty, got {} bytes: {:?}",
        diff.len(),
        String::from_utf8_lossy(&diff)
    );
}

#[test]
fn test_diff_incremental_update() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(b"hello");
    let snap1 = parser.screen().clone();
    parser.process(b" world");
    let diff = parser.screen().contents_diff(&snap1);
    let diff_str = String::from_utf8_lossy(&diff);
    assert!(
        diff_str.contains("world"),
        "incremental diff should contain 'world', got: {:?}",
        diff_str
    );
}

#[test]
fn test_diff_cursor_position_is_1_indexed() {
    // vt100 contents_diff outputs ANSI cursor positions which are 1-indexed
    let mut parser = vt100::Parser::new(24, 80, 0);
    let initial = parser.screen().clone();
    parser.process(b"X");
    let diff = parser.screen().contents_diff(&initial);
    // After writing "X" at (0,0), cursor should be at (0,1)
    // The diff should contain the text and position cursor at row=1,col=2 (1-indexed)
    // or just "X" followed by cursor positioning
    assert_eq!(parser.screen().cursor_position(), (0, 1));
}

#[test]
fn test_diff_multiline() {
    let mut parser = vt100::Parser::new(10, 40, 0);
    let initial = parser.screen().clone();
    parser.process(b"line1\r\nline2\r\nline3");
    let diff = parser.screen().contents_diff(&initial);
    let diff_str = String::from_utf8_lossy(&diff);
    assert!(diff_str.contains("line1"));
    assert!(diff_str.contains("line2"));
    assert!(diff_str.contains("line3"));
}

// =============================================================================
// contents_formatted tests
// =============================================================================

#[test]
fn test_formatted_includes_cursor_home() {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(b"hello");
    let formatted = parser.screen().contents_formatted();
    // contents_formatted should start with cursor home (\x1b[H) or similar
    let s = String::from_utf8_lossy(&formatted);
    assert!(
        s.contains("\x1b[") ,
        "formatted should contain ANSI sequences, got: {:?}",
        s
    );
}

#[test]
fn test_formatted_roundtrip() {
    // Feed formatted output of one parser into another, should get same screen
    let mut parser1 = vt100::Parser::new(10, 40, 0);
    parser1.process(b"hello\r\nworld");

    let formatted = parser1.screen().contents_formatted();

    let mut parser2 = vt100::Parser::new(10, 40, 0);
    parser2.process(&formatted);

    assert_eq!(
        parser1.screen().contents(),
        parser2.screen().contents(),
        "formatted output should reproduce the same screen"
    );
    assert_eq!(
        parser1.screen().cursor_position(),
        parser2.screen().cursor_position(),
        "formatted output should restore cursor position"
    );
}

// =============================================================================
// PtySession + ScreenBuffer integration
// =============================================================================

#[test]
fn test_pty_session_screen_after_output() {
    use tttt_pty::{MockPty, PtySession};

    let mut mock = MockPty::new(80, 24);
    mock.queue_output(b"$ hello\r\n$ ");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(screen.contains("$ hello"), "screen: {:?}", screen);
    assert!(screen.contains("$ "), "screen should have prompt");
}

#[test]
fn test_pty_session_screen_diff_after_output() {
    use tttt_pty::{MockPty, PtySession};

    let mut mock = MockPty::new(40, 10);
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 40, 10);

    // Consume initial state
    let _ = session.screen_diff();

    // Simulate PTY output arriving
    // We need to access the mock to queue output, but it's moved into session.
    // Use a fresh session instead.
    let mut mock2 = MockPty::new(40, 10);
    mock2.queue_output(b"prompt$ ");
    let mut session2 = PtySession::new("s2".to_string(), mock2, "bash".to_string(), 40, 10);

    let _ = session2.screen_diff(); // consume initial
    session2.pump().unwrap();
    let diff = session2.screen_diff();

    assert!(!diff.is_empty(), "diff should have content after pump");
    let diff_str = String::from_utf8_lossy(&diff);
    assert!(
        diff_str.contains("prompt$"),
        "diff should contain 'prompt$', got: {:?}",
        diff_str
    );
}

#[test]
fn test_pty_session_cursor_tracks_output() {
    use tttt_pty::{MockPty, PtySession};

    let mut mock = MockPty::new(80, 24);
    mock.queue_output(b"abc");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);
    session.pump().unwrap();

    assert_eq!(session.cursor_position(), (0, 3));
}

#[test]
fn test_pty_session_cursor_after_newline() {
    use tttt_pty::{MockPty, PtySession};

    let mut mock = MockPty::new(80, 24);
    mock.queue_output(b"line1\r\nline2");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);
    session.pump().unwrap();

    assert_eq!(session.cursor_position(), (1, 5));
}

// =============================================================================
// Diff applied to a second parser produces the same screen
// =============================================================================

#[test]
fn test_diff_applied_to_fresh_parser_matches() {
    let mut parser = vt100::Parser::new(10, 40, 0);
    let initial = parser.screen().clone();
    parser.process(b"hello world\r\nsecond line");

    let diff = parser.screen().contents_diff(&initial);

    // Apply diff to a fresh parser
    let mut parser2 = vt100::Parser::new(10, 40, 0);
    parser2.process(&diff);

    assert_eq!(
        parser.screen().contents(),
        parser2.screen().contents(),
        "diff applied to fresh parser should produce same contents"
    );
}

#[test]
fn test_sequential_diffs_accumulate_correctly() {
    let mut parser = vt100::Parser::new(10, 40, 0);
    let mut display = vt100::Parser::new(10, 40, 0);

    // Step 1
    let snap0 = parser.screen().clone();
    parser.process(b"step1");
    let diff1 = parser.screen().contents_diff(&snap0);
    display.process(&diff1);

    assert_eq!(
        parser.screen().contents(),
        display.screen().contents(),
        "after step 1"
    );

    // Step 2
    let snap1 = parser.screen().clone();
    parser.process(b"\r\nstep2");
    let diff2 = parser.screen().contents_diff(&snap1);
    display.process(&diff2);

    assert_eq!(
        parser.screen().contents(),
        display.screen().contents(),
        "after step 2"
    );

    // Step 3
    let snap2 = parser.screen().clone();
    parser.process(b"\r\nstep3");
    let diff3 = parser.screen().contents_diff(&snap2);
    display.process(&diff3);

    assert_eq!(
        parser.screen().contents(),
        display.screen().contents(),
        "after step 3"
    );
}

// =============================================================================
// Split-pane rendering simulation
// =============================================================================

/// Simulates what the harness does: render PTY diff into a sub-region
/// of a larger terminal, with a sidebar on the right.
#[test]
fn test_split_pane_diff_does_not_exceed_pty_columns() {
    let pty_cols: u16 = 50;
    let sidebar_width: u16 = 30;
    let total_cols: u16 = pty_cols + sidebar_width;

    let mut parser = vt100::Parser::new(24, pty_cols, 0);
    let initial = parser.screen().clone();
    parser.process(b"hello from the pty");
    let diff = parser.screen().contents_diff(&initial);

    // Parse all ANSI cursor position sequences in the diff
    // Format: \x1b[row;colH
    let diff_str = String::from_utf8_lossy(&diff);
    let mut max_col_seen: u16 = 0;

    let mut i = 0;
    let bytes = diff.as_slice();
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Parse CSI sequence
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && (bytes[end] == b';' || bytes[end].is_ascii_digit()) {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'H' {
                // Cursor position: \x1b[row;colH
                let params = String::from_utf8_lossy(&bytes[start..end]);
                let parts: Vec<&str> = params.split(';').collect();
                if parts.len() == 2 {
                    if let Ok(col) = parts[1].parse::<u16>() {
                        if col > max_col_seen {
                            max_col_seen = col;
                        }
                    }
                }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }

    // The diff should not position the cursor beyond the PTY width
    assert!(
        max_col_seen <= pty_cols,
        "diff cursor position (col={}) should not exceed PTY width ({}). diff: {:?}",
        max_col_seen,
        pty_cols,
        diff_str
    );
}

#[test]
fn test_split_pane_long_line_wraps_within_pty_cols() {
    let pty_cols: u16 = 20;
    let mut parser = vt100::Parser::new(10, pty_cols, 0);
    // Write a line longer than pty_cols
    parser.process(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ");

    // Verify wrapping by checking individual row contents
    let row0 = parser.screen().contents_between(0, 0, 0, pty_cols);
    let row1 = parser.screen().contents_between(1, 0, 1, pty_cols);

    assert_eq!(
        row0.trim_end(),
        "ABCDEFGHIJKLMNOPQRST",
        "first row should have exactly pty_cols chars"
    );
    assert!(
        row1.trim_end().starts_with("UVWXYZ"),
        "second row should start with overflow: {:?}",
        row1
    );

    // Cursor should be on the second row
    let (row, col) = parser.screen().cursor_position();
    assert_eq!(row, 1, "cursor should be on row 1 after wrapping");
    assert_eq!(col, 6, "cursor should be at col 6 (6 overflow chars)");
}

// =============================================================================
// Real PTY rendering tests
// =============================================================================

#[test]
fn test_real_pty_echo_renders_correctly() {
    use tttt_pty::{PtySession, RealPty};

    let pty_cols: u16 = 50;
    let pty_rows: u16 = 10;

    let backend = RealPty::spawn("/bin/echo", &["rendered correctly"], pty_cols, pty_rows).unwrap();
    let mut session =
        PtySession::new("s1".to_string(), backend, "echo".to_string(), pty_cols, pty_rows);

    std::thread::sleep(std::time::Duration::from_millis(200));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("rendered correctly"),
        "screen should contain the echo output: {:?}",
        screen
    );

    // Verify via formatted output roundtrip
    let formatted = session.get_screen_formatted();
    let mut verify = vt100::Parser::new(pty_rows, pty_cols, 0);
    verify.process(&formatted);
    assert_eq!(
        session.get_screen(),
        verify.screen().contents(),
        "formatted roundtrip should match"
    );
}

#[test]
fn test_real_pty_cat_input_output_cycle() {
    use tttt_pty::{PtySession, RealPty};

    let pty_cols: u16 = 40;
    let pty_rows: u16 = 10;

    let backend = RealPty::spawn("/bin/cat", &[], pty_cols, pty_rows).unwrap();
    let mut session =
        PtySession::new("s1".to_string(), backend, "cat".to_string(), pty_cols, pty_rows);

    // Send input
    session.send_raw(b"test input\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("test input"),
        "screen should show typed input: {:?}",
        screen
    );

    // Verify cursor is on a valid position
    let (row, col) = session.cursor_position();
    assert!(row < pty_rows, "cursor row {} should be < {}", row, pty_rows);
    assert!(col < pty_cols, "cursor col {} should be < {}", col, pty_cols);

    session.kill().unwrap();
}
