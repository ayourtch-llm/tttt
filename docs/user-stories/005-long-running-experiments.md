# User Story 005: Long-Running Experiments

**As the root agent, I need to manage experiments that take 5 minutes to 2+ hours, without losing track of progress or results.**

## Context

This was the second biggest challenge. Our experiments ranged from 2 minutes (simple CV) to 2+ hours (200 permutation tests). The executor Claude had its own strategies (background tasks, sleep+tail polling) but they were fragile and slow.

## Stories

### 005a: Monitoring running experiments

**Given** the executor has started a long-running script
**When** I want to check progress
**Then** I should be able to see current output without disrupting the process

**What happened in practice:**
1. Executor starts `uv run --script experiment.py`
2. Script runs for 2-10 minutes within Claude Code's bash timeout
3. If it exceeds 10 minutes, Claude Code backgrounds it automatically
4. Executor then polls with `sleep N && tail output_file`
5. I read the screen to see the tail output
6. If still running, executor issues another sleep+tail

**This polling loop was EXTREMELY slow.** A typical long experiment (20+ minutes):
- Initial run: 10 min (timeout)
- First poll (sleep 300 + tail): wait 5 min, get partial results
- Second poll (sleep 600 + tail): wait 10 min, get more results
- Third poll (sleep 600 + tail): wait 10 min, get final results
- Total overhead: 25+ minutes of just WAITING for polls

**Ideal behavior:** Direct access to the background process output stream without the sleep+tail dance. Either:
1. A `get_process_output(pid)` tool that returns current stdout
2. A `wait_for_pattern(session_id, pattern, timeout)` tool that blocks until a pattern appears
3. A notification when a background process completes

**Test cases:**
- Monitor a 5-minute process without polling overhead
- Detect when a background process completes
- Read partial output from an in-progress experiment
- Handle process that produces output in bursts (nothing for minutes, then many lines)

### 005b: Background task detection

**Given** the executor's Bash command has timed out and gone to background
**When** the executor shows "Running in the background (↓ to manage)"
**Then** I should detect this state and adapt my polling strategy

**Pain point:** The transition from foreground to background was disruptive. The executor's screen changed to show a background task indicator, and subsequent interactions required navigating the background task UI. Sometimes the background task panel would overlay the main content, trapping keyboard input.

**Key indicators observed:**
- "Running in the background (↓ to manage)" — task just backgrounded
- "N bashes · Enter to view tasks · Esc to close" — task panel visible
- "uv run --script X.py (running) · ↓ to manage" — status bar indicator
- Background task panel shows "No tasks currently running" when done

**Test cases:**
- Detect foreground-to-background transition
- Navigate background task panel (view, close)
- Detect background task completion
- Handle multiple background tasks simultaneously
- Escape from background task panel to return to prompt

### 005c: Waiting efficiently

**Given** an experiment will take 20+ minutes
**When** I need to wait for results
**Then** I should not waste context window on polling loops

**Pain point:** My context window filled with hundreds of `sleep 300` / `sleep 600` tool calls and their results. Each poll cycle consumed tokens:
1. `Bash("sleep 300", run_in_background=true)` — launch timer
2. Timer notification — "completed"
3. `get_screen()` — read current state
4. Analyze state, decide to wait more
5. Repeat

Over the 8-hour session, I estimate 40% of my tool calls were just sleep/wait/check cycles. This is pure waste.

**Ideal behavior:** A `wait_for_completion(session_id, timeout)` or `wait_for_prompt(session_id, timeout)` that blocks until the executor returns to its input prompt, with periodic progress callbacks.

**Test cases:**
- Wait for prompt with 30-minute timeout
- Get periodic progress updates without explicit polling
- Handle timeout (process still running after max wait)
- Cancel a wait
- Wait for specific text pattern in output

### 005d: Experiment result extraction

**Given** a long experiment has completed
**When** the executor summarizes results
**Then** I should be able to extract the summary reliably

**What happened:** The executor would produce a summary table after each experiment:
```
============================================================
SUMMARY
============================================================

  Method                              CV Mean    Std
  ----------------------------------- --------  -------
  Pairwise distill (ref)               0.7415   0.0133
  Sleeping Experts (50%)               0.7330   0.0140
  ...

  Previous best (pairwise distill):  0.7415
```

But if the experiment produced a lot of output, this summary would scroll off screen, and I'd only see the commit message which was a compressed version.

**Test cases:**
- Extract summary from visible screen
- Extract summary that has scrolled off screen
- Parse tabular results into structured data
- Handle experiments that error out (extract error message instead)
