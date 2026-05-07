//! Run Composer-style scripts hooked to install lifecycle events.
//!
//! Composer's full script runner can dispatch class-method handlers via the
//! Composer runtime, which we don't have. For service-app's actual scripts (and
//! the common Laravel/Symfony shapes) the events break down as:
//!
//! - `pre-install-cmd`, `post-install-cmd` — lifecycle hooks
//! - `post-autoload-dump` — fires after our dump-autoload step
//!
//! Each event is a list of strings. We classify each entry:
//! - leading `@php ` → run via PHP
//! - leading `@putenv X=Y` → set env in our process for subsequent scripts
//! - leading `@composer ` → warn and skip (we don't reinvoke composer)
//! - other `@`-prefix → run via shell with `@` stripped (legacy convention)
//! - contains `::` and no spaces → class-method handler; warn and skip
//! - everything else → shell command
//!
//! Scripts inherit our env, run with CWD = project root, and any non-zero
//! exit code aborts the install (matching Composer).

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub enum Lifecycle {
    PreInstallCmd,
    PostInstallCmd,
    PostAutoloadDump,
}

impl Lifecycle {
    pub fn key(&self) -> &'static str {
        match self {
            Lifecycle::PreInstallCmd => "pre-install-cmd",
            Lifecycle::PostInstallCmd => "post-install-cmd",
            Lifecycle::PostAutoloadDump => "post-autoload-dump",
        }
    }
}

#[derive(Default, Debug)]
pub struct ScriptOutcome {
    pub ran: usize,
    pub skipped: usize,
}

pub struct Runner<'a> {
    pub project_root: &'a Path,
    pub vendor_bin_dir: &'a Path,
    /// Composer-style scripts section from the root composer.json — we accept
    /// the raw JSON value because entries can be strings or lists of strings.
    pub scripts: &'a serde_json::Value,
}

impl<'a> Runner<'a> {
    pub fn dispatch(&self, event: Lifecycle) -> Result<ScriptOutcome> {
        let key = event.key();
        let entries = match self.scripts.get(key) {
            Some(v) => v,
            None => {
                tracing::debug!("no scripts for {key}, skipping");
                return Ok(ScriptOutcome::default());
            }
        };
        let list: Vec<String> = match entries {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => {
                tracing::warn!("scripts.{key} has unexpected shape, skipping");
                return Ok(ScriptOutcome::default());
            }
        };

        let mut env_overrides: HashMap<String, String> = HashMap::new();
        let mut outcome = ScriptOutcome::default();
        for raw in list {
            self.run_one(&raw, &mut env_overrides, &mut outcome)
                .with_context(|| format!("script `{raw}` (event {key})"))?;
        }
        Ok(outcome)
    }

    fn run_one(
        &self,
        raw: &str,
        env: &mut HashMap<String, String>,
        outcome: &mut ScriptOutcome,
    ) -> Result<()> {
        let cmd = raw.trim();

        // @putenv FOO=bar : update env for subsequent commands in this event.
        if let Some(rest) = cmd.strip_prefix("@putenv") {
            let rest = rest.trim();
            if let Some((k, v)) = rest.split_once('=') {
                env.insert(k.trim().to_string(), v.trim().to_string());
            } else {
                env.remove(rest);
            }
            outcome.skipped += 1;
            return Ok(());
        }

        // @composer X : we'd need a real composer to run sub-scripts. Skip.
        if cmd.starts_with("@composer ") || cmd == "@composer" {
            tracing::warn!("skipping `{cmd}` — composr does not recursively invoke composer");
            outcome.skipped += 1;
            return Ok(());
        }

        // @php X : run with the PHP interpreter.
        if let Some(rest) = cmd.strip_prefix("@php ") {
            return self.exec("php", parse_argv(rest)?, env, outcome);
        }

        // Other @prefix: composer treats this as an alias for one of the
        // event names ("@<event-name>"). For service-app there are none of these
        // in install-time scripts, so warn-and-skip is fine.
        if cmd.starts_with('@') {
            tracing::warn!("skipping `{cmd}` — unsupported @-script");
            outcome.skipped += 1;
            return Ok(());
        }

        // class-method handler: `Foo\Bar::baz` (no spaces, contains ::).
        // We can't run these without the Composer runtime; warn and skip.
        if !cmd.contains(char::is_whitespace) && cmd.contains("::") {
            tracing::warn!(
                "skipping class-method script `{cmd}` — not supported (run `composer run-script` for a fully-evaluated event)"
            );
            outcome.skipped += 1;
            return Ok(());
        }

        // Bare shell command. Use sh -c so users can write what they'd write
        // in composer.json (including pipes and shell expansion).
        self.exec("sh", vec!["-c".into(), cmd.into()], env, outcome)
    }

    fn exec(
        &self,
        program: &str,
        args: Vec<String>,
        env: &HashMap<String, String>,
        outcome: &mut ScriptOutcome,
    ) -> Result<()> {
        let mut c = Command::new(program);
        c.args(&args).current_dir(self.project_root);

        // Composer prepends vendor/bin to PATH for scripts so users can call
        // `phpunit`, `pest`, etc. without the prefix.
        if let Ok(existing) = std::env::var("PATH") {
            let new_path = format!("{}:{}", self.vendor_bin_dir.display(), existing);
            c.env("PATH", new_path);
        } else {
            c.env("PATH", self.vendor_bin_dir);
        }
        for (k, v) in env {
            c.env(k, v);
        }

        let status = c
            .status()
            .with_context(|| format!("spawning `{program} {args:?}`"))?;
        if !status.success() {
            bail!("`{program} {args:?}` exited with {status}");
        }
        outcome.ran += 1;
        Ok(())
    }
}

/// Tiny argv splitter — splits on whitespace honoring single/double quotes.
/// Matches what Composer does for scripts: it doesn't run them through a
/// shell, so we do the same.
fn parse_argv(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', _) => {
                if let Some(&n) = chars.peek() {
                    chars.next();
                    buf.push(n);
                }
            }
            ('"', None) => quote = Some('"'),
            ('\'', None) => quote = Some('\''),
            (c, Some(q)) if c == q => quote = None,
            (c, None) if c.is_whitespace() => {
                if !buf.is_empty() {
                    out.push(std::mem::take(&mut buf));
                }
            }
            (c, _) => buf.push(c),
        }
    }
    if quote.is_some() {
        return Err(anyhow!("unterminated quote in script: {s}"));
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_argv_simple() {
        assert_eq!(parse_argv("a b c").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_argv_quoted() {
        assert_eq!(
            parse_argv("artisan migrate --message 'hello world'").unwrap(),
            vec!["artisan", "migrate", "--message", "hello world"]
        );
    }
}
