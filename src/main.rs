use anyhow::{Context, Result};
use clap::Parser;
use composer_classmap_rs::{
    cache::{hash_composer_lock, Cache, CacheEntry},
    config::ResolvedConfig,
    emit::{render_classmap_php, render_static_classmap_body, replace_static_classmap, ClassMap},
    filter::admit,
    parse::PhpParser,
    walk::{collect_files, PhpFile},
};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "composer-classmap-rs",
    about = "Fast incremental Rust replacement for `composer dump-autoload -o`"
)]
struct Cli {
    /// Project root (defaults to cwd).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Cache file path (default: <root>/vendor/composer/.classmap-cache.bin).
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Skip autoload-dev entries.
    #[arg(long)]
    no_dev: bool,
    /// Hash file contents in addition to mtime+size (slower, paranoid).
    #[arg(long)]
    strict: bool,
    /// Bypass cache entirely — re-parse every file.
    #[arg(long)]
    no_cache: bool,
    /// Log warnings & per-file decisions.
    #[arg(long)]
    verbose: bool,
    /// Compute the map but don't write output files.
    #[arg(long)]
    dry_run: bool,
    /// Print { walked, hits, misses, errors, elapsed_ms } as JSON on stderr.
    #[arg(long)]
    emit_stats: bool,
    /// Override rayon thread count (default: num_cpus).
    #[arg(long)]
    threads: Option<usize>,
    /// Output directory for autoload_classmap.php / autoload_static.php
    /// (default: <root>/vendor/composer). Use this to write to a scratch
    /// dir for diffing against Composer's output without overwriting it.
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let exit = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(exit);
}

fn run(cli: Cli) -> Result<i32> {
    let started = Instant::now();

    let root = match &cli.root {
        Some(r) => r.clone(),
        None => std::env::current_dir().context("getting cwd")?,
    };
    let cache_path = cli.cache.clone().unwrap_or_else(|| {
        root.join("vendor")
            .join("composer")
            .join(".classmap-cache.bin")
    });

    if let Some(n) = cli.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok(); // fine if already initialized
    }

    tracing::info!(?root, "loading composer config");
    let cfg = ResolvedConfig::load(&root, !cli.no_dev)?;
    tracing::info!(packages = cfg.packages.len(), "config loaded");

    // Walk all packages in parallel (each package walks its own dirs).
    let walked: Vec<PhpFile> = cfg
        .packages
        .par_iter()
        .map(|pkg| collect_files(pkg).unwrap_or_default())
        .reduce(Vec::new, |mut a, b| {
            a.extend(b);
            a
        });
    tracing::info!(files = walked.len(), "walk complete");

    // Cache load.
    let lock_hash = hash_composer_lock(&root);
    let mut cache = if cli.no_cache {
        Cache::empty(lock_hash)
    } else {
        Cache::load_or_empty(&cache_path, lock_hash)
    };

    let hits = AtomicUsize::new(0);
    let misses = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);

    // Parse files. Cache hits return Vec<Declaration> directly. Misses parse
    // and stash a fresh entry in a thread-local accumulator we merge below.
    let new_entries: Mutex<Vec<(PathBuf, CacheEntry)>> = Mutex::new(Vec::new());

    let map = Mutex::new(ClassMap::new());

    // Snapshot the cache HashMap so workers can read it without locking.
    let snapshot = std::mem::take(&mut cache.entries);

    walked.par_iter().for_each_init(PhpParser::new, |parser, pf| {
        // Snapshot lookup vs. fresh parse. We keep an owned `Vec` alive for
        // the miss case via `decls_owned`; on hit we borrow from snapshot.
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
            // PSR-4/PSR-0 admission: Composer drops declarations whose FQCN
            // doesn't match the file's expected layout under the prefix.
            if !admit(d, &pf.path, &pf.source) {
                continue;
            }
            // Composer's behavior on duplicate FQCNs: warn, keep the *first*
            // path encountered. We mirror that.
            m.entry(d.fqcn.clone()).or_insert_with(|| pf.path.clone());
        }
    });

    // Composer always injects vendor/composer/InstalledVersions.php into the
    // classmap as `Composer\InstalledVersions`. The file lives outside any
    // package's autoload section, so the walker would otherwise skip it.
    let installed = cfg.vendor_dir.join("composer").join("InstalledVersions.php");
    if installed.is_file() {
        let mut m = map.lock().unwrap();
        m.entry("Composer\\InstalledVersions".to_string())
            .or_insert(installed);
    }

    // Rebuild cache entries: keep snapshot entries that still correspond to a
    // walked file, plus the new ones we parsed this run.
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

    if !cli.dry_run {
        let output_dir = cli
            .output_dir
            .clone()
            .unwrap_or_else(|| cfg.vendor_dir.join("composer"));
        write_outputs(&cfg.root, &cfg.vendor_dir, &output_dir, &map)?;
        if !cli.no_cache {
            cache.save(&cache_path)?;
        }
    }

    let stats = serde_json::json!({
        "walked": walked.len(),
        "hits": hits.load(Ordering::Relaxed),
        "misses": misses.load(Ordering::Relaxed),
        "errors": errors.load(Ordering::Relaxed),
        "classes": map.len(),
        "elapsed_ms": started.elapsed().as_millis(),
    });

    if cli.emit_stats {
        eprintln!("{stats}");
    } else if cli.verbose {
        eprintln!("{stats:#}");
    }

    if errors.load(Ordering::Relaxed) > 0 {
        Ok(2)
    } else {
        Ok(0)
    }
}

fn write_outputs(
    root: &Path,
    vendor_dir: &Path,
    output_dir: &Path,
    map: &ClassMap,
) -> Result<()> {
    std::fs::create_dir_all(output_dir).ok();

    // autoload_classmap.php — full rewrite.
    let classmap_php = render_classmap_php(map, root, vendor_dir);
    write_atomic(&output_dir.join("autoload_classmap.php"), &classmap_php)?;

    // autoload_static.php — surgical $classMap replacement. We always read
    // the *real* static.php from vendor/composer (the source of truth for
    // the file-include hashes, PSR-4 maps, and class-name suffix). The
    // result is then written to output_dir, which may equal the source dir
    // (in-place update) or be a scratch dir for diffing.
    let real_static = vendor_dir.join("composer").join("autoload_static.php");
    let out_static = output_dir.join("autoload_static.php");
    if real_static.exists() {
        let existing = std::fs::read_to_string(&real_static)
            .with_context(|| format!("reading {}", real_static.display()))?;
        let body = render_static_classmap_body(map, root, vendor_dir);
        let updated = replace_static_classmap(&existing, &body)?;
        write_atomic(&out_static, &updated)?;
    } else {
        tracing::warn!(
            "{} does not exist; skipping static autoload update — \
             run `composer dump-autoload --no-scripts` once to bootstrap it",
            real_static.display()
        );
    }
    Ok(())
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("php.tmp");
    std::fs::write(&tmp, contents)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = if verbose {
        EnvFilter::new("info")
    } else {
        EnvFilter::new("warn")
    };
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .compact()
        .init();
}

// Re-export Declaration so we can name it concisely in main.rs's closure.
use composer_classmap_rs::parse::Declaration;
