//! Tests verifying fixes for issues found in the test drive report (docs/tests/test-drive-report.md).
//!
//! Issue 1: PTY buffer overflow on large writes (EAGAIN / os error 35)
//! Issue 2: Injections don't auto-submit (missing \r)
//! Issue 3: Simultaneous notifications garble user input (no pacing)
//!
//! Issue 2 is tested in handler.rs unit tests (test_self_inject_appends_cr,
//! test_self_inject_does_not_double_cr) using MockPty for byte-level verification.
//! Issue 3's pacing logic is in App::run() and tested via the notification registry.

use tttt_pty::{PtySession, RealPty};

// === Issue 1: Large PTY writes ===
// EAGAIN is a race condition that depends on kernel buffer state and can't be
// reliably triggered in isolation. These tests verify large writes succeed with
// the chunked write implementation and serve as regression tests.

#[test]
fn test_real_pty_large_write_1100_chars() {
    // This is the exact scenario from test 5.1: ~1100 chars caused EAGAIN.
    let backend = RealPty::spawn("/bin/cat", &[], 80, 24).unwrap();
    let mut session = PtySession::new("large-1".to_string(), backend, "cat".to_string(), 80, 24);

    let large_msg = "A".repeat(1100);
    session.send_keys(&large_msg).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(500));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("AAAA"),
        "screen should contain echoed input, got: {:?}",
        screen.chars().take(200).collect::<String>()
    );

    session.kill().unwrap();
}

#[test]
fn test_real_pty_large_write_4096_chars() {
    let backend = RealPty::spawn("/bin/cat", &[], 80, 24).unwrap();
    let mut session = PtySession::new("large-2".to_string(), backend, "cat".to_string(), 80, 24);

    let large_msg = "B".repeat(4096);
    session.send_keys(&large_msg).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(500));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("BBBB"),
        "screen should contain echoed input after 4KB write"
    );

    session.kill().unwrap();
}

#[test]
fn test_real_pty_large_write_raw_bytes() {
    let backend = RealPty::spawn("/bin/cat", &[], 80, 24).unwrap();
    let mut session = PtySession::new("large-3".to_string(), backend, "cat".to_string(), 80, 24);

    let large_data = vec![b'C'; 2000];
    session.send_raw(&large_data).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(500));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("CCCC"),
        "screen should contain echoed raw bytes after 2KB write"
    );

    session.kill().unwrap();
}

// === Issue 3: Notification pacing ===
// The pacing logic lives in App::run() which is hard to test in isolation.
// We test the underlying NotificationRegistry behavior that feeds the queue.

#[test]
fn test_multiple_watchers_produce_multiple_injections() {
    // Verify that when multiple watchers fire simultaneously, they all produce
    // separate injections (which the pacing queue will then drain one at a time).
    use tttt_mcp::notification::NotificationRegistry;

    let mut reg = NotificationRegistry::new();
    reg.add_watcher("pty-1".into(), "done", "notify-A".into(), "root".into(), true)
        .unwrap();
    reg.add_watcher("pty-1".into(), "done", "notify-B".into(), "root".into(), true)
        .unwrap();
    reg.add_watcher("pty-1".into(), "done", "notify-C".into(), "root".into(), true)
        .unwrap();

    // First check: snapshot
    let _ = reg.check_session("pty-1", "working...");
    // Second check: pattern appears — all three should fire
    let injections = reg.check_session("pty-1", "working... done!");

    assert_eq!(
        injections.len(),
        3,
        "all three watchers should fire, producing 3 separate injections for the queue"
    );
    let texts: Vec<&str> = injections.iter().map(|i| i.text.as_str()).collect();
    assert!(texts.contains(&"notify-A"));
    assert!(texts.contains(&"notify-B"));
    assert!(texts.contains(&"notify-C"));
}
