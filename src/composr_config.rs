//! Per-project composr configuration, read from `composr.json` in the
//! project root. Separate from `composer.json` so we don't stomp on what
//! other tools / users put under `extra`.
//!
//! Schema (all keys optional):
//! ```json
//! {
//!   "allow-inert-plugins": ["vendor/pkg-a", "vendor/pkg-b"]
//! }
//! ```
//!
//! Missing or malformed file → defaults (empty config). Malformed file logs
//! a warning but doesn't fail the run; the config is purely advisory.

use serde::Deserialize;
use std::path::Path;

pub const FILE_NAME: &str = "composr.json";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ComposrConfig {
    /// Composer-plugin package names that should NOT trigger the
    /// "installing as inert files" warning. The hardcoded allowlist in
    /// `composer_shim::INERT_ALLOWLIST` is always applied; this extends it
    /// for project-specific known-safe plugins.
    #[serde(rename = "allow-inert-plugins")]
    pub allow_inert_plugins: Vec<String>,
}

impl ComposrConfig {
    /// Read `<root>/composr.json` if present. Absent file → defaults.
    /// Malformed file → defaults plus a warn log (we don't want a typo here
    /// to break install).
    pub fn load(root: &Path) -> Self {
        let path = root.join(FILE_NAME);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!("could not read {}: {e}; using defaults", path.display());
                return Self::default();
            }
        };
        match serde_json::from_slice::<ComposrConfig>(&bytes) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "could not parse {}: {e}; using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn missing_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let cfg = ComposrConfig::load(dir.path());
        assert!(cfg.allow_inert_plugins.is_empty());
    }

    #[test]
    fn reads_allow_inert_plugins() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("composr.json"),
            r#"{"allow-inert-plugins": ["vendor/a", "vendor/b"]}"#,
        )
        .unwrap();
        let cfg = ComposrConfig::load(dir.path());
        assert_eq!(cfg.allow_inert_plugins, vec!["vendor/a", "vendor/b"]);
    }

    #[test]
    fn malformed_file_returns_defaults_without_panicking() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("composr.json"), "{ not json").unwrap();
        let cfg = ComposrConfig::load(dir.path());
        assert!(cfg.allow_inert_plugins.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("composr.json"),
            r#"{"future-key": 42, "allow-inert-plugins": ["x/y"]}"#,
        )
        .unwrap();
        let cfg = ComposrConfig::load(dir.path());
        assert_eq!(cfg.allow_inert_plugins, vec!["x/y"]);
    }
}
