# Render Pipeline and Debouncing

## The Problem

Claude Code (and similar TUI agents) redraws its entire conversation history
on every message. In a naive implementation, this causes:

1. **Server terminal flood**: thousands of cell updates written to stdout on
   every message, causing visible scrolling/flicker
2. **Attach client flood**: the same cell updates sent over the Unix socket
   and rendered on the viewer's terminal (e.g., a phone), causing extreme
   scrolling on a small screen

## Architecture Overview

```
PTY (inner program, e.g., Claude Code)
  │
  │  raw bytes (ANSI escape sequences)
  ▼
Server vt100::Parser (screen buffer)
  │
  ├──→ Server Terminal (laptop)
  │     │
  │     │  debounced (50ms quiet / 200ms max)
  │     │  PaneRenderer (cell-by-cell dirty tracking)
  │     ▼
  │     Real terminal stdout
  │
  └──→ Viewer Clients (phone via tttt attach)
        │
        │  contents_formatted() sent over Unix socket
        │  (only when PaneRenderer detects changes)
        ▼
        Client vt100::Parser (virtual screen)
        │
        │  lazy sync (render only when socket drained)
        │  PaneRenderer (cell-by-cell dirty tracking)
        ▼
        Real terminal stdout
```

## Server-Side Debouncing (app.rs)

### Mechanism

When PTY output arrives, we **pump it into the vt100 screen buffer immediately**
but **defer rendering** to the real terminal. Rendering only happens when:

1. **Burst ended**: no new PTY data for `RENDER_DEBOUNCE_MS` (50ms), OR
2. **Max latency exceeded**: `RENDER_DEBOUNCE_MS * 4` (200ms) since the burst
   started, even if data is still arriving

### Implementation

```rust
const RENDER_DEBOUNCE_MS: u64 = 50;

// On PTY data arrival:
if !self.server_render_dirty {
    self.first_dirty_time = Some(now);  // burst start
}
self.server_render_dirty = true;
self.last_pty_data_time = Some(now);    // burst continuation

// On each loop iteration:
let burst_ended = last_pty_data_time.elapsed() >= 50ms;
let max_latency = first_dirty_time.elapsed() >= 200ms;
if burst_ended || max_latency {
    // render now
}
```

### Poll Timeout Adaptation

When a render is pending (`server_render_dirty == true`), the poll timeout
is reduced from 100ms to 10ms. This ensures we check the debounce condition
frequently enough for responsive rendering after the burst ends.

```rust
let poll_timeout_ms = if self.server_render_dirty { 10 } else { 100 };
```

### Why Two Conditions?

- **Burst-ended (50ms quiet)**: Handles the common case — a burst of updates
  (e.g., Claude Code redrawing history) finishes, and we render the final state
  50ms after the last update. Fast and smooth.

- **Max latency (200ms)**: Safety valve for continuous streams (e.g., `cat /dev/urandom`
  or a long compilation output). Without this, we'd never render during continuous
  output. The 200ms cap ensures the user sees progress.

### Rendering

When the debounce fires, we render using `PaneRenderer::render()` which does
cell-by-cell dirty tracking. Even after a full Claude Code history redraw,
most cells haven't changed from the previous render, so the actual terminal
output is minimal.

## Viewer Protocol (Unix Socket)

### What Gets Sent

The server sends `ServerMsg::ScreenUpdate` containing:

- `screen_data`: `vt100::Screen::contents_formatted()` — a replayable ANSI
  sequence that reproduces the exact screen state (including colors, attributes,
  and cursor position) when fed to any vt100 parser
- `cursor_row`, `cursor_col`: 0-indexed PTY cursor coordinates

### When It Gets Sent

The server's `update_viewers()` runs each event loop tick. For each viewer,
it calls `ViewerClient::send_screen_update()` which:

1. Runs `PaneRenderer::render()` on the server's screen for **dirty detection only**
2. If nothing changed (empty diff AND same cursor), **skips sending** entirely
3. If something changed, sends `contents_formatted()` — the full screen snapshot

This means:
- Idle screen = no messages sent (zero bandwidth)
- Small change (cursor move) = one message with full screen (but client renders
  only the diff)
- Big change (history redraw) = one message with full screen (debounced by server)

### Why contents_formatted() Instead of PaneRenderer Output?

We tried sending PaneRenderer output (cell-by-cell ANSI with `\x1b[row;colH`
positioning) directly. This failed because:

1. The PaneRenderer output is designed for a real terminal, not a vt100 parser
2. When fed into the client's vt100 parser, the cursor-goto sequences created
   confusing state (double cursor, corrupted content)
3. The PaneRenderer positions assume terminal width = PTY width, which may
   differ between server and client

`contents_formatted()` is the canonical way to serialize a vt100 screen state.
It includes `\x1b[H` (cursor home), `\x1b[J` (clear), all content with inline
attributes, and works correctly with any vt100 parser regardless of terminal size.

## Client-Side Lazy Sync (attach.rs)

### Mechanism

The attach client uses a **virtual screen** approach:

1. All incoming `ScreenUpdate` messages are applied to a local `vt100::Parser`
   (the "virtual screen") **immediately**
2. The real terminal is **only updated when the socket has been fully drained**
3. This means rapid updates are absorbed silently into the virtual screen

### Implementation

```rust
// Drain socket completely
loop {
    match stream.read(&mut tmp) {
        Ok(n) => { read_buf.extend(&tmp[..n]); got_server_data = true; }
        Err(WouldBlock) => break,  // socket empty
    }
}

// Process all pending messages into virtual screen
while let Some((msg, consumed)) = decode_message(&read_buf) {
    // Apply to virtual screen (fresh parser each time for clean state)
    virtual_screen = vt100::Parser::new(rows, cols, 0);
    virtual_screen.process(&screen_data);
    virtual_dirty = true;
}

// Only render when socket is quiet
if virtual_dirty && !got_server_data {
    let output = renderer.render(virtual_screen.screen());
    write_fd(stdout_fd, &output);
    virtual_dirty = false;
}
```

### Why Fresh Parser Each Time?

`contents_formatted()` is a **full screen snapshot**, not a diff. If we
accumulated multiple snapshots in the same parser, we'd get state corruption.
Creating a fresh parser for each snapshot ensures clean state.

### Why This Isn't a Time-Based Debounce

The client's `!got_server_data` check is **not** a time-based debounce. It's
a "process everything available before rendering" check:

- If the socket has data → process it, don't render yet
- If the socket is empty → render the virtual screen now

This means single updates render immediately (no added latency). Only bursts
of multiple updates get coalesced. This is fundamentally different from the
server's 50ms time-based debounce.

## Total Latency Analysis

For a single keystroke (best case):

```
User types key
  → 0ms: key forwarded to PTY
  → ~5ms: PTY echoes back
  → 0ms: server pumps into vt100 screen, marks dirty
  → 50ms max: server debounce fires (burst-end condition)
  → 0ms: PaneRenderer computes diff
  → 0ms: server renders to laptop terminal
  → 0ms: server sends ScreenUpdate to viewer
  → ~1ms: viewer receives, applies to virtual screen
  → 0ms: viewer PaneRenderer computes diff
  → 0ms: viewer renders to phone terminal

Total: ~55ms worst case, ~15ms typical
```

For a Claude Code history redraw (burst):

```
Claude Code starts redrawing
  → 0-500ms: PTY produces stream of ANSI updates
  → 0ms each: server pumps into vt100 screen, extends burst
  → 200ms max: server max-latency cap fires, renders intermediate state
  → 50ms after burst ends: server debounce fires, renders final state
  → server sends one or two ScreenUpdates total
  → viewer applies last update to virtual screen
  → viewer PaneRenderer computes minimal diff
  → viewer renders only changed cells

Total: ~250-550ms, but user sees clean final state, no scroll flood
```

## No Double Debouncing

From the attach client's perspective:

- Server debounce: 50ms time-based (controls when to send)
- Client lazy sync: drain-based, not time-based (controls when to render)

These are **complementary**, not redundant:

- Server debounce reduces the number of messages sent over the socket
- Client lazy sync coalesces multiple messages received in one poll cycle

A message that passes the server debounce is rendered immediately by the
client (if the socket is empty after reading it). There is no additional
time-based delay on the client side.

## Tmux-Style Terminal Resize

When multiple clients are connected with different terminal sizes:

1. Each client sends `ClientMsg::Resize { cols, rows }` on connect and SIGWINCH
2. `cols` = usable PTY width (client's full width, no sidebar subtraction —
   the client doesn't render a sidebar)
3. Server computes `min(server_cols - sidebar_width, client1_cols, client2_cols, ...)`
4. All PTY sessions resized to this minimum
5. Server clears and redraws its terminal (filling gap with gray dots)
6. All viewer renderers resized and invalidated
7. When a client disconnects, `resize_pty_to_min_and_redraw()` runs again,
   potentially expanding the PTY back up

## Gray Dots

When the PTY is narrower than the terminal (due to a smaller client connected):

- **Server (laptop)**: gap between PTY right edge and sidebar filled with
  dim gray dots (`\x1b[2;90m` + `.` repeated)
- **Client (phone)**: right margin filled with dim gray dots if PTY is
  narrower than client terminal (rare after resize, but handles edge cases)

## File Reference

| File | Role |
|------|------|
| `src/app.rs` | Server event loop, debounce logic, viewer management |
| `src/attach.rs` | Client event loop, virtual screen, lazy sync |
| `crates/tttt-tui/src/pane_renderer.rs` | Cell-by-cell dirty tracking |
| `crates/tttt-tui/src/viewer.rs` | Server-side viewer client, screen updates |
| `crates/tttt-tui/src/protocol.rs` | Wire protocol (ServerMsg, ClientMsg) |
| `crates/vt100/` | Vendored vt100 parser with scrollback_contents() |
