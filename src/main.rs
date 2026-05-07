use anyhow::{Context, Result};
use composr::dump::{dump as run_dump, DumpOptions};
use composr::install::{run as run_install, InstallOptions};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "composr",
    about = "Fast Rust install + dump-autoload for Composer projects (vowels not included)",
    arg_required_else_help = false
)]
struct Cli {
    /// Project root (defaults to cwd). Applies to both subcommands.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Log warnings & per-file decisions.
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate vendor/composer/autoload_classmap.php and patch
    /// autoload_static.php (the original behavior of this binary).
    DumpAutoload(DumpArgs),
    /// Install packages from composer.lock into vendor/.
    Install(InstallArgs),
}

#[derive(Parser, Debug)]
struct DumpArgs {
    #[arg(long)]
    cache: Option<PathBuf>,
    #[arg(long)]
    no_dev: bool,
    #[arg(long)]
    strict: bool,
    #[arg(long)]
    no_cache: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    emit_stats: bool,
    #[arg(long)]
    threads: Option<usize>,
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct InstallArgs {
    /// Skip dev dependencies (matches `composer install --no-dev`).
    #[arg(long)]
    no_dev: bool,
    /// Skip running composer scripts (pre/post-install-cmd, post-autoload-dump).
    #[arg(long)]
    no_scripts: bool,
    /// Print operations without changing vendor/.
    #[arg(long)]
    dry_run: bool,
    /// Number of parallel HTTP downloads. Defaults to ~CPU count, capped at 12.
    #[arg(long)]
    concurrency: Option<usize>,
    /// Override the dist cache directory.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Refuse to install if any composer-plugin is in the lock.
    #[arg(long)]
    strict_plugins: bool,
    /// Never shell out to composer — pure-native install. Class-method
    /// scripts and missing autoload bootstrap files won't be handled.
    #[arg(long)]
    no_composer: bool,
    /// Override the composer binary path used for delegation. If unset,
    /// COMPOSR_COMPOSER (or legacy AUTOLOAD_DUMPR_COMPOSER) and PATH are checked.
    #[arg(long)]
    composer_bin: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let exit = match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(exit);
}

fn dispatch(cli: Cli) -> Result<i32> {
    let root = match cli.root {
        Some(r) => r,
        None => std::env::current_dir().context("getting cwd")?,
    };
    match cli.command {
        // No subcommand → preserve old behavior (dump-autoload).
        None => {
            let stats = run_dump(DumpOptions {
                root,
                cache_path: None,
                with_dev: true,
                no_cache: false,
                dry_run: false,
                output_dir: None,
            })?;
            eprintln!(
                "dump-autoload: {} classes ({} walked, {} hits, {} misses, {}ms)",
                stats.classes, stats.walked, stats.hits, stats.misses, stats.elapsed_ms
            );
            Ok(if stats.errors > 0 { 2 } else { 0 })
        }
        Some(Cmd::DumpAutoload(args)) => {
            let started = Instant::now();
            let stats = run_dump(DumpOptions {
                root,
                cache_path: args.cache,
                with_dev: !args.no_dev,
                no_cache: args.no_cache,
                dry_run: args.dry_run,
                output_dir: args.output_dir,
            })?;
            if args.emit_stats {
                let blob = serde_json::json!({
                    "walked": stats.walked,
                    "hits": stats.hits,
                    "misses": stats.misses,
                    "errors": stats.errors,
                    "classes": stats.classes,
                    "elapsed_ms": stats.elapsed_ms,
                });
                eprintln!("{blob}");
            } else {
                eprintln!(
                    "dump-autoload: {} classes ({}ms total)",
                    stats.classes,
                    started.elapsed().as_millis()
                );
            }
            Ok(if stats.errors > 0 { 2 } else { 0 })
        }
        Some(Cmd::Install(args)) => {
            let concurrency = args.concurrency.unwrap_or_else(|| {
                let n = num_cpus::get().max(2);
                n.min(12)
            });
            let report = run_install(InstallOptions {
                root,
                dev: !args.no_dev,
                no_scripts: args.no_scripts,
                dry_run: args.dry_run,
                concurrency,
                cache_dir: args.cache_dir,
                strict_plugins: args.strict_plugins,
                no_composer: args.no_composer,
                composer_bin: args.composer_bin,
            })?;
            eprintln!(
                "install: {} installed, {} removed ({} cached, {} fetched) in {}ms ({} composer call{})",
                report.installed,
                report.removed,
                report.from_cache,
                report.fetched,
                report.elapsed_ms,
                report.composer_calls,
                if report.composer_calls == 1 { "" } else { "s" }
            );
            Ok(0)
        }
    }
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
