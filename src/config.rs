use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level configuration for tttt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The prefix key byte (default: 0x1c = Ctrl+\).
    pub prefix_key: u8,

    /// Width of the right sidebar in columns.
    pub sidebar_width: u16,

    /// Command to launch as the root process.
    pub root_command: String,

    /// Arguments for the root command.
    #[serde(default)]
    pub root_args: Vec<String>,

    /// Working directory for launched sessions.
    pub work_dir: PathBuf,

    /// Directory for text log files.
    pub log_dir: PathBuf,

    /// Path to the SQLite log database.
    pub db_path: PathBuf,

    /// Maximum concurrent sessions.
    pub max_sessions: usize,

    /// Default terminal width.
    pub default_cols: u16,

    /// Default terminal height.
    pub default_rows: u16,

    /// Log user keystrokes (input events) to SQLite. DISABLED by default because
    /// input may contain passwords and other sensitive data.
    #[serde(default)]
    pub log_input: bool,

    /// Enable TUI control MCP tools (tui_switch, tui_get_info, tui_highlight).
    #[serde(default)]
    pub tui_tools: bool,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let data_dir = PathBuf::from(&home).join(".local/share/tttt");

        Self {
            prefix_key: 0x1c,
            sidebar_width: 30,
            root_command: std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string()),
            root_args: Vec::new(),
            work_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            log_dir: data_dir.join("logs"),
            db_path: data_dir.join("tttt.db"),
            max_sessions: 15,
            default_cols: 80,
            default_rows: 24,
            log_input: false,
            tui_tools: false,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file, falling back to defaults.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path.as_ref())?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Load config from default locations, or return defaults if none found.
    pub fn load_default() -> Self {
        let candidates = [
            std::env::current_dir()
                .ok()
                .map(|d| d.join("tttt.toml")),
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config/tttt/config.toml")),
        ];

        for candidate in candidates.iter().flatten() {
            if candidate.exists() {
                if let Ok(config) = Self::load(candidate) {
                    return config;
                }
            }
        }

        Self::default()
    }

    /// Apply environment variable overrides.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("TTTT_PREFIX_KEY") {
            if let Ok(byte) = val.parse::<u8>() {
                self.prefix_key = byte;
            }
        }
        if let Ok(val) = std::env::var("TTTT_SIDEBAR_WIDTH") {
            if let Ok(width) = val.parse::<u16>() {
                self.sidebar_width = width;
            }
        }
        if let Ok(val) = std::env::var("TTTT_ROOT_COMMAND") {
            self.root_command = val;
        }
        if let Ok(val) = std::env::var("TTTT_WORK_DIR") {
            self.work_dir = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("TTTT_LOG_DIR") {
            self.log_dir = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("TTTT_DB_PATH") {
            self.db_path = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("TTTT_MAX_SESSIONS") {
            if let Ok(n) = val.parse::<usize>() {
                self.max_sessions = n;
            }
        }
    }

    /// Convert to DisplayConfig for the TUI.
    pub fn display_config(&self) -> tttt_tui::DisplayConfig {
        tttt_tui::DisplayConfig {
            sidebar_width: self.sidebar_width,
            prefix_key: self.prefix_key,
            status_line: true,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Parse(e)
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config I/O error: {}", e),
            ConfigError::Parse(e) => write!(f, "config parse error: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.prefix_key, 0x1c);
        assert_eq!(config.sidebar_width, 30);
        assert_eq!(config.max_sessions, 15);
        assert_eq!(config.default_cols, 80);
        assert_eq!(config.default_rows, 24);
        assert!(!config.log_input, "input logging must be disabled by default");
    }

    #[test]
    fn test_config_log_input_default_false() {
        // log_input must be false when not set in TOML
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.log_input);
    }

    #[test]
    fn test_config_log_input_can_be_enabled() {
        let config: Config = toml::from_str("log_input = true").unwrap();
        assert!(config.log_input);
    }

    #[test]
    fn test_config_from_toml() {
        let toml_str = r#"
prefix_key = 1
sidebar_width = 40
root_command = "/usr/bin/fish"
max_sessions = 10
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.prefix_key, 1);
        assert_eq!(config.sidebar_width, 40);
        assert_eq!(config.root_command, "/usr/bin/fish");
        assert_eq!(config.max_sessions, 10);
    }

    #[test]
    fn test_config_partial_toml() {
        let toml_str = r#"
sidebar_width = 25
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.sidebar_width, 25);
        // defaults for unspecified
        assert_eq!(config.prefix_key, 0x1c);
    }

    #[test]
    fn test_config_load_nonexistent() {
        let result = Config::load("/nonexistent/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_load_default_returns_defaults() {
        // In test environment, no config files should exist at the checked paths
        let config = Config::load_default();
        assert_eq!(config.prefix_key, 0x1c);
    }

    #[test]
    fn test_config_display_config() {
        let config = Config {
            sidebar_width: 35,
            prefix_key: 0x01,
            ..Default::default()
        };
        let dc = config.display_config();
        assert_eq!(dc.sidebar_width, 35);
        assert_eq!(dc.prefix_key, 0x01);
        assert!(dc.status_line);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.prefix_key, config.prefix_key);
        assert_eq!(parsed.sidebar_width, config.sidebar_width);
        assert_eq!(parsed.max_sessions, config.max_sessions);
    }
}
