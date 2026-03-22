# User Story 012: TextFSMPlus as the Interaction Engine

**As the tttt harness, I should use TextFSMPlus templates (from ayclic/aytextfsmplus) to drive the interaction state machine with executor agents, rather than hardcoding pattern matching in Rust.**

## Context

The `ayclic` project at `/Users/ayourtch/rust/ayclic/` already solves the exact problem tttt faces: driving an interactive text-based session by matching output patterns and sending appropriate responses. The `aytextfsmplus` crate provides:

1. **A PEG grammar** (`textfsm.pest`) for defining interaction templates
2. **A state machine** with states, regex-based rules, and transitions
3. **`Send` actions** — when a pattern matches, send text to the stream
4. **`Done` state** — interaction completed successfully
5. **`Error` state** — interaction failed
6. **`feed()` method** — incremental byte-by-byte feeding for prompt detection (no newline required!)
7. **Variable expansion** — `${Variable}` in send text, with aycalc expression evaluation
8. **Preset values** — inject variables (like passwords) into templates at runtime

The `drive_interactive()` function in `ayclic/src/path.rs` is the core loop:
```
read data → feed to FSM → match? → Send response / Done / Error / read more
```

This is EXACTLY what tttt needs for agent interaction, just with different templates.

## How It Maps to tttt

### Cisco IOS login (ayclic today):
```textfsm
Value Preset Username ()
Value Preset Password ()

Start
  ^[Uu]sername:\s* -> Send ${Username} WaitPassword
  ^[Pp]assword:\s* -> Send ${Password} WaitPrompt

WaitPassword
  ^[Pp]assword:\s* -> Send ${Password} WaitPrompt

WaitPrompt
  ^.*# -> Send "terminal length 0" TermLen
  ^.*> -> Send "terminal length 0" TermLen
  ^% -> Error "login failed"

TermLen
  ^.*# -> Done
  ^.*> -> Done
```

### Claude Code interaction (tttt would need):
```textfsm
Value Preset Message ()
Value PromptType ()
Value FileName ()

Start
  ^.*❯\s*$$ -> Send ${Message} WaitResponse

WaitResponse
  ^.*[⏺✻✶✳✽·✢].*\.\.\.  -> Continue WaitResponse
  ^.*Do you want to create\s+(\S+)\? -> Send "\r" WaitResponse
  ^.*Do you want to make this edit -> Send "\r" WaitResponse
  ^.*Do you want to proceed\? -> Send "\r" WaitResponse
  ^.*Command contains quote -> Send "\r" WaitResponse
  ^.*esc to interrupt -> Continue WaitResponse
  ^.*❯\s*$$ -> Done

WaitResponse_NoAutoApprove
  ^.*[⏺✻✶✳✽·✢].*\.\.\.  -> Continue WaitResponse_NoAutoApprove
  ^.*Do you want to -> Done
  ^.*❯\s*$$ -> Done
```

### Shell interaction:
```textfsm
Value Preset Command ()

Start
  ^.*[\$#%]\s*$$ -> Send ${Command} WaitOutput

WaitOutput
  ^.*[\$#%]\s*$$ -> Done
```

### Codex CLI interaction:
```textfsm
Value Preset Message ()

Start
  ^.*>\s*$$ -> Send ${Message} WaitResponse

WaitResponse
  ^.*Execute this command\? -> Send "y" WaitResponse
  ^.*Apply changes\? -> Send "y" WaitResponse
  ^.*working\.\.\. -> Continue WaitResponse
  ^.*>\s*$$ -> Done
```

## Stories

### 012a: Template-driven agent profiles

**Given** tttt needs to support multiple agent types
**When** defining agent interaction behavior
**Then** use TextFSMPlus templates instead of hardcoded Rust logic

**Benefits:**
- New agent types added by writing a template file, not recompiling
- The same battle-tested FSM engine from ayclic
- `feed()` handles prompts that don't end with newlines (critical for Claude Code)
- Variable substitution for dynamic content (`${Message}`)
- State transitions capture the exact UI state machine from user story 007

**Test cases:**
- Parse Claude Code template, verify states and transitions
- Parse Shell template, verify simpler state machine
- Parse Codex template, verify different permission patterns
- Invalid template → clear error message
- Template with undefined variable → error at send time

### 012b: The `feed()` advantage for prompt detection

**Given** AI agents often show prompts without a trailing newline
**When** the executor shows `❯ ` (prompt with cursor waiting)
**Then** `feed()` should detect it without waiting for newline

**This is critical!** The `feed()` method in TextFSMPlus processes the byte stream incrementally, matching against the accumulated buffer. Unlike `parse_line_interactive()` which needs complete lines, `feed()` can match partial lines — exactly what's needed for detecting `❯` prompts, `Password:` prompts, and `[Y/n]` prompts.

From the ayclic code:
```rust
// feed() works on accumulated buffer, not line-by-line
let result = fsm.feed(&buffer, vars, funcs);
if result.consumed > 0 {
    buffer.drain(..result.consumed);
}
match result.action {
    InteractiveAction::Send(text) => { /* send to PTY */ }
    InteractiveAction::Done => { /* interaction complete */ }
    InteractiveAction::Error(msg) => { /* handle error */ }
    InteractiveAction::None => { /* read more data */ }
}
```

**Test cases:**
- Feed "❯ " byte by byte, verify prompt detected
- Feed "Do you want to create foo.py?\n❯ 1. Yes\n" — verify permission detected
- Feed partial output, then more — verify correct accumulation
- Feed output with ANSI escapes — verify stripping/handling

### 012c: Auto-response via Send actions

**Given** a permission prompt matches a template rule
**When** the rule has a `Send` action
**Then** the response should be sent automatically

**This is how ayclic handles Cisco password prompts — the SAME mechanism handles Claude Code permission prompts:**

```
^[Pp]assword:\s* -> Send ${Password} WaitPrompt
```
becomes:
```
^.*Do you want to create -> Send "\r" WaitResponse
```

The `Send` action with variable expansion means:
- Static responses: `Send "\r"` (approve), `Send "y"` (yes)
- Dynamic responses: `Send ${Message}` (the user's message)
- Computed responses: `Send ${compute_response(Challenge)}` (via aycalc)

**Test cases:**
- Template with `Send "\r"` on permission match — verify \r is sent
- Template with `Send ${Message}` on prompt — verify message is sent
- Template with multiple Send actions in sequence — verify all sent in order

### 012d: State-aware interaction

**Given** the TextFSMPlus template defines multiple states
**When** the interaction progresses through states
**Then** the root agent should know the current state

**The state names themselves become semantic labels:**
- `Start` — waiting for initial prompt
- `WaitResponse` — agent is processing
- `WaitPermission` — permission prompt detected
- `BackgroundRunning` — command went to background
- `Done` — interaction complete

**This directly implements the UI state machine from user story 007, but declaratively rather than imperatively.**

**Test cases:**
- Query current FSM state during interaction
- Verify state transitions match expected sequence
- Handle unexpected state (rule doesn't match any pattern in current state)

### 012e: Template hot-reload

**Given** a user wants to adjust interaction patterns during a session
**When** they edit a template file
**Then** new interactions should use the updated template

**Why:** During our 8-hour session, I would have loved to adjust the auto-approve patterns without restarting. Template hot-reload enables this.

**Test cases:**
- Modify template file, verify next interaction uses new patterns
- Invalid template edit → keep using previous valid template + log error
- Add new state to template mid-session

### 012f: Template composition

**Given** common patterns repeat across agent templates
**When** defining a new agent profile
**Then** it should be possible to compose templates from reusable fragments

**Example fragments:**
- `permission_auto_approve.textfsm` — common permission handling
- `git_operations.textfsm` — git commit/push prompts
- `background_task.textfsm` — background task panel navigation

These could be included/imported into agent-specific templates.

**Test cases:**
- Include a fragment into a main template
- Override a fragment's rule in the main template
- Fragment with its own states that integrate with main template
