//! Native implementation of Laravel's `package:discover` artisan command.
//!
//! Mirrors `Illuminate\Foundation\PackageManifest::build()` from
//! laravel/framework. The algorithm is small but the artisan boot to run it
//! costs ~10–30s on real apps (almost all of `post-autoload-dump`), so doing
//! it natively in milliseconds is the next obvious win after killing the
//! `dump-autoload` composer call.
//!
//! Algorithm:
//!  1. Read root `composer.json` → `extra.laravel.dont-discover` (a list of
//!     package names to skip; `"*"` means "skip all auto-discovery").
//!  2. Read `vendor/composer/installed.json`.
//!  3. For each package, build a `(name, extra.laravel)` entry.
//!  4. Walk the entries and merge each package's own `dont-discover` into the
//!     global ignore list (transitive opt-out).
//!  5. Drop entries whose package name is in the ignore list (or all if `*`
//!     was present at the root).
//!  6. Drop entries whose `extra.laravel` is empty (matches PHP's `->filter()`
//!     truthiness check on `[]`).
//!  7. Render the result as PHP, byte-compatible with
//!     `<?php return ` . `var_export($manifest, true)` . `;`, and write to
//!     `bootstrap/cache/packages.php` atomically.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    /// Project base path — used for `<root>/composer.json` (dont-discover
    /// source) and the default manifest output path. Equivalent to Laravel's
    /// `$basePath` in `PackageManifest`.
    pub root: PathBuf,
    /// `vendor/` directory. Defaults to `<root>/vendor` — matches Laravel's
    /// `$basePath . '/vendor'` fallback. For multi-app layouts where the
    /// Laravel project lives inside a larger composer root (monolith-app's
    /// `laravel-app/` under a vendor at the outer root), pass an explicit
    /// override.
    pub vendor_dir: Option<PathBuf>,
    /// Output path. Defaults to `<root>/bootstrap/cache/packages.php`.
    pub manifest_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct DiscoverReport {
    pub packages_in_manifest: usize,
    pub elapsed_ms: u128,
    pub manifest_path: PathBuf,
}

pub fn discover(opts: DiscoverOptions) -> Result<DiscoverReport> {
    let started = Instant::now();
    let root = opts.root.clone();
    let vendor_dir = opts
        .vendor_dir
        .clone()
        .unwrap_or_else(|| root.join("vendor"));
    let manifest_path = opts
        .manifest_path
        .clone()
        .unwrap_or_else(|| root.join("bootstrap").join("cache").join("packages.php"));

    // 1. Root dont-discover.
    let mut ignore: Vec<String> = read_root_dont_discover(&root)?;
    let ignore_all = ignore.iter().any(|s| s == "*");

    // 2. Installed packages.
    let installed_path = vendor_dir.join("composer").join("installed.json");
    let installed_packages = read_installed_packages(&installed_path)?;

    // 3. Build (name, extra.laravel) entries.
    let mut entries: Vec<(String, Value)> = installed_packages
        .into_iter()
        .filter_map(|p| {
            let name = p.get("name").and_then(|x| x.as_str())?.to_string();
            let cfg = p
                .get("extra")
                .and_then(|x| x.get("laravel"))
                .cloned()
                .unwrap_or(Value::Object(Default::default()));
            // PackageManifest treats non-array `extra.laravel` as `[]`.
            let cfg = if cfg.is_object() || cfg.is_array() {
                cfg
            } else {
                Value::Object(Default::default())
            };
            Some((name, cfg))
        })
        .collect();

    // 4. Transitive dont-discover.
    for (_, cfg) in &entries {
        if let Some(arr) = cfg.get("dont-discover").and_then(|x| x.as_array()) {
            for s in arr {
                if let Some(s) = s.as_str() {
                    ignore.push(s.to_string());
                }
            }
        }
    }

    // 5 + 6. Filter ignored, then drop empties.
    entries.retain(|(name, cfg)| {
        if ignore_all || ignore.iter().any(|i| i == name) {
            return false;
        }
        !is_empty_config(cfg)
    });

    // Sort alphabetically by package name (matches what we observe in real
    // app output and what `var_export` produces from a map keyed in
    // installed.json's natural order — packages are already sorted there).
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // 7. Render and write.
    let php = render_packages_php(&entries);

    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = manifest_path.with_extension("php.tmp");
    std::fs::write(&tmp, &php)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &manifest_path)
        .with_context(|| format!("renaming into {}", manifest_path.display()))?;

    Ok(DiscoverReport {
        packages_in_manifest: entries.len(),
        elapsed_ms: started.elapsed().as_millis(),
        manifest_path,
    })
}

fn read_root_dont_discover(root: &Path) -> Result<Vec<String>> {
    let path = root.join("composer.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_slice(&bytes).context("parsing composer.json")?;
    let arr = v
        .get("extra")
        .and_then(|x| x.get("laravel"))
        .and_then(|x| x.get("dont-discover"))
        .and_then(|x| x.as_array());
    Ok(arr
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

fn read_installed_packages(path: &Path) -> Result<Vec<Value>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_slice(&bytes).context("parsing installed.json")?;
    // Composer 2 wraps in `{ "packages": [...] }`; Composer 1 emitted a bare
    // array. PackageManifest::build accepts both shapes — match it.
    if let Some(arr) = v.get("packages").and_then(|x| x.as_array()) {
        Ok(arr.clone())
    } else if let Some(arr) = v.as_array() {
        Ok(arr.clone())
    } else {
        Ok(Vec::new())
    }
}

fn is_empty_config(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

/// Render the manifest as PHP, byte-compatible with
/// `<?php return ` . `var_export($manifest, true)` . `;`.
///
/// `var_export` quirks we mirror:
/// - 2-space indent per nesting level.
/// - Associative arrays: each key=>nested-array entry breaks across lines as
///   `<indent>'key' => \n<indent>array (\n...<indent>),`. Note the trailing
///   space after `=>`.
/// - Scalar entries stay inline: `<indent>'key' => 'value',`.
/// - Lists become numerically-keyed associative arrays (`0 => 'x', 1 => 'y'`).
/// - Strings use single quotes and only escape `\\` and `\'`.
/// - The top-level `array (...)` closes with `);` and no trailing newline.
fn render_packages_php(entries: &[(String, Value)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 * 1024);
    out.extend_from_slice(b"<?php return array (\n");
    for (k, v) in entries {
        write_entry(&mut out, EntryKey::Str(k), v, 1);
    }
    out.extend_from_slice(b");");
    out
}

enum EntryKey<'a> {
    Str(&'a str),
    Int(i64),
}

fn indent_of(out: &mut Vec<u8>, depth: usize) {
    for _ in 0..(depth * 2) {
        out.push(b' ');
    }
}

fn write_entry(out: &mut Vec<u8>, key: EntryKey<'_>, value: &Value, depth: usize) {
    indent_of(out, depth);
    match key {
        EntryKey::Str(s) => write_php_string(out, s),
        EntryKey::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
    }
    out.extend_from_slice(b" => ");
    match value {
        Value::Object(m) => {
            out.push(b'\n');
            indent_of(out, depth);
            out.extend_from_slice(b"array (\n");
            for (k, v) in m {
                write_entry(out, EntryKey::Str(k.as_str()), v, depth + 1);
            }
            indent_of(out, depth);
            out.extend_from_slice(b"),\n");
        }
        Value::Array(arr) => {
            out.push(b'\n');
            indent_of(out, depth);
            out.extend_from_slice(b"array (\n");
            for (i, v) in arr.iter().enumerate() {
                write_entry(out, EntryKey::Int(i as i64), v, depth + 1);
            }
            indent_of(out, depth);
            out.extend_from_slice(b"),\n");
        }
        scalar => {
            write_scalar(out, scalar);
            out.extend_from_slice(b",\n");
        }
    }
}

fn write_scalar(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.extend_from_slice(b"NULL"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => write_php_string(out, s),
        // Arrays/objects routed via write_entry; scalar callers pass scalars.
        _ => {}
    }
}

fn write_php_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'\'');
    for c in s.chars() {
        match c {
            '\'' => out.extend_from_slice(b"\\'"),
            '\\' => out.extend_from_slice(b"\\\\"),
            _ => {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                out.extend_from_slice(encoded.as_bytes());
            }
        }
    }
    out.push(b'\'');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn write_files(dir: &Path, root_extra: Value, packages: Vec<Value>) {
        let composer_json = json!({
            "name": "test/app",
            "extra": root_extra,
        });
        fs::create_dir_all(dir.join("vendor").join("composer")).unwrap();
        fs::write(
            dir.join("composer.json"),
            serde_json::to_vec(&composer_json).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("vendor/composer/installed.json"),
            serde_json::to_vec(&json!({ "packages": packages })).unwrap(),
        )
        .unwrap();
    }

    fn run(dir: &Path) -> (DiscoverReport, String) {
        let report = discover(DiscoverOptions {
            root: dir.to_path_buf(),
            vendor_dir: None,
            manifest_path: None,
        })
        .unwrap();
        let bytes = fs::read(&report.manifest_path).unwrap();
        let s = String::from_utf8(bytes).unwrap();
        (report, s)
    }

    #[test]
    fn renders_basic_manifest_byte_compatible_with_var_export() {
        let tmp = TempDir::new().unwrap();
        write_files(
            tmp.path(),
            json!({ "laravel": { "dont-discover": [] } }),
            vec![
                json!({
                    "name": "vendor/pkg-a",
                    "extra": {
                        "laravel": {
                            "providers": ["Vendor\\PkgA\\ServiceProvider"],
                        }
                    }
                }),
                json!({
                    "name": "vendor/pkg-b",
                    "extra": {
                        "laravel": {
                            "aliases": { "PkgB": "Vendor\\PkgB\\Facade" },
                            "providers": ["Vendor\\PkgB\\ServiceProvider"],
                        }
                    }
                }),
            ],
        );
        let (report, s) = run(tmp.path());
        assert_eq!(report.packages_in_manifest, 2);
        let expected = "<?php return array (\n\
              \x20\x20'vendor/pkg-a' => \n\
              \x20\x20array (\n\
              \x20\x20\x20\x20'providers' => \n\
              \x20\x20\x20\x20array (\n\
              \x20\x20\x20\x20\x20\x200 => 'Vendor\\\\PkgA\\\\ServiceProvider',\n\
              \x20\x20\x20\x20),\n\
              \x20\x20),\n\
              \x20\x20'vendor/pkg-b' => \n\
              \x20\x20array (\n\
              \x20\x20\x20\x20'aliases' => \n\
              \x20\x20\x20\x20array (\n\
              \x20\x20\x20\x20\x20\x20'PkgB' => 'Vendor\\\\PkgB\\\\Facade',\n\
              \x20\x20\x20\x20),\n\
              \x20\x20\x20\x20'providers' => \n\
              \x20\x20\x20\x20array (\n\
              \x20\x20\x20\x20\x20\x200 => 'Vendor\\\\PkgB\\\\ServiceProvider',\n\
              \x20\x20\x20\x20),\n\
              \x20\x20),\n\
              );";
        assert_eq!(s, expected);
    }

    #[test]
    fn dont_discover_wildcard_drops_everything() {
        let tmp = TempDir::new().unwrap();
        write_files(
            tmp.path(),
            json!({ "laravel": { "dont-discover": ["*"] } }),
            vec![
                json!({
                    "name": "vendor/pkg-a",
                    "extra": { "laravel": { "providers": ["X"] } }
                }),
                json!({
                    "name": "vendor/pkg-b",
                    "extra": { "laravel": { "providers": ["Y"] } }
                }),
            ],
        );
        let (report, s) = run(tmp.path());
        assert_eq!(report.packages_in_manifest, 0);
        assert_eq!(s, "<?php return array (\n);");
    }

    #[test]
    fn dont_discover_named_drops_named() {
        let tmp = TempDir::new().unwrap();
        write_files(
            tmp.path(),
            json!({ "laravel": { "dont-discover": ["vendor/pkg-a"] } }),
            vec![
                json!({
                    "name": "vendor/pkg-a",
                    "extra": { "laravel": { "providers": ["X"] } }
                }),
                json!({
                    "name": "vendor/pkg-b",
                    "extra": { "laravel": { "providers": ["Y"] } }
                }),
            ],
        );
        let (report, s) = run(tmp.path());
        assert_eq!(report.packages_in_manifest, 1);
        assert!(s.contains("'vendor/pkg-b'"));
        assert!(!s.contains("'vendor/pkg-a'"));
    }

    #[test]
    fn transitive_dont_discover_from_a_package_drops_another() {
        // pkg-a declares `dont-discover: [vendor/pkg-b]` — that should drop
        // pkg-b even though pkg-b is otherwise discoverable.
        let tmp = TempDir::new().unwrap();
        write_files(
            tmp.path(),
            json!({ "laravel": { "dont-discover": [] } }),
            vec![
                json!({
                    "name": "vendor/pkg-a",
                    "extra": {
                        "laravel": {
                            "dont-discover": ["vendor/pkg-b"],
                            "providers": ["A"],
                        }
                    }
                }),
                json!({
                    "name": "vendor/pkg-b",
                    "extra": { "laravel": { "providers": ["B"] } }
                }),
            ],
        );
        let (report, s) = run(tmp.path());
        assert_eq!(report.packages_in_manifest, 1);
        // pkg-a is the only top-level entry. pkg-b is not (note the trailing
        // ` => ` only appears on top-level package keys).
        assert!(s.contains("'vendor/pkg-a' => "));
        assert!(!s.contains("'vendor/pkg-b' => "));
        // PHP's PackageManifest doesn't strip `dont-discover` from the
        // rendered config — the literal "vendor/pkg-b" string still shows up
        // inside pkg-a's dont-discover list.
        assert!(s.contains("'vendor/pkg-b'"));
    }

    #[test]
    fn packages_with_no_extra_laravel_are_skipped() {
        let tmp = TempDir::new().unwrap();
        write_files(
            tmp.path(),
            json!({}),
            vec![
                json!({ "name": "vendor/pkg-a" }), // no extra at all
                json!({ "name": "vendor/pkg-b", "extra": {} }), // no laravel key
                json!({ "name": "vendor/pkg-c", "extra": { "laravel": {} } }), // empty
                json!({
                    "name": "vendor/pkg-d",
                    "extra": { "laravel": { "providers": ["D"] } }
                }),
            ],
        );
        let (report, s) = run(tmp.path());
        assert_eq!(report.packages_in_manifest, 1);
        assert!(s.contains("'vendor/pkg-d'"));
        assert!(!s.contains("'vendor/pkg-a'"));
        assert!(!s.contains("'vendor/pkg-b'"));
        assert!(!s.contains("'vendor/pkg-c'"));
    }

    #[test]
    fn empty_manifest_round_trips() {
        let tmp = TempDir::new().unwrap();
        write_files(tmp.path(), json!({}), vec![]);
        let (report, s) = run(tmp.path());
        assert_eq!(report.packages_in_manifest, 0);
        assert_eq!(s, "<?php return array (\n);");
    }

    #[test]
    fn vendor_dir_override_used_for_installed_json() {
        // base/composer.json + sibling-vendor/composer/installed.json layout
        // — mirrors monolith-app's `laravel-app/` + `monolith-app/vendor/`.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("laravel-app");
        let alt_vendor = tmp.path().join("vendor");
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("composer.json"),
            serde_json::to_vec(&json!({ "name": "test/laravel-app" })).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(alt_vendor.join("composer")).unwrap();
        fs::write(
            alt_vendor.join("composer/installed.json"),
            serde_json::to_vec(&json!({
                "packages": [
                    { "name": "vendor/x", "extra": { "laravel": { "providers": ["X"] } } }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let report = discover(DiscoverOptions {
            root: base.clone(),
            vendor_dir: Some(alt_vendor),
            manifest_path: None,
        })
        .unwrap();
        assert_eq!(report.packages_in_manifest, 1);
        assert_eq!(
            report.manifest_path,
            base.join("bootstrap/cache/packages.php")
        );
    }

    #[test]
    fn escapes_backslash_and_quote_in_strings() {
        let mut out = Vec::new();
        write_php_string(&mut out, "Foo\\Bar");
        assert_eq!(out, b"'Foo\\\\Bar'");
        let mut out = Vec::new();
        write_php_string(&mut out, "it's");
        assert_eq!(out, b"'it\\'s'");
    }
}
