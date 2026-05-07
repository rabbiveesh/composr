//! Auth resolution for dist downloads.
//!
//! We support the subset of Composer auth that service-app actually exercises:
//! `github-oauth` (api.github.com → bearer token) and `http-basic` (any host
//! with username/password). Sources are checked in this order:
//!
//! 1. `COMPOSER_AUTH` env (a JSON blob — same shape as auth.json's "config")
//! 2. `<project>/auth.json`
//! 3. `<COMPOSER_HOME>/auth.json` — typically `~/.composer/auth.json`
//!
//! Sources earlier in the list win on conflict (matches Composer).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    #[serde(default, rename = "github-oauth")]
    github_oauth: HashMap<String, String>,
    #[serde(default, rename = "gitlab-token")]
    gitlab_token: HashMap<String, String>,
    #[serde(default, rename = "http-basic")]
    http_basic: HashMap<String, HttpBasic>,
    #[serde(default)]
    bearer: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
struct HttpBasic {
    username: String,
    password: String,
}

#[derive(Debug, Default, Clone)]
pub struct AuthConfig {
    github_oauth: HashMap<String, String>,
    gitlab_token: HashMap<String, String>,
    http_basic: HashMap<String, HttpBasic>,
    bearer: HashMap<String, String>,
}

impl AuthConfig {
    /// Load auth from env + project + home, layering with first-source-wins.
    pub fn load(project_root: &Path) -> Result<Self> {
        let mut cfg = AuthConfig::default();

        if let Ok(env) = std::env::var("COMPOSER_AUTH") {
            if !env.trim().is_empty() {
                let parsed: AuthFile = serde_json::from_str(&env)
                    .context("parsing COMPOSER_AUTH env var as JSON")?;
                cfg.merge_lower_priority(parsed);
            }
        }

        let project_auth = project_root.join("auth.json");
        if project_auth.is_file() {
            let parsed = read_auth_file(&project_auth)?;
            cfg.merge_lower_priority(parsed);
        }

        if let Some(home) = composer_home() {
            let home_auth = home.join("auth.json");
            if home_auth.is_file() {
                let parsed = read_auth_file(&home_auth)?;
                cfg.merge_lower_priority(parsed);
            }
        }

        // Convenience: GH_TOKEN / GITHUB_TOKEN as a last-resort github-oauth
        // source. These aren't Composer conventions, but they're how most
        // dev environments already store the token.
        if !cfg.github_oauth.contains_key("github.com") {
            for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
                if let Ok(tok) = std::env::var(var) {
                    if !tok.is_empty() {
                        cfg.github_oauth.insert("github.com".into(), tok);
                        break;
                    }
                }
            }
        }

        Ok(cfg)
    }

    fn merge_lower_priority(&mut self, other: AuthFile) {
        for (k, v) in other.github_oauth {
            self.github_oauth.entry(k).or_insert(v);
        }
        for (k, v) in other.gitlab_token {
            self.gitlab_token.entry(k).or_insert(v);
        }
        for (k, v) in other.http_basic {
            self.http_basic.entry(k).or_insert(v);
        }
        for (k, v) in other.bearer {
            self.bearer.entry(k).or_insert(v);
        }
    }

    /// Decide which header to send for a given URL. Returns the header value
    /// for `Authorization`, or `None` if the URL has no matching credential.
    pub fn header_for(&self, url: &str) -> Option<String> {
        let host = host_of(url)?;
        // GitHub oauth — applies to api.github.com and github.com.
        if host == "api.github.com" || host == "github.com" {
            let tok = self
                .github_oauth
                .get("github.com")
                .or_else(|| self.github_oauth.get(&host));
            if let Some(tok) = tok {
                return Some(format!("Bearer {tok}"));
            }
        }
        if let Some(tok) = self.gitlab_token.get(&host) {
            return Some(format!("Bearer {tok}"));
        }
        if let Some(tok) = self.bearer.get(&host) {
            return Some(format!("Bearer {tok}"));
        }
        if let Some(creds) = self.http_basic.get(&host) {
            use base64ish::encode_basic;
            return Some(format!(
                "Basic {}",
                encode_basic(&creds.username, &creds.password)
            ));
        }
        None
    }
}

fn read_auth_file(path: &Path) -> Result<AuthFile> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    // auth.json is sometimes nested under a `"config"` key — accept both.
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    let inner = v.get("config").cloned().unwrap_or(v);
    let parsed: AuthFile = serde_json::from_value(inner)
        .with_context(|| format!("auth.json schema mismatch in {}", path.display()))?;
    Ok(parsed)
}

fn composer_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("COMPOSER_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    // macOS: Composer historically uses $HOME/.composer (it doesn't follow
    // XDG by default). Match that.
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".composer"))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(|s| s.to_string())
}

// Tiny inline base64 encoder so we don't pull in a dep just for one header.
mod base64ish {
    pub fn encode_basic(user: &str, pass: &str) -> String {
        let raw = format!("{user}:{pass}");
        encode(raw.as_bytes())
    }
    fn encode(input: &[u8]) -> String {
        const A: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
        let mut i = 0;
        while i + 3 <= input.len() {
            let b = &input[i..i + 3];
            out.push(A[(b[0] >> 2) as usize] as char);
            out.push(A[(((b[0] & 0b11) << 4) | (b[1] >> 4)) as usize] as char);
            out.push(A[(((b[1] & 0b1111) << 2) | (b[2] >> 6)) as usize] as char);
            out.push(A[(b[2] & 0b111111) as usize] as char);
            i += 3;
        }
        let rem = input.len() - i;
        if rem == 1 {
            let b0 = input[i];
            out.push(A[(b0 >> 2) as usize] as char);
            out.push(A[((b0 & 0b11) << 4) as usize] as char);
            out.push_str("==");
        } else if rem == 2 {
            let b0 = input[i];
            let b1 = input[i + 1];
            out.push(A[(b0 >> 2) as usize] as char);
            out.push(A[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
            out.push(A[((b1 & 0b1111) << 2) as usize] as char);
            out.push('=');
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn matches_rfc4648() {
            assert_eq!(encode_basic("user", "pass"), "dXNlcjpwYXNz");
            assert_eq!(encode(b"f"), "Zg==");
            assert_eq!(encode(b"fo"), "Zm8=");
            assert_eq!(encode(b"foo"), "Zm9v");
            assert_eq!(encode(b"foob"), "Zm9vYg==");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_oauth_header_for_api_github_com() {
        let mut cfg = AuthConfig::default();
        cfg.github_oauth
            .insert("github.com".into(), "tokenABC".into());
        assert_eq!(
            cfg.header_for("https://api.github.com/repos/x/y/zipball/abc"),
            Some("Bearer tokenABC".into())
        );
        assert_eq!(
            cfg.header_for("https://github.com/x/y"),
            Some("Bearer tokenABC".into())
        );
        assert_eq!(cfg.header_for("https://other.example/x"), None);
    }

    #[test]
    fn http_basic_header() {
        let mut cfg = AuthConfig::default();
        cfg.http_basic.insert(
            "repo.example".into(),
            HttpBasic {
                username: "user".into(),
                password: "pass".into(),
            },
        );
        assert_eq!(
            cfg.header_for("https://repo.example/x.zip"),
            Some("Basic dXNlcjpwYXNz".into())
        );
    }
}
