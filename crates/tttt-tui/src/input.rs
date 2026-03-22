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
}

/// State machine for parsing input with prefix-key awareness.
pub struct InputParser {
    config: DisplayConfig,
    prefix_armed: bool,
}

impl InputParser {
    pub fn new(config: DisplayConfig) -> Self {
        Self {
            config,
            prefix_armed: false,
        }
    }

    /// Process raw input bytes into events.
    ///
    /// Returns a list of events. In most cases this is a single PassThrough,
    /// but the prefix key produces different events.
    pub fn process(&mut self, input: &RawInput) -> Vec<InputEvent> {
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
    fn test_config_defaults() {
        let config = DisplayConfig::default();
        assert_eq!(config.sidebar_width, 30);
        assert_eq!(config.prefix_key, 0x1c);
        assert!(config.status_line);
    }
}
