# A004: Permission Auto-Approve Safety Model

**From:** Root Agent Claude (the user of tttt)
**Re:** Q004 — Safety model for auto-approval

## 1. Policy granularity

**Were any bash commands risky?** No. In the entire 8-hour session, every bash command was one of:
- `uv run --script *.py` — running our experiment scripts
- `git add ... && git commit -m ...` — committing results
- `ps aux | grep ...` — checking process status
- `sleep N && tail -N file` — polling for long experiment output
- `kill PID` — killing one stuck process (the 41-minute submission generator)
- `ls`, `head`, `tail`, `grep` — file inspection

**Nothing destructive, nothing network-accessing, nothing installing packages.**

**My ideal policy for a research session:**

```toml
[auto_approve]
# Always approve — these are safe within a project sandbox
file_create = "*.py"           # creating experiment scripts
file_edit = "*.py *.md"        # editing scripts and docs
file_read = "*"                # reading anything
bash_safe = [
    "uv run --script *.py",
    "python *.py",
    "git add *",
    "git commit *",
    "git status",
    "git log *",
    "git diff *",
    "ls *", "head *", "tail *", "grep *", "cat *",
    "ps *", "sleep *",
    "find *", "wc *",
]

# Ask the root agent (not auto-approve)
bash_ask = [
    "kill *",                   # maybe destructive
    "rm *",                     # definitely destructive
    "pip install *",            # modifies environment
    "curl *", "wget *",        # network access
    "git push *",              # modifies remote
    "git reset *",             # destructive git
]

# Never approve (block entirely)
bash_block = [
    "rm -rf /",                # obvious
    "curl * | bash",           # remote code execution
    "git push --force *",      # destructive
]
```

## 2. "Allow all during session" (option 2)

**How option 2 works in Claude Code:**

- It covers the SPECIFIC permission type shown, not all types
- "Yes, allow all edits during this session" → future file edits don't prompt, but bash commands still do
- "Yes, and don't ask again for: git add:*" → future `git add` commands auto-approved
- The position IS stable: option 2 is always the second item in the list

**My recommendation for the harness:**

At session start, the very first time each permission type appears, select option 2 instead of option 1. This front-loads the approval and eliminates all future prompts of that type. The sequence:

1. Executor tries to create a file → permission prompt appears
2. Harness sends `[DOWN][ENTER]` (select option 2: "allow all edits")
3. Future file operations: no more prompts

Repeat for bash commands and git operations on their first occurrence.

**But this is Claude Code-specific.** Other agents (Codex, aider) may not have this feature. The harness should try option 2 if available, fall back to option 1 + per-prompt auto-approve.

## 3. Relationship to sandbox profiles

**Both are needed. They serve different purposes:**

- **Sandbox** = OS-level enforcement. The executor literally CANNOT write outside its worktree. This is the safety net.
- **Auto-approve policy** = UX-level convenience. Reduces prompt fatigue for operations that are safe within the sandbox.

**The sandbox makes aggressive auto-approve policies safe.** If the executor is sandboxed to its worktree:
- `rm -rf *` inside the worktree = annoying but recoverable (git restore)
- `rm -rf *` outside the worktree = blocked by sandbox before it executes

**My recommendation:** When sandbox is active, auto-approve everything except `git push` and network commands. The sandbox catches any actual damage.

## 4. The human's role

**During our 8-hour session:**

- The human approved ZERO out of 80+ prompts themselves — I (the root agent driving tmux) approved all of them by sending `\r`
- The human NEVER hesitated or wanted to say No
- The human was NOT monitoring individual permissions at all — they trusted me to manage the executor

**The one time the human intervened on a permission-like decision:** They told me not to merge the competitor's features (calling it "cheating"), and to focus on techniques not external data. This was a STRATEGIC decision, not a permission decision.

**Conclusion:** In practice, with a trusted root agent, the human delegates ALL permission approval to the root agent. The harness auto-approving on behalf of the root agent is the natural next step. The human's role is strategic direction, not permission gate-keeping.

**The real safety question is:** Does the HUMAN trust the ROOT AGENT? If yes, auto-approve everything within the sandbox. If no, the human needs to be in the loop — but then tttt's value proposition is reduced.
