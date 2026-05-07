//! Read/write `vendor/composer/installed.json` and `installed.php`.
//!
//! `installed.json` is the source of truth for "what is currently in vendor/".
//! We compare it against the lock to compute install/remove operations and
//! rewrite it after we mutate vendor/.
//!
//! `installed.php` is consumed at runtime by `Composer\InstalledVersions` —
//! Laravel's package discovery and others depend on it, so we have to emit
//! a byte-identical-shape file (PHP `<?php return array(...);`).

use crate::lock::{normalize_version, Lock, LockedPackage};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
pub struct InstalledJson {
    #[serde(default)]
    pub packages: Vec<Value>,
    #[serde(default)]
    pub dev: bool,
    #[serde(default, rename = "dev-package-names")]
    pub dev_package_names: Vec<String>,
}

impl InstalledJson {
    pub fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let parsed: InstalledJson =
            serde_json::from_slice(&bytes).context("parsing installed.json")?;
        Ok(parsed)
    }

    /// Map name -> reference. The reference is what changes when a package
    /// is updated to a new commit; if it differs, we need to re-install.
    pub fn refs(&self) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        for p in &self.packages {
            let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let r = p
                .get("dist")
                .and_then(|d| d.get("reference"))
                .and_then(|x| x.as_str())
                .or_else(|| {
                    p.get("source")
                        .and_then(|s| s.get("reference"))
                        .and_then(|x| x.as_str())
                })
                .unwrap_or("");
            if !name.is_empty() {
                m.insert(name.to_string(), r.to_string());
            }
        }
        m
    }
}

/// Compose the installed.json payload from a lock + the dev flag chosen for
/// this install run. Packages are written in alphabetical order, mirroring
/// Composer.
pub fn build_installed_json(lock: &Lock, with_dev: bool) -> Value {
    let mut packages: Vec<Value> = lock
        .packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .map(installed_entry_for)
        .collect();
    packages.sort_by(|a, b| {
        let an = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
        an.cmp(bn)
    });

    let mut dev_names: Vec<String> = if with_dev {
        lock.dev_package_names()
    } else {
        Vec::new()
    };
    dev_names.sort();

    json!({
        "packages": packages,
        "dev": with_dev,
        "dev-package-names": dev_names,
    })
}

/// Convert a locked package into an installed.json entry: it's the lock entry
/// plus three derived fields (`version_normalized`, `installation-source`,
/// `install-path`).
fn installed_entry_for(pkg: &LockedPackage) -> Value {
    let mut obj: Map<String, Value> = pkg
        .raw
        .as_object()
        .cloned()
        .unwrap_or_default();
    obj.insert(
        "version_normalized".into(),
        Value::String(normalize_version(&pkg.version)),
    );
    obj.insert("installation-source".into(), Value::String("dist".into()));
    if pkg.package_type == "metapackage" {
        // Metapackages don't get an install path.
    } else {
        obj.insert(
            "install-path".into(),
            Value::String(format!("../{}", pkg.name)),
        );
    }
    Value::Object(obj)
}

pub fn write_installed_json(path: &Path, payload: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let pretty = serde_json::to_vec_pretty(payload).context("serializing installed.json")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, pretty).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Render `installed.php` — a PHP file that returns a nested array. Consumed
/// by `Composer\InstalledVersions` at runtime (Laravel's package:discover
/// reads it via that class).
pub fn render_installed_php(
    lock: &Lock,
    with_dev: bool,
    root_name: &str,
    root_version: &str,
    root_reference: &str,
    root_type: &str,
    root_install_path: &Path, // absolute path to project root
    vendor_composer_dir: &Path, // absolute path to vendor/composer
) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64 * 1024);
    s.push_str("<?php return array(\n");

    // root entry
    let rel_root = pathdiff_to_php_string(vendor_composer_dir, root_install_path);
    write!(s, "    'root' => array(\n").unwrap();
    write!(s, "        'name' => {},\n", php_str(root_name)).unwrap();
    write!(s, "        'pretty_version' => {},\n", php_str(root_version)).unwrap();
    write!(
        s,
        "        'version' => {},\n",
        php_str(&normalize_version(root_version))
    )
    .unwrap();
    write!(s, "        'reference' => {},\n", php_str(root_reference)).unwrap();
    write!(s, "        'type' => {},\n", php_str(root_type)).unwrap();
    write!(s, "        'install_path' => {},\n", rel_root).unwrap();
    write!(s, "        'aliases' => array(),\n").unwrap();
    write!(s, "        'dev' => {},\n", if with_dev { "true" } else { "false" }).unwrap();
    write!(s, "    ),\n").unwrap();

    // versions
    write!(s, "    'versions' => array(\n").unwrap();
    let mut entries: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    // root self-reference
    write!(s, "        {} => array(\n", php_str(root_name)).unwrap();
    write!(s, "            'pretty_version' => {},\n", php_str(root_version)).unwrap();
    write!(
        s,
        "            'version' => {},\n",
        php_str(&normalize_version(root_version))
    )
    .unwrap();
    write!(
        s,
        "            'reference' => {},\n",
        php_str(root_reference)
    )
    .unwrap();
    write!(s, "            'type' => {},\n", php_str(root_type)).unwrap();
    write!(s, "            'install_path' => {},\n", rel_root).unwrap();
    write!(s, "            'aliases' => array(),\n").unwrap();
    write!(s, "            'dev_requirement' => false,\n").unwrap();
    write!(s, "        ),\n").unwrap();

    for pkg in entries {
        let dist_ref = pkg
            .dist
            .as_ref()
            .map(|d| d.reference.as_str())
            .unwrap_or("");
        let install_path = if pkg.package_type == "metapackage" {
            None
        } else {
            let pkg_dir = vendor_composer_dir
                .parent()
                .unwrap_or(vendor_composer_dir)
                .join(&pkg.name);
            Some(pathdiff_to_php_string(vendor_composer_dir, &pkg_dir))
        };

        write!(s, "        {} => array(\n", php_str(&pkg.name)).unwrap();
        write!(
            s,
            "            'pretty_version' => {},\n",
            php_str(&pkg.version)
        )
        .unwrap();
        write!(
            s,
            "            'version' => {},\n",
            php_str(&normalize_version(&pkg.version))
        )
        .unwrap();
        write!(s, "            'reference' => {},\n", php_str(dist_ref)).unwrap();
        write!(s, "            'type' => {},\n", php_str(&pkg.package_type)).unwrap();
        if let Some(ip) = install_path {
            write!(s, "            'install_path' => {},\n", ip).unwrap();
        }
        write!(s, "            'aliases' => array(),\n").unwrap();
        write!(
            s,
            "            'dev_requirement' => {},\n",
            if pkg.dev { "true" } else { "false" }
        )
        .unwrap();
        write!(s, "        ),\n").unwrap();
    }
    s.push_str("    ),\n);\n");
    s
}

/// Render a path as PHP code, expressed relative to vendor/composer using
/// `__DIR__ . '/...'` — Composer's exact convention so the file is
/// relocatable.
fn pathdiff_to_php_string(from: &Path, to: &Path) -> String {
    let rel = path_relative(from, to);
    format!("__DIR__ . '/{}'", rel.replace('\\', "/"))
}

/// Compute `to` relative to `from`, using `..` to walk up. Both inputs should
/// be absolute. Falls back to the absolute path of `to` if they share no
/// prefix (cross-volume on Windows).
fn path_relative(from: &Path, to: &Path) -> String {
    let from = from.components().collect::<Vec<_>>();
    let to_components: Vec<PathBuf> = to
        .components()
        .map(|c| PathBuf::from(c.as_os_str()))
        .collect();

    let mut common = 0;
    for (a, b) in from.iter().zip(to.components()) {
        if a.as_os_str() == b.as_os_str() {
            common += 1;
        } else {
            break;
        }
    }

    let ups = from.len().saturating_sub(common);
    let mut parts: Vec<String> = (0..ups).map(|_| "..".to_string()).collect();
    for c in to_components.iter().skip(common) {
        parts.push(c.to_string_lossy().to_string());
    }
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// PHP single-quoted string. `'` and `\` are the only escapes.
fn php_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Set of names from the lock — used for sanity checks in install.rs.
pub fn lock_names(lock: &Lock, with_dev: bool) -> HashSet<String> {
    lock.packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .map(|p| p.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_walks_up() {
        let from = Path::new("/a/b/c");
        let to = Path::new("/a/b/x/y");
        assert_eq!(path_relative(from, to), "../x/y");
    }

    #[test]
    fn relative_path_same_dir() {
        let from = Path::new("/a/b");
        let to = Path::new("/a/b");
        assert_eq!(path_relative(from, to), ".");
    }

    #[test]
    fn php_str_escapes_quote_and_backslash() {
        assert_eq!(php_str("hi"), "'hi'");
        assert_eq!(php_str("it's"), "'it\\'s'");
        assert_eq!(php_str("a\\b"), "'a\\\\b'");
    }
}
