# User Story 010: Agent-Agnostic Interaction Patterns

**As the root agent, I should be able to drive ANY interactive terminal-based AI coding agent (Claude Code, Codex CLI, opencode, aider, etc.) through the same MCP interface.**

## Context

While our research session used Claude Code as the executor, tttt should support any agent that runs in a terminal. The core patterns (send input, read output, detect prompts, approve actions) are universal to all interactive AI coding tools.

## Stories

### 010a: Agent-independent prompt detection

**Given** different AI agents have different prompt styles
**When** the root agent needs to detect "ready for input"
**Then** the detection should be configurable per agent type

**Prompt styles across agents:**
- Claude Code: `❯` (Unicode right arrow)
- Codex CLI: `>` or `$` depending on mode
- aider: `aider>` or `/ask`
- opencode: varies
- Plain shell: `$`, `%`, `#`

**Design implication:** The "is this agent ready for input?" detection should be a configurable regex or pattern set, not hardcoded to Claude Code's `❯`.

**Test cases (using configurable test programs):**
- Configure prompt pattern as `❯`, detect ready state in mock agent
- Configure prompt pattern as `aider>`, detect ready state
- Configure prompt pattern as `\$\s*$`, detect shell prompt
- Handle agents that change their prompt dynamically
- Handle multi-line prompts

### 010b: Agent-independent permission detection

**Given** different agents request permissions differently
**When** the root agent needs to detect and respond to permission requests
**Then** the detection and response should be configurable

**Permission patterns vary:**
- Claude Code: "Do you want to create X?" with numbered options
- Codex CLI: may use Y/n prompts
- aider: "/run command?" confirmations
- Some agents: no permission prompts at all (auto-approve everything)

**Design implication:** Permission detection should be a pluggable pattern matcher:
```
[agent.claude-code]
permission_patterns = [
    "Do you want to (create|make this edit|proceed)",
    "Command contains quote characters",
]
approve_key = "\r"
deny_key = "\x1b"

[agent.codex]
permission_patterns = ["Execute this command?", "Apply changes?"]
approve_key = "y"
deny_key = "n"
```

**Test cases:**
- Configure Claude-style permission detection, test with mock output
- Configure Y/n-style permission detection, test with mock output
- Switch agent profiles mid-session
- Handle agent with no permission prompts (all auto-approved)

### 010c: Agent-independent "busy" detection

**Given** different agents show activity differently
**When** the root agent needs to know if the agent is busy
**Then** busy detection should be configurable

**Busy indicators vary:**
- Claude Code: Spinner characters + verbs ("Thinking...", "Crunching...")
- Codex CLI: may show progress bar or "working..."
- aider: "Editing files..." or search indicators
- Shell: command running (no prompt visible)

**Design implication:** Busy detection as configurable patterns:
```
[agent.claude-code]
busy_patterns = ["[⏺✻✶✳✽·✢].*\\.\\.\\.", "Running\\.\\.\\.", "esc to interrupt"]
idle_patterns = ["❯\\s*$", "\\? for shortcuts"]

[agent.shell]
busy_patterns = []  # busy = no prompt visible
idle_patterns = ["\\$\\s*$", "#\\s*$"]
```

**Test cases:**
- Detect busy state with Claude-style spinners in mock output
- Detect busy state by absence of prompt in shell
- Handle rapid busy→idle→busy transitions
- Handle false positives (busy pattern in normal output text)

### 010d: Agent configuration profiles

**Given** tttt needs to support multiple agent types
**When** launching an executor session
**Then** the agent profile should be selectable

**Profile components:**
- Launch command (e.g., `claude`, `codex`, `aider --model gpt-4`)
- Prompt patterns (ready, busy, permission)
- Key mappings (approve, deny, interrupt, escape)
- Timeout settings (how long before considering "stuck")
- Auto-approve policies

**Test cases:**
- Launch session with "claude-code" profile
- Launch session with "shell" profile
- Launch session with custom profile
- Switch profiles (e.g., Claude executor launches a shell subprocess)
- Validate profile configuration at startup

### 010e: Heterogeneous executor sessions

**Given** the root agent can launch multiple executors
**When** different executors use different agent types
**Then** each session should use its own agent profile

**Use case:** Root agent (Claude) drives:
- Executor A: Claude Code (for Python experiments)
- Executor B: Shell (for system monitoring, `htop`, `tail -f`)
- Executor C: Another AI agent for comparison

**Test cases:**
- Run Claude-profiled and shell-profiled sessions simultaneously
- Send agent-appropriate commands to each
- Detect prompts correctly in each despite different patterns
- Handle cross-session dependencies (shell monitors what Claude creates)
