//! `dump-autoload` core — same logic that used to live inline in main.rs,
//! lifted into a library function so `install` can call it after extracting
//! packages.

use crate::cache::{hash_composer_lock, Cache, CacheEntry};
use crate::config::ResolvedConfig;
use crate::emit::{
    render_classmap_php, render_static_classmap_body, replace_static_classmap, ClassMap,
};
use crate::filter::admit;
use crate::parse::{Declaration, PhpParser};
use crate::walk::{collect_files, PhpFile};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct DumpOptions {
    pub root: PathBuf,
    pub cache_path: Option<PathBuf>,
    pub with_dev: bool,
    pub no_cache: bool,
    pub dry_run: bool,
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
pub struct DumpStats {
    pub walked: usize,
    pub hits: usize,
    pub misses: usize,
    pub errors: usize,
    pub classes: usize,
    pub elapsed_ms: u128,
}

/// Compute the classmap without writing anything. Returned alongside the
/// ResolvedConfig + DumpStats so callers (the bootstrap path in install.rs,
/// in particular) can write a different shape of output without redoing the
/// expensive walk + parse work.
pub fn build_classmap(opts: &DumpOptions) -> Result<(ResolvedConfig, ClassMap, DumpStats)> {
    let started = Instant::now();
    let root = opts.root.clone();
    let cache_path = opts.cache_path.clone().unwrap_or_else(|| {
        root.join("vendor")
            .join("composer")
            .join(".classmap-cache.bin")
    });

    tracing::info!(?root, "loading composer config");
    let cfg = ResolvedConfig::load(&root, opts.with_dev)?;
    tracing::info!(packages = cfg.packages.len(), "config loaded");

    let walked: Vec<PhpFile> = cfg
        .packages
        .par_iter()
        .map(|pkg| collect_files(pkg).unwrap_or_default())
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });
    tracing::info!(files = walked.len(), "walk complete");

    let lock_hash = hash_composer_lock(&root);
    let mut cache = if opts.no_cache {
        Cache::empty(lock_hash)
    } else {
        Cache::load_or_empty(&cache_path, lock_hash)
    };

    let hits = AtomicUsize::new(0);
    let misses = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let new_entries: Mutex<Vec<(PathBuf, CacheEntry)>> = Mutex::new(Vec::new());
    let map = Mutex::new(ClassMap::new());

    let snapshot = std::mem::take(&mut cache.entries);

    walked.par_iter().for_each_init(PhpParser::new, |parser, pf| {
        let decls_owned: Vec<Declaration>;
        let decls: &Vec<Declaration> = match snapshot.get(&pf.path) {
            Some(entry) if entry.mtime_ns == pf.mtime_ns && entry.size == pf.size => {
                hits.fetch_add(1, Ordering::Relaxed);
                &entry.declarations
            }
            _ => {
                misses.fetch_add(1, Ordering::Relaxed);
                let bytes = match std::fs::read(&pf.path) {
                    Ok(b) => b,
                    Err(e) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("read failed {}: {e}", pf.path.display());
                        return;
                    }
                };
                decls_owned = parser.extract(&bytes);
                let entry = CacheEntry {
                    mtime_ns: pf.mtime_ns,
                    size: pf.size,
                    declarations: decls_owned.clone(),
                };
                new_entries.lock().unwrap().push((pf.path.clone(), entry));
                &decls_owned
            }
        };
        if decls.is_empty() {
            return;
        }
        let mut m = map.lock().unwrap();
        for d in decls {
            if !admit(d, &pf.path, &pf.source) {
                continue;
            }
            m.entry(d.fqcn.clone()).or_insert_with(|| pf.path.clone());
        }
    });

    let installed = cfg.vendor_dir.join("composer").join("InstalledVersions.php");
    if installed.is_file() {
        let mut m = map.lock().unwrap();
        m.entry(b"Composer\\InstalledVersions".to_vec())
            .or_insert(installed);
    }

    let walked_paths: std::collections::HashSet<_> =
        walked.iter().map(|f| f.path.clone()).collect();
    let mut entries = std::collections::HashMap::new();
    for (p, e) in snapshot {
        if walked_paths.contains(&p) {
            entries.insert(p, e);
        }
    }
    for (p, e) in new_entries.into_inner().unwrap() {
        entries.insert(p, e);
    }
    cache.entries = entries;

    let map = map.into_inner().unwrap();

    if !opts.no_cache {
        cache.save(&cache_path).ok();
    }

    let stats = DumpStats {
        walked: walked.len(),
        hits: hits.load(Ordering::Relaxed),
        misses: misses.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
        classes: map.len(),
        elapsed_ms: started.elapsed().as_millis(),
    };
    Ok((cfg, map, stats))
}

/// Standalone dump-autoload — patches the existing autoload_static.php and
/// rewrites autoload_classmap.php. Used for incremental updates where the
/// bootstrap files already exist (the fast path).
pub fn dump(opts: DumpOptions) -> Result<DumpStats> {
    let dry_run = opts.dry_run;
    let output_dir_override = opts.output_dir.clone();
    let (cfg, map, stats) = build_classmap(&opts)?;

    if !dry_run {
        let output_dir =
            output_dir_override.unwrap_or_else(|| cfg.vendor_dir.join("composer"));
        write_outputs(&cfg.root, &cfg.vendor_dir, &output_dir, &map)?;
    }

    Ok(stats)
}

fn write_outputs(
    root: &Path,
    vendor_dir: &Path,
    output_dir: &Path,
    map: &ClassMap,
) -> Result<()> {
    std::fs::create_dir_all(output_dir).ok();

    let classmap_php = render_classmap_php(map, root, vendor_dir);
    write_atomic_bytes(&output_dir.join("autoload_classmap.php"), &classmap_php)?;

    let real_static = vendor_dir.join("composer").join("autoload_static.php");
    let out_static = output_dir.join("autoload_static.php");
    if real_static.exists() {
        let existing = std::fs::read(&real_static)
            .with_context(|| format!("reading {}", real_static.display()))?;
        let body = render_static_classmap_body(map, root, vendor_dir);
        let updated = replace_static_classmap(&existing, &body)?;
        write_atomic_bytes(&out_static, &updated)?;
    } else {
        tracing::warn!(
            "{} does not exist; skipping static autoload update — \
             run `composer dump-autoload --no-scripts` once to bootstrap it",
            real_static.display()
        );
    }
    Ok(())
}

fn write_atomic_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("php.tmp");
    std::fs::write(&tmp, contents)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}
