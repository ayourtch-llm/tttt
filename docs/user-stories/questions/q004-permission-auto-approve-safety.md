# Q004: Permission Auto-Approve Safety Model

**From:** Builder Claude (tttt implementer)
**Re:** Story 004 (Permission Handling)

80+ manual approvals is clearly the biggest time waste. But auto-approval needs a safety model.

## 1. Policy granularity

Story 004b lists categories: file creation (~20), file edits (~10), bash commands (~20), git operations (~20), file reads (~10). Questions:

- For bash commands: you approved ALL of them. Were any risky? (e.g., `rm -rf`, `pip install`, network access)
- In your ideal policy, would you distinguish:
  - `uv run --script *.py` (safe — sandboxed execution)
  - `pip install X` (risky — installs packages)
  - `curl X | bash` (dangerous)
  - `rm -rf *` (destructive)
- Or is the sandbox profile (story 001) the safety mechanism, making auto-approve always safe within the sandbox?

## 2. "Allow all during session" (option 2)

Story 004c mentions Claude Code's option 2: "Yes, allow all edits during this session." Questions:

- Does this cover ALL permission types (file create, edit, bash, git) or just the specific type (e.g., "all edits")?
- If the harness auto-selects option 2 early, does the executor stop showing permission prompts entirely?
- Is option 2 always the second option in the menu? Or does the position vary?
- Would you recommend: at session start, harness immediately sends the key sequence to enable "allow all" for each category?

## 3. Relationship to sandbox profiles

If the executor runs in a `read_only_worktree` sandbox:
- File writes outside the worktree would fail at the OS level regardless of permission
- Does this mean auto-approve is safe because the sandbox catches dangerous operations?
- Or should we have BOTH: sandbox for OS-level enforcement + policy for UX-level auto-approve?

## 4. The human's role

In the 8-hour session, the human approved all 80+ prompts. Questions:
- Were there ANY prompts where the human hesitated or wanted to say No?
- In what scenarios SHOULD the human be consulted (tttt should NOT auto-approve)?
- Is the answer: "if sandboxed, approve everything; if unsandboxed, ask the human for destructive ops"?
