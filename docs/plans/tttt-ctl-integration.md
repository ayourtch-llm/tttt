# Plan: Integrate tttt-ctl into tttt binary

## Goal
Merge the standalone `tttt-ctl` CLI bridge (from `tmp/orchestration-pack/tttt-ctl-rs/`)
into the main `tttt` binary, accessible via:
1. **argv[0] dispatch**: `ln -s tttt tttt-ctl` then `tttt-ctl list` works
2. **Subcommand**: `tttt ctl list` also works

## Architecture

### New module: `src/ctl.rs`
A self-contained module in the main `tttt` crate that implements the ctl functionality.

**Key design decisions:**
- Use `serde_json` (already a dependency) instead of the hand-rolled JSON parser
- Use `clap` (already a dependency) for argument parsing instead of manual `args[i]` matching
- Reuse the existing `find_tttt_socket()` logic pattern from `main.rs` (adapted for MCP sockets)
- Keep the length-prefixed binary protocol (4-byte BE u32 + JSON-RPC) as-is — it matches the MCP server

### Changes to `src/main.rs`
- Add `mod ctl;`
- Before `Cli::parse()`, check argv[0] for `tttt-ctl` — if matched, delegate to `ctl::main()`
- Add `Ctl` variant to the `Commands` enum so `tttt ctl ...` also works

## Module Structure: `src/ctl.rs`

### Types (clap-derived)
```
CtlCli
  └── CtlCommand (enum)
       ├── Launch { command, workdir, name }
       ├── Send { session, text/enter/keys/file }
       ├── Screen { session }
       ├── Scrollback { session, lines }
       ├── List
       ├── Status
       ├── Kill { session_or_all }
       ├── Resize { session, rows, cols }
       ├── Wait { session, pattern, file, timeout, poll }
       ├── WaitIdle { session, idle, timeout }
       ├── HandleRateLimit { session, margin }
       ├── Notify (subcommand)
       │    ├── OnPattern { watch, pattern, inject, target }
       │    ├── OnPrompt { watch, pattern, inject, target }
       │    ├── List
       │    └── Cancel { watcher_id }
       ├── HasSession { session }
       └── SocketPath
```

### Core functions (from tttt-ctl-rs, modernized)
- `find_mcp_socket() -> Result<PathBuf>` — env var then glob `/tmp/tttt-mcp-*.sock`
- `McpConnection` struct wrapping UnixStream
  - `connect(path) -> Result<Self>` — connect + initialize handshake
  - `call_tool(name, args: serde_json::Value) -> Result<serde_json::Value>`
  - `send_msg/recv_msg` — length-prefixed protocol
- `extract_text(response) -> String` — pull text from MCP tool result
- `parse_session_id(s) -> Result<u32>` — validate "pty-N" or "N"
- Individual `cmd_*` functions per command

## Implementation Tasks

### Task 1: Create `src/ctl.rs` with clap CLI + MCP connection + tests
- CtlCli / CtlCommand clap structs
- McpConnection (connect, send_msg, recv_msg, call_tool, extract_text)
- parse_session_id with tests
- json_escape (if needed beyond serde) with tests
- Unit tests for parse_session_id, extract_text

### Task 2: Implement all ctl commands + tests
- All cmd_* functions that use McpConnection to call tools
- The `run()` entry point that matches on CtlCommand
- Integration-style tests where possible (mock stream tests)

### Task 3: Wire into main.rs
- Add `mod ctl;` to main.rs
- argv[0] check before Cli::parse()
- Add `Ctl` subcommand to Commands enum
- Symlink creation note in help text

## Testing Strategy
- Unit tests for pure functions: parse_session_id, extract_text, json_escape
- McpConnection protocol tests using UnixStream pairs (std::os::unix::net::UnixStream::pair())
- Command dispatch tests verifying correct tool names and argument serialization
- argv[0] detection test

## File Changes Summary
- **New**: `src/ctl.rs` (~500-600 lines)
- **Modified**: `src/main.rs` (~20 lines added)
