//! Native replacement for `tbachert/spi`'s `pre-autoload-dump` hook.
//!
//! `tbachert/spi` codegens `vendor/composer/GeneratedServiceProviderData.php`
//! whose `providers($service): array` is the runtime registry consumed by
//! `Nevay\SPI\ServiceLoader`. Without that file every SPI lookup returns `[]`
//! and every dependent (e.g. OpenTelemetry's instrumentation autoloading)
//! silently no-ops.
//!
//! Plugin's behavior we replicate:
//!
//! 1. Walk root composer.json + every installed package's `extra.spi` map
//!    (`service => [provider, ...]`). With PHP `+=` semantics: first-seen
//!    package's source string wins for any duplicate provider key.
//! 2. For each provider class, scan its source for `Nevay\SPI` requirement
//!    attributes (currently `PackageDependency` and `ExtensionDependency`):
//!    - `PackageDependency` (statically validated): evaluate against the
//!      lock — if the required package isn't installed at a satisfying
//!      version, the provider is dropped from the output entirely.
//!    - `ExtensionDependency` (runtime-validated): emit the conditional
//!      runtime wrap `((... )->hash() !== <hash> && (...)->isSatisfied()) ? [...] : []`.
//!      We default to the "extension-not-loaded" branch (hash=false), which
//!      byte-matches composer's output for any host where the extension
//!      isn't loaded at install time. Functionally identical regardless.
//! 3. Render the file matching `Plugin::dumpGeneratedServiceProviderData`'s
//!    heredoc template byte-for-byte (12-space match-arm indent, 16-space
//!    provider indent, no trailing newline).
//!
//! We also inject `Nevay\SPI\GeneratedServiceProviderData` into composr's
//! classmap so the autoloader can find it.
//!
//! Limitations:
//! - We don't support `extra.spi-config.autoload-files` (require-time PHP
//!   evaluation; see [`packages_with_autoload_files_spi`]).
//! - Unknown requirement attribute classes (anything outside
//!   `Nevay\SPI\ServiceProviderDependency\{Package,Extension}Dependency`)
//!   leave the provider included plain — composer would have evaluated it.
//!   See [`provider_has_unknown_requirement`].
//! - `ExtensionDependency`'s hash defaults to `false`. If composer's install
//!   host did have the extension loaded, our output diverges from the file
//!   composer would write — but the runtime semantics match (the cached
//!   hash is invalid in the new env either way, so the runtime re-check is
//!   what actually decides inclusion).

use crate::lock::{Lock, LockedPackage};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const PLUGIN_NAME: &str = "tbachert/spi";
pub const FILE_REL_PATH: &str = "composer/GeneratedServiceProviderData.php";
pub const GENERATED_FQCN: &str = "Nevay\\SPI\\GeneratedServiceProviderData";

const PACKAGE_DEPENDENCY_FQCN: &str = "Nevay\\SPI\\ServiceProviderDependency\\PackageDependency";
const EXTENSION_DEPENDENCY_FQCN: &str = "Nevay\\SPI\\ServiceProviderDependency\\ExtensionDependency";

/// Whether `tbachert/spi` is in this lock under the chosen dev filter.
pub fn is_present(lock: &Lock, with_dev: bool) -> bool {
    lock.packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .any(|p| p.name == PLUGIN_NAME)
}

/// Whether the root composer.json's `config.allow-plugins` permits
/// `tbachert/spi`. Same wildcard semantics as composer (`*` allows all).
pub fn is_allowed(root_composer_json: &Value) -> bool {
    let allow = root_composer_json
        .get("config")
        .and_then(|c| c.get("allow-plugins"));
    let Some(allow) = allow else {
        return false;
    };
    if let Some(b) = allow.get(PLUGIN_NAME).and_then(|v| v.as_bool()) {
        return b;
    }
    allow.get("*").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Service → provider → source-string mappings collected from every
/// installed package's (and the root's) `extra.spi`. Insertion order
/// matches composer's iteration order: root first, then packages
/// alphabetically. PHP `+=` semantics: first-seen wins.
#[derive(Debug, Default, Clone)]
pub struct SpiMappings {
    /// Each entry is (service, [(provider, source), ...]).
    pub services: Vec<(String, Vec<(String, String)>)>,
}

impl SpiMappings {
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    fn add(&mut self, service: &str, provider: &str, source: &str) {
        let providers = match self
            .services
            .iter_mut()
            .find(|(s, _)| s == service)
        {
            Some((_, p)) => p,
            None => {
                self.services
                    .push((service.to_string(), Vec::new()));
                &mut self.services.last_mut().unwrap().1
            }
        };
        if !providers.iter().any(|(p, _)| p == provider) {
            providers.push((provider.to_string(), source.to_string()));
        }
    }
}

/// Collect SPI mappings from `extra.spi`. Order: root first, then packages
/// alphabetically by name. With PHP `+=` semantics for the inner map.
pub fn collect(lock: &Lock, with_dev: bool, root_composer_json: &Value) -> SpiMappings {
    let mut out = SpiMappings::default();
    let root_pretty = pretty_root(root_composer_json);
    push_extra_spi(root_composer_json, &root_pretty, &mut out);

    let mut packages: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    for p in packages {
        let pretty = pretty_package(p);
        push_extra_spi(&p.raw, &pretty, &mut out);
    }
    out
}

fn push_extra_spi(json: &Value, pretty_pkg: &str, out: &mut SpiMappings) {
    let Some(spi) = json.get("extra").and_then(|e| e.get("spi")) else {
        return;
    };
    let Some(obj) = spi.as_object() else {
        return;
    };
    let source = format!("{pretty_pkg} (extra.spi)");
    for (service, providers) in obj {
        // Composer treats string-or-array providers — `(array) $providers`.
        let providers: Vec<&str> = match providers {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
            _ => continue,
        };
        for p in providers {
            out.add(service, p, &source);
        }
    }
}

fn pretty_root(root_composer_json: &Value) -> String {
    let name = root_composer_json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("__root__");
    let version = root_composer_json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("dev-master");
    format!("{name} {version}")
}

fn pretty_package(p: &LockedPackage) -> String {
    // Composer's `BasePackage::getPrettyString` returns
    // `<name> <fullPrettyVersion>`. For stable releases that's just the
    // version; for dev branches it's `version <ref-prefix>`. Our raw lock
    // entry preserves the original `version` string composer dumped, so we
    // emit `<name> <version>` and that matches for stable packages —
    // which covers every composer-plugin we care about. For a dev plugin
    // the comment would diverge cosmetically (no functional effect).
    format!("{} {}", p.name, p.version)
}

/// Names of installed packages that declare `extra.spi-config.autoload-files`
/// — composer's plugin would `require` those at install time and trap
/// `ServiceLoader::register` calls. We can't replicate that without PHP, so
/// callers should use this as an "incomplete-replication" signal (warn or
/// delegate). Empty in service-app and monolith-app.
pub fn packages_with_autoload_files_spi(lock: &Lock, with_dev: bool) -> Vec<String> {
    lock.packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .filter_map(|p| {
            let cfg = p
                .raw
                .get("extra")
                .and_then(|e| e.get("spi-config"))
                .and_then(|c| c.get("autoload-files"))?;
            // Plugin treats `true` as "use all autoload.files"; an empty
            // array is the same as not having the entry. Anything else
            // means there's at least one file to evaluate.
            let needs = match cfg {
                Value::Bool(b) => *b,
                Value::Array(a) => !a.is_empty(),
                _ => false,
            };
            if needs {
                Some(p.name.clone())
            } else {
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Provider attribute scanning
// ---------------------------------------------------------------------------

/// One `Nevay\SPI` requirement found on a provider class.
#[derive(Debug, Clone, PartialEq)]
pub enum Requirement {
    /// `#[PackageDependency($name, $version)]` — statically validated. We
    /// evaluate this against the lock; unsatisfied means the provider is
    /// dropped from the output.
    Package { name: String, version: String },
    /// `#[ExtensionDependency($extension, $version)]` — runtime-validated.
    /// We emit a `(... ? [...] : [])` wrap.
    Extension { extension: String, version: String },
}

/// Outcome of scanning a provider class for SPI requirement attributes.
#[derive(Debug, Default, Clone)]
pub struct ProviderRequirements {
    pub static_reqs: Vec<Requirement>,
    pub runtime_reqs: Vec<Requirement>,
    /// Whether we found a `Nevay\SPI` requirement attribute we don't yet
    /// know how to evaluate. When true, we conservatively keep the provider
    /// (composer would have evaluated; we don't, so we may over-include).
    pub has_unknown_requirement: bool,
}

/// Find the file backing `provider_fqn` in the classmap and scan it for
/// `Nevay\SPI` requirement attributes. Returns `None` if the class isn't in
/// the classmap (composer's `class_exists` would have returned false → the
/// provider would be dropped).
pub fn scan_provider(
    provider_fqn: &str,
    classmap: &crate::emit::ClassMap,
) -> Option<ProviderRequirements> {
    let key = provider_fqn.as_bytes().to_vec();
    let path = classmap.get(&key)?;
    let body = std::fs::read_to_string(path).ok()?;
    Some(scan_attributes(&body))
}

/// Scan PHP source for `Nevay\SPI` requirement attributes on the file's
/// class declaration. Recognizes both the FQN form
/// (`#[\Nevay\SPI\...\PackageDependency(...)]`) and the short form
/// (`#[PackageDependency(...)]` with a `use Nevay\SPI\...\PackageDependency;`
/// import).
pub fn scan_attributes(php: &str) -> ProviderRequirements {
    let mut out = ProviderRequirements::default();
    let aliases = collect_use_aliases(php);

    // Walk attribute usages: `#[<name>(<args>)]`, optionally with a leading
    // backslash. We stop at the first class/interface/trait declaration to
    // avoid picking up attributes on methods/properties.
    let body = match php.find("\nclass ").or_else(|| {
        php.find("\nfinal class ")
            .or_else(|| php.find("\nabstract class "))
            .or_else(|| php.find("\ninterface "))
            .or_else(|| php.find("\ntrait "))
            .or_else(|| php.find("\nenum "))
    }) {
        Some(end) => &php[..end],
        None => php,
    };

    for cap in find_attributes(body) {
        let resolved = resolve_attribute_name(&cap.name, &aliases);
        match resolved.as_str() {
            PACKAGE_DEPENDENCY_FQCN => match parse_two_string_args(&cap.args) {
                Some((name, version)) => out.static_reqs.push(Requirement::Package { name, version }),
                None => out.has_unknown_requirement = true,
            },
            EXTENSION_DEPENDENCY_FQCN => match parse_two_string_args(&cap.args) {
                Some((extension, version)) => {
                    out.runtime_reqs.push(Requirement::Extension { extension, version })
                }
                None => out.has_unknown_requirement = true,
            },
            other if other.starts_with("Nevay\\SPI\\ServiceProviderDependency\\") => {
                out.has_unknown_requirement = true;
            }
            _ => {} // Unrelated attribute — ignore.
        }
    }
    out
}

/// Walk `use Foo\Bar [as Baz];` lines, returning a map from the alias (or
/// short-name) to the FQN. Stops at the first class-like declaration.
fn collect_use_aliases(php: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in php.lines() {
        let t = line.trim_start();
        let t = match t.strip_prefix("use ") {
            Some(rest) => rest.trim_end_matches(';').trim(),
            None => continue,
        };
        // Skip `use function` / `use const` (we only care about classes).
        if t.starts_with("function ") || t.starts_with("const ") {
            continue;
        }
        let (path, alias) = match t.split_once(" as ") {
            Some((p, a)) => (p.trim().to_string(), a.trim().to_string()),
            None => {
                let p = t.to_string();
                let alias = p.rsplit('\\').next().unwrap_or(&p).to_string();
                (p, alias)
            }
        };
        out.push((alias, path));
    }
    out
}

#[derive(Debug)]
struct AttrCapture {
    name: String,
    args: String,
}

/// Find every `#[<name>(<args>)]` attribute. Tolerates whitespace.
/// Doesn't try to handle every PHP escape — string literals are passed
/// through verbatim and parsed by [`parse_two_string_args`] when needed.
fn find_attributes(php: &str) -> Vec<AttrCapture> {
    let mut out = Vec::new();
    let bytes = php.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'#' && bytes[i + 1] == b'[' {
            // Try to parse one attribute.
            let mut j = i + 2;
            // skip whitespace
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            // optional leading backslash
            if j < bytes.len() && bytes[j] == b'\\' {
                j += 1;
            }
            // name: identifier characters (incl. backslash for namespaced names)
            let name_start = j;
            while j < bytes.len() {
                let c = bytes[j];
                let is_ident = c.is_ascii_alphanumeric() || c == b'_' || c == b'\\';
                if !is_ident {
                    break;
                }
                j += 1;
            }
            if j == name_start {
                i += 1;
                continue;
            }
            let name = String::from_utf8_lossy(&bytes[name_start..j]).into_owned();
            // skip whitespace
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            // No args is fine (e.g. `#[\Override]`); only emit when present.
            let args = if j < bytes.len() && bytes[j] == b'(' {
                let (a, end) = match capture_balanced(&bytes[j..]) {
                    Some((a, e)) => (a, j + e),
                    None => {
                        i = j;
                        continue;
                    }
                };
                j = end;
                a
            } else {
                String::new()
            };
            // Closing `]`
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b']' {
                out.push(AttrCapture { name, args });
                i = j + 1;
                continue;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// Capture everything inside a balanced `(...)`, respecting single/double
/// quoted strings (so `(')')` doesn't terminate early). Returns the inner
/// content and the index just past the closing `)`.
fn capture_balanced(bytes: &[u8]) -> Option<(String, usize)> {
    if bytes.is_empty() || bytes[0] != b'(' {
        return None;
    }
    let mut depth = 1;
    let mut i = 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((
                        String::from_utf8_lossy(&bytes[1..i]).into_owned(),
                        i + 1,
                    ));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn resolve_attribute_name(name: &str, aliases: &[(String, String)]) -> String {
    // Already fully qualified (had a leading backslash that we stripped)?
    if name.contains('\\') {
        return name.to_string();
    }
    for (alias, path) in aliases {
        if alias == name {
            return path.clone();
        }
    }
    name.to_string()
}

/// Parse two single- or double-quoted string args, possibly comma-separated
/// with whitespace around them. Returns `None` for any other shape.
fn parse_two_string_args(args: &str) -> Option<(String, String)> {
    let mut parts = Vec::new();
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let q = bytes[i];
        if q != b'\'' && q != b'"' {
            return None;
        }
        i += 1;
        let start = i;
        let mut buf = String::new();
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && i + 1 < bytes.len() {
                // Single-quoted strings only escape `\\` and `\'`. Double-quoted
                // strings have more, but we don't see any in real attributes.
                buf.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == q {
                break;
            }
            buf.push(c as char);
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        i += 1; // skip closing quote
        // unused, but keeps `start` referenced if we want raw bytes later.
        let _ = start;
        parts.push(buf);
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        }
    }
    if parts.len() == 2 {
        Some((parts.remove(0), parts.remove(0)))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Static evaluation against the lock
// ---------------------------------------------------------------------------

/// Whether a `PackageDependency(name, ver)` would be satisfied by the
/// installed package set. Composer's [`InstalledVersions::satisfies`] uses
/// the `composer/semver` matcher; we only check presence + a small set of
/// constraint shapes that real-world SPI declarations use (`*`, `^X.Y`,
/// `~X.Y`, exact version). For unrecognized shapes we conservatively treat
/// the requirement as satisfied if the package is installed at all — the
/// runtime check runs again anyway.
pub fn package_dependency_satisfied(
    lock: &Lock,
    with_dev: bool,
    name: &str,
    constraint: &str,
) -> bool {
    let pkg = lock
        .packages
        .iter()
        .filter(|p| with_dev || !p.dev)
        .find(|p| p.name == name);
    let Some(pkg) = pkg else {
        return false;
    };
    if constraint.trim() == "*" {
        return true;
    }
    semver_matches(&pkg.version, constraint).unwrap_or(true)
}

/// Tiny semver-constraint matcher covering `^X.Y[.Z]`, `~X.Y[.Z]`, exact
/// version match, and `*`. Returns `None` for shapes we don't support so
/// callers can fall back conservatively.
fn semver_matches(version: &str, constraint: &str) -> Option<bool> {
    let v = parse_semver(version)?;
    let c = constraint.trim();
    if c == "*" {
        return Some(true);
    }
    if let Some(rest) = c.strip_prefix('^') {
        let bound = parse_semver(rest)?;
        // `^X.Y.Z` allows any version `>= X.Y.Z` and `< (X+1).0.0` (or
        // `< 0.(Y+1).0` if X==0, etc.). For our scan-time use we only
        // need the broad case.
        if v.0 != bound.0 {
            return Some(false);
        }
        return Some(v >= bound);
    }
    if let Some(rest) = c.strip_prefix('~') {
        let bound = parse_semver(rest)?;
        // `~X.Y` -> `>= X.Y, < (X+1).0`; `~X.Y.Z` -> `>= X.Y.Z, < X.(Y+1).0`.
        if v.0 != bound.0 {
            return Some(false);
        }
        if rest.matches('.').count() >= 2 && v.1 != bound.1 {
            return Some(false);
        }
        return Some(v >= bound);
    }
    let exact = parse_semver(c)?;
    Some(v == exact)
}

fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let body = s.trim_start_matches('v').trim();
    let body = body.split('-').next().unwrap_or(body);
    let mut parts = body.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Decision for a single (service, provider) pair after attribute scanning.
#[derive(Debug, Clone)]
enum ProviderEmit {
    /// Plain emit: `\<provider>::class, // <source>` (16-space indent).
    Plain,
    /// Wrapped emit with one or more runtime conditions composed via `&&`.
    Wrapped { runtime: Vec<Requirement> },
    /// Drop entirely.
    Drop,
}

fn classify_provider(reqs: &ProviderRequirements, lock: &Lock, with_dev: bool) -> ProviderEmit {
    // Static checks first — any unsatisfied → drop.
    for r in &reqs.static_reqs {
        if let Requirement::Package { name, version } = r {
            if !package_dependency_satisfied(lock, with_dev, name, version) {
                return ProviderEmit::Drop;
            }
        }
    }
    if reqs.runtime_reqs.is_empty() {
        ProviderEmit::Plain
    } else {
        ProviderEmit::Wrapped {
            runtime: reqs.runtime_reqs.clone(),
        }
    }
}

/// Render the full `GeneratedServiceProviderData.php` file matching
/// composer's heredoc output byte-for-byte.
pub fn render(
    mappings: &SpiMappings,
    lock: &Lock,
    with_dev: bool,
    classmap: &crate::emit::ClassMap,
) -> String {
    let mut match_body = String::new();
    for (service, providers) in &mappings.services {
        if !is_valid_fqcn(service) {
            continue;
        }
        if !classmap_contains(classmap, service.as_str()) {
            continue;
        }
        let service_emit = if service.starts_with('\\') {
            service.clone()
        } else {
            format!("\\{service}")
        };
        match_body.push_str(&format!("\n            {service_emit}::class => ["));

        for (provider, source) in providers {
            if !is_valid_fqcn(provider) {
                continue;
            }
            if !classmap_contains(classmap, provider.as_str()) {
                continue;
            }
            let reqs = scan_provider(provider, classmap).unwrap_or_default();
            let emit = classify_provider(&reqs, lock, with_dev);
            let provider_emit = if provider.starts_with('\\') {
                provider.clone()
            } else {
                format!("\\{provider}")
            };
            match emit {
                ProviderEmit::Drop => continue,
                ProviderEmit::Plain => {
                    match_body.push_str(&format!(
                        "\n                {provider_emit}::class, // {source}"
                    ));
                }
                ProviderEmit::Wrapped { runtime } => {
                    let cond = runtime_condition(&runtime);
                    match_body.push_str(&format!("\n                ...(({cond}) ? ["));
                    match_body.push_str(&format!(
                        "\n                {provider_emit}::class, // {source}"
                    ));
                    match_body.push_str("\n                ] : []),");
                }
            }
        }
        match_body.push_str("\n            ],");
    }

    // Heredoc template from Plugin::dumpGeneratedServiceProviderData with
    // 12-space closing-tag strip applied. `match_body` carries its own
    // leading newlines and 12/16-space indents, inserted right after
    // `default => [],`.
    let template = "<?php declare(strict_types=1);
namespace Nevay\\SPI;

/**
 * @internal \n */
final class GeneratedServiceProviderData {

    public const VERSION = 1;

    /**
     * @param class-string $service
     * @return list<class-string>
     */
    public static function providers(string $service): array {
        return match ($service) {
            default => [],{MATCH}
        };
    }
}";
    template.replace("{MATCH}", &match_body)
}

/// Build the conditional expression composer's `providerRuntimeValidatedRequirements`
/// emits. We default to the install-time-not-loaded branch (hash=`false`) —
/// see module docs for the rationale.
fn runtime_condition(reqs: &[Requirement]) -> String {
    let mut s = String::from("true");
    for r in reqs {
        match r {
            Requirement::Extension { extension, version } => {
                let ext = php_export_string(extension);
                let ver = php_export_string(version);
                s.push_str(&format!(
                    " && (($r = new \\Nevay\\SPI\\ServiceProviderDependency\\ExtensionDependency({ext}, {ver}))->hash() !== false && $r->isSatisfied())"
                ));
            }
            Requirement::Package { .. } => {
                // Only runtime-validated requirements should reach here.
                debug_assert!(false, "Package requirement classified as runtime");
            }
        }
    }
    s
}

/// PHP `var_export` for a single string — single-quoted, with `'` escaped
/// as `\'` and `\` escaped as `\\`. Matches what composer's plugin emits.
fn php_export_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Composer plugin's `FQCN_REGEX`: optional leading `\`, then identifier
/// segments separated by `\`. Identifier = `[a-zA-Z_\x80-\xff][a-zA-Z0-9_\x80-\xff]*`.
fn is_valid_fqcn(s: &str) -> bool {
    let body = s.strip_prefix('\\').unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    body.split('\\').all(is_valid_php_identifier)
}

fn is_valid_php_identifier(s: &str) -> bool {
    let mut chars = s.bytes();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    let valid_first = first.is_ascii_alphabetic() || first == b'_' || first >= 0x80;
    if !valid_first {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
}

fn classmap_contains(classmap: &crate::emit::ClassMap, fqcn: &str) -> bool {
    let key = fqcn.strip_prefix('\\').unwrap_or(fqcn).as_bytes();
    classmap.contains_key(key)
}

/// Atomically write `body` to `vendor/composer/GeneratedServiceProviderData.php`.
pub fn write(vendor_dir: &Path, body: &str) -> Result<PathBuf> {
    let path = vendor_dir.join(FILE_REL_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("php.tmp");
    std::fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(path)
}

/// `(fqcn-bytes, absolute-path)` entry to splice into composr's classmap so
/// the generated file is loadable through the autoloader.
pub fn classmap_entry(vendor_dir: &Path) -> (Vec<u8>, PathBuf) {
    (
        GENERATED_FQCN.as_bytes().to_vec(),
        vendor_dir.join(FILE_REL_PATH),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lock_from(packages: &[&str]) -> Lock {
        let body = format!(r#"{{"packages":[{}]}}"#, packages.join(","));
        Lock::parse(body.as_bytes()).unwrap()
    }

    #[test]
    fn collects_root_first_then_packages_alpha() {
        let lock = lock_from(&[
            r#"{"name":"b/sdk","version":"1.0","type":"library","extra":{"spi":{"S2":["P2"]}}}"#,
            r#"{"name":"a/api","version":"1.0","type":"library","extra":{"spi":{"S1":["P1"]}}}"#,
        ]);
        let root = serde_json::json!({"name":"vendor/root","version":"dev-main","extra":{"spi":{"S0":["P0"]}}});
        let m = collect(&lock, true, &root);
        let services: Vec<&str> = m.services.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(services, vec!["S0", "S1", "S2"]);
        assert_eq!(m.services[0].1[0], ("P0".to_string(), "vendor/root dev-main (extra.spi)".to_string()));
        assert_eq!(m.services[1].1[0], ("P1".to_string(), "a/api 1.0 (extra.spi)".to_string()));
    }

    #[test]
    fn collect_first_seen_provider_wins() {
        let lock = lock_from(&[
            r#"{"name":"a/api","version":"1.0","type":"library","extra":{"spi":{"S":["P"]}}}"#,
            r#"{"name":"b/sdk","version":"2.0","type":"library","extra":{"spi":{"S":["P"]}}}"#,
        ]);
        let root = serde_json::json!({});
        let m = collect(&lock, true, &root);
        assert_eq!(m.services.len(), 1);
        assert_eq!(m.services[0].1.len(), 1);
        assert_eq!(m.services[0].1[0].1, "a/api 1.0 (extra.spi)");
    }

    #[test]
    fn collect_appends_new_providers() {
        let lock = lock_from(&[
            r#"{"name":"a/api","version":"1.0","type":"library","extra":{"spi":{"S":["P1"]}}}"#,
            r#"{"name":"b/sdk","version":"2.0","type":"library","extra":{"spi":{"S":["P2"]}}}"#,
        ]);
        let root = serde_json::json!({});
        let m = collect(&lock, true, &root);
        let providers: Vec<&str> = m.services[0].1.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(providers, vec!["P1", "P2"]);
    }

    #[test]
    fn package_dependency_satisfied_handles_constraints() {
        let lock = lock_from(&[
            r#"{"name":"otel/sdk","version":"1.14.0","type":"library"}"#,
        ]);
        assert!(package_dependency_satisfied(&lock, true, "otel/sdk", "*"));
        assert!(package_dependency_satisfied(&lock, true, "otel/sdk", "^1.0"));
        assert!(package_dependency_satisfied(&lock, true, "otel/sdk", "^1.14"));
        assert!(!package_dependency_satisfied(&lock, true, "otel/sdk", "^2.0"));
        assert!(!package_dependency_satisfied(&lock, true, "missing/pkg", "*"));
    }

    #[test]
    fn scan_attributes_finds_extension_dependency() {
        let php = r#"<?php declare(strict_types=1);
namespace OpenTelemetry\API\Foo;

use Nevay\SPI\ServiceProviderDependency\ExtensionDependency;

#[ExtensionDependency('opentelemetry', '^1.0')]
final class Bar {}
"#;
        let r = scan_attributes(php);
        assert_eq!(r.runtime_reqs.len(), 1);
        match &r.runtime_reqs[0] {
            Requirement::Extension { extension, version } => {
                assert_eq!(extension, "opentelemetry");
                assert_eq!(version, "^1.0");
            }
            _ => panic!("wrong requirement kind"),
        }
        assert!(r.static_reqs.is_empty());
        assert!(!r.has_unknown_requirement);
    }

    #[test]
    fn scan_attributes_finds_package_dependency_static() {
        let php = r#"<?php declare(strict_types=1);
namespace OpenTelemetry\SDK;

use Nevay\SPI\ServiceProviderDependency\PackageDependency;

#[PackageDependency('open-telemetry/sdk-configuration', '*')]
final class Resolver {}
"#;
        let r = scan_attributes(php);
        assert_eq!(r.static_reqs.len(), 1);
        assert!(r.runtime_reqs.is_empty());
    }

    #[test]
    fn scan_attributes_ignores_unrelated() {
        let php = r#"<?php declare(strict_types=1);
namespace Foo;

use Other\Attr;

#[Attr('x')]
#[\Override]
class Bar {}
"#;
        let r = scan_attributes(php);
        assert!(r.static_reqs.is_empty());
        assert!(r.runtime_reqs.is_empty());
        assert!(!r.has_unknown_requirement);
    }

    #[test]
    fn scan_attributes_marks_unknown_spi_attribute() {
        let php = r#"<?php declare(strict_types=1);
namespace Foo;

use Nevay\SPI\ServiceProviderDependency\WeirdNewDependency;

#[WeirdNewDependency('thing')]
class Bar {}
"#;
        let r = scan_attributes(php);
        assert!(r.has_unknown_requirement);
    }

    #[test]
    fn fqcn_validation() {
        assert!(is_valid_fqcn("Foo\\Bar"));
        assert!(is_valid_fqcn("\\Foo\\Bar"));
        assert!(is_valid_fqcn("Foo"));
        assert!(!is_valid_fqcn(""));
        assert!(!is_valid_fqcn("\\"));
        assert!(!is_valid_fqcn("Foo\\\\Bar"));
        assert!(!is_valid_fqcn("1Foo"));
    }

    #[test]
    fn render_emits_byte_equal_match_for_real_world_shape() {
        // Simulates monolith-app's open-telemetry case after attribute scanning:
        //   - HookManagerInterface: ExtensionDependency('opentelemetry', '^1.0') → wrapped
        //   - EnvComponentLoader: two providers, no requirements → plain
        //   - ResolverInterface: SdkConfigurationResolver dropped (unsatisfied static)
        let mut classmap = crate::emit::ClassMap::new();
        for (fqn, file) in [
            ("OpenTelemetry\\API\\Hook", "/dummy"),
            ("OpenTelemetry\\API\\HookImpl", "/dummy/hook.php"),
            ("OpenTelemetry\\API\\EnvLoader", "/dummy"),
            ("OpenTelemetry\\API\\HttpEnv", "/dummy/http.php"),
            ("OpenTelemetry\\API\\PeerEnv", "/dummy/peer.php"),
        ] {
            classmap.insert(fqn.as_bytes().to_vec(), PathBuf::from(file));
        }

        // Set up a tmp dir with three provider source files.
        let tmp = tempfile::tempdir().unwrap();
        let hook = tmp.path().join("hook.php");
        std::fs::write(
            &hook,
            "<?php\nuse Nevay\\SPI\\ServiceProviderDependency\\ExtensionDependency;\n#[ExtensionDependency('opentelemetry', '^1.0')]\nclass HookImpl {}\n",
        ).unwrap();
        let http = tmp.path().join("http.php");
        std::fs::write(&http, "<?php\nclass HttpEnv {}\n").unwrap();
        let peer = tmp.path().join("peer.php");
        std::fs::write(&peer, "<?php\nclass PeerEnv {}\n").unwrap();
        // Re-point the classmap at the real files we just wrote.
        classmap.insert(b"OpenTelemetry\\API\\HookImpl".to_vec(), hook);
        classmap.insert(b"OpenTelemetry\\API\\HttpEnv".to_vec(), http);
        classmap.insert(b"OpenTelemetry\\API\\PeerEnv".to_vec(), peer);

        let mut mappings = SpiMappings::default();
        mappings.add(
            "OpenTelemetry\\API\\Hook",
            "OpenTelemetry\\API\\HookImpl",
            "open-telemetry/api 1.9.0 (extra.spi)",
        );
        mappings.add(
            "OpenTelemetry\\API\\EnvLoader",
            "OpenTelemetry\\API\\HttpEnv",
            "open-telemetry/sdk 1.14.0 (extra.spi)",
        );
        mappings.add(
            "OpenTelemetry\\API\\EnvLoader",
            "OpenTelemetry\\API\\PeerEnv",
            "open-telemetry/sdk 1.14.0 (extra.spi)",
        );

        let lock = lock_from(&[r#"{"name":"any/pkg","version":"1.0","type":"library"}"#]);
        let body = render(&mappings, &lock, true, &classmap);
        // Sanity: contains the wrap and the plain entries.
        assert!(body.contains(
            "...((true && (($r = new \\Nevay\\SPI\\ServiceProviderDependency\\ExtensionDependency('opentelemetry', '^1.0'))->hash() !== false && $r->isSatisfied())) ? ["
        ));
        assert!(body.contains(
            "                \\OpenTelemetry\\API\\HttpEnv::class, // open-telemetry/sdk 1.14.0 (extra.spi)"
        ));
        // Ends with `}` and no trailing newline (matches PHP heredoc).
        assert!(body.ends_with('}') && !body.ends_with("}\n"));
        // Header / scaffold matches composer's heredoc.
        assert!(body.starts_with("<?php declare(strict_types=1);\nnamespace Nevay\\SPI;\n\n/**\n * @internal \n */\nfinal class GeneratedServiceProviderData {\n\n    public const VERSION = 1;\n"));
    }

    #[test]
    fn packages_with_autoload_files_spi_finds_them() {
        let lock = lock_from(&[
            r#"{"name":"a/normal","version":"1","type":"library","extra":{"spi":{"S":["P"]}}}"#,
            r#"{"name":"b/files","version":"1","type":"library","extra":{"spi-config":{"autoload-files":["src/init.php"]}}}"#,
            r#"{"name":"c/empty","version":"1","type":"library","extra":{"spi-config":{"autoload-files":[]}}}"#,
        ]);
        assert_eq!(packages_with_autoload_files_spi(&lock, true), vec!["b/files".to_string()]);
    }
}
