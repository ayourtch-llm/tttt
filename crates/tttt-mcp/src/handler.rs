use crate::error::{McpError, Result};
use serde_json::{json, Value};
use tttt_pty::{MockPty, PtyBackend, PtySession, SessionManager};

/// Trait for handling MCP tool calls.
pub trait ToolHandler: Send {
    /// Handle a tool call by name with the given arguments.
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value>;

    /// Return the list of tool definitions this handler provides.
    fn tool_definitions(&self) -> Vec<Value>;
}

/// Handles PTY-related tool calls by delegating to a SessionManager.
pub struct PtyToolHandler<B: PtyBackend> {
    manager: SessionManager<B>,
    work_dir: std::path::PathBuf,
    default_cols: u16,
    default_rows: u16,
}

impl<B: PtyBackend> PtyToolHandler<B> {
    pub fn new(manager: SessionManager<B>, work_dir: std::path::PathBuf) -> Self {
        Self {
            manager,
            work_dir,
            default_cols: 80,
            default_rows: 24,
        }
    }

    /// Access the session manager.
    pub fn manager(&self) -> &SessionManager<B> {
        &self.manager
    }

    /// Access the session manager mutably.
    pub fn manager_mut(&mut self) -> &mut SessionManager<B> {
        &mut self.manager
    }

    fn handle_pty_list(&self) -> Result<Value> {
        let sessions = self.manager.list();
        Ok(json!(sessions))
    }

    fn handle_pty_get_screen(&mut self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let session = self.manager.get_mut(session_id)?;
        session.pump()?;
        let contents = session.get_screen();
        let cursor = session.cursor_position();
        Ok(json!({
            "screen": contents,
            "cursor": [cursor.1, cursor.0]
        }))
    }

    fn handle_pty_send_keys(&mut self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let keys = args["keys"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("keys required".to_string()))?;
        let raw = args["raw"].as_bool().unwrap_or(false);

        let session = self.manager.get_mut(session_id)?;
        if raw {
            session.send_keys(keys)?;
        } else {
            let with_newline = format!("{}\n", keys);
            session.send_keys(&with_newline)?;
        }
        Ok(json!({"status": "ok"}))
    }

    fn handle_pty_kill(&mut self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        self.manager.kill_session(session_id)?;
        Ok(json!({"status": "ok"}))
    }

    fn handle_pty_get_cursor(&mut self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let session = self.manager.get(session_id)?;
        let (row, col) = session.cursor_position();
        Ok(json!({"row": row, "col": col}))
    }

    fn handle_pty_resize(&mut self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let cols = args["cols"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("cols required".to_string()))? as u16;
        let rows = args["rows"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("rows required".to_string()))? as u16;
        let session = self.manager.get_mut(session_id)?;
        session.resize(cols, rows)?;
        Ok(json!({"status": "ok"}))
    }

    fn handle_pty_set_scrollback(&mut self, args: &Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| McpError::InvalidParams("session_id required".to_string()))?;
        let lines = args["lines"]
            .as_u64()
            .ok_or_else(|| McpError::InvalidParams("lines required".to_string()))? as usize;
        let session = self.manager.get_mut(session_id)?;
        session.set_scrollback(lines);
        Ok(json!({"status": "ok"}))
    }
}

impl PtyToolHandler<MockPty> {
    /// Launch a session using MockPty (for testing).
    pub fn handle_pty_launch_mock(&mut self, args: &Value) -> Result<Value> {
        let cols = args["cols"].as_u64().unwrap_or(self.default_cols as u64) as u16;
        let rows = args["rows"].as_u64().unwrap_or(self.default_rows as u64) as u16;
        let command = args["command"].as_str().unwrap_or("bash").to_string();

        let id = self.manager.generate_id();
        let mock = MockPty::new(cols, rows);
        let session = PtySession::new(id.clone(), mock, command, cols, rows);
        self.manager.add_session(session)?;
        Ok(json!({"session_id": id}))
    }
}

impl PtyToolHandler<tttt_pty::RealPty> {
    /// Launch a real PTY session.
    pub fn handle_pty_launch_real(&mut self, args: &Value) -> Result<Value> {
        let cols = args["cols"].as_u64().unwrap_or(self.default_cols as u64) as u16;
        let rows = args["rows"].as_u64().unwrap_or(self.default_rows as u64) as u16;
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let command = args["command"]
            .as_str()
            .unwrap_or(&default_shell);
        let cmd_args: Vec<&str> = args["args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let backend = tttt_pty::RealPty::spawn(command, &cmd_args, cols, rows)?;
        let id = self.manager.generate_id();
        let session = PtySession::new(id.clone(), backend, command.to_string(), cols, rows);
        self.manager.add_session(session)?;
        Ok(json!({"session_id": id}))
    }
}

impl ToolHandler for PtyToolHandler<tttt_pty::RealPty> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "pty_launch" => self.handle_pty_launch_real(args),
            "pty_list" => self.handle_pty_list(),
            "pty_get_screen" => self.handle_pty_get_screen(args),
            "pty_send_keys" => self.handle_pty_send_keys(args),
            "pty_kill" => self.handle_pty_kill(args),
            "pty_get_cursor" => self.handle_pty_get_cursor(args),
            "pty_resize" => self.handle_pty_resize(args),
            "pty_set_scrollback" => self.handle_pty_set_scrollback(args),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
    }
}

impl ToolHandler for PtyToolHandler<MockPty> {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "pty_launch" => self.handle_pty_launch_mock(args),
            "pty_list" => self.handle_pty_list(),
            "pty_get_screen" => self.handle_pty_get_screen(args),
            "pty_send_keys" => self.handle_pty_send_keys(args),
            "pty_kill" => self.handle_pty_kill(args),
            "pty_get_cursor" => self.handle_pty_get_cursor(args),
            "pty_resize" => self.handle_pty_resize(args),
            "pty_set_scrollback" => self.handle_pty_set_scrollback(args),
            _ => Err(McpError::ToolNotFound(name.to_string())),
        }
    }

    fn tool_definitions(&self) -> Vec<Value> {
        crate::tools::pty_tool_definitions()
    }
}

/// Combines multiple ToolHandlers, dispatching to the first that handles a tool.
pub struct CompositeToolHandler {
    handlers: Vec<Box<dyn ToolHandler>>,
}

impl CompositeToolHandler {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn add_handler(&mut self, handler: Box<dyn ToolHandler>) {
        self.handlers.push(handler);
    }
}

impl Default for CompositeToolHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolHandler for CompositeToolHandler {
    fn handle_tool_call(&mut self, name: &str, args: &Value) -> Result<Value> {
        for handler in &mut self.handlers {
            match handler.handle_tool_call(name, args) {
                Err(McpError::ToolNotFound(_)) => continue,
                result => return result,
            }
        }
        Err(McpError::ToolNotFound(name.to_string()))
    }

    fn tool_definitions(&self) -> Vec<Value> {
        self.handlers
            .iter()
            .flat_map(|h| h.tool_definitions())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_handler() -> PtyToolHandler<MockPty> {
        let manager = SessionManager::new();
        PtyToolHandler::new(manager, std::path::PathBuf::from("/tmp"))
    }

    fn make_handler_with_session() -> PtyToolHandler<MockPty> {
        let mut handler = make_handler();
        handler
            .handle_pty_launch_mock(&json!({}))
            .unwrap();
        handler
    }

    #[test]
    fn test_pty_launch_mock() {
        let mut handler = make_handler();
        let result = handler.handle_pty_launch_mock(&json!({})).unwrap();
        assert!(result["session_id"].is_string());
        assert_eq!(handler.manager().session_count(), 1);
    }

    #[test]
    fn test_pty_launch_custom_dims() {
        let mut handler = make_handler();
        let result = handler
            .handle_pty_launch_mock(&json!({"cols": 120, "rows": 40}))
            .unwrap();
        let id = result["session_id"].as_str().unwrap();
        let session = handler.manager().get(id).unwrap();
        let meta = session.metadata();
        assert_eq!(meta.cols, 120);
        assert_eq!(meta.rows, 40);
    }

    #[test]
    fn test_pty_list_empty() {
        let handler = make_handler();
        let result = handler.handle_pty_list().unwrap();
        assert_eq!(result.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_pty_list_with_sessions() {
        let mut handler = make_handler_with_session();
        let result = handler.handle_pty_list().unwrap();
        assert_eq!(result.as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_pty_send_keys() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        let result = handler.handle_tool_call(
            "pty_send_keys",
            &json!({"session_id": id, "keys": "hello"}),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_pty_send_keys_raw() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        let result = handler.handle_tool_call(
            "pty_send_keys",
            &json!({"session_id": id, "keys": "hello", "raw": true}),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_pty_send_keys_missing_session_id() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("pty_send_keys", &json!({"keys": "hello"}));
        assert!(result.is_err());
    }

    #[test]
    fn test_pty_get_screen() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        let result = handler
            .handle_tool_call("pty_get_screen", &json!({"session_id": id}))
            .unwrap();
        assert!(result["screen"].is_string());
        assert!(result["cursor"].is_array());
    }

    #[test]
    fn test_pty_kill() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        handler
            .handle_tool_call("pty_kill", &json!({"session_id": id}))
            .unwrap();
        assert_eq!(handler.manager().session_count(), 0);
    }

    #[test]
    fn test_pty_get_cursor() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        let result = handler
            .handle_tool_call("pty_get_cursor", &json!({"session_id": id}))
            .unwrap();
        assert_eq!(result["row"], 0);
        assert_eq!(result["col"], 0);
    }

    #[test]
    fn test_pty_resize() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        handler
            .handle_tool_call("pty_resize", &json!({"session_id": id, "cols": 100, "rows": 50}))
            .unwrap();
        let session = handler.manager().get(&id).unwrap();
        let meta = session.metadata();
        assert_eq!(meta.cols, 100);
        assert_eq!(meta.rows, 50);
    }

    #[test]
    fn test_pty_set_scrollback() {
        let mut handler = make_handler_with_session();
        let id = handler.manager().list()[0].id.clone();
        handler
            .handle_tool_call("pty_set_scrollback", &json!({"session_id": id, "lines": 5000}))
            .unwrap();
    }

    #[test]
    fn test_unknown_tool() {
        let mut handler = make_handler();
        let result = handler.handle_tool_call("nonexistent", &json!({}));
        assert!(matches!(result.unwrap_err(), McpError::ToolNotFound(_)));
    }

    #[test]
    fn test_composite_merges_definitions() {
        let mut composite = CompositeToolHandler::new();
        let handler = make_handler();
        composite.add_handler(Box::new(handler));
        let defs = composite.tool_definitions();
        assert_eq!(defs.len(), 8); // 8 PTY tools
    }

    #[test]
    fn test_composite_dispatches() {
        let mut composite = CompositeToolHandler::new();
        let handler = make_handler();
        composite.add_handler(Box::new(handler));
        let result = composite.handle_tool_call("pty_list", &json!({}));
        assert!(result.is_ok());
    }

    #[test]
    fn test_composite_unknown_tool() {
        let mut composite = CompositeToolHandler::new();
        let handler = make_handler();
        composite.add_handler(Box::new(handler));
        let result = composite.handle_tool_call("nonexistent", &json!({}));
        assert!(matches!(result.unwrap_err(), McpError::ToolNotFound(_)));
    }
}
