# `composr discover` — native Laravel package:discover

## Goal

Replace `php artisan package:discover` with a Rust subcommand that produces
the same `bootstrap/cache/packages.php` 50–100× faster.

For monolith-app, `php artisan package:discover --ansi` (run as the last entry
of `post-autoload-dump`) takes ~25–30s of the 42.5s composer-delegated
script call. On service-app it's ~10s of 13s. It dominates cold-install
wall-time after our native bootstrap killed the dump-autoload delegation —
so this is the next obvious win.

The user nicknamed this work, in full sincerity: "we'll have to target
package:discover in another branch lol".

## What package:discover actually does

Mechanically — the canonical implementation lives at:
```
vendor/laravel/framework/src/Illuminate/Foundation/PackageManifest.php
```
(see also `Illuminate\Foundation\Console\PackageDiscoverCommand` which is
the artisan wrapper).

Algorithm:

1. Read `vendor/composer/installed.json`.
2. Read root `composer.json["extra"]["laravel"]["dont-discover"]` — a list
   of package names to skip. May contain the literal `"*"` meaning "skip
   all auto-discovery". Default: empty list.
3. For each package in installed.json:
   a. Take its `extra.laravel` field (or `{}` if absent).
   b. Merge that package's own `dont-discover` into the global ignore list
      (i.e. transitive opt-outs).
4. Filter packages: drop any whose name is in the ignore list, OR drop all
   if `"*"` was present.
5. Drop packages whose `extra.laravel` is empty after step 3.
6. Write to `<base>/bootstrap/cache/packages.php` as a PHP file:
   ```php
   <?php return array (
     'package/name' => array (
       'providers' => array (0 => 'Foo\\Bar', ),
       'aliases' => array ('Alias' => 'Foo\\Bar', ),
     ),
     ...
   );
   ```

The format is `var_export(...)` of an associative array — same shape as
Laravel's existing output. We need to match it byte-for-byte (or close
enough that `is_file($manifestPath)` passes and the array's contents
parse-equal).

The fields under `extra.laravel` in practice are: `providers` (list),
`aliases` (associative), `dont-discover` (list — consumed/removed before
write).

## Implementation plan

New module: `src/discover.rs`. New CLI subcommand: `composr discover
[--root <path>] [--manifest-path <path>]`.

The function signature roughly:
```rust
pub struct DiscoverOptions {
    pub root: PathBuf,           // project root
    pub manifest_path: Option<PathBuf>, // override; default <root>/bootstrap/cache/packages.php
}

pub fn discover(opts: DiscoverOptions) -> anyhow::Result<DiscoverReport>;

pub struct DiscoverReport {
    pub packages_in_manifest: usize,
    pub elapsed_ms: u128,
}
```

Steps:

1. Parse `<root>/composer.json` → extract `extra.laravel.dont-discover`.
2. Parse `<root>/vendor/composer/installed.json` → list of package
   entries (we already have `crate::installed::InstalledJson` — reuse).
3. For each package: copy `extra.laravel` (a `serde_json::Value`) if
   present.
4. Walk through the collected entries to collect transitive `dont-discover`,
   then filter.
5. Render PHP via `php_var_export` — see emit.rs for the existing
   var-export-style writers we use (the classmap renderer is similar).
6. Write atomically to `bootstrap/cache/packages.php`.

If `bootstrap/cache/` doesn't exist, create it. (Composer's post-autoload-
dump scripts already do `rm -f bootstrap/cache/*.php` first; we should
also be safe to overwrite.)

### Wire into install

In `src/install.rs`, after our native bootstrap, optionally run discover
when:
- `<root>/laravel-app/artisan` or `<root>/artisan` exists (Laravel
  detection — could be more thorough by reading composer.json's
  `"type": "project"` or `extra.laravel`)
- AND `<root>/composer.json["scripts"]["post-autoload-dump"]` contains
  an entry calling `package:discover`

If both, run our native discover and skip the artisan call. The
class-method-handler delegation to composer remains for things we don't
emulate (e.g. `Illuminate\Foundation\ComposerScripts::postAutoloadDump`,
which does `clearCompiled()` — that's just a few unlinks, also trivial
to do natively if we want).

Add a `--no-discover` flag to opt out (parallel to `--no-scripts`).

## Files to read for reference

In our codebase:
- `src/installed.rs` — `InstalledJson::read` and the existing
  `installed.php` writer. The shape we need to write is the same flavor
  of PHP code.
- `src/emit.rs` — `render_classmap_php` does var-export-style PHP
  emission. Pattern to mirror for `packages.php`.
- `src/bootstrap.rs` — `write_atomic` helper.

In the sister composer/composer repo (`/Users/avishai.goldman/composer`):
- nothing relevant — package:discover is a Laravel thing, not composer.

In Laravel:
- `<any project>/vendor/laravel/framework/src/Illuminate/Foundation/PackageManifest.php`
  — the spec. ~190 lines, very readable.
- `<any project>/vendor/laravel/framework/src/Illuminate/Foundation/Console/PackageDiscoverCommand.php`
  — the artisan wrapper. Mostly just calls PackageManifest::build().

### Dont-discover finder details

The merge order in PackageManifest::build:
```php
$ignoreAll = in_array('*', $ignore = $this->packagesToIgnore());
// $this->packagesToIgnore() reads root composer.json extra.laravel.dont-discover

each package's `extra.laravel.dont-discover` then ALSO contributes:
$ignore = array_merge($ignore, $configuration['dont-discover'] ?? []);

// Then reject:
$ignoreAll || in_array($package, $ignore);
```

So the rule is: a package can declare other packages it suppresses
(transitive opt-out). Capture that.

## Testing approach

For fast feedback:

1. Unit tests in `discover::tests`:
   - Reads a fake installed.json + root composer.json fixture, produces
     expected packages.php content.
   - Honors `*` in `dont-discover`.
   - Honors per-package transitive `dont-discover`.
   - Skips packages with no `extra.laravel`.

2. Integration: golden file. Take service-app's existing
   `bootstrap/cache/packages.php` (composer-generated), wipe it, run
   `composr discover --root <service-app>`, diff. Should match byte-for-byte
   modulo whitespace differences. We may need to mirror Laravel's
   `var_export` exact formatting — pay attention to:
   - 2-space-indented array syntax, with `array (` on its own line for
     nested arrays
   - Trailing comma after each entry
   - `0 =>` numeric keys for list-shaped arrays (var_export's default
     for lists without sequential keys preserved)
   - String quoting style: `var_export` uses single quotes, escapes `\`
     and `'` only

3. Functional: copy service-app's packages.php aside, run our discover, then
   `php artisan list` — same 469 commands should appear.

## Performance target

Composer-side post-autoload-dump on monolith-app: 42.5s, of which ~30s is
package:discover itself.

Goal: native discover in **<500ms** for monolith-app's 292 packages.

The work is genuinely tiny — it's just JSON parsing of one installed.json
file and PHP-string emission. No filesystem walk needed. Should be
trivially fast.

## Out of scope (initially)

- `php artisan optimize` / `route:cache` / `view:cache` — these are
  separate caching concerns, also slow in artisan, also potentially
  portable to native. Leave for follow-ups.
- `clearCompiled()` (the class-method handler that nukes
  bootstrap/cache/{config,services,packages}.php) — easy to do natively
  but not load-bearing for discovery.
- Non-Laravel project types. Discover is opt-in based on Laravel
  detection.
- Composer plugin-driven discovery (e.g. some packages register their
  service providers via plugin code rather than `extra.laravel`).
  Behavior matches Laravel: not our problem — those projects already
  rely on those plugins running, which they do at composer-script-time
  via our class-method-script delegation.

## Branch shape

Suggest branch name: `discover-laravel`.

Single commit ideally, structured as:
- `src/discover.rs` (new)
- `src/lib.rs` (mod registration)
- `src/main.rs` (`composr discover` subcommand)
- `src/install.rs` (auto-trigger when Laravel detected; `--no-discover` flag)
- Tests added in `discover::tests`

Keep the change tight — no broader refactors. The existing
`src/scripts.rs` runner stays untouched; we just intercept
`post-autoload-dump` events whose entries call `package:discover` and
substitute our native run for that one line.

## Quick start checklist for the next session

1. `cd /Users/avishai.goldman/composr`
2. `git checkout -b discover-laravel` (off `install-command` or wherever
   master is by then)
3. Read the three reference files above (PackageManifest.php in service-app's
   vendor, plus our existing installed.rs + emit.rs).
4. Sketch `php_var_export` first — it's the trickiest piece. Match
   PHP's `var_export` output for nested associative arrays.
5. Build the discover function, unit-test against a small fixture.
6. Wire into install.rs.
7. Golden-test against service-app: `cmp` our output vs composer's existing
   packages.php after a clean composer install.
8. Time it on monolith-app — should be sub-second.
9. Commit, report.
