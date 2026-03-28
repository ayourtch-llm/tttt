//! Tests for cursor movement keys (arrows, backspace) in real PTY sessions.
//!
//! These verify that cursor position changes correctly when the user presses
//! arrow keys and edits text.

use tttt_pty::{PtySession, RealPty};

fn wait_and_pump(session: &mut PtySession<RealPty>, ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
    let _ = session.pump();
}

/// Type "cisco123", then left arrow 3 times, then type "0".
/// Expected result: "cisco0123" with cursor after the "0".
#[test]
fn test_left_arrow_and_insert() {
    let pty_cols: u16 = 80;
    let pty_rows: u16 = 24;

    let backend =
        RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    // Wait for prompt
    for _ in 0..20 {
        wait_and_pump(&mut session, 100);
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    // Type "cisco123"
    session.send_raw(b"cisco123").unwrap();
    wait_and_pump(&mut session, 100);

    let screen_after_type = session.get_screen();
    assert!(
        screen_after_type.contains("cisco123"),
        "should see 'cisco123' after typing: {:?}",
        screen_after_type
    );
    let (_, col_after_type) = session.cursor_position();

    // Press left arrow 3 times
    session.send_raw(b"\x1b[D").unwrap(); // left
    wait_and_pump(&mut session, 50);
    let (_, col_after_left1) = session.cursor_position();
    assert_eq!(
        col_after_left1,
        col_after_type - 1,
        "cursor should move left by 1 after first left arrow"
    );

    session.send_raw(b"\x1b[D").unwrap(); // left
    wait_and_pump(&mut session, 50);
    let (_, col_after_left2) = session.cursor_position();
    assert_eq!(
        col_after_left2,
        col_after_type - 2,
        "cursor should move left by 2 after second left arrow"
    );

    session.send_raw(b"\x1b[D").unwrap(); // left
    wait_and_pump(&mut session, 50);
    let (_, col_after_left3) = session.cursor_position();
    assert_eq!(
        col_after_left3,
        col_after_type - 3,
        "cursor should move left by 3 after third left arrow"
    );

    // Type "0" — should insert at cursor position
    session.send_raw(b"0").unwrap();
    wait_and_pump(&mut session, 100);

    let screen_after_insert = session.get_screen();
    assert!(
        screen_after_insert.contains("cisco0123"),
        "should see 'cisco0123' after inserting '0': {:?}",
        screen_after_insert
    );

    // Cursor should be one position to the right of where we inserted
    let (_, col_after_insert) = session.cursor_position();
    assert_eq!(
        col_after_insert,
        col_after_left3 + 1,
        "cursor should advance by 1 after inserting character"
    );

    session.kill().unwrap();
}

/// Type "hello", then right arrow (should do nothing since cursor is at end).
#[test]
fn test_right_arrow_at_end() {
    let pty_cols: u16 = 80;
    let pty_rows: u16 = 24;

    let backend =
        RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    for _ in 0..20 {
        wait_and_pump(&mut session, 100);
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    session.send_raw(b"hello").unwrap();
    wait_and_pump(&mut session, 100);
    let (_, col_before) = session.cursor_position();

    // Right arrow at end should not move cursor
    session.send_raw(b"\x1b[C").unwrap(); // right
    wait_and_pump(&mut session, 100);
    let (_, col_after) = session.cursor_position();
    assert_eq!(
        col_after, col_before,
        "right arrow at end should not move cursor"
    );

    session.kill().unwrap();
}

/// Type "hello", left arrow to middle, right arrow back.
#[test]
fn test_left_then_right_arrow() {
    let pty_cols: u16 = 80;
    let pty_rows: u16 = 24;

    let backend =
        RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    for _ in 0..20 {
        wait_and_pump(&mut session, 100);
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    session.send_raw(b"abcde").unwrap();
    wait_and_pump(&mut session, 100);
    let (_, col_end) = session.cursor_position();

    // Left 2
    session.send_raw(b"\x1b[D\x1b[D").unwrap();
    wait_and_pump(&mut session, 100);
    let (_, col_after_left) = session.cursor_position();
    assert_eq!(col_after_left, col_end - 2, "should be 2 left of end");

    // Right 1
    session.send_raw(b"\x1b[C").unwrap();
    wait_and_pump(&mut session, 100);
    let (_, col_after_right) = session.cursor_position();
    assert_eq!(col_after_right, col_end - 1, "should be 1 left of end");

    session.kill().unwrap();
}

/// Type "hello", then backspace 3 times. Should show "he".
#[test]
fn test_backspace_deletes_chars() {
    let pty_cols: u16 = 80;
    let pty_rows: u16 = 24;

    let backend =
        RealPty::spawn("/bin/bash", &["--norc", "--noprofile"], pty_cols, pty_rows).unwrap();
    let mut session = PtySession::new(
        "s1".to_string(),
        backend,
        "bash".to_string(),
        pty_cols,
        pty_rows,
    );

    for _ in 0..20 {
        wait_and_pump(&mut session, 100);
        if session.get_screen().contains("$") || session.get_screen().contains("#") {
            break;
        }
    }

    session.send_raw(b"hello").unwrap();
    wait_and_pump(&mut session, 100);
    assert!(session.get_screen().contains("hello"));

    // Backspace 3 times
    session.send_raw(b"\x7f\x7f\x7f").unwrap();
    wait_and_pump(&mut session, 200);

    let screen = session.get_screen();
    // Should have "he" remaining on the prompt line, not "hello"
    // Find the prompt line
    let prompt_line = screen
        .lines()
        .find(|l| l.contains("$") || l.contains("#"))
        .unwrap_or("");

    assert!(
        prompt_line.contains("he"),
        "prompt line should contain 'he': {:?}",
        prompt_line
    );
    // "hello" should no longer be fully visible on the prompt line
    assert!(
        !prompt_line.ends_with("hello"),
        "prompt line should not end with 'hello' after backspace: {:?}",
        prompt_line
    );

    session.kill().unwrap();
}

