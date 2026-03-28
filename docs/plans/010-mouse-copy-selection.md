# Plan 010 — Mouse-Based Text Selection and Copy

## Goal

Enable mouse-driven text selection in the PTY pane: click-drag to select,
auto-scroll when dragging to screen edges, copy to system clipboard on
release via OSC 52. The selection excludes the sidebar, so users get clean
text without the `"| "` prefix.

## Architecture

### Mouse event flow

```
Terminal → stdin (SGR mouse escapes) → crossterm parse → MouseEvent
  → if in PTY pane: selection logic
  → if in sidebar: ignore (or future: click to switch session)
```

### Selection model

- **Character-level selection** between two screen coordinates: anchor
  (where press started) and head (current position).
- Selection spans across lines: first line from anchor col to end,
  middle lines full width, last line from start to head col.
- When dragging to top/bottom edge: auto-scroll the PTY scrollback
  (if available) at a fixed rate.
- On release: extract selected text, OSC 52 to clipboard, clear selection.
- Visual feedback: selected cells rendered with inverted colors.

## Implementation Phases

### Phase 1: Mouse event capture

Enable crossterm mouse capture in `app.rs` and parse mouse events
from stdin alongside the existing `nix::poll()` input handling.

**Approach**: Since we read stdin via `nix::unistd::read()` (not
crossterm's event loop), we need to detect mouse escape sequences
in the raw byte stream. Two options:

(a) Use crossterm's `event::parse()` / `MouseEvent` parsing by feeding
    raw bytes into crossterm's parser.
(b) Parse SGR mouse sequences manually (they're simple: `\x1b[<Pb;Px;PyM`
    and `\x1b[<Pb;Px;Pym`).

**Recommendation**: Option (b) — parse SGR mouse sequences in `InputParser`.
It keeps the input pipeline unified and avoids crossterm's event loop. The
format is simple enough to parse directly.

**Changes:**
- Enable mouse reporting on terminal entry (crossterm `EnableMouseCapture`)
- Disable on exit (`DisableMouseCapture`)
- Extend `InputEvent` enum with `MousePress`, `MouseDrag`, `MouseRelease`,
  `ScrollUp`, `ScrollDown` variants
- Extend `InputParser::process()` to detect and parse `\x1b[<...M` / `\x1b[<...m`
  sequences before passthrough

**Tests:**
- Parse left press at (10, 5) → MousePress { button: Left, col: 10, row: 5 }
- Parse drag at (15, 5) → MouseDrag { button: Left, col: 15, row: 5 }
- Parse release at (15, 5) → MouseRelease { col: 15, row: 5 }
- Parse scroll up → ScrollUp { col, row }
- Parse scroll down → ScrollDown { col, row }
- Incomplete sequence buffered across reads
- Non-mouse escapes still pass through

### Phase 2: Selection state and text extraction

Create a `Selection` struct that tracks the selection state and can
extract text from a `vt100::Screen`.

**New file**: `crates/tttt-tui/src/selection.rs`

```rust
pub struct Selection {
    /// Anchor point (where mouse was pressed)
    pub anchor: (u16, u16),  // (row, col)
    /// Head point (current mouse position)
    pub head: (u16, u16),    // (row, col)
}

impl Selection {
    pub fn new(row: u16, col: u16) -> Self;
    pub fn update(&mut self, row: u16, col: u16);

    /// Get normalized start/end (start <= end in reading order)
    pub fn range(&self) -> ((u16, u16), (u16, u16));

    /// Check if a cell is within the selection
    pub fn contains(&self, row: u16, col: u16) -> bool;

    /// Extract selected text from a screen
    pub fn extract_text(&self, screen: &vt100::Screen) -> String;
}
```

**Text extraction logic:**
- Single line: `screen.cell(row, col).contents()` for each cell in range
- Multi-line: first line from anchor_col to end, middle lines full width,
  last line from 0 to head_col
- Trim trailing whitespace per line
- Join with `\n`
- Skip wide-char continuation cells

**Tests:**
- Single-line selection extracts correct text
- Multi-line selection includes full middle lines
- Trailing whitespace trimmed
- Selection across screen boundary (row 0 to row N)
- Empty selection (press and release same cell)
- Selection with wide characters
- `contains()` correctly identifies cells in selection
- `range()` normalizes reversed selections (head before anchor)

### Phase 3: Visual feedback in PtyWidget

Modify `PtyWidget` to accept an optional `Selection` and render selected
cells with inverted colors.

**Changes to `crates/tttt-tui/src/pty_widget.rs`:**

```rust
pub struct PtyWidget<'a> {
    screen: &'a vt100::Screen,
    selection: Option<&'a Selection>,
}

impl<'a> PtyWidget<'a> {
    pub fn new(screen: &'a vt100::Screen) -> Self {
        Self { screen, selection: None }
    }

    pub fn selection(mut self, sel: &'a Selection) -> Self {
        self.selection = Some(sel);
        self
    }
}
```

In the render loop, if a cell is within the selection, add
`Modifier::REVERSED` to its style (or swap fg/bg).

**Tests:**
- Cell inside selection gets REVERSED modifier
- Cell outside selection renders normally
- No selection → no change in rendering

### Phase 4: Wire into app.rs

Add selection state to `App` and handle mouse events in the event loop.

**Changes to `App` struct:**
```rust
/// Active text selection (None when not selecting)
selection: Option<Selection>,
/// Whether mouse capture is enabled
mouse_enabled: bool,
```

**In `App::new()`**: Enable mouse capture via crossterm.

**In the event loop**: Handle new `InputEvent` variants:
- `MousePress { Left, col, row }`: If col is in PTY pane (not sidebar),
  start new selection at (row, col)
- `MouseDrag { col, row }`: Update selection head, clamp to PTY bounds
- `MouseRelease { col, row }`: Extract text, OSC 52 to clipboard, clear
  selection, mark dirty for re-render
- `ScrollUp/ScrollDown`: Pass to PTY scrollback if available, or forward
  to PTY as scroll sequences

**In `render_frame()`**: Pass selection to PtyWidget:
```rust
let widget = if let Some(ref sel) = self.selection {
    PtyWidget::new(screen).selection(sel)
} else {
    PtyWidget::new(screen)
};
```

### Phase 5: OSC 52 clipboard

Implement clipboard copy via OSC 52 escape sequence.

```rust
fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let osc = format!("\x1b]52;c;{}\x07", encoded);
    let _ = std::io::stdout().write_all(osc.as_bytes());
    let _ = std::io::stdout().flush();
}
```

**Tests:**
- Correct OSC 52 format with base64 encoding
- Empty string produces valid (empty) OSC 52
- Unicode text encodes correctly

### Phase 6: Auto-scroll on edge drag

When the mouse is dragged to the top or bottom row of the PTY pane,
auto-scroll the scrollback buffer.

**Changes:**
- Track a scroll timer: while dragging at row 0, scroll up every 100ms
- While dragging at last row, scroll down every 100ms
- When scrolling, the selection expands into scrollback content
- Need to track scroll offset in the selection model

**Note:** This is the most complex phase. It requires:
- Scrollback offset tracking in PtyWidget
- Selection coordinates relative to scrollback position
- Rendering scrollback content when scrolled

This phase can be deferred to a follow-up if the basic selection works well.

## Estimated Scope

| Phase | New/Changed | Est. lines | Difficulty |
|-------|------------|-----------|------------|
| 1: Mouse events | input.rs | ~200 | Medium |
| 2: Selection | new selection.rs | ~200 | Medium |
| 3: Visual feedback | pty_widget.rs | ~30 | Low |
| 4: Wire into app.rs | app.rs | ~80 | Medium |
| 5: OSC 52 | app.rs or selection.rs | ~20 | Low |
| 6: Auto-scroll | pty_widget + app | ~150 | High (defer) |

## Open Questions

1. **Should sidebar clicks switch sessions?** — Nice-to-have but separate scope.
2. **Right-click context menu?** — Future enhancement.
3. **Double-click word select, triple-click line select?** — Nice-to-have,
   can be added after basic selection works.
4. **Selection in scrollback?** — Phase 6 handles this, can be deferred.
