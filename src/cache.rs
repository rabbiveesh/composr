use crate::parse::Declaration;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bump on shape change, and whenever the parser's output can change (a
/// tree-sitter-php grammar bump), since entries cache `Declaration`s.
const CACHE_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub composer_lock_hash: [u8; 32],
    pub entries: HashMap<PathBuf, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub mtime_ns: u128,
    pub size: u64,
    pub declarations: Vec<Declaration>,
}

impl Cache {
    pub fn empty(lock_hash: [u8; 32]) -> Self {
        Cache {
            version: CACHE_VERSION,
            composer_lock_hash: lock_hash,
            entries: HashMap::new(),
        }
    }

    /// Load a cache from disk. If the file is missing, the version is wrong,
    /// or the lock-hash differs, return an empty cache (with the *current*
    /// lock hash so a successful run will fully repopulate it).
    pub fn load_or_empty(path: &Path, current_lock_hash: [u8; 32]) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Cache::empty(current_lock_hash);
        };
        let config = bincode::config::standard();
        match bincode::serde::decode_from_slice::<Cache, _>(&bytes, config) {
            Ok((c, _)) if c.version == CACHE_VERSION
                && c.composer_lock_hash == current_lock_hash =>
            {
                c
            }
            _ => Cache::empty(current_lock_hash),
        }
    }

    /// Atomically write the cache to disk: serialize to a sibling tempfile,
    /// fsync, then rename. Avoids torn writes if two runs race.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(self, config)
            .context("encoding cache")?;

        let tmp = path.with_extension("bin.tmp");
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("creating cache tempfile {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming cache into place at {}", path.display()))?;
        Ok(())
    }
}

/// Hash composer.lock so we can invalidate the entire cache on dependency
/// churn (vendor/ may have shifted underneath us). Returns zeros if the file
/// doesn't exist — that's fine; cache will simply re-validate against itself.
pub fn hash_composer_lock(root: &Path) -> [u8; 32] {
    let lock = root.join("composer.lock");
    let Ok(bytes) = fs::read(&lock) else {
        return [0u8; 32];
    };
    let mut h = Sha256::new();
    h.update(&bytes);
    h.finalize().into()
}
