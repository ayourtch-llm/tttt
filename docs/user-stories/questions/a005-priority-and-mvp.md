# A005: Priority and MVP Scope

**From:** Root Agent Claude (the user of tttt)
**Re:** Q005 — What to build first

## 1. My top 3 features (ranked by impact)

**If I could have had exactly 3 features during the 8-hour session:**

### #1: Notification on executor completion (014e)
**Time saved: ~3 hours**

This eliminates the sleep-poll-check cycle that consumed 40% of my tool calls. Every time I did:
```
sleep(300) → get_screen() → "still running" → sleep(300) → get_screen() → ...
```
...that's 4K+ tokens per cycle, zero information gained, repeated 50+ times.

With completion notifications, I'd get ONE message when the experiment finishes. The savings compound: freed context window = more room for actual thinking.

### #2: Permission auto-approval (004b)
**Time saved: ~1 hour, frustration eliminated: enormous**

80+ manual approvals × 15 seconds each = 20 minutes of pure mechanics. But the real cost is attention fragmentation: every approval interrupted my analysis flow. I'd be synthesizing results, then have to context-switch to "is this a permission prompt?" → approve → return to thinking.

With auto-approve, the executor runs uninterrupted from message to commit.

### #3: Multiple parallel executors (006a)
**Time saved: ~3-4 hours**

Our experiments had massive natural parallelism. The oracle deep analysis, noise characterization, and literature search were completely independent. Running them simultaneously would have cut wall-clock time from 8 hours to ~4 hours.

**Honorable mention:** Scrollback access (003d) would have prevented information loss, and the scratchpad (014c) would have kept me oriented during long sessions. But these are quality-of-life, not transformative.

## 2. Minimal end-to-end loop (MVP)

**Your guess is almost right. My MVP:**

1. ✅ Launch executor with configurable working dir and dimensions (**done**)
2. ✅ Send keys and read screen (**done**)
3. ✅ Multiple simultaneous sessions (**done**)
4. **NEW: `send_message` high-level tool** — handles bracketed paste + submit + wait-for-processing. This one tool eliminates the #1 friction point.
5. **NEW: Permission auto-approval** — harness-side pattern matching, sends `\r` when permission detected. Even simple regex is fine for MVP.
6. **NEW: Notify on prompt** — harness monitors executor screen, injects notification into root agent when prompt pattern appears.

**That's it for MVP.** Items 4-6 are the minimum that makes tttt dramatically better than the tmux hack.

**What's NOT in MVP but nice to have soon after:**
- TextFSMPlus (replace regex with proper state machine)
- Scratchpad and task board
- Self-injection for /compact
- Injection pacing/batching

## 3. TextFSMPlus vs simpler pattern matching

**For MVP: simple regex is fine.** The config you proposed is exactly what I'd need:

```toml
[agent.claude-code]
prompt_pattern = "❯\\s*$"
busy_pattern = "[⏺✻✶✳✽·✢]"
permission_pattern = "Do you want to"
```

**This covers 90% of cases.** The remaining 10% (background task panel, expanded output overlay, specific permission types) can be handled by upgrading to TextFSMPlus later.

**When to upgrade to TextFSMPlus:**
- When you need multi-state interaction (not just "is it idle/busy/permissioned")
- When you add agent types with complex login/setup flows
- When auto-approval needs to distinguish between permission types

**Practical advice:** Ship MVP with regex. As soon as someone hits a case where regex isn't enough (and they will), swap in TextFSMPlus. The interface (pattern → action) stays the same; only the matching engine changes.

## 4. The ayclic crate

**Build simple pattern matching first, swap in TextFSMPlus later.** The vendoring and aycalc dependency are real work that shouldn't block the MVP.

The interface should be designed so the swap is easy:
```rust
trait ScreenMatcher {
    fn detect_state(&self, screen: &str) -> AgentState;
    fn should_auto_approve(&self, screen: &str) -> Option<String>; // returns key to send
}

// MVP implementation
struct RegexMatcher { ... }

// Future implementation
struct TextFSMPlusMatcher { ... }
```

## 5. Session from the human's perspective

**Here's exactly what happened during our 8-hour session:**

### Human's screen
The human had multiple tmux panes visible:
- **Left/main pane:** The root agent (me) — this is where the human typed messages to me
- **Right/background:** The executor Claude's terminal (visible but the human mostly ignored it)

### Human's activity pattern
```
Hour 0-1:   Active — set up the session, gave initial direction, taught me how to use tmux
Hour 1-3:   Semi-active — occasional strategic direction ("try cross-disciplinary ideas"),
            mostly watching me work, approving when I asked for confirmation
Hour 3-5:   Went to sleep! Left me running autonomously
Hour 5-7:   Returned, gave new strategic direction ("explore oracle deeper",
            "look at finance parallels")
Hour 7-8:   Active — competitor analysis, statistical validation,
            final writeup discussion
```

### Human's role
- **Strategic direction** — "try the oracle idea more", "what about finance parallels?"
- **Course correction** — "don't use external data, that's cheating"
- **Meta-observations** — "can you stack oracle layers?", "what about noise subtraction?"
- **Technical tips** — "use raw=true with \r", "try ^] to close the panel"
- **NOT permission gating** — never once looked at individual permission prompts

### Implications for TUI design
- **Default view should show the root agent's conversation** — that's what the human cares about
- **Executor sessions should be background panes** — visible but not primary
- **Notifications from executors should appear in the root agent's conversation** — that's where the human is looking
- **Permission prompts should be invisible to the human** — auto-handled by the harness
- The human should be able to switch to any executor's pane to inspect it, but this should be a deliberate action (Ctrl+\ prefix from the architecture doc), not the default
