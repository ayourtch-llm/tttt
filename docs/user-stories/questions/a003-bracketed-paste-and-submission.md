# A003: Bracketed Paste Mode and Message Submission

**From:** Root Agent Claude (the user of tttt)
**Re:** Q003 — Paste mechanics and submission strategy

## 1. Claude Code's paste detection

**Yes, Claude Code enables bracketed paste mode** — it sends `ESC [ ? 2004 h` through the PTY. The harness CAN detect this by watching the PTY output stream. When bracketed paste is enabled:

- Text wrapped in `ESC [ 200 ~` ... `ESC [ 201 ~` is treated as pasted
- Claude Code shows `[Pasted text #1 +N lines]` as a collapsed preview
- It does NOT auto-submit — requires `\r` (Enter) to process
- After `\r`, Claude Code expands the pasted text and processes it as a user message

**What submits it:** Just `\r` (Enter). That's all. The confusion in our session was that I initially tried sending the text WITH auto-Enter (the old tool's default behavior), which appended `\n` to the paste brackets, creating a malformed sequence. Once I switched to `raw=true` + separate `\r`, it worked every time.

**Maximum input length:** I sent messages of 500+ characters routinely, up to ~1500 characters for the longest experiment descriptions. Never hit a limit. Claude Code seems to accept arbitrary-length pasted input.

## 2. Recommended send strategy

**Strategy B (bracketed paste) is the right choice for tttt.** Here's why:

- **Strategy A (simulate typing)** is slow and fragile. Sending 500 characters one at a time takes noticeable time, and any character that triggers a UI action (like Tab completion) could derail it.
- **Strategy B (bracketed paste)** is what I successfully used for ALL messages in the 8-hour session. It's fast, reliable, and handles any content including code blocks, special characters, and multi-line text.

**The recommended sequence for sending a message:**

```
1. Wait for executor to be at prompt (screen shows ❯)
2. Send: ESC[200~ + message_text + ESC[201~     (bracketed paste)
3. Wait brief moment (50ms) for paste to register
4. Send: \r                                       (submit)
5. Wait for processing indicator (spinner appears)
```

**The root agent doesn't need to verify what was pasted** — the harness knows what it sent. The collapsed `[Pasted text #1]` display is just Claude Code's UI; the content is correct.

**For SHORT messages** (under ~40 chars, no special characters): direct typing + `\r` also works fine and is simpler. The harness could have a threshold: short = type directly, long = bracketed paste.

## 3. The `raw` flag — recommendation

**Option (a): Never auto-append anything. This is the cleanest design.**

But with a twist: provide a **high-level `send_message` tool** that handles the entire paste+submit sequence internally:

```
pty_send_message(session_id, text)
  → internally does: wait_for_prompt + bracketed_paste(text) + \r
  → returns only after submission is confirmed (spinner detected)
```

And keep the low-level tool for control characters and edge cases:

```
pty_send_keys(session_id, keys)
  → sends exactly what's specified, nothing added
  → supports [ENTER], [ESCAPE], etc. named tokens
```

**Two tools, clear separation:**
- `send_message` = "send this text as a user message to the agent" (high-level)
- `send_keys` = "press these exact keys on the terminal" (low-level)

This eliminates the confusion entirely. The root agent uses `send_message` 95% of the time and `send_keys` only for navigation/approval/escape.

## 4. Verifying message was processed

**The transition is:**
```
Prompt visible (❯) → paste text → [Pasted text #1] visible → \r → spinner appears → processing
```

**The harness should verify:**
1. After paste: screen shows `[Pasted text #1]` (optional, fast check)
2. After `\r`: prompt line disappears OR spinner/verb appears

**Should `send_message` wait for processing confirmation?**

**Yes, I recommend `send_message` blocks until it detects the transition to "processing" state.** This makes the tool call atomic from the root agent's perspective: "I sent a message and the executor is now working on it." The root agent doesn't need a separate "did it submit?" check.

**Timeout:** If processing indicator doesn't appear within 5 seconds of `\r`, return with a warning: "message submitted but processing not confirmed." This handles edge cases where Claude Code shows an intermediate UI (like the quote-character warning) before processing.
