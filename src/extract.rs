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

/// Install a `dist.type == "path"` package: place a symlink at
/// `install_path` pointing to the source directory. Composer's PathDownloader
/// (PathDownloader.php) defaults to `relative: true, symlink: null`, meaning
/// a relative symlink with mirror as a fallback when symlink fails. We mirror
/// that policy.
///
/// `symlink_pref`:
///   - `Some(true)`  → symlink only; error if it fails
///   - `Some(false)` → mirror (recursive copy) only
///   - `None`        → try symlink, fall back to mirror on error
pub fn install_path_package(
    source: &Path,
    install_path: &Path,
    relative: bool,
    symlink_pref: Option<bool>,
) -> Result<()> {
    let canon_source = source.canonicalize().with_context(|| {
        format!("resolving path-dist source {}", source.display())
    })?;

    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Wipe any existing target. Be careful with symlinks: removing the link,
    // not the dir it points to.
    if install_path.is_symlink() || install_path.is_file() {
        fs::remove_file(install_path).with_context(|| {
            format!("removing existing {}", install_path.display())
        })?;
    } else if install_path.exists() {
        fs::remove_dir_all(install_path).with_context(|| {
            format!("removing existing dir {}", install_path.display())
        })?;
    }

    let try_symlink = || -> Result<()> {
        let target: PathBuf = if relative {
            let parent = install_path
                .parent()
                .ok_or_else(|| anyhow!("install path has no parent: {}", install_path.display()))?;
            // Canonicalize the parent so we compare in the same path-prefix
            // namespace as `canon_source` (matters on macOS where /var is a
            // symlink to /private/var; otherwise we'd compute ../../../../...
            // and walk past the filesystem root).
            let canon_parent = parent.canonicalize().with_context(|| {
                format!("canonicalizing install parent {}", parent.display())
            })?;
            PathBuf::from(path_relative_components(&canon_parent, &canon_source))
        } else {
            canon_source.clone()
        };
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, install_path)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            // Fall through to mirror — symlinks on Windows need junctions
            // and elevation that we don't bother with here.
            Err(anyhow!("symlinks unsupported on this platform"))
        }
    };

    let try_mirror = || -> Result<()> {
        copy_dir_recursive(&canon_source, install_path)
    };

    match symlink_pref {
        Some(true) => try_symlink().context("path package: symlink required and failed"),
        Some(false) => try_mirror(),
        None => match try_symlink() {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    "symlink for path package {} failed ({e}); mirroring instead",
                    install_path.display()
                );
                try_mirror()
            }
        },
    }
}

/// Compute a relative path from `from` to `to` as a forward-slash string.
/// Both should be absolute. Used by `install_path_package` to mirror
/// PathDownloader's `findShortestPath` behavior.
fn path_relative_components(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let ups = from.len() - common;
    let mut parts: Vec<String> = (0..ups).map(|_| "..".to_string()).collect();
    for c in to.iter().skip(common) {
        parts.push(c.as_os_str().to_string_lossy().to_string());
    }
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

#[derive(Debug)]
pub struct ExtractResult {
    pub install_path: PathBuf,
    pub bytes_extracted: u64,
    pub entries_extracted: usize,
}

/// Top-level entry point: extract `archive` (a `.zip` on disk) into the
/// final install path under `vendor_dir`. Uses a unique temp dir under
/// `vendor_dir/composer/` so plugin or installer dirs in vendor don't
/// leak into staging, and so finalize is a same-volume `rename`.
///
/// Replaces the existing `install_path` via async-trash: the old contents
/// get an instant `rename` into a vendor-local `.composr-trash-<rand>/`
/// entry, and the actual recursive remove happens in a detached process
/// kicked off by [`spawn_trash_cleanup`] at the end of the install run.
/// This keeps the foreground extract path off the rm-rf hot loop, which
/// for a 113k-entry replace is otherwise a ~12s wait.
pub fn extract_zip_to_install_path(
    archive: &Path,
    install_path: &Path,
    vendor_dir: &Path,
) -> Result<ExtractResult> {
    let composer_dir = vendor_dir.join("composer");
    fs::create_dir_all(&composer_dir)
        .with_context(|| format!("creating {}", composer_dir.display()))?;
    let tmp = unique_tmp_dir(&composer_dir)?;

    // Order: extract first, then swap in. install_path is untouched until
    // we know the new tree is on disk and ready, so a failed extract
    // leaves the old install in place rather than a half-deleted vendor.
    let work = || -> Result<(u64, usize)> {
        fs::create_dir_all(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let (bytes, entries) = extract_zip_into(archive, &tmp)?;

        let source = match single_top_dir(&tmp)? {
            Some(d) => d,
            None => tmp.clone(),
        };

        if let Some(parent) = install_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent {}", parent.display()))?;
        }

        // Move the existing install_path out of the way. CRITICAL: if it's
        // a symlink (a path-dist install we're switching to zip), only
        // remove the link — never touch the link target, which is the
        // user's source dir.
        if install_path.is_symlink() {
            fs::remove_file(install_path).with_context(|| {
                format!("removing existing symlink {}", install_path.display())
            })?;
        } else if install_path.exists() {
            if vendor_dir.starts_with(install_path) {
                bail!(
                    "refusing to trash {} because vendor dir lives inside it",
                    install_path.display()
                );
            }
            let trash = unique_dir(vendor_dir, ".composr-trash-")?;
            fs::rename(install_path, &trash).with_context(|| {
                format!(
                    "trashing {} -> {}",
                    install_path.display(),
                    trash.display()
                )
            })?;
        }

        // Move the new tree into place. Same volume (tmp lives under
        // vendor/composer/), so rename almost always wins; copy is the
        // cross-fs fallback.
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

        Ok((bytes, entries))
    };

    let (bytes, entries) = match work() {
        Ok(t) => t,
        Err(e) => {
            let _ = remove_dir_best_effort(&tmp);
            return Err(e);
        }
    };

    let _ = remove_dir_best_effort(&tmp);

    Ok(ExtractResult {
        install_path: install_path.to_path_buf(),
        bytes_extracted: bytes,
        entries_extracted: entries,
    })
}

/// Spawn a detached `/bin/rm -rf` over every `<vendor>/.composr-trash-*/`
/// entry (both this run's trash and any orphans from prior incomplete
/// runs). Returns immediately; the child outlives this process — on
/// Unix it gets reparented to launchd/init when we exit.
///
/// Idempotent: if no trash dirs exist, this is a no-op. Safe to call
/// even if the previous install bailed mid-rename.
pub fn spawn_trash_cleanup(vendor_dir: &Path) {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(vendor_dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".composr-trash-")
            {
                paths.push(entry.path());
            }
        }
    }
    if paths.is_empty() {
        return;
    }
    #[cfg(unix)]
    {
        let mut cmd = std::process::Command::new("/bin/rm");
        cmd.arg("-rf");
        for p in &paths {
            cmd.arg(p);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let _ = cmd.spawn();
    }
    #[cfg(not(unix))]
    {
        for p in &paths {
            let _ = fs::remove_dir_all(p);
        }
    }
}

/// Returns `(bytes_written, file_entries_extracted)`. Directory-only
/// entries don't count toward the entries number — it's "files written".
fn extract_zip_into(archive: &Path, out_dir: &Path) -> Result<(u64, usize)> {
    let f = fs::File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f)
        .with_context(|| format!("reading zip {}", archive.display()))?;

    // Two-pass: first walk dedups directories so we mkdir each unique
    // parent once instead of once per file (a typical PHP archive has
    // ~10x more file entries than unique dirs). Then a second walk
    // replays by index for the actual content writes.
    struct Entry {
        idx: usize,
        dest: PathBuf,
        unix_mode: Option<u32>,
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(zip.len());
    let mut unique_dirs: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    for i in 0..zip.len() {
        let entry = zip
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
            unique_dirs.insert(dest);
            continue;
        }
        if let Some(parent) = dest.parent() {
            if !unique_dirs.contains(parent) {
                unique_dirs.insert(parent.to_path_buf());
            }
        }
        entries.push(Entry {
            idx: i,
            dest,
            unix_mode: entry.unix_mode(),
        });
    }

    for d in &unique_dirs {
        fs::create_dir_all(d)
            .with_context(|| format!("mkdir {}", d.display()))?;
    }

    let mut total: u64 = 0;
    for ent in &entries {
        let mut entry = zip
            .by_index(ent.idx)
            .with_context(|| format!("reading entry {} from {}", ent.idx, archive.display()))?;
        let mut out = fs::File::create(&ent.dest)
            .with_context(|| format!("creating {}", ent.dest.display()))?;
        let n = io::copy(&mut entry, &mut out)
            .with_context(|| format!("extracting {}", ent.dest.display()))?;
        total = total.saturating_add(n);
        // Restore unix permissions from the zip metadata. Skip the syscall
        // for plain 0o644 — `File::create` + default umask 022 already
        // produces that mode, so calling `set_permissions` would be a
        // no-op. Keeps the +x bits intact for `bin/*` entries.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = ent.unix_mode {
                if mode & 0o777 != 0o644 {
                    let _ = fs::set_permissions(
                        &ent.dest,
                        fs::Permissions::from_mode(mode),
                    );
                }
            }
        }
    }
    Ok((total, entries.len()))
}

fn unique_tmp_dir(under: &Path) -> Result<PathBuf> {
    unique_dir(under, ".dumpr-")
}

fn unique_dir(under: &Path, prefix: &str) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(under.join(format!("{prefix}{pid}-{now:x}-{n}")))
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
    #[cfg(unix)]
    fn path_install_creates_relative_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        let source = proj.join("lib/local-pkg");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("src/Foo.php"), b"<?php class Foo {}").unwrap();
        fs::write(source.join("composer.json"), b"{}").unwrap();
        let install = proj.join("vendor/acme/local-pkg");
        install_path_package(&source, &install, true, None).unwrap();
        assert!(install.is_symlink());
        let target = fs::read_link(&install).unwrap();
        // Should be relative.
        assert!(target.is_relative(), "target {} should be relative", target.display());
        // Resolve and verify file is reachable through the symlink.
        assert!(install.join("src/Foo.php").exists());
        assert!(install.join("composer.json").exists());
    }

    #[test]
    #[cfg(unix)]
    fn path_install_replaces_existing_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        let source_a = proj.join("lib/a");
        let source_b = proj.join("lib/b");
        fs::create_dir_all(&source_a).unwrap();
        fs::create_dir_all(&source_b).unwrap();
        fs::write(source_a.join("a.txt"), b"a").unwrap();
        fs::write(source_b.join("b.txt"), b"b").unwrap();
        let install = proj.join("vendor/x/y");
        install_path_package(&source_a, &install, true, None).unwrap();
        install_path_package(&source_b, &install, true, None).unwrap();
        assert!(install.join("b.txt").exists());
        assert!(!install.join("a.txt").exists());
        // The original source_a must still have its file — symlink replace
        // must not have followed the old link.
        assert!(source_a.join("a.txt").exists());
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
