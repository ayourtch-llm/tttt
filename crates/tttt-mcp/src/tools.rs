use serde_json::{json, Value};

/// Returns the tool definitions for all PTY management tools.
pub fn pty_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tttt_pty_launch",
            "description": "Launch a new terminal session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command to run (default: shell)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Command arguments" },
                    "working_dir": { "type": "string", "description": "Working directory" },
                    "cols": { "type": "integer", "description": "Terminal width (default: 80)" },
                    "rows": { "type": "integer", "description": "Terminal height (default: 24)" },
                    "sandbox_profile": {
                        "type": "string",
                        "description": "Sandbox profile: 'none', 'read_only_worktree', 'read_write_worktree', 'own_worktree'",
                        "enum": ["none", "read_only_worktree", "read_write_worktree", "own_worktree"]
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional unique name for the session (can be used instead of session_id in other tools)"
                    }
                }
            }
        }),
        json!({
            "name": "tttt_pty_send_keys",
            "description": "Send keystrokes to a terminal session. Keys are sent as-is. Use [ENTER] to submit. IMPORTANT: When sending multi-line text to Claude Code or similar TUI apps, send the text and [ENTER] as TWO SEPARATE calls — the app buffers pasted multi-line text and needs a separate Enter to submit it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "keys": { "type": "string", "description": "Keys to send (supports [UP], [ENTER], [CTRL+C], ^C, etc.)" }
                },
                "required": ["session_id", "keys"]
            }
        }),
        json!({
            "name": "tttt_pty_get_screen",
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
            "name": "tttt_pty_list",
            "description": "List all terminal sessions",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "tttt_pty_kill",
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
            "name": "tttt_pty_get_cursor",
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
            "name": "tttt_pty_resize",
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
            "name": "tttt_pty_set_scrollback",
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
        json!({
            "name": "tttt_pty_get_scrollback",
            "description": "Get scrollback buffer contents (text that has scrolled off the visible screen)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "lines": { "type": "integer", "description": "Max lines to return (default: 100)" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "tttt_pty_wait_for",
            "description": "Block until a regex pattern appears in the session's screen content, or timeout",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "pattern": { "type": "string", "description": "Regex pattern to match against screen content" },
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default: 30000)" }
                },
                "required": ["session_id", "pattern"]
            }
        }),
        json!({
            "name": "tttt_pty_wait_for_idle",
            "description": "Block until a session has produced no output for idle_seconds, or timeout expires. Returns status 'idle' or 'timeout'. If ignore_pattern is provided, idle is detected by screen content hash (excluding text matching the pattern) rather than raw output silence — useful when timestamps or other noise keep updating.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" },
                    "idle_seconds": { "type": "number", "description": "Seconds of silence to consider idle (default: 10)" },
                    "timeout": { "type": "number", "description": "Max seconds to wait before returning timeout (default: 300)" },
                    "ignore_pattern": { "type": "string", "description": "Optional regex: text matching this pattern is stripped before idle detection (e.g. '\\\\d{2}:\\\\d{2}:\\\\d{2}' to ignore HH:MM:SS timestamps)" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "tttt_pty_start_capture",
            "description": "Begin capturing raw PTY output (including ANSI sequences) to a temp file. Only one capture per session at a time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "tttt_pty_stop_capture",
            "description": "Stop capturing raw PTY output. Returns the file path and number of bytes written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session ID" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "tttt_pty_handle_rate_limit",
            "description": "Detect a Claude Code rate limit dialog, wait until the limit resets, then auto-continue. Blocks until the session is resumed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session to check and handle" },
                    "safety_margin_minutes": { "type": "number", "description": "Extra minutes to wait after reset time (default: 15)" },
                    "continuation_prompt": { "type": "string", "description": "Text to inject after resuming (default: 'Continue from where you left off.')" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "tttt_get_status",
            "description": "Get a dashboard summary of all sessions and system state. Returns session list with last output line and idle time, plus counts of pending reminders and active watchers.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

/// Returns the tool definition for the sidebar message tool.
pub fn sidebar_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tttt_sidebar_message",
            "description": "Display a message in the tttt sidebar REMINDERS section. Messages persist until cleared. Maximum 10 messages (oldest dropped when full).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Message to display in the sidebar" },
                    "clear": { "type": "boolean", "description": "If true, clear all sidebar messages (message param ignored)" }
                }
            }
        }),
        json!({
            "name": "tttt_sidebar_list",
            "description": "Return the current sidebar messages as a JSON array.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

/// Returns tool definitions for scheduler tools.
pub fn scheduler_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tttt_reminder_set",
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
            "name": "tttt_cron_create",
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
            "name": "tttt_cron_list",
            "description": "List all cron jobs",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "tttt_cron_delete",
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

/// Returns tool definitions for notification and self-management tools.
pub fn notification_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tttt_notify_on_prompt",
            "description": "Register a one-shot notification: when the target session's screen matches the pattern, inject text into the specified session and auto-submit (Enter). Eliminates polling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "watch_session_id": { "type": "string", "description": "Session to watch" },
                    "pattern": { "type": "string", "description": "Regex pattern to match against screen content" },
                    "inject_text": { "type": "string", "description": "Text to inject when pattern matches" },
                    "inject_session_id": { "type": "string", "description": "Session to inject into" }
                },
                "required": ["watch_session_id", "pattern", "inject_text", "inject_session_id"]
            }
        }),
        json!({
            "name": "tttt_notify_on_pattern",
            "description": "Register a recurring notification: fires every time the pattern matches (not removed after firing). Auto-submits (Enter) after injection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "watch_session_id": { "type": "string", "description": "Session to watch" },
                    "pattern": { "type": "string", "description": "Regex pattern to match" },
                    "inject_text": { "type": "string", "description": "Text to inject on match" },
                    "inject_session_id": { "type": "string", "description": "Session to inject into" }
                },
                "required": ["watch_session_id", "pattern", "inject_text", "inject_session_id"]
            }
        }),
        json!({
            "name": "tttt_notify_cancel",
            "description": "Cancel a registered notification watcher.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "watcher_id": { "type": "string", "description": "ID of the watcher to cancel" }
                },
                "required": ["watcher_id"]
            }
        }),
        json!({
            "name": "tttt_notify_list",
            "description": "List all active notification watchers.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "tttt_self_inject",
            "description": "Inject text into a session's PTY stdin and auto-submit (Enter). Can be used to inject commands, /compact, reminders, etc.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session to inject into" },
                    "text": { "type": "string", "description": "Text to inject" }
                },
                "required": ["session_id", "text"]
            }
        }),
    ]
}

/// Returns tool definitions for session replay tools.
pub fn replay_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tttt_replay_list_sessions",
            "description": "List all recorded terminal sessions available for replay",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        json!({
            "name": "tttt_replay_get_screen",
            "description": "Replay a recorded session and return the terminal screen at a given point",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session ID to replay" },
                    "event_index": { "type": "integer", "description": "Replay up to this event index (exclusive)" },
                    "timestamp_ms": { "type": "integer", "description": "Replay up to this timestamp in milliseconds" }
                },
                "required": ["session_id"]
            }
        }),
        json!({
            "name": "tttt_replay_get_timeline",
            "description": "Get the event timeline for a recorded session (index, timestamp, direction for each event)",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Session ID" }
                },
                "required": ["session_id"]
            }
        }),
    ]
}

/// Returns tool definitions for scratchpad tools.
pub fn scratchpad_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tttt_scratchpad_write",
            "description": "Write or append content to a named scratchpad key. Use for private working notes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Scratchpad key name" },
                    "content": { "type": "string", "description": "Content to write" },
                    "append": { "type": "boolean", "description": "Append to existing content instead of overwriting (default: false)" }
                },
                "required": ["key", "content"]
            }
        }),
        json!({
            "name": "tttt_scratchpad_read",
            "description": "Read content from a named scratchpad key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Scratchpad key name" }
                },
                "required": ["key"]
            }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pty_tool_count() {
        assert_eq!(pty_tool_definitions().len(), 15);
    }

    #[test]
    fn test_sidebar_tool_count() {
        assert_eq!(sidebar_tool_definitions().len(), 2);
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
        let send_keys = tools.iter().find(|t| t["name"] == "tttt_pty_send_keys").unwrap();
        let required = send_keys["inputSchema"]["required"].as_array().unwrap();
        assert!(required.contains(&Value::from("session_id")));
        assert!(required.contains(&Value::from("keys")));
    }

    #[test]
    fn test_pty_launch_no_required_params() {
        let tools = pty_tool_definitions();
        let launch = tools.iter().find(|t| t["name"] == "tttt_pty_launch").unwrap();
        assert!(launch["inputSchema"]["required"].is_null());
    }

    #[test]
    fn test_notification_tool_count() {
        assert_eq!(notification_tool_definitions().len(), 5);
    }

    #[test]
    fn test_sidebar_message_tool_present() {
        let tools = sidebar_tool_definitions();
        let tool = tools.iter().find(|t| t["name"] == "tttt_sidebar_message").unwrap();
        assert!(tool["inputSchema"]["properties"]["message"].is_object());
        assert!(tool["inputSchema"]["properties"]["clear"].is_object());
    }

    #[test]
    fn test_replay_tool_count() {
        assert_eq!(replay_tool_definitions().len(), 3);
    }

    #[test]
    fn test_replay_get_screen_required_params() {
        let tools = replay_tool_definitions();
        let tool = tools.iter().find(|t| t["name"] == "tttt_replay_get_screen").unwrap();
        let required = tool["inputSchema"]["required"].as_array().unwrap();
        assert!(required.contains(&Value::from("session_id")));
    }

    #[test]
    fn test_replay_list_sessions_no_required() {
        let tools = replay_tool_definitions();
        let tool = tools.iter().find(|t| t["name"] == "tttt_replay_list_sessions").unwrap();
        assert!(tool["inputSchema"]["required"].is_null());
    }

    #[test]
    fn test_tool_names_unique() {
        let all: Vec<Value> = pty_tool_definitions()
            .into_iter()
            .chain(scheduler_tool_definitions())
            .chain(notification_tool_definitions())
            .chain(scratchpad_tool_definitions())
            .chain(sidebar_tool_definitions())
            .chain(replay_tool_definitions())
            .collect();
        let names: Vec<&str> = all.iter().map(|t| t["name"].as_str().unwrap()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "duplicate tool names found");
    }

    #[test]
    fn test_scratchpad_tool_definitions_count() {
        assert_eq!(scratchpad_tool_definitions().len(), 2);
    }

}
