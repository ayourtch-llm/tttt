// Test insert line - standalone version to see expected output
use std::io::{self, Write};

fn main() {
    let mut out = io::stdout();
    
    // Test 1: Simple insert at fixed position
    println!("=== Test 1: Simple Insert Line ===");
    print!("\x1b[2J\x1b[H");
    for i in 1..=10 {
        println!("Line-{}", i);
    }
    
    // Insert at line 5
    print!("\x1b[5;1H\x1b[L");  // Go to line 5, insert line
    println!("INSERTED");
    
    println!("\n=== Test 2: Multiple Inserts ===");
    print!("\x1b[2J\x1b[H");
    for i in 1..=10 {
        println!("Orig-{}", i);
    }
    
    // Insert 3 lines at position 5
    print!("\x1b[5;1H");
    print!("\x1b[L\x1b[L\x1b[L");  // Insert 3 lines
    println!("New1");
    println!("New2");  
    println!("New3");
    
    println!("\n=== Test 3: Cursor Save/Restore with Insert ===");
    print!("\x1b[2J\x1b[H");
    for i in 1..=10 {
        println!("Base-{}", i);
    }
    
    // Save, move up, insert, restore
    print!("\x1b[s");           // Save
    print!("\x1b[3A");          // Move up 3
    print!("\x1b[L");           // Insert
    println!("INSERTED-HERE");
    print!("\x1b[u");           // Restore
    
    println!("\nDone - scroll up to see results");
    out.flush().unwrap();
}