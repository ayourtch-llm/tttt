use crate::error::{McpError, Result};
use crate::handler::ToolHandler;
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// MCP server that reads JSON-RPC requests from a reader and writes responses to a writer.
///
/// Generic over reader, writer, and handler for testability.
pub struct McpServer<R: BufRead, W: Write, H: ToolHandler> {
    reader: R,
    writer: W,
    handler: H,
    server_name: String,
    server_version: String,
    initialized: bool,
}

impl<R: BufRead, W: Write, H: ToolHandler> McpServer<R, W, H> {
    pub fn new(reader: R, writer: W, handler: H) -> Self {
        Self {
            reader,
            writer,
            handler,
            server_name: "tttt".to_string(),
            server_version: "0.1.0".to_string(),
            initialized: false,
        }
    }

    /// Process a single JSON-RPC request line and return the response.
    pub fn process_line(&mut self, line: &str) -> Option<JsonRpcResponse> {
        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(_) => {
                return Some(JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::parse_error(),
                ));
            }
        };

        let id = request.id.clone().unwrap_or(Value::Null);

        let result = match request.method.as_str() {
            "initialize" => self.handle_initialize(&request),
            "initialized" => return None, // notification, no response
            "ping" => Ok(json!({})),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => self.handle_tools_call(&request),
            "notifications/cancelled" => return None,
            _ => Err(McpError::Protocol(request.method.clone())),
        };

        Some(match result {
            Ok(value) => JsonRpcResponse::success(id, value),
            Err(McpError::ToolNotFound(name)) => {
                JsonRpcResponse::error(id, JsonRpcError::method_not_found(&name))
            }
            Err(McpError::InvalidParams(msg)) => {
                JsonRpcResponse::error(id, JsonRpcError::invalid_params(&msg))
            }
            Err(McpError::Protocol(method)) => {
                JsonRpcResponse::error(id, JsonRpcError::method_not_found(&method))
            }
            Err(e) => JsonRpcResponse::error(id, JsonRpcError::internal_error(&e.to_string())),
        })
    }

    fn handle_initialize(&mut self, _request: &JsonRpcRequest) -> Result<Value> {
        self.initialized = true;
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": self.server_name,
                "version": self.server_version
            }
        }))
    }

    fn handle_tools_list(&self) -> Result<Value> {
        let tools = self.handler.tool_definitions();
        Ok(json!({"tools": tools}))
    }

    fn handle_tools_call(&mut self, request: &JsonRpcRequest) -> Result<Value> {
        let params = &request.params;
        let name = params["name"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("tool name required".to_string()))?;
        let args = &params["arguments"];

        let result = self.handler.handle_tool_call(name, args)?;
        let text = serde_json::to_string(&result)?;

        Ok(json!({
            "content": [{"type": "text", "text": text}]
        }))
    }

    /// Run the server loop, reading lines from the reader until EOF.
    pub fn run(&mut self) -> Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                break; // EOF
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(response) = self.process_line(trimmed) {
                let json = serde_json::to_string(&response)?;
                writeln!(self.writer, "{}", json)?;
                self.writer.flush()?;
            }
        }
        Ok(())
    }

    /// Access the handler.
    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// Access the handler mutably.
    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }

    /// Access the writer (for testing).
    pub fn writer(&self) -> &W {
        &self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::PtyToolHandler;
    use std::io::Cursor;
    use tttt_pty::{MockPty, SessionManager};

    fn make_server(
        input: &str,
    ) -> McpServer<std::io::BufReader<Cursor<Vec<u8>>>, Vec<u8>, PtyToolHandler<MockPty>> {
        let reader = std::io::BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let writer = Vec::new();
        let manager: SessionManager<MockPty> = SessionManager::new();
        let handler = PtyToolHandler::new_owned(manager, std::path::PathBuf::from("/tmp"));
        McpServer::new(reader, writer, handler)
    }

    #[test]
    fn test_initialize_handshake() {
        let mut server = make_server("");
        let resp = server
            .process_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();
        assert!(resp.result.is_some());
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["serverInfo"]["name"], "tttt");
    }

    #[test]
    fn test_initialized_notification() {
        let mut server = make_server("");
        let resp = server.process_line(r#"{"jsonrpc":"2.0","method":"initialized"}"#);
        assert!(resp.is_none()); // notifications don't get responses
    }

    #[test]
    fn test_ping() {
        let mut server = make_server("");
        let resp = server
            .process_line(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
            .unwrap();
        assert!(resp.result.is_some());
    }

    #[test]
    fn test_tools_list() {
        let mut server = make_server("");
        let resp = server
            .process_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#)
            .unwrap();
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 15);
    }

    #[test]
    fn test_tools_call_pty_list() {
        let mut server = make_server("");
        let resp = server
            .process_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"tttt_pty_list","arguments":{}}}"#,
            )
            .unwrap();
        let result = resp.result.unwrap();
        assert!(result["content"].is_array());
    }

    #[test]
    fn test_tools_call_missing_name() {
        let mut server = make_server("");
        let resp = server
            .process_line(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"arguments":{}}}"#,
            )
            .unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32602);
    }

    #[test]
    fn test_tools_call_unknown_tool() {
        let mut server = make_server("");
        let resp = server
            .process_line(
                r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nonexistent","arguments":{}}}"#,
            )
            .unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn test_unknown_method() {
        let mut server = make_server("");
        let resp = server
            .process_line(r#"{"jsonrpc":"2.0","id":7,"method":"unknown_method"}"#)
            .unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn test_malformed_json() {
        let mut server = make_server("");
        let resp = server.process_line("not json at all").unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32700);
    }

    #[test]
    fn test_run_loop() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            "\n",
        );
        let mut server = make_server(input);
        server.run().unwrap();
        let output = String::from_utf8(server.writer.clone()).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        // First response should be initialize
        let resp1: JsonRpcResponse = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp1.id, Value::from(1));
        // Second response should be ping
        let resp2: JsonRpcResponse = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp2.id, Value::from(2));
    }

    #[test]
    fn test_run_skips_empty_lines() {
        let input = "\n\n";
        let mut server = make_server(input);
        server.run().unwrap();
        assert!(server.writer.is_empty());
    }

    #[test]
    fn test_response_id_matches_request() {
        let mut server = make_server("");
        let resp = server
            .process_line(r#"{"jsonrpc":"2.0","id":"abc-123","method":"ping"}"#)
            .unwrap();
        assert_eq!(resp.id, Value::from("abc-123"));
    }
}
