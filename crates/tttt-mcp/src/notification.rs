//! Notification system for watching session screens and injecting text.
//!
//! Enables "notify when executor returns to prompt" without polling.

use serde::{Deserialize, Serialize};

/// A registered notification watcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationWatcher {
    /// Unique watcher ID.
    pub id: String,
    /// Session to watch.
    pub watch_session_id: String,
    /// Regex pattern to match against screen contents.
    pub pattern: String,
    /// Compiled regex (not serialized).
    #[serde(skip)]
    pub compiled: Option<regex::Regex>,
    /// Text to inject when pattern matches.
    pub inject_text: String,
    /// Session to inject into (usually the root agent).
    pub inject_session_id: String,
    /// Whether this is a one-shot notification (removed after firing).
    pub one_shot: bool,
    /// Whether this watcher has fired.
    pub fired: bool,
    /// Snapshot of screen content at last check; None means first check (snapshot only).
    #[serde(skip)]
    pub last_screen: Option<String>,
}

/// Registry of notification watchers.
pub struct NotificationRegistry {
    watchers: Vec<NotificationWatcher>,
    next_id: u64,
}

/// Result of checking watchers against a session's screen.
#[derive(Debug)]
pub struct Injection {
    /// Session to inject into.
    pub target_session_id: String,
    /// Text to inject.
    pub text: String,
    /// Watcher ID that triggered this.
    pub watcher_id: String,
}

impl NotificationRegistry {
    pub fn new() -> Self {
        Self {
            watchers: Vec::new(),
            next_id: 1,
        }
    }

    /// Register a new watcher. Returns the watcher ID.
    pub fn add_watcher(
        &mut self,
        watch_session_id: String,
        pattern: &str,
        inject_text: String,
        inject_session_id: String,
        one_shot: bool,
    ) -> Result<String, String> {
        let compiled = regex::Regex::new(pattern)
            .map_err(|e| format!("invalid regex '{}': {}", pattern, e))?;
        let id = format!("notify-{}", self.next_id);
        self.next_id += 1;
        self.watchers.push(NotificationWatcher {
            id: id.clone(),
            watch_session_id,
            pattern: pattern.to_string(),
            compiled: Some(compiled),
            inject_text,
            inject_session_id,
            one_shot,
            fired: false,
            last_screen: None,
        });
        Ok(id)
    }

    /// Remove a watcher by ID.
    pub fn remove_watcher(&mut self, id: &str) -> bool {
        let len_before = self.watchers.len();
        self.watchers.retain(|w| w.id != id);
        self.watchers.len() < len_before
    }

    /// List all active watchers.
    pub fn list_watchers(&self) -> &[NotificationWatcher] {
        &self.watchers
    }

    /// Check a session's screen content against all watchers for that session.
    /// Returns a list of injections to perform.
    pub fn check_session(&mut self, session_id: &str, screen_content: &str) -> Vec<Injection> {
        let mut injections = Vec::new();

        for watcher in &mut self.watchers {
            if watcher.watch_session_id != session_id {
                continue;
            }
            if watcher.fired && watcher.one_shot {
                continue;
            }

            // On first check, take a snapshot and skip matching.
            let prev = match watcher.last_screen.take() {
                None => {
                    watcher.last_screen = Some(screen_content.to_string());
                    continue;
                }
                Some(p) => p,
            };

            // Find the new content since last check.
            let common_bytes = prev.as_bytes().iter()
                .zip(screen_content.as_bytes().iter())
                .take_while(|(a, b)| a == b)
                .count();
            let cut = (0..=common_bytes).rev()
                .find(|&i| screen_content.is_char_boundary(i))
                .unwrap_or(0);
            let diff = &screen_content[cut..];

            watcher.last_screen = Some(screen_content.to_string());

            if diff.is_empty() {
                continue;
            }

            let matches = watcher
                .compiled
                .as_ref()
                .map_or(false, |re| re.is_match(diff));

            if matches {
                injections.push(Injection {
                    target_session_id: watcher.inject_session_id.clone(),
                    text: watcher.inject_text.clone(),
                    watcher_id: watcher.id.clone(),
                });
                watcher.fired = true;
            }
        }

        // Remove fired one-shot watchers
        self.watchers
            .retain(|w| !(w.one_shot && w.fired));

        injections
    }

    /// Get the number of active watchers.
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }
}

impl Default for NotificationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_watcher() {
        let mut reg = NotificationRegistry::new();
        let id = reg
            .add_watcher(
                "pty-1".into(),
                r"❯\s*$",
                "[NOTIFICATION] Executor done".into(),
                "root".into(),
                true,
            )
            .unwrap();
        assert_eq!(id, "notify-1");
        assert_eq!(reg.watcher_count(), 1);
    }

    #[test]
    fn test_add_watcher_invalid_regex() {
        let mut reg = NotificationRegistry::new();
        let result = reg.add_watcher("pty-1".into(), "[invalid", "text".into(), "root".into(), true);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_watcher() {
        let mut reg = NotificationRegistry::new();
        let id = reg
            .add_watcher("pty-1".into(), "prompt", "text".into(), "root".into(), true)
            .unwrap();
        assert!(reg.remove_watcher(&id));
        assert_eq!(reg.watcher_count(), 0);
    }

    #[test]
    fn test_remove_nonexistent_watcher() {
        let mut reg = NotificationRegistry::new();
        assert!(!reg.remove_watcher("nope"));
    }

    #[test]
    fn test_check_session_first_call_snapshots() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "notify".into(), "root".into(), true)
            .unwrap();
        // First call: snapshot only, no match even if pattern is present
        let injections = reg.check_session("pty-1", "❯ ");
        assert!(injections.is_empty());
        assert_eq!(reg.watcher_count(), 1);
    }

    #[test]
    fn test_check_session_no_match() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "notify".into(), "root".into(), true)
            .unwrap();
        let _ = reg.check_session("pty-1", "Thinking..."); // snapshot
        let injections = reg.check_session("pty-1", "Thinking... still");
        assert!(injections.is_empty());
        assert_eq!(reg.watcher_count(), 1); // still active
    }

    #[test]
    fn test_check_session_match() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "[DONE]".into(), "root".into(), true)
            .unwrap();
        let _ = reg.check_session("pty-1", "output here\n"); // snapshot
        let injections = reg.check_session("pty-1", "output here\n❯ ");
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].text, "[DONE]");
        assert_eq!(injections[0].target_session_id, "root");
    }

    #[test]
    fn test_one_shot_removed_after_fire() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "notify".into(), "root".into(), true)
            .unwrap();
        let _ = reg.check_session("pty-1", ""); // snapshot
        let _ = reg.check_session("pty-1", "❯ "); // fires
        assert_eq!(reg.watcher_count(), 0); // removed after firing
    }

    #[test]
    fn test_recurring_watcher_not_removed() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "notify".into(), "root".into(), false)
            .unwrap();
        let _ = reg.check_session("pty-1", ""); // snapshot
        let injections = reg.check_session("pty-1", "❯ ");
        assert_eq!(injections.len(), 1);
        assert_eq!(reg.watcher_count(), 1); // still active (recurring)
    }

    #[test]
    fn test_recurring_fires_again_on_new_content() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "ping".into(), "root".into(), false)
            .unwrap();

        let _ = reg.check_session("pty-1", ""); // snapshot
        // New content with pattern — fires
        let inj1 = reg.check_session("pty-1", "❯ ");
        assert_eq!(inj1.len(), 1);

        // Same screen — no new content, should not fire again
        let inj2 = reg.check_session("pty-1", "❯ ");
        assert_eq!(inj2.len(), 0);

        // New content with pattern again — fires
        let inj3 = reg.check_session("pty-1", "❯ \nnew output\n❯ ");
        assert_eq!(inj3.len(), 1);
    }

    #[test]
    fn test_pattern_present_at_registration_does_not_fire_immediately() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "next slide", "advance".into(), "root".into(), true)
            .unwrap();
        // Pattern already on screen when registered — should not fire
        let inj = reg.check_session("pty-1", "All right\nNext slide\nOkay");
        assert!(inj.is_empty());
        assert_eq!(reg.watcher_count(), 1); // still waiting
    }

    #[test]
    fn test_check_wrong_session() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "notify".into(), "root".into(), true)
            .unwrap();
        let injections = reg.check_session("pty-2", "❯ "); // wrong session
        assert!(injections.is_empty());
        assert_eq!(reg.watcher_count(), 1); // unchanged
    }

    #[test]
    fn test_multiple_watchers() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "❯", "done1".into(), "root".into(), true)
            .unwrap();
        reg.add_watcher("pty-2".into(), "\\$", "done2".into(), "root".into(), true)
            .unwrap();
        reg.add_watcher("pty-1".into(), "error", "err".into(), "root".into(), true)
            .unwrap();

        let _ = reg.check_session("pty-1", "some output\n"); // snapshot
        let injections = reg.check_session("pty-1", "some output\n❯ ");
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].text, "done1");
        assert_eq!(reg.watcher_count(), 2); // one removed, two remain
    }

    #[test]
    fn test_list_watchers() {
        let mut reg = NotificationRegistry::new();
        reg.add_watcher("pty-1".into(), "a", "x".into(), "root".into(), true)
            .unwrap();
        reg.add_watcher("pty-2".into(), "b", "y".into(), "root".into(), false)
            .unwrap();
        let watchers = reg.list_watchers();
        assert_eq!(watchers.len(), 2);
        assert_eq!(watchers[0].watch_session_id, "pty-1");
        assert_eq!(watchers[1].watch_session_id, "pty-2");
    }

    #[test]
    fn test_regex_pattern_matching() {
        let mut reg = NotificationRegistry::new();
        // Match Claude Code style spinner
        reg.add_watcher(
            "pty-1".into(),
            "[⏺✻✶✳✽·✢]",
            "busy".into(),
            "root".into(),
            false,
        )
        .unwrap();

        let _ = reg.check_session("pty-1", ""); // snapshot
        let inj = reg.check_session("pty-1", "⏺ Thinking...");
        assert_eq!(inj.len(), 1);

        let inj = reg.check_session("pty-1", "normal output");
        assert!(inj.is_empty());
    }
}
