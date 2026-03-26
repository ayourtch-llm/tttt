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

/// Maximum number of reconnect attempts before giving up on a single request.
const MAX_RECONNECT_ATTEMPTS: u32 = 30;

/// Base delay between reconnect attempts (doubles each retry, capped at 2s).
const RECONNECT_BASE_DELAY_MS: u64 = 100;

/// Send a request over the socket and read the response.
/// Returns Ok(Some(response_bytes)) for normal requests, Ok(None) for notifications.
/// Returns Err on connection failure (caller should reconnect).
fn send_and_receive(
    socket: &mut UnixStream,
    request: &[u8],
    is_notification: bool,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    let len = request.len() as u32;
    socket.write_all(&len.to_be_bytes())?;
    socket.write_all(request)?;
    socket.flush()?;

    if is_notification {
        return Ok(None);
    }

    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf)?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;

    let mut resp_buf = vec![0u8; resp_len];
    socket.read_exact(&mut resp_buf)?;
    Ok(Some(resp_buf))
}

/// Try to connect to the socket, retrying with backoff.
fn connect_with_retry(socket_path: &str, max_attempts: u32) -> Result<UnixStream, std::io::Error> {
    let mut delay_ms = RECONNECT_BASE_DELAY_MS;
    for attempt in 0..max_attempts {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(_) if attempt + 1 < max_attempts => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(2000);
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "max reconnect attempts exceeded",
    ))
}

/// Re-initialize the MCP session after reconnecting to a new server instance.
fn reinitialize(socket: &mut UnixStream) -> Result<(), Box<dyn std::error::Error>> {
    let init_req = r#"{"jsonrpc":"2.0","id":"_reconnect_init","method":"initialize","params":{}}"#;
    let _resp = send_and_receive(socket, init_req.as_bytes(), false)?;
    let initialized = r#"{"jsonrpc":"2.0","id":null,"method":"initialized","params":{}}"#;
    send_and_receive(socket, initialized.as_bytes(), true)?;
    Ok(())
}

/// Run the MCP proxy: forward JSON-RPC between stdio and a Unix socket.
///
/// - Reads JSON-RPC lines from `reader` (Claude's stdin)
/// - Forwards each line to the Unix socket (tttt TUI)
/// - Reads response from socket
/// - Writes response to `writer` (Claude's stdout)
///
/// On socket disconnection (e.g., during tttt live reload via execv),
/// automatically reconnects and retries the current request.
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

        let is_notification = is_jsonrpc_notification(trimmed);
        let request_bytes = trimmed.as_bytes();

        // Try to send request; on failure, reconnect and retry
        let response = match send_and_receive(&mut socket, request_bytes, is_notification) {
            Ok(resp) => resp,
            Err(_) => {
                // Connection lost — likely a live reload. Reconnect.
                socket = connect_with_retry(socket_path, MAX_RECONNECT_ATTEMPTS)?;
                reinitialize(&mut socket)?;
                send_and_receive(&mut socket, request_bytes, is_notification)
                    .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?
            }
        };

        if let Some(resp_buf) = response {
            writer.write_all(&resp_buf)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
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

    fn make_handler() -> PtyToolHandler<MockPty> {
        PtyToolHandler::new_owned(
            SessionManager::<MockPty>::new(),
            std::path::PathBuf::from("/tmp"),
        )
    }

    // ── is_jsonrpc_notification ──────────────────────────────────────────────

    #[test]
    fn test_is_notification_null_no_space() {
        assert!(is_jsonrpc_notification(r#"{"id":null,"method":"initialized"}"#));
    }

    #[test]
    fn test_is_notification_null_with_space() {
        assert!(is_jsonrpc_notification(r#"{"id": null,"method":"initialized"}"#));
    }

    #[test]
    fn test_is_not_notification_numeric_id() {
        assert!(!is_jsonrpc_notification(r#"{"id":1,"method":"ping"}"#));
    }

    #[test]
    fn test_is_not_notification_string_id() {
        assert!(!is_jsonrpc_notification(r#"{"id":"abc","method":"ping"}"#));
    }

    #[test]
    fn test_is_not_notification_id_zero() {
        assert!(!is_jsonrpc_notification(r#"{"id":0,"method":"ping"}"#));
    }

    // ── process_jsonrpc_request ──────────────────────────────────────────────

    #[test]
    fn test_process_parse_error() {
        let mut handler = make_handler();
        let resp = process_jsonrpc_request("not json {{{", &mut handler, "srv");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32700); // parse error code
    }

    #[test]
    fn test_process_ping() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], serde_json::json!({}));
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn test_process_initialized_notification_returns_empty() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":null,"method":"initialized","params":{}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        assert!(resp.is_empty());
    }

    #[test]
    fn test_process_notifications_cancelled_returns_empty() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":null,"method":"notifications/cancelled","params":{}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        assert!(resp.is_empty());
    }

    #[test]
    fn test_process_unknown_method() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"no/such","params":{}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32601); // method not found
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn test_process_tools_call_unknown_tool() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"does_not_exist","arguments":{}}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], -32601); // ToolNotFound → method_not_found
        assert_eq!(v["id"], 3);
    }

    #[test]
    fn test_process_tools_call_success_pty_launch() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{}}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v["result"]["content"].is_array(), "expected content array");
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        let result: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(result["session_id"], "pty-1");
    }

    #[test]
    fn test_process_initialize_returns_server_info() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":5,"method":"initialize","params":{}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "my-server");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "my-server");
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn test_process_tools_list() {
        let mut handler = make_handler();
        let req = r#"{"jsonrpc":"2.0","id":6,"method":"tools/list","params":{}}"#;
        let resp = process_jsonrpc_request(req, &mut handler, "srv");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v["result"]["tools"].is_array());
        assert!(!v["result"]["tools"].as_array().unwrap().is_empty());
    }

    // ── send_and_receive notification path ───────────────────────────────────

    #[test]
    fn test_send_and_receive_notification_returns_none() {
        let (mut client, mut server) = make_socket_pair();
        // Spawn a thread that reads the framed message from the server side.
        let server_handle = std::thread::spawn(move || {
            let mut len_buf = [0u8; 4];
            server.read_exact(&mut len_buf).unwrap();
            let n = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; n];
            server.read_exact(&mut buf).unwrap();
        });

        let result = send_and_receive(&mut client, b"hello", true).unwrap();
        assert!(result.is_none());
        server_handle.join().unwrap();
    }

    #[test]
    fn test_send_and_receive_request_returns_response() {
        let (mut client, mut server) = make_socket_pair();
        let reply = b"world";
        let server_handle = std::thread::spawn(move || {
            // read the framed request
            let mut len_buf = [0u8; 4];
            server.read_exact(&mut len_buf).unwrap();
            let n = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; n];
            server.read_exact(&mut buf).unwrap();
            // write a framed reply
            let rlen = reply.len() as u32;
            server.write_all(&rlen.to_be_bytes()).unwrap();
            server.write_all(reply).unwrap();
            server.flush().unwrap();
        });

        let result = send_and_receive(&mut client, b"hello", false).unwrap();
        assert_eq!(result, Some(b"world".to_vec()));
        server_handle.join().unwrap();
    }

    // ── connect_with_retry ───────────────────────────────────────────────────

    #[test]
    fn test_connect_with_retry_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retry.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let path_str = path.to_string_lossy().to_string();

        let accept_handle = std::thread::spawn(move || {
            let _ = listener.accept().unwrap();
        });

        let result = connect_with_retry(&path_str, 3);
        assert!(result.is_ok());
        accept_handle.join().unwrap();
    }

    #[test]
    fn test_connect_with_retry_failure() {
        let result = connect_with_retry("/nonexistent_tttt_test.sock", 1);
        assert!(result.is_err());
    }

    // ── run_proxy end-to-end ─────────────────────────────────────────────────

    #[test]
    fn test_run_proxy_initialize() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("run_proxy.sock");
        let sock_str = sock_path.to_string_lossy().to_string();

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Server thread: accept one connection, handle it with PtyToolHandler
        let server_handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut handler = PtyToolHandler::new_owned(
                SessionManager::<MockPty>::new(),
                std::path::PathBuf::from("/tmp"),
            );
            handle_proxy_client(stream, &mut handler, "tttt").unwrap();
        });

        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";
        let reader = std::io::BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        let mut output: Vec<u8> = Vec::new();

        run_proxy(reader, &mut output, &sock_str).unwrap();

        let out_str = String::from_utf8(output).unwrap();
        let v: serde_json::Value = serde_json::from_str(out_str.trim()).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "tttt");

        server_handle.join().unwrap();
    }

    #[test]
    fn test_run_proxy_notification_produces_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("run_proxy_notif.sock");
        let sock_str = sock_path.to_string_lossy().to_string();

        let listener = UnixListener::bind(&sock_path).unwrap();

        let server_handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut handler = make_handler();
            handle_proxy_client(stream, &mut handler, "tttt").unwrap();
        });

        // A notification (id:null) should be forwarded but produce no output line
        let input = "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"initialized\",\"params\":{}}\n";
        let reader = std::io::BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        let mut output: Vec<u8> = Vec::new();

        run_proxy(reader, &mut output, &sock_str).unwrap();

        assert!(output.is_empty(), "notifications should produce no proxy output");
        server_handle.join().unwrap();
    }

    #[test]
    fn test_run_proxy_empty_lines_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("run_proxy_empty.sock");
        let sock_str = sock_path.to_string_lossy().to_string();

        let listener = UnixListener::bind(&sock_path).unwrap();

        let server_handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut handler = make_handler();
            handle_proxy_client(stream, &mut handler, "tttt").unwrap();
        });

        // Only blank lines + real request
        let input = "\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{}}\n";
        let reader = std::io::BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        let mut output: Vec<u8> = Vec::new();

        run_proxy(reader, &mut output, &sock_str).unwrap();

        let out_str = String::from_utf8(output).unwrap();
        let v: serde_json::Value = serde_json::from_str(out_str.trim()).unwrap();
        assert_eq!(v["result"], serde_json::json!({}));
        server_handle.join().unwrap();
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
        assert_eq!(tools.len(), 15); // 15 PTY tools

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
