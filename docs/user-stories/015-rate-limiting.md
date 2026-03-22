# User Story 015: Injection Pacing and Throttling

**As the tttt harness that injects messages and notifications into terminal-based AI agents, I need to pace injections to avoid overwhelming the agents or causing them to miss/garble input.**

## Context

tttt does NOT make API calls directly — it drives CLI-based agents (Claude Code, Codex, etc.) via terminal I/O. However, each message injected into an agent's input stream causes that agent to process it, which internally triggers an API call to its provider. tttt can't see or control the API layer, but it CAN control how fast it injects messages.

The concern is NOT "API rate limiting" but rather:
1. **Don't inject a new message while the agent is still processing the previous one** — the message would be lost or garbled
2. **Don't fire multiple notifications at once** — each one triggers agent processing, and rapid-fire injections could confuse the agent or hit provider limits indirectly
3. **Add jitter to prevent synchronized bursts** — if 3 executors finish simultaneously, stagger the notifications

## Stories

### 015a: Injection pacing for notifications

**Given** the harness can inject reminders and notifications into the root agent's input
**When** multiple notifications are ready to deliver
**Then** they should be paced with delays between them

**Why:** If I register completion notifications on 5 executors and all 5 finish within seconds, injecting 5 messages rapidly would:
- The root agent processes message 1, generating a response
- Messages 2-5 arrive while the agent is mid-response — they're queued or lost
- When the agent finishes response 1, it sees messages 2-5 stacked up
- This creates confusion and potentially garbled context

**Better approach:** Queue notifications and deliver one at a time, only after the agent has returned to its input prompt.

**Configuration:**
```toml
[injection]
# Only inject when the target session is at its input prompt
wait_for_prompt = true
# Minimum delay between successive injections
min_interval_ms = 2000
# Random jitter to avoid predictable timing
jitter_ms = 1000
# Batch notifications arriving within this window into one message
batch_window_ms = 5000
```

**Batching example:**
Instead of 3 separate injections:
```
[NOTIFICATION] Executor A completed
[NOTIFICATION] Executor B completed
[NOTIFICATION] Executor C completed
```

Deliver one batched message:
```
[NOTIFICATION BATCH]
- Executor A: oracle_deep_analysis.py completed
- Executor B: noise_characterization.py completed
- Executor C: literature_search completed
```

**Test cases (using mock interactive programs):**
- Inject 3 notifications within 1 second — verify they're batched
- Inject notification while target program is "busy" — verify it waits for prompt
- Verify jitter is applied (timing is not deterministic)
- Single notification — verify minimum delay before delivery
- Batch window expires — verify accumulated notifications are delivered

### 015b: Wait-for-prompt before injection

**Given** the root agent is processing a previous response
**When** a notification or reminder is ready
**Then** the harness should wait until the agent shows its input prompt before injecting

**This is the most important pacing rule.** Injecting text while an agent is mid-generation:
- Could be interpreted as user interrupt
- Could be appended to the agent's output (garbled)
- Could be lost entirely

**The harness already knows how to detect prompts (via TextFSMPlus templates from story 012).** The same prompt detection used for executors should gate injections into the root agent.

**Test cases:**
- Start a long operation in test program, try to inject — verify injection waits
- Test program returns to prompt — verify queued injection is delivered
- Multiple injections queued — verify delivered one at a time with delays
- Test program crashes — verify queued injections are discarded (or delivered with warning)

### 015c: Executor send pacing

**Given** the root agent wants to send messages to multiple executors
**When** sending to executor A then immediately to executor B
**Then** a small delay with jitter should be inserted between sends

**Why:** Even though tttt sends to local terminals (not APIs), the executors themselves make API calls when they receive input. If the root agent fires messages at executors A, B, and C in the same millisecond, all three executors hit their respective API providers simultaneously. A small staggered delay (1-3 seconds with jitter) spreads the load.

**Test cases:**
- Send to 3 sessions in rapid succession — verify sends are staggered
- Verify jitter makes timing non-deterministic
- Single send — verify no unnecessary delay
- Configurable delay per session or globally

### 015d: Screen read throttling

**Given** the root agent may request screen reads frequently
**When** reading the same session multiple times in quick succession
**Then** return cached content if nothing has changed

**Screen reads are local operations (no API call), but the root agent processing the screen content IS an API call (on the root agent's side). Returning a cached "no change" response is cheaper for the root agent to process than a full screen dump.**

**Implementation:**
- Track a hash of each session's screen content
- If `get_screen()` is called and the hash hasn't changed since last read, return a short "no change" indicator instead of full screen content
- Always return full content if explicitly requested or if hash has changed

**Test cases:**
- Read screen, read again immediately — verify cached response (or "no change")
- Screen content changes between reads — verify fresh content returned
- Configurable cache duration
- Force-refresh option to bypass cache

### 015e: Backoff on agent errors

**Given** an executor's CLI agent shows an error (rate limited, crashed, etc.)
**When** the root agent would normally retry or send another message
**Then** the harness should enforce a backoff period

**Error indicators in terminal output:**
- "Rate limited" or "429" in agent's output
- "Error" or crash traceback
- Agent process exits unexpectedly
- No response for extended period (stuck)

**Test cases:**
- Detect "rate limit" text in session output, verify backoff
- Increasing backoff on repeated errors (2s, 4s, 8s, ...)
- Backoff is per-session (one executor's error doesn't slow others)
- Recovery: when agent responds normally, reset backoff timer
