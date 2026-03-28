//! ctl — CLI bridge for tttt's MCP server.
//!
//! Implements all tttt-ctl functionality using clap derive macros and serde_json.
//! Connect to a running tttt instance via Unix socket and send JSON-RPC tool calls.
//!
//! Protocol: length-prefixed binary (4-byte BE u32 + JSON-RPC payload).

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

// ── Global request-id counter ─────────────────────────────────────────────────

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1
}

// ── Error type ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum CtlError {
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidSessionId(String),
    SocketNotFound,
    Other(String),
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlError::Io(e) => write!(f, "IO error: {e}"),
            CtlError::Json(e) => write!(f, "JSON error: {e}"),
            CtlError::InvalidSessionId(s) => write!(f, "invalid session ID '{s}' (expected 'pty-N' or integer N)"),
            CtlError::SocketNotFound => write!(f, "no tttt MCP socket found. Set TTTT_MCP_SOCKET or ensure tttt is running"),
            CtlError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for CtlError {}

impl From<std::io::Error> for CtlError {
    fn from(e: std::io::Error) -> Self {
        CtlError::Io(e)
    }
}

impl From<serde_json::Error> for CtlError {
    fn from(e: serde_json::Error) -> Self {
        CtlError::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, CtlError>;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "tttt-ctl", about = "CLI bridge for tttt MCP server")]
pub struct CtlCli {
    #[command(subcommand)]
    pub command: CtlCommand,
}

#[derive(Subcommand)]
pub enum CtlCommand {
    /// Launch a new PTY session
    Launch {
        /// Command to run (defaults to /bin/sh)
        command: Option<String>,
        #[arg(short, long)]
        workdir: Option<String>,
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Send text or keys to a session
    Send {
        /// Session ID (e.g., "pty-1" or "1")
        session: String,
        /// Text to send
        text: Option<String>,
        /// Send Enter key
        #[arg(long)]
        enter: bool,
        /// Send special keys (e.g., "[CTRL+C]")
        #[arg(long)]
        keys: Option<String>,
        /// Send file contents
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Get the current screen contents
    Screen {
        /// Session ID
        session: String,
    },
    /// Get scrollback buffer
    Scrollback {
        /// Session ID
        session: String,
        /// Number of lines to retrieve
        #[arg(long, default_value = "100")]
        lines: u64,
    },
    /// List all sessions
    List,
    /// Get tttt status
    Status,
    /// Kill a session
    Kill {
        /// Session ID (or use --all)
        session: Option<String>,
        /// Kill all non-root sessions
        #[arg(long)]
        all: bool,
    },
    /// Resize a session
    Resize {
        /// Session ID
        session: String,
        /// Number of rows
        rows: u32,
        /// Number of columns
        cols: u32,
    },
    /// Wait for a pattern or file
    Wait {
        /// Session ID
        session: String,
        /// Pattern to wait for on screen
        #[arg(long)]
        pattern: Option<String>,
        /// File glob to wait for
        #[arg(long)]
        file: Option<String>,
        /// Timeout in seconds
        #[arg(long, default_value = "300")]
        timeout: u64,
        /// Poll interval in seconds
        #[arg(long, default_value = "5")]
        poll: u64,
    },
    /// Wait for session to become idle
    WaitIdle {
        /// Session ID
        session: String,
        /// Idle duration in seconds
        #[arg(long, default_value = "10")]
        idle: u64,
        /// Timeout in seconds
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Handle rate limit for a session
    HandleRateLimit {
        /// Session ID
        session: String,
        /// Safety margin in minutes
        #[arg(long, default_value = "15")]
        margin: u64,
    },
    /// Manage notifications
    Notify {
        #[command(subcommand)]
        subcommand: NotifyCommand,
    },
    /// Check if a session exists (exits 0 if yes, 1 if no)
    HasSession {
        /// Session ID
        session: String,
    },
    /// Print MCP socket path
    SocketPath,
}

#[derive(Subcommand)]
pub enum NotifyCommand {
    /// Register a pattern-based notification
    OnPattern {
        /// Session ID to watch
        #[arg(long)]
        watch: String,
        /// Pattern to match
        #[arg(long)]
        pattern: String,
        /// Text to inject when pattern matches
        #[arg(long)]
        inject: String,
        /// Session ID to inject into
        #[arg(long)]
        target: String,
    },
    /// Register a prompt-based notification
    OnPrompt {
        /// Session ID to watch
        #[arg(long)]
        watch: String,
        /// Pattern to match
        #[arg(long)]
        pattern: String,
        /// Text to inject when pattern matches
        #[arg(long)]
        inject: String,
        /// Session ID to inject into
        #[arg(long)]
        target: String,
    },
    /// List all active notifications
    List,
    /// Cancel a notification by ID
    Cancel {
        /// Watcher ID to cancel
        watcher_id: u64,
    },
}

// ── Socket discovery ──────────────────────────────────────────────────────────

pub fn find_mcp_socket() -> Result<PathBuf> {
    // 1. TTTT_MCP_SOCKET env var
    if let Ok(p) = env::var("TTTT_MCP_SOCKET") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        eprintln!("WARNING: TTTT_MCP_SOCKET={p} does not exist");
    }

    // 2. Auto-detect /tmp/tttt-mcp-*.sock
    let mut candidates: Vec<PathBuf> = std::fs::read_dir("/tmp")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("tttt-mcp-") && s.ends_with(".sock")
        })
        .map(|e| e.path())
        .collect();

    if candidates.is_empty() {
        return Err(CtlError::SocketNotFound);
    }

    if candidates.len() > 1 {
        // Pick most recently modified
        candidates.sort_by(|a, b| {
            let ma = a.metadata().and_then(|m| m.modified()).ok();
            let mb = b.metadata().and_then(|m| m.modified()).ok();
            mb.cmp(&ma)
        });
    }

    Ok(candidates.remove(0))
}

// ── Parse session ID ──────────────────────────────────────────────────────────

/// Parse "pty-N" or "N" into a u32. Returns error on invalid input.
pub fn parse_session_id(s: &str) -> Result<u32> {
    let id = s.strip_prefix("pty-").unwrap_or(s);
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
        id.parse::<u32>().map_err(|e| CtlError::Other(e.to_string()))
    } else {
        Err(CtlError::InvalidSessionId(s.to_string()))
    }
}

// ── Extract text from MCP tool result ────────────────────────────────────────

/// Extract a human-readable string from an MCP JSON-RPC response.
pub fn extract_text(resp: &Value) -> String {
    // Try result.content[0].text (standard tool result format)
    if let Some(result) = resp.get("result") {
        if let Some(text) = result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
        {
            return text.to_string();
        }
        // Try result.screen (get_screen format)
        if let Some(screen) = result.get("screen").and_then(|s| s.as_str()) {
            return screen.to_string();
        }
        return result.to_string();
    }
    // Error response
    if let Some(msg) = resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return format!("ERROR: {msg}");
    }
    resp.to_string()
}

// ── MCP connection ────────────────────────────────────────────────────────────

pub struct McpConnection {
    stream: UnixStream,
}

impl McpConnection {
    /// Connect to the MCP server at `path` and perform the initialize handshake.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

        let init = json!({
            "jsonrpc": "2.0",
            "id": next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "tttt-ctl", "version": "1.0"}
            }
        });
        let init_bytes = serde_json::to_vec(&init)?;
        let mut conn = McpConnection { stream };
        conn.send_msg(&init_bytes)?;
        conn.recv_msg()?; // ignore init response
        Ok(conn)
    }

    /// Send a length-prefixed message (4-byte BE u32 + payload).
    pub fn send_msg(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let len = (payload.len() as u32).to_be_bytes();
        self.stream.write_all(&len)?;
        self.stream.write_all(payload)?;
        self.stream.flush()
    }

    /// Receive a length-prefixed message.
    pub fn recv_msg(&mut self) -> std::io::Result<Vec<u8>> {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr)?;
        let length = u32::from_be_bytes(hdr) as usize;
        let mut buf = vec![0u8; length];
        self.stream.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Send a tools/call JSON-RPC request and return the parsed response.
    pub fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": next_id(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            }
        });
        let req_bytes = serde_json::to_vec(&req)?;
        self.send_msg(&req_bytes)?;
        let resp_bytes = self.recv_msg()?;
        let resp: Value = serde_json::from_slice(&resp_bytes)?;
        Ok(resp)
    }
}

// ── Command implementations ───────────────────────────────────────────────────

fn cmd_launch(conn: &mut McpConnection, command: Option<String>, workdir: Option<String>, name: Option<String>) {
    let mut args = json!({
        "command": command.as_deref().unwrap_or("/bin/sh")
    });
    if let Some(wd) = workdir {
        args["working_dir"] = json!(wd);
    }
    if let Some(n) = name {
        args["name"] = json!(n);
    }
    let resp = conn.call_tool("tttt_pty_launch", args).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    println!("{}", extract_text(&resp));
}

fn cmd_send(conn: &mut McpConnection, session: &str, text: Option<String>, enter: bool, keys: Option<String>, file: Option<PathBuf>) {
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });

    let key_str: String;
    let keys_to_send: &str;

    if enter {
        keys_to_send = "[ENTER]";
    } else if let Some(k) = &keys {
        keys_to_send = k.as_str();
    } else if let Some(f) = file {
        key_str = std::fs::read_to_string(&f).unwrap_or_else(|e| {
            eprintln!("ERROR: Cannot read {}: {e}", f.display());
            process::exit(1);
        });
        keys_to_send = &key_str;
    } else if let Some(t) = &text {
        keys_to_send = t.as_str();
    } else {
        eprintln!("ERROR: send requires text, --enter, --keys, or --file");
        process::exit(1);
    }

    let args = json!({"session_id": sid, "keys": keys_to_send});
    conn.call_tool("tttt_pty_send_keys", args).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
}

fn cmd_screen(conn: &mut McpConnection, session: &str) {
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let resp = conn.call_tool("tttt_pty_get_screen", json!({"session_id": sid})).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    println!("{}", extract_text(&resp));
}

fn cmd_scrollback(conn: &mut McpConnection, session: &str, lines: u64) {
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let resp = conn.call_tool("tttt_pty_get_scrollback", json!({"session_id": sid, "lines": lines})).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    println!("{}", extract_text(&resp));
}

fn cmd_list(conn: &mut McpConnection) {
    let resp = conn.call_tool("tttt_pty_list", json!({})).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    println!("{}", extract_text(&resp));
}

fn cmd_status(conn: &mut McpConnection) {
    let resp = conn.call_tool("tttt_get_status", json!({})).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    println!("{}", extract_text(&resp));
}

fn cmd_kill(conn: &mut McpConnection, session: Option<String>, all: bool) {
    if all {
        // Get list first, then kill each non-pty-0 session
        let resp = conn.call_tool("tttt_pty_list", json!({})).unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            process::exit(1);
        });
        let text = extract_text(&resp);
        for word in text.split_whitespace() {
            let token = word.trim_end_matches(':');
            if token.starts_with("pty-") && token != "pty-0" {
                let sid = match parse_session_id(token) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                match conn.call_tool("tttt_pty_kill", json!({"session_id": sid})) {
                    Ok(resp) => {
                        if resp.get("error").is_some() {
                            eprintln!("WARNING: failed to kill pty-{sid}: {}", extract_text(&resp));
                        } else {
                            println!("Killed pty-{sid}");
                        }
                    }
                    Err(e) => eprintln!("WARNING: failed to kill pty-{sid}: {e}"),
                }
            }
        }
    } else {
        let s = session.unwrap_or_else(|| {
            eprintln!("ERROR: kill requires SESSION_ID or --all");
            process::exit(1);
        });
        let sid = parse_session_id(&s).unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            process::exit(1);
        });
        conn.call_tool("tttt_pty_kill", json!({"session_id": sid})).unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            process::exit(1);
        });
        println!("Killed {s}");
    }
}

fn cmd_resize(conn: &mut McpConnection, session: &str, rows: u32, cols: u32) {
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    conn.call_tool("tttt_pty_resize", json!({"session_id": sid, "rows": rows, "cols": cols})).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    println!("Resized {session} to {rows}x{cols}");
}

/// Simple glob matching: supports * wildcard only.
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 2 {
        let prefix = parts[0];
        let suffix = parts[1];
        name.starts_with(prefix) && name.ends_with(suffix) && name.len() >= prefix.len() + suffix.len()
    } else {
        // Multiple wildcards
        let mut remaining = name;
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            if i == 0 {
                if !remaining.starts_with(part) {
                    return false;
                }
                remaining = &remaining[part.len()..];
            } else if i == parts.len() - 1 {
                if !remaining.ends_with(part) {
                    return false;
                }
            } else if let Some(pos) = remaining.find(part) {
                remaining = &remaining[pos + part.len()..];
            } else {
                return false;
            }
        }
        true
    }
}

fn cmd_wait(
    conn: &mut McpConnection,
    session: &str,
    pattern: Option<String>,
    file: Option<String>,
    timeout: u64,
    poll: u64,
) {
    if pattern.is_none() && file.is_none() {
        eprintln!("ERROR: wait requires --pattern or --file");
        process::exit(1);
    }

    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let deadline = Instant::now() + Duration::from_secs(timeout);

    if let Some(file_glob) = file {
        let start = std::time::SystemTime::now();
        let glob_path = Path::new(&file_glob);
        let parent = glob_path
            .parent()
            .map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p })
            .unwrap_or_else(|| Path::new("."));
        let file_pattern = glob_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "*".to_string());

        loop {
            match std::fs::read_dir(parent) {
                Err(e) => eprintln!("WARNING: Cannot read directory '{}': {e}", parent.display()),
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if glob_match(&file_pattern, &name) {
                            if let Ok(meta) = entry.metadata() {
                                if let Ok(modified) = meta.modified() {
                                    if modified >= start {
                                        println!("FOUND: {}", entry.path().display());
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if Instant::now() > deadline {
                eprintln!("TIMEOUT: No file matching '{file_glob}' after {timeout}s");
                process::exit(1);
            }
            std::thread::sleep(Duration::from_secs(poll));
        }
    } else {
        let pat = pattern.unwrap();
        loop {
            let resp = conn
                .call_tool("tttt_pty_get_screen", json!({"session_id": sid}))
                .unwrap_or_else(|e| {
                    eprintln!("ERROR: {e}");
                    process::exit(1);
                });
            let text = extract_text(&resp);
            if text.contains(pat.as_str()) {
                println!("Pattern '{pat}' found.");
                return;
            }
            if Instant::now() > deadline {
                eprintln!("TIMEOUT: pattern '{pat}' not found after {timeout}s");
                process::exit(1);
            }
            std::thread::sleep(Duration::from_secs(poll));
        }
    }
}

fn cmd_wait_idle(conn: &mut McpConnection, session: &str, idle: u64, timeout: u64) {
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let resp = conn
        .call_tool(
            "tttt_pty_wait_for_idle",
            json!({"session_id": sid, "idle_seconds": idle, "timeout": timeout}),
        )
        .unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            process::exit(1);
        });
    println!("{}", extract_text(&resp));
}

fn cmd_handle_rate_limit(conn: &mut McpConnection, session: &str, margin: u64) {
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let resp = conn
        .call_tool(
            "tttt_pty_handle_rate_limit",
            json!({"session_id": sid, "safety_margin_minutes": margin}),
        )
        .unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            process::exit(1);
        });
    println!("{}", extract_text(&resp));
}

fn cmd_notify(conn: &mut McpConnection, sub: NotifyCommand) {
    match sub {
        NotifyCommand::List => {
            let resp = conn.call_tool("tttt_notify_list", json!({})).unwrap_or_else(|e| {
                eprintln!("ERROR: {e}");
                process::exit(1);
            });
            println!("{}", extract_text(&resp));
        }
        NotifyCommand::Cancel { watcher_id } => {
            let resp = conn
                .call_tool("tttt_notify_cancel", json!({"watcher_id": watcher_id}))
                .unwrap_or_else(|e| {
                    eprintln!("ERROR: {e}");
                    process::exit(1);
                });
            println!("{}", extract_text(&resp));
        }
        NotifyCommand::OnPattern { watch, pattern, inject, target } => {
            let watch_sid = parse_session_id(&watch).unwrap_or_else(|e| { eprintln!("ERROR: {e}"); process::exit(1); });
            let target_sid = parse_session_id(&target).unwrap_or_else(|e| { eprintln!("ERROR: {e}"); process::exit(1); });
            let resp = conn
                .call_tool(
                    "tttt_notify_on_pattern",
                    json!({
                        "watch_session_id": watch_sid,
                        "pattern": pattern,
                        "inject_text": inject,
                        "inject_session_id": target_sid
                    }),
                )
                .unwrap_or_else(|e| { eprintln!("ERROR: {e}"); process::exit(1); });
            println!("{}", extract_text(&resp));
        }
        NotifyCommand::OnPrompt { watch, pattern, inject, target } => {
            let watch_sid = parse_session_id(&watch).unwrap_or_else(|e| { eprintln!("ERROR: {e}"); process::exit(1); });
            let target_sid = parse_session_id(&target).unwrap_or_else(|e| { eprintln!("ERROR: {e}"); process::exit(1); });
            let resp = conn
                .call_tool(
                    "tttt_notify_on_prompt",
                    json!({
                        "watch_session_id": watch_sid,
                        "pattern": pattern,
                        "inject_text": inject,
                        "inject_session_id": target_sid
                    }),
                )
                .unwrap_or_else(|e| { eprintln!("ERROR: {e}"); process::exit(1); });
            println!("{}", extract_text(&resp));
        }
    }
}

fn cmd_has_session(conn: &mut McpConnection, session: &str) {
    let resp = conn.call_tool("tttt_pty_list", json!({})).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let text = extract_text(&resp);
    let sid = parse_session_id(session).unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let needle = format!("pty-{sid}");
    // Word-boundary check: must be followed by ':', space, comma, or end
    let found = text
        .split(|c: char| c.is_whitespace() || c == ':' || c == ',')
        .any(|token| token == needle);
    if found {
        process::exit(0);
    } else {
        process::exit(1);
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run a single ctl command (used by both `tttt ctl ...` and `tttt-ctl ...`).
pub fn run_command(command: CtlCommand) -> ! {
    // socket-path doesn't need a connection
    if let CtlCommand::SocketPath = &command {
        let path = find_mcp_socket().unwrap_or_else(|e| {
            eprintln!("ERROR: {e}");
            process::exit(1);
        });
        println!("{}", path.display());
        process::exit(0);
    }

    let socket_path = find_mcp_socket().unwrap_or_else(|e| {
        eprintln!("ERROR: {e}");
        process::exit(1);
    });
    let mut conn = McpConnection::connect(&socket_path).unwrap_or_else(|e| {
        eprintln!("ERROR: Cannot connect to {}: {e}", socket_path.display());
        process::exit(1);
    });

    match command {
        CtlCommand::Launch { command, workdir, name } => cmd_launch(&mut conn, command, workdir, name),
        CtlCommand::Send { session, text, enter, keys, file } => cmd_send(&mut conn, &session, text, enter, keys, file),
        CtlCommand::Screen { session } => cmd_screen(&mut conn, &session),
        CtlCommand::Scrollback { session, lines } => cmd_scrollback(&mut conn, &session, lines),
        CtlCommand::List => cmd_list(&mut conn),
        CtlCommand::Status => cmd_status(&mut conn),
        CtlCommand::Kill { session, all } => cmd_kill(&mut conn, session, all),
        CtlCommand::Resize { session, rows, cols } => cmd_resize(&mut conn, &session, rows, cols),
        CtlCommand::Wait { session, pattern, file, timeout, poll } => {
            cmd_wait(&mut conn, &session, pattern, file, timeout, poll)
        }
        CtlCommand::WaitIdle { session, idle, timeout } => cmd_wait_idle(&mut conn, &session, idle, timeout),
        CtlCommand::HandleRateLimit { session, margin } => cmd_handle_rate_limit(&mut conn, &session, margin),
        CtlCommand::Notify { subcommand } => cmd_notify(&mut conn, subcommand),
        CtlCommand::HasSession { session } => cmd_has_session(&mut conn, &session),
        CtlCommand::SocketPath => unreachable!("handled above"),
    }

    process::exit(0);
}

/// Entry point when invoked as `tttt-ctl` (argv[0] dispatch).
pub fn main() -> ! {
    let cli = CtlCli::parse();
    run_command(cli.command);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    // ── parse_session_id ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_session_id_pty_prefix() {
        assert_eq!(parse_session_id("pty-1").unwrap(), 1);
        assert_eq!(parse_session_id("pty-0").unwrap(), 0);
        assert_eq!(parse_session_id("pty-42").unwrap(), 42);
    }

    #[test]
    fn test_parse_session_id_bare_number() {
        assert_eq!(parse_session_id("5").unwrap(), 5);
        assert_eq!(parse_session_id("0").unwrap(), 0);
        assert_eq!(parse_session_id("999").unwrap(), 999);
    }

    #[test]
    fn test_parse_session_id_invalid_letters() {
        assert!(parse_session_id("abc").is_err());
        assert!(parse_session_id("pty-abc").is_err());
    }

    #[test]
    fn test_parse_session_id_empty() {
        assert!(parse_session_id("").is_err());
        assert!(parse_session_id("pty-").is_err());
    }

    #[test]
    fn test_parse_session_id_negative_rejected() {
        // Negative numbers have '-' which is not ascii_digit
        assert!(parse_session_id("-1").is_err());
    }

    // ── extract_text ─────────────────────────────────────────────────────────

    #[test]
    fn test_extract_text_content_array() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{"type": "text", "text": "hello world"}]
            }
        });
        assert_eq!(extract_text(&resp), "hello world");
    }

    #[test]
    fn test_extract_text_screen_field() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "screen": "terminal output here"
            }
        });
        assert_eq!(extract_text(&resp), "terminal output here");
    }

    #[test]
    fn test_extract_text_error_response() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32000,
                "message": "session not found"
            }
        });
        assert_eq!(extract_text(&resp), "ERROR: session not found");
    }

    #[test]
    fn test_extract_text_raw_result_fallback() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "plain string result"
        });
        // Falls through to result.to_string()
        let text = extract_text(&resp);
        assert!(text.contains("plain string result"));
    }

    #[test]
    fn test_extract_text_empty_content_array() {
        let resp = json!({
            "result": {
                "content": []
            }
        });
        // Falls through to result.to_string()
        let text = extract_text(&resp);
        assert!(text.contains("content"));
    }

    #[test]
    fn test_extract_text_prefers_content_over_screen() {
        let resp = json!({
            "result": {
                "content": [{"type": "text", "text": "from content"}],
                "screen": "from screen"
            }
        });
        assert_eq!(extract_text(&resp), "from content");
    }

    // ── send_msg / recv_msg ──────────────────────────────────────────────────

    fn make_pair() -> (McpConnection, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let conn = McpConnection { stream: a };
        (conn, b)
    }

    #[test]
    fn test_send_msg_length_prefix() {
        let (mut conn, mut peer) = make_pair();
        let payload = b"hello";
        conn.send_msg(payload).unwrap();

        let mut hdr = [0u8; 4];
        peer.read_exact(&mut hdr).unwrap();
        assert_eq!(u32::from_be_bytes(hdr), 5);

        let mut body = vec![0u8; 5];
        peer.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"hello");
    }

    #[test]
    fn test_recv_msg_reads_length_prefixed() {
        let (mut conn, mut peer) = make_pair();
        let payload = b"world";
        let len = (payload.len() as u32).to_be_bytes();
        peer.write_all(&len).unwrap();
        peer.write_all(payload).unwrap();
        peer.flush().unwrap();

        let received = conn.recv_msg().unwrap();
        assert_eq!(&received, b"world");
    }

    #[test]
    fn test_send_recv_roundtrip() {
        let (mut conn, mut peer) = make_pair();
        let msg = b"roundtrip test";
        conn.send_msg(msg).unwrap();

        // Peer reads back
        let mut hdr = [0u8; 4];
        peer.read_exact(&mut hdr).unwrap();
        let len = u32::from_be_bytes(hdr) as usize;
        let mut body = vec![0u8; len];
        peer.read_exact(&mut body).unwrap();
        assert_eq!(&body, msg);
    }

    #[test]
    fn test_send_empty_payload() {
        let (mut conn, mut peer) = make_pair();
        conn.send_msg(&[]).unwrap();

        let mut hdr = [0u8; 4];
        peer.read_exact(&mut hdr).unwrap();
        assert_eq!(u32::from_be_bytes(hdr), 0);
    }

    // ── McpConnection initialize handshake ───────────────────────────────────

    #[test]
    fn test_mcp_connection_initialize_handshake() {
        let (server_side, client_side) = UnixStream::pair().unwrap();

        // Spawn a thread to play the server role
        let server_thread = std::thread::spawn(move || {
            let mut server = McpConnection { stream: server_side };
            // Read the initialize request
            let bytes = server.recv_msg().unwrap();
            let req: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(req["method"], "initialize");

            // Send back a minimal init response
            let resp = json!({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05"}});
            let resp_bytes = serde_json::to_vec(&resp).unwrap();
            server.send_msg(&resp_bytes).unwrap();
        });

        // Client: wrap the client_side stream into a McpConnection manually
        // (we can't use ::connect since there's no socket file here, use internal API)
        let mut client = McpConnection { stream: client_side };
        client.stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

        let init = json!({
            "jsonrpc": "2.0",
            "id": next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "tttt-ctl", "version": "1.0"}
            }
        });
        let init_bytes = serde_json::to_vec(&init).unwrap();
        client.send_msg(&init_bytes).unwrap();
        let _resp = client.recv_msg().unwrap(); // should succeed

        server_thread.join().unwrap();
    }

    // ── call_tool ────────────────────────────────────────────────────────────

    #[test]
    fn test_call_tool_sends_correct_json_rpc() {
        let (mut conn, mut peer) = make_pair();

        // Spawn a thread to be the server
        let server_thread = std::thread::spawn(move || {
            let mut hdr = [0u8; 4];
            peer.read_exact(&mut hdr).unwrap();
            let len = u32::from_be_bytes(hdr) as usize;
            let mut body = vec![0u8; len];
            peer.read_exact(&mut body).unwrap();
            let req: Value = serde_json::from_slice(&body).unwrap();

            // Validate request structure
            assert_eq!(req["jsonrpc"], "2.0");
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "tttt_pty_list");

            // Send a valid response back
            let resp = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "content": [{"type": "text", "text": "pty-0: shell\npty-1: agent"}]
                }
            });
            let resp_bytes = serde_json::to_vec(&resp).unwrap();
            let len = (resp_bytes.len() as u32).to_be_bytes();
            peer.write_all(&len).unwrap();
            peer.write_all(&resp_bytes).unwrap();
            peer.flush().unwrap();
        });

        let resp = conn.call_tool("tttt_pty_list", json!({})).unwrap();
        assert_eq!(extract_text(&resp), "pty-0: shell\npty-1: agent");
        server_thread.join().unwrap();
    }

    // ── glob_match ───────────────────────────────────────────────────────────

    #[test]
    fn test_glob_match_no_wildcard() {
        assert!(glob_match("foo.txt", "foo.txt"));
        assert!(!glob_match("foo.txt", "bar.txt"));
    }

    #[test]
    fn test_glob_match_prefix_suffix() {
        assert!(glob_match("*.txt", "hello.txt"));
        assert!(glob_match("foo*", "foobar"));
        assert!(!glob_match("*.txt", "hello.rs"));
    }

    #[test]
    fn test_glob_match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_multiple_wildcards() {
        assert!(glob_match("foo*bar*baz", "foo_bar_baz"));
        assert!(!glob_match("foo*bar*baz", "foo_baz"));
    }

    // ── CtlCli parsing ────────────────────────────────────────────────────────

    #[test]
    fn test_cli_parse_launch_default() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "launch"]).unwrap();
        if let CtlCommand::Launch { command, workdir, name } = cli.command {
            assert!(command.is_none());
            assert!(workdir.is_none());
            assert!(name.is_none());
        } else {
            panic!("expected Launch");
        }
    }

    #[test]
    fn test_cli_parse_launch_with_args() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "launch", "bash", "--workdir", "/tmp", "--name", "test"]).unwrap();
        if let CtlCommand::Launch { command, workdir, name } = cli.command {
            assert_eq!(command.as_deref(), Some("bash"));
            assert_eq!(workdir.as_deref(), Some("/tmp"));
            assert_eq!(name.as_deref(), Some("test"));
        } else {
            panic!("expected Launch");
        }
    }

    #[test]
    fn test_cli_parse_send_text() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "send", "pty-1", "hello"]).unwrap();
        if let CtlCommand::Send { session, text, enter, keys, file } = cli.command {
            assert_eq!(session, "pty-1");
            assert_eq!(text.as_deref(), Some("hello"));
            assert!(!enter);
            assert!(keys.is_none());
            assert!(file.is_none());
        } else {
            panic!("expected Send");
        }
    }

    #[test]
    fn test_cli_parse_send_enter() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "send", "1", "--enter"]).unwrap();
        if let CtlCommand::Send { enter, .. } = cli.command {
            assert!(enter);
        } else {
            panic!("expected Send");
        }
    }

    #[test]
    fn test_cli_parse_kill_all() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "kill", "--all"]).unwrap();
        if let CtlCommand::Kill { session, all } = cli.command {
            assert!(all);
            assert!(session.is_none());
        } else {
            panic!("expected Kill");
        }
    }

    #[test]
    fn test_cli_parse_kill_session() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "kill", "pty-2"]).unwrap();
        if let CtlCommand::Kill { session, all } = cli.command {
            assert!(!all);
            assert_eq!(session.as_deref(), Some("pty-2"));
        } else {
            panic!("expected Kill");
        }
    }

    #[test]
    fn test_cli_parse_resize() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "resize", "pty-1", "24", "80"]).unwrap();
        if let CtlCommand::Resize { session, rows, cols } = cli.command {
            assert_eq!(session, "pty-1");
            assert_eq!(rows, 24);
            assert_eq!(cols, 80);
        } else {
            panic!("expected Resize");
        }
    }

    #[test]
    fn test_cli_parse_wait_pattern() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "wait", "pty-1", "--pattern", "ready>"]).unwrap();
        if let CtlCommand::Wait { session, pattern, file, timeout, poll } = cli.command {
            assert_eq!(session, "pty-1");
            assert_eq!(pattern.as_deref(), Some("ready>"));
            assert!(file.is_none());
            assert_eq!(timeout, 300);
            assert_eq!(poll, 5);
        } else {
            panic!("expected Wait");
        }
    }

    #[test]
    fn test_cli_parse_notify_list() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "notify", "list"]).unwrap();
        if let CtlCommand::Notify { subcommand } = cli.command {
            assert!(matches!(subcommand, NotifyCommand::List));
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn test_cli_parse_notify_cancel() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "notify", "cancel", "42"]).unwrap();
        if let CtlCommand::Notify { subcommand } = cli.command {
            if let NotifyCommand::Cancel { watcher_id } = subcommand {
                assert_eq!(watcher_id, 42);
            } else {
                panic!("expected Cancel");
            }
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn test_cli_parse_notify_on_pattern() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from([
            "tttt-ctl", "notify", "on-pattern",
            "--watch", "pty-1", "--pattern", "error", "--inject", "retry", "--target", "pty-2",
        ]).unwrap();
        if let CtlCommand::Notify { subcommand } = cli.command {
            if let NotifyCommand::OnPattern { watch, pattern, inject, target } = subcommand {
                assert_eq!(watch, "pty-1");
                assert_eq!(pattern, "error");
                assert_eq!(inject, "retry");
                assert_eq!(target, "pty-2");
            } else {
                panic!("expected OnPattern");
            }
        } else {
            panic!("expected Notify");
        }
    }

    #[test]
    fn test_cli_parse_socket_path() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "socket-path"]).unwrap();
        assert!(matches!(cli.command, CtlCommand::SocketPath));
    }

    #[test]
    fn test_cli_parse_list() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "list"]).unwrap();
        assert!(matches!(cli.command, CtlCommand::List));
    }

    #[test]
    fn test_cli_parse_status() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "status"]).unwrap();
        assert!(matches!(cli.command, CtlCommand::Status));
    }

    #[test]
    fn test_cli_parse_has_session() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "has-session", "pty-3"]).unwrap();
        if let CtlCommand::HasSession { session } = cli.command {
            assert_eq!(session, "pty-3");
        } else {
            panic!("expected HasSession");
        }
    }

    #[test]
    fn test_cli_parse_scrollback_default_lines() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "scrollback", "pty-1"]).unwrap();
        if let CtlCommand::Scrollback { session, lines } = cli.command {
            assert_eq!(session, "pty-1");
            assert_eq!(lines, 100);
        } else {
            panic!("expected Scrollback");
        }
    }

    #[test]
    fn test_cli_parse_wait_idle_defaults() {
        use clap::Parser;
        let cli = CtlCli::try_parse_from(["tttt-ctl", "wait-idle", "pty-1"]).unwrap();
        if let CtlCommand::WaitIdle { session, idle, timeout } = cli.command {
            assert_eq!(session, "pty-1");
            assert_eq!(idle, 10);
            assert_eq!(timeout, 300);
        } else {
            panic!("expected WaitIdle");
        }
    }

    // ── next_id atomicity ────────────────────────────────────────────────────

    #[test]
    fn test_next_id_increments() {
        let a = next_id();
        let b = next_id();
        assert!(b > a);
    }
}
