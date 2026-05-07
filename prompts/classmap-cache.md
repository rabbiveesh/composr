# `composr` — persistent per-package classmap cache

## Goal

Make the `bootstrap` stage near-free on every install after the first.
Currently it's the last serial 10–14s lump in `composr install` and the
main remaining gap between us and "trivially faster than composer."

For monolith-app:

| stage          | warm-cache cold install | this branch's target |
|----------------|------------------------:|---------------------:|
| download       |                    2ms  |                  2ms |
| extract (289)  |                  ~14s   |                ~14s  |
| metadata       |                    9ms  |                  9ms |
| **bootstrap**  |                **~12s** |          **~500ms**  |
| scripts        |                   30ms  |                 30ms |
| **total**      |               **~26s**  |          **~14s**    |

Cold-cold (network) saves the same ~10–11s but isn't the target — we
agreed pipelining download↔extract is a separate follow-up since it
only matters on the rare cold-cold path.

## Current behavior

`build_classmap` (`src/dump.rs`) already has a cache, but it lives at
`vendor/composer/.classmap-cache.bin`, keyed against the
`composer.lock` content hash, fingerprinting each entry with `(mtime_ns,
size)`. Two reasons it doesn't help cold installs:

1. **Wiped with vendor/.** Every `composr install` after a vendor
   wipe starts with no cache file at all → 100% misses.
2. **mtime fingerprint changes on extract.** Even if we moved the cache
   outside `vendor/`, freshly-extracted files have a fresh mtime, so
   `(mtime_ns, size)` would still mismatch every entry.

The Mutex<Vec<(PathBuf, CacheEntry)>> + per-file granularity is fine for
the project-root code (the user's app, where mtime is meaningful between
runs) but wrong for vendor packages.

## Proposed shape: two-tier cache

### Tier 1 — per-package shared cache (the new bit)

Keyed by `(package_name, dist.reference || source.reference)`. Same
reference ⇒ identical code ⇒ identical classmap. This is exactly the
invariant composer relies on for its dist-archive cache.

Layout:

```
$XDG_CACHE_HOME/composr/classmap/<package-name>/<sha1(reference)>.bin
```

Same shape as `dist_cache.rs` uses for archives. Mirrors composer's own
`<package>/<sha1(url)>.<ext>` convention.

Contents:

```rust
struct PackageClassmap {
    /// Cache format version — bump on shape change.
    version: u32,
    /// Map: relpath inside the package install dir → declarations.
    /// Relpath so the cache survives the package being installed at a
    /// different absolute path on the next run.
    files: HashMap<String, Vec<Declaration>>,
}
```

Lookup flow inside `build_classmap`:

1. For each package in the resolved config (skip the root):
   a. Compute the cache key from `package.dist.reference` (or
      `source.reference` — fall back; if neither, treat as "uncacheable"
      and parse normally).
   b. Try `PackageClassmap::load(<cache-path>)`. On hit:
      - For each `(relpath, decls)`: rebase to absolute path with
        `package.root.join(relpath)`, push to ClassMap directly.
      - Mark the package's files as "cache-hit", skip the walk + parse
        entirely.
   c. On miss: walk + parse this package as today, then write the
      result to the cache at the end.

The huge win is "skip the walk" — we don't even enumerate vendor
package PHP files when their reference is unchanged.

### Tier 2 — project-root file cache (mostly today's code)

Keep the existing `vendor/composer/.classmap-cache.bin` for the root
package (the user's `app/`, `src/`, etc.). Files there change between
runs; mtime+size is the right fingerprint; the cache living inside
vendor is fine because the root files don't get wiped with vendor.

Actually — the root cache could equally live next to the new tier-1
cache (in `$XDG_CACHE_HOME/composr/classmap/__root__/<hash-of-project-path>.bin`)
to keep all classmap state in one place. Worth doing for symmetry; a
small refactor of the existing flow.

## Cache key details

The "reference" we hash needs to be stable across runs of the same
package version:

- **Git/dist installs:** `dist.reference` is a git SHA. Use that
  directly. Length and charset are safe for filenames; sha1 of it is
  still the rule for filesystem hygiene with weird future references.
- **Path installs** (`dist.type == "path"`): no stable reference —
  contents may have changed since last run. Don't cache; walk + parse
  every time. Path installs are rare and usually small (sibling repos
  in monorepos), so this isn't a meaningful loss.
- **Metapackages:** no install path, no PHP — skip entirely (same as
  today's code already does for installed.php).

## Implementation plan

New module: `src/classmap_cache.rs` (sibling of `dist_cache.rs`). It
gets the same `discover()`-based override hierarchy
(`COMPOSR_CACHE_DIR`/XDG/`~/.cache`).

Touch points:

1. **`src/classmap_cache.rs` (new):** `PackageClassmap` struct, load/save
   helpers, path resolution. Mirror `dist_cache.rs`'s API surface — keep
   it boring.
2. **`src/dump.rs` (modify):** in `build_classmap`, before kicking off
   `walked.par_iter()`, partition packages into "cacheable hit",
   "cacheable miss" (needs walk), and "uncacheable" (path installs,
   root, no-reference). Run the existing walk only on the latter two
   sets. After the walk, persist tier-1 entries for the misses.
3. **`src/cache.rs` (light touch, optional):** if we move the root
   classmap cache to the user-cache dir as discussed, the existing Cache
   struct gets a project-fingerprint key. Otherwise leave as-is for now
   and revisit later.
4. **`src/install.rs`:** no changes — `build_classmap` is the
   integration point.

A few details worth getting right:

- **Concurrency.** Multiple `composr install` runs against the same
  project (e.g. CI parallelism) shouldn't corrupt the cache. Keep the
  existing atomic-write pattern (`tmp` + `rename`) per file; tier-1
  cache files are independent per package so contention is minimal.
- **Cache size cap?** No bound today on `dist_cache`; let's not bother
  for tier-1 either. ~292 packages × few-KB-per-bin-file = single-digit
  MB of cache. Trivial.
- **Sharing.** Two projects with the same package version ref share
  the cache. Free win.

## Sanity check before optimizing further

After landing, measure on monolith-app:

1. Wipe vendor + cache. Run install. Expect bootstrap ~12s (cold).
2. Wipe vendor only (keep cache). Run install. Expect bootstrap <1s.
3. Wipe vendor and ONE package's cache file. Run install. Expect
   bootstrap to walk just that package, ~30–100ms.

Also gut-check correctness: `cargo test --lib` stays green; the
generated `vendor/composer/autoload_classmap.php` is byte-identical to
composer's output, both with and without cache hits, on service-app and
monolith-app.

## Edge cases / gotchas

- **Renamed packages.** Composer occasionally renames a package
  (`laravel-app/foo` → `acme/foo`). Cache key includes the package
  name, so the rename produces a fresh miss. Correct.
- **Classmap include rules.** `filter::admit` decides what makes the
  classmap (excludes `.skip` markers, exclude-from-classmap globs,
  etc.). We cache `Vec<Declaration>` — pre-admission. The admission
  step runs identically on cache hits and misses, so behavior is
  preserved if `composer.json` `autoload.exclude-from-classmap` rules
  change between runs without a vendor change. (Unlikely but worth
  noting; mtime/contents of `composer.json` is implicitly part of the
  ResolvedConfig for the affected package.)
- **Symlinked packages** (path-dist already excluded; PSR-4 entries
  resolved through symlinks): out of scope — we already don't cache
  these.
- **Cache version bumps.** Bump `CACHE_VERSION` when the
  `Vec<Declaration>` shape changes. Ignore old-format files on load.
  Same pattern `cache.rs` already uses.
- **Project relocation.** If the user moves their project dir, paths
  in tier-2 (root) cache become wrong. Existing code handles this by
  invalidating on lock-hash mismatch — not perfect, but adequate.
  Tier-1 stores relpaths, so it's robust to install-path movement.

## Out of scope

- **Pipelining download↔extract.** Separate branch. Worth ~10s only on
  cold-cold installs, which we agreed are rare.
- **Cross-machine cache distribution** (e.g. CI sharing tier-1 across
  runners). Build artifact for ops to figure out — the cache files
  are just bins under a known path.
- **Bootstrap autoload-static rewrite caching.** The classmap walk is
  the expensive part; emitting `autoload_static.php` from the map is
  already milliseconds. No-op.
- **Composer-compat cache format.** Composer doesn't cache classmap
  data, so there's nothing to share. Use whatever bincode shape we
  like.

## Files to read for reference

- `src/cache.rs` — current `Cache`/`CacheEntry`, lock-hash invalidation,
  bincode load/save.
- `src/dist_cache.rs` — the discover/override hierarchy and on-disk
  layout (`$XDG_CACHE_HOME/composr/files/<package>/<sha1>.<ext>`) we
  should copy.
- `src/dump.rs` — `build_classmap`, where the per-file cache is
  consulted today (lines ~67–139). The new tier-1 path slots in before
  the `walked.par_iter()` block.
- `src/parse.rs` — `Declaration` (already serde-derived; reusable in
  the new bin format).
- `src/walk.rs` — `collect_files` walks one package's PSR-4/PSR-0 dirs.
  Per-package partitioning of the input is natural.
- `src/config.rs` — `ResolvedConfig::load` and `PackageAutoload`. We
  need access to each package's `dist.reference` here; check whether
  that's already on `PackageAutoload` or if we need to thread it
  through.

## Quick start checklist for the next session

1. `cd /Users/avishai.goldman/composr`
2. Branch off `main` once `discover-laravel` lands.
3. Add `src/classmap_cache.rs` mirroring `dist_cache.rs`'s shape; load/save
   `PackageClassmap` with the same atomic-write pattern.
4. Confirm `dist.reference` reaches `dump::build_classmap`. Thread it
   through `PackageAutoload` if not.
5. Refactor `build_classmap` to partition packages → run today's walk
   only on misses.
6. Time it on monolith-app: cold (no cache) ≈ today's 12s; warm (cache
   present, vendor freshly extracted) target <1s.
7. Verify byte-identical `autoload_classmap.php` against composer on
   service-app and monolith-app in both modes.
8. Update README perf table.
9. Commit, report.
