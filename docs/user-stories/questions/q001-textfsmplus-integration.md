# Q001: TextFSMPlus Integration Details

**From:** Builder Claude (tttt implementer)
**Re:** Story 012 — TextFSMPlus as the interaction engine
**Updated:** After reading ayclic/aytextfsmplus source code

I've read the full aytextfsmplus crate. MIT licensed, excellent `feed()` API with `FeedResult { action, consumed }`, preset variables, Send actions with aycalc expression expansion. Most of my original questions are answered. Remaining:

## 1. ANSI stripping strategy

The FSM works on raw bytes via `String::from_utf8_lossy()` but does NOT strip ANSI escape codes. For matching agent prompts (e.g., Claude Code's `❯` which may have color codes around it), we have two options:

**(a)** Feed the FSM the vt100 screen contents (plain text, ANSI-free) instead of raw PTY bytes
- Pro: Clean text, simple regex patterns
- Con: Loses timing info (screen updates are debounced), can't match partial prompts before newline

**(b)** Feed raw PTY bytes and write ANSI-aware regex patterns
- Pro: Immediate matching, works with `feed()` byte-by-byte advantage
- Con: Patterns are ugly: `(?:\x1b\[[0-9;]*m)*❯` instead of just `❯`

**(c)** Add an ANSI stripping layer between PTY output and FSM feed
- Pro: Clean patterns + immediate matching
- Con: Need to implement/vendor an ANSI stripper

Which approach did you envision for tttt? My instinct is (c) — a simple ANSI stripper is ~50 lines and gives us clean patterns with real-time matching.

## 2. aycalc vendoring

aytextfsmplus depends on `aycalc` at `../../aycalc/`. For tttt, I'd need to vendor both crates. Questions:

- Should I vendor aycalc into tttt as well?
- Or should aytextfsmplus be adapted to work without aycalc (using a simpler expression evaluator) since tttt's Send actions are likely just `${Variable}` substitutions, not complex expressions?

## 3. Template hot-reload (story 012e)

You describe editing templates mid-session. For tttt:

- Should templates be loaded from a config directory (e.g., `~/.config/tttt/templates/`)?
- Should there be built-in templates embedded in the binary for common agents (claude-code, shell)?
- Should hot-reload be file-system watch based, or triggered by an MCP tool (`template_reload`)?

## 4. FSM lifecycle per session

When the FSM reaches `Done`, the interaction is complete. But a PTY session is long-lived (many interactions). How should the FSM lifecycle work?

My current thinking:
- Each PTY session has an FSM instance
- FSM starts in `Start` state when session is created
- FSM processes all PTY output continuously
- `Done` state means "agent returned to idle/ready" — FSM auto-restarts to `Start`
- `Error` state means "unexpected output" — log and restart to `Start`

Is this correct? Or should there be separate FSM instances per "interaction" (send message → wait for response)?
