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
use std::sync::mpsc;

/// Maximum number of reconnect attempts before giving up on a single request.
const MAX_RECONNECT_ATTEMPTS: u32 = 30;

/// Base delay between reconnect attempts (doubles each retry, capped at 2s).
const RECONNECT_BASE_DELAY_MS: u64 = 100;

/// Send a request over the socket and read the response (length-prefixed binary).
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

/// Send a request over the socket using newline-delimited JSON framing.
/// Returns Ok(Some(response_bytes)) for normal requests, Ok(None) for notifications.
fn send_and_receive_ndjson(
    socket: &mut UnixStream,
    request: &[u8],
    is_notification: bool,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    socket.write_all(request)?;
    socket.write_all(b"\n")?;
    socket.flush()?;

    if is_notification {
        return Ok(None);
    }

    // Read response byte-by-byte until newline to avoid over-buffering
    let mut resp = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        socket.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        resp.push(byte[0]);
    }
    Ok(Some(resp))
}

/// Write a response using the specified framing format.
fn write_response<W: Write>(writer: &mut W, response: &str, ndjson: bool) -> std::io::Result<()> {
    let resp_bytes = response.as_bytes();
    if ndjson {
        writer.write_all(resp_bytes)?;
        writer.write_all(b"\n")?;
    } else {
        let resp_len = resp_bytes.len() as u32;
        writer.write_all(&resp_len.to_be_bytes())?;
        writer.write_all(resp_bytes)?;
    }
    writer.flush()
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
fn reinitialize(
    socket: &mut UnixStream,
    debug_protocol: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let init_req = r#"{"jsonrpc":"2.0","id":"_reconnect_init","method":"initialize","params":{}}"#;
    let send_fn = if debug_protocol {
        send_and_receive_ndjson
    } else {
        send_and_receive
    };
    let _resp = send_fn(socket, init_req.as_bytes(), false)?;
    let initialized = r#"{"jsonrpc":"2.0","id":null,"method":"initialized","params":{}}"#;
    send_fn(socket, initialized.as_bytes(), true)?;
    Ok(())
}

/// Events sent from reader threads to the main proxy event loop.
enum ProxyEvent {
    /// A complete JSON-RPC line read from stdin.
    StdinLine(String),
    /// Stdin reached EOF.
    StdinEof,
    /// A complete length-prefixed response read from the socket.
    SocketResponse(Vec<u8>),
    /// The socket reader encountered an error or EOF.
    SocketError,
}

/// Spawn a thread that reads messages from a socket and sends them as
/// `ProxyEvent::SocketResponse` over the channel. When `ndjson` is true,
/// reads newline-delimited JSON; otherwise reads length-prefixed binary.
fn spawn_socket_reader(
    socket: UnixStream,
    tx: mpsc::Sender<ProxyEvent>,
    ndjson: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(socket);
        if ndjson {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(ProxyEvent::SocketError);
                        return;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let resp_buf = trimmed.as_bytes().to_vec();
                            if tx.send(ProxyEvent::SocketResponse(resp_buf)).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        } else {
            loop {
                let mut len_buf = [0u8; 4];
                if reader.read_exact(&mut len_buf).is_err() {
                    let _ = tx.send(ProxyEvent::SocketError);
                    return;
                }
                let resp_len = u32::from_be_bytes(len_buf) as usize;
                let mut resp_buf = vec![0u8; resp_len];
                if reader.read_exact(&mut resp_buf).is_err() {
                    let _ = tx.send(ProxyEvent::SocketError);
                    return;
                }
                if tx.send(ProxyEvent::SocketResponse(resp_buf)).is_err() {
                    return;
                }
            }
        }
    })
}

/// Run the MCP proxy: forward JSON-RPC between stdio and a Unix socket.
///
/// - Reads JSON-RPC lines from `reader` (Claude's stdin) in a background thread
/// - Reads responses from the Unix socket (tttt TUI) in another background thread
/// - Main thread coordinates: forwards requests to socket, responses to `writer`
///
/// The concurrent design ensures that notifications (like cancellations) are
/// forwarded immediately, even while waiting for a long-running tool response.
///
/// On socket disconnection (e.g., during tttt live reload via execv),
/// automatically reconnects and retries the current request.
pub fn run_proxy<R: Read + Send + 'static, W: Write>(
    reader: R,
    mut writer: W,
    socket_path: &str,
    debug_protocol: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel();

    // Spawn stdin reader thread
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match buf_reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = stdin_tx.send(ProxyEvent::StdinEof);
                    return;
                }
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        if stdin_tx.send(ProxyEvent::StdinLine(trimmed)).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    // Connect to TUI socket and spawn socket reader thread
    let mut socket = UnixStream::connect(socket_path)?;
    let socket_read = socket.try_clone()?;
    // Keep a sender clone for spawning new socket reader threads on reconnect.
    // Set to None when draining to allow the channel to close.
    let mut master_tx = Some(tx);
    spawn_socket_reader(socket_read, master_tx.as_ref().unwrap().clone(), debug_protocol);

    let mut draining = false;

    for event in &rx {
        match event {
            ProxyEvent::StdinLine(line) => {
                if draining {
                    continue;
                }

                let req_bytes = line.as_bytes();

                let write_request = |sock: &mut UnixStream| -> std::io::Result<()> {
                    if debug_protocol {
                        sock.write_all(req_bytes)?;
                        sock.write_all(b"\n")?;
                    } else {
                        let len = req_bytes.len() as u32;
                        sock.write_all(&len.to_be_bytes())?;
                        sock.write_all(req_bytes)?;
                    }
                    sock.flush()
                };

                match write_request(&mut socket) {
                    Ok(()) => {}
                    Err(_) => {
                        // Connection lost — likely a live reload. Reconnect.
                        socket =
                            connect_with_retry(socket_path, MAX_RECONNECT_ATTEMPTS)?;
                        reinitialize(&mut socket, debug_protocol)?;
                        let new_read = socket.try_clone()?;
                        spawn_socket_reader(
                            new_read,
                            master_tx.as_ref().unwrap().clone(),
                            debug_protocol,
                        );

                        // Retry the write
                        write_request(&mut socket)?;
                    }
                }
            }
            ProxyEvent::SocketResponse(resp) => {
                writer.write_all(&resp)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
            ProxyEvent::StdinEof => {
                draining = true;
                let _ = socket.shutdown(std::net::Shutdown::Write);
                // Drop master sender so channel closes when threads exit
                master_tx = None;
            }
            ProxyEvent::SocketError => {
                if draining {
                    continue;
                }
                // Proactively reconnect so we're ready for the next request
                if let Ok(new_socket) =
                    connect_with_retry(socket_path, MAX_RECONNECT_ATTEMPTS)
                {
                    socket = new_socket;
                    if reinitialize(&mut socket, debug_protocol).is_ok() {
                        if let Ok(new_read) = socket.try_clone() {
                            spawn_socket_reader(
                                new_read,
                                master_tx.as_ref().unwrap().clone(),
                                debug_protocol,
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Check if a JSON-RPC request is a notification (id is null or missing).
/// Per JSON-RPC 2.0, a notification has no "id" field or "id" is null.
#[cfg(test)]
fn is_jsonrpc_notification(line: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => match v.get("id") {
            None => true,
            Some(id) => id.is_null(),
        },
        Err(_) => false,
    }
}

/// Events from the TUI-side socket reader thread.
enum TuiSocketEvent {
    /// A complete request read from the socket. The bool indicates ndjson framing.
    Request(Vec<u8>, bool),
    /// A cancellation notification was received (already applied to cancel token).
    CancelReceived,
    /// The socket reader encountered EOF or an error.
    Eof,
}

/// Read messages from a socket and send them over a channel.
/// Auto-detects framing per-message: if first byte is `{` (0x7B), reads
/// newline-delimited JSON; otherwise reads length-prefixed binary.
/// Cancel notifications are detected inline and immediately applied to the
/// cancel token, so long-running handlers can observe cancellation promptly.
fn spawn_tui_socket_reader(
    socket: UnixStream,
    tx: mpsc::Sender<TuiSocketEvent>,
    cancel_token: std::sync::Arc<std::sync::atomic::AtomicBool>,
    debug_path: String,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let debug_log = |msg: &str| {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&debug_path)
            {
                let _ = writeln!(
                    f,
                    "[{:?}] [tui-reader] {}",
                    std::time::SystemTime::now(),
                    msg
                );
            }
        };

        let mut reader = BufReader::new(socket);
        loop {
            // Peek first byte to detect framing format
            let first_byte = match reader.fill_buf() {
                Ok([]) => {
                    debug_log("EOF on socket read");
                    let _ = tx.send(TuiSocketEvent::Eof);
                    return;
                }
                Ok(b) => b[0],
                Err(_) => {
                    debug_log("error on socket read");
                    let _ = tx.send(TuiSocketEvent::Eof);
                    return;
                }
            };

            let req_buf = if first_byte == b'{' {
                // Ndjson framing: read until newline
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        debug_log("EOF/error reading ndjson line");
                        let _ = tx.send(TuiSocketEvent::Eof);
                        return;
                    }
                    Ok(_) => line.trim().as_bytes().to_vec(),
                }
            } else {
                // Length-prefixed binary framing
                let mut len_buf = [0u8; 4];
                match reader.read_exact(&mut len_buf) {
                    Ok(()) => {}
                    Err(_) => {
                        debug_log("EOF/error on socket read");
                        let _ = tx.send(TuiSocketEvent::Eof);
                        return;
                    }
                }
                let req_len = u32::from_be_bytes(len_buf) as usize;
                let mut req_buf = vec![0u8; req_len];
                if reader.read_exact(&mut req_buf).is_err() {
                    debug_log("EOF/error reading request body");
                    let _ = tx.send(TuiSocketEvent::Eof);
                    return;
                }
                req_buf
            };

            let is_ndjson = first_byte == b'{';

            // Check for cancel notifications inline and set the token immediately
            let req_str = String::from_utf8_lossy(&req_buf);
            if req_str.contains("notifications/cancelled") {
                debug_log(&format!(
                    "cancel notification detected, setting token: {}",
                    &req_str[..req_str.len().min(200)]
                ));
                cancel_token.store(true, std::sync::atomic::Ordering::Relaxed);
                if tx.send(TuiSocketEvent::CancelReceived).is_err() {
                    return;
                }
            } else {
                debug_log(&format!(
                    "forwarding request len={} ndjson={}: {}",
                    req_buf.len(),
                    is_ndjson,
                    &req_str[..req_str.len().min(200)]
                ));
                if tx.send(TuiSocketEvent::Request(req_buf, is_ndjson)).is_err() {
                    return;
                }
            }
        }
    })
}

/// Server-side handler: reads proxied requests from a Unix socket,
/// processes them using a ToolHandler, and sends responses back.
///
/// Uses a reader thread so that cancellation notifications can be detected
/// while a long-running tool call (like wait_for_idle) is executing.
///
/// This runs in a thread within the TUI process.
pub fn handle_proxy_client<H: crate::handler::ToolHandler>(
    stream: UnixStream,
    handler: &mut H,
    server_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = stream.try_clone()?;

    let pid = std::process::id();
    let debug_path = format!("/tmp/tttt-{}-debug.txt", pid);
    let debug_path_clone = debug_path.clone();
    let debug_log = move |msg: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&debug_path_clone)
        {
            let _ = writeln!(f, "[{:?}] {}", std::time::SystemTime::now(), msg);
        }
    };
    debug_log(&format!("proxy started, debug_path={}", debug_path));

    // Cancel token shared between the reader thread and the handler.
    // The reader thread sets it when it detects a cancel notification;
    // long-running handlers check it each iteration.
    let cancel_token = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn reader thread to decouple socket reads from request processing.
    let (tx, rx) = mpsc::channel();
    let _reader_handle =
        spawn_tui_socket_reader(stream, tx, cancel_token.clone(), debug_path.clone());

    loop {
        debug_log("waiting for request...");
        let (req_buf, client_ndjson) = match rx.recv() {
            Ok(TuiSocketEvent::Request(buf, ndjson)) => (buf, ndjson),
            Ok(TuiSocketEvent::CancelReceived) => {
                debug_log("cancel received while idle (no active tool call), ignoring");
                continue;
            }
            Ok(TuiSocketEvent::Eof) | Err(_) => {
                debug_log("EOF on read, breaking");
                break;
            }
        };

        let req_str = String::from_utf8_lossy(&req_buf);
        debug_log(&format!(
            "got request len={}: {}",
            req_buf.len(),
            &req_str[..req_str.len().min(200)]
        ));

        // Check if this is a tools/call (potentially long-running)
        let is_tools_call = req_str.contains("\"method\":\"tools/call\"")
            || req_str.contains("\"method\": \"tools/call\"");

        if is_tools_call {
            // Reset cancel token and set it on the handler before processing
            cancel_token.store(false, std::sync::atomic::Ordering::Relaxed);
            handler.set_cancel_token(cancel_token.clone());
            debug_log("tools/call: cancel token armed, processing...");
        }

        // Process the request (may block for a long time on tools/call).
        // During this time, the reader thread continues reading from the socket
        // and will set cancel_token if a cancel notification arrives.
        let response = process_jsonrpc_request(&req_str, handler, server_name);
        debug_log(&format!(
            "response len={}: {}",
            response.len(),
            &response[..response.len().min(200)]
        ));

        // After processing, drain any pending messages that arrived while blocked.
        loop {
            match rx.try_recv() {
                Ok(TuiSocketEvent::Request(pending_buf, pending_ndjson)) => {
                    let pending_str = String::from_utf8_lossy(&pending_buf);
                    debug_log(&format!(
                        "draining pending request len={}: {}",
                        pending_buf.len(),
                        &pending_str[..pending_str.len().min(200)]
                    ));
                    // Process the queued request
                    let pending_response =
                        process_jsonrpc_request(&pending_str, handler, server_name);
                    if !pending_response.is_empty() {
                        debug_log(&format!(
                            "sending pending response len={}",
                            pending_response.len()
                        ));
                        write_response(&mut writer, &pending_response, pending_ndjson)?;
                        debug_log("pending response sent");
                    }
                }
                Ok(TuiSocketEvent::CancelReceived) => {
                    debug_log("cancel notification consumed from pending queue");
                }
                Ok(TuiSocketEvent::Eof) => {
                    debug_log("EOF while draining pending");
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // Skip sending for notifications (empty response)
        if response.is_empty() {
            debug_log("notification (empty response), continuing");
            continue;
        }

        // Send response using the same framing the client used
        match write_response(&mut writer, &response, client_ndjson) {
            Ok(()) => {}
            Err(e) => {
                debug_log(&format!("write error: {}", e));
                return Err(e.into());
            }
        }
        debug_log("response sent");
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

    #[test]
    fn test_is_notification_missing_id() {
        // JSON-RPC 2.0: a request with no "id" field at all is a notification
        assert!(is_jsonrpc_notification(
            r#"{"method":"notifications/cancelled","params":{"requestId":2}}"#
        ));
    }

    #[test]
    fn test_is_not_notification_invalid_json() {
        assert!(!is_jsonrpc_notification("not json {{{"));
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

        run_proxy(reader, &mut output, &sock_str, false).unwrap();

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

        run_proxy(reader, &mut output, &sock_str, false).unwrap();

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

        run_proxy(reader, &mut output, &sock_str, false).unwrap();

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
