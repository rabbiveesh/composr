//! Generate the autoload bootstrap files from scratch.
//!
//! Composer's `dump-autoload` produces these in vendor/composer/ (and one
//! in vendor/):
//!
//!   vendor/autoload.php           — entry point: requires autoload_real.php,
//!                                    calls ComposerAutoloaderInit<HASH>::getLoader()
//!   vendor/composer/autoload_real.php       — defines ComposerAutoloaderInit<HASH>
//!   vendor/composer/autoload_static.php     — opcache-friendly static class with
//!                                              every PSR-4/PSR-0/files/classmap entry inlined
//!   vendor/composer/autoload_classmap.php   — classmap entries (also written by emit.rs)
//!   vendor/composer/autoload_psr4.php       — PSR-4 prefix → paths
//!   vendor/composer/autoload_namespaces.php — PSR-0 prefix → paths
//!   vendor/composer/autoload_files.php      — files to require eagerly
//!   vendor/composer/ClassLoader.php         — composer's runtime classloader (static)
//!   vendor/composer/InstalledVersions.php   — composer's runtime versions API (static)
//!   vendor/composer/LICENSE                 — composer's MIT LICENSE for the bundled bits
//!   vendor/composer/platform_check.php      — PHP version + ext-* checks (we emit a no-op stub)
//!
//! We bundle the static files (ClassLoader.php, InstalledVersions.php,
//! LICENSE) as `include_bytes!` assets — they're MIT-licensed copies of
//! composer/composer source. We generate the rest from project data.
//!
//! Path style in generated files mirrors composer:
//!   - `$vendorDir . '/foo'` for paths under vendor/
//!   - `$baseDir . '/foo'` for paths under the project root
//!   - `$vendorDir = dirname(__DIR__);` and `$baseDir = dirname($vendorDir);`
//!     are emitted at the top of each data file.

use crate::config::PackageAutoload;
use crate::installed::write_installed_versions_php;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Inputs for bootstrap generation. Most fields come from `ResolvedConfig`
/// + the dump-autoload classmap result.
pub struct BootstrapInput<'a> {
    pub root: &'a Path,
    pub vendor_dir: &'a Path,
    pub packages: &'a [PackageAutoload],
    /// FQCN -> file (absolute path), as computed by the existing
    /// dump-autoload pipeline. Stored as raw bytes because PSR-4 keys can
    /// contain non-ASCII bytes in pathological cases.
    pub classmap: &'a BTreeMap<Vec<u8>, PathBuf>,
    /// `<lock content-hash or random>` — used as the suffix for the
    /// `ComposerAutoloaderInit<suffix>` and `ComposerStaticInit<suffix>`
    /// class names. Two requirements: stable-across-runs (so the file is
    /// reproducible) and 32-char hex (composer's convention).
    pub suffix: &'a str,
}

/// Emit every file the autoload bootstrap needs. Idempotent — running it
/// twice produces the same outputs.
pub fn write_bootstrap(input: &BootstrapInput) -> Result<()> {
    let composer_dir = input.vendor_dir.join("composer");
    std::fs::create_dir_all(&composer_dir)
        .with_context(|| format!("creating {}", composer_dir.display()))?;

    // Collect autoload data across all packages, with root last so its
    // entries don't get shadowed by vendor.
    let aggregated = aggregate_autoloads(input.packages);

    // Write data-driven array files first — autoload_static.php embeds the
    // same data, but reading the array files is composer's convention and
    // some tools (e.g. older consumers of $vendorDir/composer/autoload_psr4.php
    // directly) still require them.
    let psr4 = render_simple_array_file(
        "autoload_psr4.php",
        &aggregated.psr_4,
        input.root,
        input.vendor_dir,
    );
    write_atomic(&composer_dir.join("autoload_psr4.php"), &psr4)?;

    let psr0 = render_simple_array_file(
        "autoload_namespaces.php",
        &aggregated.psr_0,
        input.root,
        input.vendor_dir,
    );
    write_atomic(&composer_dir.join("autoload_namespaces.php"), &psr0)?;

    let files = render_files_array(&aggregated.files, input.root, input.vendor_dir);
    write_atomic(&composer_dir.join("autoload_files.php"), &files)?;

    // autoload_classmap.php — render via the existing emit.rs path so
    // bootstrap and patch-mode produce byte-identical output.
    let classmap_php = crate::emit::render_classmap_php(
        input.classmap,
        input.root,
        input.vendor_dir,
    );
    write_atomic(&composer_dir.join("autoload_classmap.php"), &classmap_php)?;

    // The big one: autoload_static.php. This embeds every entry inline so
    // opcache can fold it into a single static class. We build it from the
    // same aggregated data + classmap so it stays in sync with the array
    // files we just wrote.
    let static_php = render_autoload_static(
        input.suffix,
        &aggregated,
        input.classmap,
        input.root,
        input.vendor_dir,
    );
    write_atomic(&composer_dir.join("autoload_static.php"), &static_php)?;

    // The runtime ComposerAutoloaderInit<suffix> class.
    let real_php = render_autoload_real(input.suffix, !aggregated.files.is_empty());
    write_atomic(&composer_dir.join("autoload_real.php"), &real_php)?;

    // Project-root entry point.
    let autoload_php = render_autoload_php(input.suffix);
    write_atomic(&input.vendor_dir.join("autoload.php"), &autoload_php)?;

    // platform_check.php — we ship a no-op for now. A correct implementation
    // would emit php-version + ext-* checks from the lock's platform reqs;
    // since we don't have those wired here, an empty stub is honest and
    // keeps autoload_real.php's `require platform_check.php` from blowing up.
    let platform_check = b"<?php // platform_check.php @generated by composr (no-op stub)\n";
    write_atomic(
        &composer_dir.join("platform_check.php"),
        platform_check,
    )?;

    // Static assets: ClassLoader.php, InstalledVersions.php, LICENSE.
    write_classloader_php(&composer_dir.join("ClassLoader.php"))?;
    write_installed_versions_php(&composer_dir.join("InstalledVersions.php"))?;
    write_license(&composer_dir.join("LICENSE"))?;

    Ok(())
}

fn write_classloader_php(path: &Path) -> Result<()> {
    const SOURCE: &[u8] = include_bytes!("../assets/ClassLoader.php");
    write_atomic(path, SOURCE)
}

fn write_license(path: &Path) -> Result<()> {
    const SOURCE: &[u8] = include_bytes!("../assets/LICENSE");
    write_atomic(path, SOURCE)
}

/// Aggregated autoload data across all packages. Ordering matters — composer
/// emits PSR-4 entries in a specific order (longest-prefix-first per PSR-4
/// spec is encoded by the runtime ClassLoader, but the file order is just
/// "vendor packages first, root last, registration order otherwise"). We
/// follow the same convention.
struct Aggregated {
    psr_4: Vec<(String, Vec<PathBuf>)>,
    psr_0: Vec<(String, Vec<PathBuf>)>,
    /// fileIdentifier (md5 of "<package>:<relpath>") → absolute path.
    files: Vec<(String, PathBuf)>,
}

fn aggregate_autoloads(packages: &[PackageAutoload]) -> Aggregated {
    // psr-4 / psr-0 are merged by prefix — multiple packages may declare the
    // same prefix (uncommon but legal; results in a list of search paths).
    let mut psr_4_map: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut psr_0_map: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for pkg in packages {
        for (prefix, paths) in &pkg.psr_4 {
            psr_4_map
                .entry(prefix.clone())
                .or_default()
                .extend(paths.iter().cloned());
        }
        for (prefix, paths) in &pkg.psr_0 {
            psr_0_map
                .entry(prefix.clone())
                .or_default()
                .extend(paths.iter().cloned());
        }
        for f in &pkg.files {
            // file identifier = md5("<package-name>:<relpath>"). We
            // approximate the relpath by stripping the package root.
            let rel = f
                .strip_prefix(&pkg.root)
                .map(|r| r.to_string_lossy().to_string())
                .unwrap_or_else(|_| f.to_string_lossy().to_string());
            let id_input = format!("{}:{}", pkg.name, rel);
            let id = md5_hex(id_input.as_bytes());
            files.push((id, f.clone()));
        }
    }

    Aggregated {
        psr_4: psr_4_map.into_iter().collect(),
        psr_0: psr_0_map.into_iter().collect(),
        files,
    }
}

/// Render `autoload_psr4.php` / `autoload_namespaces.php` — same structure,
/// only the comment header and which map differ.
fn render_simple_array_file(
    file_name: &str,
    entries: &[(String, Vec<PathBuf>)],
    root: &Path,
    vendor_dir: &Path,
) -> Vec<u8> {
    let mut s = Vec::with_capacity(2048);
    s.extend_from_slice(b"<?php\n\n// ");
    s.extend_from_slice(file_name.as_bytes());
    s.extend_from_slice(b" @generated by composr\n\n$vendorDir = dirname(__DIR__);\n");
    s.extend_from_slice(b"$baseDir = dirname($vendorDir);\n\nreturn array(\n");

    for (prefix, paths) in entries {
        s.extend_from_slice(b"    ");
        write_php_string_to(&mut s, prefix);
        s.extend_from_slice(b" => array(");
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                s.extend_from_slice(b", ");
            }
            write_var_path(&mut s, p, root, vendor_dir);
        }
        s.extend_from_slice(b"),\n");
    }
    s.extend_from_slice(b");\n");
    s
}

/// Render `autoload_files.php`.
fn render_files_array(entries: &[(String, PathBuf)], root: &Path, vendor_dir: &Path) -> Vec<u8> {
    let mut s = Vec::with_capacity(2048);
    s.extend_from_slice(b"<?php\n\n// autoload_files.php @generated by composr\n\n");
    s.extend_from_slice(b"$vendorDir = dirname(__DIR__);\n$baseDir = dirname($vendorDir);\n\nreturn array(\n");
    for (id, path) in entries {
        s.extend_from_slice(b"    ");
        write_php_string_to(&mut s, id);
        s.extend_from_slice(b" => ");
        write_var_path(&mut s, path, root, vendor_dir);
        s.extend_from_slice(b",\n");
    }
    s.extend_from_slice(b");\n");
    s
}

/// Render `autoload.php` — the entry point that lives at vendor/autoload.php.
fn render_autoload_php(suffix: &str) -> Vec<u8> {
    let mut s = String::with_capacity(1024);
    write!(
        s,
        r#"<?php

// autoload.php @generated by composr

if (PHP_VERSION_ID < 50600) {{
    if (!headers_sent()) {{
        header('HTTP/1.1 500 Internal Server Error');
    }}
    $err = 'Composer 2.3.0 dropped support for autoloading on PHP <5.6 and you are running '.PHP_VERSION.', please upgrade PHP or use Composer 2.2 LTS via "composer self-update --2.2". Aborting.'.PHP_EOL;
    if (!ini_get('display_errors')) {{
        if (PHP_SAPI === 'cli' || PHP_SAPI === 'phpdbg') {{
            fwrite(STDERR, $err);
        }} elseif (!headers_sent()) {{
            echo $err;
        }}
    }}
    throw new RuntimeException($err);
}}

require_once __DIR__ . '/composer/autoload_real.php';

return ComposerAutoloaderInit{suffix}::getLoader();
"#
    )
    .unwrap();
    s.into_bytes()
}

/// Render `vendor/composer/autoload_real.php`. The runtime entry that defines
/// `ComposerAutoloaderInit<suffix>::getLoader()`.
fn render_autoload_real(suffix: &str, has_files: bool) -> Vec<u8> {
    let mut s = String::with_capacity(2048);
    write!(
        s,
        r#"<?php

// autoload_real.php @generated by composr

class ComposerAutoloaderInit{suffix}
{{
    private static $loader;

    public static function loadClassLoader($class)
    {{
        if ('Composer\Autoload\ClassLoader' === $class) {{
            require __DIR__ . '/ClassLoader.php';
        }}
    }}

    /**
     * @return \Composer\Autoload\ClassLoader
     */
    public static function getLoader()
    {{
        if (null !== self::$loader) {{
            return self::$loader;
        }}

        require __DIR__ . '/platform_check.php';

        spl_autoload_register(array('ComposerAutoloaderInit{suffix}', 'loadClassLoader'), true, true);
        self::$loader = $loader = new \Composer\Autoload\ClassLoader(\dirname(__DIR__));
        spl_autoload_unregister(array('ComposerAutoloaderInit{suffix}', 'loadClassLoader'));

        require __DIR__ . '/autoload_static.php';
        call_user_func(\Composer\Autoload\ComposerStaticInit{suffix}::getInitializer($loader));

        $loader->register(true);
"#
    )
    .unwrap();
    if has_files {
        write!(
            s,
            r#"
        $filesToLoad = \Composer\Autoload\ComposerStaticInit{suffix}::$files;
        $requireFile = \Closure::bind(static function ($fileIdentifier, $file) {{
            if (empty($GLOBALS['__composer_autoload_files'][$fileIdentifier])) {{
                $GLOBALS['__composer_autoload_files'][$fileIdentifier] = true;

                require $file;
            }}
        }}, null, null);
        foreach ($filesToLoad as $fileIdentifier => $file) {{
            $requireFile($fileIdentifier, $file);
        }}
"#
        )
        .unwrap();
    }
    write!(
        s,
        r#"
        return $loader;
    }}
}}
"#
    )
    .unwrap();
    s.into_bytes()
}

/// Render `vendor/composer/autoload_static.php` — the opcache-friendly form
/// where every entry is inlined as a static class property. Composer
/// computes prefix-length buckets for PSR-4 (group prefixes by their first
/// character + length), which the ClassLoader's `findFile` consults for a
/// fast first-character → bucket → length-match lookup.
fn render_autoload_static(
    suffix: &str,
    aggregated: &Aggregated,
    classmap: &BTreeMap<Vec<u8>, PathBuf>,
    root: &Path,
    vendor_dir: &Path,
) -> Vec<u8> {
    let mut s = Vec::with_capacity(64 * 1024);
    s.extend_from_slice(b"<?php\n\n// autoload_static.php @generated by composr\n\nnamespace Composer\\Autoload;\n\nclass ComposerStaticInit");
    s.extend_from_slice(suffix.as_bytes());
    s.extend_from_slice(b"\n{\n");

    // public static $files
    s.extend_from_slice(b"    public static $files = array (\n");
    for (id, path) in &aggregated.files {
        s.extend_from_slice(b"        ");
        write_php_string_to(&mut s, id);
        s.extend_from_slice(b" => ");
        write_dir_relative_path(&mut s, path, root, vendor_dir);
        s.extend_from_slice(b",\n");
    }
    s.extend_from_slice(b"    );\n\n");

    // PSR-4: prefixLengthsPsr4 grouped by first char and prefix length, then
    // prefixDirsPsr4 maps the prefix to its dirs. The ClassLoader uses the
    // lengths bucket for fast lookup.
    let (lengths, dirs) = build_psr4_buckets(&aggregated.psr_4);
    s.extend_from_slice(b"    public static $prefixLengthsPsr4 = array (\n");
    for (first, by_len) in &lengths {
        s.extend_from_slice(b"        ");
        write_php_string_to(&mut s, &first.to_string());
        s.extend_from_slice(b" => \n        array (\n");
        for (prefix, len) in by_len {
            s.extend_from_slice(b"            ");
            write_php_string_to(&mut s, prefix);
            s.extend_from_slice(b" => ");
            write!(&mut s as &mut Vec<u8>, "{len}").unwrap();
            s.extend_from_slice(b",\n");
        }
        s.extend_from_slice(b"        ),\n");
    }
    s.extend_from_slice(b"    );\n\n");

    s.extend_from_slice(b"    public static $prefixDirsPsr4 = array (\n");
    for (prefix, paths) in &dirs {
        s.extend_from_slice(b"        ");
        write_php_string_to(&mut s, prefix);
        s.extend_from_slice(b" => \n        array (\n");
        for (i, path) in paths.iter().enumerate() {
            s.extend_from_slice(b"            ");
            write!(&mut s as &mut Vec<u8>, "{i}").unwrap();
            s.extend_from_slice(b" => ");
            write_dir_relative_path(&mut s, path, root, vendor_dir);
            s.extend_from_slice(b",\n");
        }
        s.extend_from_slice(b"        ),\n");
    }
    s.extend_from_slice(b"    );\n\n");

    // PSR-0: prefixesPsr0 grouped by first char.
    let psr0_grouped = build_psr0_buckets(&aggregated.psr_0);
    s.extend_from_slice(b"    public static $prefixesPsr0 = array (\n");
    for (first, by_prefix) in &psr0_grouped {
        s.extend_from_slice(b"        ");
        write_php_string_to(&mut s, &first.to_string());
        s.extend_from_slice(b" => \n        array (\n");
        for (prefix, paths) in by_prefix {
            s.extend_from_slice(b"            ");
            write_php_string_to(&mut s, prefix);
            s.extend_from_slice(b" => \n            array (\n");
            for (i, path) in paths.iter().enumerate() {
                s.extend_from_slice(b"                ");
                write!(&mut s as &mut Vec<u8>, "{i}").unwrap();
                s.extend_from_slice(b" => ");
                write_dir_relative_path(&mut s, path, root, vendor_dir);
                s.extend_from_slice(b",\n");
            }
            s.extend_from_slice(b"            ),\n");
        }
        s.extend_from_slice(b"        ),\n");
    }
    s.extend_from_slice(b"    );\n\n");

    // public static $classMap — same content as autoload_classmap.php's
    // body, just inlined here.
    s.extend_from_slice(b"    public static $classMap = array (\n");
    let body = crate::emit::render_static_classmap_body(classmap, root, vendor_dir);
    s.extend_from_slice(&body);
    s.extend_from_slice(b"    );\n\n");

    // getInitializer — closure-bound to ClassLoader so we can poke its
    // private properties from outside the class without reflection.
    s.extend_from_slice(b"    public static function getInitializer(ClassLoader $loader)\n");
    s.extend_from_slice(b"    {\n");
    s.extend_from_slice(b"        return \\Closure::bind(function () use ($loader) {\n");
    s.extend_from_slice(b"            $loader->prefixLengthsPsr4 = ComposerStaticInit");
    s.extend_from_slice(suffix.as_bytes());
    s.extend_from_slice(b"::$prefixLengthsPsr4;\n");
    s.extend_from_slice(b"            $loader->prefixDirsPsr4 = ComposerStaticInit");
    s.extend_from_slice(suffix.as_bytes());
    s.extend_from_slice(b"::$prefixDirsPsr4;\n");
    s.extend_from_slice(b"            $loader->prefixesPsr0 = ComposerStaticInit");
    s.extend_from_slice(suffix.as_bytes());
    s.extend_from_slice(b"::$prefixesPsr0;\n");
    s.extend_from_slice(b"            $loader->classMap = ComposerStaticInit");
    s.extend_from_slice(suffix.as_bytes());
    s.extend_from_slice(b"::$classMap;\n");
    s.extend_from_slice(b"\n        }, null, ClassLoader::class);\n");
    s.extend_from_slice(b"    }\n");
    s.extend_from_slice(b"}\n");

    s
}

/// Group PSR-4 prefixes by first character, with each entry mapping
/// prefix → length. Mirrors composer's
/// `ClassLoader::$prefixLengthsPsr4` shape.
fn build_psr4_buckets(
    entries: &[(String, Vec<PathBuf>)],
) -> (
    BTreeMap<char, Vec<(String, usize)>>,
    Vec<(String, Vec<PathBuf>)>,
) {
    let mut by_first: BTreeMap<char, Vec<(String, usize)>> = BTreeMap::new();
    let mut dirs: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for (prefix, paths) in entries {
        if prefix.is_empty() {
            // Composer treats the empty PSR-4 prefix as a fallback under a
            // separate `fallbackDirsPsr4`. Rare; we don't see it in
            // service-app/monolith-app, so preserve it under prefixDirsPsr4 for now.
            dirs.push((prefix.clone(), paths.clone()));
            continue;
        }
        let first = prefix.chars().next().unwrap();
        by_first
            .entry(first)
            .or_default()
            .push((prefix.clone(), prefix.len()));
        dirs.push((prefix.clone(), paths.clone()));
    }
    // Sort prefixes within each bucket by descending length — composer
    // matches this so longer (more specific) prefixes are tried first.
    for v in by_first.values_mut() {
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
    (by_first, dirs)
}

fn build_psr0_buckets(
    entries: &[(String, Vec<PathBuf>)],
) -> BTreeMap<char, BTreeMap<String, Vec<PathBuf>>> {
    let mut by_first: BTreeMap<char, BTreeMap<String, Vec<PathBuf>>> = BTreeMap::new();
    for (prefix, paths) in entries {
        if prefix.is_empty() {
            continue;
        }
        let first = prefix.chars().next().unwrap();
        by_first
            .entry(first)
            .or_default()
            .insert(prefix.clone(), paths.clone());
    }
    by_first
}

/// Render a path as `$vendorDir . '/...'` or `$baseDir . '/...'`. Used in
/// the array files (autoload_psr4.php etc.) that share `$vendorDir` /
/// `$baseDir` declarations at the top.
fn write_var_path(out: &mut Vec<u8>, path: &Path, root: &Path, vendor_dir: &Path) {
    if let Ok(rel) = path.strip_prefix(vendor_dir) {
        out.extend_from_slice(b"$vendorDir . '/");
        out.extend_from_slice(rel.to_string_lossy().as_bytes());
        out.push(b'\'');
    } else if let Ok(rel) = path.strip_prefix(root) {
        out.extend_from_slice(b"$baseDir . '/");
        out.extend_from_slice(rel.to_string_lossy().as_bytes());
        out.push(b'\'');
    } else {
        write_php_string_to(out, &path.to_string_lossy());
    }
}

/// Render a path inline inside autoload_static.php as
/// `__DIR__ . '/..' . '/foo'` (vendor) or `__DIR__ . '/../..' . '/foo'`
/// (project root). autoload_static.php lives at vendor/composer/, so up-one
/// = vendor/, up-two = project root.
fn write_dir_relative_path(out: &mut Vec<u8>, path: &Path, root: &Path, vendor_dir: &Path) {
    if let Ok(rel) = path.strip_prefix(vendor_dir) {
        out.extend_from_slice(b"__DIR__ . '/..' . '/");
        out.extend_from_slice(rel.to_string_lossy().as_bytes());
        out.push(b'\'');
    } else if let Ok(rel) = path.strip_prefix(root) {
        out.extend_from_slice(b"__DIR__ . '/../..' . '/");
        out.extend_from_slice(rel.to_string_lossy().as_bytes());
        out.push(b'\'');
    } else {
        write_php_string_to(out, &path.to_string_lossy());
    }
}

fn write_php_string_to(out: &mut Vec<u8>, s: &str) {
    out.push(b'\'');
    for b in s.bytes() {
        match b {
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(b),
        }
    }
    out.push(b'\'');
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

fn md5_hex(input: &[u8]) -> String {
    crate::install::md5_hex_export(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_autoload_php_with_suffix() {
        let out = render_autoload_php("abc123");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("ComposerAutoloaderInitabc123::getLoader()"));
        assert!(s.contains("require_once __DIR__ . '/composer/autoload_real.php';"));
    }

    #[test]
    fn renders_autoload_real_with_files_block_when_files_present() {
        let with = render_autoload_real("xy", true);
        assert!(std::str::from_utf8(&with).unwrap().contains("$filesToLoad"));
        let without = render_autoload_real("xy", false);
        assert!(!std::str::from_utf8(&without).unwrap().contains("$filesToLoad"));
    }

    #[test]
    fn psr4_buckets_group_by_first_char_and_sort_by_length() {
        let entries = vec![
            ("Foo\\".to_string(), vec![PathBuf::from("/x")]),
            ("Foo\\Bar\\".to_string(), vec![PathBuf::from("/y")]),
            ("Bar\\".to_string(), vec![PathBuf::from("/z")]),
        ];
        let (lengths, _dirs) = build_psr4_buckets(&entries);
        let f_bucket = lengths.get(&'F').unwrap();
        // Longer prefix first.
        assert_eq!(f_bucket[0].0, "Foo\\Bar\\");
        assert_eq!(f_bucket[1].0, "Foo\\");
    }

    #[test]
    fn render_array_file_basic_shape() {
        let entries = vec![("Foo\\".to_string(), vec![PathBuf::from("/proj/vendor/foo/src")])];
        let root = Path::new("/proj");
        let vendor = Path::new("/proj/vendor");
        let bytes = render_simple_array_file("autoload_psr4.php", &entries, root, vendor);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("$vendorDir = dirname(__DIR__);"));
        assert!(s.contains("'Foo\\\\' => array($vendorDir . '/foo/src')"));
    }
}
