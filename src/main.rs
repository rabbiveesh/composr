use anyhow::{Context, Result};
use composr::discover::{discover as run_discover, DiscoverOptions};
use composr::dump::{dump as run_dump, DumpOptions};
use composr::bench_fs::{run as run_bench_fs, BenchOptions};
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
    /// Install post-checkout / post-merge / post-rewrite hooks into this
    /// repo's hooks dir so vendor + classmap stay in sync across branch
    /// switches, pulls, and rebases. Each hook just calls `composr git-hook`
    /// and exits 0 on its own, so a failed sync never blocks the git op.
    #[command(name = "install-hooks")]
    InstallHooks(InstallHooksArgs),
    /// Synthetic filesystem benchmark for picking a fast staging surface.
    /// Runs an extract-shaped workload (parallel mkdirs + small file
    /// writes + renames + rm -rf) against each given path and reports
    /// per-phase timings. Use it to compare candidate locations.
    #[command(name = "bench-fs")]
    BenchFs(BenchFsArgs),
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
struct InstallHooksArgs {
    /// Overwrite existing hook files even if composr didn't write them.
    /// Without this, a hook composr doesn't recognize is left untouched.
    #[arg(long)]
    force: bool,
}

#[derive(Parser, Debug)]
struct BenchFsArgs {
    /// One or more candidate directories to benchmark. Each gets its own
    /// row in the output table.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Number of synthetic packages (one parallel task per package).
    #[arg(long, default_value_t = 126)]
    packages: usize,
    /// Subdirectories per package.
    #[arg(long, default_value_t = 50)]
    dirs_per_pkg: usize,
    /// Files per subdirectory.
    #[arg(long, default_value_t = 18)]
    files_per_dir: usize,
    /// Bytes per file (constant payload).
    #[arg(long, default_value_t = 3 * 1024)]
    file_bytes: usize,
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
            composr::summary::print_install(&report, None);
            Ok(0)
        }
        Some(Cmd::GitHook(args)) => Ok(run_git_hook(&root, &args.label)),
        Some(Cmd::InstallHooks(args)) => run_install_hooks(&root, args.force),
        Some(Cmd::BenchFs(args)) => {
            run_bench_fs(BenchOptions {
                paths: args.paths,
                packages: args.packages,
                dirs_per_pkg: args.dirs_per_pkg,
                files_per_dir: args.files_per_dir,
                file_bytes: args.file_bytes,
            })?;
            Ok(0)
        }
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
        Ok(report) => composr::summary::print_install(&report, Some(label)),
        Err(e) => {
            eprintln!("{dim}[{label}]{reset} install failed: {e:#} {dim}(continuing){reset}");
        }
    }
    0
}

/// Marker line embedded in every hook composr writes. Lets a re-run (or a
/// future `install-hooks`) recognize its own files and overwrite them
/// silently, while leaving hooks composr didn't author untouched.
const HOOK_MARKER: &str = "# managed by composr install-hooks";

const POST_CHECKOUT_HOOK: &str = "#!/usr/bin/env bash\n\
# managed by composr install-hooks\n\
[ \"$3\" = \"1\" ] || exit 0\n\
command -v composr >/dev/null 2>&1 || exit 0\n\
composr git-hook post-checkout\n";

const POST_MERGE_HOOK: &str = "#!/usr/bin/env bash\n\
# managed by composr install-hooks\n\
command -v composr >/dev/null 2>&1 || exit 0\n\
composr git-hook post-merge\n";

const POST_REWRITE_HOOK: &str = "#!/usr/bin/env bash\n\
# managed by composr install-hooks\n\
cat >/dev/null     # drain stdin so we don't SIGPIPE the parent\n\
command -v composr >/dev/null 2>&1 || exit 0\n\
composr git-hook post-rewrite\n";

/// Write the three git hooks into the repo's hooks dir. Idempotent: a hook
/// composr previously wrote (carries `HOOK_MARKER`) is overwritten silently;
/// a foreign hook is skipped with a warning unless `--force` is given.
fn run_install_hooks(root: &std::path::Path, force: bool) -> Result<i32> {
    let hooks_dir = resolve_hooks_dir(root)?;
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("creating {}", hooks_dir.display()))?;

    let hooks = [
        ("post-checkout", POST_CHECKOUT_HOOK),
        ("post-merge", POST_MERGE_HOOK),
        ("post-rewrite", POST_REWRITE_HOOK),
    ];

    let mut wrote = 0;
    let mut skipped = 0;
    for (name, body) in hooks {
        let path = hooks_dir.join(name);
        if path.exists() && !force {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains(HOOK_MARKER) {
                eprintln!(
                    "skipped {name}: {} exists and wasn't written by composr (use --force to overwrite)",
                    path.display()
                );
                skipped += 1;
                continue;
            }
        }
        std::fs::write(&path, body)
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("setting +x on {}", path.display()))?;
        }
        println!("installed {}", path.display());
        wrote += 1;
    }

    if wrote > 0 {
        println!(
            "{wrote} hook(s) installed — vendor + classmap will sync on checkout/merge/rebase."
        );
    }
    // Exit non-zero only if every hook was skipped, so scripting can tell the
    // difference between "nothing to do" and "did nothing because conflicts".
    Ok(if wrote == 0 && skipped > 0 { 1 } else { 0 })
}

/// Resolve the hooks directory via git itself, so `core.hooksPath`, linked
/// worktrees, and submodules (where `.git` is a file, not a dir) all work.
fn resolve_hooks_dir(root: &std::path::Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .context("running `git rev-parse --git-path hooks` (is git installed?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "not a git repository at {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let p = PathBuf::from(&rel);
    Ok(if p.is_absolute() { p } else { root.join(p) })
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
