# Lessons From the Field

**Non-obvious gotchas discovered during 8+ hours of real two-Claude collaboration. These don't fit neatly into a single user story but will save the implementer debugging time.**

## 1. The screen is NOT the full output

The PTY screen buffer (e.g., 50 rows x 200 cols) only holds what's currently visible. Long experiment outputs scroll off and are gone. Claude Code shows `… +N lines (ctrl+o to expand)` for collapsed content. The root agent CANNOT press Ctrl+O reliably. **Consider capturing the full PTY output stream in addition to the screen buffer.**

## 2. The executor thinks out loud

AI agents don't just show prompts and results. They show thinking indicators with creative verb names: "Crunching...", "Baking...", "Moseying...", "Prestidigitating...", "Whatchamacalliting...". These are NOT predictable strings. Match on the spinner characters (`⏺✻✶✳✽·✢`) rather than the verbs.

## 3. Unicode everywhere

AI agent output is full of Unicode: box-drawing characters for tables (`┌─┬┐│├┤└┘`), emoji-like prompt symbols (`❯⏺✻`), and occasional actual emoji. The screen reader must handle UTF-8 correctly — byte-level processing will break on multi-byte characters.

## 4. The "auto-Enter" trap

The old MCP tool had a design where `send_keys` auto-appended Enter unless `raw=true`. This seems convenient but causes problems:
- Pasted text gets an unwanted Enter appended
- Control sequences like Escape get an Enter appended
- The root agent has to remember to set `raw=true` for anything non-trivial

**Recommendation:** Do NOT auto-append Enter. Make the root agent always explicit about what they're sending. `[ENTER]` as a named token is clearer than implicit behavior.

## 5. Permission prompts are blocking

When an AI agent shows a permission prompt, it blocks ALL processing until the prompt is answered. There's no timeout, no auto-deny. If the root agent doesn't notice the prompt (e.g., it's waiting for a background timer), the executor sits frozen indefinitely. **The harness monitoring the executor screen should detect permission prompts and alert the root agent immediately.**

## 6. Background tasks change the UI model

When Claude Code backgrounds a task, the UI fundamentally changes:
- A status bar appears at the bottom showing the background task
- Pressing ↓ opens a task panel overlay
- The overlay traps input (most keys don't work until Escape)
- Background completion triggers a notification that may overlay content

This is a state machine transition that needs to be handled explicitly, not as a special case.

## 7. The executor's context window matters too

The executor Claude also has a context window that fills up. After 60+ experiments, the executor's context was massive. Its responses got slower, and it showed "Tip: Use /clear to start fresh" hints. **If the root agent can detect this, it should consider starting a fresh executor session and re-priming it with a summary.**

## 8. Git operations need coordination

The executor commits frequently (after each experiment). If the root agent also tried to commit (or if multiple executors commit), git conflicts occur. **Rule: only the executor commits. The root agent never touches git in the working directory.**

## 9. Time is the real bottleneck

Most experiments ran in 2-10 minutes. But the overhead of:
- Sending the idea (30s)
- Executor writing code (1-2 min)
- Approving file creation (10s)
- Approving bash execution (10s)
- Waiting for results (2-10 min)
- Approving commit (10s)
- Reading and analyzing results (30s)

Total per experiment: 5-15 minutes. Over 60 experiments, that's 5-15 hours. **The harness should minimize ceremony (auto-approve) and maximize parallelism (multiple executors) to compress this.**

## 10. The root agent forgets

After ~4 hours of context, earlier experiment results start getting compressed away. The root agent may re-suggest ideas already tried, or forget the current best score. **The scratchpad (story 014c) and task board (story 014d) are not nice-to-haves — they're essential for sessions longer than 2 hours.**
