//! Tests for bracketed paste mode detection in attach.rs
//!
//! These tests verify that the prefix key (0x1c, Ctrl+\) is correctly
//! ignored when inside a bracketed paste sequence.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PasteMode {
    None,
    StartEsc,
    StartBracket,
    Start2,
    Start0,
    InPaste,
    EndEsc,
    EndBracket,
    End2,
    End0,
    End1,
}

fn process_paste_bytes(bytes: &[u8], paste_mode: &mut PasteMode, contains_detach_key: &mut bool) {
    *contains_detach_key = false;
    for &byte in bytes {
        match *paste_mode {
            PasteMode::None => {
                if byte == 0x1b {
                    *paste_mode = PasteMode::StartEsc;
                } else if byte == 0x1c {
                    *contains_detach_key = true;
                }
            }
            PasteMode::StartEsc => {
                if byte == b'[' {
                    *paste_mode = PasteMode::StartBracket;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::StartBracket => {
                if byte == b'2' {
                    *paste_mode = PasteMode::Start2;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::Start2 => {
                if byte == b'0' {
                    *paste_mode = PasteMode::Start0;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::Start0 => {
                if byte == b'0' {
                    *paste_mode = PasteMode::InPaste;
                } else if byte == b'1' {
                    *paste_mode = PasteMode::End1;
                } else {
                    *paste_mode = PasteMode::None;
                }
            }
            PasteMode::InPaste => {
                if byte == 0x1b {
                    *paste_mode = PasteMode::EndEsc;
                }
            }
            PasteMode::EndEsc => {
                if byte == b'[' {
                    *paste_mode = PasteMode::EndBracket;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::EndBracket => {
                if byte == b'2' {
                    *paste_mode = PasteMode::End2;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::End2 => {
                if byte == b'0' {
                    *paste_mode = PasteMode::End0;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::End0 => {
                if byte == b'1' {
                    *paste_mode = PasteMode::End1;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
            PasteMode::End1 => {
                if byte == b'~' {
                    *paste_mode = PasteMode::None;
                } else {
                    *paste_mode = PasteMode::InPaste;
                }
            }
        }
    }
}

#[test]
fn test_prefix_key_triggers_detach() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);

    assert!(contains_detach_key, "Prefix key should trigger detach");
    assert_eq!(paste_mode, PasteMode::None);
}

#[test]
fn test_prefix_key_in_text_triggers_detach() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    let input = b"hello\x1cworld";
    process_paste_bytes(input, &mut paste_mode, &mut contains_detach_key);

    assert!(
        contains_detach_key,
        "Prefix key in text should trigger detach"
    );
    assert_eq!(paste_mode, PasteMode::None);
}

#[test]
fn test_prefix_key_in_bracketed_paste_ignored() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    // Bracketed paste start: \x1b[200~
    let paste_start = [0x1b, b'[', b'2', b'0', b'0', b'~'];
    process_paste_bytes(&paste_start, &mut paste_mode, &mut contains_detach_key);

    assert!(
        !contains_detach_key,
        "Paste start should not trigger detach"
    );
    assert_eq!(
        paste_mode,
        PasteMode::InPaste,
        "Should be in paste mode after start marker"
    );

    // Now send the prefix key - should NOT trigger detach
    contains_detach_key = false;
    process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);

    assert!(
        !contains_detach_key,
        "Prefix key in paste should NOT trigger detach"
    );

    // Bracketed paste end: \x1b[201~
    let paste_end = [0x1b, b'[', b'2', b'0', b'1', b'~'];
    process_paste_bytes(&paste_end, &mut paste_mode, &mut contains_detach_key);

    assert!(!contains_detach_key, "Paste end should not trigger detach");
    assert_eq!(paste_mode, PasteMode::None);
}

#[test]
fn test_prefix_key_after_bracketed_paste_triggers_detach() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    // Send bracketed paste start and end
    let paste_sequence = [
        0x1b, b'[', b'2', b'0', b'0', b'~', // start
        b't', b'e', b'x', b't', // content
        0x1b, b'[', b'2', b'0', b'1', b'~', // end
    ];
    process_paste_bytes(&paste_sequence, &mut paste_mode, &mut contains_detach_key);

    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::None);

    // Now send the prefix key - should trigger detach
    process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);

    assert!(
        contains_detach_key,
        "Prefix key after paste should trigger detach"
    );
}

#[test]
fn test_bracketed_paste_across_multiple_reads() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    // First read: \x1b[200
    process_paste_bytes(
        &[0x1b, b'[', b'2', b'0'],
        &mut paste_mode,
        &mut contains_detach_key,
    );
    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::Start0);

    // Second read: ~text\x1c
    process_paste_bytes(
        &[b'0', b'~', b't', b'e', b'x', b't', 0x1c],
        &mut paste_mode,
        &mut contains_detach_key,
    );
    assert!(
        !contains_detach_key,
        "Prefix key in paste should NOT trigger detach"
    );
    assert_eq!(paste_mode, PasteMode::InPaste);

    // Third read: \x1b[201~
    process_paste_bytes(
        &[0x1b, b'[', b'2', b'0', b'1', b'~'],
        &mut paste_mode,
        &mut contains_detach_key,
    );
    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::None);
}

#[test]
fn test_normal_input_no_paste() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    let input = b"hello world";
    process_paste_bytes(input, &mut paste_mode, &mut contains_detach_key);

    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::None);
}

#[test]
fn test_incomplete_escape_sequence() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    // Send just ESC
    process_paste_bytes(&[0x1b], &mut paste_mode, &mut contains_detach_key);
    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::StartEsc);

    // Send regular character, should reset
    process_paste_bytes(&[b'a'], &mut paste_mode, &mut contains_detach_key);
    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::None);

    // Now send prefix key, should trigger detach
    process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);
    assert!(contains_detach_key);
}

#[test]
fn test_incomplete_bracketed_paste() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    // Send \x1b[200
    process_paste_bytes(
        &[0x1b, b'[', b'2', b'0'],
        &mut paste_mode,
        &mut contains_detach_key,
    );
    assert_eq!(paste_mode, PasteMode::Start0);

    // Send something that completes the paste start
    process_paste_bytes(&[b'0'], &mut paste_mode, &mut contains_detach_key);
    assert_eq!(paste_mode, PasteMode::InPaste);

    // Send prefix key, should NOT trigger detach
    process_paste_bytes(&[0x1c], &mut paste_mode, &mut contains_detach_key);
    assert!(
        !contains_detach_key,
        "Prefix key in incomplete paste should NOT trigger detach"
    );

    // Send escape to start end sequence
    process_paste_bytes(
        &[0x1b, b'[', b'2', b'0', b'1'],
        &mut paste_mode,
        &mut contains_detach_key,
    );
    assert!(!contains_detach_key);
    assert_eq!(paste_mode, PasteMode::End1);

    // Finally complete with ~ to exit paste mode
    process_paste_bytes(&[b'~'], &mut paste_mode, &mut contains_detach_key);
    assert_eq!(paste_mode, PasteMode::None);
}

#[test]
fn test_real_world_paste_scenario() {
    let mut paste_mode = PasteMode::None;
    let mut contains_detach_key = false;

    // Simulate pasting: \x1b[200~echo "test\x1c"\x1b[201~
    // This contains the prefix key inside the paste
    let paste = [
        0x1b, b'[', b'2', b'0', b'0', b'~', // start
        b'e', b'c', b'h', b'o', b' ', b'"', b't', b'e', b's', b't', 0x1c,
        b'"', // content with prefix key
        0x1b, b'[', b'2', b'0', b'1', b'~', // end
    ];

    process_paste_bytes(&paste, &mut paste_mode, &mut contains_detach_key);

    assert!(
        !contains_detach_key,
        "Prefix key inside paste should NOT trigger detach"
    );
    assert_eq!(paste_mode, PasteMode::None);
}
