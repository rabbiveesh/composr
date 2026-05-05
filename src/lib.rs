pub mod cache;
pub mod config;
pub mod emit;
pub mod filter;
pub mod parse;
pub mod walk;

use std::path::PathBuf;

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
