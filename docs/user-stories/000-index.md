# tttt User Stories Index

**Source:** Real 8+ hour medical research session (2026-03-22) where a root Claude instance drove an executor Claude instance via tmux PTY to run 60+ survival prediction experiments.

**Key outcome:** The two-Claude collaboration beat the competition winner (0.7231) with a score of 0.7424. The interaction patterns, pain points, and lessons learned form the basis for these user stories.

## Stories

| # | Title | Priority | Pain Level |
|---|-------|----------|------------|
| [001](001-session-lifecycle.md) | Session Lifecycle | Critical | Medium |
| [002](002-message-submission.md) | Message Submission | Critical | **HIGH** |
| [003](003-screen-reading.md) | Screen Reading & Parsing | Critical | **HIGH** |
| [004](004-permission-handling.md) | Permission Prompt Handling | Critical | **HIGH** |
| [005](005-long-running-experiments.md) | Long-Running Experiments | High | **HIGH** |
| [006](006-multi-executor-parallel.md) | Multi-Executor Parallel | High | Medium |
| [007](007-ui-state-machine.md) | UI State Machine | High | **HIGH** |
| [008](008-error-recovery.md) | Error Recovery & Resilience | Medium | Medium |
| [009](009-logging-and-replay.md) | Logging & Replay | Medium | Low |
| [010](010-agent-agnostic-patterns.md) | Agent-Agnostic Patterns | High | N/A (design) |
| [011](011-heterogeneous-ai-army.md) | Heterogeneous AI Team | Vision | N/A (future) |
| [012](012-textfsmplus-interaction-engine.md) | TextFSMPlus Interaction Engine | **Critical** | **HIGH** |
| [013](013-key-reference.md) | Complete Key Input Reference | **Critical** | **HIGH** |
| [014](014-self-injection.md) | Self-Injected Commands & Notifications | High | **HIGH** |
| [015](015-rate-limiting.md) | Injection Pacing & Throttling | **Critical** | Medium |
| [016](016-bootstrap-prompt.md) | Root Agent Bootstrap Prompt | **Critical** | **HIGH** |
| [999](999-lessons-from-the-field.md) | Lessons From the Field | Reference | — |
| [011](011-heterogeneous-ai-army.md) | Heterogeneous AI Team | Vision | N/A (future) |

## Top 5 Pain Points (implement these first)

1. **Message submission for multi-line text** (002b) — The paste-mode handling caused the most confusion and wasted time. The harness should transparently handle pasted text submission.

2. **Permission prompt auto-approval** (004b) — 80+ manual approvals over 8 hours. A configurable auto-approve policy would save enormous time.

3. **Long experiment polling** (005c) — The sleep-tail-read cycle wasted 40% of tool calls. A `wait_for_prompt` or notification mechanism would eliminate this.

4. **UI state detection** (007a) — Getting stuck in the wrong UI state (background task panel, expanded output) was disorienting. Reliable state detection prevents this.

5. **Screen scrollback access** (003d) — Important results scrolling off screen was a recurring problem. Access to scrollback buffer would prevent information loss.

## Design Principles Learned

1. **Agent-agnostic by default** — Don't hardcode Claude Code patterns. Use configurable agent profiles (010).
2. **Notification over polling** — Push model (notify on completion) beats pull model (sleep + check).
3. **State machine, not string matching** — The executor has discrete states with defined transitions (007). Model this explicitly.
4. **Parallel by default** — Single-executor was the bottleneck. Design for N executors from day one (006).
5. **Log everything** — The interaction log is the session's memory. Without it, multi-hour sessions are unreproducible (009).

## Session Statistics That Informed These Stories

- Duration: 8+ hours
- Experiments run: 60+
- Permission prompts approved: 80+
- Sleep/poll cycles: ~100
- Times stuck in wrong UI state: ~5
- Times message submission failed: ~3
- Context window tokens spent on waiting: ~40%
