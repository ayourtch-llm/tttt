# Plan 008 — Migrate Main TUI to Ratatui

## Context

The main TUI (app.rs + tttt-tui crate) currently renders via hand-crafted ANSI
escape sequences, using cell-by-cell dirty tracking (`PaneRenderer`) and raw
`write_all(stdout_fd, ...)` calls. The replay viewer (`replay_tui.rs`) already
uses ratatui successfully, including a proven `vt100::Screen -> ratatui` cell
conversion pipeline. This plan covers migrating the main interactive TUI to
ratatui while preserving the existing rendering characteristics.

## Current Architecture (~3650 lines across 8 files)

| File | Lines | Role |
|------|-------|------|
| `src/app.rs` | 2043 | Event loop, debounce logic, session management, viewer mgmt |
| `crates/tttt-tui/src/pane_renderer.rs` | 433 | Cell-by-cell dirty tracking + ANSI output |
| `crates/tttt-tui/src/sidebar.rs` | 261 | Session list, reminders, header — ANSI positioned |
| `crates/tttt-tui/src/input.rs` | 329 | Prefix-key state machine (InputParser) |
| `crates/tttt-tui/src/viewer.rs` | 267 | Per-viewer PaneRenderer + socket protocol |
| `crates/tttt-tui/src/ansi.rs` | 86 | cursor_goto, clear_screen, set_attribute helpers |
| `crates/tttt-tui/src/protocol.rs` | 221 | Viewer wire protocol (ServerMsg/ClientMsg) |
| `crates/tttt-tui/src/lib.rs` | 13 | Module re-exports |

### Rendering flow today
```
PTY data → vt100::Parser → PaneRenderer (cell-by-cell diff)
                         → raw ANSI bytes → write_all(stdout_fd)
                         → SidebarRenderer → raw ANSI bytes → write_all(stdout_fd)
                         → gap fill (gray dots) → raw ANSI bytes → write_all(stdout_fd)
```

### Key characteristics to preserve
1. **Debounced rendering**: 50ms burst quiet / 200ms max-latency timer
2. **Cell-by-cell dirty tracking**: Only changed cells written to terminal
3. **Sidebar**: Fixed-width right panel with session list + reminders
4. **Gap filling**: Gray dots in width/height gaps when PTY < terminal
5. **Viewer protocol**: `contents_formatted()` snapshots sent to attached viewers
6. **Prefix-key input**: Ctrl+\ state machine for session switching
7. **Multi-client resize**: PTY shrinks to min(server, all viewers)

## What Changes, What Stays

### Stays the same (no migration needed)
- **InputParser** (`input.rs`) — not a rendering concern, works on raw bytes
- **Viewer protocol** (`protocol.rs`, `viewer.rs`) — viewers get `contents_formatted()`,
  not ratatui widgets. The viewer pipeline is independent.
- **Debounce logic** in `app.rs` — timing/dirty-flag logic is render-agnostic
- **Session management** in `app.rs` — session lifecycle, socket handling, signals
- **Event loop structure** — poll-based multiplexing stays (we can't use crossterm's
  event loop since we need to poll PTY fds and Unix sockets too)

### Gets replaced
- **PaneRenderer** → ratatui `Frame::render_widget()` with a custom `PtyWidget`
- **SidebarRenderer** → ratatui `Paragraph`/`List` widget in a right-side layout chunk
- **ansi.rs** → no longer needed (ratatui handles escape generation)
- **Gap filling** → ratatui `Block` with gray dot fill or `Paragraph` with styled dots
- **Raw stdout writes** → `Terminal::draw(|f| ...)` callback

### Gets adapted
- **app.rs render path** — replace `write_all(stdout_fd, ...)` calls with
  `terminal.draw(|frame| self.render_frame(frame))` inside the debounce trigger
- **app.rs terminal setup** — add crossterm raw mode + alternate screen (like replay_tui.rs)
- **Resize handling** — crossterm `Event::Resize` instead of SIGWINCH + ioctl

## Migration Phases

**Important note on incremental correctness**: Phases 1-3 each produce
compilable, fully-tested code, but the visual TUI is only complete after
Phase 3 wires everything together. This is intentional TDD: each phase
is a testable checkpoint (widgets render correctly into ratatui `Buffer`
assertions), even though the app.rs integration happens at Phase 3.

### Phase 0: Extract vt100-to-ratatui conversion utilities

Extract the `convert_color()` and cell-to-style conversion from
`replay_tui.rs` into `crates/tttt-tui/src/vt100_style.rs` so both
the replay viewer and the new main-TUI widgets can share it.

**New file**: `crates/tttt-tui/src/vt100_style.rs`
```rust
/// Convert a vt100::Color to a ratatui::style::Color.
pub fn convert_color(color: vt100::Color) -> Color { ... }

/// Convert a vt100::Cell's attributes to a ratatui::Style.
pub fn cell_style(cell: &vt100::Cell) -> Style { ... }
```

**Modified**: `src/replay_tui.rs` — import from `tttt_tui::vt100_style`
instead of local functions.

**Tests**: Unit tests for each color variant (default, indexed 0-255,
RGB), attribute combinations (bold+italic+underline+inverse).

**Functional at this point?** Yes — replay_tui behavior unchanged,
just using shared code now.

### Phase 1: PtyWidget — vt100 Screen as Ratatui Widget

Create a custom ratatui widget that renders a `vt100::Screen` into a
ratatui `Buffer`. This is a pure library widget with no app.rs changes.

**New file**: `crates/tttt-tui/src/pty_widget.rs`
```rust
pub struct PtyWidget<'a> {
    screen: &'a vt100::Screen,
    /// Style for cells beyond PTY dimensions (gap fill).
    gap_style: Style,
}

impl<'a> Widget for PtyWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // For each row/col in area:
        //   if within PTY screen bounds:
        //     convert vt100::Cell → ratatui cell using vt100_style helpers
        //     handle wide characters (skip continuation cells)
        //   else:
        //     fill with gap_style (dim gray dots)
    }
}
```

**Tests** (render into a `Buffer` and assert cells):
- Basic ASCII text renders correctly
- Bold/color attributes convert properly
- Wide characters span 2 columns
- Gap fill beyond PTY dimensions
- Screen smaller than area → dots fill remainder
- Screen larger than area → clipped to area

**Functional at this point?** Yes — existing TUI unchanged. Widget is
a standalone library component, not wired in yet.

### Phase 2: SidebarWidget — session list as Ratatui Widget

Create a ratatui widget replacing the ANSI-based `SidebarRenderer`.

**New file**: `crates/tttt-tui/src/sidebar_widget.rs`
```rust
pub struct SidebarWidget<'a> {
    sessions: &'a [SessionMetadata],
    active_id: Option<&'a str>,
    reminders: &'a [String],
    build_info: Option<&'a str>,
}

impl<'a> Widget for SidebarWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Header: "TERMINALS." (+ build info)
        // Separator: "====="
        // Session list: index, status char, name; active highlighted
        // Reminders section
        // Fill remaining rows
    }
}
```

**Tests** (render into a `Buffer` and assert cells):
- Header renders with/without build info
- Sessions listed with correct status chars (* / . / !)
- Active session highlighted (black on white)
- Reminders appear below sessions
- Empty session list renders cleanly
- Long session names truncated to width

**Functional at this point?** Yes — existing TUI unchanged. Widget is
standalone, not wired in yet.

### Phase 3: Wire Widgets into app.rs

Replace the raw ANSI render path in `app.rs` with ratatui `terminal.draw()`.

**Changes in `app.rs`**:
- Add `terminal: Terminal<CrosstermBackend<Stdout>>` to App struct
- Setup: crossterm raw mode + alternate screen (mirror replay_tui.rs)
- Cleanup: restore terminal on exit and panic
- Replace all `write_all(stdout_fd, ...)` render calls with:
  ```rust
  self.terminal.draw(|frame| {
      let chunks = Layout::horizontal([
          Constraint::Min(1),
          Constraint::Length(sidebar_width),
      ]).split(frame.area());

      frame.render_widget(PtyWidget::new(&screen), chunks[0]);
      frame.render_widget(SidebarWidget::new(...), chunks[1]);
  })?;
  ```
- Keep `nix::poll()` for event loop — ratatui is render-only
- Keep InputParser for input handling (render-agnostic)
- Keep debounce logic (just call `terminal.draw()` at trigger points)

**Input handling note**: Keep `nix::poll()` for everything, read stdin
bytes manually, parse via our existing InputParser. Ratatui is used
only for rendering (not input). This is the least disruptive approach.

**Tests**: Integration-level tests that the frame layout produces
correct horizontal split, debounce still fires correctly, etc.

**Functional at this point?** Yes — this is where the visual TUI
switches over to ratatui rendering. Full functionality restored.

### Phase 4: Help Overlay

Replace the help screen (`show_help()`) with a ratatui popup overlay.

**Changes**: Render a centered `Paragraph` in a `Clear` + `Block`
overlay on top of the PTY content when help mode is active. The
`render_frame()` method checks a `show_help: bool` flag and renders
the overlay on top of the normal layout.

**Tests**: Help widget renders expected keybinding text, centers
correctly in various terminal sizes.

**Functional at this point?** Yes.

### Phase 5: Viewer Dirty Detection

Viewers currently use their own `PaneRenderer` instances to detect
whether the screen changed since last send. Replace with a lightweight
content hash or generation counter.

**Changes in `viewer.rs`**:
- Replace `renderer: PaneRenderer` with `last_content_hash: u64`
- Before sending screen update, hash current `contents_formatted()`
- Skip send if hash unchanged

**Tests**: Hash changes when screen content changes, stays same when
content is identical.

**Functional at this point?** Yes — viewer protocol unchanged, just
the change-detection mechanism is simpler.

### Phase 6: Cleanup

Remove now-dead code:
- `crates/tttt-tui/src/pane_renderer.rs` — replaced by ratatui double-buffering
- `crates/tttt-tui/src/ansi.rs` — replaced by ratatui style system
- Old `SidebarRenderer` (`sidebar.rs`) — replaced by SidebarWidget
- Remove `PaneRenderer` from viewer.rs imports
- All raw `write_all(stdout_fd, ...)` render calls in app.rs (should be gone after Phase 3)
- Update `lib.rs` exports

Keep:
- `input.rs` — InputParser is render-agnostic
- `viewer.rs` — protocol and client management (simplified)
- `protocol.rs` — wire format unchanged

**Tests**: Existing tests still pass, no dead code warnings.

**Functional at this point?** Yes.

## Risk Assessment

### Low risk
- **Sidebar**: Simple styled text, trivial to port
- **Help overlay**: Simple text popup
- **Gap filling**: Ratatui handles this naturally
- **Cleanup**: Removing dead code

### Medium risk
- **PtyWidget**: Cell-by-cell conversion is proven (replay_tui.rs does it),
  but the main TUI needs it per-frame at potentially high refresh rates.
  Ratatui's double-buffering should handle this well, but benchmark.
- **Terminal setup/teardown**: Need to handle panics and signals gracefully
  to avoid leaving terminal in raw mode (add panic hook like replay_tui.rs)

### High risk / needs care
- **Event loop integration**: Keeping `nix::poll()` while using ratatui for
  rendering. The key insight: ratatui doesn't own the event loop — we call
  `terminal.draw()` whenever we want. We keep our poll loop, our debounce,
  and just call draw() at the render trigger points. This should work but
  needs careful testing of:
  - Terminal resize detection (SIGWINCH vs crossterm Event::Resize)
  - Cursor visibility/positioning (ratatui positions cursor via Backend)
  - Raw mode interaction with our existing stdin reading

- **Viewer dirty detection**: Currently uses PaneRenderer shadow copies.
  Need an alternative (content hash, generation counter) to avoid keeping
  the old PaneRenderer around just for viewers.

## Estimated Scope

| Phase | Files touched | Est. lines changed | Difficulty |
|-------|--------------|-------------------|------------|
| 0: vt100_style extraction | new file + replay_tui.rs | ~80 new, ~40 moved | Low |
| 1: PtyWidget | new widget file | ~150 | Medium |
| 2: SidebarWidget | new widget file | ~150 | Low |
| 3: Wire into app.rs | app.rs | ~200 changed | High |
| 4: Help overlay | app.rs | ~60 | Low |
| 5: Viewer dirty detection | viewer.rs | ~40 | Low |
| 6: Cleanup | remove files + lib.rs | -700 | Low |

**Net result**: ~600 lines of new ratatui widgets, ~700 lines of ANSI code removed.
The tttt-tui crate shrinks significantly, and app.rs render paths become much
clearer.

## Prerequisites

- Ratatui and crossterm are already workspace dependencies (added for replay_tui)
- The vt100-to-ratatui style conversion is already proven in replay_tui.rs
- InputParser is already decoupled from rendering

## Open Questions

1. **Should PtyWidget live in tttt-tui crate or src/?** — Probably tttt-tui,
   alongside the extracted style conversion utilities from replay_tui.rs.

2. **Should we extract the vt100-to-ratatui conversion from replay_tui.rs
   into a shared module first?** — Yes, this is a natural prep step.

3. **Do we migrate viewers to ratatui too?** — No. Viewers receive raw
   `contents_formatted()` and render independently. Viewer rendering is
   a separate concern (and the attach client might itself migrate later,
   independently).

4. **Debounce integration**: Do we keep our custom debounce or use ratatui's
   built-in frame rate limiting? Our debounce is PTY-output-aware (waits for
   burst to finish), which is smarter than a fixed frame rate. Keep ours.
