pub mod auth;
pub mod binsetup;
pub mod bootstrap;
pub mod cache;
pub mod classmap_cache;
pub mod composer_shim;
pub mod config;
pub mod diff;
pub mod discover;
pub mod dist_cache;
pub mod download;
pub mod dump;
pub mod emit;
pub mod extract;
pub mod filter;
pub mod install;
pub mod installed;
pub mod lock;
pub mod parse;
pub mod scripts;
pub mod walk;

use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub walked: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub parse_errors: usize,
    pub elapsed_ms: u128,
}

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
