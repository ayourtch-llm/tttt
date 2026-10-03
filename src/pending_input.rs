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
