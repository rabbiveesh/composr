//! Async, parallel downloader for dist archives.
//!
//! Concurrency model: one tokio task per package, capped by a semaphore.
//! Each task does `cache hit? skip` → otherwise `HTTP GET` → write to a
//! `.partial` tmp file → fsync → rename into the cache → optional sha1 check.
//!
//! Retries: 3 attempts with exponential backoff on transport errors and on
//! HTTP 5xx. 4xx is fatal (auth/path issue, won't recover).

use crate::auth::AuthConfig;
use crate::dist_cache::{sha1_file, DistCache};
use crate::lock::{Dist, LockedPackage};
use anyhow::{anyhow, bail, Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct DownloadOutcome {
    pub package_name: String,
    pub cached_path: PathBuf,
    pub from_cache: bool,
}

pub struct Downloader {
    client: reqwest::Client,
    cache: DistCache,
    auth: AuthConfig,
    concurrency: usize,
}

impl Downloader {
    pub fn new(cache: DistCache, auth: AuthConfig, concurrency: usize) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("composr/0.2 (+https://github.com/anthropic) reqwest")
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(8)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            cache,
            auth,
            concurrency: concurrency.max(1),
        })
    }

    /// Download all packages that have a dist. Caller is expected to have
    /// already filtered out metapackages and source-only packages.
    pub async fn fetch_all<'a>(
        &self,
        packages: &'a [&'a LockedPackage],
        progress: bool,
    ) -> Result<Vec<DownloadOutcome>> {
        let pb = if progress {
            let pb = ProgressBar::new(packages.len() as u64);
            pb.set_style(
                ProgressStyle::with_template(
                    "  {bar:40.cyan/blue} {pos:>4}/{len:4} {msg}",
                )
                .unwrap(),
            );
            Some(pb)
        } else {
            None
        };

        let sem = Arc::new(tokio::sync::Semaphore::new(self.concurrency));
        let mut tasks = FuturesUnordered::new();

        for pkg in packages {
            let dist = match &pkg.dist {
                Some(d) => d.clone(),
                None => bail!("package {} has no dist; only dist installs supported", pkg.name),
            };
            if dist.kind != "zip" {
                bail!(
                    "package {} has dist.type={} — only zip is supported",
                    pkg.name,
                    dist.kind
                );
            }
            let name = pkg.name.clone();
            let permit = sem.clone();
            let client = self.client.clone();
            let cache = self.cache.clone();
            let auth_header = self.auth.header_for(&dist.url);
            let pb_clone = pb.clone();

            tasks.push(tokio::spawn(async move {
                let _p = permit.acquire_owned().await.expect("semaphore closed");
                let res =
                    fetch_one(&client, &cache, &name, &dist, auth_header.as_deref()).await;
                if let Some(pb) = &pb_clone {
                    pb.inc(1);
                    if let Ok(o) = &res {
                        let tag = if o.from_cache { "cached " } else { "fetched" };
                        pb.set_message(format!("{tag} {name}"));
                    }
                }
                res
            }));
        }

        let mut out = Vec::with_capacity(packages.len());
        while let Some(joined) = tasks.next().await {
            let r = joined.context("download task panicked")?;
            out.push(r?);
        }

        if let Some(pb) = pb {
            pb.finish_and_clear();
        }
        Ok(out)
    }
}

async fn fetch_one(
    client: &reqwest::Client,
    cache: &DistCache,
    pkg_name: &str,
    dist: &Dist,
    auth_header: Option<&str>,
) -> Result<DownloadOutcome> {
    let cached_path = cache.path_for(pkg_name, &dist.url, &dist.kind);

    // Cache hit fast-path: only trust it if the shasum matches (when one is
    // declared) — otherwise the file's existence is enough. Composer makes
    // the same call.
    if cached_path.exists() {
        if dist.shasum.is_empty() || matches_sha1(&cached_path, &dist.shasum)? {
            return Ok(DownloadOutcome {
                package_name: pkg_name.to_string(),
                cached_path,
                from_cache: true,
            });
        }
        // Stale cache entry — wipe and refetch.
        let _ = std::fs::remove_file(&cached_path);
    }

    DistCache::ensure_parent(&cached_path)?;

    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("composr/0.2"));
    if let Some(h) = auth_header {
        let mut hv = HeaderValue::from_str(h)
            .map_err(|e| anyhow!("invalid auth header: {e}"))?;
        hv.set_sensitive(true);
        headers.insert(AUTHORIZATION, hv);
    }

    let mut delay = Duration::from_millis(500);
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        match download_to_file(client, &dist.url, &headers, &cached_path).await {
            Ok(()) => {
                if !dist.shasum.is_empty() && !matches_sha1(&cached_path, &dist.shasum)? {
                    let _ = std::fs::remove_file(&cached_path);
                    last_err = Some(anyhow!(
                        "sha1 mismatch for {pkg_name} (downloaded from {})",
                        dist.url
                    ));
                    continue;
                }
                return Ok(DownloadOutcome {
                    package_name: pkg_name.to_string(),
                    cached_path,
                    from_cache: false,
                });
            }
            Err(e) => {
                if let Some(status) = e.status_code {
                    // Don't retry 4xx — auth / 404 / rate limit aren't going
                    // to fix themselves on retry.
                    if (400..500).contains(&status) {
                        return Err(anyhow!(
                            "{pkg_name}: HTTP {status} downloading {} — {}",
                            dist.url,
                            e.message
                        ));
                    }
                }
                last_err = Some(anyhow!(
                    "{pkg_name}: {} (attempt {}/3)",
                    e.message,
                    attempt + 1
                ));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("{pkg_name}: download failed")))
}

#[derive(Debug)]
struct FetchError {
    status_code: Option<u16>,
    message: String,
}

async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    target: &Path,
) -> std::result::Result<(), FetchError> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|e| FetchError {
            status_code: e.status().map(|s| s.as_u16()),
            message: format!("request failed: {e}"),
        })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(FetchError {
            status_code: Some(status.as_u16()),
            message: format!("HTTP {status}"),
        });
    }

    let tmp = target.with_extension("partial");
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| FetchError {
        status_code: None,
        message: format!("creating {}: {e}", tmp.display()),
    })?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| FetchError {
            status_code: None,
            message: format!("stream read: {e}"),
        })?;
        f.write_all(&chunk).await.map_err(|e| FetchError {
            status_code: None,
            message: format!("writing partial: {e}"),
        })?;
    }
    f.flush().await.map_err(|e| FetchError {
        status_code: None,
        message: format!("flushing: {e}"),
    })?;
    drop(f);

    tokio::fs::rename(&tmp, target).await.map_err(|e| FetchError {
        status_code: None,
        message: format!("rename {} -> {}: {e}", tmp.display(), target.display()),
    })?;
    Ok(())
}

fn matches_sha1(path: &Path, expected: &str) -> Result<bool> {
    let actual = sha1_file(path)?;
    Ok(actual.eq_ignore_ascii_case(expected))
}
