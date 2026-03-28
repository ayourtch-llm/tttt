# Plan 011: C-Port Feature Backport

**Source:** user feedback
**Date:** 2026-03-28

## Scope

Items 1-10 from the feature delta document. Items 5 (alt screen resize safety),
6 (deferred wrap), and 8 (omitted) are skipped — Rust ownership and the `vte`
crate already handle 5 and 6.

## Actionable Items

| ID | Feature | Files touched | Effort |
|----|---------|---------------|--------|
| F1 | Ctrl+C escape hint | `tttt-tui/src/input.rs`, `src/app.rs` | S |
| F2 | CSI intermediate byte guard | `vt100/src/screen.rs` | S |
| F3 | Synchronized output (DEC 2026) | `vt100/src/screen.rs`, `src/app.rs` | M |
| F4 | Process group kill + bounded polling | `tttt-pty/src/backend.rs`, `tttt-pty/src/restored.rs` | M |
| F7 | Render debounce tuning (poll timeouts) | `src/app.rs` | S |
| F9 | `--debug-protocol` newline-delimited JSON-RPC | `tttt-mcp/src/proxy.rs`, `src/main.rs` | S |
| F10 | VT100 diagnostic tool (replay + report) | new: `src/diag.rs`, `src/main.rs` | M |

## Dependency Graph

```
         ┌─── F1 (Ctrl+C hint)         ── independent
         ├─── F4 (kill + bounded poll)  ── independent
Stage 1  ├─── F9 (debug protocol)      ── independent
         └─── F10 (VT100 diag tool)    ── independent

Stage 2  ├─── F2 (intermediate guard)  ── touches vt100/screen.rs
         └─── F3a (DEC 2026 mode bit)  ── touches vt100/screen.rs (after F2)

Stage 3  ├─── F3b (render suppression) ── depends on F3a (mode bit exists)
         └─── F7 (debounce tuning)     ── co-located with F3b in app.rs
```

Stage 1 items are fully independent — all four can run in parallel.
Stage 2 items both modify `vt100/src/screen.rs` — run sequentially (F2 then F3a).
Stage 3 items depend on Stage 2 and touch `src/app.rs` — run together.

## Stage 1: Independent Features (parallel)

### F1 — Ctrl+C Escape Hint

**Goal:** When user presses Ctrl+C 4 times within 2 seconds, show a transient
hint: "Press Ctrl+\ then q to detach from tttt"

**Implementation:**
1. In `tttt-tui/src/input.rs`, add `CtrlCTracker` struct:
   - Ring buffer of 4 `Instant` timestamps
   - `record() -> Option<InputEvent>` — push timestamp, if 4th within 2s of 1st, return `InputEvent::ShowCtrlCHint`
2. Add `InputEvent::ShowCtrlCHint` variant
3. In `InputParser::process()`, intercept byte `0x03` before passthrough, feed to tracker
4. In `src/app.rs`, handle `ShowCtrlCHint`:
   - Set `ctrl_c_hint_until: Option<Instant>` = now + 3 seconds
   - Render hint overlay in status line area during that window
5. **Tests:** Unit test the tracker with mock instants; test that <4 presses or >2s gap yields None.

### F4 — Process Group Kill with Bounded Polling

**Goal:** Replace fire-and-forget SIGTERM with bounded escalation:
SIGTERM → poll 150ms → SIGKILL + close master_fd → poll 500ms → give up.

**Implementation:**
1. Add `kill_with_escalation(&mut self) -> Result<()>` to the `PtyBackend` trait
   with a default impl that calls the existing `kill()`.
2. In `RealPty`: extract child PID via `portable_pty::Child::process_id()`,
   use `kill(-pid, SIGTERM)` for process group, then bounded poll loop.
3. In `RestoredPty`: same pattern, already has `child_pid` and `master_fd`.
4. Close `master_fd` before final SIGKILL wait (macOS kernel-read unblock).
5. Total worst case: 650ms, never blocks indefinitely.
6. **Tests:** MockPty test that verifies escalation sequence; integration test
   that spawns `sleep 9999` and confirms cleanup within 1s.

### F9 — `--debug-protocol` Newline-Delimited JSON-RPC

**Goal:** When `tttt mcp-server --connect SOCK --debug-protocol` is passed,
use `{json}\n` framing instead of length-prefixed binary.

**Implementation:**
1. Add `--debug-protocol` flag to the `McpServer` CLI args in `src/main.rs`.
2. In `tttt-mcp/src/proxy.rs`, add `send_and_receive_ndjson()`:
   - Write: `socket.write_all(request)`, `socket.write_all(b"\n")`
   - Read: `BufReader::read_line()`
3. In `run_proxy()`, branch on the debug flag to choose framing function.
4. Server side (app.rs MCP handler): also needs to support ndjson on its end
   when the connecting client uses it. Detect by peeking first byte (if `{`,
   it's ndjson; if 4 binary bytes, it's length-prefixed). Or: separate socket
   path / flag.
5. **Tests:** Unit test round-trip with ndjson framing.

### F10 — VT100 Diagnostic Tool

**Goal:** `tttt diag --database DB --session ID` replays a recorded session
through the vt100 parser and reports unhandled escape sequences.

**Implementation:**
1. Add `diag` subcommand to CLI in `src/main.rs`.
2. Create `src/diag.rs`:
   - Load session events from SQLite via `tttt-log`
   - Feed each output event byte-by-byte through a diagnostic vt100 parser
   - The diagnostic parser wraps `vt100::Parser` but hooks `vte::Perform`
     to log every CSI/OSC/DCS dispatch with params
   - Compare dispatched sequences against a "known handled" set
   - Print report: sequence, count, first occurrence offset
3. **Tests:** Feed known sequences, verify report output.

## Stage 2: VT100 Parser Changes (sequential)

### F2 — CSI Intermediate Byte Guard

**Goal:** Verify and ensure that CSI sequences with intermediate bytes
(`>`, `=`, `!`) don't get misinterpreted as standard CSI.

**Implementation:**
1. In `vt100/src/screen.rs`, check how `vte::Perform::csi_dispatch()` is
   called — the `intermediates` slice is already a parameter.
2. In the `csi_dispatch` impl, if `intermediates` is non-empty, log and
   return early (don't dispatch to sgr/decset/etc).
3. **Tests:** Feed `\x1b[>1u` and `\x1b[>4m` through the parser, verify
   cursor position and attrs are unchanged.

### F3a — DEC Mode 2026 State Tracking

**Goal:** Track synchronized output mode in the vt100 screen state.

**Implementation:**
1. Add `MODE_SYNCHRONIZED_OUTPUT` constant to modes.
2. In `decset()`: `&[2026] => self.set_mode(MODE_SYNCHRONIZED_OUTPUT)`
3. In `decrst()`: `&[2026] => self.clear_mode(MODE_SYNCHRONIZED_OUTPUT)`
4. Add public accessor: `pub fn synchronized_output(&self) -> bool`
5. **Tests:** Feed `\x1b[?2026h`, check mode is set; feed `\x1b[?2026l`,
   check mode is cleared.

## Stage 3: Render Integration (after Stage 2)

### F3b — Render Suppression During Sync Output

**Goal:** Skip rendering while the active session's screen has sync mode set.

**Implementation:**
1. In `src/app.rs`, in the render decision block (around line 858), add:
   ```rust
   if active_screen.synchronized_output() {
       continue; // suppress render during sync bracket
   }
   ```
2. When sync clears (next PTY data parse), normal debounce resumes.

### F7 — Render Debounce Tuning

**Goal:** Align poll timeouts with C version's tuning (10ms dirty / 50ms clean
is already what we do; ensure burst gap is 50ms and max latency is 200ms).

**Implementation:**
1. Current: `RENDER_DEBOUNCE_MS = 50`, max latency = `4 * 50 = 200ms` — already matches.
2. Poll timeout: currently 10ms dirty / 100ms clean. Change clean to 50ms
   per C version's finding (lower idle CPU, still responsive).
3. **Tests:** Existing debounce tests should pass with new value.

## Verification

After all stages: `cargo test --workspace` must pass, `cargo clippy` clean.
Manual smoke test: run `tttt`, launch Claude Code in a PTY, observe:
- No flicker during rapid output (F3b sync suppression)
- Ctrl+C hint appears on rapid presses (F1)
- `tttt diag` reports unhandled sequences (F10)
- `--debug-protocol` works with socat (F9)
