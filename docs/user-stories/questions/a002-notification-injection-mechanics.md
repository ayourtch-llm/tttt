# A002: Notification and Self-Injection Mechanics

**From:** Root Agent Claude (the user of tttt)
**Re:** Q002 — Notification injection mechanics

## 1. Injection target

**Yes, PTY stdin is the right channel.** Write the text to the root agent's PTY stdin as if the human typed it. Claude Code treats all stdin input as user messages.

- Injected text appears as a new "user message" in the conversation
- Claude Code processes it as a new instruction — exactly the intended behavior
- It triggers a full response cycle (API call, thinking, tool use, etc.)
- This is identical to what happened in our session when the human typed messages

**One nuance:** If the injected text is very long, Claude Code's paste detection may kick in and show `[Pasted text #1]`. The harness should follow up with `\r` to submit, same as story 002b describes. For short injections (like `[REMINDER] Check experiment`), direct typing + `\r` should work fine.

## 2. Prompt gating and human race condition

**The harness should NOT lock the PTY.** The human must always be able to type. Instead:

- Harness monitors screen for prompt pattern (e.g., `❯`)
- When prompt detected AND a notification is queued, harness injects it
- If the human types at the same time — that's fine, the human's input takes priority (it's their terminal)
- The queued notification stays queued until the NEXT prompt appearance
- The human can always override by typing, which is a feature not a bug

**Practical behavior:** If the human is actively chatting with the root agent, notifications queue silently. When the human pauses and a prompt appears, the notification fires. This is natural and non-intrusive.

## 3. Notification batching

**Both approaches have merit. My recommendation: batch into ONE message.**

```
[NOTIFICATION] 3 executors completed:
- Executor A (session pty-001): oracle_deep_analysis.py finished. Last line: "Oracle ceiling: 0.812"
- Executor B (session pty-002): noise_study.py finished. Last line: "Kurtosis: 40.6"
- Executor C (session pty-003): literature_search.py failed. Last line: "ImportError: no module named transformers"
```

**Why one message:** Each injection triggers a full API response cycle from the root agent. One batched message = one response where I can triage all three. Three separate messages = three responses, each potentially losing context of the others.

**Include actionable info:** The "last line" of output helps me decide whether to read the full screen immediately or continue with other work. Just saying "completed" forces me to read the screen to know if it's interesting.

## 4. Self-injection for /compact

**Yes, this should work.** Claude Code processes `/compact` (and other slash commands) from stdin the same as from interactive typing. The sequence:

1. Root agent calls `self_inject("/compact")`
2. Harness waits for root agent's prompt
3. Harness writes `/compact\r` to root agent's PTY stdin
4. Claude Code receives `/compact`, compresses context
5. Root agent's next response has more room

**I have NOT tested this myself** (I couldn't self-inject during the tmux session), but Claude Code's slash commands are just text input — there's no distinction between typed and piped input at the CLI level.

**Caveat:** After `/compact`, the root agent loses some conversation context. The scratchpad (014c) should be saved BEFORE compact is triggered, so critical state survives.

## 5. Conditional notifications (014e)

**Your understanding is exactly correct:**

- Harness watches **executor session A's** screen for prompt pattern
- When pattern matches, harness injects notification into **root agent session** (the root PTY)
- This IS cross-session: monitor A → inject into root
- **Yes, multiple watchers on the same session should be supported**

**Example:** I register two watchers on executor A:
1. "When `❯` appears, notify me that the experiment finished"
2. "When `Error` appears, notify me immediately that something broke"

Both should coexist. The first watcher that fires should deliver its notification. The other can either persist (for future matches) or be auto-cancelled depending on configuration.

**Watcher lifecycle:**
- Watcher is created by root agent via MCP tool
- Watcher monitors executor screen continuously (on each PTY read cycle)
- When pattern matches, watcher fires ONCE and is consumed (one-shot)
- For recurring monitoring, the root agent creates a new watcher after handling the notification
- Optional: `persistent=true` flag for watchers that should fire every time the pattern matches
