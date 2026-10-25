//! Per-package classmap cache.
//!
//! Same idea as `dist_cache.rs` — content-addressed under
//! `$XDG_CACHE_HOME/composr/classmap/<package>/<sha1(reference)>.bin` —
//! but instead of dist archives we store the parsed PHP `Declaration`s
//! for each file in the package, keyed by relative path within the
//! package's install dir. Same `(package, reference)` ⇒ same code ⇒
//! same classmap, so a cache hit lets us skip the walk + parse for that
//! package entirely.
//!
//! Lives outside `vendor/` (unlike the per-file mtime cache in
//! `cache.rs`) so it survives `rm -rf vendor && composr install` — that
//! is the whole point: cold installs become near-free on the bootstrap
//! stage when the user has previously installed the same package
//! versions on this machine.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bump on shape change. We treat unknown versions as a cache miss.
///
/// The cached data is *post-admission* — i.e. the FQCN→file pairs that
/// survive `filter::admit` for this package's autoload config. So a
/// change to admission semantics (PSR-4/PSR-0 strictness, classmap
/// include rules) or to what the parser extracts (a tree-sitter-php
/// grammar bump) needs a version bump even though the on-disk shape
/// hasn't changed.
const CACHE_VERSION: u32 = 2;

/// One package's contribution to the classmap, in the smallest form
/// that lets us merge it into the global map without walking or
/// parsing any files.
#[derive(Debug, Serialize, Deserialize)]
pub struct PackageClassmap {
    pub version: u32,
    /// `(FQCN bytes, file relpath within the package install dir)`.
    /// FQCN is raw bytes because PHP identifiers may legally include
    /// non-UTF-8 (`Symfony\\<0xa9>` etc. — see `parse::Declaration`).
    /// Relpath rather than absolute so the cache survives the package
    /// being installed at a different absolute path next run.
    pub entries: Vec<(Vec<u8>, String)>,
}

impl PackageClassmap {
    pub fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            entries: Vec::new(),
        }
    }
}

impl Default for PackageClassmap {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolves where per-package classmap files live on disk. Same
/// override hierarchy as `DistCache`.
#[derive(Debug, Clone)]
pub struct ClassmapCache {
    root: PathBuf,
}

impl ClassmapCache {
    /// Resolve the cache directory.
    /// Override order:
    ///   1. caller-provided path (currently unused — added for symmetry
    ///      with `DistCache`)
    ///   2. `COMPOSR_CACHE_DIR` env (or legacy `AUTOLOAD_DUMPR_CACHE_DIR`)
    ///   3. `$XDG_CACHE_HOME/composr/classmap`
    ///   4. `$HOME/.cache/composr/classmap`
    ///
    /// Returns `None` if neither `HOME` nor `XDG_CACHE_HOME` are set —
    /// callers treat that as "caching disabled" and fall back to the
    /// existing parse path.
    pub fn discover(explicit: Option<PathBuf>) -> Option<Self> {
        if let Some(p) = explicit {
            return Some(Self { root: p });
        }
        for var in ["COMPOSR_CACHE_DIR", "AUTOLOAD_DUMPR_CACHE_DIR"] {
            if let Ok(p) = std::env::var(var) {
                if !p.is_empty() {
                    return Some(Self {
                        root: PathBuf::from(p).join("classmap"),
                    });
                }
            }
        }
        let base = if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(xdg)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".cache")
        } else {
            return None;
        };
        Some(Self {
            root: base.join("composr").join("classmap"),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path where a `(package, reference)` pair lives. We sha1 the
    /// reference both for filesystem hygiene (refs are usually git
    /// SHAs but could in theory be anything) and to mirror what
    /// `dist_cache` does for its archive paths.
    pub fn path_for(&self, package: &str, reference: &str) -> PathBuf {
        let key = sha1_hex(reference.as_bytes());
        self.root.join(package).join(format!("{key}.bin"))
    }

    /// Try to load a previously-cached classmap for one package. Any
    /// failure (missing file, bad bytes, version mismatch) returns
    /// `None` so the caller treats it as a miss and re-parses.
    pub fn load(&self, package: &str, reference: &str) -> Option<PackageClassmap> {
        let path = self.path_for(package, reference);
        let bytes = fs::read(&path).ok()?;
        let config = bincode::config::standard();
        let (entry, _) =
            bincode::serde::decode_from_slice::<PackageClassmap, _>(&bytes, config).ok()?;
        if entry.version != CACHE_VERSION {
            return None;
        }
        Some(entry)
    }

    /// Atomically write a package's classmap to the cache. Same
    /// pattern `cache.rs` uses: write to `<path>.tmp`, fsync, rename.
    /// Errors are logged but not propagated — the cache is best-effort
    /// and a write failure shouldn't fail the install.
    pub fn save(&self, package: &str, reference: &str, entry: &PackageClassmap) -> Result<()> {
        let path = self.path_for(package, reference);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let config = bincode::config::standard();
        let bytes =
            bincode::serde::encode_to_vec(entry, config).context("encoding classmap cache")?;
        let tmp = path.with_extension("bin.tmp");
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("creating cache tempfile {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("renaming cache into place at {}", path.display()))?;
        Ok(())
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    let digest = h.finalize();
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cache_in(tmp: &Path) -> ClassmapCache {
        ClassmapCache {
            root: tmp.to_path_buf(),
        }
    }

    #[test]
    fn round_trip_save_then_load() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let mut pkg = PackageClassmap::new();
        pkg.entries.push((b"Acme\\Foo".to_vec(), "src/Foo.php".to_string()));
        pkg.entries.push((b"Acme\\Bar".to_vec(), "src/Foo.php".to_string()));
        // Non-UTF-8 FQCN survives round-trip — Symfony's `<0xa9>` case.
        pkg.entries
            .push((vec![0xa9], "Traits/ValueWrapper.php".to_string()));
        cache.save("acme/lib", "abc123", &pkg).unwrap();
        let loaded = cache.load("acme/lib", "abc123").unwrap();
        assert_eq!(loaded.version, CACHE_VERSION);
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.entries[0].0, b"Acme\\Foo");
        assert_eq!(loaded.entries[0].1, "src/Foo.php");
        assert_eq!(loaded.entries[2].0, vec![0xa9]);
    }

    #[test]
    fn different_reference_is_a_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let pkg = PackageClassmap::new();
        cache.save("acme/lib", "ref-a", &pkg).unwrap();
        assert!(cache.load("acme/lib", "ref-b").is_none());
    }

    #[test]
    fn missing_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        assert!(cache.load("never/saved", "xyz").is_none());
    }

    #[test]
    fn version_mismatch_is_a_miss() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        // Hand-write a bin file with a bogus version.
        let bogus = PackageClassmap {
            version: CACHE_VERSION.wrapping_add(99),
            entries: Vec::new(),
        };
        let path = cache.path_for("acme/lib", "ref");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&bogus, config).unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(cache.load("acme/lib", "ref").is_none());
    }

    #[test]
    fn corrupt_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let path = cache.path_for("acme/lib", "ref");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not bincode").unwrap();
        assert!(cache.load("acme/lib", "ref").is_none());
    }

    #[test]
    fn path_layout_uses_sha1_of_reference() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let p = cache.path_for("acme/lib", "abc123");
        // sha1("abc123") = "6367c48dd193d56ea7b0baad25b19455e529f5ee"
        assert!(p.ends_with(
            "acme/lib/6367c48dd193d56ea7b0baad25b19455e529f5ee.bin"
        ));
    }

    #[test]
    fn discover_falls_back_to_xdg_then_home() {
        // Save and clear env so the test is deterministic.
        let saved_xdg = std::env::var_os("XDG_CACHE_HOME");
        let saved_composr = std::env::var_os("COMPOSR_CACHE_DIR");
        let saved_legacy = std::env::var_os("AUTOLOAD_DUMPR_CACHE_DIR");
        std::env::remove_var("COMPOSR_CACHE_DIR");
        std::env::remove_var("AUTOLOAD_DUMPR_CACHE_DIR");

        std::env::set_var("XDG_CACHE_HOME", "/tmp/test-xdg-cache");
        let c = ClassmapCache::discover(None).unwrap();
        assert_eq!(c.root(), Path::new("/tmp/test-xdg-cache/composr/classmap"));

        // Restore.
        std::env::remove_var("XDG_CACHE_HOME");
        if let Some(v) = saved_xdg {
            std::env::set_var("XDG_CACHE_HOME", v);
        }
        if let Some(v) = saved_composr {
            std::env::set_var("COMPOSR_CACHE_DIR", v);
        }
        if let Some(v) = saved_legacy {
            std::env::set_var("AUTOLOAD_DUMPR_CACHE_DIR", v);
        }
    }
}
