# tttt Next Steps (post user stories)

## Informed by: 18 user stories from 8-hour dual-Claude research session

## MVP Priority (highest impact first)

### P0: Permission auto-approval via TextFSMPlus
- **Impact:** Eliminates 80+ manual approvals (biggest time sink)
- **Approach:**
  - Vendor aytextfsmplus into tttt workspace
  - Add aycalc as git dep from github.com/ayourtch/aycalc
  - Harness monitors executor screen (vt100 contents, NOT raw bytes)
  - Per-interaction FSM: created on pattern trigger, runs to Done, dropped
  - Built-in template for Claude Code permission prompts
  - Template auto-responds with Enter (`\r`) to approve

### P1: Notification on executor completion
- **Impact:** Eliminates 40% polling waste (second biggest win)
- **Approach:**
  - Harness monitors each executor's screen for prompt pattern
  - When pattern matches, inject notification into root agent's PTY stdin
  - Injection gated on root agent being at its own prompt (wait-for-prompt)
  - Rate limited: min interval, jitter, batching (story 015)
  - MCP tool: `notify_on_prompt(session_id, pattern, callback_text)`

### P2: In-process MCP server (pipe to root agent)
- **Impact:** Enables the full orchestration loop
- **Approach:**
  - TUI spawns root agent with MCP server on pipe (not stdio)
  - MCP server thread operates on shared `Arc<Mutex<SessionManager>>`
  - Root agent configured to connect to pipe-based MCP server
  - Sessions created via MCP visible in TUI sidebar immediately

### P3: Parallel executors
- **Impact:** Could halve wall-clock time (session manager already supports this)
- **Approach:**
  - Already works at SessionManager level
  - Need TUI support for switching between sessions (already implemented)
  - Need sidebar to show all session statuses (already implemented)
  - Need git worktree isolation per executor (sandbox profile)

## Key Design Decisions from User Stories

### TextFSMPlus integration pattern
- Feed: vt100 screen contents (clean text), not raw bytes
- FSM lifecycle: per-interaction (create → run → Done/drop)
- aycalc: git dependency from github.com/ayourtch/aycalc
- Templates: config directory + embedded defaults

### Message submission
- NO auto-append Enter (story 999 lesson #4)
- Bracketed paste mode awareness for long messages
- Explicit `[ENTER]` token in named key vocabulary

### Injection mechanics
- Write to target PTY stdin (as if human typed it)
- Gate on target being at prompt (detected via screen pattern)
- Rate limit: min_interval_ms, jitter_ms, batch_window_ms
- Cross-session: watch session A's screen, inject into session B

### Logging
- Every PTY byte in/out logged to text + SQLite (already implemented)
- Add: inferred FSM state to log events
- Add: structured experiment metadata
- Add: permission audit trail

## What NOT to build yet
- Session replay (009b) — nice but not blocking
- Scratchpad/task board (014c/d) — useful for 2+ hour sessions, not MVP
- Heterogeneous AI teams (011) — vision, not MVP
- Template hot-reload (012e) — file restart is fine for now
- Bootstrap prompt generation (016) — manual for now
- Context limit detection (014f) — nice-to-have
