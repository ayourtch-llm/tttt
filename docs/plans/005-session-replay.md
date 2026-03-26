# Session Replay for tttt

## Context

tttt already logs raw PTY output bytes to SQLite via `tttt-log::SqliteLogger` (events table with timestamps per session). These raw bytes ARE naturally differential -- each event is the new output since the last pump cycle. Feeding them sequentially through a vt100 parser reconstructs the screen. What's missing: session metadata, resize tracking, and a replay API.

## What Exists (DON'T RE-IMPLEMENT)
- `SqliteLogger` in `crates/tttt-log/src/sqlite.rs` -- logs to `events(id, session_id, timestamp_ms, direction, data)`
- `LogEvent`, `Direction`, `LogSink` trait in `crates/tttt-log/src/event.rs` and `lib.rs`
- Raw output logging wired in `src/app.rs` at every `pump_raw()` call
- `ScreenBuffer` wrapping vt100::Parser in `crates/tttt-pty/src/screen.rs`

## Implementation Steps

### Step 1: Session metadata in SQLite (`crates/tttt-log/src/sqlite.rs`)

Add `sessions` table:
```sql
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    command TEXT NOT NULL,
    cols INTEGER NOT NULL,
    rows INTEGER NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    name TEXT
);
```

New struct `SessionInfo` (in `crates/tttt-log/src/event.rs`):
- `session_id, command, cols, rows, started_at_ms, ended_at_ms: Option<u64>, name: Option<String>`

New methods on `SqliteLogger`:
- `log_session_start(session_id, command, cols, rows, name)`
- `log_session_end(session_id)` -- sets ended_at_ms
- `list_sessions() -> Vec<SessionInfo>`
- `get_session_info(session_id) -> Option<SessionInfo>`
- `open_read_only(path)` -- for replay handler (concurrent read-safe)

Tests: start+query, end sets timestamp, list empty/multiple, schema idempotent, name field

### Step 2: Replay engine (`crates/tttt-log/src/replay.rs` -- NEW FILE)

Add `vt100 = { workspace = true }` + `serde_json = { workspace = true }` to `crates/tttt-log/Cargo.toml`.

```rust
pub struct SessionReplay {
    events: Vec<LogEvent>,
    screen: vt100::Parser,
    current_index: usize,  // next event to process
    cols: u16,
    rows: u16,
}
```

Methods:
- `new(events, cols, rows)` -- create parser, store events
- `step_forward() -> bool` -- process next event (Output->feed parser, Meta resize->resize parser, skip others)
- `seek_to_index(idx)` -- reset+replay if backward, step forward if ahead
- `seek_to_timestamp(ts)` -- process events up to timestamp
- `screen_contents() -> String`, `screen_contents_formatted() -> Vec<u8>`
- `cursor_position() -> (u16, u16)`
- `current_index()`, `event_count()`, `current_timestamp()`
- `timeline() -> Vec<(usize, u64, Direction)>`

Tests: empty replay, single/multiple events, seek forward/backward, resize events, input skipped, ANSI sequences, timeline

### Step 3: SharedSqliteLogSink wrapper (`crates/tttt-log/src/sqlite.rs`)

```rust
pub struct SharedSqliteLogSink(pub Arc<Mutex<SqliteLogger>>);
impl LogSink for SharedSqliteLogSink { /* lock and forward */ }
```

Export from `lib.rs`.

### Step 4: Wire session lifecycle in `src/app.rs`

- Add `sqlite_logger: Option<Arc<Mutex<SqliteLogger>>>` to `App`
- In `init_loggers()`: create SqliteLogger, wrap in Arc<Mutex>, add SharedSqliteLogSink to MultiLogger, keep reference
- On session launch: call `log_session_start()`
- On session exit (`check_session_exit`): call `log_session_end()`
- On resize: log Meta event `{"type":"resize","cols":N,"rows":N}`

### Step 5: MCP replay tools (`crates/tttt-mcp/`)

Add `tttt-log = { path = "../tttt-log" }` to `crates/tttt-mcp/Cargo.toml`.

Tool definitions in `tools.rs`:
- `tttt_replay_list_sessions` -- no required params
- `tttt_replay_get_screen` -- session_id required; timestamp_ms or event_index optional
- `tttt_replay_get_timeline` -- session_id required

`ReplayToolHandler` in `handler.rs`:
- Holds `db_path: PathBuf`
- Opens read-only SqliteLogger per request
- Queries events, creates SessionReplay, seeks, returns screen

Wire into composite handler in `app.rs` MCP proxy thread.

### Step 6: Pass sqlite_logger to PtyToolHandler for MCP-launched sessions

Add `Option<Arc<Mutex<SqliteLogger>>>` to `PtyToolHandler` so sessions launched via MCP also get logged to sessions table.

## Verification
1. `cargo test -p tttt-log` -- all replay + sqlite tests pass
2. `cargo test -p tttt-mcp` -- replay handler tests pass
3. `cargo build` -- full project compiles
4. Manual: launch tttt, create session, produce output, use `tttt_replay_list_sessions` and `tttt_replay_get_screen` MCP tools

## Critical Files
- `crates/tttt-log/src/sqlite.rs` -- sessions table, SharedSqliteLogSink, metadata methods
- `crates/tttt-log/src/replay.rs` -- NEW: replay engine
- `crates/tttt-log/src/event.rs` -- SessionInfo struct
- `crates/tttt-log/src/lib.rs` -- exports
- `crates/tttt-log/Cargo.toml` -- add vt100 dep
- `crates/tttt-mcp/src/handler.rs` -- ReplayToolHandler
- `crates/tttt-mcp/src/tools.rs` -- replay tool definitions
- `crates/tttt-mcp/Cargo.toml` -- add tttt-log dep
- `src/app.rs` -- wiring
