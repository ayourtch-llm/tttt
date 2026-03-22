use crate::capability::{AccessMode, CapabilitySet};
use crate::error::Result;
use std::path::PathBuf;

/// Predefined sandbox profiles for common agent configurations.
#[derive(Debug, Clone)]
pub enum SandboxProfile {
    /// Agent has read-only access to the worktree.
    ReadOnlyWorktree { worktree_path: PathBuf },

    /// Agent has read-write access to the worktree.
    ReadWriteWorktree { worktree_path: PathBuf },

    /// Agent gets its own git worktree (read-write).
    OwnWorktree { worktree_path: PathBuf },

    /// Custom capability set.
    Custom(CapabilitySet),
}

impl SandboxProfile {
    /// Convert this profile to a CapabilitySet.
    pub fn to_capability_set(&self) -> Result<CapabilitySet> {
        match self {
            SandboxProfile::ReadOnlyWorktree { worktree_path } => {
                let caps = CapabilitySet::new()
                    .allow_path(worktree_path, AccessMode::Read)?
                    .allow_path_unchecked("/usr", AccessMode::Read)
                    .allow_path_unchecked("/lib", AccessMode::Read)
                    .allow_path_unchecked("/bin", AccessMode::Read)
                    .allow_path_unchecked("/sbin", AccessMode::Read)
                    .block_network();
                Ok(caps)
            }
            SandboxProfile::ReadWriteWorktree { worktree_path } => {
                let caps = CapabilitySet::new()
                    .allow_path(worktree_path, AccessMode::ReadWrite)?
                    .allow_path_unchecked("/usr", AccessMode::Read)
                    .allow_path_unchecked("/lib", AccessMode::Read)
                    .allow_path_unchecked("/bin", AccessMode::Read)
                    .allow_path_unchecked("/sbin", AccessMode::Read);
                Ok(caps)
            }
            SandboxProfile::OwnWorktree { worktree_path } => {
                let caps = CapabilitySet::new()
                    .allow_path(worktree_path, AccessMode::ReadWrite)?
                    .allow_path_unchecked("/usr", AccessMode::Read)
                    .allow_path_unchecked("/lib", AccessMode::Read)
                    .allow_path_unchecked("/bin", AccessMode::Read)
                    .allow_path_unchecked("/sbin", AccessMode::Read);
                Ok(caps)
            }
            SandboxProfile::Custom(caps) => Ok(caps.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readonly_worktree_profile() {
        let profile = SandboxProfile::ReadOnlyWorktree {
            worktree_path: PathBuf::from("/tmp"),
        };
        let caps = profile.to_capability_set().unwrap();
        assert!(caps.network_blocked);
        // Should have /tmp as read-only
        let tmp_cap = caps
            .fs_capabilities
            .iter()
            .find(|c| c.original == PathBuf::from("/tmp"));
        assert!(tmp_cap.is_some());
        assert_eq!(tmp_cap.unwrap().access, AccessMode::Read);
    }

    #[test]
    fn test_readwrite_worktree_profile() {
        let profile = SandboxProfile::ReadWriteWorktree {
            worktree_path: PathBuf::from("/tmp"),
        };
        let caps = profile.to_capability_set().unwrap();
        assert!(!caps.network_blocked);
        let tmp_cap = caps
            .fs_capabilities
            .iter()
            .find(|c| c.original == PathBuf::from("/tmp"));
        assert!(tmp_cap.is_some());
        assert_eq!(tmp_cap.unwrap().access, AccessMode::ReadWrite);
    }

    #[test]
    fn test_own_worktree_profile() {
        let profile = SandboxProfile::OwnWorktree {
            worktree_path: PathBuf::from("/tmp"),
        };
        let caps = profile.to_capability_set().unwrap();
        let tmp_cap = caps
            .fs_capabilities
            .iter()
            .find(|c| c.original == PathBuf::from("/tmp"));
        assert!(tmp_cap.is_some());
        assert_eq!(tmp_cap.unwrap().access, AccessMode::ReadWrite);
    }

    #[test]
    fn test_custom_profile() {
        let custom = CapabilitySet::new()
            .allow_path_unchecked("/custom", AccessMode::Read)
            .block_network();
        let profile = SandboxProfile::Custom(custom);
        let caps = profile.to_capability_set().unwrap();
        assert!(caps.network_blocked);
        assert_eq!(caps.fs_capabilities.len(), 1);
    }

    #[test]
    fn test_profile_nonexistent_worktree() {
        let profile = SandboxProfile::ReadOnlyWorktree {
            worktree_path: PathBuf::from("/nonexistent/worktree"),
        };
        assert!(profile.to_capability_set().is_err());
    }
}
