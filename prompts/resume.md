# Session Resume Context

## What Was Built This Session

### Session Replay System (complete, working)
- **SQLite session logging**: `sessions` table with metadata (command, dims, timestamps, PID), automatic differential output logging via existing `events` table
- **Replay engine**: `crates/tttt-log/src/replay.rs` — `SessionReplay` struct using vt100::Parser, supports step/seek/timeline
- **MCP tools**: `tttt_replay_list_sessions`, `tttt_replay_get_screen`, `tttt_replay_get_timeline` in `crates/tttt-mcp/src/handler.rs` (`ReplayToolHandler`)
- **Interactive TUI viewer**: `src/replay_tui.rs` — ratatui-based `tttt replay` subcommand with session list table, full-color playback, play/pause/speed/seek controls
- **PID disambiguation**: nullable `pid INTEGER` column in both tables, auto-captured at startup, used to separate sessions across tttt restarts
- **Legacy DB support**: graceful handling of databases without `sessions` table, NULL-pid orphan splitting by 1-hour time gaps, visual grouping with separator rows
- **Input logging flag**: `--danger-log-user-input-including-passwords` (disabled by default)
- **SharedSqliteLogSink**: `Arc<Mutex<SqliteLogger>>` wrapper for shared access between logging pipeline and direct session-table operations

### app.rs Refactoring Phase 1 (complete)
Extracted 7 pure logic functions from the monolithic `src/app.rs` via TDD (48 new tests):
1. `prefix_key_name()`, `format_help_screen()` — help overlay text
2. `compute_relative_index()` — circular session navigation
3. `reconcile_session_order()` — session list order maintenance
4. `should_render_now()` — render debounce decision
5. `calculate_pane_dimensions()`, `calculate_min_dimensions()` — layout math
6. `SessionExitAction` + `compute_exit_action()` — session exit decision
7. `InputAction` + `decide_input_action()` — input event routing

### Test Coverage Improvements
- Installed `cargo-llvm-cov` for branch coverage measurement
- Added TestBackend-based ratatui rendering tests for `replay_tui.rs`
- Improved `proxy.rs` coverage from 38% → 80%

## Current Coverage (LLVM branch coverage, nightly-2025-09-01)

| Crate | Lines | Branches |
|-------|-------|----------|
| tttt-log | 94.2% | 85.9% |
| tttt-mcp | 91.5% | 80.2% |
| tttt-scheduler | 96.4% | 81.3% |
| tttt-tui | 91.8% | 66.3% |
| tttt-pty | 83.8% | 64.6% |
| tttt binary | 65.8% | 35.0% |

Command: `cargo +nightly-2025-09-01 llvm-cov --branch -p <crate> --summary-only`

## Key Files Modified/Created

- `crates/tttt-log/src/replay.rs` — NEW: replay engine
- `crates/tttt-log/src/sqlite.rs` — sessions table, PID column, orphan chunks, SharedSqliteLogSink
- `crates/tttt-log/src/event.rs` — SessionInfo struct
- `crates/tttt-mcp/src/handler.rs` — ReplayToolHandler
- `crates/tttt-mcp/src/tools.rs` — replay tool definitions
- `crates/tttt-mcp/src/proxy.rs` — improved test coverage
- `src/replay_tui.rs` — NEW: ratatui TUI replay viewer
- `src/app.rs` — session lifecycle wiring + 7 extracted pure functions
- `src/main.rs` — `Replay` subcommand + `--danger-log-user-input-including-passwords` flag
- `src/config.rs` — `log_input` field

## Plans & Docs
- `docs/plans/005-session-replay.md` — original replay plan
- `docs/plans/006-replay-tui.md` — ratatui viewer plan
- `docs/plans/007-app-refactoring.md` — app.rs refactoring plan with results

## What's Next (Phase 2: ratatui migration)

The app.rs refactoring was Phase 1, preparing for eventual full ratatui migration:
1. **Migrate sidebar** to ratatui widget (lowest risk, proves the pattern)
2. **Migrate main pane** — replace PaneRenderer with ratatui using existing `screen_to_lines()` from replay_tui.rs
3. **Migrate event loop** — switch from raw poll/read to crossterm event loop
4. **Migrate attach viewer** — rethink protocol or keep as-is

Each step should be its own TDD chunk. The pure functions extracted in Phase 1 will slot cleanly into the ratatui state/render/event pattern.

## Working Conventions
- Use Opus for planning, Sonnet (via tttt PTY) for coding
- TDD with 90%+ coverage target
- Small incremental commits
- Document in docs/ as you go
- Always look up tttt PID fresh before sending signals
- Never ad-hoc test; always write proper test functions
