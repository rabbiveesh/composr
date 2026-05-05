use crate::config::PackageAutoload;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// One file the walker found that needs to be considered for the classmap, plus
/// the (optional) PSR prefix/source-root pair that brought it in. The prefix is
/// kept so we can report ambiguous declarations (`namespace doesn't match psr-4
/// prefix Foo\`) the way Composer does.
#[derive(Debug, Clone)]
pub struct PhpFile {
    pub path: PathBuf,
    pub mtime_ns: u128,
    pub size: u64,
    pub source: PhpSource,
}

#[derive(Debug, Clone)]
pub enum PhpSource {
    /// File reached through a PSR-4 prefix → directory mapping.
    Psr4 { prefix: String, root: PathBuf },
    /// File reached through a PSR-0 prefix → directory mapping.
    Psr0 { prefix: String, root: PathBuf },
    /// File reached through a `classmap` entry (dir or file).
    Classmap,
}

/// Compile Composer's `exclude-from-classmap` patterns into a globset. Composer
/// historically treats these as regex-ish — anchored to the start of the
/// vendor-relative path with `**/` style wildcards. Globset's default semantics
/// match closely enough for almost all real-world patterns; we additionally
/// strip a leading `/` so patterns like `/Tests/` match anywhere.
pub fn build_exclude_globset(patterns: &[String]) -> anyhow::Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        // Composer accepts both `/Tests/` (anchored) and `**/Tests/**` style.
        // We normalize to `**/<stripped>**` so they match anywhere in the path.
        let stripped = p.trim_start_matches('/');
        // If user wrote `Tests/` add `**` to allow trailing path components.
        let with_globs = if stripped.contains('*') {
            stripped.to_string()
        } else if stripped.ends_with('/') {
            format!("**/{}**", stripped)
        } else {
            format!("**/{}**", stripped)
        };
        b.add(Glob::new(&with_globs)?);
    }
    Ok(b.build()?)
}

/// Walk all autoload sources for a single package, returning every `.php` file
/// that's a candidate for the classmap. Excludes are applied here.
pub fn collect_files(pkg: &PackageAutoload) -> anyhow::Result<Vec<PhpFile>> {
    let exclude = build_exclude_globset(&pkg.exclude_from_classmap)?;

    let mut out = Vec::new();

    for (prefix, dirs) in &pkg.psr_4 {
        for dir in dirs {
            walk_dir(
                dir,
                &exclude,
                PhpSource::Psr4 {
                    prefix: prefix.clone(),
                    root: dir.clone(),
                },
                &mut out,
            );
        }
    }
    for (prefix, dirs) in &pkg.psr_0 {
        for dir in dirs {
            walk_dir(
                dir,
                &exclude,
                PhpSource::Psr0 {
                    prefix: prefix.clone(),
                    root: dir.clone(),
                },
                &mut out,
            );
        }
    }
    for entry in &pkg.classmap {
        if entry.is_file() {
            if let Some(f) = make_file(entry, PhpSource::Classmap) {
                if !is_excluded(&exclude, entry) {
                    out.push(f);
                }
            }
        } else if entry.is_dir() {
            walk_dir(entry, &exclude, PhpSource::Classmap, &mut out);
        }
    }

    Ok(out)
}

fn walk_dir(dir: &Path, exclude: &GlobSet, source: PhpSource, out: &mut Vec<PhpFile>) {
    if !dir.exists() {
        // Composer tolerates missing PSR-4 roots (warns); we just skip.
        tracing::debug!("autoload root missing: {}", dir.display());
        return;
    }
    let walker = WalkBuilder::new(dir)
        .standard_filters(false) // composer ignores .gitignore — match it
        .hidden(false)
        .follow_links(true)
        .build();

    for ent in walker {
        let ent = match ent {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("walk error: {e}");
                continue;
            }
        };
        let path = ent.path();
        let ftype = match ent.file_type() {
            Some(t) => t,
            None => continue,
        };
        if !ftype.is_file() {
            continue;
        }
        if !is_php(path) {
            continue;
        }
        if is_excluded(exclude, path) {
            continue;
        }
        if let Some(f) = make_file(path, source.clone()) {
            out.push(f);
        }
    }
}

fn is_php(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()) == Some("php")
}

fn is_excluded(exclude: &GlobSet, path: &Path) -> bool {
    if exclude.is_empty() {
        return false;
    }
    // Match against the absolute path; patterns like `**/Tests/**` cope with
    // any prefix.
    exclude.is_match(path)
}

fn make_file(path: &Path, source: PhpSource) -> Option<PhpFile> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let mtime_ns = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(PhpFile {
        path: path.to_path_buf(),
        mtime_ns,
        size: md.len(),
        source,
    })
}
