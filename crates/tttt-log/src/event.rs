use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Direction of terminal I/O.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Input,
    Output,
    Meta,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
            Direction::Meta => "meta",
        }
    }
}

/// A single terminal I/O event to be logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub timestamp_ms: u64,
    pub session_id: String,
    pub direction: Direction,
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

impl LogEvent {
    /// Create a new log event with the current timestamp.
    pub fn new(session_id: String, direction: Direction, data: Vec<u8>) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            timestamp_ms,
            session_id,
            direction,
            data,
        }
    }

    /// Create a log event with a specific timestamp (for testing).
    pub fn with_timestamp(
        timestamp_ms: u64,
        session_id: String,
        direction: Direction,
        data: Vec<u8>,
    ) -> Self {
        Self {
            timestamp_ms,
            session_id,
            direction,
            data,
        }
    }
}

/// Serde helper for encoding Vec<u8> as base64 in JSON.
mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    // Simple base64 without pulling in a dependency
    const CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn encode(data: &[u8]) -> String {
        let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let triple = (b0 << 16) | (b1 << 8) | b2;
            result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
            result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
            } else {
                result.push('=');
            }
            if chunk.len() > 2 {
                result.push(CHARS[(triple & 0x3F) as usize] as char);
            } else {
                result.push('=');
            }
        }
        result
    }

    fn decode(s: &str) -> Result<Vec<u8>, String> {
        fn val(c: u8) -> Result<u32, String> {
            match c {
                b'A'..=b'Z' => Ok((c - b'A') as u32),
                b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
                b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
                b'+' => Ok(62),
                b'/' => Ok(63),
                b'=' => Ok(0),
                _ => Err(format!("invalid base64 char: {}", c as char)),
            }
        }
        let bytes = s.as_bytes();
        let mut result = Vec::with_capacity(bytes.len() * 3 / 4);
        for chunk in bytes.chunks(4) {
            if chunk.len() < 4 {
                break;
            }
            let a = val(chunk[0])?;
            let b = val(chunk[1])?;
            let c = val(chunk[2])?;
            let d = val(chunk[3])?;
            let triple = (a << 18) | (b << 12) | (c << 6) | d;
            result.push(((triple >> 16) & 0xFF) as u8);
            if chunk[2] != b'=' {
                result.push(((triple >> 8) & 0xFF) as u8);
            }
            if chunk[3] != b'=' {
                result.push((triple & 0xFF) as u8);
            }
        }
        Ok(result)
    }

    pub fn serialize<S>(data: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode(data))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direction_as_str() {
        assert_eq!(Direction::Input.as_str(), "input");
        assert_eq!(Direction::Output.as_str(), "output");
        assert_eq!(Direction::Meta.as_str(), "meta");
    }

    #[test]
    fn test_log_event_new() {
        let event = LogEvent::new("s1".to_string(), Direction::Input, b"hello".to_vec());
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.direction, Direction::Input);
        assert_eq!(event.data, b"hello");
        assert!(event.timestamp_ms > 0);
    }

    #[test]
    fn test_log_event_with_timestamp() {
        let event = LogEvent::with_timestamp(12345, "s1".to_string(), Direction::Output, b"world".to_vec());
        assert_eq!(event.timestamp_ms, 12345);
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.direction, Direction::Output);
        assert_eq!(event.data, b"world");
    }

    #[test]
    fn test_log_event_json_roundtrip() {
        let event = LogEvent::with_timestamp(
            1000,
            "s1".to_string(),
            Direction::Input,
            b"hello\x1b[A".to_vec(),
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: LogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.timestamp_ms, event.timestamp_ms);
        assert_eq!(parsed.session_id, event.session_id);
        assert_eq!(parsed.direction, event.direction);
        assert_eq!(parsed.data, event.data);
    }

    #[test]
    fn test_log_event_binary_data_roundtrip() {
        let binary_data: Vec<u8> = (0..=255).collect();
        let event = LogEvent::with_timestamp(1000, "s1".to_string(), Direction::Output, binary_data.clone());
        let json = serde_json::to_string(&event).unwrap();
        let parsed: LogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.data, binary_data);
    }

    #[test]
    fn test_direction_serde() {
        let json = serde_json::to_string(&Direction::Input).unwrap();
        assert_eq!(json, "\"input\"");
        let parsed: Direction = serde_json::from_str("\"output\"").unwrap();
        assert_eq!(parsed, Direction::Output);
    }
}
