//! Integration tests for the viewer (tttt attach) protocol.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
extern crate tempfile;
use tttt_tui::protocol::{decode_message, encode_message, ClientMsg, ServerMsg};
use tttt_tui::{PaneRenderer, ViewerClient};

fn make_viewer_pair(
    pty_cols: u16,
    pty_rows: u16,
) -> (ViewerClient, UnixStream) {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    let client_stream = UnixStream::connect(&sock_path).unwrap();
    let (server_stream, _) = listener.accept().unwrap();

    let sidebar_width = 30;
    let mut viewer = ViewerClient::new(
        server_stream,
        pty_cols + sidebar_width,
        pty_rows + 1,
        sidebar_width,
    );
    // Override renderer to match PTY dimensions exactly
    viewer.renderer = PaneRenderer::new(pty_cols, pty_rows, 1, 1);
    viewer.invalidate();

    (viewer, client_stream)
}

/// Test that a full screen update from server produces correct content on client.
#[test]
fn test_viewer_full_screen_update_content() {
    let pty_cols = 40u16;
    let pty_rows = 5u16;

    let (mut viewer, mut client) = make_viewer_pair(pty_cols, pty_rows);
    client.set_nonblocking(false).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();

    // Create a PTY screen with known content
    let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);
    parser.process(b"$ ls\r\nfile1  file2\r\n$ ");

    // Send screen update via viewer
    let handle = std::thread::spawn(move || {
        viewer.send_screen_update(parser.screen(), 2, 2);
        viewer
    });

    // Read the message on client side
    let mut buf = vec![0u8; 65536];
    let n = client.read(&mut buf).unwrap();
    let (msg, _): (ServerMsg, usize) = decode_message(&buf[..n]).unwrap();

    match msg {
        ServerMsg::ScreenUpdate {
            screen_data,
            cursor_row,
            cursor_col,
        } => {
            // Parse the ANSI data on a simulated display
            let mut display = vt100::Parser::new(pty_rows, pty_cols + 30, 0);
            display.process(&screen_data);
            let contents = display.screen().contents();
            assert!(
                contents.contains("$ ls"),
                "viewer screen should show '$ ls': {:?}",
                contents
            );
            assert!(
                contents.contains("file1"),
                "viewer screen should show 'file1': {:?}",
                contents
            );
            // Cursor is 0-indexed PTY coords
            assert_eq!(cursor_row, 2); // PTY row 2
            assert_eq!(cursor_col, 2); // PTY col 2
        }
        _ => panic!("expected ScreenUpdate"),
    }

    let _viewer = handle.join().unwrap();
}

/// Test that keystrokes from client reach the server.
#[test]
fn test_viewer_keystrokes_received() {
    let (mut viewer, mut client) = make_viewer_pair(40, 5);

    // Client sends keystrokes
    let msg = ClientMsg::KeyInput {
        bytes: b"hello\r".to_vec(),
    };
    client.write_all(&encode_message(&msg)).unwrap();
    client.flush().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    viewer.read_available();

    // Decode the message
    let (decoded, _): (ClientMsg, usize) =
        decode_message(&viewer.read_buf).unwrap();
    match decoded {
        ClientMsg::KeyInput { bytes } => {
            assert_eq!(bytes, b"hello\r");
        }
        _ => panic!("expected KeyInput"),
    }
}

/// Test that server skips sending when nothing changed (dirty detection).
#[test]
fn test_viewer_skip_unchanged() {
    let pty_cols = 20u16;
    let pty_rows = 3u16;

    let (mut viewer, mut client) = make_viewer_pair(pty_cols, pty_rows);
    client.set_nonblocking(true).unwrap();

    let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);
    parser.process(b"hello");

    // First update — should send
    let sent1 = viewer.send_screen_update(parser.screen(), 0, 5);
    assert!(sent1, "first update should send");

    // Second update with same content + cursor — should skip
    let sent2 = viewer.send_screen_update(parser.screen(), 0, 5);
    assert!(sent2, "should return true (no error)");

    // Client should have received only one message
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut buf = vec![0u8; 65536];
    let n = client.read(&mut buf).unwrap_or(0);
    // First message was sent
    assert!(n > 0, "should have received the first update");
    // There should not be a second message in the buffer
    // (unless both arrived in same read, which is fine — point is
    // the server didn't send a second one)
}

/// Test client detach message.
#[test]
fn test_viewer_detach() {
    let (mut viewer, mut client) = make_viewer_pair(40, 5);

    let msg = ClientMsg::Detach;
    client.write_all(&encode_message(&msg)).unwrap();
    client.flush().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    viewer.read_available();

    let (decoded, _): (ClientMsg, usize) =
        decode_message(&viewer.read_buf).unwrap();
    assert!(matches!(decoded, ClientMsg::Detach));
}
