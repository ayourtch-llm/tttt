use serde_json::{json, Value};

/// Returns the tool definitions for all PTY management tools.
pub fn pty_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "pty_launch",
            "description": "Launch a new terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command to run (default: shell)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments" },
                    "working_dir": { "type": "string", "description": "Working directory" },
                    "cols": { "type": "integer", "description": "Terminal width (default: 80)" },
                    "rows": { "type": "integer", "description": "Terminal height (default: 24)" }
                }
            }
        }),
        json!({
            "name": "pty_send_keys",
            "description": "Send keystrokes to a terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "keys": { "type": "string", "description": "Keys to send (supports [UP], ^C, etc.)" },
                    "raw": { "type": "boolean", "description": "If true, don't append newline (default: false)" }
                },
                "required": ["session_id", "keys"]
            }
        }),
        json!({
            "name": "pty_get_screen",
            "description": "Get the current screen contents of a terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "pty_list",
            "description": "List all terminal sessions",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "pty_kill",
            "description": "Kill a terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session to kill" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "pty_get_cursor",
            "description": "Get the cursor position in a terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "pty_resize",
            "description": "Resize a terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "cols": { "type": "integer", "description": "New width" },
                    "rows": { "type": "integer", "description": "New height" }
                },
                "required": ["session_id", "cols", "rows"]
            }
        }),
        json!({
            "name": "pty_set_scrollback",
            "description": "Set the scrollback buffer size for a terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "lines": { "type": "integer", "description": "Scrollback buffer size in lines" }
                },
                "required": ["session_id", "lines"]
            }
        }),
    ]
}

/// Returns tool definitions for scheduler tools.
pub fn scheduler_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "reminder_set",
            "description": "Set a one-shot reminder that will be injected at a future time",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Reminder message" },
                    "delay_seconds": { "type": "integer", "description": "Seconds from now" }
                },
                "required": ["message", "delay_seconds"]
            }
        }),
        json!({
            "name": "cron_create",
            "description": "Create a recurring cron job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": { "type": "string", "description": "Cron expression (e.g., '*/5 * * * *')" },
                    "command": { "type": "string", "description": "Command or message to execute/inject" },
                    "session_id": { "type": "string", "description": "Optional target session" }
                },
                "required": ["expression", "command"]
            }
        }),
        json!({
            "name": "cron_list",
            "description": "List all cron jobs",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "cron_delete",
            "description": "Delete a cron job",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "ID of the cron job to delete" }
                },
                "required": ["job_id"]
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pty_tool_count() {
        assert_eq!(pty_tool_definitions().len(), 8);
    }

    #[test]
    fn test_scheduler_tool_count() {
        assert_eq!(scheduler_tool_definitions().len(), 4);
    }

    #[test]
    fn test_all_tools_have_name_and_description() {
        let all: Vec<Value> = pty_tool_definitions()
            .into_iter()
            .chain(scheduler_tool_definitions())
            .collect();
        for tool in &all {
            assert!(tool["name"].is_string(), "tool missing name: {:?}", tool);
            assert!(tool["description"].is_string(), "tool missing description: {:?}", tool);
            assert!(tool["inputSchema"].is_object(), "tool missing inputSchema: {:?}", tool);
        }
    }

    #[test]
    fn test_required_params_present() {
        let tools = pty_tool_definitions();
        let send_keys = tools.iter().find(|t| t["name"] == "pty_send_keys").unwrap();
        let required = send_keys["inputSchema"]["required"].as_array().unwrap();
        assert!(required.contains(&Value::from("session_id")));
        assert!(required.contains(&Value::from("keys")));
    }

    #[test]
    fn test_pty_launch_no_required_params() {
        let tools = pty_tool_definitions();
        let launch = tools.iter().find(|t| t["name"] == "pty_launch").unwrap();
        assert!(launch["inputSchema"]["required"].is_null());
    }

    #[test]
    fn test_tool_names_unique() {
        let all: Vec<Value> = pty_tool_definitions()
            .into_iter()
            .chain(scheduler_tool_definitions())
            .collect();
        let names: Vec<&str> = all.iter().map(|t| t["name"].as_str().unwrap()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "duplicate tool names found");
    }
}
