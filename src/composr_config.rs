//! Per-project composr configuration, read from `composr.json` in the
//! project root. Separate from `composer.json` so we don't stomp on what
//! other tools / users put under `extra`.
//!
//! Schema (all keys optional):
//! ```json
//! {
//!   "allow-inert-plugins": ["vendor/pkg-a", "vendor/*", "*"]
//! }
//! ```
//!
//! Each entry is matched against composer-plugin package names. `*` is the
//! only wildcard and matches any sequence of characters — same behavior as
//! composer's `config.allow-plugins`. So `vendor/*` matches everything from
//! `vendor`, `*/foo` matches `foo` from any vendor, and bare `*` matches all.
//!
//! Missing or malformed file → defaults (empty config). Malformed file logs
//! a warning but doesn't fail the run; the config is purely advisory.

use serde::Deserialize;
use std::path::Path;

pub const FILE_NAME: &str = "composr.json";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ComposrConfig {
    /// Composer-plugin package patterns that should NOT trigger the
    /// "installing as inert files" warning, and (with delegation enabled)
    /// won't force a composer subprocess fallback. The hardcoded allowlist
    /// in `composer_shim::INERT_ALLOWLIST` is always applied; this extends
    /// it for project-specific known-safe plugins. Supports `*` wildcards.
    #[serde(rename = "allow-inert-plugins")]
    pub allow_inert_plugins: Vec<String>,
}

impl ComposrConfig {
    /// True if `plugin_name` matches any pattern in `allow_inert_plugins`.
    /// `*` is the wildcard (any sequence), per composer's allow-plugins.
    pub fn allows_inert(&self, plugin_name: &str) -> bool {
        self.allow_inert_plugins
            .iter()
            .any(|p| matches_pattern(plugin_name, p))
    }
}

/// Match `name` against `pattern`, treating `*` as "any sequence of any
/// characters." All other characters compare literally. No anchors — the
/// pattern must match the *entire* name.
///
/// Mirrors composer's `BasePackage::packageNameToRegexp` semantics: every
/// `*` becomes `.*`, everything else is literal, and the whole input must
/// match.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    // Fast paths for the common cases — no wildcard, or universal wildcard.
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return name == pattern;
    }

    // Walk the pattern segments around `*`. The first segment must prefix
    // `name`, the last must suffix the remainder, and intermediate segments
    // must each appear in order — left-to-right, non-overlapping.
    let segments: Vec<&str> = pattern.split('*').collect();
    let last = segments.len() - 1;
    let mut cursor = name;
    for (i, seg) in segments.iter().enumerate() {
        if i == 0 {
            if !cursor.starts_with(seg) {
                return false;
            }
            cursor = &cursor[seg.len()..];
        } else if i == last {
            return cursor.ends_with(seg);
        } else if seg.is_empty() {
            // Consecutive `**` — equivalent to a single `*`.
            continue;
        } else {
            match cursor.find(seg) {
                Some(idx) => cursor = &cursor[idx + seg.len()..],
                None => return false,
            }
        }
    }
    // Pattern was entirely empty segments (e.g. `**`) → matches anything.
    true
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

    #[test]
    fn matches_pattern_exact() {
        assert!(matches_pattern("vendor/pkg", "vendor/pkg"));
        assert!(!matches_pattern("vendor/pkg", "vendor/other"));
    }

    #[test]
    fn matches_pattern_universal_wildcard() {
        assert!(matches_pattern("anything/at-all", "*"));
        assert!(matches_pattern("", "*"));
    }

    #[test]
    fn matches_pattern_vendor_glob() {
        assert!(matches_pattern("vendor/foo", "vendor/*"));
        assert!(matches_pattern("vendor/bar-baz", "vendor/*"));
        assert!(!matches_pattern("other/foo", "vendor/*"));
    }

    #[test]
    fn matches_pattern_package_glob() {
        assert!(matches_pattern("acme/pest", "*/pest"));
        assert!(matches_pattern("zzz/pest", "*/pest"));
        assert!(!matches_pattern("acme/pest-plugin", "*/pest"));
    }

    #[test]
    fn matches_pattern_internal_wildcard() {
        assert!(matches_pattern("acme/pest-plugin-foo", "acme/pest-*-foo"));
        assert!(!matches_pattern("acme/foo", "acme/pest-*-foo"));
    }

    #[test]
    fn matches_pattern_consecutive_stars_no_op() {
        assert!(matches_pattern("vendor/pkg", "vendor/**"));
    }

    #[test]
    fn allows_inert_walks_patterns() {
        let cfg = ComposrConfig {
            allow_inert_plugins: vec![
                "exact/match".into(),
                "glob/*".into(),
            ],
        };
        assert!(cfg.allows_inert("exact/match"));
        assert!(cfg.allows_inert("glob/anything"));
        assert!(!cfg.allows_inert("other/pkg"));
    }
}
