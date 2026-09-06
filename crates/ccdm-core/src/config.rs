//! App configuration (cf. XDM `Config`).
//!
//! Stored as JSON so users can inspect/edit it without any tooling:
//! `<config-dir>/ccdm/config.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{CcdmError, Result};

/// Global settings for the download manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Where finished files go.
    pub download_dir: PathBuf,
    /// Max connections (segments) per download, like XDM's setting.
    pub max_connections: usize,
    /// Max downloads running at the same time.
    pub max_concurrent_downloads: usize,
    /// Global speed cap in KiB/s (0 = unlimited).
    pub speed_limit_kbps: u32,
    /// Whether the speed cap applies.
    pub enable_speed_limit: bool,
    /// HTTP(S) proxy URL, e.g. `http://127.0.0.1:8080` (cf. XDM ProxyInfo).
    /// `None` (default) means direct connection.
    pub proxy_url: Option<String>,
    /// Sort finished files into `<dir>/<Category>/` subfolders by extension
    /// (cf. XDM categories folders). Off by default.
    pub organize_by_category: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        let download_dir = dirs::download_dir().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        });
        Self {
            download_dir,
            max_connections: 8,
            max_concurrent_downloads: 3,
            speed_limit_kbps: 0,
            enable_speed_limit: false,
            proxy_url: None,
            organize_by_category: false,
        }
    }
}

impl AppConfig {
    /// Clamp to sane bounds (at least 1 connection, ...) and drop an
    /// empty proxy string back to `None`.
    pub fn normalized(mut self) -> Self {
        self.max_connections = self.max_connections.clamp(1, 32);
        self.max_concurrent_downloads = self.max_concurrent_downloads.clamp(1, 10);
        if self.proxy_url.as_deref().map(str::trim) == Some("") {
            self.proxy_url = None;
        }
        self
    }

    /// OS-specific config file path, if a config dir is known.
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("ccdm").join("config.json"))
    }

    /// Load from `path`, or return defaults when the file is missing.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default().normalized());
        }
        let text = std::fs::read_to_string(path).map_err(CcdmError::from)?;
        serde_json::from_str::<Self>(&text)
            .map(|c| c.normalized())
            .map_err(CcdmError::from)
    }

    /// Save to `path`, creating parent directories as needed.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CcdmError::from)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(CcdmError::from)?;
        std::fs::write(path, text).map_err(CcdmError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_clamps() {
        let cfg = AppConfig {
            max_connections: 0,
            max_concurrent_downloads: 99,
            ..AppConfig::default()
        }
        .normalized();
        assert_eq!(cfg.max_connections, 1);
        assert_eq!(cfg.max_concurrent_downloads, 10);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let cfg = AppConfig::default().normalized();
        cfg.save(&path).unwrap();
        let back = AppConfig::load(&path).unwrap();
        assert_eq!(back.max_connections, cfg.max_connections);
    }
}
