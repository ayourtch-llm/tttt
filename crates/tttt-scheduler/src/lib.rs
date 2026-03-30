mod error;

pub use error::{SchedulerError, Result};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// What to do when a cron/reminder fires but the target session is busy (user typing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BusyPolicy {
    /// Drop the message silently.
    #[default]
    Drop,
    /// Wait until the session is idle, then inject.
    Wait,
}

impl BusyPolicy {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s {
            Some("wait") => BusyPolicy::Wait,
            _ => BusyPolicy::Drop,
        }
    }
}

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
    pub if_busy: BusyPolicy,
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
        if_busy: BusyPolicy,
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
            if_busy,
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

/// Parse an interval expression.
///
/// Supports multiple formats:
/// - Compact: "10s", "5m", "1h"
/// - Cron-style: "*/5 * * * *" (every 5 minutes), "*/1 * * * *" (every minute)
/// - Shorthand cron: "*/5" (every 5 minutes)
/// - Human: "every 30 seconds", "every 2 minutes", "every 1 hour"
fn parse_interval(expr: &str) -> Result<Duration> {
    let expr = expr.trim();

    // Compact: "10s", "5m", "1h"
    if let Some(secs) = expr.strip_suffix('s') {
        if let Ok(n) = secs.parse::<u64>() {
            return Ok(Duration::from_secs(n));
        }
    }

    if let Some(mins) = expr.strip_suffix('m') {
        if let Ok(n) = mins.parse::<u64>() {
            return Ok(Duration::from_secs(n * 60));
        }
    }

    if let Some(hours) = expr.strip_suffix('h') {
        if let Ok(n) = hours.parse::<u64>() {
            return Ok(Duration::from_secs(n * 3600));
        }
    }

    // Standard 5-field cron: only "*/N" in the first (minute) field is supported.
    // Matches "*/1 * * * *", "*/5 * * * *", etc.
    {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() == 5 && fields[1..] == ["*", "*", "*", "*"] {
            if let Some(n_str) = fields[0].strip_prefix("*/") {
                if let Ok(n) = n_str.parse::<u64>() {
                    if n > 0 {
                        return Ok(Duration::from_secs(n * 60));
                    }
                }
            }
        }
    }

    // Shorthand cron: "*/5" (every 5 minutes)
    if let Some(interval) = expr.strip_prefix("*/") {
        if let Ok(n) = interval.parse::<u64>() {
            if n > 0 {
                return Ok(Duration::from_secs(n * 60));
            }
        }
    }

    // Human: "every N second(s)/minute(s)/hour(s)"
    if let Some(rest) = expr.strip_prefix("every ").or_else(|| expr.strip_prefix("every\t")) {
        let rest = rest.trim();
        // Split into number and unit
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        if parts.len() == 2 {
            if let Ok(n) = parts[0].parse::<u64>() {
                let unit = parts[1].trim().to_lowercase();
                match unit.as_str() {
                    "second" | "seconds" | "sec" | "secs" | "s" => {
                        return Ok(Duration::from_secs(n));
                    }
                    "minute" | "minutes" | "min" | "mins" | "m" => {
                        return Ok(Duration::from_secs(n * 60));
                    }
                    "hour" | "hours" | "hr" | "hrs" | "h" => {
                        return Ok(Duration::from_secs(n * 3600));
                    }
                    _ => {}
                }
            }
        }
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
            .add_cron("10s".to_string(), "check".to_string(), None, BusyPolicy::Drop, e)
            .unwrap();
        assert_eq!(id, "cron-1");
        assert_eq!(sched.cron_count(), 1);
    }

    #[test]
    fn test_add_cron_invalid() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        let result = sched.add_cron("invalid!!!".to_string(), "x".to_string(), None, BusyPolicy::Drop, e);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_cron() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        let id = sched
            .add_cron("10s".to_string(), "x".to_string(), None, BusyPolicy::Drop, e)
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
        sched.add_cron("10s".to_string(), "a".to_string(), None, BusyPolicy::Drop, e).unwrap();
        sched.add_cron("20s".to_string(), "b".to_string(), None, BusyPolicy::Drop, e).unwrap();
        assert_eq!(sched.list_cron().len(), 2);
    }

    #[test]
    fn test_cron_fires_on_schedule() {
        let e = epoch();
        let mut sched = Scheduler::with_epoch(e);
        sched
            .add_cron("10s".to_string(), "ping".to_string(), None, BusyPolicy::Drop, e)
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
            .add_cron("5s".to_string(), "tick".to_string(), None, BusyPolicy::Drop, e)
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
            .add_cron("10s".to_string(), "x".to_string(), None, BusyPolicy::Drop, e)
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
    fn test_parse_interval_cron_shorthand() {
        assert_eq!(parse_interval("*/5").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn test_parse_interval_cron_5_field() {
        // "*/1 * * * *" = every 1 minute
        assert_eq!(parse_interval("*/1 * * * *").unwrap(), Duration::from_secs(60));
        // "*/5 * * * *" = every 5 minutes
        assert_eq!(parse_interval("*/5 * * * *").unwrap(), Duration::from_secs(300));
        // "*/30 * * * *" = every 30 minutes
        assert_eq!(parse_interval("*/30 * * * *").unwrap(), Duration::from_secs(1800));
    }

    #[test]
    fn test_parse_interval_human_seconds() {
        assert_eq!(parse_interval("every 30 seconds").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_interval("every 1 second").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_interval("every 10 secs").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_interval("every 5 s").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn test_parse_interval_human_minutes() {
        assert_eq!(parse_interval("every 1 minute").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_interval("every 2 minutes").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_interval("every 5 min").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn test_parse_interval_human_hours() {
        assert_eq!(parse_interval("every 1 hour").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_interval("every 2 hours").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_interval_invalid() {
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("").is_err());
        // Unsupported cron patterns
        assert!(parse_interval("0 12 * * *").is_err());
    }

    #[test]
    fn test_scheduler_default() {
        let sched = Scheduler::default();
        assert_eq!(sched.reminder_count(), 0);
        assert_eq!(sched.cron_count(), 0);
    }
}
