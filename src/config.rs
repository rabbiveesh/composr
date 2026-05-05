use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
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

        let mut packages = Vec::new();

        let root_composer = root.join("composer.json");
        if root_composer.exists() {
            packages.push(load_package(&root_composer, &root, include_dev)?);
        } else {
            anyhow::bail!("no composer.json at {}", root_composer.display());
        }

        if vendor_dir.is_dir() {
            for vendor in std::fs::read_dir(&vendor_dir)? {
                let vendor = vendor?;
                if !vendor.file_type()?.is_dir() {
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
                    if !pkg.file_type()?.is_dir() {
                        continue;
                    }
                    let composer_json = pkg_path.join("composer.json");
                    if composer_json.exists() {
                        // vendor packages never use autoload-dev (composer ignores it
                        // for non-root packages), so always pass false.
                        match load_package(&composer_json, &pkg_path, false) {
                            Ok(p) => packages.push(p),
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

fn load_package(
    composer_json: &Path,
    pkg_root: &Path,
    include_dev: bool,
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
                entry.push(pkg_root.join(p));
            }
        }
        for (prefix, paths) in &sec.psr_0 {
            let entry = psr_0.entry(prefix.clone()).or_default();
            for p in paths.as_slice() {
                entry.push(pkg_root.join(p));
            }
        }
        for c in &sec.classmap {
            classmap.push(pkg_root.join(c));
        }
        for f in &sec.files {
            files.push(pkg_root.join(f));
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
    })
}
