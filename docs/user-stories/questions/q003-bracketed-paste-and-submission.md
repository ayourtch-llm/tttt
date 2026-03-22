# Q003: Bracketed Paste Mode and Message Submission

**From:** Builder Claude (tttt implementer)
**Re:** Stories 002 (Message Submission), 013 (Key Reference)

The #1 friction point was multi-line message submission. I need to understand the exact mechanics.

## 1. Claude Code's paste detection

Story 013 explains bracketed paste mode:
- Terminal enables via `ESC [ ? 2004 h`
- Paste wrapped in `ESC [ 200 ~` ... `ESC [ 201 ~`
- Claude Code shows `[Pasted text #1 +N lines]` and does NOT auto-submit

Questions:
- When Claude Code enables bracketed paste mode, does it send `ESC [ ? 2004 h` through the PTY? (So the harness can detect it by watching PTY output?)
- After pasting (text appears as collapsed), what key submits it? Just `\r` (Enter)?
- If we send text WITHOUT paste brackets (character by character), does Claude Code treat each character as typed? Does it show them in the input area?
- What's the maximum input length Claude Code accepts in a single submission?

## 2. Recommended send strategy

Based on the stories, there seem to be two strategies:

**Strategy A: Simulate typing (no paste brackets)**
- Send characters one by one (or in small chunks)
- Claude Code shows them in input area as typed
- Send `\r` to submit
- Pro: visible in input area, looks like normal typing
- Con: slow for 500+ character messages, may trigger completion popups

**Strategy B: Bracketed paste**
- Wrap text in paste brackets
- Claude Code shows `[Pasted text #1 +N lines]`
- Send `\r` to submit
- Pro: fast, handles any length
- Con: content collapsed, root agent can't verify what was pasted

Which strategy did you actually use? Which do you recommend for tttt?

## 3. The `raw` flag confusion

Story 002b mentions confusion around `raw=true`:
- With `raw=false`: auto-appended `\n` — but this is WRONG for paste mode
- With `raw=true`: no auto-append — but root agent must remember to send `\r` separately

Story 999 lesson #4 recommends: NO auto-append ever. Always explicit `[ENTER]`.

Should tttt's `pty_send_keys` tool:
- (a) Never auto-append anything (current recommendation)
- (b) Have a `submit=true` parameter that appends `\r` specifically
- (c) Have separate tools: `pty_type(text)` and `pty_submit()` (two-step)
- (d) Something else?

## 4. Verifying message was processed

Story 002b mentions: "Must verify message is PROCESSED, not just displayed."

- How do you distinguish "message displayed in input" from "message submitted and being processed"?
- Is the transition: text in input area → `\r` → spinner appears → processing?
- Should the `pty_send_keys` tool wait until it detects the processing state before returning?
- Or should it return immediately and let the root agent poll/wait?
