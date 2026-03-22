mod capability;
mod error;
mod profile;

pub use capability::{AccessMode, CapabilitySet, FsCapability};
pub use error::{SandboxError, Result};
pub use profile::SandboxProfile;

/// Check if OS-level sandboxing is available on this platform.
pub fn is_supported() -> bool {
    cfg!(target_os = "linux") || cfg!(target_os = "macos")
}

/// Apply the sandbox. This is irreversible — once applied, the process
/// cannot regain capabilities that were not granted.
///
/// Currently a no-op placeholder. Will be implemented with:
/// - Landlock on Linux
/// - Seatbelt on macOS
pub fn apply(caps: &CapabilitySet) -> Result<()> {
    let _ = caps;
    // TODO: implement platform-specific sandboxing
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_supported() {
        // On macOS and Linux this should be true
        if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
            assert!(is_supported());
        }
    }

    #[test]
    fn test_apply_placeholder() {
        let caps = CapabilitySet::new();
        apply(&caps).unwrap();
    }
}
