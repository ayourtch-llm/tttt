//! Integration tests using real PTY processes.

use tttt_pty::{PtySession, RealPty, SessionManager};

#[test]
fn test_real_pty_echo() {
    let backend = RealPty::spawn("/bin/echo", &["hello from pty"], 80, 24).unwrap();
    let mut session = PtySession::new("test-1".to_string(), backend, "echo".to_string(), 80, 24);

    // Give the process time to produce output
    std::thread::sleep(std::time::Duration::from_millis(200));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("hello from pty"),
        "screen should contain 'hello from pty', got: {:?}",
        screen
    );
}

#[test]
fn test_real_pty_exit_code() {
    let backend = RealPty::spawn("/usr/bin/true", &[], 80, 24).unwrap();
    let mut session = PtySession::new("test-2".to_string(), backend, "true".to_string(), 80, 24);

    std::thread::sleep(std::time::Duration::from_millis(200));
    session.pump().unwrap();

    assert_eq!(*session.status(), tttt_pty::SessionStatus::Exited(0));
}

#[test]
fn test_real_pty_false_exit_code() {
    let backend = RealPty::spawn("/usr/bin/false", &[], 80, 24).unwrap();
    let mut session =
        PtySession::new("test-3".to_string(), backend, "false".to_string(), 80, 24);

    std::thread::sleep(std::time::Duration::from_millis(200));
    session.pump().unwrap();

    assert_eq!(*session.status(), tttt_pty::SessionStatus::Exited(1));
}

#[test]
fn test_real_pty_resize() {
    let backend = RealPty::spawn("/bin/sleep", &["60"], 80, 24).unwrap();
    let mut session =
        PtySession::new("test-4".to_string(), backend, "sleep".to_string(), 80, 24);

    session.resize(120, 40).unwrap();
    let meta = session.metadata();
    assert_eq!(meta.cols, 120);
    assert_eq!(meta.rows, 40);

    session.kill().unwrap();
}

#[test]
fn test_real_pty_send_keys_and_read() {
    // Launch cat, which echoes back what it receives
    let backend = RealPty::spawn("/bin/cat", &[], 80, 24).unwrap();
    let mut session = PtySession::new("test-5".to_string(), backend, "cat".to_string(), 80, 24);

    session.send_keys("test input\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    session.pump().unwrap();

    let screen = session.get_screen();
    assert!(
        screen.contains("test input"),
        "screen should contain 'test input', got: {:?}",
        screen
    );

    session.kill().unwrap();
}

#[test]
fn test_real_pty_manager_lifecycle() {
    let mut manager: SessionManager<RealPty> = SessionManager::new();

    let backend = RealPty::spawn("/bin/echo", &["managed"], 80, 24).unwrap();
    let id = manager.generate_id();
    let session = PtySession::new(id.clone(), backend, "echo".to_string(), 80, 24);
    manager.add_session(session).unwrap();

    assert_eq!(manager.session_count(), 1);
    assert!(manager.exists(&id));

    std::thread::sleep(std::time::Duration::from_millis(200));
    manager.pump_all().unwrap();

    let screen = manager.get(&id).unwrap().get_screen();
    assert!(screen.contains("managed"));

    manager.kill_session(&id).unwrap();
    assert_eq!(manager.session_count(), 0);
}

#[test]
fn test_real_pty_special_keys() {
    // Launch cat and send ctrl-c to kill it
    let backend = RealPty::spawn("/bin/cat", &[], 80, 24).unwrap();
    let mut session = PtySession::new("test-6".to_string(), backend, "cat".to_string(), 80, 24);

    session.send_keys("^C").unwrap();
    // Give the signal time to be delivered and processed
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = session.pump();
        if matches!(*session.status(), tttt_pty::SessionStatus::Exited(_)) {
            break;
        }
    }

    // cat should have exited from SIGINT
    assert!(
        matches!(*session.status(), tttt_pty::SessionStatus::Exited(_)),
        "cat should have exited after ^C, status: {:?}",
        session.status()
    );
}
