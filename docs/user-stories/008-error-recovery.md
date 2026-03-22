# User Story 008: Error Recovery and Resilience

**As the root agent, I need the harness to be resilient when things go wrong in executor sessions.**

## Context

Over 8+ hours, several things went wrong: processes hung, scripts errored, UI states got confused. The harness needs to handle these gracefully.

Note: Test cases should verify the PTY/MCP harness behavior, not the specific application being driven. Use simple test programs (shell scripts, interactive programs like `python3 -i`, `bc`, or custom test harnesses) to simulate the scenarios.

## Stories

### 008a: Process crash in executor

**Given** the executor's main process crashes or exits unexpectedly
**When** the root agent tries to interact with the session
**Then** the harness should report the session as dead, not hang

**Real scenario:** If the child process inside a PTY exits (e.g., `exit 1`), the session should be marked as terminated. Subsequent `send_keys` or `get_screen` should return appropriate errors.

**Test cases (using shell/test programs):**
- Launch `bash -c "sleep 2; exit 1"`, verify session reports exit after 2s
- Send keys to an exited session, verify clear error response
- Get screen from an exited session, verify it returns last screen content + exit status
- Launch program that segfaults, verify harness doesn't crash

### 008b: Hung process detection

**Given** a process in the executor appears to be hung (no output, high CPU)
**When** the root agent suspects a hang
**Then** the harness should provide tools to diagnose and recover

**Real scenario:** Our 41-minute submission script used 100% CPU with no output for long periods. I had to check `ps aux` through a separate command to verify it was alive.

**Test cases:**
- Launch `while true; do :; done`, verify harness remains responsive
- Get screen from session with hung process, verify it returns (doesn't block)
- Send `^C` to hung process, verify interrupt is delivered
- Kill session with hung process, verify clean cleanup

### 008c: PTY buffer overflow

**Given** a process produces enormous amounts of output rapidly
**When** the output exceeds normal buffer sizes
**Then** the harness should not lose data or crash

**Real scenario:** Some experiments produced thousands of lines of output. The vt100 screen buffer only holds the visible portion, but the raw output stream could be much larger.

**Test cases:**
- Run `seq 1 100000` in session, verify harness remains stable
- Get screen after large output, verify it shows recent content
- Verify scrollback buffer (if available) captures historical output
- Run process that outputs 10MB of text, verify no memory issues

### 008d: Concurrent access to session

**Given** the root agent makes multiple rapid tool calls to the same session
**When** send_keys and get_screen overlap
**Then** operations should be serialized safely

**Real scenario:** I sometimes sent keys and immediately read the screen. With the `Arc<Mutex<>>` shared SessionManager from the architecture doc, this should be safe, but the timing matters.

**Test cases:**
- Send keys and get_screen simultaneously (from two threads), verify no deadlock
- Rapidly alternate send_keys/get_screen 100 times, verify consistency
- Send keys to session A while reading screen from session B, verify independence
- Multiple get_screen calls to same session in rapid succession

### 008e: Harness crash recovery

**Given** the tttt harness itself crashes or is restarted
**When** executor sessions were running
**Then** sessions should either be recoverable or cleanly terminated

**Test cases:**
- Kill harness process, verify child PTY processes are also terminated (no orphans)
- Verify PID file or socket lock prevents duplicate harness instances
- If SQLite logging is active, verify log is consistent after crash
- Restart harness after crash, verify no stale session state
