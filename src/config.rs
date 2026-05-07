use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Parsed `autoload` section of a composer.json (or `autoload-dev`).
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct AutoloadSection {
    /// PSR-4 prefix → one path or array of paths (relative to package root).
    #[serde(rename = "psr-4")]
    pub psr_4: BTreeMap<String, StringOrVec>,
    /// PSR-0 prefix → one path or array of paths (relative to package root).
    #[serde(rename = "psr-0")]
    pub psr_0: BTreeMap<String, StringOrVec>,
    /// Classmap dirs/files (relative to package root).
    pub classmap: Vec<String>,
    /// File-include list (relative to package root).
    pub files: Vec<String>,
    /// Glob patterns (relative to package root) to exclude when scanning classmaps.
    #[serde(rename = "exclude-from-classmap")]
    pub exclude_from_classmap: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    pub fn as_slice(&self) -> Vec<&str> {
        match self {
            StringOrVec::One(s) => vec![s.as_str()],
            StringOrVec::Many(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ComposerJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    autoload: Option<AutoloadSection>,
    #[serde(default, rename = "autoload-dev")]
    autoload_dev: Option<AutoloadSection>,
    #[serde(default, rename = "target-dir")]
    target_dir: Option<String>,
}

/// One package's autoload contribution, with paths resolved to absolute form.
#[derive(Debug, Clone)]
pub struct PackageAutoload {
    pub name: String,
    pub root: PathBuf,
    /// PSR-4 prefix → list of absolute directories.
    pub psr_4: Vec<(String, Vec<PathBuf>)>,
    /// PSR-0 prefix → list of absolute directories.
    pub psr_0: Vec<(String, Vec<PathBuf>)>,
    /// Absolute paths of classmap dirs/files.
    pub classmap: Vec<PathBuf>,
    /// Absolute paths of files to be `require`d at autoload bootstrap.
    pub files: Vec<PathBuf>,
    /// Globset of paths excluded from classmap scanning. Patterns are absolute glob
    /// expressions in the same form Composer accepts (regex-ish; we approximate with
    /// globset). Empty if none.
    pub exclude_from_classmap: Vec<String>,
    /// PSR-0 `target-dir` legacy escape hatch — only ever set on the root package
    /// historically. Kept for completeness; we don't use it for classmap purposes.
    pub target_dir: Option<String>,
    /// `dist.reference` (or `source.reference`) from
    /// `vendor/composer/installed.json` — a stable per-version content
    /// fingerprint, used as the cache key for the per-package classmap
    /// cache. `None` for the root package, path-dist installs, or when
    /// installed.json is absent / lacks a reference.
    pub reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub root: PathBuf,
    pub vendor_dir: PathBuf,
    pub packages: Vec<PackageAutoload>,
}

impl ResolvedConfig {
    /// Read the project's root composer.json plus every installed package's
    /// composer.json under `<root>/vendor/<vendor>/<package>/composer.json`.
    pub fn load(root: &Path, include_dev: bool) -> Result<Self> {
        let root = root.canonicalize().with_context(|| {
            format!("could not canonicalize project root {}", root.display())
        })?;
        let vendor_dir = root.join("vendor");

        let references = read_installed_references(&vendor_dir);

        let mut packages = Vec::new();

        let root_composer = root.join("composer.json");
        if root_composer.exists() {
            // Root never has a reference — see PackageAutoload.reference docs.
            packages.push(load_package(&root_composer, &root, include_dev, None)?);
        } else {
            anyhow::bail!("no composer.json at {}", root_composer.display());
        }

        if vendor_dir.is_dir() {
            for vendor in std::fs::read_dir(&vendor_dir)? {
                let vendor = vendor?;
                // Follow symlinks: composer's `path` repositories install
                // packages as symlinks pointing at sibling checkouts, and
                // `file_type()` reports the symlink itself (not its target).
                if !is_dir_following_symlinks(&vendor.path()) {
                    continue;
                }
                let vname = vendor.file_name();
                if vname == "bin" {
                    // vendor/bin is a symlink dir for executables, not packages.
                    // (vendor/composer DOES contain real packages like
                    // composer/ca-bundle alongside its scaffolding files —
                    // we filter the scaffolding by `composer.json` presence.)
                    continue;
                }
                for pkg in std::fs::read_dir(vendor.path())? {
                    let pkg = pkg?;
                    let pkg_path = pkg.path();
                    if !is_dir_following_symlinks(&pkg_path) {
                        continue;
                    }
                    let composer_json = pkg_path.join("composer.json");
                    if composer_json.exists() {
                        // vendor packages never use autoload-dev (composer ignores it
                        // for non-root packages), so always pass false.
                        // Look up reference by package name (we use the
                        // composer.json `name` field after loading; pre-flight
                        // here we hand the whole map and let load_package pick).
                        match load_package(&composer_json, &pkg_path, false, None) {
                            Ok(mut p) => {
                                p.reference = references.get(&p.name).cloned();
                                packages.push(p);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "skipping malformed {}: {e}",
                                    composer_json.display()
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(ResolvedConfig {
            root,
            vendor_dir,
            packages,
        })
    }
}

/// Like `Path::is_dir()`, but explicitly follows symlinks. We want this for
/// vendor package directories — composer's `path` repository type symlinks
/// packages from sibling checkouts, and `DirEntry::file_type()` reports the
/// symlink itself rather than the target.
fn is_dir_following_symlinks(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

/// Join `relative` onto `base` and lexically remove `.` components — without
/// canonicalizing (which would resolve symlinks). Composer normalizes paths
/// like `"Console": "./"` to drop the `./`, and our output paths must match
/// byte-for-byte. We cannot canonicalize because `path` repositories install
/// vendor packages as symlinks, and resolving those would put outputs under
/// the symlink target rather than under `vendor/`.
fn lexical_join(base: &Path, relative: &str) -> PathBuf {
    let joined = base.join(relative);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {} // drop `.`
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Build a name → reference map from `vendor/composer/installed.json`.
/// Prefers `dist.reference`; falls back to `source.reference`. Skips
/// `dist.type == "path"` because path-installed packages have no stable
/// reference. Empty map if installed.json is absent or malformed —
/// callers treat missing references as "uncacheable", so this degrades
/// gracefully (we just skip the per-package classmap cache).
fn read_installed_references(vendor_dir: &Path) -> HashMap<String, String> {
    let path = vendor_dir.join("composer").join("installed.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return HashMap::new(),
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let packages = v
        .get("packages")
        .and_then(|x| x.as_array())
        .or_else(|| v.as_array());
    let Some(packages) = packages else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(packages.len());
    for p in packages {
        let Some(name) = p.get("name").and_then(|x| x.as_str()) else {
            continue;
        };
        // Path installs have a synthetic reference but the contents may
        // change between runs without it changing — don't cache.
        if p.get("dist").and_then(|d| d.get("type")).and_then(|x| x.as_str())
            == Some("path")
        {
            continue;
        }
        let reference = p
            .get("dist")
            .and_then(|d| d.get("reference"))
            .and_then(|x| x.as_str())
            .or_else(|| {
                p.get("source")
                    .and_then(|s| s.get("reference"))
                    .and_then(|x| x.as_str())
            });
        if let Some(r) = reference {
            if !r.is_empty() {
                out.insert(name.to_string(), r.to_string());
            }
        }
    }
    out
}

fn load_package(
    composer_json: &Path,
    pkg_root: &Path,
    include_dev: bool,
    reference: Option<String>,
) -> Result<PackageAutoload> {
    let bytes = std::fs::read(composer_json)
        .with_context(|| format!("reading {}", composer_json.display()))?;
    let parsed: ComposerJson = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", composer_json.display()))?;

    let mut psr_4: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut psr_0: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut classmap: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();

    let mut sections: Vec<&AutoloadSection> = Vec::new();
    if let Some(a) = &parsed.autoload {
        sections.push(a);
    }
    if include_dev {
        if let Some(a) = &parsed.autoload_dev {
            sections.push(a);
        }
    }

    for sec in sections {
        for (prefix, paths) in &sec.psr_4 {
            let entry = psr_4.entry(prefix.clone()).or_default();
            for p in paths.as_slice() {
                entry.push(lexical_join(pkg_root, p));
            }
        }
        for (prefix, paths) in &sec.psr_0 {
            let entry = psr_0.entry(prefix.clone()).or_default();
            for p in paths.as_slice() {
                entry.push(lexical_join(pkg_root, p));
            }
        }
        for c in &sec.classmap {
            classmap.push(lexical_join(pkg_root, c));
        }
        for f in &sec.files {
            files.push(lexical_join(pkg_root, f));
        }
        for x in &sec.exclude_from_classmap {
            excludes.push(x.clone());
        }
    }

    Ok(PackageAutoload {
        name: parsed.name.unwrap_or_else(|| {
            pkg_root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        root: pkg_root.to_path_buf(),
        psr_4: psr_4.into_iter().collect(),
        psr_0: psr_0.into_iter().collect(),
        classmap,
        files,
        exclude_from_classmap: excludes,
        target_dir: parsed.target_dir,
        reference,
    })
}
