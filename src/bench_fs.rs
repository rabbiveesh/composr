//! Synthetic filesystem benchmark for picking a fast staging surface.
//!
//! Recreates the shape of an `extract`-stage workload — many parallel
//! mkdirs, many parallel small file writes, many renames, then a recursive
//! rm — against an arbitrary target directory. Run it across candidate
//! paths to find one that isn't paying metadata tax (FSEvents, mds,
//! Docker bind sync, etc.) for this kind of churn.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct BenchOptions {
    pub paths: Vec<PathBuf>,
    pub packages: usize,
    pub dirs_per_pkg: usize,
    pub files_per_dir: usize,
    pub file_bytes: usize,
}

impl BenchOptions {
    /// Defaults sized to roughly match a real composr extract workload
    /// (126 archives × ~50 dirs × ~18 files ≈ 113k files, 6.3k dirs,
    /// ~330MB of payload).
    pub fn defaults_for(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            packages: 126,
            dirs_per_pkg: 50,
            files_per_dir: 18,
            file_bytes: 3 * 1024,
        }
    }
}

#[derive(Debug)]
struct PhaseTimes {
    mkdir_ms: u128,
    write_ms: u128,
    rename_ms: u128,
    cleanup_ms: u128,
}

impl PhaseTimes {
    fn total_ms(&self) -> u128 {
        self.mkdir_ms + self.write_ms + self.rename_ms + self.cleanup_ms
    }
}

pub fn run(opts: BenchOptions) -> Result<()> {
    let total_dirs = opts.packages * opts.dirs_per_pkg;
    let total_files = total_dirs * opts.files_per_dir;
    let total_mb = (total_files * opts.file_bytes) / (1024 * 1024);

    eprintln!(
        "Workload: {} pkgs × {} dirs × {} files = {} files / {} dirs / ~{} MB",
        opts.packages,
        opts.dirs_per_pkg,
        opts.files_per_dir,
        total_files,
        total_dirs,
        total_mb,
    );
    eprintln!();
    eprintln!(
        "{:>10} {:>10} {:>10} {:>10} {:>10}  {}",
        "mkdir", "write", "rename", "rm -rf", "TOTAL", "path",
    );

    for path in &opts.paths {
        match bench_one(path, &opts) {
            Ok(r) => eprintln!(
                "{:>10} {:>10} {:>10} {:>10} {:>10}  {}",
                ms(r.mkdir_ms),
                ms(r.write_ms),
                ms(r.rename_ms),
                ms(r.cleanup_ms),
                ms(r.total_ms()),
                path.display(),
            ),
            Err(e) => eprintln!("{:>54}  {}: {e:#}", "FAILED", path.display()),
        }
    }
    Ok(())
}

fn ms(v: u128) -> String {
    format!("{}ms", v)
}

fn bench_one(target: &Path, opts: &BenchOptions) -> Result<PhaseTimes> {
    fs::create_dir_all(target)
        .with_context(|| format!("creating {}", target.display()))?;
    let scratch = target.join(format!(".composr-bench-{}", std::process::id()));
    if scratch.exists() {
        fs::remove_dir_all(&scratch)?;
    }
    fs::create_dir_all(&scratch)?;

    let payload: Vec<u8> = vec![b'x'; opts.file_bytes];

    let t = Instant::now();
    (0..opts.packages).into_par_iter().try_for_each(|p| -> Result<()> {
        for d in 0..opts.dirs_per_pkg {
            let dir = scratch.join(format!("pkg{p:03}/dir{d:03}"));
            fs::create_dir_all(&dir)
                .with_context(|| format!("mkdir {}", dir.display()))?;
        }
        Ok(())
    })?;
    let mkdir_ms = t.elapsed().as_millis();

    let t = Instant::now();
    (0..opts.packages).into_par_iter().try_for_each(|p| -> Result<()> {
        for d in 0..opts.dirs_per_pkg {
            for f in 0..opts.files_per_dir {
                let path = scratch.join(format!("pkg{p:03}/dir{d:03}/file{f:03}.php"));
                let mut out = fs::File::create(&path)
                    .with_context(|| format!("create {}", path.display()))?;
                out.write_all(&payload)
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        Ok(())
    })?;
    let write_ms = t.elapsed().as_millis();

    let t = Instant::now();
    (0..opts.packages).into_par_iter().try_for_each(|p| -> Result<()> {
        let from = scratch.join(format!("pkg{p:03}"));
        let to = scratch.join(format!("done{p:03}"));
        fs::rename(&from, &to)
            .with_context(|| format!("rename {} -> {}", from.display(), to.display()))?;
        Ok(())
    })?;
    let rename_ms = t.elapsed().as_millis();

    let t = Instant::now();
    fs::remove_dir_all(&scratch)
        .with_context(|| format!("rm -rf {}", scratch.display()))?;
    let cleanup_ms = t.elapsed().as_millis();

    Ok(PhaseTimes {
        mkdir_ms,
        write_ms,
        rename_ms,
        cleanup_ms,
    })
}
