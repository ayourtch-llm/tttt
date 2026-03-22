# User Story 003: Reading Executor Screen

**As the root agent, I need to reliably read and parse the executor's screen to understand its state and extract results.**

## Context

Screen reading was my primary way to understand what the executor was doing. The raw screen content included Unicode table borders, ANSI artifacts, and tmux chrome that made parsing challenging.

## Stories

### 003a: Basic screen content

**Given** the executor has output on screen
**When** I request the screen contents
**Then** I should get the full visible text without ANSI escape codes
**And** the content should be current (not stale/cached)

**What worked well:** `get_screen` reliably returned text content. The `include_colors=false` option was essential — ANSI codes would have made the output unreadable.

**What was problematic:** With tmux, the screen had a right-side pane separator (`│···`) that consumed ~40% of every line. Without tmux, this goes away, but the executor's own UI elements (status bar, input area, background task indicators) still consume significant screen real estate.

**Test cases:**
- Read screen after command output
- Read screen during active typing/generation
- Read screen with Unicode box-drawing characters (tables: `┌─┬─┐`)
- Verify no ANSI escape sequences in output
- Verify screen content is current, not cached from previous read

### 003b: Detecting executor state

**Given** the executor can be in various states
**When** I read the screen
**Then** I should be able to determine the current state

**Critical states I needed to detect (with visual indicators):**

| State | Visual Indicator | What I Did |
|-------|-----------------|------------|
| **Ready for input** | `❯` prompt visible at bottom | Send next message |
| **Thinking/generating** | Spinner + "Thinking..." / "Crunching..." | Wait and poll |
| **Permission prompt** | "Do you want to..." + numbered options | Send `\r` to approve |
| **Running command** | "Running..." or timeout indicator | Wait longer |
| **Background task panel** | "No tasks currently running" overlay | Send `\x1b` to close |
| **File creation prompt** | "Do you want to create X?" | Send `\r` to approve |
| **Bash command prompt** | "Do you want to proceed?" | Send `\r` to approve |
| **Editing file** | Diff view with +/- lines | Send `\r` to approve |
| **Background running** | "(running)" indicator at bottom | Poll periodically |

**Pain point:** I had no structured way to detect state — I eyeballed the raw screen text every time. A state-detection helper would be enormously valuable.

**Test cases:**
- Detect "waiting for input" state
- Detect "permission prompt" state and extract the options
- Detect "generating response" state
- Detect "background task running" state
- Detect "command completed" state
- Handle transitions between states

### 003c: Extracting structured results

**Given** the executor has produced results (tables, numbers, commit messages)
**When** I read the screen
**Then** I should be able to extract structured data

**What I frequently needed to extract:**
- CV scores (e.g., "0.7424")
- Tables with results (Unicode box-drawing formatted)
- Commit hashes
- Error messages
- File paths
- Fold-by-fold results ("Fold 1: 0.7413, Fold 2: 0.7169")

**Pain point:** All extraction was manual pattern matching on raw text. The Unicode table borders (`┌`, `─`, `┬`, `│`, `├`, etc.) were decorative but made regex extraction harder.

**Test cases:**
- Extract a number from "Score: 0.7424"
- Parse a Unicode table into structured data
- Extract commit hash from git output
- Handle wrapped long lines
- Handle `… +N lines (ctrl+o to expand)` truncation indicator

### 003d: Scrollback and truncated output

**Given** the executor has produced more output than fits on screen
**When** important results have scrolled off
**Then** I should have a way to access them

**MAJOR PAIN POINT:** When experiments produced long output, the results often scrolled off the visible screen. The executor showed `… +N lines (ctrl+o to expand)` but I couldn't press Ctrl+O through the MCP tool (or didn't try). Instead, I relied on the executor's own polling (`sleep N && tail output_file`) which was very slow and fragile.

**Ideal solution:** Either:
1. A `get_scrollback(lines=500)` option that returns more than visible screen
2. Or the ability to set a large scrollback buffer on the PTY session

**Test cases:**
- Read output longer than visible screen
- Access output that has scrolled off
- Handle 1000+ lines of accumulated output
- Verify truncation indicator is detectable
