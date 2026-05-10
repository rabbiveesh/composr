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
use crate::binsetup;
use crate::bootstrap::{write_bootstrap, BootstrapInput};
use crate::composer_shim::{bootstrap_complete, entries_need_composer, Composer};
use crate::composr_config::ComposrConfig;
use crate::diff::{compute_ops, Op};
use crate::dist_cache::DistCache;
use crate::download::{DownloadOutcome, Downloader};
use crate::dump::{build_classmap, DumpOptions};
use crate::extract::{extract_zip_to_install_path, install_path_package};
use crate::installed::{
    build_installed_json, render_installed_php, write_installed_json,
    write_installed_versions_php, InstalledJson,
};
use crate::lock::{Lock, LockedPackage};
use crate::scripts::{Lifecycle, Runner, ScriptOutcome};
use anyhow::{anyhow, bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
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
    /// If true, never shell out to composer — use the pure-Rust path even
    /// when bootstrap files are missing or class-method scripts exist
    /// (those will be skipped with warnings, as in the pre-delegation v1).
    pub no_composer: bool,
    /// Override path to the composer binary. If unset, we look at
    /// COMPOSR_COMPOSER (or legacy AUTOLOAD_DUMPR_COMPOSER) and then PATH.
    pub composer_bin: Option<PathBuf>,
    /// If true, skip the native `package:discover` interception. Useful as
    /// an escape hatch if the heuristic mis-identifies a script entry.
    pub no_discover: bool,
}

#[derive(Debug)]
pub struct InstallReport {
    pub installed: usize,
    pub removed: usize,
    pub from_cache: usize,
    pub fetched: usize,
    pub elapsed_ms: u128,
    /// Number of times we shelled out to composer (bootstrap +
    /// class-method-script events). Useful for confirming the hot path
    /// stayed on the fast track for incremental installs.
    pub composer_calls: usize,
}

pub fn run(opts: InstallOptions) -> Result<InstallReport> {
    let started = Instant::now();
    // Per-stage timing breakdown to stderr when COMPOSR_TIMING is set in the
    // environment (any non-empty value). Useful when investigating where
    // cold-install time is going — see README.
    let timing = std::env::var("COMPOSR_TIMING")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
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

    // Plugin policy. Strict mode bails on any composer-plugin in the lock,
    // regardless of allowlists. In the default (relaxed) mode, classify each
    // plugin: known-inert (built-in [`INERT_ALLOWLIST`] or `composr.json`'s
    // `allow-inert-plugins`) and natively-replicated ([`NATIVELY_REPLICATED_PLUGINS`])
    // are silent. Anything else is "unknown" — its lifecycle subscribers may
    // be load-bearing, so we'll force a `composer run-script` delegation per
    // event when one is locked.
    let composr_cfg = ComposrConfig::load(&root);
    let plugins: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|p| p.package_type == "composer-plugin")
        .collect();
    if !plugins.is_empty() && opts.strict_plugins {
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        bail!(
            "composer-plugin packages present in lock and --strict-plugins is set: {}",
            names.join(", ")
        );
    }
    let unknown_plugins: Vec<String> =
        crate::composer_shim::unknown_plugins(&lock, &composr_cfg)
            .into_iter()
            .map(String::from)
            .collect();

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
            composer_calls: 0,
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

    // Locate composer once up front. If it isn't reachable we proceed
    // native-only (the user opted into hybrid mode but we can't satisfy it).
    let composer = if opts.no_composer {
        None
    } else {
        let c = Composer::locate(opts.composer_bin.as_deref());
        if c.is_none() {
            tracing::warn!(
                "no `composer` binary on PATH (set AUTOLOAD_DUMPR_COMPOSER or pass --composer-bin); \
                 running in pure-native mode — class-method scripts and missing autoload bootstrap files won't be handled"
            );
        }
        c
    };
    let mut composer_calls: usize = 0;
    let composer_env = composer_passthrough_env();

    // Surface the unknown-plugin policy now that we know whether composer
    // is reachable. With a composer binary, we'll delegate each lifecycle
    // event so unknown subscribers fire correctly. Without one, those
    // hooks won't run — the message has to reflect that.
    if !unknown_plugins.is_empty() {
        let joined = unknown_plugins.join(", ");
        if composer.is_some() {
            tracing::info!(
                "composer-plugin(s) with unknown install-time hooks: {joined}; \
                 lifecycle events will be delegated to `composer run-script`"
            );
        } else {
            tracing::warn!(
                "composer-plugin(s) with unknown install-time hooks: {joined}; \
                 their composer-side hooks won't run (no composer binary available). \
                 Add to `allow-inert-plugins` in composr.json to silence this warning."
            );
        }
    }

    let unknown_plugin_refs: Vec<&str> =
        unknown_plugins.iter().map(String::as_str).collect();

    if !opts.no_scripts {
        run_event(
            Lifecycle::PreInstallCmd,
            &runner,
            &scripts_section,
            composer.as_ref(),
            &root,
            &composer_env,
            &mut composer_calls,
            opts.no_discover,
            &unknown_plugin_refs,
        )?;
    }

    // Split installs by dist type. Only zip-dist packages need an HTTP
    // fetch; path-dist packages get symlinked from a local source dir.
    let (zip_installs, path_installs): (Vec<&LockedPackage>, Vec<&LockedPackage>) =
        installs.iter().copied().partition(|p| {
            p.dist
                .as_ref()
                .map(|d| d.kind != "path")
                .unwrap_or(true)
        });

    let cache = DistCache::discover(opts.cache_dir.clone())?;
    let auth = AuthConfig::load(&root)?;
    let t_download = Instant::now();
    let download_outcomes = if zip_installs.is_empty() {
        Vec::new()
    } else {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("starting tokio runtime")?;
        let downloader = Downloader::new(cache.clone(), auth.clone(), opts.concurrency)?;
        runtime
            .block_on(downloader.fetch_all(&zip_installs, true))
            .context("downloading dist archives")?
    };
    if timing {
        eprintln!("STAGE download: {}ms", t_download.elapsed().as_millis());
    }
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

    // Zip installs. Each package extracts to its own `install_path` and
    // writes proxies to a shared `vendor/bin/` — both extract and bin
    // installation are idempotent on shared dirs (mkdir -p style), so this
    // loop is safe to run in parallel. With a warm dist cache the work is
    // CPU+IO bound on zip decompression; rayon scales it across cores.
    //
    // Progress bar mirrors the download bar's style/template so the
    // visual signal is consistent across the install pipeline.
    // `ProgressBar` is `Send + Sync` (Arc-backed), so the rayon closure
    // can call `inc` / `set_message` on it without extra wrapping.
    let extract_pb = if !zip_installs.is_empty() {
        let pb = ProgressBar::new(zip_installs.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "  {bar:40.cyan/blue} {pos:>4}/{len:4} {msg}",
            )
            .unwrap(),
        );
        Some(pb)
    } else {
        None
    };

    // Sort by compressed size and split: huge packages run sequentially on
    // one thread (largest first), small packages fill the remaining threads
    // in parallel. This pipelines a single big package against the small
    // tail without letting multiple big extractions compete for memory + IO.
    // Threshold tunable via COMPOSR_HUGE_THRESHOLD_MB (default 5MB).
    //
    // Why this shape: rayon doesn't expose "is another thread currently in
    // a huge job?" mid-flight, so we enforce the constraint structurally —
    // one closure runs serially, the other parallel, and `rayon::join`
    // schedules both on the same pool concurrently.
    let huge_threshold_bytes: u64 = std::env::var("COMPOSR_HUGE_THRESHOLD_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5)
        * 1024
        * 1024;
    let sized: Vec<(&LockedPackage, u64)> = zip_installs
        .iter()
        .copied()
        .map(|pkg| {
            let size = cache_path_by_name
                .get(&pkg.name)
                .and_then(|o| std::fs::metadata(&o.cached_path).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            (pkg, size)
        })
        .collect();
    let (mut huge, small): (Vec<_>, Vec<_>) = sized
        .into_iter()
        .partition(|(_, s)| *s >= huge_threshold_bytes);
    huge.sort_by_key(|(_, s)| std::cmp::Reverse(*s));

    let extract_one = |pkg: &LockedPackage| -> Result<()> {
        let outcome = cache_path_by_name
            .get(&pkg.name)
            .ok_or_else(|| anyhow!("internal: no download outcome for {}", pkg.name))?;
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
        if let Some(pb) = &extract_pb {
            pb.inc(1);
            pb.set_message(format!("extracted {}", pkg.name));
        }
        Ok(())
    };

    // Only activate the serial-huge / parallel-small split when smalls
    // can actually keep the pool busy alongside the serial huge stream.
    // Heuristic: throttle iff `small.len() >= cores` (and there's a huge
    // to serialize). With fewer smalls than cores, the parallel arm would
    // drain in a fraction of huge wall time and leave most threads idle —
    // a plain `par_iter` over everything wins both makespan and simplicity.
    let cores = rayon::current_num_threads();
    let throttle = !huge.is_empty() && small.len() >= cores;
    let t_extract = Instant::now();
    if !throttle {
        huge.par_iter()
            .chain(small.par_iter())
            .try_for_each(|(pkg, _)| extract_one(pkg))?;
    } else {
        let (r_huge, r_small) = rayon::join(
            || -> Result<()> {
                for (pkg, _) in &huge {
                    extract_one(pkg)?;
                }
                Ok(())
            },
            || -> Result<()> {
                small
                    .par_iter()
                    .try_for_each(|(pkg, _)| extract_one(pkg))
            },
        );
        r_huge?;
        r_small?;
    }
    if let Some(pb) = extract_pb {
        pb.finish_and_clear();
    }
    if timing {
        eprintln!(
            "STAGE extract ({} zips): {}ms",
            zip_installs.len(),
            t_extract.elapsed().as_millis()
        );
    }

    // Path installs (local sibling dirs from `repositories[type=path]`).
    // Default to relative symlinks per Composer's PathDownloader.
    for pkg in &path_installs {
        let dist = pkg.dist.as_ref().ok_or_else(|| {
            anyhow!("path package {} has no dist entry", pkg.name)
        })?;
        let source_rel = Path::new(&dist.url);
        let source_abs: PathBuf = if source_rel.is_absolute() {
            source_rel.to_path_buf()
        } else {
            root.join(source_rel)
        };
        if !source_abs.exists() {
            bail!(
                "path package {}: source {} not found",
                pkg.name,
                source_abs.display()
            );
        }
        let install_path = vendor_dir.join(&pkg.name);
        let relative = pkg.transport_options.relative.unwrap_or(true);
        let symlink_pref = pkg.transport_options.symlink;
        install_path_package(&source_abs, &install_path, relative, symlink_pref)
            .with_context(|| format!("installing path package {}", pkg.name))?;
        binsetup::install_package_bins(
            &pkg.name,
            &install_path,
            &pkg.bin,
            &bin_dir,
            &vendor_dir,
        )?;
    }

    // Persist installed metadata.
    let t_metadata = Instant::now();
    let installed_payload = build_installed_json(&lock, opts.dev);
    write_installed_json(&installed_json_path, &installed_payload)?;
    // Composer's runtime InstalledVersions class — copied from composer's
    // own source. Composer normally writes this during `install` (not
    // dump-autoload), so even our delegate-to-composer path doesn't seed
    // it. Always written by us.
    write_installed_versions_php(&composer_dir.join("InstalledVersions.php"))?;

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
    if timing {
        eprintln!("STAGE metadata: {}ms", t_metadata.elapsed().as_millis());
    }

    let t_bootstrap = Instant::now();
    // Autoload generation. Two paths:
    //   - Bootstrap-needed (cold start — autoload.php / ClassLoader.php /
    //     etc. don't exist): generate the full bootstrap natively. We do
    //     the same expensive classmap walk as dump-autoload, but emit
    //     ALL the autoload_*.php files instead of just patching one.
    //   - Otherwise: take the fast path — patch the existing
    //     autoload_static.php and rewrite autoload_classmap.php.
    //
    // For pre-autoload-dump plugins we replicate natively (currently
    // tbachert/spi): build the classmap, run the plugin's codegen against
    // it, then write the autoload outputs with the generated class spliced
    // in. Both paths share the same shape so the SPI splice happens once.
    let needs_bootstrap = !bootstrap_complete(&vendor_dir);
    let dump_opts = DumpOptions {
        root: root.clone(),
        cache_path: None,
        with_dev: opts.dev,
        no_cache: false,
        dry_run: false,
        output_dir: None,
    };
    let (cfg, mut classmap, autoload_stats) =
        build_classmap(&dump_opts).context("computing classmap for autoload")?;

    // pre-autoload-dump native replication for tbachert/spi.
    if crate::spi::is_present(&lock, opts.dev) && crate::spi::is_allowed(&root_json) {
        let unsupported = crate::spi::packages_with_autoload_files_spi(&lock, opts.dev);
        if !unsupported.is_empty() {
            tracing::warn!(
                "skipping native SPI replication: packages use `extra.spi-config.autoload-files` \
                 (require PHP at install time): {}",
                unsupported.join(", ")
            );
        } else {
            let mappings = crate::spi::collect(&lock, opts.dev, &root_json);
            let body = crate::spi::render(&mappings, &lock, opts.dev, &classmap);
            let path = crate::spi::write(&vendor_dir, &body)
                .context("writing vendor/composer/GeneratedServiceProviderData.php")?;
            let (key, value) = crate::spi::classmap_entry(&vendor_dir);
            classmap.insert(key, value);
            tracing::info!(
                "wrote {} ({} services)",
                path.display(),
                mappings.services.len()
            );
        }
    }

    let autoload_mode = if needs_bootstrap {
        tracing::info!("autoload bootstrap missing — generating natively");
        let suffix = lock
            .content_hash
            .clone()
            .unwrap_or_else(|| crate::install::md5_hex_export(b"composr-fallback"));
        write_bootstrap(&BootstrapInput {
            root: &cfg.root,
            vendor_dir: &cfg.vendor_dir,
            packages: &cfg.packages,
            classmap: &classmap,
            suffix: &suffix,
        })
        .context("writing autoload bootstrap")?;
        "bootstrap"
    } else {
        crate::dump::write_outputs(
            &cfg.root,
            &cfg.vendor_dir,
            &cfg.vendor_dir.join("composer"),
            &classmap,
        )
        .context("writing autoload files (incremental)")?;
        "incremental"
    };

    // One-line user-visible summary, mirroring the install/discover lines.
    // Headline metrics: total classes, per-package classmap-cache hit rate
    // (the headline win when warm), per-file mtime-cache hit rate (the
    // root + uncacheable packages), and wall time.
    let pkg_total = autoload_stats.package_cache_hits + autoload_stats.package_cache_misses;
    let pkg_summary = if pkg_total > 0 {
        format!(
            "{}/{} pkg cached",
            autoload_stats.package_cache_hits, pkg_total
        )
    } else {
        "no cacheable pkgs".to_string()
    };
    println!(
        "  Autoload ({}): {} classes ({}, {} files reparsed, {}ms)",
        autoload_mode,
        autoload_stats.classes,
        pkg_summary,
        autoload_stats.misses,
        autoload_stats.elapsed_ms
    );
    if timing {
        eprintln!("STAGE bootstrap: {}ms", t_bootstrap.elapsed().as_millis());
    }

    let t_scripts = Instant::now();
    if !opts.no_scripts {
        run_event(
            Lifecycle::PostAutoloadDump,
            &runner,
            &scripts_section,
            composer.as_ref(),
            &root,
            &composer_env,
            &mut composer_calls,
            opts.no_discover,
            &unknown_plugin_refs,
        )?;

        // pestphp/pest-plugin's post-autoload-dump subscriber writes
        // vendor/pest-plugins.json from `extra.pest.plugins` across every
        // installed package. composr replicates that natively so we don't
        // have to delegate the whole event to composer just for it. Safe to
        // run after the event proper — no script in service-app's
        // post-autoload-dump consumes the file.
        if crate::pest_plugins::is_present(&lock, opts.dev)
            && crate::pest_plugins::is_allowed(&root_json)
        {
            let plugins = crate::pest_plugins::collect(&lock, opts.dev, &root_json);
            let path = crate::pest_plugins::write(&vendor_dir, &plugins)
                .context("writing vendor/pest-plugins.json")?;
            tracing::info!(
                "wrote {} ({} pest plugins)",
                path.display(),
                plugins.len()
            );
        }

        run_event(
            Lifecycle::PostInstallCmd,
            &runner,
            &scripts_section,
            composer.as_ref(),
            &root,
            &composer_env,
            &mut composer_calls,
            opts.no_discover,
            &unknown_plugin_refs,
        )?;
    }
    if timing {
        eprintln!("STAGE scripts: {}ms", t_scripts.elapsed().as_millis());
    }

    Ok(InstallReport {
        installed: installs.len(),
        removed: removes.len(),
        from_cache,
        fetched,
        elapsed_ms: started.elapsed().as_millis(),
        composer_calls,
    })
}

/// Run one lifecycle event. Delegates to `composer run-script <event>` when
/// composer is required (class-method handler in composer.json scripts, OR
/// any unknown composer-plugin in the lock that may have a subscriber for
/// this event). Otherwise uses our native runner.
///
/// The post-autoload-dump event additionally gets a Laravel-aware fast path:
/// when every entry is either a `package:discover` artisan invocation, the
/// `Illuminate\Foundation\ComposerScripts::postAutoloadDump` clearCompiled
/// handler, or a plain shell/php script, we run them all natively (replacing
/// `package:discover` with a millisecond-scale Rust port). This is what
/// turns the ~30s artisan call into ~5ms on monolith-app and skips the composer
/// delegation entirely. `--no-discover` opts out.
fn run_event(
    event: Lifecycle,
    runner: &Runner,
    scripts_section: &Value,
    composer: Option<&Composer>,
    cwd: &std::path::Path,
    env: &HashMap<String, String>,
    composer_calls: &mut usize,
    no_discover: bool,
    unknown_plugins: &[&str],
) -> Result<()> {
    let key = event.key();

    // If any composer-plugin in the lock isn't on a known list, its
    // `getSubscribedEvents` may register handlers for this event that we
    // can't dispatch natively. Force composer delegation to be safe — even
    // when composer.json has no scripts for this event (subscribers fire
    // anyway). When composer isn't reachable, fall through to native
    // dispatch with the warning that already fired in run().
    if !unknown_plugins.is_empty() {
        if let Some(c) = composer {
            tracing::info!(
                "delegating {key} to composer (unknown plugin subscribers may fire: {})",
                unknown_plugins.join(", ")
            );
            let outcome = c.run_script(cwd, env, key)?;
            *composer_calls += 1;
            tracing::info!(
                "composer run-script {key} completed in {}ms",
                outcome.elapsed_ms
            );
            return Ok(());
        }
    }

    let entries = match scripts_section.get(key) {
        Some(v) => v,
        None => return Ok(()),
    };

    if !no_discover && matches!(event, Lifecycle::PostAutoloadDump) {
        if let Some(result) = try_intercept_post_autoload_dump(runner, entries, cwd) {
            return result;
        }
    }

    if entries_need_composer(entries) {
        match composer {
            Some(c) => {
                tracing::info!("delegating {key} to composer (class-method handler present)");
                let outcome = c.run_script(cwd, env, key)?;
                *composer_calls += 1;
                tracing::info!(
                    "composer run-script {key} completed in {}ms",
                    outcome.elapsed_ms
                );
                return Ok(());
            }
            None => {
                tracing::warn!(
                    "{key} has class-method handler but no composer available; falling back to native runner (handler will be skipped)"
                );
            }
        }
    }
    runner.dispatch(event)?;
    Ok(())
}

/// One step in the planned native execution of post-autoload-dump.
enum PostAutoloadStep {
    /// Run native discover with this base path. The vendor dir is always
    /// the install pipeline's `<root>/vendor` (composer's CWD/vendor).
    Discover { base_path: PathBuf },
    /// Skip a known-safe class-method handler (currently just
    /// `Illuminate\Foundation\ComposerScripts::postAutoloadDump`).
    SkipKnownClassMethod(&'static str),
    /// Run the entry through the existing native script runner (shell, @php,
    /// @putenv, etc.).
    Runner(String),
}

/// Try to plan a fully-native execution of the post-autoload-dump event.
/// Returns `Some(_)` if every entry is classifiable; `None` if we hit an
/// unknown class-method handler we'd need composer for.
///
/// The return type is `Option<Result<()>>` so the caller can distinguish
/// "did not intercept" (`None`, fall through to existing logic) from
/// "intercepted, succeeded/failed" (`Some(Ok)`/`Some(Err)`).
fn try_intercept_post_autoload_dump(
    runner: &Runner,
    entries: &Value,
    root: &std::path::Path,
) -> Option<Result<()>> {
    let list: Vec<String> = match entries {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => return None,
    };

    // We only attempt interception when the event actually contains a
    // package:discover call — otherwise the existing path is fine.
    if !list
        .iter()
        .any(|s| parse_package_discover(s.trim(), root).is_some())
    {
        return None;
    }

    let mut plan: Vec<PostAutoloadStep> = Vec::with_capacity(list.len());
    for raw in &list {
        let trimmed = raw.trim();
        if trimmed == "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump" {
            plan.push(PostAutoloadStep::SkipKnownClassMethod(
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
            ));
            continue;
        }
        if let Some(base) = parse_package_discover(trimmed, root) {
            plan.push(PostAutoloadStep::Discover { base_path: base });
            continue;
        }
        // Unknown class-method handler — bail out, let composer handle it.
        if !trimmed.starts_with('@')
            && !trimmed.contains(char::is_whitespace)
            && trimmed.contains("::")
        {
            tracing::debug!(
                "post-autoload-dump has unrecognized class-method `{trimmed}` — won't intercept"
            );
            return None;
        }
        plan.push(PostAutoloadStep::Runner(raw.clone()));
    }

    Some(execute_post_autoload_plan(runner, plan, root))
}

fn execute_post_autoload_plan(
    runner: &Runner,
    plan: Vec<PostAutoloadStep>,
    root: &std::path::Path,
) -> Result<()> {
    let mut env_overrides: HashMap<String, String> = HashMap::new();
    let mut outcome = ScriptOutcome::default();
    let vendor_dir = root.join("vendor");
    for step in plan {
        match step {
            PostAutoloadStep::SkipKnownClassMethod(name) => {
                tracing::info!(
                    "post-autoload-dump: skipping {name} natively (clearCompiled is redundant — discover regenerates packages.php)"
                );
            }
            PostAutoloadStep::Discover { base_path } => {
                let report = crate::discover::discover(crate::discover::DiscoverOptions {
                    root: base_path.clone(),
                    vendor_dir: Some(vendor_dir.clone()),
                    manifest_path: None,
                })
                .with_context(|| {
                    format!(
                        "running native package:discover for base path {}",
                        base_path.display()
                    )
                })?;
                tracing::info!(
                    "native package:discover wrote {} packages to {} ({}ms)",
                    report.packages_in_manifest,
                    report.manifest_path.display(),
                    report.elapsed_ms
                );
            }
            PostAutoloadStep::Runner(raw) => {
                runner
                    .run_one(&raw, &mut env_overrides, &mut outcome)
                    .with_context(|| format!("script `{raw}` (event post-autoload-dump)"))?;
            }
        }
    }
    Ok(())
}

/// Recognize `@php <artisan-path> package:discover [args...]` and return the
/// directory containing `artisan` — Laravel's `$basePath`. Returns `None`
/// for any other entry shape.
fn parse_package_discover(entry: &str, root: &std::path::Path) -> Option<PathBuf> {
    let body = entry.strip_prefix("@php ")?.trim();
    let mut parts = body.split_whitespace();
    let artisan = parts.next()?;
    let command = parts.next()?;
    if command != "package:discover" {
        return None;
    }
    // Composer scripts run with CWD = project root, so a relative artisan
    // path resolves against `root`.
    let artisan_path: PathBuf = if Path::new(artisan).is_absolute() {
        PathBuf::from(artisan)
    } else {
        root.join(artisan)
    };
    let base = artisan_path.parent()?.to_path_buf();
    Some(base)
}

/// Build the env we hand to composer when delegating. Composer reads
/// `COMPOSER_AUTH`, `COMPOSER_HOME`, `COMPOSER_CACHE_DIR`, etc.; if those
/// were set in our process we want them propagated. We also synthesize
/// `COMPOSER_AUTH` from `GH_TOKEN`/`GITHUB_TOKEN` when the user gave us a
/// token but didn't construct the auth blob themselves.
fn composer_passthrough_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    for var in [
        "COMPOSER_AUTH",
        "COMPOSER_HOME",
        "COMPOSER_CACHE_DIR",
        "COMPOSER_PROCESS_TIMEOUT",
        "COMPOSER_DISABLE_NETWORK",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "PATH",
    ] {
        if let Ok(v) = std::env::var(var) {
            env.insert(var.to_string(), v);
        }
    }
    if !env.contains_key("COMPOSER_AUTH") {
        let token = env
            .get("GH_TOKEN")
            .or_else(|| env.get("GITHUB_TOKEN"))
            .cloned();
        if let Some(t) = token {
            if !t.is_empty() {
                env.insert(
                    "COMPOSER_AUTH".into(),
                    format!("{{\"github-oauth\":{{\"github.com\":\"{t}\"}}}}"),
                );
            }
        }
    }
    env
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
    if dir.is_symlink() {
        std::fs::remove_file(&dir)
            .with_context(|| format!("unlink {}", dir.display()))?;
    } else if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("rm -rf {}", dir.display()))?;
    }
    // Try to remove the now-empty vendor parent (e.g. vendor/acme).
    if let Some(parent) = dir.parent() {
        if parent != vendor_dir {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Ok(())
}

/// Compute the content-hash composer uses to compare composer.json against
/// composer.lock — byte-for-byte compatible with PHP's
/// `Composer\Package\Locker::getContentHash`:
///
/// ```php
/// $content = json_decode($composerFileContents, true);
/// $relevantKeys = [
///     'name', 'version', 'require', 'require-dev', 'conflict', 'replace',
///     'provide', 'minimum-stability', 'prefer-stable', 'repositories', 'extra',
/// ];
/// $relevantContent = [];
/// foreach (array_intersect($relevantKeys, array_keys($content)) as $key) {
///     $relevantContent[$key] = $content[$key];
/// }
/// if (isset($content['config']['platform'])) {
///     $relevantContent['config']['platform'] = $content['config']['platform'];
/// }
/// ksort($relevantContent);
/// return md5(JsonFile::encode($relevantContent, 0));
/// ```
///
/// Two divergences from `serde_json::to_string` we have to handle to match
/// PHP's `json_encode($x, JSON_THROW_ON_ERROR)` (i.e. options=0 — none of
/// `JSON_UNESCAPED_SLASHES`, `JSON_UNESCAPED_UNICODE`, `JSON_PRETTY_PRINT`):
///
///   1. `/` is escaped as `\/`.
///   2. Non-ASCII characters are escaped as `\uXXXX` (surrogate pairs for
///      supplementary-plane code points), not emitted as raw UTF-8.
///
/// Inner object key order mirrors the JSON file's order — `serde_json` with
/// the `preserve_order` feature already does this for us.
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

    // Build the subset, then sort top-level keys alphabetically (composer
    // calls `ksort` on the array right before encoding).
    let mut subset: Vec<(String, Value)> = Vec::new();
    if let Some(obj) = v.as_object() {
        for k in &relevant_keys {
            if let Some(val) = obj.get(*k) {
                subset.push(((*k).to_string(), val.clone()));
            }
        }
    }
    if let Some(platform) = v
        .get("config")
        .and_then(|c| c.get("platform"))
        .cloned()
    {
        let mut config_obj = serde_json::Map::new();
        config_obj.insert("platform".to_string(), platform);
        subset.push(("config".to_string(), Value::Object(config_obj)));
    }
    subset.sort_by(|a, b| a.0.cmp(&b.0));

    // Insertion order is preserved (preserve_order feature → IndexMap), so
    // iteration during encode also follows alphabetical order at the top.
    let mut ordered = serde_json::Map::new();
    for (k, val) in subset {
        ordered.insert(k, val);
    }

    let mut json = String::with_capacity(bytes.len());
    crate::php_json::encode_into(
        &Value::Object(ordered),
        &mut json,
        crate::php_json::EncodeOptions::default(),
    );
    Ok(format!("{:x}", md5_compute(json.as_bytes())))
}

/// Pure-Rust md5. We only use it for the content-hash check, which is
/// advisory.
fn md5_compute(input: &[u8]) -> Md5Digest {
    Md5Digest(md5_impl(input))
}

/// Hex-encoded md5. Public so other modules (bootstrap.rs, in particular)
/// can reuse it without reimplementing or pulling in another crate.
pub fn md5_hex_export(input: &[u8]) -> String {
    format!("{:x}", md5_compute(input))
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
    fn parse_package_discover_recognizes_artisan_invocations() {
        let root = Path::new("/proj");
        assert_eq!(
            parse_package_discover("@php artisan package:discover --ansi", root),
            Some(PathBuf::from("/proj"))
        );
        assert_eq!(
            parse_package_discover("@php laravel-app/artisan package:discover", root),
            Some(PathBuf::from("/proj/laravel-app"))
        );
        assert_eq!(
            parse_package_discover(
                "@php /abs/path/to/artisan package:discover --ansi",
                root
            ),
            Some(PathBuf::from("/abs/path/to"))
        );
    }

    #[test]
    fn parse_package_discover_rejects_other_entries() {
        let root = Path::new("/proj");
        // class-method handler
        assert_eq!(
            parse_package_discover(
                "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
                root
            ),
            None
        );
        // shell command
        assert_eq!(parse_package_discover("rm -f bootstrap/cache/*.php", root), None);
        // wrong artisan command
        assert_eq!(
            parse_package_discover("@php artisan migrate --force", root),
            None
        );
        // not @php
        assert_eq!(
            parse_package_discover("@composer dump-autoload", root),
            None
        );
    }

    /// Helper for hash tests: write `body` to a tempfile and compute the
    /// content-hash. Each fixture is hand-verifiable: take the expected
    /// PHP-encoded byte string in the assertion comment, pipe it through
    /// `md5` (or any md5sum tool), and you should get the asserted hex.
    fn hash_of_fixture(body: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("composer.json");
        std::fs::write(&path, body).unwrap();
        composer_json_content_hash(&path).unwrap()
    }

    #[test]
    fn content_hash_simple_name_and_require() {
        // Expected encoded bytes:  {"name":"a\/b","require":{"x\/y":"1"}}
        // printf '%s' '{"name":"a\/b","require":{"x\/y":"1"}}' | md5
        //   → 449f936eb3249b5f3a7b1fbd13173062
        let h = hash_of_fixture(r#"{"name":"a/b","require":{"x/y":"1"}}"#);
        assert_eq!(h, "449f936eb3249b5f3a7b1fbd13173062");
    }

    #[test]
    fn content_hash_ignores_non_relevant_keys() {
        // `description` and `authors` aren't in the relevant-keys subset
        // and must be dropped before encode. Expected encoded bytes:
        //   {"name":"a\/b"}
        // → 7f686f5492b90e6f7c1f5878e6f3e259
        let h = hash_of_fixture(
            r#"{"description":"ignored","authors":[{"name":"x"}],"name":"a/b"}"#,
        );
        assert_eq!(h, "7f686f5492b90e6f7c1f5878e6f3e259");
    }

    #[test]
    fn content_hash_ksorts_top_level_preserves_inner_order_escapes_unicode() {
        // The big one — exercises every non-trivial part of the algorithm:
        //
        //   * Top-level keys appear in JSON order `extra,name,require`,
        //     but composer ksorts them → output order alphabetical:
        //     `extra,name,require`. (It happens to already be alphabetical
        //     here; an explicit non-alphabetical input is in the next test.)
        //   * Inner `require` keys stay in the JSON file's declared order
        //     (`z` before `a`) — composer doesn't re-sort below the top level.
        //   * The non-ASCII `ñ` (U+00F1) is escaped as `ñ` lowercase.
        //
        // Expected encoded bytes:
        //   {"extra":{"k":"ñ"},"name":"a","require":{"z":"1","a":"2"}}
        // → bd4eeb8dd78f7a60fd458e459a17826a
        let h = hash_of_fixture(
            "{\"extra\":{\"k\":\"\u{00f1}\"},\"name\":\"a\",\"require\":{\"z\":\"1\",\"a\":\"2\"}}",
        );
        assert_eq!(h, "bd4eeb8dd78f7a60fd458e459a17826a");
    }

    #[test]
    fn content_hash_empty_object_becomes_empty_array() {
        // PHP-ism: `json_decode('{}', true)` produces an empty array,
        // and `json_encode([])` emits `[]`. So a `{"laravel": {}}` in
        // the source composer.json contributes `"laravel":[]` (not
        // `{}`) to the hashed bytes. Real-world: monolith-app's
        // `extra.laravel = {}` killed the original implementation.
        //
        // Expected encoded bytes:
        //   {"extra":{"laravel":[]},"name":"a\/b"}
        // → f4c9576fde00a3b42ca3ac725fb5b375
        let h = hash_of_fixture(r#"{"name":"a/b","extra":{"laravel":{}}}"#);
        assert_eq!(h, "f4c9576fde00a3b42ca3ac725fb5b375");
    }

    #[test]
    fn content_hash_ksort_reorders_non_alphabetical_top_level() {
        // Same payload as the previous test but with `name` first in the
        // input JSON. After ksort we still emit `extra,name,require` →
        // identical hash to the previous test.
        let h = hash_of_fixture(
            "{\"name\":\"a\",\"extra\":{\"k\":\"\u{00f1}\"},\"require\":{\"z\":\"1\",\"a\":\"2\"}}",
        );
        assert_eq!(h, "bd4eeb8dd78f7a60fd458e459a17826a");
    }

    #[test]
    fn md5_basic() {
        assert_eq!(format!("{:x}", md5_compute(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            format!("{:x}", md5_compute(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }
}

