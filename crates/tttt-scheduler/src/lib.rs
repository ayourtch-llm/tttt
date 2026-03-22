mod error;

pub use error::{SchedulerError, Result};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// A one-shot reminder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub id: String,
    pub message: String,
    #[serde(skip)]
    pub fire_at: Option<Instant>,
}

/// A recurring cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub expression: String,
    pub command: String,
    pub session_id: Option<String>,
    /// Interval in seconds (simplified cron: we parse "every N seconds" style).
    #[serde(skip)]
    pub interval: Option<Duration>,
    #[serde(skip)]
    pub next_fire: Option<Instant>,
}

/// Events emitted by the scheduler.
#[derive(Debug, Clone)]
pub enum SchedulerEvent {
    ReminderFired(Reminder),
    CronFired(CronJob),
}

/// Time-based scheduler for reminders and cron jobs.
///
/// Designed for testability: `tick()` takes an explicit `now` parameter.
pub struct Scheduler {
    reminders: BTreeMap<u64, Vec<Reminder>>,
    cron_jobs: HashMap<String, CronJob>,
    next_reminder_id: u64,
    next_cron_id: u64,
    /// Monotonic counter used as BTreeMap key to maintain insertion order
    /// while allowing multiple reminders at the same logical time.
    epoch: Instant,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            reminders: BTreeMap::new(),
            cron_jobs: HashMap::new(),
            next_reminder_id: 1,
            next_cron_id: 1,
            epoch: Instant::now(),
        }
    }

    /// Create a scheduler with a specific epoch (for testing).
    pub fn with_epoch(epoch: Instant) -> Self {
        Self {
            reminders: BTreeMap::new(),
            cron_jobs: HashMap::new(),
            next_reminder_id: 1,
            next_cron_id: 1,
            epoch,
        }
    }

    /// Add a one-shot reminder.
    pub fn add_reminder(&mut self, message: String, fire_at: Instant) -> String {
        let id = format!("reminder-{}", self.next_reminder_id);
        self.next_reminder_id += 1;

        let key = fire_at.duration_since(self.epoch).as_millis() as u64;
        let reminder = Reminder {
            id: id.clone(),
            message,
            fire_at: Some(fire_at),
        };

        self.reminders.entry(key).or_default().push(reminder);
        id
    }

    /// Add a cron job with an interval in seconds.
    pub fn add_cron(
        &mut self,
        expression: String,
        command: String,
        session_id: Option<String>,
        now: Instant,
    ) -> Result<String> {
        let interval = parse_interval(&expression)?;
        let id = format!("cron-{}", self.next_cron_id);
        self.next_cron_id += 1;

        let job = CronJob {
            id: id.clone(),
            expression,
            command,
            session_id,
            interval: Some(interval),
            next_fire: Some(now + interval),
        };

        self.cron_jobs.insert(id.clone(), job);
        Ok(id)
    }

    /// Remove a cron job.
    pub fn remove_cron(&mut self, id: &str) -> Result<()> {
        self.cron_jobs
            .remove(id)
            .ok_or_else(|| SchedulerError::NotFound(id.to_string()))?;
        Ok(())
    }

    /// List all cron jobs.
    pub fn list_cron(&self) -> Vec<&CronJob> {
        self.cron_jobs.values().collect()
    }

    /// Tick the scheduler, returning all events that should fire at or before `now`.
    pub fn tick(&mut self, now: Instant) -> Vec<SchedulerEvent> {
        let mut events = Vec::new();

        // Fire due reminders
        let now_key = now.duration_since(self.epoch).as_millis() as u64;
        let due_keys: Vec<u64> = self
            .reminders
            .range(..=now_key)
            .map(|(k, _)| *k)
            .collect();

        for key in due_keys {
            if let Some(reminders) = self.reminders.remove(&key) {
                for reminder in reminders {
                    events.push(SchedulerEvent::ReminderFired(reminder));
                }
            }
        }

        // Fire due cron jobs
        let cron_ids: Vec<String> = self.cron_jobs.keys().cloned().collect();
        for id in cron_ids {
            let should_fire = self
                .cron_jobs
                .get(&id)
                .and_then(|j| j.next_fire)
                .map_or(false, |t| now >= t);

            if should_fire {
                if let Some(job) = self.cron_jobs.get_mut(&id) {
                    events.push(SchedulerEvent::CronFired(job.clone()));
                    // Schedule next fire
                    if let Some(interval) = job.interval {
                        job.next_fire = Some(now + interval);
                    }
                }
            }
        }

        events
    }

    /// Get the next time any event will fire, if any.
    pub fn next_wake(&self) -> Option<Instant> {
        let next_reminder = self.reminders.iter().next().map(|(key, _)| {
            self.epoch + Duration::from_millis(*key)
        });

        let next_cron = self
            .cron_jobs
            .values()
            .filter_map(|j| j.next_fire)
            .min();

        match (next_reminder, next_cron) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Get the number of pending reminders.
    pub fn reminder_count(&self) -> usize {
        self.reminders.values().map(|v| v.len()).sum()
    }

    /// Get the number of cron jobs.
    pub fn cron_count(&self) -> usize {
        self.cron_jobs.len()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a simple interval expression.
/// Supports: "10s", "5m", "1h", "*/5" (interpreted as every 5 minutes).
fn parse_interval(expr: &str) -> Result<Duration> {
    let expr = expr.trim();

    if let Some(secs) = expr.strip_suffix('s') {
        let n: u64 = secs
            .parse()
            .map_err(|_| SchedulerError::InvalidExpression(expr.to_string()))?;
        return Ok(Duration::from_secs(n));
    }

    if let Some(mins) = expr.strip_suffix('m') {
        let n: u64 = mins
            .parse()
            .map_err(|_| SchedulerError::InvalidExpression(expr.to_string()))?;
        return Ok(Duration::from_secs(n * 60));
    }

    if let Some(hours) = expr.strip_suffix('h') {
        let n: u64 = hours
            .parse()
            .map_err(|_| SchedulerError::InvalidExpression(expr.to_string()))?;
        return Ok(Duration::from_secs(n * 3600));
    }

    if let Some(interval) = expr.strip_prefix("*/") {
        let n: u64 = interval
            .parse()
            .map_err(|_| SchedulerError::InvalidExpression(expr.to_string()))?;
        return Ok(Duration::from_secs(n * 60));
    }

    Err(SchedulerError::InvalidExpression(expr.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> Instant {
        Instant::now()
    }

    #[test]
    fn test_add_reminder() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        let id = sched.add_reminder("test".to_string(), e + Duration::from_secs(10));
        assert_eq!(id, "reminder-1");
        assert_eq!(sched.reminder_count(), 1);
    }

    #[test]
    fn test_reminder_fires_at_correct_time() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched.add_reminder("hello".to_string(), e + Duration::from_secs(5));

        // Not yet
        let events = sched.tick(e + Duration::from_secs(3));
        assert!(events.is_empty());

        // Now
        let events = sched.tick(e + Duration::from_secs(5));
        assert_eq!(events.len(), 1);
        match &events[0] {
            SchedulerEvent::ReminderFired(r) => assert_eq!(r.message, "hello"),
            _ => panic!("expected ReminderFired"),
        }
    }

    #[test]
    fn test_reminder_not_premature() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched.add_reminder("future".to_string(), e + Duration::from_secs(100));
        let events = sched.tick(e + Duration::from_secs(50));
        assert!(events.is_empty());
        assert_eq!(sched.reminder_count(), 1);
    }

    #[test]
    fn test_reminder_consumed_after_fire() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched.add_reminder("once".to_string(), e + Duration::from_secs(1));
        sched.tick(e + Duration::from_secs(2));
        assert_eq!(sched.reminder_count(), 0);
        // Second tick should not fire again
        let events = sched.tick(e + Duration::from_secs(3));
        assert!(events.is_empty());
    }

    #[test]
    fn test_multiple_reminders_ordered() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched.add_reminder("second".to_string(), e + Duration::from_secs(20));
        sched.add_reminder("first".to_string(), e + Duration::from_secs(10));
        sched.add_reminder("third".to_string(), e + Duration::from_secs(30));

        let events = sched.tick(e + Duration::from_secs(25));
        assert_eq!(events.len(), 2);
        match &events[0] {
            SchedulerEvent::ReminderFired(r) => assert_eq!(r.message, "first"),
            _ => panic!("expected first"),
        }
        match &events[1] {
            SchedulerEvent::ReminderFired(r) => assert_eq!(r.message, "second"),
            _ => panic!("expected second"),
        }
    }

    #[test]
    fn test_add_cron_valid() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        let id = sched
            .add_cron("10s".to_string(), "check".to_string(), None, e)
            .unwrap();
        assert_eq!(id, "cron-1");
        assert_eq!(sched.cron_count(), 1);
    }

    #[test]
    fn test_add_cron_invalid() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        let result = sched.add_cron("invalid!!!".to_string(), "x".to_string(), None, e);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_cron() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        let id = sched
            .add_cron("10s".to_string(), "x".to_string(), None, e)
            .unwrap();
        sched.remove_cron(&id).unwrap();
        assert_eq!(sched.cron_count(), 0);
    }

    #[test]
    fn test_remove_cron_not_found() {
        let mut sched = Scheduler::new();
        assert!(sched.remove_cron("nope").is_err());
    }

    #[test]
    fn test_list_cron() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched.add_cron("10s".to_string(), "a".to_string(), None, e).unwrap();
        sched.add_cron("20s".to_string(), "b".to_string(), None, e).unwrap();
        assert_eq!(sched.list_cron().len(), 2);
    }

    #[test]
    fn test_cron_fires_on_schedule() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched
            .add_cron("10s".to_string(), "ping".to_string(), None, e)
            .unwrap();

        // Not yet
        let events = sched.tick(e + Duration::from_secs(5));
        assert!(events.is_empty());

        // Fire
        let events = sched.tick(e + Duration::from_secs(10));
        assert_eq!(events.len(), 1);
        match &events[0] {
            SchedulerEvent::CronFired(j) => assert_eq!(j.command, "ping"),
            _ => panic!("expected CronFired"),
        }
    }

    #[test]
    fn test_cron_recurs() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched
            .add_cron("5s".to_string(), "tick".to_string(), None, e)
            .unwrap();

        // First fire
        let events = sched.tick(e + Duration::from_secs(5));
        assert_eq!(events.len(), 1);

        // Should not fire at +7
        let events = sched.tick(e + Duration::from_secs(7));
        assert!(events.is_empty());

        // Should fire again at +10
        let events = sched.tick(e + Duration::from_secs(10));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_next_wake_empty() {
        let sched = Scheduler::new();
        assert!(sched.next_wake().is_none());
    }

    #[test]
    fn test_next_wake_reminder() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched.add_reminder("a".to_string(), e + Duration::from_secs(10));
        sched.add_reminder("b".to_string(), e + Duration::from_secs(5));
        let wake = sched.next_wake().unwrap();
        // Should be the earlier one
        assert!(wake <= e + Duration::from_secs(5) + Duration::from_millis(1));
    }

    #[test]
    fn test_next_wake_cron() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched
            .add_cron("10s".to_string(), "x".to_string(), None, e)
            .unwrap();
        let wake = sched.next_wake().unwrap();
        assert!(wake <= e + Duration::from_secs(10) + Duration::from_millis(1));
    }

    #[test]
    fn test_parse_interval_seconds() {
        assert_eq!(parse_interval("10s").unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn test_parse_interval_minutes() {
        assert_eq!(parse_interval("5m").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn test_parse_interval_hours() {
        assert_eq!(parse_interval("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_interval_cron_style() {
        assert_eq!(parse_interval("*/5").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn test_parse_interval_invalid() {
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("").is_err());
    }

    #[test]
    fn test_scheduler_default() {
        let sched = Scheduler::default();
        assert_eq!(sched.reminder_count(), 0);
        assert_eq!(sched.cron_count(), 0);
    }
}
