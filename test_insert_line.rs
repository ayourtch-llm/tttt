// Minimal test - just the escape sequence, no delays
use std::io::{self, Write};

fn main() {
    let mut out = io::stdout();
    
    // Full sequence in one go
    let seq = "\x1b[2J\x1b[H"  // Clear
        .to_string()
        + (1..=20).map(|i| format!("INIT-{:02}\n", i)).collect::<String>().as_str()
        + (1..=5).map(|i| format!("\x1b[s\x1b[5A\x1b[LINS-{}\n\x1b[u", i)).collect::<String>().as_str()
        + "\n===FINAL-STATE===";
    
    print!("{}", seq);
    out.flush().unwrap();
}