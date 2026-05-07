//! Top-level `install` orchestrator.
//!
//! Pipeline:
//!  1. Load composer.json (root) and composer.lock; check the lock matches.
//!  2. Read existing vendor/composer/installed.json.
//!  3. Compute install / remove operations.
//!  4. Run `pre-install-cmd` scripts.
//!  5. Concurrently fetch every dist zip we need.
//!  6. Apply removes (rm -rf old install paths and their bins).
//!  7. Apply installs (extract zip → install path; install bins).
//!  8. Write vendor/composer/installed.json + installed.php.
//!  9. Hand off to the existing dump-autoload code.
//! 10. Run `post-autoload-dump` then `post-install-cmd` scripts.
//!
//! Plugin handling: `composer-plugin` packages are installed as plain library
//! files (their PHP code lands in vendor/), but their composer-side
//! activation code is never invoked. For service-app's plugins (php-http/discovery,
//! pestphp/pest-plugin) the runtime parts continue to function.

use crate::auth::AuthConfig;
use crate::diff::{compute_ops, Op};
use crate::dist_cache::DistCache;
use crate::download::{DownloadOutcome, Downloader};
use crate::extract::extract_zip_to_install_path;
use crate::installed::{
    build_installed_json, render_installed_php, write_installed_json, InstalledJson,
};
use crate::lock::{Lock, LockedPackage};
use crate::scripts::{Lifecycle, Runner};
use crate::binsetup;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub root: PathBuf,
    pub dev: bool,
    pub no_scripts: bool,
    pub dry_run: bool,
    pub concurrency: usize,
    pub cache_dir: Option<PathBuf>,
    /// If true, abort if any composer-plugin is in the lock. Default: install
    /// them as inert libraries with a warning.
    pub strict_plugins: bool,
}

#[derive(Debug)]
pub struct InstallReport {
    pub installed: usize,
    pub removed: usize,
    pub from_cache: usize,
    pub fetched: usize,
    pub elapsed_ms: u128,
}

pub fn run(opts: InstallOptions) -> Result<InstallReport> {
    let started = Instant::now();
    let root = opts.root.canonicalize().with_context(|| {
        format!("canonicalizing project root {}", opts.root.display())
    })?;
    let composer_json = root.join("composer.json");
    let composer_lock = root.join("composer.lock");
    let vendor_dir = root.join("vendor");
    let composer_dir = vendor_dir.join("composer");
    let installed_json_path = composer_dir.join("installed.json");
    let installed_php_path = composer_dir.join("installed.php");
    let bin_dir = vendor_dir.join("bin");

    if !composer_lock.is_file() {
        bail!(
            "no composer.lock at {} — `install` is lock-driven; run an update first",
            composer_lock.display()
        );
    }

    let root_json: Value = if composer_json.is_file() {
        let bytes = std::fs::read(&composer_json)
            .with_context(|| format!("reading {}", composer_json.display()))?;
        serde_json::from_slice(&bytes).context("parsing composer.json")?
    } else {
        Value::Object(Default::default())
    };
    let root_name = root_json
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("__root__")
        .to_string();
    let root_type = root_json
        .get("type")
        .and_then(|x| x.as_str())
        .unwrap_or("library")
        .to_string();

    let lock = Lock::read(&composer_lock)?;
    if let Some(content_hash) = &lock.content_hash {
        let computed = composer_json_content_hash(&composer_json)?;
        if &computed != content_hash {
            tracing::warn!(
                "composer.lock content-hash does not match composer.json (got {computed}, lock says {content_hash}); proceeding but lock may be stale"
            );
        }
    }

    // Plugin policy. Even with the relaxed default, surface the plugin names
    // up front so the user knows which packages are inert.
    let plugins: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|p| p.package_type == "composer-plugin")
        .collect();
    if !plugins.is_empty() {
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        if opts.strict_plugins {
            bail!(
                "composer-plugin packages present in lock and --strict-plugins is set: {}",
                names.join(", ")
            );
        }
        tracing::warn!(
            "installing composer-plugin packages as inert files (their composer-side hooks won't run): {}",
            names.join(", ")
        );
    }

    let installed = InstalledJson::read(&installed_json_path)?;
    let ops = compute_ops(&lock, &installed, opts.dev);

    let removes: Vec<&Op> = ops.iter().filter(|o| matches!(o, Op::Remove { .. })).collect();
    let installs: Vec<&LockedPackage> = ops
        .iter()
        .filter_map(|o| match o {
            Op::Install(p) => Some(*p),
            _ => None,
        })
        .collect();

    println!(
        "  Install ops: {} install, {} remove ({})",
        installs.len(),
        removes.len(),
        if opts.dev { "with dev" } else { "no dev" }
    );

    if opts.dry_run {
        for op in &ops {
            match op {
                Op::Install(p) => println!("  + {} {}", p.name, p.version),
                Op::Remove { name } => println!("  - {name}"),
            }
        }
        return Ok(InstallReport {
            installed: 0,
            removed: 0,
            from_cache: 0,
            fetched: 0,
            elapsed_ms: started.elapsed().as_millis(),
        });
    }

    let scripts_section = root_json
        .get("scripts")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let runner = Runner {
        project_root: &root,
        vendor_bin_dir: &bin_dir,
        scripts: &scripts_section,
    };

    if !opts.no_scripts {
        runner.dispatch(Lifecycle::PreInstallCmd)?;
    }

    // Async download phase.
    let cache = DistCache::discover(opts.cache_dir.clone())?;
    let auth = AuthConfig::load(&root)?;
    let download_outcomes = if installs.is_empty() {
        Vec::new()
    } else {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("starting tokio runtime")?;
        let downloader = Downloader::new(cache.clone(), auth.clone(), opts.concurrency)?;
        runtime
            .block_on(downloader.fetch_all(&installs, true))
            .context("downloading dist archives")?
    };
    let cache_path_by_name: HashMap<String, &DownloadOutcome> = download_outcomes
        .iter()
        .map(|o| (o.package_name.clone(), o))
        .collect();
    let from_cache = download_outcomes.iter().filter(|o| o.from_cache).count();
    let fetched = download_outcomes.len() - from_cache;

    // Removals first so install paths are clear before extraction.
    for op in &removes {
        if let Op::Remove { name } = op {
            apply_remove(name, &installed, &vendor_dir, &bin_dir)?;
        }
    }

    // Installs.
    for pkg in &installs {
        let outcome = cache_path_by_name.get(&pkg.name).ok_or_else(|| {
            anyhow!("internal: no download outcome for {}", pkg.name)
        })?;
        let install_path = vendor_dir.join(&pkg.name);
        extract_zip_to_install_path(&outcome.cached_path, &install_path, &vendor_dir)
            .with_context(|| format!("extracting {}", pkg.name))?;
        binsetup::install_package_bins(
            &pkg.name,
            &install_path,
            &pkg.bin,
            &bin_dir,
            &vendor_dir,
        )?;
    }

    // Persist installed metadata.
    let installed_payload = build_installed_json(&lock, opts.dev);
    write_installed_json(&installed_json_path, &installed_payload)?;

    let root_version_pretty = root_json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("dev-master")
        .to_string();
    let root_reference = root_reference_for(&root)?;
    let php = render_installed_php(
        &lock,
        opts.dev,
        &root_name,
        &root_version_pretty,
        &root_reference,
        &root_type,
        &root,
        &composer_dir,
    );
    let installed_php_tmp = installed_php_path.with_extension("php.tmp");
    std::fs::write(&installed_php_tmp, php)
        .with_context(|| format!("writing {}", installed_php_tmp.display()))?;
    std::fs::rename(&installed_php_tmp, &installed_php_path)
        .with_context(|| format!("renaming into {}", installed_php_path.display()))?;

    // Hand off to the existing dump-autoload pipeline. We invoke the same
    // entry points so the cache, parser, and emit logic stay shared.
    crate::dump_autoload(&root, opts.dev)
        .context("running dump-autoload after install")?;

    if !opts.no_scripts {
        runner.dispatch(Lifecycle::PostAutoloadDump)?;
        runner.dispatch(Lifecycle::PostInstallCmd)?;
    }

    Ok(InstallReport {
        installed: installs.len(),
        removed: removes.len(),
        from_cache,
        fetched,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn apply_remove(
    name: &str,
    installed: &InstalledJson,
    vendor_dir: &Path,
    bin_dir: &Path,
) -> Result<()> {
    // Remove the bin proxies for the package about to disappear.
    let bins: Vec<String> = installed
        .packages
        .iter()
        .find(|p| p.get("name").and_then(|x| x.as_str()) == Some(name))
        .and_then(|p| p.get("bin"))
        .and_then(|b| b.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    binsetup::remove_package_bins(&bins, bin_dir)?;

    let dir = vendor_dir.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("rm -rf {}", dir.display()))?;
        // Try to remove the now-empty vendor parent (e.g. vendor/acme).
        if let Some(parent) = dir.parent() {
            if parent != vendor_dir {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    Ok(())
}

/// Compute the content-hash composer uses to compare composer.json against
/// composer.lock. Composer's algorithm: take a stable subset of fields
/// (require, require-dev, conflict, replace, provide, minimum-stability,
/// prefer-stable, repositories, extra), JSON-encode them with
/// JSON_UNESCAPED_SLASHES + JSON_UNESCAPED_UNICODE, then md5.
fn composer_json_content_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let v: Value = serde_json::from_slice(&bytes).context("parsing composer.json")?;
    let relevant_keys = [
        "name",
        "version",
        "require",
        "require-dev",
        "conflict",
        "replace",
        "provide",
        "minimum-stability",
        "prefer-stable",
        "repositories",
        "extra",
    ];
    let mut subset = serde_json::Map::new();
    if let Some(obj) = v.as_object() {
        for k in &relevant_keys {
            if let Some(val) = obj.get(*k) {
                subset.insert((*k).to_string(), val.clone());
            }
        }
    }
    // PHP's JSON_UNESCAPED_SLASHES + JSON_UNESCAPED_UNICODE — serde_json
    // matches both by default for non-ASCII; slashes also aren't escaped.
    // JSON object key order: we mirror Composer by emitting keys in the
    // order they appeared above (they're stable).
    let mut ordered = serde_json::Map::new();
    for k in &relevant_keys {
        if let Some(val) = subset.remove(*k) {
            ordered.insert((*k).to_string(), val);
        }
    }
    let json = serde_json::to_string(&Value::Object(ordered))
        .context("serializing composer.json subset")?;
    Ok(format!("{:x}", md5_compute(json.as_bytes())))
}

/// Pure-Rust md5. We only use it for the content-hash check, which is
/// advisory.
fn md5_compute(input: &[u8]) -> Md5Digest {
    Md5Digest(md5_impl(input))
}

struct Md5Digest([u8; 16]);

impl std::fmt::LowerHex for Md5Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0.iter() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

// Tiny MD5 — public domain reference implementation. We avoid pulling a
// crate just for one digest.
fn md5_impl(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, w) in chunk.chunks_exact(4).enumerate() {
            m[i] = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

/// Resolve the project's git HEAD reference for installed.php. Falls back to
/// an empty string if not in a git repo or git is unavailable.
fn root_reference_for(root: &Path) -> Result<String> {
    let head = root.join(".git").join("HEAD");
    if !head.exists() {
        return Ok(String::new());
    }
    let body = std::fs::read_to_string(&head).unwrap_or_default();
    if let Some(rest) = body.strip_prefix("ref: ") {
        let ref_path = root.join(".git").join(rest.trim());
        if let Ok(sha) = std::fs::read_to_string(&ref_path) {
            return Ok(sha.trim().to_string());
        }
    }
    Ok(body.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_basic() {
        assert_eq!(format!("{:x}", md5_compute(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            format!("{:x}", md5_compute(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }
}

