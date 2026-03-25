use std::process::Command;

fn main() {
    // Embed build timestamp as compile-time env var
    let output = Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", output);

    // Re-run if the binary itself changes (i.e., on every build)
    println!("cargo:rerun-if-changed=build.rs");
}
