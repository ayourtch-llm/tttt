# 004 — Live Reload via `execv`

## Motivation

When developing or upgrading tttt, the process currently must be restarted, which kills all child PTY sessions and loses in-memory state (scheduler jobs, notification watchers, screen buffers). For a tool designed to manage long-running agent sessions, this is disruptive.

The goal is to allow tttt to **replace its own binary in-place** while keeping all child processes alive and the terminal session uninterrupted.

## Core Mechanism: `execv` + FD Inheritance

`execv(2)` replaces the current process image but preserves:

| Preserved by `execv` | Lost by `execv` |
|---|---|
| PID (children still have us as parent) | Heap, stack, all in-memory state |
| Open file descriptors (unless `O_CLOEXEC`) | Memory-mapped regions |
| Signal mask | Threads (all threads die) |
| Process group, session, controlling terminal | Pending signals |
| Environment variables | |
| Current working directory | |

This is the same technique used by nginx, HAProxy, and systemd for zero-downtime restarts.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│  BEFORE EXEC                                            │
│                                                         │
│  App { sessions, scheduler, notifications, viewers }    │
│       │          │            │              │           │
│       ▼          ▼            ▼              ▼           │
│   PTY master   in-memory   in-memory     unix socket    │
│   FDs (raw)    state       state         FDs            │
│                                                         │
│  1. Serialize state → JSON/bincode                      │
│  2. Clear FD_CLOEXEC on PTY master FDs                  │
│  3. Pass state via env var or temp file                  │
│  4. execv(new_binary_path, original_args)               │
└─────────────────────────────────────────────────────────┘
                         │
                    execv(2)
                         │
┌─────────────────────────────────────────────────────────┐
│  AFTER EXEC                                             │
│                                                         │
│  1. Detect restore mode (env var TTTT_RESTORE_FD)       │
│  2. Read saved state from temp file                     │
│  3. Reconstruct RealPty from raw FDs                    │
│  4. Rebuild App state                                   │
│  5. Resume event loop — children never noticed          │
└─────────────────────────────────────────────────────────┘
```

## Detailed Design

### 1. Saved State Structure

```rust
#[derive(Serialize, Deserialize)]
struct SavedState {
    /// Version for forward-compatibility
    version: u32,

    /// Per-session state
    sessions: Vec<SavedSession>,

    /// Which session is active
    active_session: Option<String>,

    /// Ordered session list (for tab ordering)
    session_order: Vec<String>,

    /// SessionManager's next_id counter
    next_session_id: u64,

    /// Scheduler state (cron jobs, pending reminders)
    scheduler: SavedScheduler,

    /// Notification watcher registrations
    notifications: Vec<SavedNotificationWatcher>,

    /// Config as loaded (so we don't need to re-parse files)
    config: Config,

    /// Terminal dimensions at time of save
    screen_cols: u16,
    screen_rows: u16,

    /// Socket paths (viewer + MCP) — sockets themselves are re-created
    viewer_socket_path: Option<String>,
    mcp_socket_path: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    id: String,
    name: Option<String>,
    command: String,
    status: SessionStatus,
    cols: u16,
    rows: u16,

    /// The raw FD number of the PTY master — must survive exec
    master_fd: i32,

    /// VT100 screen state: the full formatted contents.
    /// On restore, we replay this through a fresh vt100::Screen
    /// to reconstruct cursor position, colors, etc.
    screen_contents_formatted: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct SavedScheduler {
    cron_jobs: Vec<SavedCronJob>,
    // Reminders are not saved — they are ephemeral by nature.
    // If a reminder was about to fire, it's lost. This is acceptable.
}

#[derive(Serialize, Deserialize)]
struct SavedCronJob {
    id: String,
    target_session: String,
    keys: String,
    interval_description: String,
    // The actual interval is re-parsed from interval_description
}

#[derive(Serialize, Deserialize)]
struct SavedNotificationWatcher {
    id: String,
    session_id: String,
    pattern: String,
    response_text: String,
}
```

### 2. Pre-Exec: Saving State

Triggered by a keybinding (e.g., `<prefix> R`) or a signal (e.g., `SIGUSR1`):

```
fn prepare_reload(&mut self) -> Result<SavedState> {
    // 1. Lock session manager
    // 2. For each session:
    //    a. Get the raw FD via backend().reader_raw_fd()
    //    b. Capture screen_contents_formatted()
    //    c. Build SavedSession
    // 3. Serialize scheduler cron jobs
    // 4. Serialize notification watchers
    // 5. Build SavedState with config, dimensions, etc.
    // 6. For each PTY master FD: clear FD_CLOEXEC
    //    fcntl(fd, F_SETFD, FdFlag::empty())
    // 7. Close non-essential FDs (viewer connections, MCP threads)
    //    - Viewer clients: send Goodbye message, drop
    //    - Unix socket listeners: close (re-created on restore)
    // 8. Write state to temp file
    //    - Path: /tmp/tttt-restore-{pid}.json
    //    - Set env var: TTTT_RESTORE_FILE=/tmp/tttt-restore-{pid}.json
    // 9. Restore terminal from raw mode
    // 10. execv()
}
```

### 3. Post-Exec: Restoring State

In `main()`, before normal startup:

```
fn main() {
    // Check for restore mode FIRST
    if let Ok(restore_file) = std::env::var("TTTT_RESTORE_FILE") {
        std::env::remove_var("TTTT_RESTORE_FILE");
        let state = read_and_delete(restore_file);
        return run_restored(state);
    }
    // ... normal startup
}
```

Restore path:

```
fn run_restored(state: SavedState) {
    // 1. Validate version compatibility
    // 2. Re-enter raw terminal mode
    // 3. For each SavedSession:
    //    a. Create RealPty::from_raw_fd(master_fd, cols, rows)
    //       - Wraps the inherited FD into reader/writer
    //       - Sets non-blocking mode (may already be set)
    //       - Does NOT spawn a child (child is already running)
    //    b. Create PtySession with the restored backend
    //    c. Replay screen_contents_formatted through vt100::Screen
    //       to restore visual state
    //    d. Add to SessionManager
    // 4. Rebuild scheduler from saved cron jobs
    // 5. Rebuild notification watchers
    // 6. Create App with restored state
    // 7. Re-create viewer socket listener (new socket, same PID path)
    // 8. Re-create MCP socket listener
    // 9. Full screen redraw
    // 10. Enter event loop
}
```

### 4. New `RealPty` Constructor: `from_raw_fd`

This is the key new API in `tttt-pty`:

```rust
impl RealPty {
    /// Reconstruct a RealPty from an inherited file descriptor.
    /// Used after execv() to re-adopt PTY sessions.
    ///
    /// The child process is already running on the slave side of this PTY.
    /// We only need the master FD for reading output and writing input.
    #[cfg(unix)]
    pub fn from_raw_fd(master_fd: RawFd, cols: u16, rows: u16) -> Result<Self> {
        // 1. Verify FD is valid: fcntl(fd, F_GETFD) != -1
        // 2. Set non-blocking: fcntl(fd, F_SETFL, O_NONBLOCK)
        // 3. Wrap into reader + writer (via File::from_raw_fd + dup for separate R/W)
        // 4. Create a stub Child that try_wait() checks via waitpid(WNOHANG)
        //    on the PID we can discover via ioctl or /proc
    }
}
```

**Challenge — `portable-pty` child handle**: After `execv`, we lose the `Box<dyn Child>` object. Options:

- **Option A**: Store child PIDs in `SavedState`, use raw `waitpid()` in a wrapper struct that implements the same interface. This is the simplest approach.
- **Option B**: Bypass `portable-pty` entirely for restored sessions, using raw FD I/O (we already do this for reads via `nix::unistd::read`). The child handle is only used for `try_wait()` and `kill()` — both can be done with the PID directly.

**Recommended: Option B** — for restored sessions, use a `RestoredPty` struct that:
- Reads/writes via the raw master FD (same as `RealPty` already does)
- Uses `nix::sys::wait::waitpid(pid, WNOHANG)` for `try_wait()`
- Uses `nix::sys::signal::kill(pid, SIGTERM)` for `kill()`

```rust
pub struct RestoredPty {
    master_fd: RawFd,
    child_pid: Option<nix::unistd::Pid>,  // None if we can't determine PID
    cols: u16,
    rows: u16,
}

impl PtyBackend for RestoredPty {
    // ... same interface, implemented with raw syscalls
}
```

### 5. Determining Child PIDs

After `execv`, we have the master FD but lost the child handle. We need the PID for `try_wait()` and `kill()`.

**Linux**: Read `/proc/self/fdinfo/{fd}` or use `ioctl(fd, TIOCGPGRP)` to get the foreground process group of the PTY.

**macOS**: Use `ioctl(fd, TIOCGPGRP)` — this gives the foreground process group ID, which is typically the child's PID for a simple shell.

**Fallback**: If PID discovery fails, the restored session operates in "monitor-only" mode:
- `try_wait()` returns `None` (assumes running) until read returns 0/EIO (PTY closed)
- `kill()` returns an error ("cannot kill: PID unknown")

This is acceptable because:
- Most sessions will have discoverable PIDs
- The common case (child exits normally) is handled by detecting PTY closure
- Users can still type `exit` in the session

### 6. Socket Handling

**Viewer socket** (`/tmp/tttt-{PID}.sock`):
- PID doesn't change across `execv`, so the path stays the same
- But the `UnixListener` FD will be closed by default (`O_CLOEXEC`)
- **Strategy**: Close the old listener, delete the socket file, create a fresh one
- Connected viewers will get disconnected and must reconnect
- The `tttt attach` protocol already handles reconnection gracefully

**MCP proxy socket** (`/tmp/tttt-mcp-{PID}.sock`):
- Same approach: close and re-create
- MCP proxy threads die on `execv` (all threads are killed)
- Claude/agents reconnect via their existing MCP config pointing to the socket path

### 7. Trigger Mechanisms

#### 7a. Keybinding: `<prefix> R`

New `InputEvent::Reload` variant:
- User presses prefix key, then `R`
- Triggers `prepare_reload()`
- Before exec, briefly shows "Reloading..." on the status bar

#### 7b. Signal: `SIGUSR1`

```rust
// In the signal handler setup (already handles SIGWINCH):
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

// Signal handler sets the flag
// Main event loop checks: if RELOAD_REQUESTED.load(Ordering::Relaxed) { ... }
```

This allows external automation:
```bash
# Build new binary, then trigger reload
cargo build --release && kill -USR1 $(pidof tttt)
```

#### 7c. Binary Path Resolution

On reload, we need to know which binary to `execv`:
1. **Default**: `std::env::current_exe()` — re-exec the same binary (for signal-triggered reloads where the binary was replaced in-place, e.g., `cargo install`)
2. **Explicit**: `TTTT_RELOAD_BINARY` env var — allows pointing to a newly built binary
3. **Keybinding variant**: Could prompt for path, but that's complex. Start with current_exe.

### 8. Version Compatibility

The `SavedState.version` field enables forward compatibility:

- **Same version**: full restore
- **Newer version that can read old format**: migrate and restore
- **Incompatible version**: log warning, start fresh (children are adopted as orphans by init — they continue running but tttt loses track of them)

In practice, during development, breaking the format is fine. The version field is insurance for when tttt is more stable.

### 9. Edge Cases & Failure Modes

| Scenario | Handling |
|---|---|
| `execv` fails (e.g., binary not found) | `execv` returns error — we're still in the old process. Log error, resume event loop. State was saved but not consumed. |
| State file write fails | Abort reload, resume event loop, show error. |
| State file read fails on restore | Start fresh (normal startup). Children become orphans. |
| PTY FD invalid after exec | Remove session from restored state, log warning. |
| Child exited during exec window | `try_wait()` on restore will detect it. Normal exit handling applies. |
| Screen state slightly stale | Acceptable — next PTY output will update the screen. The formatted contents replay gets us 99% there. |
| Viewer connected during reload | Gets disconnected. Viewer's event loop detects EOF and can auto-reconnect. |
| MCP call in-flight during reload | Proxy thread dies mid-call. Claude sees a broken pipe and retries. |

### 10. Implementation Plan

#### Phase 1: Core Infrastructure
1. Add `SavedState` and related structs in a new `src/reload.rs` module
2. Add `RestoredPty` struct implementing `PtyBackend` in `tttt-pty`
3. Make `SessionManager` generic enough to hold mixed session types (or use an enum backend)

#### Phase 2: Save Path
4. Add `prepare_reload()` to `App` — serializes state, clears CLOEXEC, writes file
5. Add SIGUSR1 handler and `InputEvent::Reload` keybinding
6. Add `execv` call with proper terminal state restoration

#### Phase 3: Restore Path
7. Add restore detection in `main()`
8. Add `run_restored()` — reconstructs App from SavedState
9. Re-create socket listeners, full screen redraw

#### Phase 4: Testing
10. Unit tests for `SavedState` serialization roundtrip
11. Unit tests for `RestoredPty` with mock FDs
12. Integration test: spawn tttt, trigger reload, verify sessions survive
13. Test SIGUSR1 trigger path

#### Phase 5: Polish
14. Viewer auto-reconnect on disconnect (for seamless viewer experience)
15. Better error messages and status bar feedback
16. Document the feature in user-facing docs

### 11. Open Questions

1. **Mixed backend types in SessionManager**: Currently `SessionManager<B: PtyBackend>` is generic over a single backend type. After restore, we'd have `RestoredPty` instead of `RealPty`. Options:
   - Make `SessionManager` use `Box<dyn PtyBackend>` (trait object) instead of generic — simplest, small performance cost
   - Use an enum `enum AnyPty { Real(RealPty), Restored(RestoredPty) }` implementing `PtyBackend`
   - Keep generic but always use `RestoredPty` after restore (even for newly spawned sessions... no, that doesn't work)
   - **Recommendation**: Enum approach (`AnyPty`) — zero-cost, works with existing generic code

2. **Should we preserve the viewer socket FD across exec?** This would allow connected viewers to stay connected without reconnecting. Adds complexity (must also preserve per-viewer state). Probably not worth it for v1 — viewers reconnect in <1s.

3. **Logger state**: Log files are append-mode. After exec, we can re-open them (same paths from config). SQLite connections need to be re-established. No data loss since logs are flushed before exec.

4. **Scratchpad state** (MCP key-value store): Currently in-memory. Should be included in `SavedState` if it contains valuable agent work state.

### 12. Non-Goals (for now)

- **Windows support**: `execv` is Unix-only. Windows would need a different approach (spawn new process, transfer handles via `DuplicateHandle`). Out of scope.
- **Multi-process orchestration**: This design handles the single tttt process. MCP proxy subprocesses are ephemeral and reconnect.
- **Preserving in-flight MCP calls**: Too complex. Let them fail and retry.
- **Hot-patching without restart**: `execv` replaces the entire binary. There's no partial code reload.
