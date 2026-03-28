/// Mouse button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Configuration for the display and input handling.
#[derive(Debug, Clone)]
pub struct DisplayConfig {
    /// Width of the right sidebar in columns.
    pub sidebar_width: u16,
    /// The prefix key byte (default: 0x1c = Ctrl+\).
    pub prefix_key: u8,
    /// Whether to show the bottom status line.
    pub status_line: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            sidebar_width: 30,
            prefix_key: 0x1c, // Ctrl+backslash
            status_line: true,
        }
    }
}

/// Raw input bytes from the terminal.
#[derive(Debug, Clone)]
pub struct RawInput {
    pub bytes: Vec<u8>,
}

/// Parsed input event after prefix-key processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Regular input to forward to the active PTY.
    PassThrough(Vec<u8>),
    /// Switch to terminal N (0-indexed).
    SwitchTerminal(usize),
    /// Switch to next terminal.
    NextTerminal,
    /// Switch to previous terminal.
    PrevTerminal,
    /// Show help overlay.
    ShowHelp,
    /// Send the literal prefix key byte.
    PrefixEscape,
    /// Detach / quit.
    Detach,
    /// Create a new terminal session.
    CreateTerminal,
    /// Live reload: save state and execv the new binary.
    Reload,
    /// Mouse button pressed at (col, row) — 0-indexed.
    MousePress { button: MouseButton, col: u16, row: u16 },
    /// Mouse dragged to (col, row) while button held — 0-indexed.
    MouseDrag { button: MouseButton, col: u16, row: u16 },
    /// Mouse button released at (col, row) — 0-indexed.
    MouseRelease { col: u16, row: u16 },
    /// Scroll wheel up at (col, row) — 0-indexed.
    ScrollUp { col: u16, row: u16 },
    /// Scroll wheel down at (col, row) — 0-indexed.
    ScrollDown { col: u16, row: u16 },
}

/// Try to parse an SGR mouse sequence from a byte slice.
/// Returns Some((event, bytes_consumed)) on success, None if not a mouse sequence
/// or if the sequence is incomplete (need more bytes).
pub fn parse_sgr_mouse(bytes: &[u8]) -> Option<(InputEvent, usize)> {
    // Must start with \x1b[<
    if bytes.len() < 3 || bytes[0] != 0x1b || bytes[1] != b'[' || bytes[2] != b'<' {
        return None;
    }
    // Find the terminator M or m
    let end = bytes[3..].iter().position(|&b| b == b'M' || b == b'm')?;
    let params = &bytes[3..3 + end];
    let terminator = bytes[3 + end];
    let consumed = 3 + end + 1;

    // Parse Pb;Px;Py
    let params_str = std::str::from_utf8(params).ok()?;
    let mut parts = params_str.splitn(3, ';');
    let pb: u16 = parts.next()?.parse().ok()?;
    let px: u16 = parts.next()?.parse().ok()?;
    let py: u16 = parts.next()?.parse().ok()?;

    // Convert to 0-indexed
    let col = px.saturating_sub(1);
    let row = py.saturating_sub(1);

    // Decode button and event type
    let is_release = terminator == b'm';
    let is_motion = pb & 32 != 0;
    let button_bits = pb & 0b11;
    // Bit 6 (value 64) signals a scroll event
    let is_scroll = pb & 64 != 0;

    let event = if is_scroll {
        // button_bits: 0 = scroll up, 1 = scroll down
        if button_bits == 0 {
            InputEvent::ScrollUp { col, row }
        } else {
            InputEvent::ScrollDown { col, row }
        }
    } else if is_release {
        InputEvent::MouseRelease { col, row }
    } else if is_motion {
        let button = match button_bits {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::Left,
        };
        InputEvent::MouseDrag { button, col, row }
    } else {
        let button = match button_bits {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::Left,
        };
        InputEvent::MousePress { button, col, row }
    };

    Some((event, consumed))
}

/// State machine for parsing input with prefix-key awareness.
pub struct InputParser {
    config: DisplayConfig,
    prefix_armed: bool,
    /// Buffer for incomplete mouse sequences split across read() calls.
    mouse_buf: Vec<u8>,
}

impl InputParser {
    pub fn new(config: DisplayConfig) -> Self {
        Self {
            config,
            prefix_armed: false,
            mouse_buf: Vec::new(),
        }
    }

    /// Process raw input bytes into events.
    ///
    /// Returns a list of events. In most cases this is a single PassThrough,
    /// but the prefix key produces different events. Mouse sequences are
    /// detected before prefix-key processing and emitted as mouse events.
    pub fn process(&mut self, input: &RawInput) -> Vec<InputEvent> {
        let mut events = Vec::new();

        // Combine any previously buffered incomplete mouse sequence with new bytes.
        let all_bytes: Vec<u8> = if self.mouse_buf.is_empty() {
            input.bytes.clone()
        } else {
            let mut combined = std::mem::take(&mut self.mouse_buf);
            combined.extend_from_slice(&input.bytes);
            combined
        };

        // Pre-process: scan for SGR mouse sequences and split the byte stream into
        // segments that are either mouse events or raw bytes for the prefix-key pipeline.
        // We produce a list of "chunks": either Mouse(event) or Raw(Vec<u8>).
        enum Chunk {
            Mouse(InputEvent),
            Raw(Vec<u8>),
        }

        let mut chunks: Vec<Chunk> = Vec::new();
        let mut pos = 0;
        let mut raw_buf: Vec<u8> = Vec::new();

        while pos < all_bytes.len() {
            // Check for start of SGR mouse sequence: \x1b[<
            if all_bytes[pos] == 0x1b
                && pos + 2 < all_bytes.len()
                && all_bytes[pos + 1] == b'['
                && all_bytes[pos + 2] == b'<'
            {
                // Try to parse a complete mouse sequence
                match parse_sgr_mouse(&all_bytes[pos..]) {
                    Some((event, consumed)) => {
                        // Flush any accumulated raw bytes first
                        if !raw_buf.is_empty() {
                            chunks.push(Chunk::Raw(std::mem::take(&mut raw_buf)));
                        }
                        chunks.push(Chunk::Mouse(event));
                        pos += consumed;
                    }
                    None => {
                        // Could be an incomplete sequence: check if there's no terminator yet
                        // by looking for M or m after position pos+3.
                        let has_terminator = all_bytes[pos + 3..]
                            .iter()
                            .any(|&b| b == b'M' || b == b'm');
                        if !has_terminator {
                            // Incomplete — buffer the rest for next call
                            self.mouse_buf.extend_from_slice(&all_bytes[pos..]);
                            break;
                        }
                        // It has a terminator but failed to parse (malformed) — treat as raw
                        raw_buf.push(all_bytes[pos]);
                        pos += 1;
                    }
                }
            } else {
                raw_buf.push(all_bytes[pos]);
                pos += 1;
            }
        }

        // Flush remaining raw bytes
        if !raw_buf.is_empty() {
            chunks.push(Chunk::Raw(raw_buf));
        }

        // Now process each chunk through the prefix-key state machine (for Raw chunks)
        // or emit directly (for Mouse chunks).
        for chunk in chunks {
            match chunk {
                Chunk::Mouse(event) => {
                    events.push(event);
                }
                Chunk::Raw(bytes) => {
                    let raw_input = RawInput { bytes };
                    let sub_events = self.process_raw_bytes(&raw_input);
                    events.extend(sub_events);
                }
            }
        }

        events
    }

    /// Internal: run the prefix-key state machine on raw bytes (no mouse detection).
    fn process_raw_bytes(&mut self, input: &RawInput) -> Vec<InputEvent> {
        let mut events = Vec::new();
        let mut passthrough = Vec::new();

        for &byte in &input.bytes {
            if self.prefix_armed {
                self.prefix_armed = false;
                match byte {
                    b'0'..=b'9' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::SwitchTerminal((byte - b'0') as usize));
                    }
                    b'n' | b'N' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::NextTerminal);
                    }
                    b'p' | b'P' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::PrevTerminal);
                    }
                    b'?' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::ShowHelp);
                    }
                    b'd' | b'D' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::Detach);
                    }
                    b'c' | b'C' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::CreateTerminal);
                    }
                    b'r' | b'R' => {
                        if !passthrough.is_empty() {
                            events.push(InputEvent::PassThrough(std::mem::take(&mut passthrough)));
                        }
                        events.push(InputEvent::Reload);
                    }
                    b if b == self.config.prefix_key => {
                        // Double prefix = send literal prefix key
                        passthrough.push(self.config.prefix_key);
                    }
                    _ => {
                        // Unknown sequence after prefix: discard
                    }
                }
            } else if byte == self.config.prefix_key {
                self.prefix_armed = true;
            } else {
                passthrough.push(byte);
            }
        }

        if !passthrough.is_empty() {
            events.push(InputEvent::PassThrough(passthrough));
        }

        events
    }

    /// Check if the prefix key is currently armed (waiting for follow-up).
    pub fn is_prefix_armed(&self) -> bool {
        self.prefix_armed
    }

    /// Cancel the armed prefix (e.g., on timeout).
    pub fn cancel_prefix(&mut self) {
        self.prefix_armed = false;
    }

    /// Get the current config.
    pub fn config(&self) -> &DisplayConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> InputParser {
        InputParser::new(DisplayConfig::default())
    }

    fn input(bytes: &[u8]) -> RawInput {
        RawInput {
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn test_regular_input_passthrough() {
        let mut p = parser();
        let events = p.process(&input(b"hello"));
        assert_eq!(events, vec![InputEvent::PassThrough(b"hello".to_vec())]);
    }

    #[test]
    fn test_prefix_key_arms() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c]));
        assert!(events.is_empty()); // prefix consumed, waiting for follow-up
        assert!(p.is_prefix_armed());
    }

    #[test]
    fn test_prefix_then_digit_switches() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'1']));
        assert_eq!(events, vec![InputEvent::SwitchTerminal(1)]);
        assert!(!p.is_prefix_armed());
    }

    #[test]
    fn test_prefix_then_zero() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'0']));
        assert_eq!(events, vec![InputEvent::SwitchTerminal(0)]);
    }

    #[test]
    fn test_prefix_then_n_next() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'n']));
        assert_eq!(events, vec![InputEvent::NextTerminal]);
    }

    #[test]
    fn test_prefix_then_p_prev() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'p']));
        assert_eq!(events, vec![InputEvent::PrevTerminal]);
    }

    #[test]
    fn test_prefix_then_question_help() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'?']));
        assert_eq!(events, vec![InputEvent::ShowHelp]);
    }

    #[test]
    fn test_prefix_then_d_detach() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'd']));
        assert_eq!(events, vec![InputEvent::Detach]);
    }

    #[test]
    fn test_prefix_then_prefix_escape() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, 0x1c]));
        assert_eq!(events, vec![InputEvent::PassThrough(vec![0x1c])]);
    }

    #[test]
    fn test_prefix_then_unknown_discards() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'x']));
        assert!(events.is_empty());
    }

    #[test]
    fn test_prefix_across_two_calls() {
        let mut p = parser();
        let events1 = p.process(&input(&[0x1c]));
        assert!(events1.is_empty());
        assert!(p.is_prefix_armed());
        let events2 = p.process(&input(&[b'1']));
        assert_eq!(events2, vec![InputEvent::SwitchTerminal(1)]);
    }

    #[test]
    fn test_mixed_input_with_prefix() {
        let mut p = parser();
        let events = p.process(&input(&[b'a', b'b', 0x1c, b'1', b'c']));
        assert_eq!(
            events,
            vec![
                InputEvent::PassThrough(b"ab".to_vec()),
                InputEvent::SwitchTerminal(1),
                InputEvent::PassThrough(b"c".to_vec()),
            ]
        );
    }

    #[test]
    fn test_cancel_prefix() {
        let mut p = parser();
        p.process(&input(&[0x1c]));
        assert!(p.is_prefix_armed());
        p.cancel_prefix();
        assert!(!p.is_prefix_armed());
    }

    #[test]
    fn test_custom_prefix_key() {
        let config = DisplayConfig {
            prefix_key: 0x01, // Ctrl+A
            ..Default::default()
        };
        let mut p = InputParser::new(config);
        let events = p.process(&input(&[0x01, b'n']));
        assert_eq!(events, vec![InputEvent::NextTerminal]);
    }

    #[test]
    fn test_empty_input() {
        let mut p = parser();
        let events = p.process(&input(&[]));
        assert!(events.is_empty());
    }

    #[test]
    fn test_prefix_then_c_create_terminal() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'c']));
        assert_eq!(events, vec![InputEvent::CreateTerminal]);
    }

    #[test]
    fn test_prefix_then_C_create_terminal_uppercase() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'C']));
        assert_eq!(events, vec![InputEvent::CreateTerminal]);
    }

    #[test]
    fn test_prefix_then_r_reload() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'r']));
        assert_eq!(events, vec![InputEvent::Reload]);
    }

    #[test]
    fn test_prefix_then_R_reload_uppercase() {
        let mut p = parser();
        let events = p.process(&input(&[0x1c, b'R']));
        assert_eq!(events, vec![InputEvent::Reload]);
    }

    #[test]
    fn test_config_defaults() {
        let config = DisplayConfig::default();
        assert_eq!(config.sidebar_width, 30);
        assert_eq!(config.prefix_key, 0x1c);
        assert!(config.status_line);
    }

    // --- SGR mouse parsing tests ---

    #[test]
    fn test_parse_sgr_mouse_left_press() {
        let bytes = b"\x1b[<0;10;5M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            event,
            InputEvent::MousePress {
                button: MouseButton::Left,
                col: 9,
                row: 4
            }
        );
    }

    #[test]
    fn test_parse_sgr_mouse_left_release() {
        let bytes = b"\x1b[<0;10;5m";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(event, InputEvent::MouseRelease { col: 9, row: 4 });
    }

    #[test]
    fn test_parse_sgr_mouse_left_drag() {
        let bytes = b"\x1b[<32;15;5M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            event,
            InputEvent::MouseDrag {
                button: MouseButton::Left,
                col: 14,
                row: 4
            }
        );
    }

    #[test]
    fn test_parse_sgr_mouse_scroll_up() {
        let bytes = b"\x1b[<64;10;5M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(event, InputEvent::ScrollUp { col: 9, row: 4 });
    }

    #[test]
    fn test_parse_sgr_mouse_scroll_down() {
        let bytes = b"\x1b[<65;10;5M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(event, InputEvent::ScrollDown { col: 9, row: 4 });
    }

    #[test]
    fn test_parse_sgr_mouse_right_press() {
        let bytes = b"\x1b[<2;5;3M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            event,
            InputEvent::MousePress {
                button: MouseButton::Right,
                col: 4,
                row: 2
            }
        );
    }

    #[test]
    fn test_parse_sgr_mouse_middle_press() {
        let bytes = b"\x1b[<1;3;7M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            event,
            InputEvent::MousePress {
                button: MouseButton::Middle,
                col: 2,
                row: 6
            }
        );
    }

    #[test]
    fn test_parse_sgr_mouse_not_mouse_sequence() {
        let bytes = b"hello";
        assert!(parse_sgr_mouse(bytes).is_none());
    }

    #[test]
    fn test_parse_sgr_mouse_incomplete() {
        let bytes = b"\x1b[<0;10;";
        assert!(parse_sgr_mouse(bytes).is_none());
    }

    #[test]
    fn test_parse_sgr_mouse_large_coordinates() {
        let bytes = b"\x1b[<0;200;100M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            event,
            InputEvent::MousePress {
                button: MouseButton::Left,
                col: 199,
                row: 99
            }
        );
    }

    #[test]
    fn test_parse_sgr_mouse_coordinate_zero_clamped() {
        // px=0 and py=0 are invalid in SGR (1-indexed), saturating_sub clamps to 0
        let bytes = b"\x1b[<0;0;0M";
        let (event, consumed) = parse_sgr_mouse(bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            event,
            InputEvent::MousePress {
                button: MouseButton::Left,
                col: 0,
                row: 0
            }
        );
    }

    // --- InputParser integration tests for mouse events ---

    #[test]
    fn test_process_mouse_press() {
        let mut p = parser();
        let events = p.process(&input(b"\x1b[<0;10;5M"));
        assert_eq!(
            events,
            vec![InputEvent::MousePress {
                button: MouseButton::Left,
                col: 9,
                row: 4
            }]
        );
    }

    #[test]
    fn test_process_mouse_release() {
        let mut p = parser();
        let events = p.process(&input(b"\x1b[<0;10;5m"));
        assert_eq!(events, vec![InputEvent::MouseRelease { col: 9, row: 4 }]);
    }

    #[test]
    fn test_process_scroll_up() {
        let mut p = parser();
        let events = p.process(&input(b"\x1b[<64;5;3M"));
        assert_eq!(events, vec![InputEvent::ScrollUp { col: 4, row: 2 }]);
    }

    #[test]
    fn test_process_scroll_down() {
        let mut p = parser();
        let events = p.process(&input(b"\x1b[<65;5;3M"));
        assert_eq!(events, vec![InputEvent::ScrollDown { col: 4, row: 2 }]);
    }

    #[test]
    fn test_process_mixed_text_and_mouse() {
        // "hi" + mouse press + "bye" → PassThrough("hi") + MousePress + PassThrough("bye")
        let mut p = parser();
        let mut bytes = b"hi".to_vec();
        bytes.extend_from_slice(b"\x1b[<0;10;5M");
        bytes.extend_from_slice(b"bye");
        let events = p.process(&input(&bytes));
        assert_eq!(
            events,
            vec![
                InputEvent::PassThrough(b"hi".to_vec()),
                InputEvent::MousePress {
                    button: MouseButton::Left,
                    col: 9,
                    row: 4
                },
                InputEvent::PassThrough(b"bye".to_vec()),
            ]
        );
    }

    #[test]
    fn test_process_incomplete_mouse_buffered_then_completed() {
        let mut p = parser();
        // First call: send only the start of the sequence
        let partial = b"\x1b[<0;10;";
        let events1 = p.process(&input(partial));
        // No events yet — incomplete sequence buffered
        assert!(events1.is_empty());

        // Second call: send the remainder
        let events2 = p.process(&input(b"5M"));
        assert_eq!(
            events2,
            vec![InputEvent::MousePress {
                button: MouseButton::Left,
                col: 9,
                row: 4
            }]
        );
    }

    #[test]
    fn test_process_two_consecutive_mouse_events() {
        let mut p = parser();
        let mut bytes = b"\x1b[<0;1;1M".to_vec();
        bytes.extend_from_slice(b"\x1b[<32;2;1M");
        let events = p.process(&input(&bytes));
        assert_eq!(
            events,
            vec![
                InputEvent::MousePress {
                    button: MouseButton::Left,
                    col: 0,
                    row: 0
                },
                InputEvent::MouseDrag {
                    button: MouseButton::Left,
                    col: 1,
                    row: 0
                },
            ]
        );
    }
}
