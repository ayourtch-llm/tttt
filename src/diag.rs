//! VT100 diagnostic tool: replays a recorded session and reports unhandled escape sequences.

use std::collections::HashMap;
use std::path::Path;

use tttt_log::{Direction, SqliteLogger};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Report data structure
// ---------------------------------------------------------------------------

/// Statistics for a single distinct unhandled sequence message.
#[derive(Debug, Clone)]
pub struct SequenceStat {
    /// The exact debug message emitted by the vt100 crate.
    pub message: String,
    /// Number of times this message was seen.
    pub count: usize,
    /// Byte offset into the output stream at first occurrence.
    pub first_offset: u64,
}

/// Complete diagnostic report for one session.
#[derive(Debug)]
pub struct DiagReport {
    pub session_id: String,
    pub events_processed: usize,
    pub bytes_processed: u64,
    /// Sequence stats ordered by first occurrence.
    pub sequences: Vec<SequenceStat>,
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Collect diagnostics without printing — useful for testing.
pub fn collect_report(db_path: &Path, session_id: &str) -> Result<DiagReport> {
    let db = SqliteLogger::open_read_only(db_path)
        .map_err(|e| format!("Failed to open database {}: {}", db_path.display(), e))?;

    let events = db.query_events(session_id)?;

    // Create a vt100 parser with diagnostic tracking enabled.
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.enable_diagnostic_tracking();

    let mut events_processed = 0usize;
    let mut byte_offset: u64 = 0;

    // Track (message -> (count, first_offset)) preserving insertion order.
    let mut map: HashMap<String, (usize, u64)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for event in &events {
        if event.direction == Direction::Output {
            let chunk_start = byte_offset;
            byte_offset += event.data.len() as u64;

            parser.process(&event.data);
            events_processed += 1;

            // Drain any unhandled sequences captured during this chunk.
            if let Some(msgs) = parser.take_unhandled() {
                for msg in msgs {
                    if let Some(entry) = map.get_mut(&msg) {
                        entry.0 += 1;
                    } else {
                        order.push(msg.clone());
                        // Use the start of this chunk as the byte offset for first occurrence.
                        map.insert(msg, (1, chunk_start));
                    }
                }
            }
        }
    }

    let mut sequences: Vec<SequenceStat> = order
        .into_iter()
        .map(|msg| {
            let (count, first_offset) = map[&msg];
            SequenceStat { message: msg, count, first_offset }
        })
        .collect();

    // Sort by first_offset for deterministic output.
    sequences.sort_by_key(|s| s.first_offset);

    Ok(DiagReport {
        session_id: session_id.to_string(),
        events_processed,
        bytes_processed: byte_offset,
        sequences,
    })
}

// ---------------------------------------------------------------------------
// Printer
// ---------------------------------------------------------------------------

fn print_report(report: &DiagReport) {
    println!("VT100 Diagnostic Report for session: {}", report.session_id);
    println!("Events processed: {}", report.events_processed);
    println!("Bytes processed:  {}", report.bytes_processed);
    println!();

    if report.sequences.is_empty() {
        println!("No unhandled sequences detected.");
        return;
    }

    println!("Unhandled sequences:");
    for seq in &report.sequences {
        println!(
            "  {:50}  \u{d7}{:<5}  (first at byte {})",
            seq.message, seq.count, seq.first_offset
        );
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the `diag` subcommand: open `db_path`, load events for `session_id`,
/// replay them through the vt100 parser, and print a report of unhandled
/// escape sequences.
pub fn run(db_path: &Path, session_id: &str) -> Result<()> {
    let report = collect_report(db_path, session_id)?;
    print_report(&report);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use tttt_log::{Direction, LogEvent, LogSink, SqliteLogger};

    fn make_output_event(session_id: &str, ts: u64, data: &[u8]) -> LogEvent {
        LogEvent::with_timestamp(ts, session_id.to_string(), Direction::Output, data.to_vec())
    }

    fn create_db_with_events(events: &[LogEvent]) -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = SqliteLogger::new(tmp.path()).unwrap();
        db.log_session_start("test-session", "bash", 80, 24, None).unwrap();
        for event in events {
            db.log_event(event).unwrap();
        }
        tmp
    }

    // -----------------------------------------------------------------------
    // Empty session
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_empty_session() {
        let tmp = create_db_with_events(&[]);
        let report = collect_report(tmp.path(), "test-session").unwrap();
        assert_eq!(report.session_id, "test-session");
        assert_eq!(report.events_processed, 0);
        assert_eq!(report.bytes_processed, 0);
        assert!(report.sequences.is_empty());
    }

    // -----------------------------------------------------------------------
    // Known-good sequences (no unhandled)
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_plain_text_no_unhandled() {
        let events = vec![
            make_output_event("test-session", 100, b"Hello, world!\r\n"),
            make_output_event("test-session", 200, b"\x1b[1mBOLD\x1b[0m"),
            make_output_event("test-session", 300, b"\x1b[2J\x1b[H"),
        ];
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();
        assert_eq!(report.events_processed, 3);
        assert!(report.bytes_processed > 0);
        // Plain text + known SGR + erase — none produce "unhandled" messages.
        assert!(
            report.sequences.is_empty(),
            "Expected no unhandled sequences, got: {:#?}",
            report.sequences
        );
    }

    // -----------------------------------------------------------------------
    // DECSET mode 2026 (synchronized output) — not handled by the vt100 crate
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_unhandled_decset_9999() {
        // CSI ? 9999 h  = DECSET mode 9999 (not a real mode, will be unhandled)
        let seq = b"\x1b[?9999h";
        let events = vec![
            make_output_event("test-session", 100, seq),
            make_output_event("test-session", 200, seq), // second occurrence
        ];
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();

        assert_eq!(report.events_processed, 2);

        // There should be at least one unhandled sequence entry mentioning 9999.
        let found = report
            .sequences
            .iter()
            .any(|s| s.message.contains("9999"));
        assert!(found, "Expected unhandled DECSET 9999 in report, got: {:#?}", report.sequences);
    }

    // -----------------------------------------------------------------------
    // Kitty keyboard protocol (CSI > 1 u) — not handled
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_csi_greater_1u_is_handled() {
        // CSI > 1 u  = Kitty keyboard enable
        // '>' (0x3E) is an intermediate byte — now handled by the intermediate guard
        let seq = b"\x1b[>1u";
        let events = vec![make_output_event("test-session", 100, seq)];
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();
        assert_eq!(report.events_processed, 1);
        // The intermediate byte guard now handles this, so it should be logged
        // but via the guard path. Check that it appears as an intermediate guard entry.
        // (The guard logs "CSI with intermediate byte(s)" which record_unhandled captures.)
        let found = report.sequences.iter().any(|s| s.message.contains("intermediate"));
        assert!(found, "Expected intermediate byte guard log entry, got: {:#?}", report.sequences);
    }

    // -----------------------------------------------------------------------
    // OSC 133 (shell integration) — not handled
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_unhandled_osc_133() {
        // OSC 133 ; A BEL
        let seq = b"\x1b]133;A\x07";
        let events = vec![
            make_output_event("test-session", 100, seq),
            make_output_event("test-session", 200, seq),
            make_output_event("test-session", 300, seq),
        ];
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();
        assert_eq!(report.events_processed, 3);

        // OSC 133 should appear as unhandled.
        let found = report
            .sequences
            .iter()
            .any(|s| s.message.contains("133") || s.message.contains("osc"));
        assert!(found, "Expected unhandled OSC 133 in report, got: {:#?}", report.sequences);
    }

    // -----------------------------------------------------------------------
    // Count deduplication: same sequence sent N times → count == N
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_count_deduplication() {
        // Send the same unhandled sequence 5 times.
        let seq = b"\x1b[?9999h";
        let events: Vec<LogEvent> = (0..5)
            .map(|i| make_output_event("test-session", i * 100, seq))
            .collect();
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();

        // Find the entry for mode 9999.
        let stat = report.sequences.iter().find(|s| s.message.contains("9999"));
        assert!(stat.is_some(), "Expected DECSET 9999 entry");
        let stat = stat.unwrap();
        assert_eq!(
            stat.count, 5,
            "Expected count of 5 for DECSET 9999, got {}",
            stat.count
        );
    }

    // -----------------------------------------------------------------------
    // Input/Meta events are ignored
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_input_events_ignored() {
        let events = vec![
            // Input event carrying what looks like an unhandled sequence — must NOT be processed.
            LogEvent::with_timestamp(
                100,
                "test-session".to_string(),
                Direction::Input,
                b"\x1b[?9999h".to_vec(),
            ),
        ];
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();
        // No output events → events_processed == 0.
        assert_eq!(report.events_processed, 0);
        assert_eq!(report.bytes_processed, 0);
        assert!(report.sequences.is_empty());
    }

    // -----------------------------------------------------------------------
    // first_offset is correctly recorded
    // -----------------------------------------------------------------------

    #[test]
    fn test_diag_first_offset() {
        // First event: 5 bytes of plain text, then second event: the unhandled sequence.
        let events = vec![
            make_output_event("test-session", 100, b"hello"),         // 5 bytes
            make_output_event("test-session", 200, b"\x1b[?9999h"),   // unhandled
        ];
        let tmp = create_db_with_events(&events);
        let report = collect_report(tmp.path(), "test-session").unwrap();

        let stat = report.sequences.iter().find(|s| s.message.contains("9999"));
        assert!(stat.is_some(), "Expected DECSET 9999 in report");
        let stat = stat.unwrap();
        // The unhandled sequence is in the second chunk starting at byte offset 5.
        assert_eq!(stat.first_offset, 5, "Expected first occurrence at byte 5");
    }
}
