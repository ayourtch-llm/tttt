# User Story 004: Permission Prompt Handling

**As the root agent, I need to detect and respond to the executor's permission prompts efficiently.**

## Context

Claude Code CLI asks for permission before file creation, file editing, bash commands, and git operations. During our session, I estimate I approved 80+ permission prompts manually. This was the single biggest time sink — each approval required: read screen, detect prompt, send `\r`, wait, verify.

## Stories

### 004a: Detect permission prompt

**Given** the executor wants to perform an action requiring permission
**When** a permission prompt appears
**Then** I should be able to detect it programmatically

**Common permission prompt patterns observed:**

```
Do you want to create age_proxy.py?
❯ 1. Yes
  2. Yes, allow all edits during this session (shift+tab)
  3. No
```

```
Do you want to make this edit to oracle_phase3.py?
❯ 1. Yes
  2. Yes, allow all edits during this session (shift+tab)
  3. No
```

```
Do you want to proceed?
❯ 1. Yes
  2. Yes, and don't ask again for: git add:*
  3. No
```

```
Do you want to proceed?
❯ 1. Yes
  2. Yes, allow reading from Challenge-data/ from this project
  3. No
```

**The `❯` cursor always indicates which option is selected (default: Yes).**

**Test cases:**
- Detect "Do you want to create X?" prompt
- Detect "Do you want to make this edit?" prompt
- Detect "Do you want to proceed?" prompt (bash)
- Detect "Command contains quote characters" warning prompt
- Distinguish permission prompt from regular output containing "Do you want"
- Extract the file name from the prompt
- Extract the available options

### 004b: Auto-approve common operations

**Given** a permission prompt appears for a routine operation
**When** the operation matches a pre-approved pattern
**Then** it should be approved automatically without root agent intervention

**During our session, I approved EVERY SINGLE prompt with option 1 (Yes). I never once said No. The categories were:**
- File creation (new .py scripts): ~20 times
- File edits (bug fixes, updates): ~10 times
- Bash commands (running scripts): ~20 times
- Git operations (add + commit): ~20 times
- File reads (reading competitor code): ~10 times

**Ideal behavior:** A configurable auto-approve policy:
- Always approve file creation in the working directory
- Always approve bash commands matching `uv run --script *.py`
- Always approve git add + commit
- Always approve file reads
- Prompt root agent only for destructive operations (git push, rm, etc.)

**Test cases:**
- Auto-approve file creation within project directory
- Auto-approve `uv run --script` commands
- Auto-approve git commit (but NOT git push)
- Require approval for commands with `rm`, `kill`, `reset --hard`
- Configurable approval patterns via policy file
- Log all auto-approved actions for auditability

### 004c: "Allow all during session" option

**Given** a permission prompt with "Yes, allow all edits during this session" option
**When** I select option 2 instead of option 1
**Then** subsequent similar prompts should not appear

**Pain point:** I always chose option 1 (Yes) instead of option 2 (Yes, allow all). This was because I was being cautious, but it meant 80+ individual approvals. The "allow all" options exist:
- "Yes, allow all edits during this session (shift+tab)"
- "Yes, and don't ask again for: git add:*"
- "Yes, allow reading from X/ from this project"

Selecting option 2 early would have saved enormous time. But doing so requires sending specific keys to select option 2 (arrow down + Enter, or pressing `2`).

**Test cases:**
- Select option 2 by sending appropriate keys
- Verify subsequent prompts of same type are auto-approved
- Verify the "allow all" scope is correctly understood (session-level vs project-level)

### 004d: Permission prompt timeout

**Given** a permission prompt appears
**When** it is not responded to within a reasonable time
**Then** the executor should NOT proceed (default is deny)
**But** the root agent should be notified

**Pain point:** If I was distracted (waiting for a background timer) and didn't notice a permission prompt, the executor would just sit there indefinitely. No timeout, no notification. The session would appear "stuck."

**Test cases:**
- Permission prompt displayed for 5+ minutes without response
- Verify executor does not proceed without approval
- Verify root agent gets notified of pending prompts
- Handle multiple pending prompts (unlikely but possible)
