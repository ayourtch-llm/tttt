use crate::error::{SandboxError, Result};
use std::path::{Path, PathBuf};

/// File access mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Read,
    Write,
    ReadWrite,
}

impl AccessMode {
    /// Check if this mode satisfies the required mode.
    pub fn contains(self, required: AccessMode) -> bool {
        match (self, required) {
            (AccessMode::ReadWrite, _) => true,
            (AccessMode::Read, AccessMode::Read) => true,
            (AccessMode::Write, AccessMode::Write) => true,
            _ => false,
        }
    }
}

/// A filesystem capability: a path with an access mode.
#[derive(Debug, Clone)]
pub struct FsCapability {
    /// Path as originally specified.
    pub original: PathBuf,
    /// Canonicalized absolute path (resolved symlinks).
    pub resolved: PathBuf,
    /// Access mode granted.
    pub access: AccessMode,
    /// Whether this is a single file (vs directory).
    pub is_file: bool,
}

/// Builder for sandbox capabilities.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    pub fs_capabilities: Vec<FsCapability>,
    pub network_blocked: bool,
}

impl CapabilitySet {
    /// Create an empty capability set (no access to anything).
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant access to a path (directory or file).
    pub fn allow_path(mut self, path: impl AsRef<Path>, access: AccessMode) -> Result<Self> {
        let path = path.as_ref();
        let resolved = path
            .canonicalize()
            .map_err(|_| SandboxError::PathNotFound(path.display().to_string()))?;
        let is_file = resolved.is_file();
        self.fs_capabilities.push(FsCapability {
            original: path.to_path_buf(),
            resolved,
            access,
            is_file,
        });
        Ok(self)
    }

    /// Grant access to a path without canonicalization (for paths that may not exist yet).
    pub fn allow_path_unchecked(mut self, path: impl AsRef<Path>, access: AccessMode) -> Self {
        let path = path.as_ref();
        self.fs_capabilities.push(FsCapability {
            original: path.to_path_buf(),
            resolved: path.to_path_buf(),
            access,
            is_file: false,
        });
        self
    }

    /// Block all network access.
    pub fn block_network(mut self) -> Self {
        self.network_blocked = true;
        self
    }

    /// Check if a path is covered by any capability.
    pub fn path_covered(&self, path: &Path) -> bool {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.fs_capabilities
            .iter()
            .any(|cap| canonical.starts_with(&cap.resolved))
    }

    /// Check if a path is covered with a specific access mode.
    pub fn path_covered_with_access(&self, path: &Path, required: AccessMode) -> bool {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.fs_capabilities
            .iter()
            .any(|cap| canonical.starts_with(&cap.resolved) && cap.access.contains(required))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_set_new_empty() {
        let caps = CapabilitySet::new();
        assert!(caps.fs_capabilities.is_empty());
        assert!(!caps.network_blocked);
    }

    #[test]
    fn test_allow_path_read() {
        let caps = CapabilitySet::new().allow_path("/tmp", AccessMode::Read).unwrap();
        assert_eq!(caps.fs_capabilities.len(), 1);
        assert_eq!(caps.fs_capabilities[0].access, AccessMode::Read);
    }

    #[test]
    fn test_allow_path_readwrite() {
        let caps = CapabilitySet::new()
            .allow_path("/tmp", AccessMode::ReadWrite)
            .unwrap();
        assert_eq!(caps.fs_capabilities[0].access, AccessMode::ReadWrite);
    }

    #[test]
    fn test_allow_path_nonexistent() {
        let result = CapabilitySet::new().allow_path("/nonexistent/path/xyz", AccessMode::Read);
        assert!(result.is_err());
    }

    #[test]
    fn test_allow_path_unchecked() {
        let caps = CapabilitySet::new().allow_path_unchecked("/future/path", AccessMode::Write);
        assert_eq!(caps.fs_capabilities.len(), 1);
    }

    #[test]
    fn test_block_network() {
        let caps = CapabilitySet::new().block_network();
        assert!(caps.network_blocked);
    }

    #[test]
    fn test_access_mode_contains() {
        assert!(AccessMode::ReadWrite.contains(AccessMode::Read));
        assert!(AccessMode::ReadWrite.contains(AccessMode::Write));
        assert!(AccessMode::ReadWrite.contains(AccessMode::ReadWrite));
        assert!(AccessMode::Read.contains(AccessMode::Read));
        assert!(!AccessMode::Read.contains(AccessMode::Write));
        assert!(!AccessMode::Read.contains(AccessMode::ReadWrite));
        assert!(AccessMode::Write.contains(AccessMode::Write));
        assert!(!AccessMode::Write.contains(AccessMode::Read));
    }

    #[test]
    fn test_path_covered() {
        let caps = CapabilitySet::new().allow_path("/tmp", AccessMode::Read).unwrap();
        assert!(caps.path_covered(Path::new("/tmp")));
    }

    #[test]
    fn test_path_not_covered() {
        let caps = CapabilitySet::new().allow_path("/tmp", AccessMode::Read).unwrap();
        // /usr is not covered by /tmp
        assert!(!caps.path_covered(Path::new("/usr")));
    }

    #[test]
    fn test_path_covered_with_access() {
        let caps = CapabilitySet::new().allow_path("/tmp", AccessMode::Read).unwrap();
        assert!(caps.path_covered_with_access(Path::new("/tmp"), AccessMode::Read));
        assert!(!caps.path_covered_with_access(Path::new("/tmp"), AccessMode::Write));
    }

    #[test]
    fn test_chaining() {
        let caps = CapabilitySet::new()
            .allow_path("/tmp", AccessMode::Read)
            .unwrap()
            .allow_path("/usr", AccessMode::Read)
            .unwrap()
            .block_network();
        assert_eq!(caps.fs_capabilities.len(), 2);
        assert!(caps.network_blocked);
    }
}
