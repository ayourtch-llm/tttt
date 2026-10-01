//! Delayed submission of injected terminal input.

use std::time::{Duration, Instant};
use tttt_log::{Direction, LogEvent, LogSink};
use tttt_pty::{
    process_special_keys, PtyBackend, PtySession, ScreenBuffer, SessionManager, SessionStatus,
};

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

pub(crate) struct PendingEnter {
    session_id: String,
    text: String,
    fire_at: Instant,
    input_at: Instant,
    quiet_since: Option<Instant>,
    checks: usize,
    retries: usize,
}

impl PendingEnter {
    /// Call immediately after successfully sending the injection, under the
    /// session lock, so later keyboard input or another injection invalidates it.
    pub(crate) fn new<B: PtyBackend>(session: &PtySession<B>, text: &str, now: Instant) -> Self {
        Self {
            session_id: session.id.clone(),
            text: text.strip_prefix("[ENTER]").unwrap_or(text).to_owned(),
            fire_at: now + Duration::from_millis(100),
            input_at: session.last_input_time(),
            quiet_since: None,
            checks: 0,
            retries: 0,
        }
    }

    fn log(&self, logger: &mut impl LogSink, message: String) {
        let _ = logger.log_event(&LogEvent::new(
            self.session_id.clone(),
            Direction::Meta,
            message.into_bytes(),
        ));
    }

    fn give_up(&self, logger: &mut impl LogSink, reason: &str) {
        self.log(
            logger,
            format!("[ENTER-GAVE-UP] {reason} ({} retries)", self.retries),
        );
    }
}

/// Column after a known prompt, including an empty prompt whose trailing space
/// was trimmed by VT100. Empty prompts must still separate input from history.
fn prompt_input_column(line: &str) -> Option<u16> {
    let trimmed = line.trim_start_matches(' ');
    let mut chars = trimmed.chars();
    if !matches!(chars.next()?, '❯' | '›' | '>' | '$' | '#')
        || !matches!(chars.next(), None | Some(' '))
    {
        return None;
    }
    Some(((line.len() - trimmed.len()) as u16).saturating_add(2))
}

fn is_input_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 4 && line.chars().all(|c| c == '─' || c == '━')
}

fn normalized_input(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Require the latest identifiable prompt connected to the cursor, or the last
/// framed composer (which can hide the cursor or leave it on a footer). Unknown
/// layouts and input whose identifying prefix has scrolled away fail closed.
fn input_matches<B: PtyBackend>(session: &PtySession<B>, text: &str) -> bool {
    let mut screen = session.screen().screen().clone();
    screen.set_scrollback(0);
    if session.synchronized_output() {
        return false;
    }
    let (row, col) = screen.cursor_position();
    let (rows, cols) = screen.size();
    let lines: Vec<_> = screen.rows(0, cols).collect();
    let Some(start) = lines
        .iter()
        .rposition(|line| prompt_input_column(line).is_some())
    else {
        return false;
    };
    let input_col = prompt_input_column(&lines[start]).unwrap();
    if input_col >= cols || start > usize::from(row) {
        return false;
    }

    let bottom = lines.iter().rposition(|line| is_input_rule(line));
    let framed_end = bottom.and_then(|bottom| {
        let top = lines[..bottom]
            .iter()
            .rposition(|line| is_input_rule(line))?;
        (top < start
            && start < bottom
            && lines[top + 1..start]
                .iter()
                .all(|line| line.trim().is_empty()))
        .then_some(bottom - 1)
    });
    let end = if let Some(end) = framed_end {
        end as u16
    } else {
        if screen.hide_cursor() || (start == usize::from(row) && col < input_col) {
            return false;
        }
        for (continuation, line) in lines
            .iter()
            .enumerate()
            .take(usize::from(row) + 1)
            .skip(start + 1)
        {
            if !screen.row_wrapped((continuation - 1) as u16)
                && (line.trim().is_empty() || !line.starts_with("  "))
            {
                return false;
            }
        }
        row
    };

    // Include the prompt column: tabs and wide glyphs must render at the same
    // columns as the real input. A normal screen height also avoids the VT100
    // parser's single-row wrap edge case. Overflow loses the prefix and fails closed.
    let mut expected = ScreenBuffer::with_scrollback(cols, rows.max(2), 0);
    expected.process(&vec![b' '; usize::from(input_col)]);
    expected.process(&process_special_keys(text));
    let first_line = expected
        .screen()
        .rows(input_col, cols - input_col)
        .next()
        .unwrap_or_default();
    let expected_text = normalized_input(&expected.contents());
    let prefix: String = first_line.trim().chars().take(40).collect();
    // A very narrow fragment such as "[CRON" is not a message identity.
    if prefix.chars().count() < expected_text.chars().count().min(16) || prefix.is_empty() {
        return false;
    }
    let actual_first = screen
        .rows(input_col, cols - input_col)
        .nth(start)
        .unwrap_or_default();
    if !actual_first.trim_start().starts_with(&prefix) {
        return false;
    }
    // Short messages must not match longer replacement drafts. For cursor-based
    // multiline input, also distinguish continuations from indented output.
    if expected_text.chars().count() <= 40 || (framed_end.is_none() && end as usize > start) {
        let actual = screen.contents_between(start as u16, input_col, end, cols);
        return normalized_input(&actual) == expected_text;
    }
    true
}

pub(crate) fn drain_delayed_enters<B: PtyBackend>(
    pending: &mut Vec<PendingEnter>,
    sessions: &mut SessionManager<B>,
    logger: &mut impl LogSink,
    now: Instant,
    input_waiting: impl Fn(&str) -> bool,
) {
    pending.retain_mut(|enter| {
        if now < enter.fire_at {
            return true;
        }
        let Ok(session) = sessions.get_mut(&enter.session_id) else {
            if enter.quiet_since.is_none() {
                enter.log(
                    logger,
                    "[NOTIFICATION-DROPPED] delayed Enter: target session gone".into(),
                );
            } else {
                enter.give_up(logger, "target session gone");
            }
            return false;
        };
        let Some(quiet_since) = enter.quiet_since else {
            // Preserve the existing first Enter, including its 100 ms delay.
            // If ownership was already lost, never arm verification for it.
            let owned =
                session.last_input_time() == enter.input_at && !input_waiting(&enter.session_id);
            if session.send_keys("[ENTER]").is_err() {
                enter.log(
                    logger,
                    "[NOTIFICATION-DROPPED] delayed Enter: send failed".into(),
                );
                return false;
            }
            if !owned {
                enter.give_up(logger, "input changed before initial Enter");
                return false;
            }
            enter.input_at = session.last_input_time();
            enter.quiet_since = Some(enter.input_at);
            enter.fire_at = now + RETRY_DELAYS[0];
            return true;
        };
        if session.status() != &SessionStatus::Running {
            enter.give_up(logger, "target session exited");
            return false;
        }
        if session.last_input_time() != enter.input_at || input_waiting(&enter.session_id) {
            enter.give_up(logger, "intervening input");
            return false;
        }
        if !input_matches(session, &enter.text) {
            // This includes successful submission: history is never sufficient
            // evidence to send another Enter. Do not claim submission succeeded.
            return false;
        }
        if enter.checks == RETRY_DELAYS.len() {
            enter.give_up(logger, "matching input remains after final check");
            return false;
        }
        // Reuse the existing output-idle clock. Activity consumes a check but
        // does not send Enter. A late echo can settle before the next check;
        // continual output cannot keep this queue entry alive indefinitely.
        let quiet_for = now.saturating_duration_since(quiet_since).as_secs_f64();
        if session.idle_seconds_at(now) >= quiet_for {
            if session.send_keys("[ENTER]").is_err() {
                enter.give_up(logger, "retry send failed");
                return false;
            }
            enter.retries += 1;
            enter.input_at = session.last_input_time();
            enter.log(
                logger,
                format!(
                    "[ENTER-RETRY {}] matching injected input is still pending",
                    enter.retries
                ),
            );
        }
        enter.checks += 1;
        enter.quiet_since = Some(Instant::now());
        enter.fire_at = now
            + RETRY_DELAYS
                .get(enter.checks)
                .copied()
                .unwrap_or(Duration::from_secs(1));
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tttt_pty::{MockPty, PtySession};

    /// Echoes injected text, ignores the first Enter, then submits on the next.
    /// Accepted text remains in history above a fresh prompt, as in an agent UI.
    struct ScriptedPty {
        io: MockPty,
        enters: usize,
        submitted: bool,
        ignore_enters: usize,
        fail_writes: bool,
    }

    impl PtyBackend for ScriptedPty {
        fn write(&mut self, data: &[u8]) -> tttt_pty::Result<()> {
            if self.fail_writes {
                return Err(tttt_pty::PtyError::SessionExited);
            }
            self.io.write(data)?;
            if data == b"\r" {
                self.enters += 1;
                if self.enters > self.ignore_enters {
                    self.submitted = true;
                    self.io.queue_output("\r\nAccepted\r\n❯ ".as_bytes());
                }
            } else {
                self.io.queue_output("\x1b[2J\x1b[H❯ ".as_bytes());
                // Scheduler injection starts with a separate leading Enter.
                // The delayed standalone Enter is the one this script ignores.
                self.io
                    .queue_output(data.strip_prefix(b"\r").unwrap_or(data));
            }
            Ok(())
        }

        fn read(&mut self, data: &mut [u8]) -> tttt_pty::Result<usize> {
            self.io.read(data)
        }

        fn resize(&mut self, cols: u16, rows: u16) -> tttt_pty::Result<()> {
            self.io.resize(cols, rows)
        }

        fn kill(&mut self) -> tttt_pty::Result<()> {
            self.io.kill()
        }

        fn try_wait(&mut self) -> tttt_pty::Result<Option<i32>> {
            self.io.try_wait()
        }
    }

    #[derive(Default)]
    struct RecordingLog(Vec<LogEvent>);

    impl LogSink for RecordingLog {
        fn log_event(&mut self, event: &LogEvent) -> tttt_log::Result<()> {
            self.0.push(event.clone());
            Ok(())
        }

        fn flush(&mut self) -> tttt_log::Result<()> {
            Ok(())
        }
    }

    struct Harness {
        sessions: SessionManager<ScriptedPty>,
        pending: Vec<PendingEnter>,
        logger: RecordingLog,
        start: Instant,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            let backend = ScriptedPty {
                io: MockPty::new(80, 24),
                enters: 0,
                submitted: false,
                ignore_enters: 1,
                fail_writes: false,
            };
            let mut session = PtySession::new("target".into(), backend, "claude".into(), 80, 24);
            session.send_keys(text).unwrap();
            session.pump().unwrap();
            let start = Instant::now();
            let pending = vec![PendingEnter::new(&session, text, start)];
            let mut sessions = SessionManager::new();
            sessions.add_session(session).unwrap();
            Self {
                sessions,
                pending,
                logger: RecordingLog::default(),
                start,
            }
        }

        fn tick(&mut self, ms: u64) {
            drain_delayed_enters(
                &mut self.pending,
                &mut self.sessions,
                &mut self.logger,
                self.start + Duration::from_millis(ms),
                |_| false,
            );
        }

        fn backend(&self) -> &ScriptedPty {
            self.sessions.get("target").unwrap().backend()
        }
    }

    #[test]
    fn ignored_first_enter_is_retried_and_submits() {
        let mut h = Harness::new("[CRON job-7]: check the pending work");
        h.tick(99);
        assert_eq!(h.backend().enters, 0);
        h.tick(100);
        assert_eq!(h.backend().enters, 1);
        assert!(!h.backend().submitted);
        h.tick(1099);
        assert_eq!(h.backend().enters, 1);
        h.tick(1100);
        assert_eq!(
            h.backend().enters,
            2,
            "ignored Enter must be retried after one second"
        );
        assert!(h.backend().submitted);

        h.sessions.get_mut("target").unwrap().pump().unwrap();
        h.tick(3100);
        h.tick(7100);
        assert_eq!(
            h.backend().enters,
            2,
            "submitted text in history must not trigger another Enter"
        );
        assert!(h.pending.is_empty());
        assert!(h.logger.0.iter().any(|event| {
            event.session_id == "target"
                && event.direction == Direction::Meta
                && event.data.starts_with(b"[ENTER-RETRY 1]")
        }));
    }

    #[test]
    fn different_input_with_injection_in_history_is_not_submitted() {
        let text = "[CRON job-7]: check the pending work";
        let mut h = Harness::new(text);
        h.tick(100);
        // A redraw replaces the input without a tttt key write. This exercises
        // the screen guard independently of any input-activity cancellation.
        let session = h.sessions.get_mut("target").unwrap();
        session
            .backend_mut()
            .io
            .queue_output(format!("\x1b[2J\x1b[H❯ {text}\r\nAccepted\r\n❯ human draft").as_bytes());
        session.pump().unwrap();
        for ms in [1100, 3100, 7100, 11100] {
            h.tick(ms);
        }
        assert_eq!(h.backend().enters, 1, "never submit replacement input");
        assert!(!h.backend().submitted);
        assert!(h.pending.is_empty());
        assert!(!h
            .logger
            .0
            .iter()
            .any(|event| event.data.starts_with(b"[ENTER-RETRY")));
    }

    #[test]
    fn three_retries_use_backoff_then_log_give_up_once() {
        let mut h = Harness::new("[REMINDER: check the pending work]");
        h.sessions
            .get_mut("target")
            .unwrap()
            .backend_mut()
            .ignore_enters = usize::MAX;
        for (ms, enters) in [
            (100, 1),
            (1099, 1),
            (1100, 2),
            (3099, 2),
            (3100, 3),
            (7099, 3),
            (7100, 4),
            (8099, 4),
            (8100, 4),
            (60000, 4),
        ] {
            h.tick(ms);
            assert_eq!(h.backend().enters, enters, "at {ms} ms");
        }
        assert!(h.pending.is_empty());
        let events: Vec<_> = h
            .logger
            .0
            .iter()
            .map(|event| {
                assert_eq!(event.direction, Direction::Meta);
                assert_eq!(event.session_id, "target");
                String::from_utf8_lossy(&event.data)
            })
            .collect();
        assert_eq!(events.len(), 4);
        for (index, event) in events[..3].iter().enumerate() {
            assert!(event.starts_with(&format!("[ENTER-RETRY {}]", index + 1)));
        }
        assert!(events[3].starts_with("[ENTER-GAVE-UP]"));
    }

    #[test]
    fn successful_initial_enter_does_not_retry_history() {
        let mut h = Harness::new("[CRON job-7]: check the pending work");
        h.sessions
            .get_mut("target")
            .unwrap()
            .backend_mut()
            .ignore_enters = 0;
        h.tick(100);
        h.sessions.get_mut("target").unwrap().pump().unwrap();
        h.tick(1100);
        assert!(h.backend().submitted);
        assert_eq!(h.backend().enters, 1);
        assert!(h.pending.is_empty());
        assert!(h.logger.0.is_empty());
    }

    #[test]
    fn late_echo_waits_for_output_silence_then_retries() {
        let mut h = Harness::new("[CRON job-7]: check the pending work");
        h.tick(100);
        // A cursor redraw counts as output even though the text is unchanged.
        let session = h.sessions.get_mut("target").unwrap();
        session.backend_mut().io.queue_output(b"\x1b[0m");
        session.pump().unwrap();
        h.tick(1100);
        assert_eq!(
            h.backend().enters,
            1,
            "output since Enter forbids this retry"
        );
        assert!(!h.pending.is_empty());
        h.tick(3100);
        assert_eq!(h.backend().enters, 2, "late echo has now settled");
    }

    #[test]
    fn continuous_output_is_bounded_without_any_retry() {
        let mut h = Harness::new("[CRON job-7]: check the pending work");
        h.tick(100);
        for ms in [1100, 3100, 7100, 8100] {
            let session = h.sessions.get_mut("target").unwrap();
            session.backend_mut().io.queue_output(b"\x1b[0m");
            session.pump().unwrap();
            h.tick(ms);
        }
        assert_eq!(h.backend().enters, 1);
        assert!(h.pending.is_empty());
        assert_eq!(h.logger.0.len(), 1);
        assert!(h.logger.0[0].data.starts_with(b"[ENTER-GAVE-UP]"));
    }

    #[test]
    fn intervening_input_cancels_even_before_its_echo() {
        for input_kind in 0..3 {
            let mut h = Harness::new("[CRON job-7]: check the pending work");
            h.tick(100);
            let session = h.sessions.get_mut("target").unwrap();
            match input_kind {
                0 => session.send_raw(b"human draft").unwrap(),
                1 => {
                    session.try_send_raw(b"human draft").unwrap();
                }
                _ => session.send_keys("another injection").unwrap(),
            }
            // Do not pump: the screen still contains the original injection.
            h.tick(1100);
            assert_eq!(h.backend().enters, 1);
            assert!(h.pending.is_empty());
            assert!(h.logger.0[0]
                .data
                .starts_with(b"[ENTER-GAVE-UP] intervening input"));
        }
    }

    #[test]
    fn wrapped_unicode_and_rendered_escape_text_matches() {
        for text in [
            "[REMINDER: café 界界 é ".to_owned() + &"long text ".repeat(20) + "]",
            r"[REMINDER: \x1b[31mcolored\x1b[0m café]".to_owned(),
        ] {
            let mut h = Harness::new(&text);
            assert!(input_matches(h.sessions.get("target").unwrap(), &text));
            h.tick(100);
            h.tick(1100);
            assert_eq!(h.backend().enters, 2);
        }
    }

    #[test]
    fn indented_multiline_input_matches_without_using_footer() {
        let text = "[REMINDER: check the pending work and report progress]\r\n  continued input";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.inject_screen_data(
            "\x1b[2J\x1b[H❯ [REMINDER: check the pending work and report progress]\r\n  continued input\r\n────────────────────\r\n  status footer\x1b[2;18H".as_bytes(),
        );
        assert!(input_matches(session, text));
        session.inject_screen_data(b"\x1b[4;16H");
        assert!(!input_matches(session, text), "footer cursor is not input");
    }

    #[test]
    fn hidden_cursor_unknown_layout_and_blank_input_do_not_match() {
        let text = "[CRON job-7]: check the pending work";
        for screen in [
            format!("❯ {text}\x1b[?25l"),
            format!("output: {text}"),
            format!("❯ {text}\r\n\r\n❯ "),
            format!("❯ {text}\x1b[?2026h"),
        ] {
            let mut h = Harness::new(text);
            let session = h.sessions.get_mut("target").unwrap();
            session.inject_screen_data(format!("\x1b[2J\x1b[H{screen}").as_bytes());
            assert!(!input_matches(session, text), "screen {screen:?}");
        }
    }

    #[test]
    fn gone_exited_and_failed_write_stop_verification() {
        for failure in 0..3 {
            let mut h = Harness::new("[CRON job-7]: check the pending work");
            h.tick(100);
            match failure {
                0 => h.sessions = SessionManager::new(),
                1 => h.sessions.get_mut("target").unwrap().kill().unwrap(),
                _ => {
                    h.sessions
                        .get_mut("target")
                        .unwrap()
                        .backend_mut()
                        .fail_writes = true
                }
            }
            h.tick(1100);
            assert!(h.pending.is_empty());
            assert_eq!(h.logger.0.len(), 1);
            assert!(h.logger.0[0].data.starts_with(b"[ENTER-GAVE-UP]"));
        }
    }

    #[test]
    fn cursor_in_history_cannot_identify_the_current_input() {
        let text = "[CRON job-7]: check the pending work";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.inject_screen_data(
            format!("\x1b[2J\x1b[H❯ {text}\r\nAccepted\r\n❯ human draft\x1b[1;20H").as_bytes(),
        );
        assert!(!input_matches(session, text));
    }

    #[test]
    fn short_message_must_match_the_whole_input() {
        let text = "[REMINDER: ok]";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.inject_screen_data(format!("\x1b[2J\x1b[H❯ {text} human draft").as_bytes());
        assert!(!input_matches(session, text));
    }

    #[test]
    fn framed_composer_can_identify_input_with_a_hidden_cursor() {
        let text = "[CRON job-7]: check the pending work";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.inject_screen_data(format!(
            "\x1b[2J\x1b[HOld conversation\r\n────────────────────\r\n❯ {text}\r\n────────────────────\r\n  status footer\x1b[?25l"
        ).as_bytes());
        assert!(input_matches(session, text));
        h.tick(100);
        h.tick(1100);
        assert_eq!(h.backend().enters, 2);
    }

    #[test]
    fn framed_history_does_not_identify_a_different_current_composer() {
        let text = "[CRON job-7]: check the pending work";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.inject_screen_data(format!(
            "\x1b[2J\x1b[H❯ {text}\r\nAccepted\r\n────────────────────\r\n❯ different input\r\n────────────────────\r\n  status footer\x1b[?25l"
        ).as_bytes());
        assert!(!input_matches(session, text));
    }

    #[test]
    fn matching_uses_the_actual_prompt_column_for_tabs() {
        let text = r"[REMINDER: \tcheck]";
        let mut h = Harness::new(text);
        assert!(input_matches(h.sessions.get("target").unwrap(), text));
        h.tick(100);
        h.tick(1100);
        assert_eq!(h.backend().enters, 2);
    }

    #[test]
    fn a_narrow_fragment_is_not_a_distinctive_prefix() {
        let text = "[CRON job-7]: check the pending work";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.resize(10, 24).unwrap();
        session.inject_screen_data(format!("\x1b[2J\x1b[H❯ {text}").as_bytes());
        assert!(!input_matches(session, text));
    }

    #[test]
    fn backpressured_keyboard_input_blocks_retry_before_a_pty_write() {
        let mut h = Harness::new("[CRON job-7]: check the pending work");
        h.tick(100);
        drain_delayed_enters(
            &mut h.pending,
            &mut h.sessions,
            &mut h.logger,
            h.start + Duration::from_millis(1100),
            |id| id == "target",
        );
        assert_eq!(h.backend().enters, 1);
        assert!(h.pending.is_empty());
        assert!(h.logger.0[0]
            .data
            .starts_with(b"[ENTER-GAVE-UP] intervening input"));
    }

    #[test]
    fn indented_output_is_not_a_continuation_of_pending_input() {
        let text = "[CRON job-7]: check the pending work and report any progress";
        let mut h = Harness::new(text);
        let session = h.sessions.get_mut("target").unwrap();
        session.inject_screen_data(
            format!("\x1b[2J\x1b[H❯ {text}\r\n  accepted and working").as_bytes(),
        );
        assert!(!input_matches(session, text));
    }

    #[test]
    fn scheduler_leading_enter_is_excluded_from_message_identity() {
        for text in [
            "[ENTER][CRON job-7]: check the pending work",
            "[ENTER][REMINDER: check the pending work]",
        ] {
            let mut h = Harness::new(text);
            h.tick(100);
            h.tick(1100);
            assert_eq!(h.backend().enters, 2);
            assert!(h.backend().submitted);
        }
    }

    #[test]
    fn failed_initial_enter_keeps_existing_drop_event_and_does_not_arm_retries() {
        for gone in [false, true] {
            let mut h = Harness::new("[CRON job-7]: check the pending work");
            if gone {
                h.sessions = SessionManager::new();
            } else {
                h.sessions
                    .get_mut("target")
                    .unwrap()
                    .backend_mut()
                    .fail_writes = true;
            }
            h.tick(100);
            h.tick(1100);
            assert!(h.pending.is_empty());
            assert_eq!(h.logger.0.len(), 1);
            assert!(h.logger.0[0].data.starts_with(b"[NOTIFICATION-DROPPED]"));
        }
    }

    #[test]
    fn a_late_event_loop_tick_does_not_burst_retries() {
        let mut h = Harness::new("[CRON job-7]: check the pending work");
        h.sessions
            .get_mut("target")
            .unwrap()
            .backend_mut()
            .ignore_enters = usize::MAX;
        h.tick(100);
        h.tick(10000);
        assert_eq!(h.backend().enters, 2);
        h.tick(10000);
        h.tick(11999);
        assert_eq!(h.backend().enters, 2);
        h.tick(12000);
        assert_eq!(h.backend().enters, 3);
    }
}
