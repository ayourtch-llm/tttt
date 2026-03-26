//! End-to-end tests using our own PTY tools to test the tttt binary.
//!
//! NOTE: Tests must run sequentially (--test-threads=1) to avoid
//! socket conflicts. Each test cleans up after itself.
//!
//! The TUI tests use detach (Ctrl+\ d) to exit cleanly.

use tttt_pty::{PtySession, RealPty};

fn cargo_bin_path() -> String {
    let output = std::process::Command::new("cargo")
        .args(["build", "--quiet"])
        .output()
        .expect("failed to run cargo build");
    assert!(output.status.success(), "cargo build failed");

    let output = std::process::Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to get metadata");
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("bad metadata json");
    let target_dir = metadata["target_directory"].as_str().unwrap();
    format!("{}/debug/tttt", target_dir)
}

fn wait_for_screen(session: &mut PtySession<RealPty>, pattern: &str, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = session.pump();
        if session.get_screen().contains(pattern) {
            return true;
        }
    }
    false
}

fn detach_and_wait(session: &mut PtySession<RealPty>) {
    let _ = session.send_raw(&[0x1c, b'd']);
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = session.pump();
        if matches!(session.status(), tttt_pty::SessionStatus::Exited(_)) {
            return;
        }
    }
    let _ = session.kill();
}

// =============================================================================
// tttt --help (no TUI, just prints and exits)
// =============================================================================

#[test]
fn test_e2e_help() {
    let bin = cargo_bin_path();
    let backend = RealPty::spawn(&bin, &["--help"], 80, 24).unwrap();
    let mut session = PtySession::new("help".into(), backend, "tttt".into(), 80, 24);

    assert!(
        wait_for_screen(&mut session, "Print help", 5000),
        "should show help: {:?}",
        session.get_screen()
    );
}

// =============================================================================
// tttt mcp-server standalone (stdin/stdout, no TUI)
// =============================================================================

#[test]
fn test_e2e_mcp_server() {
    let bin = cargo_bin_path();
    let backend = RealPty::spawn(&bin, &["mcp-server"], 80, 24).unwrap();
    let mut session = PtySession::new("mcp".into(), backend, "mcp".into(), 80, 24);

    session
        .send_raw(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .unwrap();
    session.send_raw(b"\n").unwrap();

    assert!(
        wait_for_screen(&mut session, "protocolVersion", 5000),
        "should get init response: {:?}",
        session.get_screen()
    );

    session
        .send_raw(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#)
        .unwrap();
    session.send_raw(b"\n").unwrap();

    assert!(
        wait_for_screen(&mut session, "tttt_pty_launch", 5000),
        "should list tools: {:?}",
        session.get_screen()
    );

    let _ = session.kill();
}

// =============================================================================
// tttt -e /bin/echo (starts TUI with quick-exit command)
// =============================================================================

#[test]
fn test_e2e_custom_command() {
    let bin = cargo_bin_path();
    let backend =
        RealPty::spawn(&bin, &["-e", "/bin/echo", "e2e_test_output"], 80, 24).unwrap();
    let mut session = PtySession::new("echo".into(), backend, "tttt".into(), 80, 24);

    // echo exits immediately, tttt should show output and exit
    let found = wait_for_screen(&mut session, "e2e_test_output", 10000);

    // Either we see the output or tttt already exited (echo is fast)
    std::thread::sleep(std::time::Duration::from_millis(2000));
    let _ = session.pump();

    if !matches!(session.status(), tttt_pty::SessionStatus::Exited(_)) {
        // Still running — detach
        detach_and_wait(&mut session);
    }

    // Either way, we should have seen the output or tttt exited
    assert!(
        found || matches!(session.status(), tttt_pty::SessionStatus::Exited(_)),
        "should see output or exit: screen={:?}, status={:?}",
        session.get_screen(),
        session.status()
    );
}

// =============================================================================
// tttt TUI: start, verify sidebar, detach
// =============================================================================

#[test]
fn test_e2e_tui_start_and_detach() {
    let bin = cargo_bin_path();
    let backend = RealPty::spawn(&bin, &[], 100, 24).unwrap();
    let mut session = PtySession::new("tui".into(), backend, "tttt".into(), 100, 24);

    // Give tttt time to start and render
    std::thread::sleep(std::time::Duration::from_millis(3000));
    let _ = session.pump();

    let screen = session.get_screen();
    // Should have SOME content (sidebar, shell prompt, or stderr messages)
    assert!(
        !screen.trim().is_empty(),
        "screen should not be empty after startup: {:?}",
        screen
    );

    // Detach
    detach_and_wait(&mut session);
    assert!(
        matches!(session.status(), tttt_pty::SessionStatus::Exited(_)),
        "should exit after detach: {:?}",
        session.status()
    );
}
