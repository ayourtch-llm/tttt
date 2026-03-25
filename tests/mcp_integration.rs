//! Integration tests for the MCP server with real PTY sessions.

use std::io::{BufReader, Cursor};
use tttt_mcp::{McpServer, PtyToolHandler};
use tttt_pty::{RealPty, SessionManager};

fn make_real_server(
    input: &str,
) -> McpServer<BufReader<Cursor<Vec<u8>>>, Vec<u8>, PtyToolHandler<RealPty>> {
    let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let writer = Vec::new();
    let manager: SessionManager<RealPty> = SessionManager::new();
    let handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
    McpServer::new(reader, writer, handler)
}

fn parse_responses(output: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(output)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn extract_tool_result(response: &serde_json::Value) -> serde_json::Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn test_mcp_initialize_handshake() {
    let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let mut server = make_real_server(&format!("{}\n", input));
    server.run().unwrap();
    let mut server = make_real_server("");
    let resp = server.process_line(input).unwrap();
    assert_eq!(resp.result.as_ref().unwrap()["protocolVersion"], "2024-11-05");
    assert_eq!(resp.result.as_ref().unwrap()["serverInfo"]["name"], "tttt");
}

#[test]
fn test_mcp_launch_real_pty() {
    let mut server = make_real_server("");
    let resp = server
        .process_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{"command":"/bin/echo","args":["hello"],"cols":40,"rows":10}}}"#,
        )
        .unwrap();

    let result = extract_tool_result(&serde_json::to_value(&resp).unwrap());
    assert_eq!(result["session_id"], "pty-1");
    assert_eq!(server.handler().manager().lock().unwrap().session_count(), 1);
}

#[test]
fn test_mcp_launch_and_get_screen() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{"command":"/bin/echo","args":["hello world"],"cols":40,"rows":10}}}"#,
        "\n",
        // Small delay via a second request to let the echo complete
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_get_screen","arguments":{"session_id":"pty-1"}}}"#,
        "\n",
    );
    let mut server = make_real_server(input);
    server.run().unwrap();

    let responses = parse_responses(server.writer());
    assert_eq!(responses.len(), 2);

    // First response: pty_launch
    let launch_result = extract_tool_result(&responses[0]);
    assert_eq!(launch_result["session_id"], "pty-1");

    // Second response: pty_get_screen — should contain "hello world"
    let screen_result = extract_tool_result(&responses[1]);
    let screen_text = screen_result["screen"].as_str().unwrap();
    assert!(
        screen_text.contains("hello world"),
        "screen should contain 'hello world', got: {:?}",
        screen_text
    );
}

#[test]
fn test_mcp_launch_and_list() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{"command":"/bin/echo","args":["test"]}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#,
        "\n",
    );
    let mut server = make_real_server(input);
    server.run().unwrap();

    let responses = parse_responses(server.writer());
    assert_eq!(responses.len(), 2);

    let list_result = extract_tool_result(&responses[1]);
    let sessions = list_result.as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "pty-1");
}

#[test]
fn test_mcp_launch_and_kill() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{"command":"/bin/sleep","args":["60"]}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_kill","arguments":{"session_id":"pty-1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#,
        "\n",
    );
    let mut server = make_real_server(input);
    server.run().unwrap();

    let responses = parse_responses(server.writer());
    assert_eq!(responses.len(), 3);

    // After kill, list should be empty
    let list_result = extract_tool_result(&responses[2]);
    let sessions = list_result.as_array().unwrap();
    assert_eq!(sessions.len(), 0);
}

#[test]
fn test_mcp_launch_and_resize() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{"command":"/bin/sleep","args":["60"],"cols":40,"rows":10}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_resize","arguments":{"session_id":"pty-1","cols":100,"rows":50}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#,
        "\n",
    );
    let mut server = make_real_server(input);
    server.run().unwrap();

    let responses = parse_responses(server.writer());
    let list_result = extract_tool_result(&responses[2]);
    let sessions = list_result.as_array().unwrap();
    assert_eq!(sessions[0]["cols"], 100);
    assert_eq!(sessions[0]["rows"], 50);
}

#[test]
fn test_mcp_launch_and_get_cursor() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{"command":"/bin/echo","args":["hi"],"cols":40,"rows":10}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_get_cursor","arguments":{"session_id":"pty-1"}}}"#,
        "\n",
    );
    let mut server = make_real_server(input);
    server.run().unwrap();

    let responses = parse_responses(server.writer());
    let cursor_result = extract_tool_result(&responses[1]);
    // Cursor should be a valid position
    assert!(cursor_result["row"].is_number());
    assert!(cursor_result["col"].is_number());
}

#[test]
fn test_mcp_full_run_loop() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#,
        "\n",
    );
    let mut server = make_real_server(input);
    server.run().unwrap();

    let responses = parse_responses(server.writer());
    // 3 responses: initialize, ping, tools/list (initialized is a notification - no response)
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[2]["id"], 3);

    // tools/list should have 12 tools
    let tools = responses[2]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 12);
}

#[test]
fn test_mcp_nonexistent_session() {
    let mut server = make_real_server("");
    let resp = server
        .process_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_get_screen","arguments":{"session_id":"nonexistent"}}}"#,
        )
        .unwrap();

    assert!(resp.error.is_some());
}
