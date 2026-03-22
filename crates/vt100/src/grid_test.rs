#[cfg(test)]
mod tests {
    #[test]
    fn test_simple_newline() {
        let mut parser = crate::Parser::new(24, 80, 0);
        
        // Clear screen
        parser.process(b"\x1b[2J\x1b[H");
        
        // Write a single line
        parser.process(b"INIT-01\n");
        
        eprintln!("Cursor after first line: {:?}", parser.screen().cursor_position());
        
        // Check row 0
        let mut row0 = String::new();
        for col in 0..80 {
            if let Some(cell) = parser.screen().cell(0, col) {
                row0.push_str(&cell.contents());
            }
        }
        eprintln!("Row 0: '{}'", row0);
        
        // Check row 1
        let mut row1 = String::new();
        for col in 0..80 {
            if let Some(cell) = parser.screen().cell(1, col) {
                row1.push_str(&cell.contents());
            }
        }
        eprintln!("Row 1: '{}'", row1);
    }
    
    #[test]
    fn test_insert_line_sequence() {
        // Reproduce the exact sequence from test_insert_line.rs:
        // 1. Clear screen
        // 2. Write 20 INIT lines
        // 3. Repeat 5 times: save cursor, move up 5, insert line, write INS-N, restore cursor
        // 4. Write final state
        
        let mut parser = crate::Parser::new(24, 80, 0);
        
        // Step 1: Clear screen
        parser.process(b"\x1b[2J\x1b[H");
        
        // Step 2: Write 20 INIT lines
        for i in 1..=20 {
            let line = format!("INIT-{:02}\n", i);
            parser.process(line.as_bytes());
        }
        
        // Debug: print what's in the screen after INIT lines
        eprintln!("After INIT lines, cursor at: {:?}", parser.screen().cursor_position());
        for row in 0..24 {
            let mut row_str = String::new();
            for col in 0..80 {
                if let Some(cell) = parser.screen().cell(row, col) {
                    row_str.push_str(&cell.contents());
                }
            }
            if !row_str.is_empty() {
                eprintln!("Row {}: '{}'", row, row_str);
            }
        }
        
        // Step 3: Insert lines sequence
        for i in 1..=5 {
            // Save cursor position
            parser.process(b"\x1b[s");
            // Move up 5 lines
            parser.process(b"\x1b[5A");
            // Insert line
            parser.process(b"\x1b[L");
            // Write INS-N
            let line = format!("INS-{}\n", i);
            parser.process(line.as_bytes());
            // Restore cursor position
            parser.process(b"\x1b[u");
        }
        
        // Debug: print what's in the screen after INS lines
        eprintln!("\nAfter INS lines, cursor at: {:?}", parser.screen().cursor_position());
        for row in 0..24 {
            let mut row_str = String::new();
            for col in 0..80 {
                if let Some(cell) = parser.screen().cell(row, col) {
                    row_str.push_str(&cell.contents());
                }
            }
            if !row_str.is_empty() {
                eprintln!("Row {}: '{}'", row, row_str);
            }
        }
        
        // Step 4: Write final state
        parser.process(b"\n===FINAL-STATE===");
        
        // Verify the final state line exists
        let (final_row, _) = parser.screen().cursor_position();
        eprintln!("\nAfter FINAL-STATE, cursor at: {:?}", parser.screen().cursor_position());
        let cell = parser.screen().cell(final_row, 0).unwrap();
        let contents = cell.contents();
        assert_eq!(contents, "=", "Final state should start with '='");
        
        // Verify INS lines exist somewhere in the screen
        let mut found_ins = vec![false; 6];
        for row in 0..24 {
            let mut row_str = String::new();
            for col in 0..80 {
                if let Some(cell) = parser.screen().cell(row, col) {
                    row_str.push_str(&cell.contents());
                }
            }
            // Check if this row contains INS-N
            for i in 1..=5 {
                if row_str.contains(&format!("INS-{}", i)) {
                    found_ins[i] = true;
                }
            }
        }
        
        // All INS lines should be visible
        for i in 1..=5 {
            assert!(found_ins[i], "INS-{} should be visible on screen", i);
        }
    }
}