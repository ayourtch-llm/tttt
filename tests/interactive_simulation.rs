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

