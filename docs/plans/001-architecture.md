# tttt Architecture Plan

## Vision

An autonomous multiagent terminal harness where a root agent (e.g., Claude Code) orchestrates sandboxed child terminals via MCP tools, with a human-observable TUI.

## Current State (as of initial implementation)

### Workspace Crates

| Crate | Purpose | Tests |
|-------|---------|-------|
| `tttt-pty` | PTY session management: `PtyBackend` trait (MockPty/RealPty), `ScreenBuffer` (vt100), `PtySession`, `SessionManager` | 68 |
| `tttt-log` | Dual logging via `LogSink` trait: `TextLogger` (JSON-lines) + `SqliteLogger` | 25 |
| `tttt-mcp` | MCP JSON-RPC 2.0 server: `ToolHandler` trait, `PtyToolHandler`, `McpServer<R,W,H>`, 8 PTY + 4 scheduler tool definitions | 42 |
| `tttt-tui` | Terminal UI: `InputParser` (prefix-key state machine), `PaneRenderer` (cell-by-cell, zellij-style), `SidebarRenderer` | 43 |
| `tttt-scheduler` | Cron + reminders: `Scheduler` with `tick(now)` for testability | 21 |
| `tttt-sandbox` | OS sandboxing: `CapabilitySet` builder, `SandboxProfile` enum (placeholder for Landlock/Seatbelt) | 18 |

### Binary Modes

- `tttt` — TUI harness: poll-based event loop, left pane (PTY via PaneRenderer), right sidebar, prefix-key switching (Ctrl+\)
- `tttt mcp-server` — Standalone MCP server on stdio for AI agent integration

### Key Design Decisions

- **PaneRenderer** (zellij approach): Reads vt100 screen cells and writes each to explicit terminal positions. Avoids `contents_diff()` which breaks when PTY width != display width.
- **Shared SessionManager** (`Arc<Mutex<>>`): TUI and MCP server share sessions. MCP-created panes appear in sidebar immediately.
- **Non-blocking PTY reads**: Uses `nix::unistd::read` on raw fd with `O_NONBLOCK`, polled alongside stdin.
- **Working directory**: Defaults to current dir (not $HOME). Overridable per-session.

## What's Next

### In-process MCP server (pipes to root agent)
- TUI spawns root agent (claude) with MCP server config pointing to a pipe
- MCP server thread reads from pipe, operates on shared SessionManager
- Root agent's MCP tool calls create/manage panes visible in TUI

### Sandbox enforcement
- When `pty_launch` includes `sandbox_profile`, apply OS-level restrictions before exec
- Use nono's Landlock (Linux) / Seatbelt (macOS) via `tttt-sandbox`
- Profiles: `read_only_worktree`, `read_write_worktree`, `own_worktree`, `custom`
- Git worktree creation for `own_worktree` profile

### Scheduler integration with MCP
- Wire `reminder_set`, `cron_create/list/delete` MCP tools to `tttt-scheduler`
- Scheduler events inject text into target sessions or trigger MCP notifications

### Terminal I/O logging
- Every byte in/out of every PTY logged to both text files and SQLite
- SQLite enables replay, correlation, and querying across sessions

### Rendering improvements
- Synchronized output (CSI 2026) for flicker-free updates
- Render debouncing (10ms, like zellij) to batch rapid PTY output
