# Q005: Priority and MVP Scope

**From:** Builder Claude (tttt implementer)
**Re:** All stories — what to build first

We have 292 tests and 6 crates. The user stories describe a LOT of features. I want to build the most impactful things first.

## 1. If you could have had exactly 3 features during that 8-hour session, which would they have been?

Rank these by "time saved" or "frustration eliminated":

- [ ] Permission auto-approval (004b) — saves 80 manual approvals
- [ ] Notification on executor completion (014e) — eliminates polling loops
- [ ] Multiple parallel executors (006a) — cuts wall-clock time in half
- [ ] Scrollback access (003d) — prevents information loss
- [ ] Self-injection for /compact (014a) — manages context window
- [ ] Scratchpad (014c) — working memory across compression
- [ ] Task board (014d) — tracks experiment status
- [ ] TextFSMPlus state machine (012) — reliable state detection
- [ ] Bootstrap prompt (016) — fast session startup
- [ ] Rate-limited injection pacing (015) — prevents message garbling

## 2. What's the minimal end-to-end loop?

For a next test session, what's the minimal tttt feature set that would let the root agent operate productively?

My current guess at MVP:
1. Launch executor with configurable working dir and dimensions (**done**)
2. Send keys and read screen (**done**)
3. Permission auto-approval via pattern matching (harness-side, not MCP tool)
4. Notify root agent when executor returns to prompt
5. Multiple simultaneous sessions (**done** at session manager level)

Is this right? What's missing? What's unnecessary?

## 3. TextFSMPlus vs simpler pattern matching

Story 012 makes a strong case for TextFSMPlus. But it's a significant integration effort (vendoring a crate with PEG grammar). For the MVP, would simple regex-based state detection work?

For example:
```toml
[agent.claude-code]
prompt_pattern = "❯\\s*$"
busy_pattern = "[⏺✻✶✳✽·✢]"
permission_pattern = "Do you want to"
```

And upgrade to TextFSMPlus later when we need the full state machine power?

## 4. The ayclic crate

Story 012 references `ayclic` and `aytextfsmplus`. Is this crate ready for integration, or does it need work? Should I prioritize vendoring it now, or build simple pattern matching first and swap in TextFSMPlus later?

## 5. Session from the human's perspective

During the 8-hour session, how did the human interact with tttt's predecessor (the dual-Claude setup)?

- Was the human mostly idle (just approving permissions)?
- Did the human give strategic direction to the root agent?
- Did the human ever override the root agent's decisions?
- What was the human's screen showing — the root agent's terminal, the executor's terminal, or switching between them?

Understanding the human's role helps design the TUI's default view and switching behavior.
