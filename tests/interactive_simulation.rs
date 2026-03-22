//! Tests simulating interactive terminal usage.
//!
//! These reproduce the bugs found in manual testing:
//! 1. Typing "ls" + Enter: echoed "ls" not visible, but output visible
//! 2. Hitting Enter multiple times: cursor moves but screen doesn't update

use tttt_pty::{MockPty, PtySession, RealPty};

/// Simulate a bash-like session: send input, get echo + output.
/// The PTY handles echo internally — when we type "ls\n" into a bash PTY,
/// bash echoes "ls\r\n" back, then runs the command and outputs results.
#[test]
fn test_bash_echo_simulation_with_mock() {
    let mut mock = MockPty::new(80, 24);
    // Simulate what bash does: echo the typed command + prompt
    mock.queue_output(b"$ ");  // initial prompt
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);

    // Consume initial prompt
    let _ = session.screen_diff();
    session.pump().unwrap();
    let diff1 = session.screen_diff();
    assert!(!diff1.is_empty(), "initial prompt should produce a diff");

    let screen = session.get_screen();
    assert!(screen.contains("$ "), "should see prompt: {:?}", screen);
    assert_eq!(session.cursor_position(), (0, 2), "cursor after '$ '");
}

#[test]
fn test_typing_and_echo_with_mock() {
    let mut mock = MockPty::new(80, 24);
    // Initial prompt
    mock.queue_output(b"$ ");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);
    session.pump().unwrap();
    let _ = session.screen_diff(); // consume initial

    // User types "ls" — we send raw bytes (no special key processing)
    session.send_raw(b"l").unwrap();
    session.send_raw(b"s").unwrap();

    // Bash echoes "ls" back through the PTY
    // We can't queue_output on the moved mock, so let's test with a fresh session
}

#[test]
fn test_full_echo_cycle_with_mock() {
    // Simulate the complete cycle: prompt -> type -> echo -> output
    let mut mock = MockPty::new(80, 24);
    // Queue everything the PTY would produce:
    // 1. Initial prompt
    // 2. Echo of typed characters (bash echoes each char as you type)
    // 3. Command output
    // 4. New prompt
    mock.queue_output(b"$ ls\r\nfile1  file2  file3\r\n$ ");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);

    let _ = session.screen_diff(); // baseline
    session.pump().unwrap();
    let diff = session.screen_diff();

    // Verify the diff contains all the expected content
    let diff_str = String::from_utf8_lossy(&diff);
    assert!(diff_str.contains("$ ls"), "diff should show '$ ls': {:?}", diff_str);
    assert!(diff_str.contains("file1"), "diff should show output: {:?}", diff_str);

    // Verify the screen state
    let screen = session.get_screen();
    assert!(screen.contains("$ ls"), "screen should show '$ ls': {:?}", screen);
    assert!(screen.contains("file1  file2  file3"), "screen: {:?}", screen);

    // Verify cursor is at the new prompt
    assert_eq!(session.cursor_position(), (2, 2), "cursor should be at new prompt");
}

#[test]
fn test_multiple_enters_with_mock() {
    // Simulate pressing Enter multiple times at bash prompt
    let mut mock = MockPty::new(80, 24);
    // Bash produces: prompt, then on each enter: newline + prompt
    mock.queue_output(b"$ \r\n$ \r\n$ \r\n$ ");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);

    let _ = session.screen_diff(); // baseline
    session.pump().unwrap();
    let diff = session.screen_diff();

    assert!(!diff.is_empty(), "diff should have content");

    // Verify cursor has moved down
    let (row, col) = session.cursor_position();
    assert_eq!(row, 3, "cursor should be on row 3 after 3 enters");
    assert_eq!(col, 2, "cursor should be at col 2 (after '$ ')");

    // Verify screen shows multiple prompts
    let screen = session.get_screen();
    let prompt_count = screen.matches("$ ").count();
    assert!(
        prompt_count >= 3,
        "should see at least 3 prompts, got {}: {:?}",
        prompt_count,
        screen
    );
}

/// Test PaneRenderer-based rendering: when we render cells from the PTY screen
/// onto a wider display, does the display match?
#[test]
fn test_pane_renderer_on_wider_display() {
    use tttt_tui::PaneRenderer;

    let pty_cols: u16 = 50;
    let pty_rows: u16 = 10;
    let display_cols: u16 = 80;

    let mut mock = MockPty::new(pty_cols, pty_rows);
    mock.queue_output(b"$ ls\r\nfile1  file2\r\n$ ");
    let mut session = PtySession::new("s1".to_string(), mock, "bash".to_string(), pty_cols, pty_rows);
    session.pump().unwrap();

    // Render using PaneRenderer
    let mut renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
    let output = renderer.render(session.screen().screen());

    // Parse on wider display
    let mut display = vt100::Parser::new(pty_rows, display_cols, 0);
    display.process(&output);

    let display_contents = display.screen().contents();
    assert!(
        display_contents.contains("$ ls"),
        "display should show '$ ls': {:?}",
        display_contents
    );
    assert!(
        display_contents.contains("file1  file2"),
        "display should show output: {:?}",
        display_contents
    );
}

/// Test sequential PaneRenderer updates on a wider display.
#[test]
fn test_pane_renderer_sequential_on_wider_display() {
    use tttt_tui::PaneRenderer;

    let pty_cols: u16 = 50;
    let pty_rows: u16 = 10;
    let display_cols: u16 = 80;

    let mut pty_parser = vt100::Parser::new(pty_rows, pty_cols, 0);
    let mut renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
    let mut display = vt100::Parser::new(pty_rows, display_cols, 0);

    // Step 1: prompt
    pty_parser.process(b"$ ");
    let output1 = renderer.render(pty_parser.screen());
    display.process(&output1);
    assert!(
        display.screen().contents().contains("$ "),
        "step 1: {:?}",
        display.screen().contents()
    );

    // Step 2: command + output
    pty_parser.process(b"ls\r\nfile1\r\n$ ");
    let output2 = renderer.render(pty_parser.screen());
    display.process(&output2);

    let contents = display.screen().contents();
    assert!(
        contents.contains("$ ls"),
        "step 2 '$ ls': {:?}",
        contents
    );
    assert!(
        contents.contains("file1"),
        "step 2 'file1': {:?}",
        contents
    );
}

// =============================================================================
// Real PTY interactive tests
// =============================================================================

#[test]
fn test_real_bash_type_ls_enter() {
    let pty_cols: u16 = 80;
    let pty_rows: u16 = 24;

    let backend = RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    // Wait for initial prompt
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        session.pump().unwrap();
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    let screen_before = session.get_screen();
    assert!(
        screen_before.contains("$") || screen_before.contains("#") || screen_before.contains("bash"),
        "should see some kind of prompt: {:?}",
        screen_before
    );

    // Type "ls" character by character
    session.send_raw(b"l").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    session.pump().unwrap();

    session.send_raw(b"s").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    session.pump().unwrap();

    // Screen should now show "ls" echoed
    let screen_after_type = session.get_screen();
    assert!(
        screen_after_type.contains("ls"),
        "screen should show echoed 'ls' after typing: {:?}",
        screen_after_type
    );

    // Press Enter
    session.send_raw(b"\r").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    session.pump().unwrap();

    // Should see ls output (directory listing) or a new prompt
    let screen_after_enter = session.get_screen();
    // The "ls" command may have scrolled off, but we should see either
    // the directory listing or a new prompt
    assert!(
        screen_after_enter.contains("$") || screen_after_enter.contains("#"),
        "screen should show a prompt after ls completes: {:?}",
        screen_after_enter
    );
    // Screen should have more content than before (ls output)
    assert!(
        screen_after_enter.trim().len() > screen_after_type.trim().len(),
        "screen should have more content after ls output"
    );

    session.kill().unwrap();
}

#[test]
fn test_real_bash_multiple_enters() {
    let pty_cols: u16 = 80;
    let pty_rows: u16 = 24;

    let backend = RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    // Wait for initial prompt
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        session.pump().unwrap();
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    let (initial_row, _) = session.cursor_position();

    // Press Enter 3 times
    for i in 0..3 {
        session.send_raw(b"\r").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        session.pump().unwrap();
    }

    let (final_row, _) = session.cursor_position();
    assert!(
        final_row > initial_row,
        "cursor should have moved down: initial_row={}, final_row={}",
        initial_row,
        final_row
    );

    // Screen should show multiple prompts
    let screen = session.get_screen();
    let lines_with_prompt: Vec<&str> = screen
        .lines()
        .filter(|l| l.contains("$") || l.contains("#"))
        .collect();
    assert!(
        lines_with_prompt.len() >= 3,
        "should see at least 3 prompt lines, got {}: {:?}",
        lines_with_prompt.len(),
        screen
    );

    session.kill().unwrap();
}

/// Test that PaneRenderer works with a real bash session.
/// This is the key test: does our rendering approach correctly show
/// what bash produces?
#[test]
fn test_real_bash_pane_renderer_pipeline() {
    use tttt_tui::PaneRenderer;

    let pty_cols: u16 = 60;
    let pty_rows: u16 = 10;
    let display_cols: u16 = 90; // wider, like with sidebar

    let backend = RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    let mut renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
    let mut display = vt100::Parser::new(pty_rows, display_cols, 0);

    // Wait for prompt
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        session.pump().unwrap();
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    // Render prompt
    let output = renderer.render(session.screen().screen());
    display.process(&output);

    // Compare line-by-line, trimming trailing whitespace per line and trailing empty lines
    fn lines_trimmed(s: &str) -> String {
        let lines: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
        let end = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
        lines[..end].join("\n")
    }

    // Compare cell-by-cell: every PTY cell should appear at the same position on display
    fn cells_match(pty_screen: &vt100::Screen, display_screen: &vt100::Screen, rows: u16, cols: u16) {
        for row in 0..rows {
            for col in 0..cols {
                let pty_cell = pty_screen.cell(row, col);
                let disp_cell = display_screen.cell(row, col);
                let pty_contents = pty_cell.map(|c| c.contents()).unwrap_or_default();
                let disp_contents = disp_cell.map(|c| c.contents()).unwrap_or_default();
                // Treat empty and space as equivalent
                let pty_c = if pty_contents.is_empty() { " " } else { &pty_contents };
                let disp_c = if disp_contents.is_empty() { " " } else { &disp_contents };
                assert_eq!(
                    pty_c, disp_c,
                    "cell mismatch at ({}, {}): pty={:?}, display={:?}",
                    row, col, pty_c, disp_c
                );
            }
        }
    }

    cells_match(session.screen().screen(), display.screen(), pty_rows, pty_cols);

    // Type "echo hi" and enter
    session.send_raw(b"echo hi\r").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    session.pump().unwrap();

    let output2 = renderer.render(session.screen().screen());
    display.process(&output2);

    let pty_screen_text = session.get_screen();
    assert!(
        pty_screen_text.contains("echo hi"),
        "PTY should contain 'echo hi': {:?}",
        pty_screen_text
    );

    cells_match(session.screen().screen(), display.screen(), pty_rows, pty_cols);

    session.kill().unwrap();
}
