use crate::backend::PtyBackend;
use crate::error::{PtyError, Result};
use crate::session::{PtySession, SessionId, SessionMetadata, SessionStatus};
use std::collections::HashMap;

const DEFAULT_MAX_SESSIONS: usize = 15;

/// Manages multiple PTY sessions.
pub struct SessionManager<B: PtyBackend> {
    sessions: HashMap<SessionId, PtySession<B>>,
    names: HashMap<String, SessionId>,
    max_sessions: usize,
    next_id: u64,
}

impl<B: PtyBackend> SessionManager<B> {
    /// Create a new session manager with default limits.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            names: HashMap::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            next_id: 1,
        }
    }

    /// Create a new session manager with a custom session limit.
    pub fn with_max_sessions(max: usize) -> Self {
        Self {
            sessions: HashMap::new(),
            names: HashMap::new(),
            max_sessions: max,
            next_id: 1,
        }
    }

    /// Generate a unique session ID.
    pub fn generate_id(&mut self) -> SessionId {
        let id = format!("pty-{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Add a session that was created externally (useful with MockPty).
    pub fn add_session(&mut self, session: PtySession<B>) -> Result<SessionId> {
        if self.sessions.len() >= self.max_sessions {
            return Err(PtyError::MaxSessionsReached(self.max_sessions));
        }
        let id = session.id.clone();
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// Add a session with an optional unique name. Errors if name is already taken.
    pub fn add_session_with_name(&mut self, mut session: PtySession<B>, name: String) -> Result<SessionId> {
        if self.names.contains_key(&name) {
            return Err(PtyError::DuplicateName(name));
        }
        if self.sessions.len() >= self.max_sessions {
            return Err(PtyError::MaxSessionsReached(self.max_sessions));
        }
        let id = session.id.clone();
        session.set_name(name.clone());
        self.names.insert(name, id.clone());
        self.sessions.insert(id.clone(), session);
        Ok(id)
    }

    /// Resolve a session ID or name to the actual session ID.
    pub fn resolve_id<'a>(&'a self, id_or_name: &'a str) -> Result<&'a str> {
        if self.sessions.contains_key(id_or_name) {
            return Ok(id_or_name);
        }
        if let Some(session_id) = self.names.get(id_or_name) {
            return Ok(session_id.as_str());
        }
        Err(PtyError::SessionNotFound(id_or_name.to_string()))
    }

    /// Get a reference to a session by ID or name.
    pub fn get(&self, id: &str) -> Result<&PtySession<B>> {
        if let Some(session) = self.sessions.get(id) {
            return Ok(session);
        }
        if let Some(session_id) = self.names.get(id) {
            return self.sessions.get(session_id.as_str())
                .ok_or_else(|| PtyError::SessionNotFound(id.to_string()));
        }
        Err(PtyError::SessionNotFound(id.to_string()))
    }

    /// Get a mutable reference to a session by ID or name.
    pub fn get_mut(&mut self, id: &str) -> Result<&mut PtySession<B>> {
        if self.sessions.contains_key(id) {
            return self.sessions.get_mut(id)
                .ok_or_else(|| PtyError::SessionNotFound(id.to_string()));
        }
        if let Some(session_id) = self.names.get(id).cloned() {
            return self.sessions.get_mut(&session_id)
                .ok_or_else(|| PtyError::SessionNotFound(id.to_string()));
        }
        Err(PtyError::SessionNotFound(id.to_string()))
    }

    /// Kill and remove a session by ID or name.
    pub fn kill_session(&mut self, id: &str) -> Result<()> {
        let session_id = if self.sessions.contains_key(id) {
            id.to_string()
        } else if let Some(sid) = self.names.get(id).cloned() {
            sid
        } else {
            return Err(PtyError::SessionNotFound(id.to_string()));
        };
        let session = self.sessions
            .get_mut(&session_id)
            .ok_or_else(|| PtyError::SessionNotFound(id.to_string()))?;
        if *session.status() == SessionStatus::Running {
            session.kill()?;
        }
        self.sessions.remove(&session_id);
        self.names.retain(|_, v| v != &session_id);
        Ok(())
    }

    /// List metadata for all sessions, sorted by session ID (natural sort).
    pub fn list(&self) -> Vec<SessionMetadata> {
        let mut sessions: Vec<SessionMetadata> = self.sessions
            .values()
            .map(|s| s.metadata())
            .collect();
        // Natural sort: extract numeric part of "pty-N" for proper ordering
        sessions.sort_by(|a, b| {
            let num_a = a.id.strip_prefix("pty-")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let num_b = b.id.strip_prefix("pty-")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            num_a.cmp(&num_b)
        });
        sessions
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Check if a session exists by ID or name.
    pub fn exists(&self, id: &str) -> bool {
        self.sessions.contains_key(id) || self.names.contains_key(id)
    }

    /// Pump all sessions (read PTY output into screen buffers).
    pub fn pump_all(&mut self) -> Result<()> {
        for session in self.sessions.values_mut() {
            session.pump()?;
        }
        Ok(())
    }

    /// Get the maximum number of sessions.
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// Access the next ID counter (for testing).
    pub fn next_id(&self) -> u64 {
        self.next_id
    }
}

impl<B: PtyBackend> Default for SessionManager<B> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockPty;

    fn make_session(id: &str) -> PtySession<MockPty> {
        let mock = MockPty::new(80, 24);
        PtySession::new(id.to_string(), mock, "bash".to_string(), 80, 24)
    }

    #[test]
    fn test_manager_new() {
        let mgr: SessionManager<MockPty> = SessionManager::new();
        assert_eq!(mgr.session_count(), 0);
        assert_eq!(mgr.max_sessions(), DEFAULT_MAX_SESSIONS);
    }

    #[test]
    fn test_manager_with_max_sessions() {
        let mgr: SessionManager<MockPty> = SessionManager::with_max_sessions(5);
        assert_eq!(mgr.max_sessions(), 5);
    }

    #[test]
    fn test_manager_add_session() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let session = make_session("s1");
        let id = mgr.add_session(session).unwrap();
        assert_eq!(id, "s1");
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_manager_get_session() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        mgr.add_session(make_session("s1")).unwrap();
        let session = mgr.get("s1").unwrap();
        assert_eq!(session.id, "s1");
    }

    #[test]
    fn test_manager_get_nonexistent() {
        let mgr: SessionManager<MockPty> = SessionManager::new();
        let result = mgr.get("nonexistent");
        assert!(result.is_err());
        match result {
            Err(PtyError::SessionNotFound(id)) => assert_eq!(id, "nonexistent"),
            _ => panic!("expected SessionNotFound"),
        }
    }

    #[test]
    fn test_manager_get_mut() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        mgr.add_session(make_session("s1")).unwrap();
        let session = mgr.get_mut("s1").unwrap();
        session.send_keys("hello").unwrap();
    }

    #[test]
    fn test_manager_kill_session() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        mgr.add_session(make_session("s1")).unwrap();
        mgr.kill_session("s1").unwrap();
        assert_eq!(mgr.session_count(), 0);
        assert!(!mgr.exists("s1"));
    }

    #[test]
    fn test_manager_kill_nonexistent() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let result = mgr.kill_session("nope");
        assert!(result.is_err());
    }

    #[test]
    fn test_manager_max_sessions_limit() {
        let mut mgr: SessionManager<MockPty> = SessionManager::with_max_sessions(2);
        mgr.add_session(make_session("s1")).unwrap();
        mgr.add_session(make_session("s2")).unwrap();
        let result = mgr.add_session(make_session("s3"));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PtyError::MaxSessionsReached(2)));
    }

    #[test]
    fn test_manager_list() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        mgr.add_session(make_session("s1")).unwrap();
        mgr.add_session(make_session("s2")).unwrap();
        let list = mgr.list();
        assert_eq!(list.len(), 2);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"s1"));
        assert!(ids.contains(&"s2"));
    }

    #[test]
    fn test_manager_list_sorted() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        // Add sessions in non-sequential order
        mgr.add_session(make_session("pty-2")).unwrap();
        mgr.add_session(make_session("pty-1")).unwrap();
        mgr.add_session(make_session("pty-10")).unwrap();
        mgr.add_session(make_session("pty-3")).unwrap();
        let list = mgr.list();
        assert_eq!(list.len(), 4);
        // Should be naturally sorted: pty-1, pty-2, pty-3, pty-10
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["pty-1", "pty-2", "pty-3", "pty-10"]);
    }

    #[test]
    fn test_manager_exists() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        mgr.add_session(make_session("s1")).unwrap();
        assert!(mgr.exists("s1"));
        assert!(!mgr.exists("s2"));
    }

    #[test]
    fn test_manager_pump_all() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let mut mock = MockPty::new(80, 24);
        mock.queue_output(b"output1");
        let session = PtySession::new("s1".to_string(), mock, "bash".to_string(), 80, 24);
        mgr.add_session(session).unwrap();
        mgr.pump_all().unwrap();
        assert!(mgr.get("s1").unwrap().get_screen().contains("output1"));
    }

    #[test]
    fn test_manager_generate_id() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id1 = mgr.generate_id();
        let id2 = mgr.generate_id();
        assert_eq!(id1, "pty-1");
        assert_eq!(id2, "pty-2");
    }

    #[test]
    fn test_manager_default() {
        let mgr: SessionManager<MockPty> = SessionManager::default();
        assert_eq!(mgr.session_count(), 0);
        assert_eq!(mgr.max_sessions(), DEFAULT_MAX_SESSIONS);
    }

    #[test]
    fn test_add_session_with_name() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id = mgr.generate_id();
        let session = make_session(&id);
        let returned_id = mgr.add_session_with_name(session, "myname".to_string()).unwrap();
        assert_eq!(returned_id, id);
        // Can get by name
        let s = mgr.get("myname").unwrap();
        assert_eq!(s.id, id);
    }

    #[test]
    fn test_add_session_duplicate_name() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id1 = mgr.generate_id();
        let id2 = mgr.generate_id();
        let s1 = make_session(&id1);
        let s2 = make_session(&id2);
        mgr.add_session_with_name(s1, "myname".to_string()).unwrap();
        let result = mgr.add_session_with_name(s2, "myname".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_id_by_name() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id = mgr.generate_id();
        let session = make_session(&id);
        mgr.add_session_with_name(session, "myname".to_string()).unwrap();
        let resolved = mgr.resolve_id("myname").unwrap();
        assert_eq!(resolved, id);
    }

    #[test]
    fn test_resolve_id_by_id() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id = mgr.generate_id();
        let session = make_session(&id);
        mgr.add_session(session).unwrap();
        let resolved = mgr.resolve_id(&id).unwrap();
        assert_eq!(resolved, id);
    }

    #[test]
    fn test_resolve_id_unknown() {
        let mgr: SessionManager<MockPty> = SessionManager::new();
        let result = mgr.resolve_id("unknown");
        assert!(result.is_err());
    }

    #[test]
    fn test_kill_session_by_name() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id = mgr.generate_id();
        let session = make_session(&id);
        mgr.add_session_with_name(session, "myname".to_string()).unwrap();
        mgr.kill_session("myname").unwrap();
        assert_eq!(mgr.session_count(), 0);
        assert!(!mgr.exists("myname"));
        assert!(!mgr.exists(&id));
    }

    #[test]
    fn test_exists_by_name() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id = mgr.generate_id();
        let session = make_session(&id);
        mgr.add_session_with_name(session, "myname".to_string()).unwrap();
        assert!(mgr.exists("myname"));
        assert!(!mgr.exists("othername"));
    }

    #[test]
    fn test_list_includes_name() {
        let mut mgr: SessionManager<MockPty> = SessionManager::new();
        let id = mgr.generate_id();
        let session = make_session(&id);
        mgr.add_session_with_name(session, "myname".to_string()).unwrap();
        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, Some("myname".to_string()));
    }
}
