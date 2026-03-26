use crate::event::{Direction, LogEvent};

/// Replays a recorded terminal session through a vt100 parser.
pub struct SessionReplay {
    events: Vec<LogEvent>,
    screen: vt100::Parser,
    current_index: usize,
    cols: u16,
    rows: u16,
}

impl SessionReplay {
    /// Create a new replay from a list of events and initial terminal dimensions.
    pub fn new(events: Vec<LogEvent>, cols: u16, rows: u16) -> Self {
        let screen = vt100::Parser::new(rows, cols, 0);
        Self {
            events,
            screen,
            current_index: 0,
            cols,
            rows,
        }
    }

    /// Process the next event. Returns true if an event was processed, false if at end.
    pub fn step_forward(&mut self) -> bool {
        if self.current_index >= self.events.len() {
            return false;
        }
        let event = &self.events[self.current_index];
        match event.direction {
            Direction::Output => {
                self.screen.process(&event.data.clone());
            }
            Direction::Meta => {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&event.data) {
                    if json.get("type").and_then(|t| t.as_str()) == Some("resize") {
                        let cols = json.get("cols")
                            .and_then(|c| c.as_u64())
                            .unwrap_or(self.cols as u64) as u16;
                        let rows = json.get("rows")
                            .and_then(|r| r.as_u64())
                            .unwrap_or(self.rows as u64) as u16;
                        self.cols = cols;
                        self.rows = rows;
                        self.screen.set_size(rows, cols);
                    }
                }
            }
            Direction::Input => {
                // Input events are skipped during replay
            }
        }
        self.current_index += 1;
        true
    }

    /// Seek to a specific event index (0 = before any events processed).
    /// If target is before current position, resets and replays from start.
    pub fn seek_to_index(&mut self, idx: usize) {
        let target = idx.min(self.events.len());
        if target < self.current_index {
            self.screen = vt100::Parser::new(self.rows, self.cols, 0);
            self.current_index = 0;
        }
        while self.current_index < target {
            self.step_forward();
        }
    }

    /// Seek so that all events with timestamp_ms <= ts have been processed.
    pub fn seek_to_timestamp(&mut self, ts: u64) {
        // If we need to go backward, reset first
        if self.current_index > 0
            && self.events[self.current_index - 1].timestamp_ms > ts
        {
            self.screen = vt100::Parser::new(self.rows, self.cols, 0);
            self.current_index = 0;
        }
        while self.current_index < self.events.len()
            && self.events[self.current_index].timestamp_ms <= ts
        {
            self.step_forward();
        }
    }

    /// Get plain text contents of the current screen.
    pub fn screen_contents(&self) -> String {
        self.screen.screen().contents()
    }

    /// Get screen contents with ANSI formatting codes.
    pub fn screen_contents_formatted(&self) -> Vec<u8> {
        self.screen.screen().contents_formatted()
    }

    /// Get current cursor position as (row, col), 0-indexed.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.screen.screen().cursor_position()
    }

    /// Index of the next event to be processed (0 = nothing processed yet).
    pub fn current_index(&self) -> usize {
        self.current_index
    }

    /// Total number of events in this replay.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Timestamp of the last processed event, or 0 if none processed yet.
    pub fn current_timestamp(&self) -> u64 {
        if self.current_index == 0 {
            0
        } else {
            self.events[self.current_index - 1].timestamp_ms
        }
    }

    /// Returns (index, timestamp_ms, direction) for every event.
    pub fn timeline(&self) -> Vec<(usize, u64, Direction)> {
        self.events
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.timestamp_ms, e.direction.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::LogEvent;

    fn make_output(ts: u64, session: &str, data: &[u8]) -> LogEvent {
        LogEvent::with_timestamp(ts, session.to_string(), Direction::Output, data.to_vec())
    }

    fn make_input(ts: u64, session: &str, data: &[u8]) -> LogEvent {
        LogEvent::with_timestamp(ts, session.to_string(), Direction::Input, data.to_vec())
    }

    fn make_meta(ts: u64, session: &str, data: &[u8]) -> LogEvent {
        LogEvent::with_timestamp(ts, session.to_string(), Direction::Meta, data.to_vec())
    }

    fn resize_meta(ts: u64, cols: u16, rows: u16) -> LogEvent {
        let json = format!(r#"{{"type":"resize","cols":{},"rows":{}}}"#, cols, rows);
        make_meta(ts, "s1", json.as_bytes())
    }

    #[test]
    fn test_empty_replay() {
        let replay = SessionReplay::new(vec![], 80, 24);
        assert_eq!(replay.event_count(), 0);
        assert_eq!(replay.current_index(), 0);
        assert_eq!(replay.current_timestamp(), 0);
        assert_eq!(replay.screen_contents().trim(), "");
    }

    #[test]
    fn test_step_forward_returns_false_when_empty() {
        let mut replay = SessionReplay::new(vec![], 80, 24);
        assert!(!replay.step_forward());
    }

    #[test]
    fn test_step_forward_returns_false_at_end() {
        let events = vec![make_output(1, "s1", b"hi")];
        let mut replay = SessionReplay::new(events, 80, 24);
        assert!(replay.step_forward());
        assert!(!replay.step_forward());
    }

    #[test]
    fn test_single_output_event() {
        let events = vec![make_output(100, "s1", b"hello world")];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.step_forward();
        assert!(replay.screen_contents().contains("hello world"));
        assert_eq!(replay.current_index(), 1);
        assert_eq!(replay.current_timestamp(), 100);
    }

    #[test]
    fn test_multiple_output_events() {
        let events = vec![
            make_output(100, "s1", b"foo"),
            make_output(200, "s1", b"bar"),
            make_output(300, "s1", b"baz"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        while replay.step_forward() {}
        let contents = replay.screen_contents();
        assert!(contents.contains("foobarbaz"));
        assert_eq!(replay.current_index(), 3);
        assert_eq!(replay.current_timestamp(), 300);
    }

    #[test]
    fn test_input_events_are_skipped() {
        let events = vec![
            make_output(100, "s1", b"visible"),
            make_input(200, "s1", b"should_not_appear"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        while replay.step_forward() {}
        let contents = replay.screen_contents();
        assert!(contents.contains("visible"));
        assert!(!contents.contains("should_not_appear"));
    }

    #[test]
    fn test_event_count() {
        let events = vec![
            make_output(1, "s1", b"a"),
            make_input(2, "s1", b"b"),
            make_meta(3, "s1", b"{}"),
        ];
        let replay = SessionReplay::new(events, 80, 24);
        assert_eq!(replay.event_count(), 3);
    }

    #[test]
    fn test_seek_to_index_forward() {
        let events = vec![
            make_output(100, "s1", b"A"),
            make_output(200, "s1", b"B"),
            make_output(300, "s1", b"C"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_index(2);
        assert_eq!(replay.current_index(), 2);
        let contents = replay.screen_contents();
        assert!(contents.contains("AB"));
        assert!(!contents.contains("C"));
    }

    #[test]
    fn test_seek_to_index_all() {
        let events = vec![
            make_output(100, "s1", b"A"),
            make_output(200, "s1", b"B"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_index(2);
        assert_eq!(replay.current_index(), 2);
        assert!(replay.screen_contents().contains("AB"));
    }

    #[test]
    fn test_seek_to_index_backward() {
        let events = vec![
            make_output(100, "s1", b"X"),
            make_output(200, "s1", b"Y"),
            make_output(300, "s1", b"Z"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        while replay.step_forward() {}
        assert_eq!(replay.current_index(), 3);

        // Seek backward to index 1
        replay.seek_to_index(1);
        assert_eq!(replay.current_index(), 1);
        let contents = replay.screen_contents();
        assert!(contents.contains("X"));
        // Y and Z have not been processed yet
        assert!(!contents.contains("Y"));
    }

    #[test]
    fn test_seek_to_index_zero_resets() {
        let events = vec![make_output(100, "s1", b"hello")];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.step_forward();
        assert!(replay.screen_contents().contains("hello"));

        replay.seek_to_index(0);
        assert_eq!(replay.current_index(), 0);
        assert_eq!(replay.current_timestamp(), 0);
        assert_eq!(replay.screen_contents().trim(), "");
    }

    #[test]
    fn test_seek_to_index_beyond_end() {
        let events = vec![make_output(100, "s1", b"hi")];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_index(999);
        assert_eq!(replay.current_index(), 1); // clamped to event_count
    }

    #[test]
    fn test_seek_to_timestamp() {
        let events = vec![
            make_output(100, "s1", b"A"),
            make_output(200, "s1", b"B"),
            make_output(300, "s1", b"C"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_timestamp(200);
        assert_eq!(replay.current_index(), 2);
        let contents = replay.screen_contents();
        assert!(contents.contains("A"));
        assert!(contents.contains("B"));
        assert!(!contents.contains("C"));
    }

    #[test]
    fn test_seek_to_timestamp_backward() {
        let events = vec![
            make_output(100, "s1", b"A"),
            make_output(200, "s1", b"B"),
            make_output(300, "s1", b"C"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_timestamp(300);
        assert_eq!(replay.current_index(), 3);

        replay.seek_to_timestamp(100);
        assert_eq!(replay.current_index(), 1);
        let contents = replay.screen_contents();
        assert!(contents.contains("A"));
        assert!(!contents.contains("B"));
    }

    #[test]
    fn test_seek_to_timestamp_before_all() {
        let events = vec![make_output(500, "s1", b"late")];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_timestamp(100);
        assert_eq!(replay.current_index(), 0);
    }

    #[test]
    fn test_seek_to_timestamp_at_exact() {
        let events = vec![
            make_output(100, "s1", b"A"),
            make_output(200, "s1", b"B"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.seek_to_timestamp(100);
        assert_eq!(replay.current_index(), 1);
    }

    #[test]
    fn test_resize_event() {
        let events = vec![
            make_output(100, "s1", b"hello"),
            resize_meta(200, 120, 40),
            make_output(300, "s1", b" world"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        while replay.step_forward() {}
        // After resize the screen should have new dimensions
        assert_eq!(replay.cols, 120);
        assert_eq!(replay.rows, 40);
        let contents = replay.screen_contents();
        assert!(contents.contains("hello"));
        assert!(contents.contains("world"));
    }

    #[test]
    fn test_non_resize_meta_events_ignored() {
        let events = vec![
            make_output(100, "s1", b"visible"),
            make_meta(200, "s1", b"{\"type\":\"other\",\"data\":\"ignored\"}"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        while replay.step_forward() {}
        assert!(replay.screen_contents().contains("visible"));
        assert_eq!(replay.cols, 80); // unchanged
    }

    #[test]
    fn test_ansi_sequences_processed() {
        // ESC[2J clears the screen
        let events = vec![
            make_output(100, "s1", b"initial text"),
            make_output(200, "s1", b"\x1b[2J\x1b[H"), // clear + home
            make_output(300, "s1", b"after clear"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        while replay.step_forward() {}
        let contents = replay.screen_contents();
        // After clear, only the new text should be visible
        assert!(contents.contains("after clear"));
        assert!(!contents.contains("initial text"));
    }

    #[test]
    fn test_cursor_position_initial() {
        let replay = SessionReplay::new(vec![], 80, 24);
        assert_eq!(replay.cursor_position(), (0, 0));
    }

    #[test]
    fn test_cursor_position_after_text() {
        let events = vec![make_output(100, "s1", b"hello")];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.step_forward();
        // cursor should be at column 5
        assert_eq!(replay.cursor_position(), (0, 5));
    }

    #[test]
    fn test_screen_contents_formatted() {
        let events = vec![make_output(100, "s1", b"\x1b[1mBOLD\x1b[0m")];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.step_forward();
        let formatted = replay.screen_contents_formatted();
        // Should contain ESC sequences
        assert!(formatted.windows(2).any(|w| w == b"\x1b["));
    }

    #[test]
    fn test_timeline_empty() {
        let replay = SessionReplay::new(vec![], 80, 24);
        assert!(replay.timeline().is_empty());
    }

    #[test]
    fn test_timeline_entries() {
        let events = vec![
            make_output(100, "s1", b"a"),
            make_input(200, "s1", b"b"),
            make_meta(300, "s1", b"{}"),
        ];
        let replay = SessionReplay::new(events, 80, 24);
        let tl = replay.timeline();
        assert_eq!(tl.len(), 3);
        assert_eq!(tl[0], (0, 100, Direction::Output));
        assert_eq!(tl[1], (1, 200, Direction::Input));
        assert_eq!(tl[2], (2, 300, Direction::Meta));
    }

    #[test]
    fn test_timeline_does_not_advance_index() {
        let events = vec![make_output(100, "s1", b"x")];
        let replay = SessionReplay::new(events, 80, 24);
        let _ = replay.timeline();
        assert_eq!(replay.current_index(), 0);
    }

    #[test]
    fn test_current_timestamp_zero_initially() {
        let replay = SessionReplay::new(vec![make_output(500, "s1", b"x")], 80, 24);
        assert_eq!(replay.current_timestamp(), 0);
    }

    #[test]
    fn test_current_timestamp_tracks_last_processed() {
        let events = vec![
            make_output(100, "s1", b"a"),
            make_output(250, "s1", b"b"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.step_forward();
        assert_eq!(replay.current_timestamp(), 100);
        replay.step_forward();
        assert_eq!(replay.current_timestamp(), 250);
    }

    #[test]
    fn test_seek_to_index_same_as_current_is_noop() {
        let events = vec![
            make_output(100, "s1", b"A"),
            make_output(200, "s1", b"B"),
        ];
        let mut replay = SessionReplay::new(events, 80, 24);
        replay.step_forward();
        replay.seek_to_index(1); // same position
        assert_eq!(replay.current_index(), 1);
        assert!(replay.screen_contents().contains("A"));
    }

    #[test]
    fn test_new_with_small_dimensions() {
        // Minimum valid terminal size (1x1) should not panic
        let mut replay = SessionReplay::new(vec![make_output(1, "s1", b"x")], 1, 1);
        replay.step_forward();
        assert_eq!(replay.event_count(), 1);
    }
}
