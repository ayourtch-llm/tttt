# User Story 002: Message Submission to Executor

**As the root agent, I need to reliably send messages (often long, multi-line) to the executor Claude and have them submitted for processing.**

## Context

This was the #1 source of friction in our session. Claude Code CLI has specific input handling for pasted text that required careful workarounds.

## Stories

### 002a: Simple single-line message

**Given** the executor is at its input prompt (showing `>`)
**When** I send a short single-line message
**Then** it should be typed into the prompt and submitted

**What worked:** `send_keys("message text")` with auto-Enter worked for simple messages. No issues here.

**Test cases:**
- Send "hello" to a prompt, verify it appears and is submitted
- Send message with special characters (`"`, `'`, `\`, `$`)
- Send message with backticks (common in code discussions)

### 002b: Multi-line / long pasted text (CRITICAL)

**Given** the executor is at its input prompt
**When** I send a long message (100+ characters, or containing what looks like multiple lines)
**Then** it should arrive as a single pasted block and be submitted

**MAJOR PAIN POINT:** Claude Code CLI detects pasted text and shows `[Pasted text #1 +N lines]` instead of the raw text. The first attempt used `send_keys("long message")` which pasted correctly but did NOT auto-submit. I had to:
1. First send the text with `raw=true` (to prevent auto-Enter)
2. Then send `\r` with `raw=true` to submit

The initial confusion cost significant time — the message sat in the paste buffer and I kept trying Enter, Escape, Ctrl+C, none of which worked until we figured out `raw=true` + `\r`.

**Required behavior:** The tool should handle this transparently. When sending a long message to a Claude Code CLI session, it should:
1. Send the text (which will be detected as paste)
2. Automatically send `\r` to submit the pasted text

**Test cases:**
- Send 500-character message, verify it's submitted (not stuck in paste mode)
- Send message containing newlines
- Send message with code blocks (triple backticks)
- Send message with markdown formatting
- Verify the `[Pasted text #1]` indicator appears then resolves
- **Critical:** Verify the message is actually PROCESSED, not just displayed

### 002c: Rapid sequential messages

**Given** the executor is processing a previous message
**When** I send another message before the first completes
**Then** the second message should be queued or I should get clear feedback that the executor is busy

**Pain point:** I never tested this scenario because I was careful to wait, but it's a potential issue. If the executor is mid-response (showing "Thinking..." spinner) and I send a new message, what happens?

**Test cases:**
- Send message while executor is generating a response
- Send message while executor is running a Bash command
- Send message while executor shows a permission prompt
- Verify no message loss in rapid-fire scenario

### 002d: Special key sequences

**Given** various UI states in the executor
**When** I need to send control sequences
**Then** they should be transmitted correctly

**Keys I needed during the session:**
- `\r` (Enter/Return) — most common, for submitting text and approving prompts
- `\x1b` (Escape) — closing panels, canceling operations
- `^C` (Ctrl+C) — interrupting operations (didn't always work through tmux)
- `^U` (Ctrl+U) — clearing input line (didn't work)
- Arrow keys `[UP]`, `[DOWN]` — navigating menus (untested)

**Test cases:**
- `\r` submits at prompt
- `\x1b` closes background task panel
- `^C` interrupts a running command
- Escape from permission prompt
- Arrow key navigation in multi-choice prompts
