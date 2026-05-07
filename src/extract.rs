//! Extract a dist zip into the package's final install path.
//!
//! Composer's algorithm (mirrored here):
//!  1. Empty the target dir if it exists.
//!  2. Extract into a temp dir under `vendor/composer/<random>`.
//!  3. If the archive has a single top-level directory (the GitHub zipball
//!     convention `vendor-name-<sha>/...`), promote its contents up one
//!     level so the package files land directly in `vendor/<vendor>/<name>/`.
//!  4. Move into the final path; remove the temp dir.
//!
//! We also enforce zip-slip protection (no entry path may escape the temp
//! dir). And we preserve unix permissions for executable bits — matters for
//! `bin/` entries.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub struct ExtractResult {
    pub install_path: PathBuf,
    pub bytes_extracted: u64,
}

/// Top-level entry point: extract `archive` (a `.zip` on disk) into the
/// final install path under `vendor_dir`. Uses a unique temp dir under
/// `vendor_dir/composer/` so plugin or installer dirs in vendor don't leak
/// into our staging.
pub fn extract_zip_to_install_path(
    archive: &Path,
    install_path: &Path,
    vendor_dir: &Path,
) -> Result<ExtractResult> {
    // Empty the target dir if present, but only if it's not vendor or one
    // of vendor's ancestors — we'd never install a package on top of vendor
    // itself, but defending against pathological cases is cheap.
    if install_path.exists() {
        if vendor_dir.starts_with(install_path) {
            bail!(
                "refusing to empty {} because vendor dir lives inside it",
                install_path.display()
            );
        }
        empty_dir(install_path).with_context(|| {
            format!("emptying existing dir {}", install_path.display())
        })?;
    }

    let composer_dir = vendor_dir.join("composer");
    fs::create_dir_all(&composer_dir)
        .with_context(|| format!("creating {}", composer_dir.display()))?;
    let tmp = unique_tmp_dir(&composer_dir)?;

    let extract = || -> Result<u64> {
        fs::create_dir_all(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let bytes = extract_zip_into(archive, &tmp)?;
        Ok(bytes)
    };

    let bytes = match extract() {
        Ok(b) => b,
        Err(e) => {
            let _ = remove_dir_best_effort(&tmp);
            return Err(e);
        }
    };

    // Single-top-dir strip: if `tmp/` contains exactly one entry and it's a
    // dir, treat that dir's contents as the package contents.
    let source = match single_top_dir(&tmp)? {
        Some(d) => d,
        None => tmp.clone(),
    };

    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent {}", parent.display()))?;
    }

    // Move source -> install_path. Try a rename first; if cross-device or
    // fails for another reason, fall back to recursive copy.
    if install_path.exists() {
        // empty_dir above should have removed it for non-empty case, but
        // emptying can leave the dir itself; remove it now so rename works.
        let _ = fs::remove_dir(install_path);
    }
    if let Err(e) = fs::rename(&source, install_path) {
        tracing::debug!(
            "rename failed ({e}), falling back to copy: {} -> {}",
            source.display(),
            install_path.display()
        );
        copy_dir_recursive(&source, install_path).with_context(|| {
            format!(
                "copying {} -> {}",
                source.display(),
                install_path.display()
            )
        })?;
    }

    // Cleanup tmp (it might still hold the wrapper dir or stale files).
    let _ = remove_dir_best_effort(&tmp);

    Ok(ExtractResult {
        install_path: install_path.to_path_buf(),
        bytes_extracted: bytes,
    })
}

fn unique_tmp_dir(under: &Path) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(under.join(format!(".dumpr-{pid}-{now:x}-{n}")))
}

fn extract_zip_into(archive: &Path, out_dir: &Path) -> Result<u64> {
    let f = fs::File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f)
        .with_context(|| format!("reading zip {}", archive.display()))?;
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .with_context(|| format!("reading zip entry {i} from {}", archive.display()))?;
        let entry_name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("zip entry {i} has unsafe path"))?
            .to_path_buf();
        if has_traversal(&entry_name) {
            bail!(
                "zip entry {} from {} contains path traversal",
                entry_name.display(),
                archive.display()
            );
        }
        let dest = out_dir.join(&entry_name);
        if entry.is_dir() {
            fs::create_dir_all(&dest)
                .with_context(|| format!("mkdir {}", dest.display()))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let mut out = fs::File::create(&dest)
            .with_context(|| format!("creating {}", dest.display()))?;
        let n = io::copy(&mut entry, &mut out)
            .with_context(|| format!("extracting {}", dest.display()))?;
        total = total.saturating_add(n);

        // Restore unix permissions if present in the zip metadata. Composer
        // archives store +x on bin entries; without this their `bin` proxies
        // wouldn't be runnable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(total)
}

fn has_traversal(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Look at the immediate children of `dir`. If there's exactly one and it's
/// a directory, return it. Skips `.DS_Store` to mirror Composer.
fn single_top_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = entry?;
        if entry.file_name() == ".DS_Store" {
            continue;
        }
        entries.push(entry.path());
        if entries.len() > 1 {
            return Ok(None);
        }
    }
    if entries.len() == 1 && entries[0].is_dir() {
        Ok(Some(entries.pop().unwrap()))
    } else {
        Ok(None)
    }
}

fn empty_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("rm -rf {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("rm {}", path.display()))?;
        }
    }
    Ok(())
}

fn remove_dir_best_effort(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    fs::remove_dir_all(dir).ok();
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("mkdir {}", dst.display()))?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("reading {}", src.display()))?
    {
        let entry = entry?;
        let p = entry.path();
        let dest = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&p, &dest)?;
        } else {
            fs::copy(&p, &dest)
                .with_context(|| format!("cp {} -> {}", p.display(), dest.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::metadata(&p)?.permissions();
                let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(perms.mode()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            for (name, content, mode) in entries {
                let mut opts: zip::write::SimpleFileOptions =
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored);
                if let Some(m) = mode {
                    opts = opts.unix_permissions(*m);
                }
                if name.ends_with('/') {
                    zw.add_directory(*name, opts).unwrap();
                } else {
                    zw.start_file(*name, opts).unwrap();
                    use std::io::Write;
                    zw.write_all(content).unwrap();
                }
            }
            zw.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extracts_with_single_top_dir_strip() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("archive.zip");
        let bytes = make_zip(&[
            ("pkg-abc123/", b"" as &[u8], None),
            ("pkg-abc123/src/", b"" as &[u8], None),
            ("pkg-abc123/src/Foo.php", b"<?php class Foo {}", None),
            ("pkg-abc123/composer.json", b"{}", None),
        ]);
        std::fs::write(&zip_path, &bytes).unwrap();

        let vendor = tmp.path().join("vendor");
        let install = vendor.join("acme/widget");
        let r = extract_zip_to_install_path(&zip_path, &install, &vendor).unwrap();

        assert!(install.join("composer.json").is_file());
        assert!(install.join("src/Foo.php").is_file());
        assert!(!install.join("pkg-abc123").exists());
        assert!(r.bytes_extracted > 0);
    }

    #[test]
    fn extracts_without_top_dir_in_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("a.zip");
        let bytes = make_zip(&[
            ("composer.json", b"{}", None),
            ("src/", b"" as &[u8], None),
            ("src/X.php", b"<?php class X {}", None),
        ]);
        std::fs::write(&zip_path, &bytes).unwrap();

        let vendor = tmp.path().join("vendor");
        let install = vendor.join("acme/x");
        extract_zip_to_install_path(&zip_path, &install, &vendor).unwrap();

        assert!(install.join("composer.json").is_file());
        assert!(install.join("src/X.php").is_file());
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bad.zip");
        // We can't easily forge ../ via add_file (it's stripped), so test
        // has_traversal directly.
        std::fs::write(&zip_path, make_zip(&[("ok.txt", b"x", None)])).unwrap();
        assert!(has_traversal(Path::new("../../etc/passwd")));
        assert!(!has_traversal(Path::new("a/b/c")));
    }
}
