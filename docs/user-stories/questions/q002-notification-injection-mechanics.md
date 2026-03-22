# Q002: Notification and Self-Injection Mechanics

**From:** Builder Claude (tttt implementer)
**Re:** Stories 014 (Self-Injection), 015 (Rate Limiting)

The notification/injection system is one of the most impactful features (eliminates 40% of polling). I need to understand the mechanics precisely.

## 1. Injection target

When the harness injects `[REMINDER] Check experiment progress` into the root agent:

- Is this written to the root agent's PTY stdin (as if the human typed it)?
- Or is it delivered through a different channel (MCP notification, separate pipe)?
- If PTY stdin: the root agent (Claude Code) would see it as a new user message in the conversation. Is that the intended behavior?
- Does Claude Code treat injected text the same as human-typed text? Would it start processing it as a new instruction?

## 2. Prompt gating (story 015b)

You emphasize "wait for prompt before injection." In practice:

- The harness monitors the executor/root screen for prompt pattern (e.g., `❯`)
- When detected, the harness writes the injection to the PTY stdin
- But there's a race: the human might also type at the same time
- How should the harness handle this? Lock the PTY stdin during injection? Queue human input?

## 3. Notification batching

Story 015a describes batching multiple notifications within a window:

- If 3 executors complete within 5 seconds, batch into one message
- What does the batched message look like? Something like:
  ```
  [NOTIFICATION] 3 events:
  - Executor A completed (score: 0.7424)
  - Executor B completed (score: 0.7312)
  - Executor C failed (timeout after 600s)
  ```
- Or separate messages delivered with minimum interval between them?

## 4. Self-injection for /compact

Story 014a describes the root agent triggering `/compact` on its own session:

- The root agent calls `self_inject("/compact")`
- The harness writes `/compact\r` to the root agent's PTY stdin
- Claude Code sees `/compact` and compresses its context
- Does this work in practice? Has this been tested? Does Claude Code process `/compact` from stdin the same as from interactive typing?

## 5. Conditional notifications (014e)

The most powerful variant: "when executor idle, read results." This requires:

- Harness continuously monitors executor screen for prompt pattern
- When pattern matches, inject notification into ROOT agent (not executor)
- This is cross-session: watching session A's screen, injecting into session B (root)
- Is this the correct understanding?
- Can there be multiple watchers on the same session?
