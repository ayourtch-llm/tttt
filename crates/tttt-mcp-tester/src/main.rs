//! MCP Tester: Interactive tool for testing MCP servers
//!
//! This binary launches an MCP server and allows interactive testing via stdin/stdout.
//! It implements the MCP protocol correctly, handling:
//! - initialize/initialized handshake
//! - tools/list
//! - tools/call
//! - ping
//!
//! Usage:
//!   mcp-tester                         # Use current directory as workdir
//!   mcp-tester --workdir /some/path    # Use specific workdir
//!
//! Then send JSON-RPC 2.0 requests line by line:
//!   echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | mcp-tester
//!   echo '{"jsonrpc":"2.0","method":"initialized"}' | mcp-tester
//!   echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' | mcp-tester
//!   echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{}}}' | mcp-tester

use clap::Parser;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use tttt_mcp::{
    CompositeToolHandler, McpServer, PtyToolHandler, SchedulerToolHandler,
};
use tttt_pty::RealPty;
use tttt_scheduler::Scheduler;

#[derive(Parser)]
#[command(name = "mcp-tester")]
#[command(about = "Interactive MCP server tester")]
struct Cli {
    /// Working directory for PTY sessions
    #[arg(short, long)]
    workdir: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let work_dir = cli.workdir.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });

    // Initialize tool handlers
    let manager = tttt_pty::SessionManager::<RealPty>::new();
    let pty_handler = PtyToolHandler::new_owned(manager, work_dir);

    let scheduler = Scheduler::new();
    let scheduler_handler = SchedulerToolHandler::new_owned(scheduler);

    // Combine handlers
    let mut composite = CompositeToolHandler::new();
    composite.add_handler(Box::new(pty_handler));
    composite.add_handler(Box::new(scheduler_handler));

    // Set up stdio
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let reader = BufReader::new(stdin);

    // Create and run server
    let mut server = McpServer::new(reader, stdout, composite);

    if cli.verbose {
        eprintln!("MCP tester ready. Send JSON-RPC 2.0 requests, one per line.");
        eprintln!("Example: echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{}}}}' | mcp-tester");
    }

    if let Err(e) = server.run() {
        eprintln!("MCP server error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_initialize_handshake() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let reader = BufReader::new(Cursor::new(format!("{}\n", input).into_bytes()));
        let mut output: Vec<u8> = Vec::new();

        let manager = tttt_pty::SessionManager::<tttt_pty::MockPty>::new();
        let pty_handler = PtyToolHandler::new_owned(manager, PathBuf::from("/tmp"));
        let scheduler = Scheduler::new();
        let scheduler_handler = SchedulerToolHandler::new_owned(scheduler);

        let mut composite = CompositeToolHandler::new();
        composite.add_handler(Box::new(pty_handler));
        composite.add_handler(Box::new(scheduler_handler));

        let mut server = McpServer::new(reader, &mut output, composite);
        server.run().unwrap();

        let response_str = String::from_utf8(output).unwrap();
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(response["result"]["serverInfo"]["name"], "tttt");
    }

    #[test]
    fn test_tools_list() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let reader = BufReader::new(Cursor::new(format!("{}\n", input).into_bytes()));
        let mut output: Vec<u8> = Vec::new();

        let manager = tttt_pty::SessionManager::<tttt_pty::MockPty>::new();
        let pty_handler = PtyToolHandler::new_owned(manager, PathBuf::from("/tmp"));
        let scheduler = Scheduler::new();
        let scheduler_handler = SchedulerToolHandler::new_owned(scheduler);

        let mut composite = CompositeToolHandler::new();
        composite.add_handler(Box::new(pty_handler));
        composite.add_handler(Box::new(scheduler_handler));

        let mut server = McpServer::new(reader, &mut output, composite);
        server.run().unwrap();

        let response_str = String::from_utf8(output).unwrap();
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["id"], 1);
        assert!(response["result"]["tools"].is_array());
    }

    #[test]
    fn test_full_handshake_and_tool_call() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tttt_pty_launch","arguments":{}}}"#,
            "\n",
        );
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let mut output: Vec<u8> = Vec::new();

        let manager = tttt_pty::SessionManager::<tttt_pty::MockPty>::new();
        let pty_handler = PtyToolHandler::new_owned(manager, PathBuf::from("/tmp"));
        let scheduler = Scheduler::new();
        let scheduler_handler = SchedulerToolHandler::new_owned(scheduler);

        let mut composite = CompositeToolHandler::new();
        composite.add_handler(Box::new(pty_handler));
        composite.add_handler(Box::new(scheduler_handler));

        let mut server = McpServer::new(reader, &mut output, composite);
        server.run().unwrap();

        let response_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = response_str.lines().collect();

        // Should have 2 responses (initialize and tools/call, initialized is notification)
        assert_eq!(lines.len(), 2);

        let init_response: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(init_response["id"], 1);
        assert_eq!(init_response["result"]["protocolVersion"], "2024-11-05");

        let launch_response: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(launch_response["id"], 2);
        let content_text = launch_response["result"]["content"][0]["text"].as_str().unwrap();
        let launch_result: Value = serde_json::from_str(content_text).unwrap();
        assert_eq!(launch_result["session_id"], "pty-1");
    }
}