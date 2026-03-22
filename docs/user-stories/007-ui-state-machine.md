# User Story 007: Executor UI State Machine

**As the root agent, I need a reliable mental model of the executor's UI states and how to transition between them.**

## Context

The biggest source of "getting stuck" was not knowing what UI state the executor was in, or sending the wrong key for the current state. A state machine model would prevent this.

## The State Machine I Discovered Empirically

```
                    ┌──────────────┐
                    │   READY      │ ← Main state, shows ❯ prompt
                    │  (awaiting   │
                    │   input)     │
                    └──────┬───────┘
                           │ send message
                           ▼
                    ┌──────────────┐
                    │  PROCESSING  │ ← Shows spinner + "Thinking..."
                    │  (thinking/  │   "Crunching...", "Baking...", etc.
                    │  generating) │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
    ┌──────────────┐ ┌──────────┐ ┌──────────────┐
    │  PERMISSION  │ │ RUNNING  │ │   RESPONSE   │
    │  PROMPT      │ │ COMMAND  │ │   COMPLETE   │
    │              │ │          │ │              │
    │ "Do you want │ │ Bash()   │ │ Shows text + │
    │  to create?" │ │ running  │ │ returns to   │
    └──────┬───────┘ └────┬─────┘ │ READY        │
           │              │       └──────────────┘
           │ \r            │
           ▼              │ timeout
    ┌──────────────┐      ▼
    │  APPROVED    │ ┌──────────────┐
    │  (executing) │ │ BACKGROUND   │
    │              │ │ TASK         │
    └──────┬───────┘ │              │
           │         │ "(running)"  │
           ▼         │ indicator    │
    ┌──────────────┐ └──────┬───────┘
    │   READY      │        │
    │  (back to    │        │ complete
    │   prompt)    │        ▼
    └──────────────┘ ┌──────────────┐
                     │ BG COMPLETE  │
                     │ notification │
                     └──────┬───────┘
                            │
                            ▼
                     ┌──────────────┐
                     │   READY      │
                     └──────────────┘

    SPECIAL STATES (overlays):

    ┌──────────────┐
    │ BG TASK      │ ← Triggered by ↓ arrow or background complete
    │ PANEL        │   Shows task list overlay
    │              │   Exit: Escape / \x1b
    └──────────────┘

    ┌──────────────┐
    │ EXPANDED     │ ← Triggered by ctrl+o
    │ OUTPUT       │   Shows full collapsed output
    │              │   Exit: Escape / \x1b
    └──────────────┘
```

## Stories

### 007a: State detection from screen content

**Given** the executor is in any of the above states
**When** I read the screen
**Then** I should be able to determine the exact state

**Detection heuristics I developed empirically:**

| State | Screen Indicators |
|-------|------------------|
| READY | Line starting with `❯` near bottom, no spinner |
| PROCESSING | Spinner character (`⏺✻✶✳✽·✢✶`) + verb ("Thinking", "Crunching", "Baking", "Moseying", etc.) |
| PERMISSION_PROMPT | "Do you want to" + numbered options (1. Yes, 2. ..., 3. No) |
| RUNNING_COMMAND | "Running..." text or timeout countdown |
| BACKGROUND | "(running)" in status bar at bottom |
| BG_TASK_PANEL | "↑/↓ to select · Enter to view · Esc to close" |
| RESPONSE_COMPLETE | New `❯` prompt after output text |

**Test cases:**
- Correctly identify each state from screen content
- Handle ambiguous states (e.g., "Do you want" in normal response text)
- Handle state transitions (detect when state changes)
- Handle rapid state changes (permission → approved → next permission in <1 second)

### 007b: Appropriate action per state

**Given** I know the executor's current state
**When** I want to take an action
**Then** I should know what keys/actions are valid

| State | Valid Actions | Invalid Actions |
|-------|-------------|-----------------|
| READY | Send message | \r (sends blank), \x1b (no effect) |
| PROCESSING | Wait, \x1b (interrupt) | Send message (queued or lost) |
| PERMISSION_PROMPT | \r (approve), \x1b (cancel), 2/3 (select option) | Send message |
| RUNNING_COMMAND | Wait, ^C (interrupt) | Send message |
| BACKGROUND | Wait, check with get_screen | Send message (goes to prompt) |
| BG_TASK_PANEL | \x1b (close), Enter (view task) | Send message (trapped) |

**Critical finding:** The BG_TASK_PANEL state TRAPS all input except Escape. I got stuck here during the session and had to figure out that `\x1b` (Escape) closes it. Any message sent while in this state is lost or corrupted.

**Test cases:**
- Send \r in PERMISSION_PROMPT state → approved
- Send \x1b in BG_TASK_PANEL state → panel closed
- Send message in READY state → processed
- Send message in BG_TASK_PANEL state → properly handled (not lost)
- Send \r in READY state with empty prompt → no blank submission

### 007c: State transition timeouts

**Given** the executor is in PROCESSING state
**When** it takes longer than expected
**Then** I should be able to estimate how long to wait

**Observed timing ranges:**

| Operation | Typical Duration | Max Observed |
|-----------|-----------------|-------------|
| Simple response | 5-30 seconds | 2 minutes |
| Script writing | 30-120 seconds | 3 minutes |
| Bash execution (in-process) | 10 seconds - 10 minutes | 10 min (timeout) |
| Bash execution (background) | 10 min - 2 hours | 2+ hours |
| Git commit | 5-15 seconds | 30 seconds |
| File read | 2-5 seconds | 10 seconds |
| Permission prompt appearance | 0-2 seconds after action | 5 seconds |

**Test cases:**
- Detect stuck state (no progress for 5+ minutes in PROCESSING)
- Detect bash timeout transition (RUNNING → BACKGROUND)
- Handle very fast transitions (permission appears and disappears in <1s)
