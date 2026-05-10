//! Native replacement for `pestphp/pest-plugin`'s post-autoload-dump hook.
//!
//! `pestphp/pest-plugin`'s `Manager::registerPlugins` (subscribed via
//! `Composer\EventSubscriberInterface`) writes `vendor/pest-plugins.json`
//! by walking every installed package's (and the root's) `extra.pest.plugins`
//! array and dumping the union as `json_encode($plugins, JSON_PRETTY_PRINT)`.
//!
//! Pest's runtime `Loader` reads that file at startup
//! (`vendor/pestphp/pest-plugin/src/Loader.php`); when it's missing,
//! `Loader::getPluginInstances()` returns `[]` — so every Pest plugin
//! (`Pest\Plugins\Coverage`, `Bail`, `Cache`, `Retry`, `Snapshot`, `Parallel`,
//! `pest-plugin-arch`, `pest-plugin-profanity`, …) silently no-ops. That
//! makes the install-time hook load-bearing, not inert. composr replicates
//! it natively so we don't have to delegate the whole post-autoload-dump
//! event to composer just for this.
//!
//! Output is byte-identical to `Commands\DumpCommand::execute`'s
//! `file_put_contents(.., json_encode($plugins, JSON_PRETTY_PRINT))`.

use crate::lock::{Lock, LockedPackage};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Composer package name of the plugin we're replacing.
pub const PEST_PLUGIN_NAME: &str = "pestphp/pest-plugin";

/// Output filename inside `vendor/`.
pub const FILE_NAME: &str = "pest-plugins.json";

/// Whether `pestphp/pest-plugin` is in this lock under the chosen dev
/// filter (i.e. would actually get installed and activated by composer).
pub fn is_present(lock: &Lock, with_dev: bool) -> bool {
    lock.packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .any(|p| p.name == PEST_PLUGIN_NAME)
}

/// Whether the root composer.json's `config.allow-plugins` permits
/// `pestphp/pest-plugin`. Composer refuses to activate plugins that aren't
/// allowed; we should match that to avoid writing a file composer wouldn't
/// have written.
pub fn is_allowed(root_composer_json: &Value) -> bool {
    let allow = root_composer_json
        .get("config")
        .and_then(|c| c.get("allow-plugins"));
    let Some(allow) = allow else {
        return false;
    };
    if let Some(b) = allow.get(PEST_PLUGIN_NAME).and_then(|v| v.as_bool()) {
        return b;
    }
    // Composer also accepts a `"*": true` wildcard.
    allow.get("*").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Walk every installed package's `extra.pest.plugins` (alphabetical by
/// package name) then the root composer.json's, append-merging into a flat
/// list of class strings. Mirrors `Commands\DumpCommand::execute`.
pub fn collect(lock: &Lock, with_dev: bool, root_composer_json: &Value) -> Vec<String> {
    let mut packages: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = Vec::new();
    for p in packages {
        push_extra_pest_plugins(&p.raw, &mut out);
    }
    push_extra_pest_plugins(root_composer_json, &mut out);
    out
}

fn push_extra_pest_plugins(json: &Value, out: &mut Vec<String>) {
    let Some(arr) = json
        .get("extra")
        .and_then(|e| e.get("pest"))
        .and_then(|p| p.get("plugins"))
        .and_then(|p| p.as_array())
    else {
        return;
    };
    for v in arr {
        if let Some(s) = v.as_str() {
            out.push(s.to_string());
        }
    }
}

/// Encode `plugins` matching PHP's `json_encode($plugins, JSON_PRETTY_PRINT)`,
/// then atomically replace `vendor/pest-plugins.json`.
pub fn write(vendor_dir: &Path, plugins: &[String]) -> Result<PathBuf> {
    let path = vendor_dir.join(FILE_NAME);
    let payload: Value = plugins.iter().cloned().map(Value::String).collect();
    let body = crate::php_json::encode_pretty(&payload);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_from(packages: &[&str], packages_dev: &[&str]) -> Lock {
        let body = format!(
            r#"{{"packages":[{}],"packages-dev":[{}]}}"#,
            packages.join(","),
            packages_dev.join(",")
        );
        Lock::parse(body.as_bytes()).unwrap()
    }

    const NO_PLUGINS: &str = r#"{"name":"x/no-plugins","version":"1.0.0","type":"library"}"#;

    #[test]
    fn collect_orders_packages_alphabetically_root_last() {
        let lock = lock_from(
            &[
                r#"{"name":"a/zero","version":"1.0.0","type":"library","extra":{"pest":{"plugins":["A1","A2"]}}}"#,
                r#"{"name":"b/one","version":"1.0.0","type":"library","extra":{"pest":{"plugins":["B1"]}}}"#,
            ],
            &[],
        );
        let root = serde_json::json!({"extra":{"pest":{"plugins":["RootPlugin"]}}});
        assert_eq!(
            collect(&lock, true, &root),
            vec!["A1", "A2", "B1", "RootPlugin"]
        );
    }

    #[test]
    fn collect_filters_dev_packages_when_no_dev() {
        let lock = lock_from(
            &[r#"{"name":"a/x","version":"1","type":"library","extra":{"pest":{"plugins":["X1"]}}}"#],
            &[r#"{"name":"b/y","version":"1","type":"library","extra":{"pest":{"plugins":["Y1"]}}}"#],
        );
        let root = serde_json::json!({});
        assert_eq!(collect(&lock, false, &root), vec!["X1"]);
        assert_eq!(collect(&lock, true, &root), vec!["X1", "Y1"]);
    }

    #[test]
    fn collect_skips_packages_without_extra_pest_plugins() {
        let lock = lock_from(
            &[
                NO_PLUGINS,
                r#"{"name":"y/with","version":"1","type":"library","extra":{"pest":{"plugins":["Yes"]}}}"#,
            ],
            &[],
        );
        let root = serde_json::json!({});
        assert_eq!(collect(&lock, true, &root), vec!["Yes"]);
    }

    #[test]
    fn is_present_matches_dev_filter() {
        let lock = lock_from(
            &[NO_PLUGINS],
            &[r#"{"name":"pestphp/pest-plugin","version":"1","type":"composer-plugin"}"#],
        );
        assert!(!is_present(&lock, false));
        assert!(is_present(&lock, true));
    }

    #[test]
    fn is_allowed_reads_config_allow_plugins() {
        let yes = serde_json::json!({
            "config": {"allow-plugins": {"pestphp/pest-plugin": true}}
        });
        let no = serde_json::json!({
            "config": {"allow-plugins": {"pestphp/pest-plugin": false}}
        });
        let star = serde_json::json!({
            "config": {"allow-plugins": {"*": true}}
        });
        let absent = serde_json::json!({"config": {}});
        assert!(is_allowed(&yes));
        assert!(!is_allowed(&no));
        assert!(is_allowed(&star));
        assert!(!is_allowed(&absent));
    }

    #[test]
    fn write_emits_php_pretty_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            &[
                "Pest\\Plugins\\Cache".to_string(),
                "Pest\\Arch\\Plugin".to_string(),
            ],
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        // Verified against PHP 8.5:
        //   php -r 'echo json_encode(["Pest\\Plugins\\Cache","Pest\\Arch\\Plugin"], JSON_PRETTY_PRINT);'
        assert_eq!(
            body,
            "[\n    \"Pest\\\\Plugins\\\\Cache\",\n    \"Pest\\\\Arch\\\\Plugin\"\n]"
        );
    }

    #[test]
    fn write_emits_bare_brackets_for_empty_input() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), &[]).unwrap();
        // PHP: `json_encode([], JSON_PRETTY_PRINT)` => `[]`
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }
}
