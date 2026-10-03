//! Non-blocking input writes, with pacing only for scheduler messages.

use crate::injected_enter::PendingEnter;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tttt_pty::{process_special_keys, PtyBackend, PtySession};
use tttt_scheduler::SchedulerEvent;

const SCHED_CHUNK_BYTES: usize = 48;
const SCHED_CHUNK_GAP_MS: u64 = 8;

#[derive(Clone, Copy)]
pub(crate) struct SchedulerInputConfig {
    bytes: usize,
    gap: Duration,
}

impl SchedulerInputConfig {
    pub(crate) fn from_env() -> Self {
        Self::parse(
            std::env::var("TTTT_SCHED_CHUNK_BYTES").ok().as_deref(),
            std::env::var("TTTT_SCHED_CHUNK_GAP_MS").ok().as_deref(),
        )
    }

    fn parse(bytes: Option<&str>, gap: Option<&str>) -> Self {
        Self {
            bytes: bytes
                .and_then(|s| s.parse().ok())
                .unwrap_or(SCHED_CHUNK_BYTES),
            gap: Duration::from_millis(
                gap.and_then(|s| s.parse().ok())
                    .unwrap_or(SCHED_CHUNK_GAP_MS),
            ),
        }
    }
}

/// Find a boundary in the already-decoded key bytes. Hex escapes may produce
/// arbitrary bytes, so do not require the entire input to be valid UTF-8.
/// A character larger than the configured limit is sent whole to make progress.
fn chunk_len(data: &[u8], limit: usize) -> usize {
    if limit == 0 || data.len() <= limit {
        return data.len();
    }
    let mut end = limit;
    while end > 0 && data[end] & 0xc0 == 0x80 {
        end -= 1;
    }
    if end == 0 {
        end = limit;
        while end < data.len() && data[end] & 0xc0 == 0x80 {
            end += 1;
        }
    }
    end
}

struct SchedulerInput {
    text: String,
    data: Vec<u8>,
    offset: usize,
    chunk_end: usize,
    next_write: Instant,
    config: SchedulerInputConfig,
}

/// User bytes retain their original priority and unpaced, single-attempt writes.
/// Scheduler continuations share the same per-session input drain.
#[derive(Default)]
pub(crate) struct PendingInput {
    pub(crate) user: VecDeque<u8>,
    scheduler: VecDeque<SchedulerInput>,
}

impl PendingInput {
    pub(crate) fn is_empty(&self) -> bool {
        self.user.is_empty() && self.scheduler.is_empty()
    }

    /// Called only after the existing input-idle and Wait/Drop checks, for both
    /// new and deferred events. Attempt the first write now so the next event's
    /// busy check observes input activity just as it did with send_keys.
    pub(crate) fn send_scheduler<B: PtyBackend>(
        &mut self,
        event: &SchedulerEvent,
        session: &mut PtySession<B>,
        config: SchedulerInputConfig,
        enters: &mut Vec<PendingEnter>,
        now: Instant,
    ) -> tttt_pty::Result<()> {
        let text = match event {
            SchedulerEvent::ReminderFired(reminder) => {
                format!("[ENTER][REMINDER: {}]", reminder.message)
            }
            SchedulerEvent::CronFired(job) => {
                let cmd = job.command.trim_end_matches(['\r', '\n']);
                format!("[ENTER][CRON {}]: {}", job.id, cmd)
            }
        };
        if config.bytes == 0 {
            // Explicit escape hatch: preserve the old send_keys/write path.
            session.send_keys(&text)?;
            enters.push(PendingEnter::new(session, &text, now.max(Instant::now())));
            return Ok(());
        }
        // Decode once, before splitting, so [ENTER], ^C and hex escapes cannot
        // be cut in half and accidentally typed literally.
        let data = process_special_keys(&text);
        let chunk_end = chunk_len(&data, config.bytes);
        self.scheduler.push_back(SchedulerInput {
            text,
            data,
            offset: 0,
            chunk_end,
            next_write: now,
            config,
        });
        self.drain_scheduler(session, enters, now)
    }

    pub(crate) fn drain<B: PtyBackend>(
        &mut self,
        session: &mut PtySession<B>,
        enters: &mut Vec<PendingEnter>,
        now: Instant,
    ) -> tttt_pty::Result<()> {
        if !self.user.is_empty() {
            let n = session.try_send_raw(self.user.make_contiguous())?;
            self.user.drain(..n.min(self.user.len()));
            return Ok(());
        }
        self.drain_scheduler(session, enters, now)
    }

    fn drain_scheduler<B: PtyBackend>(
        &mut self,
        session: &mut PtySession<B>,
        enters: &mut Vec<PendingEnter>,
        now: Instant,
    ) -> tttt_pty::Result<()> {
        let Some(input) = self.scheduler.front_mut() else {
            return Ok(());
        };
        if now < input.next_write {
            return Ok(());
        }
        let n = session.try_send_raw(&input.data[input.offset..input.chunk_end])?;
        input.offset += n;
        // Use the time AFTER the write, never a deadline from an earlier tick.
        let written_at = now.max(session.last_input_time());
        if input.offset == input.data.len() {
            enters.push(PendingEnter::new(session, &input.text, written_at));
            self.scheduler.pop_front();
        } else {
            if input.offset == input.chunk_end {
                input.chunk_end += chunk_len(&input.data[input.offset..], input.config.bytes);
            }
            // One attempt per drain, including short writes and backpressure.
            // Never catch up by bursting overdue chunks in a loop.
            input.next_write = written_at + input.config.gap;
        }
        Ok(())
    }

    /// Bound the normal event-loop poll by the next scheduler write deadline.
    /// Round up to milliseconds to avoid spinning just before a deadline.
    pub(crate) fn poll_timeout_ms(&self, now: Instant, normal: u16) -> u16 {
        if !self.user.is_empty() {
            // Scheduler writes cannot run while user input has priority. Keep
            // the existing backpressure polling interval for those user bytes.
            return normal;
        }
        self.scheduler.front().map_or(normal, |input| {
            let wait = input.next_write.saturating_duration_since(now);
            let ms = wait.as_millis() + u128::from(wait.subsec_nanos() % 1_000_000 != 0);
            normal.min(ms.min(u128::from(u16::MAX)) as u16)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injected_enter::drain_delayed_enters;
    use tttt_log::MultiLogger;
    use tttt_pty::{MockPty, SessionManager};
    use tttt_scheduler::{BusyPolicy, CronJob, Reminder};

    struct RecordingPty {
        io: MockPty,
        writes: Vec<Vec<u8>>,
        max_write: usize,
    }

    impl PtyBackend for RecordingPty {
        fn write(&mut self, data: &[u8]) -> tttt_pty::Result<()> {
            self.writes.push(data.to_vec());
            self.io.write(data)
        }

        fn try_write(&mut self, data: &[u8]) -> tttt_pty::Result<usize> {
            let n = data.len().min(self.max_write);
            if n > 0 {
                self.write(&data[..n])?;
            }
            Ok(n)
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

    fn session() -> PtySession<RecordingPty> {
        PtySession::new(
            "target".into(),
            RecordingPty {
                io: MockPty::new(80, 24),
                writes: Vec::new(),
                max_write: usize::MAX,
            },
            "claude".into(),
            80,
            24,
        )
    }

    fn reminder(message: &str) -> SchedulerEvent {
        SchedulerEvent::ReminderFired(Reminder {
            id: "reminder-1".into(),
            message: message.into(),
            session_id: Some("target".into()),
            fire_at: None,
        })
    }

    fn cron(command: &str) -> SchedulerEvent {
        SchedulerEvent::CronFired(CronJob {
            id: "cron-1".into(),
            expression: "1s".into(),
            command: command.into(),
            session_id: Some("target".into()),
            if_busy: BusyPolicy::Wait,
            interval: None,
            next_fire: None,
        })
    }

    fn chunks(mut data: &[u8], bytes: usize) -> Vec<&[u8]> {
        let mut result = Vec::new();
        while !data.is_empty() {
            let n = chunk_len(data, bytes);
            result.push(&data[..n]);
            data = &data[n..];
        }
        result
    }

    #[test]
    fn chunk_splitting_preserves_utf8_even_with_tiny_limits() {
        let text = "aé界🦀e\u{301}z";
        for limit in 1..=text.len() {
            let parts = chunks(text.as_bytes(), limit);
            assert_eq!(parts.concat(), text.as_bytes());
            for part in parts {
                let part = std::str::from_utf8(part).unwrap();
                assert!(part.len() <= limit || part.chars().count() == 1);
            }
        }
    }

    #[test]
    fn chunk_splitting_exact_multiple_empty_and_single_write_hatch() {
        assert_eq!(chunks(b"abcdef", 3), vec![b"abc", b"def"]);
        assert_eq!(chunks(b"abcdef", 0), vec![b"abcdef"]);
        assert_eq!(chunks("café".as_bytes(), 0), vec!["café".as_bytes()]);
        for limit in [0, 1, 48] {
            assert_eq!(chunk_len(b"", limit), 0);
            assert!(chunks(b"", limit).is_empty());
        }
    }

    #[test]
    fn scheduler_config_defaults_overrides_and_invalid_values() {
        let default = SchedulerInputConfig::parse(None, None);
        assert_eq!(default.bytes, 48);
        assert_eq!(default.gap, Duration::from_millis(8));
        let custom = SchedulerInputConfig::parse(Some("12"), Some("25"));
        assert_eq!(custom.bytes, 12);
        assert_eq!(custom.gap, Duration::from_millis(25));
        let disabled = SchedulerInputConfig::parse(Some("0"), Some("0"));
        assert_eq!(disabled.bytes, 0);
        assert_eq!(disabled.gap, Duration::ZERO);
        let invalid = SchedulerInputConfig::parse(Some("-1"), Some("oops"));
        assert_eq!(invalid.bytes, default.bytes);
        assert_eq!(invalid.gap, default.gap);
    }

    #[test]
    fn scheduler_cron_and_reminder_write_long_text_in_paced_chunks() {
        // This is the shared scheduler delivery entry point used by both the
        // immediate arms and deferred replay in App, with the existing MockPty.
        let message = "café 界 🦀 ".repeat(20);
        for (event, expected) in [
            (
                cron(&(message.clone() + "\r\n")),
                format!("\r[CRON cron-1]: {message}"),
            ),
            (reminder(&message), format!("\r[REMINDER: {message}]")),
        ] {
            let mut session = session();
            let mut input = PendingInput::default();
            let mut enters = Vec::new();
            let config = SchedulerInputConfig::parse(None, None);
            let mut now = Instant::now() + Duration::from_secs(1);
            input
                .send_scheduler(&event, &mut session, config, &mut enters, now)
                .unwrap();
            assert_eq!(session.backend().writes.len(), 1);
            while !input.is_empty() {
                let count = session.backend().writes.len();
                input
                    .drain(
                        &mut session,
                        &mut enters,
                        now + config.gap - Duration::from_nanos(1),
                    )
                    .unwrap();
                assert_eq!(session.backend().writes.len(), count);
                now += config.gap;
                input.drain(&mut session, &mut enters, now).unwrap();
                assert_eq!(session.backend().writes.len(), count + 1);
            }
            assert_eq!(session.backend().io.input_buf, expected.as_bytes());
            assert!(session.backend().writes.len() > 3);
            for write in &session.backend().writes {
                assert!(write.len() <= 48);
                assert!(std::str::from_utf8(write).is_ok());
            }
            assert_eq!(enters.len(), 1);
        }
    }

    #[test]
    fn scheduler_zero_bytes_uses_original_single_write_and_key_decoding() {
        let mut session = session();
        session.backend_mut().max_write = 0; // try_write would make no progress.
        let mut input = PendingInput::default();
        let mut enters = Vec::new();
        let text = "long text ".repeat(40) + "[TAB]café\\x1b[31m";
        input
            .send_scheduler(
                &reminder(&text),
                &mut session,
                SchedulerInputConfig::parse(Some("0"), None),
                &mut enters,
                Instant::now(),
            )
            .unwrap();
        assert_eq!(
            session.backend().writes,
            vec![process_special_keys(&format!("[ENTER][REMINDER: {text}]"))]
        );
        assert!(input.is_empty());
        assert_eq!(enters.len(), 1);
    }

    #[test]
    fn scheduler_decodes_special_keys_before_chunking() {
        let mut session = session();
        let mut input = PendingInput::default();
        let mut enters = Vec::new();
        let text = "[TAB]café^C\\x1b[31m\\xff[ENTER]";
        let mut now = Instant::now() + Duration::from_secs(1);
        let config = SchedulerInputConfig::parse(Some("2"), Some("8"));
        input
            .send_scheduler(&reminder(text), &mut session, config, &mut enters, now)
            .unwrap();
        while !input.is_empty() {
            now += config.gap;
            input.drain(&mut session, &mut enters, now).unwrap();
        }
        assert_eq!(
            session.backend().io.input_buf,
            process_special_keys(&format!("[ENTER][REMINDER: {text}]"))
        );
    }

    #[test]
    fn pending_enter_starts_after_last_chunk_and_verifies_full_text() {
        let mut session = session();
        let mut input = PendingInput::default();
        let mut enters = Vec::new();
        let config = SchedulerInputConfig::parse(Some("8"), Some("75"));
        let start = Instant::now() + Duration::from_secs(1);
        input
            .send_scheduler(
                &reminder("check café"),
                &mut session,
                config,
                &mut enters,
                start,
            )
            .unwrap();
        let mut now = start;
        while !input.is_empty() {
            assert!(enters.is_empty(), "no Enter timer before the last chunk");
            now += config.gap;
            input.drain(&mut session, &mut enters, now).unwrap();
        }
        assert!(now.duration_since(start) > Duration::from_millis(100));
        let text = session.backend().io.input_buf.clone();
        session.inject_screen_data("❯ [REMINDER: check café]".as_bytes());
        let mut sessions = SessionManager::new();
        sessions.add_session(session).unwrap();
        let mut logger = MultiLogger::new();
        drain_delayed_enters(
            &mut enters,
            &mut sessions,
            &mut logger,
            now + Duration::from_millis(99),
            |_| false,
        );
        assert_eq!(sessions.get("target").unwrap().backend().io.input_buf, text);
        drain_delayed_enters(
            &mut enters,
            &mut sessions,
            &mut logger,
            now + Duration::from_millis(100),
            |_| false,
        );
        assert_eq!(
            sessions.get("target").unwrap().backend().io.input_buf,
            [text.as_slice(), b"\r"].concat()
        );
        // The mock ignores Enter. Retry requires the complete message identity,
        // not the final chunk, and uses the unchanged Enter-verification code.
        drain_delayed_enters(
            &mut enters,
            &mut sessions,
            &mut logger,
            now + Duration::from_millis(1100),
            |_| false,
        );
        assert_eq!(
            sessions.get("target").unwrap().backend().io.input_buf,
            [text.as_slice(), b"\r\r"].concat()
        );
    }

    #[test]
    fn scheduler_short_writes_and_backpressure_do_not_lose_bytes_or_arm_enter() {
        let mut session = session();
        session.backend_mut().max_write = 0;
        let mut input = PendingInput::default();
        let mut enters = Vec::new();
        let config = SchedulerInputConfig::parse(Some("5"), None);
        let mut now = Instant::now() + Duration::from_secs(1);
        input
            .send_scheduler(&reminder("work"), &mut session, config, &mut enters, now)
            .unwrap();
        assert!(session.backend().io.input_buf.is_empty());
        assert!(enters.is_empty());
        session.backend_mut().max_write = 2;
        while !input.is_empty() {
            assert!(enters.is_empty());
            now += config.gap;
            input.drain(&mut session, &mut enters, now).unwrap();
        }
        assert_eq!(session.backend().io.input_buf, b"\r[REMINDER: work]");
        assert_eq!(enters.len(), 1);
    }

    #[test]
    fn scheduler_late_tick_does_not_burst_and_poll_follows_deadline() {
        let mut session = session();
        let mut input = PendingInput::default();
        let mut enters = Vec::new();
        let config = SchedulerInputConfig::parse(Some("4"), None);
        let start = Instant::now() + Duration::from_secs(1);
        input
            .send_scheduler(&reminder("work"), &mut session, config, &mut enters, start)
            .unwrap();
        assert_eq!(input.poll_timeout_ms(start, 50), 8);
        assert_eq!(
            input.poll_timeout_ms(start + Duration::from_micros(7500), 50),
            1
        );
        let late = start + Duration::from_secs(10);
        assert_eq!(input.poll_timeout_ms(late, 50), 0);
        input.drain(&mut session, &mut enters, late).unwrap();
        input.drain(&mut session, &mut enters, late).unwrap();
        assert_eq!(session.backend().writes.len(), 2);
        assert_eq!(input.poll_timeout_ms(late, 50), 8);
    }

    #[test]
    fn user_input_retains_priority_and_unpaced_delivery() {
        let mut session = session();
        let mut input = PendingInput::default();
        let mut enters = Vec::new();
        let now = Instant::now() + Duration::from_secs(1);
        input
            .send_scheduler(
                &reminder("work"),
                &mut session,
                SchedulerInputConfig::parse(Some("4"), None),
                &mut enters,
                now,
            )
            .unwrap();
        let user = vec![b'x'; 1024];
        input.user.extend(&user);
        assert_eq!(input.poll_timeout_ms(now + Duration::from_secs(1), 10), 10);
        input.drain(&mut session, &mut enters, now).unwrap();
        assert_eq!(session.backend().writes.last().unwrap(), &user);
        assert!(input.user.is_empty());
        assert!(enters.is_empty());
    }
}
