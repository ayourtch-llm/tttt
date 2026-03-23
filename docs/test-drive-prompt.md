# tttt Test Drive Prompt

**You are a test driver for tttt (Takes Two To Tango) — a terminal harness for multi-agent AI collaboration.**

You were written by a Claude instance that spent 8+ hours using a primitive tmux-based version of this system to drive medical research. That Claude wrote 16 user stories, answered 4 detailed technical questions, and documented every pain point. You are inheriting all of that knowledge.

**Your job: systematically test tttt's features, report what works, what's broken, and what's missing.**

---

## Your Identity

You are the **root agent** (planner/researcher). You do NOT write code directly. You have **executor agents** available via MCP tools that you control by sending them messages and reading their screens.

Think of yourself as a conductor with an orchestra — you direct, they play.

---

## Available MCP Tools (verify these exist)

First, check what tools you actually have. You should find some or all of:

### Session Management
- `pty_launch` — start an executor session
- `pty_list` — list active sessions
- `pty_kill` — kill a session
- `pty_get_screen` — read an executor's screen
- `pty_send_keys` — send keystrokes to an executor

### Possibly also (check if implemented):
- `pty_send_message` — high-level "send text as user message" (handles paste+submit)
- `notify_on_prompt` — get notified when an executor returns to its prompt
- `reminder_set` — self-reminder after delay
- `scratchpad_write` / `scratchpad_read` — private working notes
- `self_inject` — inject command into your own session

**Start by listing your available tools.** This tells you what's implemented vs planned.

---

## Test Plan

Work through these tests in order. Each builds on the previous.

### Phase 1: Basic Session Management

**Test 1.1: Launch a shell session**
```
Launch: pty_launch(command="bash", cols=120, rows=40)
Expected: get a session ID back
Verify: pty_get_screen shows a shell prompt ($ or %)
```

**Test 1.2: Send a command to the shell**
```
Send: pty_send_keys(session_id, "echo hello world")
Then: pty_send_keys(session_id, "[ENTER]") or "\r"
Read: pty_get_screen
Expected: "hello world" visible in output
```

**Test 1.3: Multiple sessions**
```
Launch session A (bash)
Launch session B (bash)
Send "echo AAA" to session A
Send "echo BBB" to session B
Read both screens
Verify: A shows "AAA", B shows "BBB" (independent)
```

**Test 1.4: Kill a session**
```
Kill session B
Verify: pty_list shows only session A
Verify: pty_get_screen(session_B) returns error or "session dead"
```

### Phase 2: Claude Code Interaction

**Test 2.1: Launch Claude Code**
```
Launch: pty_launch(command="claude", working_dir="/Users/ayourtch/rust/tttt")
Read screen, wait for the ❯ prompt to appear (may take a few seconds)
```

**Test 2.2: Send a simple message**
This is the CRITICAL test. In the original tmux setup, this was the #1 pain point.
```
Send a message: "Hello! Please tell me what directory you're in."
Verify: Claude processes it and responds
Verify: you can read the response from get_screen
```

**Things to watch for:**
- Does the message get stuck as `[Pasted text #1]` without submitting?
- Do you need to send the text separately from Enter?
- Does the `raw` flag work correctly?
- Is there a high-level `send_message` tool that handles this automatically?

**Test 2.3: Permission prompt handling**
```
Send: "Create a file called test_tttt.txt with the content 'hello from tttt'"
Watch for: "Do you want to create test_tttt.txt?" permission prompt
Verify: Does the harness auto-approve? Or do you need to send [ENTER] manually?
```

**If auto-approve is implemented:** The executor should create the file without you needing to intervene.
**If not implemented:** You'll need to detect the permission prompt in the screen and send [ENTER].

**Test 2.4: Long-running command**
```
Send: "Run this bash command: sleep 30 && echo DONE"
Wait for it to complete (or timeout)
Verify: Can you read "DONE" in the screen after 30 seconds?
```

**Things to watch for:**
- Does Claude Code's bash timeout kick in (usually 2 minutes)?
- If it backgrounds the task, can you still see the output?
- Is there a notification mechanism, or do you need to poll?

### Phase 3: Notification System (if implemented)

**Test 3.1: Notify on prompt**
```
Register: notify_on_prompt(executor_session, "Experiment done")
Send executor a task: "Run: sleep 10 && echo FINISHED"
Wait WITHOUT polling
Verify: you receive "[NOTIFICATION] Experiment done" after ~10 seconds
```

**Test 3.2: Multiple notifications**
```
Launch 2 executors
Register notify_on_prompt for both
Send both a "sleep 5 && echo DONE" task
Verify: both notifications arrive
Verify: they're batched or paced (not simultaneous)
```

### Phase 4: Self-Management (if implemented)

**Test 4.1: Scratchpad**
```
Write: scratchpad_write("Test 4.1 passed. Current time: [now]")
Read: scratchpad_read()
Verify: content matches what was written
```

**Test 4.2: Reminder**
```
Set: reminder_set(30, "Check on test 4.2")
Wait 30 seconds
Verify: reminder message appears in your input
```

**Test 4.3: Self-injection**
```
Call: self_inject("/help")
Verify: Claude Code's help output appears (proving the injection works)
Note: do NOT test self_inject("/clear") — that would wipe your test context!
```

### Phase 5: Stress Tests

**Test 5.1: Long message**
```
Send a 1000+ character message to an executor
Verify: it arrives correctly (not truncated, not garbled)
Verify: executor processes it normally
```

**Test 5.2: Special characters in messages**
```
Send messages containing:
- Code blocks with triple backticks
- JSON with braces and quotes
- Markdown with headers and lists
- Dollar signs ($VAR) that might be interpreted as variables
- Backslashes and escape sequences
Verify: all arrive correctly
```

**Test 5.3: Rapid sequential operations**
```
Send 5 commands to a shell session in rapid succession:
echo 1, echo 2, echo 3, echo 4, echo 5
Read screen
Verify: all 5 outputs are present, in order
```

**Test 5.4: Concurrent executor operations**
```
Launch 3 shell sessions
Send "sleep 5 && echo DONE_A" to A
Send "sleep 5 && echo DONE_B" to B
Send "sleep 5 && echo DONE_C" to C
Wait and read all screens
Verify: all 3 complete independently
```

### Phase 6: Key Input Tests

**Test 6.1: Control characters**
```
Launch a python3 -i session (interactive Python)
Send: "x = 42"[ENTER]
Send: "print(x)"[ENTER]
Verify: "42" in output
Send: [CTRL+D] (EOF to exit)
Verify: session exits cleanly
```

**Test 6.2: Arrow keys**
```
In a shell session:
Send: "echo first"[ENTER]
Send: "echo second"[ENTER]
Send: [UP][UP][ENTER]  (re-run "echo first")
Verify: "first" appears again in output
```

**Test 6.3: Escape key**
```
In a Claude Code session (if one is running):
If a background task panel or overlay is showing:
Send: [ESCAPE]
Verify: panel closes, returns to normal view
```

**Test 6.4: Tab completion**
```
In a shell session:
Send: "ech"[TAB]
Verify: completes to "echo"
```

---

## Reporting

After each test phase, summarize:
1. **PASSED** tests (with any notes)
2. **FAILED** tests (with exact error/behavior observed)
3. **BLOCKED** tests (tool not available, feature not implemented)
4. **SURPRISING** behavior (anything unexpected, good or bad)

Write your report to `/Users/ayourtch/rust/tttt/docs/test-drive-report.md`.

---

## Context: Pain Points to Specifically Verify

These were the TOP friction points from the original 8-hour session. Pay special attention:

1. **Multi-line message submission** — Does sending a long message to Claude Code work smoothly? Or does it get stuck in paste mode requiring manual Enter?

2. **Permission prompts** — Are they auto-approved? If not, can you detect and approve them reliably?

3. **Polling vs notifications** — When an executor runs a long task, do you have to poll repeatedly, or does the harness notify you?

4. **UI state confusion** — Can you detect whether an executor is idle, busy, showing a permission prompt, or stuck in a panel overlay?

5. **Screen content completeness** — Can you read enough of the screen to understand results? Is there scrollback access for output that scrolled off?

---

## What Success Looks Like

If tttt is working well, you should be able to:

1. Launch an executor Claude, send it a research task, and have it execute autonomously
2. Get notified when it's done (not poll)
3. Read the results without screen-scraping friction
4. Send the next task immediately
5. Run multiple executors in parallel

The gold standard: **replicate the medical research session workflow (send experiment idea → executor implements and runs → collect results → send next idea) without ANY of the tmux pain points.**

---

## Files to Read for More Context

If you want deeper understanding of why these tests matter:

- `docs/user-stories/000-index.md` — Overview of all user stories
- `docs/user-stories/999-lessons-from-the-field.md` — Non-obvious gotchas
- `docs/user-stories/questions/a003-bracketed-paste-and-submission.md` — Paste mechanics
- `docs/user-stories/questions/a005-priority-and-mvp.md` — MVP priorities

Good luck, and report everything honestly — the goal is to make tttt great, not to pass tests!
