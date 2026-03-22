//! Tests for MCP server integration with the TUI session manager.
//!
//! The key architecture: the MCP server shares a SessionManager with the TUI,
//! so panes created via MCP tools are visible in the sidebar and can be
//! switched to by the user.

use std::io::{BufReader, Cursor};
use std::sync::{Arc, Mutex};
use tttt_mcp::{McpServer, PtyToolHandler, ToolHandler};
use tttt_pty::{MockPty, PtySession, SessionManager};

/// Test that sessions created via MCP are visible in the shared manager.
#[test]
fn test_mcp_creates_session_in_shared_manager() {
    let manager: SessionManager<MockPty> = SessionManager::new();
    let mut handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));

    // Launch a session via MCP tool call
    let result = handler
        .handle_tool_call(
            "tttt_pty_launch",
            &serde_json::json!({"cols": 80, "rows": 24}),
        )
        .unwrap();

    let session_id = result["session_id"].as_str().unwrap();
    assert_eq!(session_id, "pty-1");

    // The session should exist in the manager
    assert_eq!(handler.manager().lock().unwrap().session_count(), 1);
    assert!(handler.manager().lock().unwrap().exists("pty-1"));
}

/// Test that multiple sessions can be created and listed.
#[test]
fn test_mcp_multiple_sessions() {
    let manager: SessionManager<MockPty> = SessionManager::new();
    let mut handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));

    handler
        .handle_tool_call("tttt_pty_launch", &serde_json::json!({}))
        .unwrap();
    handler
        .handle_tool_call("tttt_pty_launch", &serde_json::json!({}))
        .unwrap();
    handler
        .handle_tool_call("tttt_pty_launch", &serde_json::json!({}))
        .unwrap();

    let list = handler
        .handle_tool_call("tttt_pty_list", &serde_json::json!({}))
        .unwrap();
    let sessions = list.as_array().unwrap();
    assert_eq!(sessions.len(), 3);
}

/// Test that killing a session via MCP removes it from the manager.
#[test]
fn test_mcp_kill_removes_session() {
    let manager: SessionManager<MockPty> = SessionManager::new();
    let mut handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));

    handler
        .handle_tool_call("tttt_pty_launch", &serde_json::json!({}))
        .unwrap();
    assert_eq!(handler.manager().lock().unwrap().session_count(), 1);

    handler
        .handle_tool_call(
            "tttt_pty_kill",
            &serde_json::json!({"session_id": "pty-1"}),
        )
        .unwrap();
    assert_eq!(handler.manager().lock().unwrap().session_count(), 0);
}

/// Test the complete MCP workflow: launch, send keys, get screen.
#[test]
fn test_mcp_workflow_launch_type_read() {
    let mut mock = MockPty::new(80, 24);
    // Pre-queue what bash would echo back
    mock.queue_output(b"$ echo hello\r\nhello\r\n$ ");
    let session = PtySession::new("pty-1".to_string(), mock, "bash".to_string(), 80, 24);

    let mut manager: SessionManager<MockPty> = SessionManager::new();
    manager.add_session(session).unwrap();

    let mut handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));

    // Pump and get screen
    let screen_result = handler
        .handle_tool_call(
            "tttt_pty_get_screen",
            &serde_json::json!({"session_id": "pty-1"}),
        )
        .unwrap();

    let screen = screen_result["screen"].as_str().unwrap();
    assert!(screen.contains("echo hello"), "screen: {:?}", screen);
    assert!(screen.contains("hello"), "screen: {:?}", screen);
}

/// Test that the MCP server properly handles a full JSON-RPC session
/// creating and managing PTY sessions.
#[test]
fn test_mcp_server_full_session_with_mock() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"initialized"}"#, "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{}}}"#, "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{}}}"#, "\n",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#, "\n",
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"tttt_pty_kill","arguments":{"session_id":"pty-1"}}}"#, "\n",
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#, "\n",
    );

    let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let writer = Vec::new();
    let manager: SessionManager<MockPty> = SessionManager::new();
    let handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
    let mut server = McpServer::new(reader, writer, handler);
    server.run().unwrap();

    let output = String::from_utf8_lossy(server.writer());
    let responses: Vec<serde_json::Value> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // Should have 6 responses (initialized notification has no response)
    assert_eq!(responses.len(), 6, "expected 6 responses, got {}", responses.len());

    // Response 1: initialize
    assert!(responses[0]["result"]["serverInfo"]["name"].as_str() == Some("tttt"));

    // Response 2: launch pty-1
    let text = responses[1]["result"]["content"][0]["text"].as_str().unwrap();
    let r: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(r["session_id"], "pty-1");

    // Response 3: launch pty-2
    let text = responses[2]["result"]["content"][0]["text"].as_str().unwrap();
    let r: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(r["session_id"], "pty-2");

    // Response 4: list should show 2 sessions
    let text = responses[3]["result"]["content"][0]["text"].as_str().unwrap();
    let r: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(r.as_array().unwrap().len(), 2);

    // Response 5: kill pty-1
    let text = responses[4]["result"]["content"][0]["text"].as_str().unwrap();
    let r: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(r["status"], "ok");

    // Response 6: list should show 1 session (pty-2)
    let text = responses[5]["result"]["content"][0]["text"].as_str().unwrap();
    let r: serde_json::Value = serde_json::from_str(text).unwrap();
    let sessions = r.as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "pty-2");
}

/// Test sandbox profile parameter is accepted by pty_launch.
#[test]
fn test_mcp_launch_with_sandbox_profile() {
    let manager: SessionManager<MockPty> = SessionManager::new();
    let mut handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));

    // Launch with sandbox profile (currently just a parameter, not enforced)
    let result = handler
        .handle_tool_call(
            "tttt_pty_launch",
            &serde_json::json!({
                "sandbox_profile": "read_only_worktree"
            }),
        )
        .unwrap();

    assert!(result["session_id"].is_string());
}

/// Test that the MCP server exposes scheduler tools.
#[test]
fn test_mcp_scheduler_tools_listed() {
    use tttt_mcp::pty_tool_definitions;

    let tools = pty_tool_definitions();
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // PTY tools
    assert!(tool_names.contains(&"tttt_pty_launch"));
    assert!(tool_names.contains(&"tttt_pty_send_keys"));
    assert!(tool_names.contains(&"tttt_pty_get_screen"));
    assert!(tool_names.contains(&"tttt_pty_list"));
    assert!(tool_names.contains(&"tttt_pty_kill"));
    assert!(tool_names.contains(&"tttt_pty_get_cursor"));
    assert!(tool_names.contains(&"tttt_pty_resize"));
    assert!(tool_names.contains(&"tttt_pty_set_scrollback"));
}
