# Detailed Implementation Plan: Basic Features

## What we have (working, tested)

- TUI harness with poll-based event loop, PaneRenderer, sidebar, prefix-key switching
- PTY session management (create, send keys, read screen, kill, resize)
- MCP JSON-RPC 2.0 server on stdio (standalone mode: `tttt mcp-server`)
- Shared SessionManager (`Arc<Mutex<>>`) between TUI and MCP
- Dual logging (text + SQLite)
- Scheduler (cron + reminders, tick-based)
- Sandbox types (CapabilitySet builder, profiles — not yet enforced)
- Working directory support
- 292 tests

## What's missing (essential, in dependency order)

### Layer 1: The root agent must be able to USE the MCP tools

Right now, `tttt mcp-server` is a standalone binary on stdio. But the actual use case is:

1. tttt starts
2. tttt launches root agent (e.g., `claude --mcp-config /tmp/tttt-mcp.json`) in a PTY
3. The root agent connects to tttt's MCP server
4. Root agent calls `pty_launch` → tttt creates a new session → visible in sidebar
5. Root agent calls `pty_send_keys` → keystrokes go to the child session
6. Root agent calls `pty_get_screen` → reads the child session's screen
7. Human sees everything in the TUI, switches between sessions with prefix key

**The missing piece is step 2-3: how does the root agent connect to tttt's MCP server?**

#### Option A: Subprocess MCP server (recommended for Claude Code)

Claude Code discovers MCP servers from its config. When tttt launches Claude Code, it:

1. Writes a temporary MCP config file:
   ```json
   {
     "mcpServers": {
       "tttt": {
         "command": "/path/to/tttt",
         "args": ["mcp-server", "--workdir", "/path/to/project"]
       }
     }
   }
   ```
2. Launches Claude Code with: `claude --mcp-config /tmp/tttt-mcp-XXXX.json`
3. Claude Code spawns `tttt mcp-server` as a child process
4. Claude Code communicates with it via stdio pipes

**Problem:** This `tttt mcp-server` subprocess has its OWN SessionManager, not the TUI's shared one. Sessions created here are invisible to the TUI.

**Solution:** The subprocess MCP server must connect BACK to the TUI process to share state. Two approaches:

##### A1: Unix socket IPC

- TUI listens on a Unix socket (e.g., `/tmp/tttt-XXXX.sock`)
- `tttt mcp-server` connects to this socket
- MCP tool calls are proxied: subprocess receives JSON-RPC from Claude → forwards over socket to TUI → TUI executes on shared SessionManager → result back

##### A2: The subprocess IS the shared manager

- `tttt mcp-server` launched with `--session-socket /tmp/tttt-XXXX.sock`
- It creates sessions locally but registers them with the TUI via the socket
- TUI discovers new sessions and adds them to its display

##### A3: In-process MCP server with pipe (simplest for MVP)

- TUI creates a pipe pair (read_fd, write_fd)
- TUI spawns root agent's PTY with these fds as extra environment:
  `TTTT_MCP_READ_FD=N TTTT_MCP_WRITE_FD=M`
- But... Claude Code doesn't read MCP from environment fds. It spawns its own MCP server subprocess.

**Decision for MVP: Go with Option A1 (Unix socket).** The subprocess MCP server proxies to the TUI over a socket. This is the cleanest separation and works with how Claude Code actually launches MCP servers.

### Layer 2: MCP tool completeness

Current tools vs what the user stories require:

| Tool | Status | Notes |
|------|--------|-------|
| `pty_launch` | **Implemented** | Has command, args, working_dir, cols, rows, sandbox_profile |
| `pty_send_keys` | **Implemented** | Has session_id, keys, raw. Story 999: remove auto-Enter, always explicit |
| `pty_get_screen` | **Implemented** | Returns plain text + cursor. Story 001a: needs cols/rows option (200x50) |
| `pty_get_scrollback` | **MISSING** | Story 003d: critical — results scroll off visible screen |
| `pty_list` | **Implemented** | Returns session metadata |
| `pty_kill` | **Implemented** | |
| `pty_get_cursor` | **Implemented** | |
| `pty_resize` | **Implemented** | |
| `pty_set_scrollback` | **Implemented** | |
| `pty_start_capture` / `pty_stop_capture` | **MISSING** | Story 009: capture raw output stream to file |
| `reminder_set` | **Defined** but not wired | Scheduler exists but MCP handler not connected |
| `cron_create/list/delete` | **Defined** but not wired | Same |
| `notify_on_prompt` | **MISSING** | Story 014e: register watcher, inject notification on match |
| `self_inject` | **MISSING** | Story 014a: inject text into root agent's own PTY |
| `scratchpad_write/read` | **MISSING** | Story 014c: persistent working notes |
| `tui_switch` | **MISSING** | Let root agent switch which pane the human sees |
| `tui_get_info` | **MISSING** | Let root agent know TUI state (active session, dimensions) |

### Layer 3: pty_send_keys improvements

Story 999 lesson #4: remove auto-Enter. Story 013: named key tokens. Current implementation:

- `raw=false` (default): appends `\n` — **this should change to NO auto-append**
- `raw=true`: no append — this becomes the only behavior
- Named tokens `[ENTER]`, `[ESCAPE]`, etc. already supported via `process_special_keys()`
- **Missing:** `[SHIFT+TAB]` (`\x1b[Z`), `[CTRL+O]` (`\x0f`)
- **Missing:** Bracketed paste mode: `[PASTE_START]text[PASTE_END]`
- **Missing:** Inter-key delay option for slow terminals

### Layer 4: Logging completeness

Current: text + SQLite logging exists but not wired into the event loop.

- PTY output bytes: logged when? Currently `pump()` reads but doesn't log.
- PTY input bytes: logged in `handle_input_event` for PassThrough.
- **Missing:** Log EVERY byte from EVERY session (not just active).
- **Missing:** Log MCP tool calls and results.
- **Missing:** Structured experiment metadata.

### Layer 5: TUI improvements for the human

- **Missing:** Status line at bottom showing: active session, session count, scheduler status
- **Missing:** Session list in sidebar should show scrollback indicator
- **Missing:** Human should be able to type into ANY session (currently only active)
- **Improvement:** Sidebar should update when MCP creates/destroys sessions

---

## Implementation Order

### Phase A: Wire the basics end-to-end (no fancy features)

**Goal:** Root agent can launch, type into, and read from child sessions via MCP tools, and human can see everything in TUI.

1. **A1: Unix socket for MCP proxy**
   - TUI creates a Unix socket at startup
   - `tttt mcp-server --connect /tmp/tttt-XXXX.sock` mode
   - MCP server receives JSON-RPC from Claude, proxies to TUI over socket
   - TUI executes on shared SessionManager, returns result
   - Tests: socket creation, connect, request/response roundtrip, session creation visible in manager

2. **A2: Root agent launch with MCP config**
   - TUI writes temporary MCP config JSON pointing to `tttt mcp-server --connect <socket>`
   - TUI spawns root agent (e.g., `claude --mcp-config <tmpfile>`)
   - Tests: config file generation, root agent launch, MCP server subprocess spawned

3. **A3: Fix pty_send_keys — no auto-Enter**
   - Remove `raw` parameter
   - Never auto-append anything
   - Document: use `[ENTER]` explicitly
   - Add missing named keys: `[SHIFT+TAB]`, `[CTRL+O]`, `[CTRL+U]`
   - Tests: verify no auto-append, all named keys resolve correctly

4. **A4: Add pty_get_scrollback**
   - Return lines from vt100 scrollback buffer
   - Parameters: session_id, lines (how many lines to return)
   - Tests: write enough output to scroll, verify scrollback retrieval

5. **A5: Wire scheduler tools to MCP**
   - `reminder_set` → creates reminder in shared Scheduler
   - `cron_create/list/delete` → manages cron jobs
   - Scheduler `tick()` called in event loop (already happening)
   - Scheduler events: inject text into target session's PTY
   - Tests: create reminder via MCP, verify it fires and injects

6. **A6: Wire logging into event loop**
   - Log every `pump()` output (all sessions, not just active)
   - Log every `send_keys`/`send_raw` input
   - Log MCP tool call + result as Meta events
   - Tests: run session, verify log events in SQLite

### Phase B: Notification and injection system

**Goal:** Root agent can register for notifications and inject into its own session.

7. **B1: notify_on_prompt tool**
   - Register: session_id, pattern (regex), callback_text
   - Harness monitors session's vt100 screen contents each tick
   - When pattern matches: inject callback_text into root agent's PTY
   - Gated on root agent being at prompt (configurable pattern)
   - Tests: register watcher, simulate session output matching, verify injection

8. **B2: self_inject tool**
   - Inject text into the root agent's own PTY stdin
   - Gated on root agent being at prompt
   - Tests: self_inject, verify text appears in root agent's input

9. **B3: Injection pacing (story 015)**
   - Queue for pending injections per session
   - `wait_for_prompt` before delivery
   - `min_interval_ms`, `jitter_ms` between deliveries
   - `batch_window_ms` for batching multiple notifications
   - Tests: rapid notifications batched, prompt gating works, interval respected

### Phase C: TUI multiplexer tools for root agent

**Goal:** Root agent can control what the human sees.

10. **C1: tui_switch tool**
    - Switch TUI's active session (what human sees on left pane)
    - Root agent decides which executor is most interesting right now
    - Tests: switch via MCP, verify active_session changes

11. **C2: tui_get_info tool**
    - Returns: active session ID, session count, screen dimensions, scheduler status
    - Tests: verify all fields populated

12. **C3: Sidebar auto-update**
    - `sync_session_order()` already runs each tick
    - Sidebar already re-renders each tick when PTY output arrives
    - Verify: session created via MCP → appears in sidebar next render
    - Tests: create session via MCP in background, verify sidebar updates

### Phase D: Capture and scrollback

13. **D1: pty_start_capture / pty_stop_capture**
    - Start: begin writing raw PTY output to a file
    - Stop: stop capture, return file path + bytes written
    - Tests: capture session output, verify file contents

14. **D2: Full output stream logging**
    - Every session's raw PTY output logged (separate from screen buffer)
    - Enables: full replay, scrollback beyond vt100 buffer
    - Tests: run command with large output, verify full output in log

---

## Files to create/modify per phase

### Phase A
```
NEW:  crates/tttt-mcp/src/socket.rs     — Unix socket server/client for MCP proxy
MOD:  crates/tttt-mcp/src/handler.rs    — Add scrollback, scheduler tool handlers
MOD:  crates/tttt-mcp/src/tools.rs      — Add pty_get_scrollback definition
MOD:  crates/tttt-pty/src/keys.rs       — Add missing named keys
MOD:  crates/tttt-pty/src/screen.rs     — Expose scrollback retrieval
MOD:  src/app.rs                        — Socket listener, MCP config generation, logging wiring
MOD:  src/main.rs                       — --connect flag for mcp-server subcommand
NEW:  tests/mcp_socket.rs              — Socket-based MCP proxy tests
NEW:  tests/scheduler_mcp.rs           — Scheduler tools via MCP tests
```

### Phase B
```
NEW:  crates/tttt-mcp/src/notification.rs — Notification registry and injection
MOD:  crates/tttt-mcp/src/handler.rs      — notify_on_prompt, self_inject handlers
MOD:  crates/tttt-mcp/src/tools.rs        — Tool definitions
MOD:  src/app.rs                          — Notification check in event loop, injection delivery
NEW:  tests/notification.rs               — Notification and injection tests
```

### Phase C
```
MOD:  crates/tttt-mcp/src/handler.rs  — tui_switch, tui_get_info handlers
MOD:  crates/tttt-mcp/src/tools.rs    — Tool definitions
MOD:  src/app.rs                      — Handle tui_switch from MCP
NEW:  tests/tui_mcp.rs               — TUI control via MCP tests
```

### Phase D
```
MOD:  crates/tttt-pty/src/session.rs  — Capture start/stop
MOD:  crates/tttt-mcp/src/handler.rs  — Capture tool handlers
MOD:  src/app.rs                      — Full output stream logging for all sessions
NEW:  tests/capture.rs               — Capture and full logging tests
```

---

## Test strategy per phase

Each phase adds integration tests that verify the end-to-end flow:

- **Phase A tests:** "Create MCP server on socket → connect → pty_launch → session exists in shared manager → pty_send_keys → pty_get_screen shows result"
- **Phase B tests:** "Register notification → executor output matches → root agent receives injection"
- **Phase C tests:** "Root agent calls tui_switch → TUI active session changes"
- **Phase D tests:** "Start capture → run command → stop capture → file has full output"

Total estimated new tests: ~60-80 across all phases.
