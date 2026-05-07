//! Parser for composer.lock — the only data source `install` needs.
//!
//! We only model what we actually consume: package identity, the dist-zip
//! download triple (url/ref/shasum), bin entries, type, and the bits that
//! flow through into `installed.json` / `installed.php` verbatim.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct RawLock {
    #[serde(default)]
    packages: Vec<Value>,
    #[serde(default, rename = "packages-dev")]
    packages_dev: Vec<Value>,
    #[serde(default, rename = "content-hash")]
    content_hash: Option<String>,
    #[serde(default)]
    aliases: Vec<Value>,
    #[serde(default)]
    platform: Value,
    #[serde(default, rename = "platform-dev")]
    platform_dev: Value,
    #[serde(default, rename = "minimum-stability")]
    minimum_stability: Option<String>,
    #[serde(default, rename = "stability-flags")]
    stability_flags: Value,
    #[serde(default, rename = "prefer-stable")]
    prefer_stable: Value,
    #[serde(default, rename = "prefer-lowest")]
    prefer_lowest: Value,
    #[serde(default, rename = "platform-overrides")]
    platform_overrides: Value,
    #[serde(default, rename = "plugin-api-version")]
    plugin_api_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LockedPackage {
    /// Original JSON object as parsed from composer.lock — preserved so we
    /// can echo it into installed.json without losing fields we don't model.
    pub raw: Value,
    pub name: String,
    pub version: String,
    /// e.g. "library", "metapackage", "composer-plugin", "project".
    pub package_type: String,
    pub dist: Option<Dist>,
    pub bin: Vec<String>,
    /// Whether this package was listed in `packages-dev`.
    pub dev: bool,
}

#[derive(Debug, Clone)]
pub struct Dist {
    pub kind: String, // "zip", "tar", "phar", ...
    pub url: String,
    pub reference: String,
    pub shasum: String, // sha1, may be empty (GitHub zipballs don't ship one)
}

/// Top-level lock payload after parsing. Holds every field we need for both
/// install operations and the `installed.json` we'll later write.
#[derive(Debug, Clone)]
pub struct Lock {
    pub packages: Vec<LockedPackage>,
    pub content_hash: Option<String>,
    pub aliases: Value,
    pub platform: Value,
    pub platform_dev: Value,
    pub platform_overrides: Value,
    pub minimum_stability: Option<String>,
    pub stability_flags: Value,
    pub prefer_stable: Value,
    pub prefer_lowest: Value,
    pub plugin_api_version: Option<String>,
}

impl Lock {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let raw: RawLock = serde_json::from_slice(bytes).context("parsing composer.lock")?;

        let mut packages = Vec::with_capacity(raw.packages.len() + raw.packages_dev.len());
        for v in raw.packages {
            packages.push(parse_package(v, false)?);
        }
        for v in raw.packages_dev {
            packages.push(parse_package(v, true)?);
        }

        // Sanity: no duplicate names. composer.lock should already be unique;
        // if it isn't we'd silently install one twice.
        let mut seen = HashSet::new();
        for p in &packages {
            if !seen.insert(&p.name) {
                return Err(anyhow!(
                    "composer.lock contains duplicate package {}",
                    p.name
                ));
            }
        }

        Ok(Lock {
            packages,
            content_hash: raw.content_hash,
            aliases: raw.aliases.into(),
            platform: raw.platform,
            platform_dev: raw.platform_dev,
            platform_overrides: raw.platform_overrides,
            minimum_stability: raw.minimum_stability,
            stability_flags: raw.stability_flags,
            prefer_stable: raw.prefer_stable,
            prefer_lowest: raw.prefer_lowest,
            plugin_api_version: raw.plugin_api_version,
        })
    }

    pub fn dev_package_names(&self) -> Vec<String> {
        self.packages
            .iter()
            .filter(|p| p.dev)
            .map(|p| p.name.clone())
            .collect()
    }
}

fn parse_package(v: Value, dev: bool) -> Result<LockedPackage> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow!("expected package object, got {v:?}"))?;
    let name = obj
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("package missing `name`"))?
        .to_string();
    let version = obj
        .get("version")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("package {name} missing `version`"))?
        .to_string();
    let package_type = obj
        .get("type")
        .and_then(|x| x.as_str())
        .unwrap_or("library")
        .to_string();

    let dist = obj.get("dist").and_then(|d| {
        let dobj = d.as_object()?;
        let kind = dobj.get("type")?.as_str()?.to_string();
        let url = dobj.get("url")?.as_str()?.to_string();
        let reference = dobj
            .get("reference")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let shasum = dobj
            .get("shasum")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        Some(Dist {
            kind,
            url,
            reference,
            shasum,
        })
    });

    let bin = obj
        .get("bin")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|b| b.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(LockedPackage {
        raw: v,
        name,
        version,
        package_type,
        dist,
        bin,
        dev,
    })
}

/// Composer normalizes versions for `version_normalized` in installed.json.
/// We don't reimplement the full VersionParser — we mirror the few rules that
/// matter for stable semver and dev branches, which is what the lock contains
/// in practice. Anything we can't normalize confidently falls back to the
/// `version` string as-is, which is what Composer does too for unusual inputs.
pub fn normalize_version(v: &str) -> String {
    let trimmed = v.trim();
    // dev-master -> dev-master, dev-main -> dev-main: leave alone.
    if let Some(rest) = trimmed.strip_prefix("dev-") {
        return format!("dev-{rest}");
    }
    // X.Y.Z-dev (branch alias) — leave intact.
    if trimmed.ends_with("-dev") {
        return trimmed.to_string();
    }
    // Strip leading 'v' if present (vX.Y.Z -> X.Y.Z).
    let body = trimmed.strip_prefix('v').unwrap_or(trimmed);

    // Split off pre-release suffix (e.g. "1.2.3-beta1").
    let (numeric, suffix) = match body.find('-') {
        Some(i) => (&body[..i], Some(&body[i..])),
        None => (body, None),
    };

    let parts: Vec<&str> = numeric.split('.').collect();
    if parts.iter().any(|p| p.parse::<u64>().is_err()) {
        return trimmed.to_string();
    }

    // Pad to 4 segments with zeros.
    let mut padded: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
    while padded.len() < 4 {
        padded.push("0".into());
    }
    let mut out = padded.join(".");
    if let Some(s) = suffix {
        out.push_str(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_packages_and_dev_split() {
        let lock = br#"{
            "packages": [
                {"name": "a/b", "version": "1.2.3", "type": "library",
                 "dist": {"type": "zip", "url": "https://x/y", "reference": "abc", "shasum": ""}}
            ],
            "packages-dev": [
                {"name": "c/d", "version": "0.1.0", "type": "library",
                 "dist": {"type": "zip", "url": "https://x/z", "reference": "def", "shasum": "deadbeef"},
                 "bin": ["bin/foo"]}
            ],
            "content-hash": "h"
        }"#;
        let l = Lock::parse(lock).unwrap();
        assert_eq!(l.packages.len(), 2);
        assert_eq!(l.packages[0].name, "a/b");
        assert!(!l.packages[0].dev);
        assert_eq!(l.packages[1].name, "c/d");
        assert!(l.packages[1].dev);
        assert_eq!(l.packages[1].bin, vec!["bin/foo".to_string()]);
        assert_eq!(l.dev_package_names(), vec!["c/d".to_string()]);
    }

    #[test]
    fn normalize_version_examples() {
        assert_eq!(normalize_version("1.2.3"), "1.2.3.0");
        assert_eq!(normalize_version("v1.2.3"), "1.2.3.0");
        assert_eq!(normalize_version("1.2"), "1.2.0.0");
        assert_eq!(normalize_version("dev-master"), "dev-master");
        assert_eq!(normalize_version("1.2.3-beta1"), "1.2.3.0-beta1");
        assert_eq!(normalize_version("3.x-dev"), "3.x-dev");
    }

    #[test]
    fn rejects_duplicates() {
        let lock = br#"{"packages":[
            {"name":"a/b","version":"1","type":"library"},
            {"name":"a/b","version":"2","type":"library"}
        ]}"#;
        assert!(Lock::parse(lock).is_err());
    }
}
