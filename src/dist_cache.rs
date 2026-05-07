//! On-disk cache of downloaded dist archives.
//!
//! We use Composer's cache key shape — `<package>/<sha1(url)>.<ext>` — so the
//! cache is conceptually shareable with Composer down the road. Path: under
//! `$XDG_CACHE_HOME/autoload-dumpr/files/` (or `~/.cache/...` fallback).

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DistCache {
    root: PathBuf,
}

impl DistCache {
    /// Resolve the cache directory.
    /// Override order: `--cache-dir` flag (caller-provided) > `AUTOLOAD_DUMPR_CACHE_DIR`
    /// > `$XDG_CACHE_HOME/autoload-dumpr/files` > `$HOME/.cache/autoload-dumpr/files`.
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        if let Some(p) = explicit {
            return Ok(Self { root: p });
        }
        if let Ok(p) = std::env::var("AUTOLOAD_DUMPR_CACHE_DIR") {
            if !p.is_empty() {
                return Ok(Self {
                    root: PathBuf::from(p),
                });
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
        Ok(Self {
            root: base.join("autoload-dumpr").join("files"),
        })
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
