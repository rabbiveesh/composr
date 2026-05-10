use anyhow::{Context, Result};
use composr::discover::{discover as run_discover, DiscoverOptions};
use composr::dump::{dump as run_dump, DumpOptions};
use composr::install::{run as run_install, InstallOptions};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "composr",
    about = "Fast Rust install + dump-autoload for Composer projects (vowels not included)",
    version,
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
    /// Native Laravel `package:discover` — write
    /// bootstrap/cache/packages.php from vendor/composer/installed.json.
    Discover(DiscoverArgs),
    /// Sync vendor + classmap after a git op (post-checkout / post-merge /
    /// post-rewrite). Equivalent to `install --no-scripts` with a styled
    /// preamble line; always exits 0 so a failed sync never blocks the git
    /// op. Bails silently when there's no composer.json at cwd.
    #[command(name = "git-hook")]
    GitHook(GitHookArgs),
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
    /// Skip the native Laravel `package:discover` interception. With this
    /// flag, post-autoload-dump runs through composer (or the native runner)
    /// untouched.
    #[arg(long)]
    no_discover: bool,
}

#[derive(Parser, Debug)]
struct GitHookArgs {
    /// Label shown in the preamble (e.g. "post-checkout"). Helps identify
    /// which hook triggered the sync when scrolling back through terminal
    /// output.
    label: String,
    /// Extra positional args from the hook (`$1`, `$2`, …) — accepted for
    /// signature-compatibility with git's hook conventions but ignored.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _trailing: Vec<String>,
}

#[derive(Parser, Debug)]
struct DiscoverArgs {
    /// Override `vendor/` location. Defaults to `<root>/vendor`. Useful for
    /// project layouts where the Laravel base path lives inside a larger
    /// composer project (e.g. `laravel-app/` under a vendor at the outer
    /// root).
    #[arg(long)]
    vendor_dir: Option<PathBuf>,
    /// Output path for packages.php. Defaults to
    /// `<root>/bootstrap/cache/packages.php`.
    #[arg(long)]
    manifest_path: Option<PathBuf>,
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
                no_discover: args.no_discover,
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
        Some(Cmd::GitHook(args)) => Ok(run_git_hook(&root, &args.label)),
        Some(Cmd::Discover(args)) => {
            let report = run_discover(DiscoverOptions {
                root,
                vendor_dir: args.vendor_dir,
                manifest_path: args.manifest_path,
            })?;
            eprintln!(
                "discover: {} packages → {} ({}ms)",
                report.packages_in_manifest,
                report.manifest_path.display(),
                report.elapsed_ms
            );
            Ok(0)
        }
    }
}

/// Drive `composr install --no-scripts` for a git hook: print the styled
/// preamble line so the user knows the lag is intentional, then run install
/// and let its own one-line summary finish the output. Always returns 0 so
/// a failed sync never blocks the git op (matches the old shell helper's
/// `|| true` behavior).
fn run_git_hook(root: &std::path::Path, label: &str) -> i32 {
    use std::io::IsTerminal;
    if !root.join("composer.json").is_file() {
        return 0;
    }
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let (dim, cyan_b, reset) = if color {
        ("\x1b[2m", "\x1b[1;36m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    eprintln!(
        "{dim}[{label}]{reset}  {cyan_b}composr install{reset} {dim}(syncing vendor){reset}"
    );

    let concurrency = num_cpus::get().clamp(2, 12);
    match run_install(InstallOptions {
        root: root.to_path_buf(),
        dev: true,
        no_scripts: true,
        dry_run: false,
        concurrency,
        cache_dir: None,
        strict_plugins: false,
        no_composer: false,
        composer_bin: None,
        no_discover: false,
    }) {
        Ok(report) => {
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
        }
        Err(e) => {
            eprintln!("{dim}[{label}]{reset} install failed: {e:#} {dim}(continuing){reset}");
        }
    }
    0
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
