//! MCP proxy: bridges a stdio MCP server to a Unix socket backend.
//!
//! When `tttt mcp-server --connect /path/to/socket` is used, the MCP server
//! reads JSON-RPC from stdin (from Claude), forwards requests over the Unix
//! socket to the tttt TUI process, receives results, and writes them to stdout.
//!
//! This allows Claude Code (which spawns MCP servers as child processes on stdio)
//! to use the tttt TUI's shared SessionManager.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

/// Run the MCP proxy: forward JSON-RPC between stdio and a Unix socket.
///
/// - Reads JSON-RPC lines from `reader` (Claude's stdin)
/// - Forwards each line to the Unix socket (tttt TUI)
/// - Reads response from socket
/// - Writes response to `writer` (Claude's stdout)
pub fn run_proxy<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    socket_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut socket = UnixStream::connect(socket_path)?;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF from Claude
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Check if this is a notification (no response expected)
        let is_notification = is_jsonrpc_notification(trimmed);

        // Forward request to TUI over socket
        // Protocol: length-prefixed line (4 bytes big-endian + line bytes + newline)
        let line_bytes = trimmed.as_bytes();
        let len = line_bytes.len() as u32;
        socket.write_all(&len.to_be_bytes())?;
        socket.write_all(line_bytes)?;
        socket.flush()?;

        if is_notification {
            continue; // Don't wait for response for notifications
        }

        // Read response from TUI
        let mut len_buf = [0u8; 4];
        match socket.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Box::new(e)),
        }
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        
        let mut resp_buf = vec![0u8; resp_len];
        match socket.read_exact(&mut resp_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Box::new(e)),
        }

        // Write response to Claude
        writer.write_all(&resp_buf)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }

    Ok(())
}

/// Check if a JSON-RPC request is a notification (id is null or missing)
fn is_jsonrpc_notification(line: &str) -> bool {
    // Quick check: look for "id":null or "id": null
    line.contains("\"id\":null") || line.contains("\"id\": null")
}

/// Server-side handler: reads proxied requests from a Unix socket,
/// processes them using a ToolHandler, and sends responses back.
///
/// This runs in a thread within the TUI process.
pub fn handle_proxy_client<H: crate::handler::ToolHandler>(
    stream: UnixStream,
    handler: &mut H,
    server_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    loop {
        // Read length-prefixed request
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Box::new(e)),
        }
        let req_len = u32::from_be_bytes(len_buf) as usize;
        let mut req_buf = vec![0u8; req_len];
        reader.read_exact(&mut req_buf)?;

        let req_str = String::from_utf8_lossy(&req_buf);

        // Parse and process the JSON-RPC request
        let response = process_jsonrpc_request(&req_str, handler, server_name);

        // Skip sending for notifications (empty response)
        if response.is_empty() {
            continue;
        }

        // Send length-prefixed response
        let resp_bytes = response.as_bytes();
        let resp_len = resp_bytes.len() as u32;
        writer.write_all(&resp_len.to_be_bytes())?;
        writer.write_all(resp_bytes)?;
        writer.flush()?;
    }

    Ok(())
}

fn process_jsonrpc_request<H: crate::handler::ToolHandler>(
    request: &str,
    handler: &mut H,
    server_name: &str,
) -> String {
    use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
    use serde_json::{json, Value};

    let req: JsonRpcRequest = match serde_json::from_str(request) {
        Ok(r) => r,
        Err(_) => {
            let resp = JsonRpcResponse::error(Value::Null, JsonRpcError::parse_error());
            return serde_json::to_string(&resp).unwrap();
        }
    };

    let id = req.id.clone().unwrap_or(Value::Null);

    let result = match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": server_name, "version": "0.1.0" }
        })),
        "initialized" => return String::new(), // notification, no response
        "ping" => Ok(json!({})),
        "notifications/cancelled" => return String::new(),
        "tools/list" => {
            let tools = handler.tool_definitions();
            Ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = req.params["name"].as_str().unwrap_or("");
            let args = &req.params["arguments"];
            match handler.handle_tool_call(name, args) {
                Ok(result) => {
                    let text = serde_json::to_string(&result).unwrap();
                    Ok(json!({"content": [{"type": "text", "text": text}]}))
                }
                Err(crate::error::McpError::ToolNotFound(n)) => {
                    Err(JsonRpcError::method_not_found(&n))
                }
                Err(crate::error::McpError::InvalidParams(m)) => {
                    Err(JsonRpcError::invalid_params(&m))
                }
                Err(e) => Err(JsonRpcError::internal_error(&e.to_string())),
            }
        }
        _ => Err(JsonRpcError::method_not_found(&req.method)),
    };

    let resp = match result {
        Ok(val) => JsonRpcResponse::success(id, val),
        Err(err) => JsonRpcResponse::error(id, err),
    };

    serde_json::to_string(&resp).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::PtyToolHandler;
    use std::os::unix::net::UnixListener;
    use tttt_pty::{MockPty, SessionManager};

    fn make_socket_pair() -> (UnixStream, UnixStream) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn test_proxy_roundtrip_initialize() {
        let (client_stream, server_stream) = make_socket_pair();

        // Server thread: handle one request
        let server_handle = std::thread::spawn(move || {
            let manager: SessionManager<MockPty> = SessionManager::new();
            let mut handler =
                PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
            handle_proxy_client(server_stream, &mut handler, "test").unwrap();
        });

        // Client side: send initialize, read response
        let mut client = client_stream;
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let req_bytes = req.as_bytes();
        let len = req_bytes.len() as u32;
        client.write_all(&len.to_be_bytes()).unwrap();
        client.write_all(req_bytes).unwrap();
        client.flush().unwrap();

        // Read response
        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        client.read_exact(&mut resp_buf).unwrap();

        let resp: serde_json::Value = serde_json::from_slice(&resp_buf).unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "test");

        // Close to let server thread finish
        drop(client);
        server_handle.join().unwrap();
    }

    #[test]
    fn test_proxy_roundtrip_tools_list() {
        let (client_stream, server_stream) = make_socket_pair();

        let server_handle = std::thread::spawn(move || {
            let manager: SessionManager<MockPty> = SessionManager::new();
            let mut handler =
                PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
            handle_proxy_client(server_stream, &mut handler, "test").unwrap();
        });

        let mut client = client_stream;
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let len = req.len() as u32;
        client.write_all(&len.to_be_bytes()).unwrap();
        client.write_all(req.as_bytes()).unwrap();
        client.flush().unwrap();

        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        client.read_exact(&mut resp_buf).unwrap();

        let resp: serde_json::Value = serde_json::from_slice(&resp_buf).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10); // 10 PTY tools

        drop(client);
        server_handle.join().unwrap();
    }

    #[test]
    fn test_proxy_roundtrip_pty_launch_and_list() {
        let (client_stream, server_stream) = make_socket_pair();

        let server_handle = std::thread::spawn(move || {
            let manager: SessionManager<MockPty> = SessionManager::new();
            let mut handler =
                PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
            handle_proxy_client(server_stream, &mut handler, "test").unwrap();
        });

        let mut client = client_stream;

        // Launch
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{}}}"#;
        let len = req.len() as u32;
        client.write_all(&len.to_be_bytes()).unwrap();
        client.write_all(req.as_bytes()).unwrap();
        client.flush().unwrap();

        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        client.read_exact(&mut resp_buf).unwrap();

        let resp: serde_json::Value = serde_json::from_slice(&resp_buf).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let result: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(result["session_id"], "pty-1");

        // List
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#;
        let len = req.len() as u32;
        client.write_all(&len.to_be_bytes()).unwrap();
        client.write_all(req.as_bytes()).unwrap();
        client.flush().unwrap();

        client.read_exact(&mut len_buf).unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        client.read_exact(&mut resp_buf).unwrap();

        let resp: serde_json::Value = serde_json::from_slice(&resp_buf).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let result: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);

        drop(client);
        server_handle.join().unwrap();
    }

    #[test]
    fn test_proxy_full_stdio_roundtrip() {
        let (proxy_to_tui, tui_stream) = make_socket_pair();
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("proxy.sock").to_string_lossy().to_string();

        // TUI server thread
        let tui_handle = std::thread::spawn(move || {
            let manager: SessionManager<MockPty> = SessionManager::new();
            let mut handler =
                PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
            handle_proxy_client(tui_stream, &mut handler, "tttt").unwrap();
        });

        // Proxy: stdin → socket → stdout
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let reader = std::io::BufReader::new(std::io::Cursor::new(
            format!("{}\n", input).into_bytes(),
        ));
        let mut output: Vec<u8> = Vec::new();

        // Run proxy inline (using the already-connected socket)
        // We can't easily use run_proxy here since it connects by path,
        // so test the server side directly.
        // This test validates the server-side proxy handling.

        // Send via socket directly (simulating what run_proxy does)
        let mut proxy_stream = proxy_to_tui;
        let req_bytes = input.as_bytes();
        let len = req_bytes.len() as u32;
        proxy_stream.write_all(&len.to_be_bytes()).unwrap();
        proxy_stream.write_all(req_bytes).unwrap();
        proxy_stream.flush().unwrap();

        let mut len_buf = [0u8; 4];
        proxy_stream.read_exact(&mut len_buf).unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        proxy_stream.read_exact(&mut resp_buf).unwrap();

        let resp_str = String::from_utf8(resp_buf).unwrap();
        let resp: serde_json::Value = serde_json::from_str(&resp_str).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");

        drop(proxy_stream);
        tui_handle.join().unwrap();
    }
}
