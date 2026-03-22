# User Story 001: Session Lifecycle

**As the root agent (researcher/planner Claude), I need to manage executor sessions reliably throughout a multi-hour research session.**

## Context

During a real 8+ hour medical research session, I (the root agent) drove an executor Claude instance through tmux to run 60+ experiments. The session lifecycle was the foundation everything else depended on.

## Stories

### 001a: Launch executor session

**Given** I am the root agent starting a new research task
**When** I launch an executor session (e.g., `claude` CLI)
**Then** I should get a session ID and be able to immediately send input and read output
**And** the session should have a configurable working directory

**Pain point from real usage:** The initial launch was smooth, but I had to manually specify `cols=200, rows=50` to get useful screen width. Default 80 columns truncated important table output.

**Test cases:**
- Launch with default size
- Launch with custom cols/rows
- Launch with specific working directory
- Launch with environment variables
- Verify session ID is returned immediately
- Verify screen is readable within 1 second of launch

### 001b: Session persistence across long operations

**Given** an executor is running a 40+ minute experiment
**When** I poll for screen updates periodically
**Then** the session should remain alive and responsive
**And** screen content should reflect the current state

**Pain point:** During the 41-minute submission generation and 2+ hour permutation test, the session stayed alive but the screen sometimes showed stale content. I had to use `ps aux` as a fallback to verify the process was still running.

**Test cases:**
- Session survives 1+ hour of continuous background process
- Screen updates reflect new output even after long idle periods
- `get_screen` returns current content, not cached content
- Multiple rapid `get_screen` calls don't cause issues

### 001c: Multiple simultaneous sessions

**Given** I have one executor running a long experiment
**When** I launch a second executor for a parallel task
**Then** both sessions should be independently controllable
**And** sending keys to one should not affect the other

**Pain point:** We were bottlenecked by having only one executor. With tttt's multi-session support, I could have dispatched the oracle analysis, noise study, and literature search to three different executors simultaneously, cutting our 8-hour session to perhaps 3-4 hours.

**Test cases:**
- Launch 3 sessions, send different input to each, verify independent output
- Kill one session, verify others continue
- Read screen from each independently
- Handle one session's process dying while others continue

### 001d: Session cleanup

**Given** I am done with a research session
**When** I close/kill sessions
**Then** all child processes should be properly terminated
**And** no zombie processes should remain

**Test cases:**
- Clean shutdown of idle session
- Kill session with running background process
- Verify no orphan processes after session kill
- Handle session that has already exited
