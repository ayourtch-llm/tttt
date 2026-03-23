// Ultra-simple test: just one insert line operation
use std::io::{self, Write};

fn main() {
    let mut out = io::stdout();
    
    print!("\x1b[2J\x1b[H");  // Clear screen
    
    // Print 5 lines
    println!("A");
    println!("B");
    println!("C");
    println!("D");
    println!("E");
    
    // Move to line 3 (where "C" is)
    print!("\x1b[3;1H");
    
    // Insert a line HERE - should push C, D, E down
    print!("\x1b[L");
    println!("INSERTED");
    
    // Show final state
    println!("\n---END---");
    out.flush().unwrap();
    
    // Expected output should be:
    // A
    // B
    // INSERTED   <- inserted at line 3
    // C          <- pushed down
    // D          <- pushed down
    // E          <- pushed down
    // ---END---
}