//! Selective delegation to a real `composer` binary on the user's PATH.
//!
//! Philosophy: we beat composer at the mechanical-and-parallelizable phases
//! (download, extract, classmap). For phases where composer's behavior is
//! the spec — autoload bootstrap-file generation, `Composer\Script\Event`
//! construction, plugin activation hooks — we shell out instead of porting.
//! Each call costs ~5–15s of composer startup, so we cluster them.
//!
//! Resolution order for which binary to run:
//!   1. `--composer-bin <path>` (caller-provided)
//!   2. `COMPOSR_COMPOSER` env var (or legacy `AUTOLOAD_DUMPR_COMPOSER`)
//!   3. `composer` on PATH
//!
//! If none is found, callers MUST treat delegation as a no-op (with a clear
//! warning).

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Composer {
    binary: PathBuf,
}

impl Composer {
    /// Locate composer. Returns `None` if no binary is reachable; callers
    /// should fall back to native handling and warn.
    pub fn locate(explicit: Option<&Path>) -> Option<Self> {
        if let Some(p) = explicit {
            if p.exists() {
                return Some(Self {
                    binary: p.to_path_buf(),
                });
            }
        }
        for var in ["COMPOSR_COMPOSER", "AUTOLOAD_DUMPR_COMPOSER"] {
            if let Ok(p) = std::env::var(var) {
                if !p.is_empty() {
                    let pb = PathBuf::from(p);
                    if pb.exists() {
                        return Some(Self { binary: pb });
                    }
                }
            }
        }
        if let Some(p) = which("composer") {
            return Some(Self { binary: p });
        }
        None
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Run `composer dump-autoload --no-scripts -o` from `cwd`. Composer
    /// rewrites every file under vendor/composer/ — bootstrap files included
    /// — so this single call covers the cold-start case AND triggers plugin
    /// activation hooks that ship install-time state (e.g. php-http/discovery).
    pub fn dump_autoload(&self, cwd: &Path, env: &HashMap<String, String>) -> Result<DelegateOutcome> {
        self.run(
            cwd,
            env,
            &["dump-autoload", "--no-scripts", "-o", "--no-interaction"],
            "dump-autoload",
        )
    }

    /// Run `composer run-script <event>` for one lifecycle event. composer's
    /// runner constructs a real `Composer\Script\Event` and dispatches via
    /// its EventDispatcher (which is the only correct way to invoke a
    /// class-method handler).
    pub fn run_script(
        &self,
        cwd: &Path,
        env: &HashMap<String, String>,
        event: &str,
    ) -> Result<DelegateOutcome> {
        self.run(
            cwd,
            env,
            &["run-script", event, "--no-interaction"],
            &format!("run-script {event}"),
        )
    }

    fn run(
        &self,
        cwd: &Path,
        env: &HashMap<String, String>,
        args: &[&str],
        label: &str,
    ) -> Result<DelegateOutcome> {
        let started = Instant::now();
        let mut cmd = Command::new(&self.binary);
        cmd.args(args).current_dir(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Composer's interactive prompts have to stay off — we're running
        // unattended.
        cmd.env("COMPOSER_NO_INTERACTION", "1");

        let status = cmd
            .status()
            .with_context(|| format!("spawning composer for {label}"))?;
        let elapsed_ms = started.elapsed().as_millis();
        if !status.success() {
            return Err(anyhow!(
                "composer {label} exited with {status} (after {elapsed_ms}ms)"
            ));
        }
        Ok(DelegateOutcome { label: label.into(), elapsed_ms })
    }
}

#[derive(Debug, Clone)]
pub struct DelegateOutcome {
    pub label: String,
    pub elapsed_ms: u128,
}

/// Minimal `which` — find the first occurrence of `name` in `$PATH`.
/// We don't pull in the `which` crate for this single use.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Decide whether the bootstrap autoload files (autoload.php, ClassLoader.php,
/// autoload_real.php, etc.) are present. If any are missing, our
/// in-place-patcher approach won't work — the install needs a composer
/// dump-autoload pass to seed them.
pub fn bootstrap_complete(vendor_dir: &Path) -> bool {
    let composer_dir = vendor_dir.join("composer");
    // The minimum-viable set for `require 'vendor/autoload.php'` to function.
    let required = [
        vendor_dir.join("autoload.php"),
        composer_dir.join("ClassLoader.php"),
        composer_dir.join("autoload_real.php"),
        composer_dir.join("autoload_static.php"),
        composer_dir.join("autoload_psr4.php"),
        composer_dir.join("autoload_classmap.php"),
        composer_dir.join("autoload_namespaces.php"),
        composer_dir.join("autoload_files.php"),
        composer_dir.join("InstalledVersions.php"),
    ];
    required.iter().all(|p| p.exists())
}

/// Composer-plugin packages whose activation only registers event
/// subscribers / runtime strategy lookups — installing their files is
/// enough; activation is safely a no-op for our purposes. Anything NOT on
/// this list is treated conservatively (warned about at install time, or
/// triggers composer delegation if activation matters). Project-local
/// extensions live in `composr.json` → `allow-inert-plugins`.
pub const INERT_ALLOWLIST: &[&str] = &[
    "pestphp/pest-plugin",
    "pestphp/pest-plugin-laravel",
    "pestphp/pest-plugin-mock",
    "pestphp/pest-plugin-arch",
    "pestphp/pest-plugin-type-coverage",
    // tbachert/spi: registers SPI providers at runtime, no install-time
    // file writes
    "tbachert/spi",
    "phpstan/extension-installer",
];

/// Returns true if any package in the lock declares a `composer-plugin` type
/// AND the plugin is one whose activation writes install-time state we can't
/// trivially reproduce. Conservative list — anything not on the explicit
/// "inert at install time" allowlist counts.
pub fn lock_needs_plugin_activation(lock: &crate::lock::Lock) -> bool {
    lock.packages
        .iter()
        .filter(|p| p.package_type == "composer-plugin")
        .any(|p| !INERT_ALLOWLIST.contains(&p.name.as_str()))
}

/// Returns true if any entry in `script_entries` is a class-method handler
/// (`Foo\Bar::baz`-shaped). Those need composer's EventDispatcher to run.
pub fn entries_need_composer(entries: &serde_json::Value) -> bool {
    let list: Vec<&str> = match entries {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => return false,
    };
    list.iter().any(|s| {
        let s = s.trim();
        // class::method, no spaces, no @-prefix
        !s.starts_with('@') && !s.contains(char::is_whitespace) && s.contains("::")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_class_method_handler() {
        let v = serde_json::json!([
            "@php artisan migrate",
            "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
            "rm -f bootstrap/cache/*.php"
        ]);
        assert!(entries_need_composer(&v));
    }

    #[test]
    fn pure_shell_event_doesnt_need_composer() {
        let v = serde_json::json!(["rm -rf cache", "@php artisan optimize"]);
        assert!(!entries_need_composer(&v));
    }

    #[test]
    fn empty_event_doesnt_need_composer() {
        assert!(!entries_need_composer(&serde_json::Value::Null));
        assert!(!entries_need_composer(&serde_json::json!([])));
    }

    #[test]
    fn bootstrap_detection() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!bootstrap_complete(tmp.path()));
        // Touch all required files; then it's complete.
        let composer_dir = tmp.path().join("composer");
        std::fs::create_dir_all(&composer_dir).unwrap();
        for name in [
            "ClassLoader.php",
            "autoload_real.php",
            "autoload_static.php",
            "autoload_psr4.php",
            "autoload_classmap.php",
            "autoload_namespaces.php",
            "autoload_files.php",
            "InstalledVersions.php",
        ] {
            std::fs::write(composer_dir.join(name), b"<?php").unwrap();
        }
        std::fs::write(tmp.path().join("autoload.php"), b"<?php").unwrap();
        assert!(bootstrap_complete(tmp.path()));
    }
}
