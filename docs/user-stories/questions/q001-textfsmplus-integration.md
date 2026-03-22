# Q001: TextFSMPlus Integration Details

**From:** Builder Claude (tttt implementer)
**Re:** Story 012 — TextFSMPlus as the interaction engine
**Status:** ANSWERED

## Answers (from ayourtch, author of textfsmplus):

### ANSI stripping / feed source
- TextFSMPlus was originally designed for Cisco IOS (serial, no ANSI colors)
- For tttt: feed the FSM with **textual content after vt100 filtering**, NOT raw bytes
- The vt100 `Screen::contents()` gives clean text — use that
- We can always add a "raw" variant later if needed

### aycalc vendoring
- Use aycalc from **github.com/ayourtch/aycalc** as a git dependency
- It's simple and stable, doesn't change much

### FSM lifecycle
- FSM is **per-interaction**, NOT long-lived
- When FSM hits `Done`, it returns to the caller and the FSM instance is dropped
- The next interaction creates a new FSM instance
- Long-lived FSMs are a future experiment, not needed now

### Feed source (confirmed)
- Readable characters after vt100 filtering, not raw bytes
- Feed `screen.contents()` output to the FSM

## Implementation plan for tttt:

1. Add `aycalc` as git dependency from github.com/ayourtch/aycalc
2. Vendor `aytextfsmplus` from ../ayclic/aytextfsmplus into tttt workspace
3. Feed FSM with vt100 screen contents (plain text)
4. FSM instances are per-interaction (created, run to Done/Error, dropped)
5. Templates loaded from config directory, with built-in defaults for common agents
