# User Story 014: Self-Injected Commands and Notifications

**As the root agent, I need mechanisms to manage my own state during long-running sessions — context compression, reminders, scratchpad, and task tracking.**

## Context

During the 8+ hour research session, I struggled with:
- Context window filling with polling noise
- Forgetting to check on long experiments
- Losing track of which experiments were done/pending
- No working memory that survives context compression

The harness can help by providing self-injection tools that the root agent calls on itself.

## Stories

### 014a: Context compression trigger

**Given** my context window is filling up with low-value content (sleep results, repeated screen reads)
**When** I decide I need to reclaim space
**Then** I should be able to trigger `/compact` or equivalent on my own session

**How it works:** The harness injects a `/compact` command (or equivalent) into the root agent's input stream, as if the human typed it. The root agent's CLI processes it normally.

**When I would have used this:** After the 4th hour, when I had hundreds of `sleep 300` → "completed" → `get_screen` → "still running" cycles consuming context. A single `/compact` would have summarized all that as "waited 2 hours for permutation test."

**Test cases (using a test program that accepts commands):**
- Inject `/compact` into root agent's stdin
- Verify the command is processed as if typed by user
- Verify it doesn't interfere with pending tool calls
- Handle injection while root agent is mid-response

### 014b: Scheduled reminders (cron injection)

**Given** I've started a long experiment on an executor
**When** I want to be reminded to check on it in 30 minutes
**Then** I should be able to set a reminder that injects a message into my session

**Example workflow:**
```
Me: "Set reminder: in 30 minutes, check permutation test progress"
→ Harness schedules a cron/timer
→ 30 minutes later, harness injects into my input:
  "[REMINDER] Check permutation test progress on executor A"
→ I see this as a new "user message" and act on it
```

**This replaces the awful pattern of:**
```
sleep(1800, run_in_background=true)  // launch timer
... do other work ...
// timer fires, I get notified, then manually check screen
```

**With reminders, the harness handles the timer AND the notification, saving context window tokens.**

**Reminder types I would have used:**

| Reminder | Timing | Message |
|----------|--------|---------|
| Experiment check | In 10 min | "Check oracle_deep_analysis.py progress" |
| Periodic poll | Every 5 min | "Poll executor A screen for completion" |
| Deadline | At 11:00 | "Wrap up current experiment, prepare summary" |
| Conditional | When executor idle | "Executor A finished — read results" |

**The conditional reminder (trigger when executor returns to prompt) would be the most valuable.** It eliminates ALL polling.

**Test cases:**
- Set reminder for 5 minutes, verify it fires
- Set recurring reminder every 2 minutes, verify multiple fires
- Cancel a pending reminder
- Set conditional reminder "when session X shows pattern Y"
- Verify reminder text appears in root agent's input stream
- Handle multiple simultaneous reminders
- Reminder fires while root agent is processing a tool call — queue it

### 014c: Private scratchpad (root agent only)

**Given** I need working notes that survive context compression
**When** I write to my scratchpad
**Then** the notes should be readable at any time, even after context window trimming

**This is different from:**
- **Memory** (saved to files, for future sessions)
- **Task list** (shared, structured)
- **Context** (ephemeral, compressed away)

**The scratchpad is for current-session working state:**
```
SCRATCHPAD:
- Current best: 0.7424 (correlation pruned + distill)
- Robust estimate: 0.738 (deflation corrected)
- Permutation test: running on executor A, started 11:08, ~200 iterations
- Next to try: competitor's outlier removal technique
- Key finding: oracle gap uniform, 80.6% stable, PLT is top routing feature
```

**Implementation:** A dedicated MCP tool (`scratchpad_write`, `scratchpad_read`) that stores text in a session-local file. NOT shared with executors — this is the root agent's private working memory.

**Test cases:**
- Write to scratchpad, read it back
- Overwrite scratchpad, verify update
- Append to scratchpad
- Read scratchpad after context compression (simulated)
- Scratchpad persists across tool calls
- Scratchpad is NOT accessible to executor sessions

### 014d: Task board

**Given** I'm managing multiple experiments across multiple executors
**When** I need to track what's done, what's running, what's next
**Then** I should have a structured task list

**Note for implementer:** Claude Code has `TodoWrite` and associated tools (check apchat for reference implementation). The task board should support:

- Create task with description and status
- Update task status (pending → in_progress → completed / failed)
- List all tasks with current status
- Associate tasks with executor sessions
- Priority ordering

**What my task board would have looked like:**
```
[DONE]       Age proxy experiment (executor A) → 0.7420
[DONE]       ELN/IPSS-M risk scores → neutral
[DONE]       Cross-disciplinary ideas → all neutral
[DONE]       Oracle deep analysis → ceiling 0.898, stability 80.6%
[RUNNING]    Permutation test (executor A) → 60/200 at 12:30
[NEXT]       Competitor's outlier removal
[NEXT]       Correlation pruning + distillation
[BLOCKED]    Submit to leaderboard (needs human to upload)
```

**Test cases:**
- Create task, verify it appears in list
- Update task status
- List tasks filtered by status
- Associate task with session ID
- Task persists across context compression
- Multiple concurrent tasks

### 014e: Executor completion notification

**Given** an executor has been running a long experiment
**When** the executor returns to its input prompt (experiment done)
**Then** the harness should notify the root agent immediately

**This is the "conditional reminder" from 014b, but important enough to call out separately.**

**How it works:**
1. Root agent registers: "Notify me when session X matches pattern `❯\s*$`"
2. Harness monitors session X's screen output in the background
3. When pattern matches, harness injects notification into root agent's input:
   "[NOTIFICATION] Executor A completed. Last output: '0.7424 new best!'"

**This completely eliminates the polling loop** that consumed 40% of tool calls.

**Test cases:**
- Register notification for prompt pattern on test session
- Run a 30-second command in test session
- Verify notification fires when command completes
- Verify notification includes last N lines of output
- Register notifications on multiple sessions simultaneously
- Notification while root agent is busy — queue and deliver when idle

### 014f: Session context limit detection

**Given** the root agent's conversation is approaching token limits
**When** Claude gives a context limit warning in the terminal
**Then** the harness should detect this and take action

**Detection:** Claude CLI shows a warning message when context is getting large (e.g., "Tip: Use /clear to start fresh when switching topics and free up context" — we actually saw this during the session at the 30+ minute mark of some responses).

**Possible actions:**
- Auto-trigger `/compact`
- Inject a warning message: "[HARNESS] Context limit approaching. Consider checkpointing."
- Save scratchpad + task board to disk as a recovery file

**Test cases:**
- Detect context warning pattern in root agent's terminal output
- Trigger configured action (compact / warning / checkpoint)
- Verify action doesn't disrupt current work
