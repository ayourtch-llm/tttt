//! Tests for working directory handling in PTY sessions.

use tttt_pty::{PtySession, RealPty};

#[test]
fn test_pty_inherits_cwd_by_default() {
    let expected_cwd = std::env::current_dir().unwrap();

    let backend = RealPty::spawn_with_cwd(
        "/bin/pwd",
        &[],
        None, // no explicit cwd = inherit
        80,
        24,
    )
    .unwrap();
    let mut session =
        PtySession::new("s1".to_string(), backend, "pwd".to_string(), 80, 24);

    std::thread::sleep(std::time::Duration::from_millis(300));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains(&expected_cwd.to_string_lossy().to_string()),
        "PTY should start in current dir ({}), got: {:?}",
        expected_cwd.display(),
        screen
    );
}

#[test]
fn test_pty_explicit_cwd() {
    let backend = RealPty::spawn_with_cwd(
        "/bin/pwd",
        &[],
        Some(std::path::Path::new("/tmp")),
        80,
        24,
    )
    .unwrap();
    let mut session =
        PtySession::new("s1".to_string(), backend, "pwd".to_string(), 80, 24);

    std::thread::sleep(std::time::Duration::from_millis(300));
    session.pump().unwrap();

    let screen = session.get_screen();
    // On macOS /tmp is a symlink to /private/tmp
    assert!(
        screen.contains("/tmp") || screen.contains("/private/tmp"),
        "PTY should start in /tmp, got: {:?}",
        screen
    );
}

#[test]
fn test_pty_cwd_override_via_spawn() {
    // Test that the old spawn() still works (backwards compat)
    let backend = RealPty::spawn("/bin/pwd", &[], 80, 24).unwrap();
    let mut session =
        PtySession::new("s1".to_string(), backend, "pwd".to_string(), 80, 24);

    std::thread::sleep(std::time::Duration::from_millis(300));
    session.pump().unwrap();

    // Should output some directory (the current one)
    let screen = session.get_screen();
    assert!(
        screen.contains("/"),
        "pwd should output a path: {:?}",
        screen
    );
}

#[test]
fn test_mcp_pty_launch_with_working_dir() {
    use std::io::{BufReader, Cursor};
    use tttt_mcp::{McpServer, PtyToolHandler};
    use tttt_pty::SessionManager;

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"pty_launch","arguments":{"command":"/bin/pwd","working_dir":"/tmp","cols":80,"rows":10}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"pty_get_screen","arguments":{"session_id":"pty-1"}}}"#,
        "\n",
    );

    let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let writer = Vec::new();
    let manager: SessionManager<RealPty> = SessionManager::new();
    let handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
    let mut server = McpServer::new(reader, writer, handler);
    server.run().unwrap();

    let output = String::from_utf8_lossy(server.writer());
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), 2);

    // Second response should have screen with /tmp
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let result: serde_json::Value = serde_json::from_str(text).unwrap();
    let screen = result["screen"].as_str().unwrap();
    assert!(
        screen.contains("/tmp") || screen.contains("/private/tmp"),
        "screen should show /tmp: {:?}",
        screen
    );
}
