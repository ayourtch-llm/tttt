# User Story 009: Logging, Replay, and Auditability

**As the root agent (and as the human overseeing the session), comprehensive logging enables debugging, reproducibility, and post-session analysis.**

## Context

Our session produced 60+ experiments over 8+ hours. Reconstructing what happened, in what order, and why specific decisions were made was only possible because the executor committed detailed messages. But the interaction log (what I sent, what I read, what I decided) was lost to context window compression.

## Stories

### 009a: Full interaction logging

**Given** a multi-hour research session
**When** the root agent sends messages and reads screens
**Then** every interaction should be logged with timestamps

**What should be logged per interaction:**
- Timestamp (ms precision)
- Session ID
- Direction (root→executor or executor→root)
- Content (keys sent, screen content read)
- Tool call ID (for correlation)
- Root agent's inferred state of the executor

**Why:** After our session, I couldn't reconstruct the exact sequence of experiments without reading git log. A proper interaction log would allow full session replay and analysis.

**Test cases (testing the logging infrastructure, not Claude):**
- Send 100 messages to a test program, verify all 100 are logged with timestamps
- Read screen 100 times, verify all reads are logged
- Verify logs are written to both text file and SQLite
- Verify log entries can be correlated by session ID
- Verify log file doesn't grow unbounded (rotation/compression)

### 009b: Session replay

**Given** a completed session with full logs
**When** someone wants to review what happened
**Then** the session should be replayable from logs

**Use case from real session:** The human went to sleep and came back to find 20+ experiments completed. They could read git log, but a full session replay showing the interaction flow would be much richer.

**Test cases:**
- Record a 10-minute session with a test program
- Replay the session from SQLite log
- Verify replay shows exact screen content at each point in time
- Support playback speed control (1x, 5x, 10x)
- Jump to specific timestamp in replay

### 009c: Experiment tracking

**Given** multiple experiments are run during a session
**When** the root agent wants to compare results across experiments
**Then** there should be structured experiment metadata

**What I tracked manually (and wish was automated):**
- Experiment name/description
- Start time, end time, duration
- CV score achieved
- Whether it beat previous best
- Script file created
- Git commit hash

**This was tracked informally in my context window and in commit messages. A structured experiment log would be far more useful.**

**Test cases:**
- Log structured metadata alongside PTY interactions
- Query experiments by score, duration, or timestamp
- Export experiment summary as JSON/CSV
- Handle experiments that error out (log the error, not just success)

### 009d: Audit trail for permissions

**Given** the root agent auto-approves or manually approves permissions
**When** someone reviews the session later
**Then** every permission decision should be logged

**Why:** If the executor does something unexpected (e.g., deletes a file), the audit trail shows exactly which permission was granted and by whom (auto-approved vs root agent approved).

**Test cases:**
- Log every permission prompt detected
- Log the response (approved, denied, auto-approved)
- Log the policy rule that triggered auto-approval
- Verify audit trail is tamper-evident (SQLite with checksums)
