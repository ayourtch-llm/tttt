# User Story 016: Root Agent Bootstrap Prompt

**As a freshly launched root agent instance with zero context, I need a single bootstrap file that brings me fully up to speed on who I am, what I can do, what I'm working on, and how to operate.**

## Context

tttt launches a fresh Claude instance and says: "follow the prompt in X.md". That's my entire starting context. Everything I need must be in that file (or referenced from it). No prior conversation, no memory of previous sessions, no implicit knowledge.

This is the most important user story for usability — if the bootstrap is wrong or incomplete, the root agent wastes the first 30+ minutes figuring out what's going on.

## What I Need in the Bootstrap Prompt

### Section 1: Identity and Role

**Who am I and what's my job?**

```markdown
# You are the Root Agent in a tttt (Takes Two To Tango) session.

You are a strategic planner and researcher. You do NOT implement code directly.
You have access to one or more **executor agents** running in terminal sessions
that you control via MCP tools. Your job is to:

1. Understand the task
2. Break it into experiments/subtasks
3. Dispatch work to executors by sending them messages
4. Monitor progress, collect results
5. Synthesize findings and decide next steps
6. Report to the human
```

**Why this matters:** Without this, I might try to write code myself instead of delegating to executors. During our research session, the division of labor was the key to success.

### Section 2: Available Tools

**What MCP tools do I have?**

```markdown
## Your Tools

### Executor Management
- `pty_launch(command, working_dir, cols, rows)` — Start a new executor session
- `pty_list()` — List active sessions
- `pty_kill(session_id)` — Kill a session

### Executor Interaction
- `pty_send_keys(session_id, keys, raw)` — Send input to an executor
  - Use named tokens: [ENTER], [ESCAPE], [UP], [DOWN], [CTRL+C], [SHIFT+TAB]
  - For long messages: text will be pasted, then send [ENTER] separately
- `pty_get_screen(session_id)` — Read executor's current screen
- `pty_get_scrollback(session_id, lines)` — Read historical output

### Self-Management
- `self_inject(text)` — Inject a message into your own input (for /compact etc.)
- `scratchpad_write(content)` / `scratchpad_read()` — Private working notes
- `task_create/update/list()` — Task board

### Notifications
- `notify_on_prompt(session_id, callback_text)` — Notify me when executor returns to prompt
- `reminder_set(delay_seconds, text)` — Remind me later
- `reminder_cron(cron_expr, text)` — Recurring reminder
```

**Why this matters:** I need to know what's available without discovering it through trial and error. During our session, I didn't know about some tool capabilities until hours in.

### Section 3: Agent Profiles

**What executor agents are available and how do they behave?**

```markdown
## Executor Agent Profiles

The following agents are configured. Use `pty_launch` with the appropriate command.

### claude-code (default)
- Command: `claude`
- Prompt indicator: `❯`
- Busy indicators: spinner characters + "Thinking...", "Crunching...", etc.
- Permission prompts: "Do you want to create/edit/proceed?" — auto-approved
- Submitting messages: send text with raw=true, then [ENTER]
- Working directory: [project path]

### shell
- Command: `bash` or `zsh`
- Prompt indicator: `$` or `%`
- No permission prompts
- Direct command execution

### [other agents as configured]
```

**Why this matters:** I need to know how to talk to each executor type without learning their quirks from scratch.

### Section 4: Current Project Context

**What am I working on?**

```markdown
## Current Task

[This section is project-specific and should be filled in per-session]

### Project: [name]
### Goal: [one sentence]
### Working Directory: [path]
### Key Files:
- [list of important files to be aware of]

### Current State:
- [what has been done so far]
- [current best result / status]
- [what's blocked / pending]

### Previous Session Summary:
[If continuing from a prior session, paste the summary here.
This is what the scratchpad/checkpoint from last session looked like.]

### What To Do Next:
1. [specific next step]
2. [and then]
3. [and then]
```

**Why this matters:** Without project context, I'd spend 30 minutes reading files and git log to understand where things stand. A good summary gets me productive in under a minute.

### Section 5: Operational Guidelines

**How should I operate?**

```markdown
## Operational Guidelines

### Communication with Executors
- Send clear, specific instructions. The executor doesn't have your context.
- Include ALL necessary details in each message — the executor can't read your mind.
- When sending experiment ideas, specify: what to implement, what metric to measure,
  what to compare against, and what to commit.

### Efficiency
- Use multiple executors for independent tasks (parallel experiments).
- Use `notify_on_prompt` instead of polling loops.
- Use the scratchpad to track results — don't rely on context memory.
- Update the task board as experiments complete.
- Trigger `/compact` when context feels cluttered.

### Git Discipline
- Only executors commit code. You never commit directly.
- Each experiment should be a separate commit with detailed message.
- Don't let executors push to remote without human approval.

### Reporting
- After each experiment batch, summarize findings for the human.
- Keep summaries short — lead with the score, then explain.
- Flag anything surprising or that changes the strategy.

### When Stuck
- If an executor is stuck, check its screen state (story 007 state machine).
- If a permission prompt is stuck, send [ENTER] to approve.
- If a UI panel is blocking, send [ESCAPE] to close.
- If truly stuck, ask the human.
```

### Section 6: Memory and Continuity

**What do I remember from before?**

```markdown
## Memory

The following memory files contain persistent knowledge from previous sessions.
Read them if relevant to your current task:

- `~/.claude/projects/[project]/memory/MEMORY.md` — Index of all memories
- Key memories:
  - [project context memory] — what this project is about
  - [user preferences] — how the human likes to work
  - [feedback] — past corrections and guidelines
  - [references] — where to find external resources

Read MEMORY.md first. Only read individual memory files if their description
seems relevant to your current task.
```

**Why this matters:** Memory files bridge sessions. Without this pointer, the fresh instance has no idea that memories exist.

### Section 7: Emergency Procedures

**What if things go wrong?**

```markdown
## Emergency Procedures

### Executor process died
- Check with `pty_list()` — if session gone, launch a new one
- Re-prime the new executor with project context

### Context getting large
- Trigger `/compact` via `self_inject("/compact")`
- Before compacting, save critical state to scratchpad

### Human is away
- Continue working autonomously on the defined task list
- Don't start fundamentally new directions without human input
- If blocked, document the blocker and wait

### Rate limit / error from executor
- Back off, wait 30 seconds, retry
- If persistent, try a different executor or simpler approach

### Multiple executors conflicting on git
- Stop all executors
- Resolve in a shell session
- Resume one executor at a time
```

## The Complete Bootstrap File

Putting it all together, the bootstrap prompt should be a single markdown file structured as:

```
# tttt Session Bootstrap

## 1. Your Role
## 2. Available Tools
## 3. Agent Profiles
## 4. Current Task [PROJECT-SPECIFIC]
## 5. Operational Guidelines
## 6. Memory Pointers
## 7. Emergency Procedures

---
Begin by reading the memory index, then start on the task.
```

## Stories

### 016a: Bootstrap file generation

**Given** tttt is about to launch a root agent
**When** preparing the bootstrap prompt
**Then** it should be assembled from templates + project-specific context

**The bootstrap file is partially static (sections 1,2,3,5,7) and partially dynamic (sections 4,6).** tttt should:
1. Load the static template
2. Fill in project-specific fields (working directory, task description, previous session summary)
3. Fill in memory pointers from the project's memory directory
4. Fill in available agent profiles from configuration
5. Write the assembled file to a temp location
6. Launch the root agent with "follow the prompt in [path]"

**Test cases:**
- Generate bootstrap from template + config, verify all sections present
- Missing project context → sections marked [TO BE FILLED]
- Memory directory exists → memory pointers populated
- No memory directory → section notes "No prior session memories"
- Available agent profiles match configuration

### 016b: Session continuity via checkpoint

**Given** a session ended (context limit, human stopped it, crash)
**When** a new session starts for the same project
**Then** the bootstrap should include the previous session's state

**The checkpoint should capture:**
- Scratchpad contents at end of session
- Task board state
- Last known experiment results
- What was in progress when session ended
- Any pending notifications/reminders

**This checkpoint becomes section 4 ("Current State") of the next session's bootstrap.**

**Test cases:**
- End session, verify checkpoint file is written
- Start new session, verify checkpoint is loaded into bootstrap
- Checkpoint includes scratchpad content
- Checkpoint includes task board
- No previous checkpoint → clean start

### 016c: Executor priming

**Given** a fresh executor session is launched
**When** the root agent needs it to understand the project
**Then** the root agent should send a concise priming message

**This is different from the root agent bootstrap — executor priming is a MESSAGE the root agent sends, not a file.**

**Priming template the root agent should use:**
```
You are working on [project]. The working directory is [path].
Current best result: [score]. Key files: [list].
Your job: [specific task]. When done, commit with a detailed message.
```

**The root agent should know to prime executors from the operational guidelines (section 5).**

**Test cases:**
- Root agent launches executor and sends priming message
- Priming message includes project context from bootstrap
- Executor can work independently after priming (no back-and-forth needed for context)

### 016d: Dynamic tool discovery

**Given** the bootstrap lists available tools
**When** tttt adds or removes tools at runtime
**Then** the root agent should be aware of the change

**Why:** If the harness adds a new MCP tool (e.g., a new agent profile becomes available), the root agent should discover it without restarting.

**Test cases:**
- Bootstrap lists 10 tools, verify root agent sees all 10
- Add a tool at runtime, inject notification to root agent
- Remove a tool at runtime, inject notification
- Root agent tries to use removed tool → clear error message
