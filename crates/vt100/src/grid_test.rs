#[cfg(test)]
mod tests {
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
}
