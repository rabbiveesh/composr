//! Install package binaries into `vendor/bin/`, generating proxy files that
//! mirror Composer's behavior closely enough that PHP CLIs (phpunit, pest,
//! pint, etc.) work the same as if Composer had installed them.
//!
//! For PHP bins (those whose first 500 bytes match `(#!.*\n)?<?php`), we emit
//! a PHP proxy that pre-defines `$GLOBALS['_composer_bin_dir']` and
//! `$GLOBALS['_composer_autoload_path']`, then `include`s the actual file —
//! same shape Composer generates. For non-PHP bins, we emit a `sh` exec
//! wrapper.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Install one package's bins. Returns the absolute paths of the proxy files
/// written, so callers can audit / clean them up later.
pub fn install_package_bins(
    package_name: &str,
    install_path: &Path,
    bin_entries: &[String],
    vendor_bin_dir: &Path,
    vendor_dir: &Path,
) -> Result<Vec<PathBuf>> {
    if bin_entries.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(vendor_bin_dir)
        .with_context(|| format!("creating {}", vendor_bin_dir.display()))?;

    let autoload_path = vendor_dir.join("autoload.php");

    let mut written = Vec::new();
    for bin in bin_entries {
        let bin_target = install_path.join(bin);
        if !bin_target.exists() {
            tracing::warn!(
                "skipped bin {bin} for {package_name}: file not found at {}",
                bin_target.display()
            );
            continue;
        }
        if bin_target.is_dir() {
            tracing::warn!(
                "skipped bin {bin} for {package_name}: target is a directory"
            );
            continue;
        }

        let proxy_name = Path::new(bin)
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_else(|| bin.into());
        let link = vendor_bin_dir.join(&proxy_name);

        // Remove any existing proxy. We never blow away an unrelated file
        // — but anything in vendor/bin that's a Composer-style proxy
        // (ours, composer's, or a symlink) is by construction equivalent
        // to what we'd write, so it's safe to replace.
        if link.exists() {
            match classify_proxy(&link) {
                ProxyKind::Ours | ProxyKind::Symlink => {
                    fs::remove_file(&link).ok();
                }
                ProxyKind::ComposerCompat => {
                    // Same functional shape as ours; differs only in
                    // comment headers / boilerplate. Log so anything weird
                    // surfaces under --verbose.
                    tracing::debug!(
                        "replacing composer-shape proxy at {} with composr's (functionally equivalent)",
                        link.display()
                    );
                    fs::remove_file(&link).ok();
                }
                ProxyKind::Foreign => {
                    tracing::warn!(
                        "skipped bin {bin} for {package_name}: {} exists and isn't ours",
                        link.display()
                    );
                    continue;
                }
            }
        }

        let is_php = looks_like_php_file(&bin_target).unwrap_or(false);
        let body = if is_php {
            generate_php_proxy(&link, &bin_target, &autoload_path)
        } else {
            generate_shell_proxy(&link, &bin_target)
        };
        fs::write(&link, body)
            .with_context(|| format!("writing proxy {}", link.display()))?;

        // Ensure the bin source itself is executable — Composer does this in
        // case the archive lost the +x bit during transport.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&bin_target, fs::Permissions::from_mode(0o755));
            let _ = fs::set_permissions(&link, fs::Permissions::from_mode(0o755));
        }
        written.push(link);
    }
    Ok(written)
}

pub fn remove_package_bins(
    bin_entries: &[String],
    vendor_bin_dir: &Path,
) -> Result<()> {
    for bin in bin_entries {
        let proxy_name = Path::new(bin)
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_else(|| bin.into());
        let link = vendor_bin_dir.join(&proxy_name);
        if link.exists() {
            // On package removal, clean up any Composer-style proxy
            // pointing at it (ours, composer's, or a symlink) — they
            // all become broken when the package goes away.
            if !matches!(classify_proxy(&link), ProxyKind::Foreign) {
                let _ = fs::remove_file(&link);
            }
        }
    }
    Ok(())
}

fn looks_like_php_file(p: &Path) -> Result<bool> {
    use std::io::Read;
    let mut f = fs::File::open(p)?;
    let mut buf = [0u8; 500];
    let n = f.read(&mut buf)?;
    let s = String::from_utf8_lossy(&buf[..n]);
    // Accept either `<?php` directly or shebang followed by `<?php`.
    let trimmed = s.trim_start();
    if trimmed.starts_with("<?php") {
        return Ok(true);
    }
    if let Some(after_shebang) = trimmed.strip_prefix("#!") {
        if let Some((_, rest)) = after_shebang.split_once('\n') {
            if rest.trim_start().starts_with("<?php") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// What's currently sitting at a `vendor/bin/<name>` path, from our
/// perspective. Used to decide whether replacing it is safe.
#[derive(Debug, PartialEq, Eq)]
enum ProxyKind {
    /// A symlink — always safe to drop and rewrite.
    Symlink,
    /// composr wrote this (PHP proxy or shell shim). Silent overwrite.
    Ours,
    /// Composer (or another composer-compatible tool) wrote this — same
    /// functional shape as ours but different boilerplate. Safe to
    /// overwrite, but worth a debug log so anything unexpected surfaces.
    ComposerCompat,
    /// Something we don't recognize — leave it alone.
    Foreign,
}

fn classify_proxy(p: &Path) -> ProxyKind {
    if p.is_symlink() {
        return ProxyKind::Symlink;
    }
    let bytes = match fs::read(p) {
        Ok(b) => b,
        Err(_) => return ProxyKind::Foreign,
    };
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);

    // Our own headers — both "Proxy PHP file generated by composr ..." and
    // "Proxy generated by composr ..." (shell shims) match this substring.
    if head.contains("generated by composr") {
        return ProxyKind::Ours;
    }

    // Composer-shape PHP proxy. The robust fingerprint is the global
    // every composer bin proxy assigns — header text has drifted across
    // composer versions, but this assignment is stable and unique to bin
    // proxies, so it's a reliable signal that the file is a proxy
    // pointing at *some* package's bin (not user code).
    if head.contains("$GLOBALS['_composer_autoload_path']")
        || head.contains("Proxy PHP file generated by Composer")
    {
        return ProxyKind::ComposerCompat;
    }

    ProxyKind::Foreign
}

/// Render a relative path from `from`'s directory to `to`, using forward
/// slashes regardless of platform — Composer does the same.
fn shortest_relative(from: &Path, to: &Path) -> String {
    let from_parent = from.parent().unwrap_or(from);
    let mut from_components: Vec<String> = from_parent
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let mut to_components: Vec<String> = to
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let mut common = 0;
    let limit = from_components.len().min(to_components.len());
    while common < limit && from_components[common] == to_components[common] {
        common += 1;
    }
    let ups = from_components.len() - common;
    from_components.truncate(0);
    let mut parts: Vec<String> = (0..ups).map(|_| "..".to_string()).collect();
    parts.extend(to_components.drain(common..));
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

fn generate_php_proxy(link: &Path, bin: &Path, autoload: &Path) -> Vec<u8> {
    let bin_rel = shortest_relative(link, bin);
    let autoload_rel = shortest_relative(link, autoload);
    let bin_export = format!("dirname(__FILE__).'/{bin_rel}'");
    let autoload_export = format!("dirname(__FILE__).'/{autoload_rel}'");
    let body = format!(
        "#!/usr/bin/env php\n\
<?php\n\
\n\
/**\n\
 * Proxy PHP file generated by composr (Composer-compatible)\n\
 *\n\
 * This file includes the referenced bin path ({bin_rel})\n\
 *\n\
 * @generated\n\
 */\n\
\n\
namespace Composer;\n\
\n\
$GLOBALS['_composer_bin_dir'] = __DIR__;\n\
$GLOBALS['_composer_autoload_path'] = {autoload_export};\n\
\n\
return include {bin_export};\n",
        autoload_export = autoload_export,
        bin_export = bin_export,
        bin_rel = bin_rel,
    );
    body.into_bytes()
}

fn generate_shell_proxy(link: &Path, bin: &Path) -> Vec<u8> {
    let bin_rel = shortest_relative(link, bin);
    let body = format!(
        "#!/usr/bin/env sh\n\
# Proxy generated by composr (Composer-compatible shell shim)\n\
self=\"$0\"\n\
dir=$(cd \"${{self%/*}}\" > /dev/null; pwd)\n\
exec \"$dir/{bin_rel}\" \"$@\"\n",
    );
    body.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_php_file_with_shebang() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"#!/usr/bin/env php\n<?php\necho 1;\n").unwrap();
        assert!(looks_like_php_file(tmp.path()).unwrap());
    }

    #[test]
    fn detects_php_file_without_shebang() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"<?php\necho 1;\n").unwrap();
        assert!(looks_like_php_file(tmp.path()).unwrap());
    }

    #[test]
    fn detects_non_php_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"#!/bin/sh\necho 1\n").unwrap();
        assert!(!looks_like_php_file(tmp.path()).unwrap());
    }

    #[test]
    fn classify_proxy_recognizes_our_php_header() {
        // Mirror of what `generate_php_proxy` actually writes today.
        // Catches the regression where the recognizer text drifted from
        // the writer text — e.g. the original `is_our_proxy_or_link`
        // looked for "generated by Composer" while we wrote
        // "generated by composr". (Yes, that bug was real.)
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            b"#!/usr/bin/env php\n<?php\n\n/**\n* Proxy PHP file generated by composr (Composer-compatible)\n*/\n",
        )
        .unwrap();
        assert_eq!(classify_proxy(tmp.path()), ProxyKind::Ours);
    }

    #[test]
    fn classify_proxy_recognizes_our_shell_header() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            b"#!/bin/sh\n# Proxy generated by composr (Composer-compatible shell shim)\nexec ...\n",
        )
        .unwrap();
        assert_eq!(classify_proxy(tmp.path()), ProxyKind::Ours);
    }

    #[test]
    fn classify_proxy_recognizes_composer_php_proxy() {
        // Modern composer's actual proxy shape — the global assignment
        // is the stable fingerprint regardless of comment-text drift.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            b"#!/usr/bin/env php\n<?php\n\
              /**\n * Proxy PHP file generated by Composer\n */\n\
              $GLOBALS['_composer_autoload_path'] = __DIR__ . '/../autoload.php';\n\
              include __DIR__ . '/../foo/bin/foo';\n",
        )
        .unwrap();
        assert_eq!(classify_proxy(tmp.path()), ProxyKind::ComposerCompat);
    }

    #[test]
    fn classify_proxy_treats_random_php_as_foreign() {
        // A PHP file that's NOT a bin proxy — say, the user accidentally
        // dropped a script in vendor/bin/. Don't touch.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"<?php\necho 'hello';\n").unwrap();
        assert_eq!(classify_proxy(tmp.path()), ProxyKind::Foreign);
    }

    #[test]
    fn shortest_relative_basic() {
        let from = Path::new("/proj/vendor/bin/phpunit");
        let to = Path::new("/proj/vendor/phpunit/phpunit/phpunit");
        assert_eq!(shortest_relative(from, to), "../phpunit/phpunit/phpunit");
    }

    #[test]
    fn install_writes_php_proxy() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("vendor/acme/widget");
        fs::create_dir_all(&pkg).unwrap();
        let bin_target = pkg.join("bin/widget");
        fs::create_dir_all(bin_target.parent().unwrap()).unwrap();
        fs::write(&bin_target, b"#!/usr/bin/env php\n<?php\necho 1;\n").unwrap();
        let bindir = tmp.path().join("vendor/bin");
        let written = install_package_bins(
            "acme/widget",
            &pkg,
            &["bin/widget".to_string()],
            &bindir,
            &tmp.path().join("vendor"),
        )
        .unwrap();
        assert_eq!(written.len(), 1);
        let content = fs::read_to_string(&written[0]).unwrap();
        assert!(content.contains("$GLOBALS['_composer_bin_dir']"));
        assert!(content.contains("autoload.php"));
    }
}
