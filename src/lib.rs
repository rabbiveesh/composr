//! Composer install + dump-autoload, as a library and the `composr` binary.
//!
//! The `cli` feature (on by default) carries the installer, autoload dumper and
//! their heavy dependencies. With `default-features = false` only the
//! composer.json / composer.lock / installed.json readers remain: [`config`],
//! [`lock`] and [`installed`].

#[cfg(feature = "cli")]
pub mod auth;
#[cfg(feature = "cli")]
pub mod bench_fs;
#[cfg(feature = "cli")]
pub mod binsetup;
#[cfg(feature = "cli")]
pub mod bootstrap;
#[cfg(feature = "cli")]
pub mod cache;
#[cfg(feature = "cli")]
pub mod classmap_cache;
#[cfg(feature = "cli")]
pub mod composer_shim;
#[cfg(feature = "cli")]
pub mod composr_config;
pub mod config;
#[cfg(feature = "cli")]
pub mod diff;
#[cfg(feature = "cli")]
pub mod discover;
#[cfg(feature = "cli")]
pub mod dist_cache;
#[cfg(feature = "cli")]
pub mod download;
#[cfg(feature = "cli")]
pub mod dump;
#[cfg(feature = "cli")]
pub mod emit;
#[cfg(feature = "cli")]
pub mod extract;
#[cfg(feature = "cli")]
pub mod filter;
#[cfg(feature = "cli")]
pub mod install;
pub mod installed;
pub mod lock;
#[cfg(feature = "cli")]
pub mod parse;
#[cfg(feature = "cli")]
pub mod pest_plugins;
#[cfg(feature = "cli")]
pub mod php_json;
#[cfg(feature = "cli")]
pub mod scripts;
#[cfg(feature = "cli")]
pub mod spi;
#[cfg(feature = "cli")]
pub mod summary;
#[cfg(feature = "cli")]
pub mod walk;

#[cfg(feature = "cli")]
use std::path::{Path, PathBuf};

#[cfg(feature = "cli")]
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub walked: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub parse_errors: usize,
    pub elapsed_ms: u128,
}

#[cfg(feature = "cli")]
#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub cache_path: PathBuf,
    pub no_dev: bool,
    pub strict: bool,
    pub no_cache: bool,
    pub verbose: bool,
    pub dry_run: bool,
    pub threads: Option<usize>,
}

/// Convenience helper used by the install pipeline: run dump-autoload with
/// the same defaults as the CLI's no-flag invocation, but with the dev flag
/// chosen by the caller. Returns the dump stats so callers can surface them.
#[cfg(feature = "cli")]
pub fn dump_autoload(root: &Path, with_dev: bool) -> anyhow::Result<dump::DumpStats> {
    dump::dump(dump::DumpOptions {
        root: root.to_path_buf(),
        cache_path: None,
        with_dev,
        no_cache: false,
        dry_run: false,
        output_dir: None,
    })
}
