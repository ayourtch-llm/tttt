//! Delayed submission of injected terminal input.

use std::time::Instant;
use tttt_log::{Direction, LogEvent, LogSink};
use tttt_pty::{PtyBackend, SessionManager};

pub(crate) fn drain_delayed_enters<B: PtyBackend>(
    pending: &mut Vec<(String, Instant)>,
    sessions: &mut SessionManager<B>,
    logger: &mut impl LogSink,
    now: Instant,
) {
    let mut remaining = Vec::new();
    for (session_id, fire_at) in std::mem::take(pending) {
        if now >= fire_at {
            let sent = match sessions.get_mut(&session_id) {
                Ok(session) => session.send_keys("[ENTER]").is_ok(),
                Err(_) => false,
            };
            if !sent {
                let _ = logger.log_event(&LogEvent::new(
                    session_id,
                    Direction::Meta,
                    b"[NOTIFICATION-DROPPED] delayed Enter: target session gone".to_vec(),
                ));
            }
        } else {
            remaining.push((session_id, fire_at));
        }
    }
    *pending = remaining;
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
    }

    impl PtyBackend for ScriptedPty {
        fn write(&mut self, data: &[u8]) -> tttt_pty::Result<()> {
            self.io.write(data)?;
            if data == b"\r" {
                self.enters += 1;
                if self.enters > 1 {
                    self.submitted = true;
                    self.io.queue_output("\r\nAccepted\r\n❯ ".as_bytes());
                }
            } else {
                self.io.queue_output("\x1b[2J\x1b[H❯ ".as_bytes());
                self.io.queue_output(data);
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
        pending: Vec<(String, Instant)>,
        logger: RecordingLog,
        start: Instant,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            let backend = ScriptedPty {
                io: MockPty::new(80, 24),
                enters: 0,
                submitted: false,
            };
            let mut session = PtySession::new("target".into(), backend, "claude".into(), 80, 24);
            session.send_keys(text).unwrap();
            session.pump().unwrap();
            let start = Instant::now();
            let mut sessions = SessionManager::new();
            sessions.add_session(session).unwrap();
            Self {
                sessions,
                pending: vec![("target".into(), start + Duration::from_millis(100))],
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
        assert_eq!(h.backend().enters, 2, "ignored Enter must be retried after one second");
        assert!(h.backend().submitted);

        h.sessions.get_mut("target").unwrap().pump().unwrap();
        h.tick(3100);
        h.tick(7100);
        assert_eq!(h.backend().enters, 2, "submitted text in history must not trigger another Enter");
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
        session.backend_mut().io.queue_output(
            format!("\x1b[2J\x1b[H❯ {text}\r\nAccepted\r\n❯ human draft").as_bytes(),
        );
        session.pump().unwrap();
        for ms in [1100, 3100, 7100, 11100] {
            h.tick(ms);
        }
        assert_eq!(h.backend().enters, 1, "never submit replacement input");
        assert!(!h.backend().submitted);
        assert!(h.pending.is_empty());
        assert!(!h.logger.0.iter().any(|event| event.data.starts_with(b"[ENTER-RETRY")));
    }
}
