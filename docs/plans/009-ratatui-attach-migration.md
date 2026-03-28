# Plan 009 — Migrate attach.rs to Ratatui

## Context

`src/attach.rs` (820 lines) is the `tttt attach` viewer client. It connects
to a running tttt instance via Unix socket, receives `ScreenUpdate` messages
containing `contents_formatted()` data, feeds them into a local
`vt100::Parser`, and renders via `PaneRenderer` + raw ANSI writes.

The main TUI (app.rs) was migrated to ratatui in Plan 008. This plan
migrates the attach client to match.

## Current Architecture

- **Terminal setup**: Manual nix termios (`RawMode` struct) + bracketed paste
- **Rendering**: `PaneRenderer::render()` → `write_fd(stdout_fd, ...)` + manual
  right-margin gap fill with raw ANSI
- **Input**: `nix::poll()` on stdin + socket fd, `process_paste_bytes()` state
  machine for detach key detection inside bracketed paste
- **Virtual screen**: `vt100::Parser` absorbs server updates; only flushed to
  real terminal when socket is drained (lazy sync)

## What Changes

### Replace rendering pipeline
- `PaneRenderer` + `write_fd()` → ratatui `Terminal::draw()` with `PtyWidget`
- Manual right-margin gap fill → `PtyWidget` handles gap fill natively
- Manual cursor positioning → `terminal.set_cursor_position()`
- `clear_screen()` → ratatui handles full redraws via double buffering

### Replace terminal setup
- `RawMode` struct (manual nix termios) → crossterm `enable_raw_mode()` /
  `disable_raw_mode()` + `EnterAlternateScreen` / `LeaveAlternateScreen`
- Bracketed paste: keep `\x1b[?2004h` / `\x1b[?2004l` — crossterm can
  handle this via `PushKeyboardEnhancementFlags` or we keep the raw writes

### Keep unchanged
- `process_paste_bytes()` state machine — this is input logic, not rendering
- Viewer protocol (`ClientMsg` / `ServerMsg`) — unchanged
- `nix::poll()` event loop — same approach as main TUI
- Reconnection logic (`attach_connect_with_retry`)
- Lazy sync pattern (buffer updates, render when socket drained)

## Implementation Phases

### Phase 1: Replace rendering in attach client

**Changes to `run_attach_loop()`:**
- Remove `stdout_fd` parameter
- Create `Terminal<CrosstermBackend<Stdout>>` at start of function
- Replace `PaneRenderer::new()` with just tracking `virtual_dirty` flag
- Replace the render block (lines 704-740) with:
  ```rust
  terminal.draw(|frame| {
      frame.render_widget(PtyWidget::new(virtual_screen.screen()), frame.area());
  })?;
  terminal.set_cursor_position((cursor_col, cursor_row))?;
  ```
- Remove right-margin gap fill code (PtyWidget handles it)
- Remove `write_fd()` calls for rendering
- Replace `clear_screen()` calls with terminal clear or just rely on ratatui

**Changes to `run_attach()`:**
- Replace `RawMode::enter()` with crossterm `enable_raw_mode()` +
  `execute!(stdout, EnterAlternateScreen)` + `execute!(stdout, EnableBracketedPaste)`
- Add cleanup in Drop or at function exit

### Phase 2: Remove dead code

- Remove `RawMode` struct
- Remove `write_fd()` function
- Remove `terminal_size()` function (crossterm provides `crossterm::terminal::size()`)
- Remove `PaneRenderer` import
- Remove `clear_screen`, `cursor_goto` imports

### Testing
- Existing 16 `process_paste_bytes` tests stay unchanged
- Add test for crossterm terminal size → PTY resize flow
- Verify viewer_integration tests still pass
