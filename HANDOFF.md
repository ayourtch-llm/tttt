# Context Handoff

## Repository

- Working directory: `/Users/ayourtch/rust/tttt`
- Branch: `main`
- The release binary is `target/release/tttt`.

## Completed Work

Two changes were implemented and committed:

1. `932ea62 fix: keep interactive PTY input responsive`
   - Prioritizes stdin over PTY output and rendering.
   - Queues direct keyboard input and drains it with nonblocking partial writes.
   - Unifies mouse and keyboard terminal focus behavior.
   - Avoids notification screen scans when there are no watchers.

2. `fa367b1 feat: add handoff context refresh tool`
   - Adds the MCP tool `tttt_clear_and_read_handoff_md`.
   - The tool requires a `filename` argument naming an existing, readable,
     nonempty Markdown file.
   - It schedules `/clear` in the first terminal after 15-20 seconds.
   - After `/clear` is successfully injected, it waits another 10-15 seconds
     and injects an instruction to read the handoff file and continue.
   - Both injected messages are followed by Enter through the existing delayed
     submission path.
   - Failed injection stages remain queued and are retried.

## Verification

- `cargo check --workspace` passed.
- `cargo test --workspace` passed.
- `cargo build --release` passed.
- The worktree was clean before this handoff file was created.

## Next Action

The user intends to restart the agent and ask it to invoke:

```text
tttt_clear_and_read_handoff_md(filename="HANDOFF.md")
```

Before invoking the tool, confirm that `HANDOFF.md` exists and contains this
handoff. The tool call returns immediately. Do not manually send `/clear` or
duplicate the restore instruction; wait for the scheduled workflow.

After the context refresh, read this file again and continue assisting the
user from this state.
