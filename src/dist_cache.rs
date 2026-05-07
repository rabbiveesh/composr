//! On-disk cache of downloaded dist archives.
//!
//! We use Composer's cache key shape — `<package>/<sha1(url)>.<ext>` — so the
//! cache is conceptually shareable with Composer down the road. Path: under
//! `$XDG_CACHE_HOME/composr/files/` (or `~/.cache/...` fallback).
//!
//! Backwards compat: when no override is set and the new default is empty
//! but the old `~/.cache/autoload-dumpr/files/` exists with content, we
//! reuse it (avoids a re-download on rename day).

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DistCache {
    root: PathBuf,
}

impl DistCache {
    /// Resolve the cache directory.
    /// Override order:
    ///   1. `--cache-dir` (caller-provided)
    ///   2. `COMPOSR_CACHE_DIR` env (or legacy `AUTOLOAD_DUMPR_CACHE_DIR`)
    ///   3. `$XDG_CACHE_HOME/composr/files`, falling back to the legacy
    ///      `autoload-dumpr` path if it exists and the new one doesn't.
    ///   4. `$HOME/.cache/composr/files` (with the same legacy fallback).
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        if let Some(p) = explicit {
            return Ok(Self { root: p });
        }
        for var in ["COMPOSR_CACHE_DIR", "AUTOLOAD_DUMPR_CACHE_DIR"] {
            if let Ok(p) = std::env::var(var) {
                if !p.is_empty() {
                    return Ok(Self {
                        root: PathBuf::from(p),
                    });
                }
            }
        }
        let base = if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(xdg)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".cache")
        } else {
            return Err(anyhow::anyhow!(
                "could not determine cache directory: HOME and XDG_CACHE_HOME are both unset"
            ));
        };
        let new_root = base.join("composr").join("files");
        let legacy_root = base.join("autoload-dumpr").join("files");
        if !new_root.exists() && legacy_root.exists() {
            tracing::info!(
                "using legacy cache at {} (rename composr -> composer one-time-migrate)",
                legacy_root.display()
            );
            return Ok(Self { root: legacy_root });
        }
        Ok(Self { root: new_root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path where a (package, url, ext) triple should live in the cache.
    pub fn path_for(&self, package: &str, url: &str, ext: &str) -> PathBuf {
        let key = sha1_hex(url.as_bytes());
        self.root.join(package).join(format!("{key}.{ext}"))
    }

    pub fn ensure_parent(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        Ok(())
    }
}

pub fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn sha1_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
