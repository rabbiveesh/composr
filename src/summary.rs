//! Styled one-line summaries for `composr install` and the autoload step.
//!
//! Ports the vocabulary from `scripts/_autoload_dumpr_pretty` into the
//! binary — so users get the same colored "blazing / refreshed / rebuilt
//! / bumpy" output without having to install a separate shell helper on
//! `$PATH`. Honors `NO_COLOR` and only colors when stderr is a TTY.
//!
//! Layout: `[label]  <stage>  <word>  ·  <speed>  ·  <extra>  [·  <classes>]`
//! Label is shown in dim brackets (git-hook context); omitted otherwise.

use crate::dump::DumpStats;
use crate::install::InstallReport;
use std::io::IsTerminal;

#[derive(Copy, Clone)]
struct Palette {
    reset: &'static str,
    dim: &'static str,
    cyan_b: &'static str,
    green: &'static str,
    green_b: &'static str,
    bgreen: &'static str,
    yellow: &'static str,
    yellow_b: &'static str,
    red: &'static str,
    red_b: &'static str,
}

const NO_COLOR: Palette = Palette {
    reset: "",
    dim: "",
    cyan_b: "",
    green: "",
    green_b: "",
    bgreen: "",
    yellow: "",
    yellow_b: "",
    red: "",
    red_b: "",
};

const ANSI: Palette = Palette {
    reset: "\x1b[0m",
    dim: "\x1b[2m",
    cyan_b: "\x1b[1;36m",
    green: "\x1b[32m",
    green_b: "\x1b[1;32m",
    bgreen: "\x1b[92m",
    yellow: "\x1b[33m",
    yellow_b: "\x1b[1;33m",
    red: "\x1b[31m",
    red_b: "\x1b[1;31m",
};

fn palette() -> Palette {
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        ANSI
    } else {
        NO_COLOR
    }
}

fn fmt_speed(elapsed_ms: u128) -> String {
    if elapsed_ms >= 1000 {
        format!("{:.1}s", elapsed_ms as f64 / 1000.0)
    } else {
        format!("{elapsed_ms}ms")
    }
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Print the install summary as a single styled line. Replaces the old
/// `install: N installed, M removed (...)` line. Word picks:
///   * `bumpy`     — composer delegations fired (yellow tag for awareness)
///   * `blazing`   — nothing actually changed (idempotent run, ~ms)
///   * `refreshed` — ops occurred but everything came from the dist cache
///   * `rebuilt`   — at least one package was fetched over the wire
pub fn print_install(report: &InstallReport, label: Option<&str>) {
    let p = palette();
    let speed = fmt_speed(report.elapsed_ms);
    let total_ops = report.installed + report.removed;

    let (word, wc, sc, extra, ec): (&str, &str, &str, String, &str);
    if total_ops == 0 && report.fetched == 0 {
        word = "blazing";
        wc = p.green_b;
        sc = if report.elapsed_ms < 800 { p.bgreen } else { p.green };
        extra = "no-op (vendor in sync)".to_string();
        ec = p.green;
    } else if report.fetched == 0 {
        word = "refreshed";
        wc = p.green_b;
        sc = if report.elapsed_ms < 3000 { p.green } else { p.yellow };
        extra = format!(
            "{} installed, {} removed · {} from cache",
            report.installed, report.removed, report.from_cache
        );
        ec = p.yellow;
    } else {
        word = "rebuilt";
        wc = p.yellow_b;
        sc = if report.elapsed_ms < 30_000 { p.yellow } else { p.red };
        extra = format!(
            "{} installed, {} removed · {} cached, {} fetched",
            report.installed, report.removed, report.from_cache, report.fetched
        );
        ec = p.yellow;
    }

    let label_prefix = match label {
        Some(l) => format!("{dim}[{l}]{reset}  ", dim = p.dim, reset = p.reset),
        None => String::new(),
    };
    let composer_tag = if report.composer_calls > 0 {
        format!(
            "  {dim}·{reset}  {yc}{n} composer call{s}{reset}",
            dim = p.dim,
            reset = p.reset,
            yc = p.yellow,
            n = report.composer_calls,
            s = if report.composer_calls == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };

    eprintln!(
        "{label}{cb}install{r}  {wc}{word}{r}  {dim}·{r}  {sc}{speed}{r}  {dim}·{r}  {ec}{extra}{r}{tag}",
        label = label_prefix,
        cb = p.cyan_b,
        r = p.reset,
        wc = wc,
        word = word,
        dim = p.dim,
        sc = sc,
        speed = speed,
        ec = ec,
        extra = extra,
        tag = composer_tag
    );
}

/// Print the autoload-stage summary as a single styled line. Replaces the
/// old `Autoload (mode): ...` line. Word picks (mirroring the pretty-printer):
///   * `bumpy`     — parse errors during walk
///   * `blazing`   — 100% cache hot (per-file misses == 0 and per-package
///                   misses == 0)
///   * `refreshed` — some cached, some reparsed
///   * `rebuilt`   — mostly cold parsing
pub fn print_autoload(stats: &DumpStats, mode: &str, label: Option<&str>) {
    let p = palette();
    let speed = fmt_speed(stats.elapsed_ms);
    let pkg_total = stats.package_cache_hits + stats.package_cache_misses;
    let any_misses = stats.misses > 0 || stats.package_cache_misses > 0;
    let any_hits = stats.hits > 0 || stats.package_cache_hits > 0;

    let (word, wc, sc, extra, ec): (&str, &str, &str, String, &str);
    if stats.errors > 0 {
        word = "bumpy";
        wc = p.red_b;
        sc = p.red;
        extra = format!("{} parse error{}", stats.errors, if stats.errors == 1 { "" } else { "s" });
        ec = p.red;
    } else if !any_misses {
        word = "blazing";
        wc = p.green_b;
        sc = if stats.elapsed_ms < 800 { p.bgreen } else { p.green };
        extra = if pkg_total > 0 {
            format!("100% cache hot ({}/{} pkg)", stats.package_cache_hits, pkg_total)
        } else {
            "100% cache hot".to_string()
        };
        ec = p.green;
    } else if any_hits {
        word = "refreshed";
        wc = p.green_b;
        sc = if stats.elapsed_ms < 3000 { p.green } else { p.yellow };
        extra = format!(
            "{} reparsed · {} cached",
            commas(stats.misses as u64),
            commas(stats.hits as u64)
        );
        ec = p.yellow;
    } else {
        word = "rebuilt";
        wc = p.yellow_b;
        sc = if stats.elapsed_ms < 5000 { p.yellow } else { p.red };
        extra = format!("{} files reparsed", commas(stats.misses as u64));
        ec = p.yellow;
    }

    let label_prefix = match label {
        Some(l) => format!("{dim}[{l}]{reset}  ", dim = p.dim, reset = p.reset),
        None => String::new(),
    };

    eprintln!(
        "{label}{cb}autoload{r} {dim}({mode}){r}  {wc}{word}{r}  {dim}·{r}  {sc}{speed}{r}  {dim}·{r}  {ec}{extra}{r}  {dim}·{r}  {dim}{classes} classes{r}",
        label = label_prefix,
        cb = p.cyan_b,
        r = p.reset,
        dim = p.dim,
        mode = mode,
        wc = wc,
        word = word,
        sc = sc,
        speed = speed,
        ec = ec,
        extra = extra,
        classes = commas(stats.classes as u64)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas_inserts_thousands_separators() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(51_556), "51,556");
        assert_eq!(commas(120_646), "120,646");
        assert_eq!(commas(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn fmt_speed_picks_unit() {
        assert_eq!(fmt_speed(0), "0ms");
        assert_eq!(fmt_speed(500), "500ms");
        assert_eq!(fmt_speed(999), "999ms");
        assert_eq!(fmt_speed(1000), "1.0s");
        assert_eq!(fmt_speed(1234), "1.2s");
        assert_eq!(fmt_speed(29_615), "29.6s");
    }
}
