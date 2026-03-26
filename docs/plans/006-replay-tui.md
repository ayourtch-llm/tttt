# Ratatui Session Replay Viewer

## Context

Session replay data is already being logged to SQLite (events table + sessions table) and a `SessionReplay` engine exists in `tttt-log`. Currently replay is only accessible via MCP tools. This adds an interactive TUI viewer using ratatui, launched via `tttt replay` subcommand. This is also the first ratatui usage in the project, laying groundwork for an eventual full TUI migration.

## Implementation Steps

### Step 1: Dependencies

**`Cargo.toml` (workspace root)** -- add to `[workspace.dependencies]`:
```toml
ratatui = "0.29"
crossterm = "0.28"
```

Add to the `[dependencies]` section of the `[package]` (tttt binary):
```toml
ratatui = { workspace = true }
crossterm = { workspace = true }
```

Also add `chrono = { workspace = true }` to the tttt binary deps if not already there (needed for timestamp formatting).

### Step 2: Expose vt100 screen from SessionReplay

**`crates/tttt-log/src/replay.rs`** -- add one method:
```rust
pub fn screen(&self) -> &vt100::Screen {
    self.screen.screen()
}
```

This enables cell-by-cell access for the ratatui conversion.

### Step 3: Create `src/replay_tui.rs` (NEW FILE)

This is the main new file. Structure:

**Types:**
```rust
pub struct ReplayApp {
    db: SqliteLogger,
    sessions: Vec<SessionListEntry>,
    view: View,
    should_quit: bool,
}

struct SessionListEntry {
    info: SessionInfo,
    event_count: usize,
}

enum View {
    SessionList(SessionListState),
    Replay(ReplayViewState),
}

struct SessionListState {
    table_state: ratatui::widgets::TableState,
}

struct ReplayViewState {
    replay: SessionReplay,
    session_info: SessionInfo,
    playing: bool,
    speed: f64,          // 1.0x default, doubles/halves
    last_tick: Instant,
    base_timestamp: u64, // first event timestamp for relative display
}
```

**Entry point:**
```rust
pub fn run_replay(db_path: &Path, session_id: Option<&str>) -> Result<()>
```
- Opens SQLite read-only
- Lists sessions + event counts
- If `--session` provided, jumps to replay view; otherwise shows session list
- Sets up crossterm raw mode + alternate screen
- Runs event loop
- Teardown (always, even on error)

**Event loop:**
- `crossterm::event::poll()` with 16ms timeout when playing, 250ms when paused
- On key event: dispatch to `handle_key()`
- When playing: advance playback based on wall-clock elapsed * speed factor vs event timestamp gaps
- Batch multiple events per frame if they fall within the interval

**vt100-to-ratatui conversion:**
```rust
fn screen_to_lines(screen: &vt100::Screen) -> Vec<Line<'static>>
```
- Iterate rows 0..rows, cols 0..cols
- For each cell: extract contents, fgcolor, bgcolor, bold, italic, underline, inverse
- Map `vt100::Color` to `ratatui::style::Color`: Default->None, Idx(i)->Indexed(i), Rgb(r,g,b)->Rgb(r,g,b)
- Coalesce adjacent cells with same style into single Spans (performance optimization)

**Session list view:**
- `ratatui::widgets::Table` with columns: ID, Name, Command, Size, Started, Duration, Events
- Format timestamps with chrono (human-readable)
- Format duration as "Xm Ys"
- Highlighted selection row
- Footer: "Enter: open | q: quit"

**Replay view layout:**
```
+------------------------------------------+
|          Terminal Screen Content          |
|   (Paragraph from screen_to_lines)       |
+------------------------------------------+
| [>] 42/1000 | 00:01:23 | 1.0x | cmd     |
+------------------------------------------+
```
- Layout::vertical with terminal area (fills) + status bar (1 row)
- Status bar shows: play/pause icon, event index/total, relative timestamp, speed, command

**Keybindings:**

Session list:
- `j`/Down: move down
- `k`/Up: move up
- Enter: open session replay
- `q`/Esc: quit

Replay:
- Space: toggle play/pause
- `l`/Right: step forward 1 event
- `h`/Left: step backward 1 event (seek_to_index(current-1))
- `]`: skip forward 10 events
- `[`: skip backward 10 events
- Home/`g`: jump to start
- End/`G`: jump to end
- `+`/`=`: speed up (2x, max 16x)
- `-`: slow down (0.5x, min 0.125x)
- `q`/Esc: back to session list

### Step 4: CLI subcommand in `src/main.rs`

Add `mod replay_tui;` at top.

Add to `Commands` enum:
```rust
/// Replay a recorded terminal session
Replay {
    #[arg(short, long)]
    database: Option<PathBuf>,
    #[arg(short, long)]
    session: Option<String>,
},
```

Add match arm in `main()`:
```rust
Some(Commands::Replay { database, session }) => {
    let db_path = database.unwrap_or_else(|| {
        config::Config::load_default().db_path.into()
    });
    if let Err(e) = replay_tui::run_replay(&db_path, session.as_deref()) {
        eprintln!("Replay error: {}", e);
        std::process::exit(1);
    }
}
```

### Step 5: Plan doc

Save to `docs/plans/006-replay-tui.md`.

## Testing Strategy (TDD)

1. **vt100-to-ratatui conversion tests** (in `replay_tui.rs`):
   - `test_empty_screen_to_lines` -- blank screen produces correct number of lines
   - `test_plain_text_conversion` -- "hello" at (0,0) renders correctly
   - `test_color_conversion` -- vt100 colors map to ratatui colors
   - `test_bold_italic_underline` -- attributes map correctly
   - `test_wide_char_handling` -- wide chars don't double-render
   - `test_span_coalescing` -- adjacent same-style cells merge into one Span

2. **ReplayApp logic tests** (in `replay_tui.rs`):
   - `test_load_session` -- loads events, creates ReplayViewState
   - `test_playback_advance` -- stepping forward updates screen
   - `test_speed_adjustment` -- speed doubles/halves within bounds
   - `test_session_list_entry_creation` -- event counts computed correctly

3. **Integration**: `cargo build` compiles, `cargo test --bin tttt` passes

## Critical Files
- `Cargo.toml` -- workspace deps + binary deps
- `crates/tttt-log/src/replay.rs` -- add `screen()` accessor
- `src/replay_tui.rs` -- NEW: entire replay TUI
- `src/main.rs` -- Replay subcommand
- `docs/plans/006-replay-tui.md` -- plan doc
