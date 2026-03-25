//! Wire protocol for viewer connections (tttt attach).
//!
//! Messages are length-prefixed JSON: 4 bytes big-endian length, then JSON payload.

use serde::{Deserialize, Serialize};

/// Message from server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Screen content as vt100 contents_formatted() — a replayable
    /// representation that a vt100 parser can consume to reproduce
    /// the exact screen state including attributes and cursor.
    ScreenUpdate {
        /// vt100 contents_formatted() output.
        screen_data: Vec<u8>,
        /// Cursor position (0-indexed PTY coords).
        cursor_row: u16,
        cursor_col: u16,
    },

    /// Session list update for sidebar.
    SessionList {
        sessions: Vec<SessionInfo>,
        active_id: Option<String>,
    },

    /// Server is shutting down.
    Goodbye,

    /// Virtual window size update (PTY dimensions).
    WindowSize {
        /// PTY columns.
        cols: u16,
        /// PTY rows.
        rows: u16,
    },
}

/// Compact session info sent to viewers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub command: String,
    pub status: String,
}

/// Message from client to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Raw keystrokes from the client.
    KeyInput { bytes: Vec<u8> },

    /// Client wants to switch which session it views.
    SwitchSession { session_id: String },

    /// Client terminal was resized.
    Resize { cols: u16, rows: u16 },

    /// Client is disconnecting.
    Detach,
}

/// Encode a message as length-prefixed JSON.
pub fn encode_message<T: Serialize>(msg: &T) -> Vec<u8> {
    let json = serde_json::to_vec(msg).unwrap();
    let len = json.len() as u32;
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    buf
}

/// Decode a length-prefixed JSON message from a buffer.
/// Returns (message, bytes_consumed) or None if not enough data.
pub fn decode_message<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Option<(T, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return None;
    }
    let msg: T = serde_json::from_slice(&buf[4..4 + len]).ok()?;
    Some((msg, 4 + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_server_msg() {
        let msg = ServerMsg::ScreenUpdate {
            screen_data: b"hello".to_vec(),
            cursor_row: 1,
            cursor_col: 5,
        };
        let encoded = encode_message(&msg);
        let (decoded, consumed): (ServerMsg, usize) = decode_message(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        match decoded {
            ServerMsg::ScreenUpdate { screen_data, cursor_row, cursor_col } => {
                assert_eq!(screen_data, b"hello");
                assert_eq!(cursor_row, 1);
                assert_eq!(cursor_col, 5);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_encode_decode_client_msg() {
        let msg = ClientMsg::KeyInput { bytes: vec![0x1b, b'[', b'A'] };
        let encoded = encode_message(&msg);
        let (decoded, _): (ClientMsg, usize) = decode_message(&encoded).unwrap();
        match decoded {
            ClientMsg::KeyInput { bytes } => assert_eq!(bytes, vec![0x1b, b'[', b'A']),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_decode_incomplete() {
        let result: Option<(ServerMsg, usize)> = decode_message(&[0, 0, 0]);
        assert!(result.is_none());
    }

    #[test]
    fn test_decode_partial_payload() {
        let msg = ServerMsg::Goodbye;
        let encoded = encode_message(&msg);
        // Give only part of the payload
        let partial = &encoded[..encoded.len() - 2];
        let result: Option<(ServerMsg, usize)> = decode_message(partial);
        assert!(result.is_none());
    }

    #[test]
    fn test_session_list_roundtrip() {
        let msg = ServerMsg::SessionList {
            sessions: vec![
                SessionInfo { id: "pty-1".into(), command: "bash".into(), status: "running".into() },
                SessionInfo { id: "pty-2".into(), command: "python".into(), status: "exited(0)".into() },
            ],
            active_id: Some("pty-1".into()),
        };
        let encoded = encode_message(&msg);
        let (decoded, _): (ServerMsg, usize) = decode_message(&encoded).unwrap();
        match decoded {
            ServerMsg::SessionList { sessions, active_id } => {
                assert_eq!(sessions.len(), 2);
                assert_eq!(active_id, Some("pty-1".into()));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_multiple_messages_in_buffer() {
        let msg1 = ClientMsg::KeyInput { bytes: b"a".to_vec() };
        let msg2 = ClientMsg::KeyInput { bytes: b"b".to_vec() };
        let mut buf = encode_message(&msg1);
        buf.extend_from_slice(&encode_message(&msg2));

        let (decoded1, consumed1): (ClientMsg, usize) = decode_message(&buf).unwrap();
        let (decoded2, _): (ClientMsg, usize) = decode_message(&buf[consumed1..]).unwrap();

        match decoded1 {
            ClientMsg::KeyInput { bytes } => assert_eq!(bytes, b"a"),
            _ => panic!("expected KeyInput"),
        }
        match decoded2 {
            ClientMsg::KeyInput { bytes } => assert_eq!(bytes, b"b"),
            _ => panic!("expected KeyInput"),
        }
    }

    #[test]
    fn test_goodbye_roundtrip() {
        let msg = ServerMsg::Goodbye;
        let encoded = encode_message(&msg);
        let (decoded, _): (ServerMsg, usize) = decode_message(&encoded).unwrap();
        assert!(matches!(decoded, ServerMsg::Goodbye));
    }

    #[test]
    fn test_detach_roundtrip() {
        let msg = ClientMsg::Detach;
        let encoded = encode_message(&msg);
        let (decoded, _): (ClientMsg, usize) = decode_message(&encoded).unwrap();
        assert!(matches!(decoded, ClientMsg::Detach));
    }

    #[test]
    fn test_resize_roundtrip() {
        let msg = ClientMsg::Resize { cols: 120, rows: 40 };
        let encoded = encode_message(&msg);
        let (decoded, _): (ClientMsg, usize) = decode_message(&encoded).unwrap();
        match decoded {
            ClientMsg::Resize { cols, rows } => {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_window_size_roundtrip() {
        let msg = ServerMsg::WindowSize { cols: 80, rows: 24 };
        let encoded = encode_message(&msg);
        let (decoded, _): (ServerMsg, usize) = decode_message(&encoded).unwrap();
        match decoded {
            ServerMsg::WindowSize { cols, rows } => {
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            _ => panic!("wrong variant"),
        }
    }
}
