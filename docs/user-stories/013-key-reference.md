# User Story 013: Complete Key Input Reference

**As the root agent driving interactive terminal applications, I need a well-defined vocabulary of keys I can send, covering every possible terminal interaction scenario — from AI agent prompts to text editors to menu-driven systems.**

## Context

During the research session, I discovered key-sending limitations empirically. Some keys worked (`\r`), some didn't (`^U`), and some required specific `raw` mode handling. For tttt to be a universal terminal driver, every key the root agent might ever need must be clearly defined and reliably deliverable.

## Key Categories

### 1. Printable Characters (alphanumeric + symbols)

These can be sent directly as their character values. No special encoding needed.

| Category | Characters | Notes |
|----------|-----------|-------|
| Lowercase | `a-z` | Direct |
| Uppercase | `A-Z` | Direct |
| Digits | `0-9` | Direct |
| Space | ` ` (0x20) | Direct |
| Punctuation | `!@#$%^&*()-_=+` | Direct |
| Brackets | `[]{}()<>` | Direct |
| Quotes | `'"` `` ` `` | May need escaping in JSON |
| Path chars | `/\.:,;` | Direct |
| Math/logic | `~\|?` | Direct, `\|` needs escaping sometimes |

**The root agent can generate these directly in the send_keys string. No special handling needed other than JSON string escaping.**

### 2. Control Characters (Ctrl+key)

These are the most commonly needed for terminal interaction. Each maps to a single byte.

| Key | Byte | Notation | Common Use | Used in Session? |
|-----|------|----------|-----------|-----------------|
| Ctrl+A | 0x01 | `^A` | Beginning of line (readline), tmux prefix | No |
| Ctrl+B | 0x02 | `^B` | Back one char (readline), tmux prefix | No |
| Ctrl+C | 0x03 | `^C` | Interrupt/SIGINT | Yes (tried, unreliable through tmux) |
| Ctrl+D | 0x04 | `^D` | EOF / logout | No |
| Ctrl+E | 0x05 | `^E` | End of line (readline) | No |
| Ctrl+F | 0x06 | `^F` | Forward one char (readline) | No |
| Ctrl+G | 0x07 | `^G` | Bell / cancel (some editors) | No |
| Ctrl+H | 0x08 | `^H` | Backspace (alternative) | No |
| Ctrl+I | 0x09 | `^I` / Tab | Tab completion / indent | No (but would need for editors) |
| Ctrl+J | 0x0A | `^J` / LF | Line feed (sometimes = Enter) | No |
| Ctrl+K | 0x0B | `^K` | Kill to end of line (readline) | No |
| Ctrl+L | 0x0C | `^L` | Clear screen / redraw | No |
| Ctrl+M | 0x0D | `^M` / CR | **Enter/Return** | **Yes — primary submit key** |
| Ctrl+N | 0x0E | `^N` | Next history (readline) / down | No |
| Ctrl+O | 0x0F | `^O` | Expand collapsed output (Claude Code) | No (but should have!) |
| Ctrl+P | 0x10 | `^P` | Previous history (readline) / up | No |
| Ctrl+Q | 0x11 | `^Q` | XON (resume output) | No |
| Ctrl+R | 0x12 | `^R` | Reverse search (readline) | No |
| Ctrl+S | 0x13 | `^S` | XOFF (pause output) | No |
| Ctrl+T | 0x14 | `^T` | Transpose chars (readline) | No |
| Ctrl+U | 0x15 | `^U` | Kill whole line (readline) | Yes (tried, didn't work in Claude Code) |
| Ctrl+V | 0x16 | `^V` | Literal next char | No |
| Ctrl+W | 0x17 | `^W` | Kill previous word (readline) | No |
| Ctrl+X | 0x18 | `^X` | Prefix for Emacs commands | No |
| Ctrl+Y | 0x19 | `^Y` | Yank (paste kill buffer) | No |
| Ctrl+Z | 0x1A | `^Z` | Suspend (SIGTSTP) | No |
| Ctrl+[ | 0x1B | `^[` / ESC | **Escape** | **Yes — close panels, cancel** |
| Ctrl+\ | 0x1C | `^\` | SIGQUIT / tttt pane switching prefix | No |
| Ctrl+] | 0x1D | `^]` | Telnet escape | Yes (tried for panel close) |
| Ctrl+^ | 0x1E | `^^` | Rarely used | No |
| Ctrl+_ | 0x1F | `^_` | Undo (some editors) | No |

### 3. Escape Sequences (multi-byte)

These start with ESC (0x1B) followed by additional bytes. Critical for cursor movement, function keys, and editor navigation.

| Key | Sequence | Notation | Common Use |
|-----|----------|----------|-----------|
| **Arrow Up** | `ESC [ A` | `\x1b[A` | Navigate up in menus, command history |
| **Arrow Down** | `ESC [ B` | `\x1b[B` | Navigate down in menus, scroll |
| **Arrow Right** | `ESC [ C` | `\x1b[C` | Cursor right |
| **Arrow Left** | `ESC [ D` | `\x1b[D` | Cursor left |
| **Home** | `ESC [ H` or `ESC [ 1 ~` | `\x1b[H` | Beginning of line |
| **End** | `ESC [ F` or `ESC [ 4 ~` | `\x1b[F` | End of line |
| **Insert** | `ESC [ 2 ~` | `\x1b[2~` | Toggle insert/overwrite |
| **Delete** | `ESC [ 3 ~` | `\x1b[3~` | Delete char under cursor |
| **Page Up** | `ESC [ 5 ~` | `\x1b[5~` | Scroll up one page |
| **Page Down** | `ESC [ 6 ~` | `\x1b[6~` | Scroll down one page |
| **F1** | `ESC O P` | `\x1bOP` | Help (in many apps) |
| **F2** | `ESC O Q` | `\x1bOQ` | Rename/edit |
| **F3** | `ESC O R` | `\x1bOR` | Search |
| **F4** | `ESC O S` | `\x1bOS` | Close/quit |
| **F5-F12** | `ESC [ 15~` through `ESC [ 24~` | varies | Application-specific |
| **Shift+Tab** | `ESC [ Z` | `\x1b[Z` | Reverse tab / "allow all" in Claude Code |
| **Alt+letter** | `ESC letter` | `\x1b` + char | Meta key combinations |

### 4. Special Sequences for Common Terminal Operations

| Operation | Sequence | When Needed |
|-----------|----------|-------------|
| **Enter/Submit** | `\r` (0x0D) | Submit input, approve prompts |
| **Escape/Cancel** | `\x1b` (0x1B) | Close panels, cancel operations |
| **Tab complete** | `\t` (0x09) | Filename/command completion |
| **Backspace** | `\x7f` (0x7F) or `\x08` | Delete character before cursor |
| **Interrupt** | `\x03` (Ctrl+C) | Kill running process |
| **EOF** | `\x04` (Ctrl+D) | Close input stream, logout |
| **Suspend** | `\x1a` (Ctrl+Z) | Background current process |
| **Clear screen** | `\x0c` (Ctrl+L) | Redraw terminal |
| **Kill line** | `\x15` (Ctrl+U) | Clear current input |

### 5. Bracketed Paste Mode

Many modern terminals (including Claude Code) use bracketed paste:

| Sequence | Meaning |
|----------|---------|
| `ESC [ 200 ~` (0x1B 0x5B 0x32 0x30 0x30 0x7E) | Start of pasted text |
| `ESC [ 201 ~` (0x1B 0x5B 0x32 0x30 0x31 0x7E) | End of pasted text |

**When bracketed paste mode is enabled by the application:**
- Text between these markers is treated as pasted (not typed character-by-character)
- This is WHY Claude Code shows `[Pasted text #1]` — it detects the paste brackets
- To simulate typing (not pasting), send characters one at a time WITHOUT brackets
- To simulate pasting, wrap text in bracket markers

**This is critical for tttt:** The tool should know whether the target application has enabled bracketed paste mode (detectable from the terminal output) and frame send_keys accordingly.

## Stories

### 013a: Named key constants

**Given** the root agent needs to send a special key
**When** specifying it in a tool call
**Then** there should be clear, unambiguous named constants

**Proposed key naming convention for the MCP tool:**

```json
{
  "keys": "hello world\r",
  "comment": "Type text then press Enter"
}

{
  "keys": "\x1b",
  "raw": true,
  "comment": "Press Escape"
}

{
  "keys": "\x1b[A",
  "raw": true,
  "comment": "Press Up Arrow"
}
```

**Alternative: named key tokens that the harness expands:**
```json
{
  "keys": "hello world[ENTER]"
}

{
  "keys": "[ESCAPE]"
}

{
  "keys": "[UP][UP][ENTER]"
}

{
  "keys": "[CTRL+C]"
}

{
  "keys": "[SHIFT+TAB]"
}

{
  "keys": "[F1]"
}
```

**The named-token approach is more readable and less error-prone than raw escape sequences.** The root agent doesn't need to memorize `\x1b[Z` for Shift+Tab — it just writes `[SHIFT+TAB]`.

**Test cases (using a test program that echoes received bytes):**
- Send `[ENTER]`, verify 0x0D received
- Send `[ESCAPE]`, verify 0x1B received
- Send `[UP]`, verify `\x1b[A` received
- Send `[CTRL+C]`, verify 0x03 received
- Send `[SHIFT+TAB]`, verify `\x1b[Z` received
- Send `[F5]`, verify correct escape sequence received
- Send `hello[ENTER]`, verify "hello\r" received
- Send `[CTRL+A]text[CTRL+E]`, verify correct byte sequence
- Send unknown token `[FOOBAR]`, verify clear error

### 013b: Bracketed paste awareness

**Given** an application has enabled bracketed paste mode
**When** the root agent sends a long message
**Then** the harness should wrap it in paste brackets for proper handling

**Test cases:**
- Detect bracketed paste mode from terminal output (application enables it via `ESC [ ? 2004 h`)
- Send text with paste brackets, verify application receives it as paste
- Send text without paste brackets, verify application receives it as typed
- Toggle paste mode awareness based on application state

### 013c: Key sequence builder for complex interactions

**Given** the root agent needs to perform a complex editor operation
**When** composing a multi-step key sequence
**Then** the sequence should be expressible clearly

**Examples of complex sequences:**

Navigate a menu:
```
[DOWN][DOWN][DOWN][ENTER]
```

Edit in vim:
```
[ESCAPE]dd:wq[ENTER]
```

Select option 2 in a numbered menu:
```
2[ENTER]
```

Ctrl+A then type (tmux-style):
```
[CTRL+A]c
```

Search and replace in less/nano:
```
[CTRL+W]searchterm[ENTER]
```

**Test cases:**
- Multi-key sequence delivered in correct order with no inter-key delay issues
- Vim-style sequences (Escape then command chars)
- Sequences with mixed printable and control characters
- Very long sequences (100+ keys) — should not buffer overflow

### 013d: Timing and inter-key delay

**Given** some applications need time between keystrokes
**When** sending rapid key sequences
**Then** the harness should support optional inter-key delay

**Why:** Some terminal applications (especially over SSH) need a small delay between keystrokes to process them correctly. Sending `[UP][UP][UP][ENTER]` too fast might be interpreted as a single escape sequence rather than four separate keys.

**Test cases:**
- Send keys with 0ms delay (default) — verify fast delivery
- Send keys with 50ms inter-key delay — verify spaced delivery
- Send keys with per-key delay specification
- Verify no key loss at any delay setting
