# composr

A Rust replacement for the slow parts of Composer-based PHP installs (vowels
not included). Targets the cold-start install path — the bit where `composer
install` chews through dist downloads, autoload bootstrap, and Laravel's
`package:discover` for half a minute or more.

## Status

Working end-to-end on real apps. Two production-shape Laravel codebases
(`service-app` and `monolith-app`) install cleanly with composr in
place of `composer install`, with output (vendor tree, autoload bootstrap,
packages.php) that's byte-equivalent to composer's own.

Latest measured on monolith-app (292 packages):

|                                   | composer | composr |
|-----------------------------------|---------:|--------:|
| Cold-cold (no caches at all)      |    ~72s  |    ~33s |
| Warm dist + classmap cache, wiped vendor | — |    ~16s |
| Warm everything (touch one app file) |   —   |  ~600ms |
| `package:discover` step           |    ~30s  |    ~5ms |
| Composer subprocess calls         |        — |       0 |

Zero composer subprocess calls means the post-autoload-dump event is being
fully handled natively (see "Hybrid mode" below).

Per-stage breakdown of a single install run (set `COMPOSR_TIMING=1` to
print these to stderr yourself). Two snapshots:

```
# Cold-cold: no vendor, no classmap cache, dist cache warm
STAGE download: 2ms
STAGE extract (289 zips): ~14s   # zip extraction, parallel via rayon
STAGE metadata: 9ms
STAGE bootstrap: ~20s            # walk + parse + write per-package cache
STAGE scripts: 30ms

# After the first run: vendor wiped, classmap cache populated
STAGE download: 5ms
STAGE extract (289 zips): ~14s
STAGE metadata: 9ms
STAGE bootstrap: ~2.3s           # 292 cache hits, no walk, no parse
STAGE scripts: 35ms

# Warm everything (re-run, nothing or one app file changed)
STAGE bootstrap: ~600ms          # only the root package walked
```

Two layers of classmap caching make the difference:

- **Per-package shared cache** at `$XDG_CACHE_HOME/composr/classmap/<package>/<sha1(reference)>.bin`,
  keyed by `(package_name, dist.reference)`. Survives `rm -rf vendor`. A
  hit means we skip walk + parse + admission for that package entirely
  and merge cached classmap entries straight into the global map.
- **Per-file mtime cache** at `vendor/composer/.classmap-cache.bin` for
  the root package and any uncacheable packages (path-installs, no
  reference). Same `(mtime_ns, size)` fingerprint as before.

Together they make the "I changed one app file, run install" loop a
sub-second operation, while still being byte-identical to composer's
classmap output on cache hits and misses.

Remaining wall-clock on cold-cold is dominated by zip extraction (~14s,
parallel via rayon — bound by disk metadata throughput / largest single
archive on M-series Macs). Classmap walking is no longer on the
critical path after the first run.

## Subcommands

```
composr install            # lock-driven install: download → extract → autoload bootstrap → scripts
composr dump-autoload      # rewrite vendor/composer/autoload_classmap.php (and patch autoload_static.php)
composr discover           # rewrite bootstrap/cache/packages.php (Laravel's package:discover)
composr                    # alias for `dump-autoload` (back-compat with the original tool name)
```

`composr install` is the one-stop shop. The others are mostly useful for
debugging or for users who only want one piece.

### What `composr install` does

1. Reads `composer.lock`; checks the content-hash against `composer.json`
   (advisory).
2. Reads `vendor/composer/installed.json`, computes diff: install / remove
   ops.
3. Concurrent HTTP fetch of every dist archive we don't already have
   cached. Cache lives at `$XDG_CACHE_HOME/composr/files` (override with
   `COMPOSR_CACHE_DIR` or `--cache-dir`).
4. Removes (rm -rf old install paths + their bin proxies).
5. Extracts each zip to its install path; sets up vendor/bin proxies.
   `dist.type == "path"` packages get symlinked from sibling repos.
6. Writes `vendor/composer/installed.json`, `installed.php`, and
   `InstalledVersions.php` (the latter bundled MIT from composer/composer).
7. Generates the autoload bootstrap natively when missing
   (`autoload.php`, `ClassLoader.php`, `autoload_real.php`,
   `autoload_static.php`, the four data files, `platform_check.php`,
   `LICENSE`). When the bootstrap is already in place, takes the fast
   path and just patches the classmap.
8. Runs script lifecycle events (`pre-install-cmd`, `post-autoload-dump`,
   `post-install-cmd`) — see "Hybrid mode" for how class-method handlers
   are dispatched.

### What `composr discover` does

Native port of `Illuminate\Foundation\PackageManifest::build`. Reads
`vendor/composer/installed.json` and the root `composer.json`'s
`extra.laravel.dont-discover`, accumulates transitive opt-outs, drops
empty configs, and writes `bootstrap/cache/packages.php` byte-compatible
with PHP's `var_export`. Used internally by `composr install`'s
post-autoload-dump path; also runnable standalone.

```
composr discover --root <project> [--vendor-dir <p>] [--manifest-path <p>]
```

`--vendor-dir` is for layouts where the Laravel base path lives inside a
larger composer project (e.g. monolith-app's `laravel-app/` with a
sibling `vendor/`).

## Hybrid mode

Composer's `EventDispatcher` is the only correct way to invoke
class-method script handlers (`Foo\Bar::baz`-shaped entries in
`composer.json` scripts). composr does not reimplement it. Instead, when
a script event contains a class-method handler, composr shells out to
a real `composer run-script <event>` for that one event.

Two narrowing exceptions reduce the delegation surface for Laravel apps:

- **`Illuminate\Foundation\ComposerScripts::postAutoloadDump`** is
  recognized as the known clearCompiled handler and skipped natively.
  Its only effect is unlinking cached `services.php` and `packages.php`
  — both are regenerated immediately afterward (by the typical
  `rm -f bootstrap/cache/*.php` line that follows it, and by composr's
  native discover).
- **`@php <artisan> package:discover [args]`** is replaced with native
  `composr discover` (basePath derived from the artisan path, vendor
  dir = the install pipeline's `<root>/vendor`).

If both exceptions fire and every other entry in the event is a plain
shell/php script, the entire `post-autoload-dump` runs without a
composer subprocess. That's how monolith-app gets to "0 composer calls"
on a cold install.

Composer binary resolution order: `--composer-bin <path>`,
`COMPOSR_COMPOSER` env, `composer` on `PATH`. If nothing's found and
delegation is needed, composr warns and continues with class-method
handlers skipped.

### Plugin policy

Composer-plugin packages get installed as plain library files; whether
their composer-side hooks run depends on the plugin:

- **Natively replicated** — composr ports the plugin's install-time
  codegen to Rust and writes the same artifact composer would. Output
  is byte-equal to composer's plugin output (verified by golden tests
  against real composer in the integration suite). Currently:
  - `pestphp/pest-plugin` — writes `vendor/pest-plugins.json`. Without
    it pest's runtime `Loader` returns `[]` and every Pest plugin
    (Coverage, Bail, Cache, Retry, Snapshot, Parallel,
    `pest-plugin-arch`, …) silently no-ops. Gated on the project's
    `config.allow-plugins.pestphp/pest-plugin: true`.
  - `tbachert/spi` — writes
    `vendor/composer/GeneratedServiceProviderData.php` (and splices the
    class into `autoload_classmap.php` / `autoload_static.php`).
    Replicates the `extra.spi` data flow; recognizes
    `#[Nevay\SPI\ServiceProviderDependency\PackageDependency]`
    (statically evaluated against the lock) and
    `#[…\ExtensionDependency]` (emitted as the runtime conditional
    wrap). Gated on `config.allow-plugins.tbachert/spi: true`.
    Limitations: doesn't replicate `extra.spi-config.autoload-files`
    (which would require running PHP at install time); presence of any
    such package logs a warning and falls through to delegation.
- **Inert** — installing the files alone is sufficient; the
  composer-side activation has no observable install-time effect for
  the apps we run. Currently `php-http/discovery`,
  `pestphp/pest-plugin-{laravel,mock,arch,type-coverage}`, and
  `phpstan/extension-installer`.
- **Unknown** — anything else. composr forces a `composer run-script
  <event>` delegation per lifecycle event so the plugin's subscribers
  still fire (assuming a `composer` binary is reachable; without one,
  composr warns that those hooks won't run). Project-local exceptions
  go in `composr.json` → `allow-inert-plugins` (supports `*` wildcards
  matching composer's `BasePackage::packageNameToRegexp`):

  ```json
  { "allow-inert-plugins": ["vendor/some-plugin", "vendor/*", "*/util-*"] }
  ```

  `--strict-plugins` ignores all allowlists and refuses any
  composer-plugin in the lock.

## Flags worth knowing

`composr install`:
- `--no-dev` — match `composer install --no-dev`
- `--no-scripts` — skip every script lifecycle event
- `--no-discover` — opt out of the `package:discover` interception
- `--no-composer` — never shell out, even for class-method handlers
  (those will be skipped with a warning)
- `--strict-plugins` — abort if any `composer-plugin` is in the lock
  (default behavior is to install plugins as inert files and warn)
- `--concurrency <n>` — parallel HTTP downloads (default ~CPU count, capped at 12)
- `--dry-run` — print the install/remove operations and exit

## Tests

```
cargo test
```

121 unit tests + integration tests at the time of writing, covering:
PHP parsing edge cases (non-UTF-8 class names, anonymous classes,
nested namespaces), classmap emission, autoload bootstrap files, lock
parsing, install diff, dist extraction (zip + path-symlinks), script
entry classification, the discover algorithm + var_export output
shape, PHP-compatible `json_encode` (default + `JSON_PRETTY_PRINT`,
verified against PHP 8.5), `composr.json` glob-pattern matching, and
golden-byte comparisons for both natively-replicated plugins
(`pest-plugins.json` and `GeneratedServiceProviderData.php`) against
real composer's output on a fixture lock.

## Git hooks

`composr git-hook <label>` runs `install --no-scripts` with a styled
preamble line, suitable for `post-checkout` / `post-merge` /
`post-rewrite`. It bails silently if there's no `composer.json` at cwd
and always exits 0, so a sync that fails for any reason can't block the
git op. The only thing you need on `$PATH` is `composr` itself.

Drop these into `.git/hooks/` (mark `chmod +x`):

```sh
# .git/hooks/post-checkout
#!/usr/bin/env bash
[ "$3" = "1" ] || exit 0
command -v composr >/dev/null 2>&1 || exit 0
composr git-hook post-checkout
```

```sh
# .git/hooks/post-merge
#!/usr/bin/env bash
command -v composr >/dev/null 2>&1 || exit 0
composr git-hook post-merge
```

```sh
# .git/hooks/post-rewrite
#!/usr/bin/env bash
cat >/dev/null     # drain stdin so we don't SIGPIPE the parent
command -v composr >/dev/null 2>&1 || exit 0
composr git-hook post-rewrite
```

Branch switches, pulls, and rebases will reconcile vendor + classmap
automatically. `composr install` is idempotent and ~700ms warm-cache
when there's nothing to do, so it's cheap to run on every git op.
`--no-scripts` is the conservative default — drop it from the
subcommand once you've audited that your project's `post-install-cmd`
is safe to fire on every branch switch.

`$COMPOSR` overrides the binary path (defaults to `composr` on PATH).
`$NO_COLOR` is honored.

## Optional helper scripts (`scripts/`)

A few extra shell helpers live in `scripts/` for cases the binary
doesn't cover. Drop the directory on `$PATH` (or symlink individual
files into `~/bin`) to use them.

- **`composer-dump-fast`** — drop-in for `composer dump-autoload -o`:
  runs the native classmap dumper, then asks composer to fire
  pre/post-autoload-dump scripts (so Laravel's `package:discover` and
  friends still run). Useful in CI flows that compose composr with a
  partial composer dispatch.
- **`_autoload_dumpr_run <label> [args...]`** /
  **`_autoload_dumpr_pretty <label>`** — wrap `composr dump-autoload
  --emit-stats` with a colored "starting…" preamble + one-line
  summary, for git hooks or wrappers that want richer output than the
  binary's default. (Filenames are a relic of the original
  `autoload-dumpr` binary; both scripts drive `composr` now.)
- **`_composr_git_hook <label> [unused] [unused]`** — back-compat
  forwarder to `composr git-hook <label>`. Kept so hook files written
  before the subcommand existed keep working; new ones should call
  the subcommand directly.

## Limitations & out-of-scope

- **No `composer update`.** composr is lock-driven; you still need
  composer to refresh the lock against `composer.json`.
- **Composer-plugin coverage.** See "Plugin policy" above for the full
  story — pest-plugin and tbachert/spi are natively replicated; a small
  allowlist is treated as inert; everything else gets delegated to
  `composer run-script` per event.
- **Platform-check stub.** `vendor/composer/platform_check.php` is
  emitted as a no-op. A correct implementation would emit php-version
  + ext-* checks from the lock's platform reqs.
- **`route:cache`, `view:cache`, `optimize`** etc. — Laravel's other
  cacheable artisan commands aren't intercepted yet. Same shape as
  `package:discover`; doable in a follow-up.
- **Plugin-driven service-provider discovery** (rare, e.g. some packages
  register providers via composer plugin code rather than
  `extra.laravel`). composr matches Laravel here: not handled. Those
  projects rely on plugin activation, which runs only via
  `composer install` — install via composer once, then composr from
  there.
