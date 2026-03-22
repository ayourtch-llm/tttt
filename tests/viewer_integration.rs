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
            ansi_data,
            cursor_row,
            cursor_col,
        } => {
            // Parse the ANSI data on a simulated display
            let mut display = vt100::Parser::new(pty_rows, pty_cols + 30, 0);
            display.process(&ansi_data);
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
            // Cursor should be at the correct terminal position (1-indexed)
            assert_eq!(cursor_row, 3); // PTY row 2 + offset 1
            assert_eq!(cursor_col, 3); // PTY col 2 + offset 1
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

/// Test that multiple screen updates use dirty tracking (second is smaller).
#[test]
fn test_viewer_dirty_tracking() {
    let pty_cols = 20u16;
    let pty_rows = 3u16;

    let (mut viewer, mut client) = make_viewer_pair(pty_cols, pty_rows);
    client.set_nonblocking(false).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();

    let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);
    parser.process(b"first");

    // First update (full render)
    let handle = std::thread::spawn(move || {
        viewer.send_screen_update(parser.screen(), 0, 5);
        (viewer, parser)
    });

    let mut buf = vec![0u8; 65536];
    let n1 = client.read(&mut buf).unwrap();
    let (msg1, _): (ServerMsg, usize) = decode_message(&buf[..n1]).unwrap();
    let first_size = match &msg1 {
        ServerMsg::ScreenUpdate { ansi_data, .. } => ansi_data.len(),
        _ => panic!("expected ScreenUpdate"),
    };

    let (mut viewer, mut parser) = handle.join().unwrap();

    // Second update — just add " world"
    parser.process(b" world");
    let handle = std::thread::spawn(move || {
        viewer.send_screen_update(parser.screen(), 0, 11);
        viewer
    });

    let n2 = client.read(&mut buf).unwrap();
    let (msg2, _): (ServerMsg, usize) = decode_message(&buf[..n2]).unwrap();
    let second_size = match &msg2 {
        ServerMsg::ScreenUpdate { ansi_data, .. } => ansi_data.len(),
        _ => panic!("expected ScreenUpdate"),
    };

    // Second update should be smaller (only changed cells)
    assert!(
        second_size < first_size,
        "incremental update ({} bytes) should be smaller than full render ({} bytes)",
        second_size,
        first_size
    );

    let _viewer = handle.join().unwrap();
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
