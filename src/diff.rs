//! Compute the install/remove operations needed to bring vendor/ into the
//! state the lock describes.
//!
//! Composer thinks in terms of install/update/uninstall operations, but for a
//! pure-`install` (lock-driven, no solver step) we only need install + remove.
//! `update` collapses to remove-then-install — same end state, simpler code.

use crate::installed::InstalledJson;
use crate::lock::{Lock, LockedPackage};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum Op<'a> {
    /// Package is missing or has the wrong reference — fetch + extract.
    Install(&'a LockedPackage),
    /// Package is in vendor/ but not in the lock — remove its dir.
    Remove { name: String },
}

/// Returns the install/remove ops in the order they should be applied
/// (removes first, then installs — matches Composer's "remove obsolete dirs
/// before extracting fresh content" ordering).
pub fn compute_ops<'a>(
    lock: &'a Lock,
    installed: &InstalledJson,
    with_dev: bool,
) -> Vec<Op<'a>> {
    let mut ops: Vec<Op<'a>> = Vec::new();
    let installed_refs: HashMap<String, String> = installed.refs();
    let lock_names: std::collections::HashSet<String> = lock
        .packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .map(|p| p.name.clone())
        .collect();

    // Removes: anything currently installed that isn't in the lock (or that's
    // dev-only when we're installing without --dev).
    for installed_name in installed_refs.keys() {
        if !lock_names.contains(installed_name) {
            ops.push(Op::Remove {
                name: installed_name.clone(),
            });
        }
    }

    // Installs: missing or reference mismatch.
    for pkg in &lock.packages {
        if !with_dev && pkg.dev {
            continue;
        }
        // Metapackages have no files to install.
        if pkg.package_type == "metapackage" {
            continue;
        }
        let want_ref = pkg
            .dist
            .as_ref()
            .map(|d| d.reference.as_str())
            .unwrap_or("");
        match installed_refs.get(&pkg.name) {
            Some(have) if have == want_ref && !want_ref.is_empty() => continue,
            _ => ops.push(Op::Install(pkg)),
        }
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock_with(packages: Vec<(&str, &str, &str, bool)>) -> Lock {
        let mut raw_pkgs: Vec<serde_json::Value> = Vec::new();
        let mut raw_dev: Vec<serde_json::Value> = Vec::new();
        for (name, ver, dist_ref, dev) in packages {
            let v = json!({
                "name": name,
                "version": ver,
                "type": "library",
                "dist": {
                    "type": "zip",
                    "url": format!("https://x/{name}.zip"),
                    "reference": dist_ref,
                    "shasum": ""
                }
            });
            if dev {
                raw_dev.push(v);
            } else {
                raw_pkgs.push(v);
            }
        }
        let bytes = serde_json::to_vec(&json!({
            "packages": raw_pkgs,
            "packages-dev": raw_dev,
        }))
        .unwrap();
        Lock::parse(&bytes).unwrap()
    }

    fn installed_with(packages: Vec<(&str, &str)>) -> InstalledJson {
        let mut pkgs = Vec::new();
        for (name, dist_ref) in packages {
            pkgs.push(json!({
                "name": name,
                "dist": {"reference": dist_ref}
            }));
        }
        InstalledJson {
            packages: pkgs,
            dev: false,
            dev_package_names: vec![],
        }
    }

    #[test]
    fn fresh_install_when_nothing_installed() {
        let lock = lock_with(vec![("a/b", "1.0", "ref1", false)]);
        let installed = installed_with(vec![]);
        let ops = compute_ops(&lock, &installed, false);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], Op::Install(p) if p.name == "a/b"));
    }

    #[test]
    fn skips_unchanged_package() {
        let lock = lock_with(vec![("a/b", "1.0", "ref1", false)]);
        let installed = installed_with(vec![("a/b", "ref1")]);
        assert_eq!(compute_ops(&lock, &installed, false).len(), 0);
    }

    #[test]
    fn reinstalls_on_ref_mismatch() {
        let lock = lock_with(vec![("a/b", "1.0", "ref2", false)]);
        let installed = installed_with(vec![("a/b", "ref1")]);
        let ops = compute_ops(&lock, &installed, false);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], Op::Install(_)));
    }

    #[test]
    fn removes_packages_dropped_from_lock() {
        let lock = lock_with(vec![]);
        let installed = installed_with(vec![("orphan/x", "ref")]);
        let ops = compute_ops(&lock, &installed, false);
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], Op::Remove { name } if name == "orphan/x"));
    }

    #[test]
    fn no_dev_filters_out_dev_packages() {
        let lock = lock_with(vec![
            ("a/b", "1.0", "r1", false),
            ("c/d", "1.0", "r2", true),
        ]);
        let installed = installed_with(vec![]);
        let ops_no_dev = compute_ops(&lock, &installed, false);
        assert_eq!(ops_no_dev.len(), 1);
        let ops_dev = compute_ops(&lock, &installed, true);
        assert_eq!(ops_dev.len(), 2);
    }

    #[test]
    fn metapackage_skipped() {
        let bytes = serde_json::to_vec(&json!({
            "packages": [{
                "name": "meta/pkg",
                "version": "1.0",
                "type": "metapackage"
            }]
        }))
        .unwrap();
        let lock = Lock::parse(&bytes).unwrap();
        let ops = compute_ops(&lock, &installed_with(vec![]), false);
        assert!(ops.is_empty());
    }
}
