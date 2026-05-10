//! Integration test: render `vendor/composer/GeneratedServiceProviderData.php`
//! against monolith-app's actual composer.lock + installed sources, and diff
//! against the file real composer's `tbachert/spi` plugin produces from the
//! same inputs.
//!
//! The reference file is captured under tests/data/monolith-app-spi-reference.php
//! (generated once via `composer dump-autoload` in an isolated monolith-app copy
//! with `allow-plugins.tbachert/spi: true`). The test re-runs every CI build
//! so a regression in our render shows up immediately.

use composr::dump::{build_classmap, DumpOptions};
use composr::lock::Lock;
use composr::spi;
use std::path::PathBuf;

fn nuggets_root() -> PathBuf {
    std::env::var("COMPOSR_MONOLITH_FIXTURE").map(PathBuf::from).unwrap_or_default()
}

#[test]
fn matches_real_composer_output_byte_for_byte() {
    let root = nuggets_root();
    if !root.join("composer.lock").exists() {
        eprintln!("skipping: monolith-app fixture not available at {root:?}");
        return;
    }

    let lock = Lock::read(&root.join("composer.lock")).unwrap();
    let composer_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("composer.json")).unwrap()).unwrap();

    // Build the same classmap composr would build during install. We need
    // it for both class-existence checks and provider-source resolution.
    let dump_opts = DumpOptions {
        root: root.clone(),
        cache_path: None,
        with_dev: true,
        no_cache: false,
        dry_run: true,
        output_dir: None,
    };
    let (_cfg, classmap, _stats) = build_classmap(&dump_opts).unwrap();

    let mappings = spi::collect(&lock, true, &composer_json);
    let body = spi::render(&mappings, &lock, true, &classmap);

    let reference_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/monolith-app-spi-reference.php");
    let reference = std::fs::read_to_string(&reference_path)
        .unwrap_or_else(|e| panic!("missing reference file {reference_path:?}: {e}"));

    if body != reference {
        let actual_path = std::env::temp_dir().join("composr-spi-actual.php");
        std::fs::write(&actual_path, &body).unwrap();
        panic!(
            "rendered SPI file does not match composer's output\n  reference: {reference_path:?}\n  actual:    {actual_path:?}\n\nFirst diverging chunk:\n{}",
            diff_preview(&reference, &body)
        );
    }
}

fn diff_preview(a: &str, b: &str) -> String {
    let mut out = String::new();
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            out.push_str(&format!("line {i}:\n  expected: {la:?}\n  got:      {lb:?}\n"));
            if out.lines().count() > 10 {
                break;
            }
        }
    }
    if a.lines().count() != b.lines().count() {
        out.push_str(&format!(
            "line counts differ: expected {} lines, got {}\n",
            a.lines().count(),
            b.lines().count()
        ));
    }
    out
}
