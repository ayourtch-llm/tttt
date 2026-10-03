#[cfg(test)]
mod tests {
    #[test]
    fn resize_clamps_saved_cursor_before_restore_and_print() {
        for (save, restore) in [
            (b"\x1b7".as_slice(), b"\x1b8".as_slice()),
            (b"\x1b[s", b"\x1b[u"),
        ] {
            let mut parser = crate::Parser::new(24, 80, 0);
            parser.process(b"\x1b[24;80H");
            parser.process(save);
            parser.set_size(5, 10);
            parser.process(restore);
            parser.process(b"X");
            assert_eq!(parser.screen().cell(4, 9).unwrap().contents(), "X");
        }
    }

    #[test]
    fn resize_wide_boundary_can_erase_character() {
        let mut parser = crate::Parser::new(2, 4, 0);
        parser.process("ab界".as_bytes());
        parser.set_size(2, 3);
        parser.process(b"\x1b[1;3H\x1b[X");
        assert_eq!(parser.screen().contents(), "ab");
        assert!(!parser.screen().cell(0, 2).unwrap().is_wide());
    }

    #[test]
    fn resize_wide_boundary_can_delete_character() {
        let mut parser = crate::Parser::new(2, 4, 0);
        parser.process("ab界".as_bytes());
        parser.set_size(2, 3);
        parser.process(b"\x1b[1;3H\x1b[P");
        assert_eq!(parser.screen().contents(), "ab");
        assert!(!parser.screen().cell(0, 2).unwrap().is_wide());
    }

    #[test]
    fn resize_preserves_complete_wide_character() {
        let mut parser = crate::Parser::new(2, 5, 0);
        parser.process("ab界".as_bytes());
        parser.set_size(2, 4);
        assert_eq!(parser.screen().contents(), "ab界");
        assert!(parser.screen().cell(0, 2).unwrap().is_wide());
        assert!(parser.screen().cell(0, 3).unwrap().is_wide_continuation());
    }

    #[test]
    fn test_simple_newline() {
        let mut parser = crate::Parser::new(24, 80, 0);

        // Clear screen
        parser.process(b"\x1b[2J\x1b[H");

        // Write a single line
        parser.process(b"INIT-01\r\n");

        // Cursor should be at row 1, col 0
        let (row, col) = parser.screen().cursor_position();
        assert_eq!(row, 1);
        assert_eq!(col, 0);

        // Row 0 should have INIT-01
        let cell = parser.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "I");
    }

    #[test]
    fn test_insert_line_sequence() {
        // Test the insert line (IL) escape sequence:
        // 1. Clear screen, write 20 lines
        // 2. Save cursor, move up, insert line, write content, restore cursor
        // 3. Verify inserted lines appear and final state is correct

        let mut parser = crate::Parser::new(24, 80, 0);

        // Step 1: Clear screen
        parser.process(b"\x1b[2J\x1b[H");

        // Step 2: Write 20 INIT lines (using CR+LF for proper column reset)
        for i in 1..=20 {
            let line = format!("INIT-{:02}\r\n", i);
            parser.process(line.as_bytes());
        }

        // Cursor should be at row 20, col 0
        let (row, _col) = parser.screen().cursor_position();
        assert!(row <= 23, "cursor row should be within screen bounds: {}", row);

        // Step 3: Insert lines sequence
        // Each iteration: save cursor, move up 5, insert blank line, write INS-N, restore cursor
        for i in 1..=5 {
            parser.process(b"\x1b[s");      // save cursor
            parser.process(b"\x1b[5A");     // move up 5
            parser.process(b"\x1b[L");      // insert line (pushes content down)
            let line = format!("INS-{}\r\n", i);
            parser.process(line.as_bytes());
            parser.process(b"\x1b[u");      // restore cursor
        }

        // Step 4: Write final state
        parser.process(b"\r\n===FINAL-STATE===");

        // Verify the final state line exists somewhere on screen
        let screen = parser.screen().contents();
        assert!(
            screen.contains("===FINAL-STATE==="),
            "screen should contain FINAL-STATE: {:?}",
            screen
        );

        // Verify INS lines exist somewhere in the screen
        for i in 1..=5 {
            let pattern = format!("INS-{}", i);
            assert!(
                screen.contains(&pattern),
                "screen should contain {}: {:?}",
                pattern,
                screen
            );
        }
    }

    /// Reproduces panic: visible_rows() underflows when scrollback_offset > rows.len().
    /// This happens when scrollback has more lines than the screen height and the
    /// user scrolls back far enough that the offset exceeds the visible row count.
    #[test]
    fn test_scrollback_offset_exceeding_rows_does_not_panic() {
        // 5-row screen with 100-line scrollback capacity
        let mut parser = crate::Parser::new(5, 40, 100);

        // Generate enough output to fill scrollback: 30 lines on a 5-row screen
        for i in 0..30 {
            parser.process(format!("line {}\r\n", i).as_bytes());
        }

        // Scroll back further than the screen height (5 rows)
        // This should not panic even though offset > rows.len()
        parser.set_scrollback(10);
        // Accessing cells should work without panic
        let _cell = parser.screen().cell(0, 0);

        // Scroll to maximum available scrollback
        let max = parser.screen().scrollback_count();
        parser.set_scrollback(max);
        let _cell = parser.screen().cell(0, 0);
    }
}
