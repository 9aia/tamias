#![cfg_attr(coverage, feature(coverage_attribute))]
#![cfg_attr(test, allow(clippy::unwrap_used))]
//! Config port (`tamias-config`) — the tamias config model + format port.
//!
//! **YAML is the standard** (`tamias.yml` / `tamias.yaml`); TOML
//! (`tamias.toml`) is the supported alternative that parses into the same
//! [`Config`]. Format impls live in `tamias-config-yml` /
//! `tamias-config-toml`; the port never links them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tamias_core::Error;

/// Config file names tried, in order — the standard YAML first, TOML last.
pub const CONFIG_NAMES: [&str; 3] = ["tamias.yml", "tamias.yaml", "tamias.toml"];

/// Env var naming an explicit config file (overrides discovery).
pub const CONFIG_ENV: &str = "TAMIAS_CONFIG";

/// A store/cache location.
///
/// `store.root`/`cache.root` resolve to the on-disk roots the CLI's
/// `Env` uses — but only when `kind` is unset or `local`, and only when
/// the corresponding `TAMIAS_*`/XDG env var is unset (env wins). Any
/// other `kind` (`cf`, …) is reserved: the CLI warns and falls back to
/// the env/default roots rather than half-applying it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Location {
    /// Impl kind — `local` (default) or `cf`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Root dir (or URL base for `cf`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// A named registry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registry {
    /// Explicit kind — `local` | `http` | `cf`. Unset → inferred from
    /// `url` (http(s) → `http`, anything else → `local`), so configs
    /// written before kinds were explicit keep working.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Base URL (http) or root dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Env var **name** holding the bearer token — never the token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// Per-registry limit overrides (values + package rules).
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
}

impl Registry {
    /// The effective kind — explicit `kind` (lowercased) when set,
    /// else inferred: an http(s) `url` → `http`, anything else →
    /// `local`. `cf` is never inferred — it's the explicit opt-in for
    /// worker-managed registries (same client transport as `http`).
    #[must_use]
    pub fn effective_kind(&self) -> String {
        if let Some(kind) = &self.kind {
            return kind.to_lowercase();
        }
        match self.url.as_deref() {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                "http".to_string()
            }
            _ => "local".to_string(),
        }
    }
}

/// Publish-target routing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Publish {
    /// Registry name `publish` targets by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Search routing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Search {
    /// Registry name `search` targets by default. Falls back to
    /// `publish.default`, then the local default root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Deserialize an optional byte count — a plain integer or a human
/// string like `"256MiB"` / `"1GiB"` / `"512KiB"` (binary suffixes;
/// bare `K`/`M`/`G`/`T`/`B` and `KB`-style spellings accepted).
fn de_bytes_opt<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Num(u64),
        Str(String),
    }
    match Option::<Repr>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Repr::Num(n)) => Ok(Some(n)),
        Some(Repr::Str(s)) => parse_bytes(&s).map(Some).map_err(serde::de::Error::custom),
    }
}

/// Serialize an optional byte count as a plain integer.
fn ser_bytes_opt<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(n) => serializer.serialize_some(n),
        None => serializer.serialize_none(),
    }
}

/// Parse `<number>[ <unit>]` into bytes. Units are binary:
/// `B`/`K`/`KB`/`KiB` = 1024ⁿ — `1K`, `1KB`, `1KiB` are all 1024.
/// Fractions allowed (`1.5GiB`).
fn parse_bytes(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let value: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid byte size '{text}'"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid byte size '{text}'"));
    }
    let multiplier: u128 = match unit.trim().to_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        "p" | "pb" | "pib" => 1 << 50,
        other => return Err(format!("unknown byte unit '{other}' in '{text}'")),
    };
    let bytes = (value * multiplier as f64) as u128;
    u64::try_from(bytes).map_err(|_| format!("byte size '{text}' overflows u64"))
}

/// Byte-size limit knobs. Every field is optional — `None` inherits
/// the next level of the resolution chain (per-package override →
/// per-registry `limits` → global `limits` → built-in default; see
/// [`Config::effective_limits`]).
///
/// `packages` maps a package name — or a `*` glob (`foo*`, `*foo`,
/// `*foo*`, `*`) — to overrides for that package. It lives on the
/// global `limits` (and may also appear under a registry's `limits`
/// for registry-scoped overrides); `packages` nested *inside* a
/// package entry is ignored — overrides are one level deep.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Largest payload tarball a publish may upload, in bytes.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_bytes_opt",
        serialize_with = "ser_bytes_opt"
    )]
    pub max_upload: Option<u64>,
    /// Largest tarball a fetch may download, in bytes.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_bytes_opt",
        serialize_with = "ser_bytes_opt"
    )]
    pub max_download: Option<u64>,
    /// Per-package overrides — exact name or `*` glob → limits.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, Limits>,
}

impl Limits {
    /// True when no knob or package rule is set — used to skip empty
    /// `limits:` blocks on render.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_upload.is_none() && self.max_download.is_none() && self.packages.is_empty()
    }
}

/// Built-in upload cap — today's hardcoded `MAX_PUT` (512 MiB).
pub const DEFAULT_MAX_UPLOAD: u64 = 512 * 1024 * 1024;

/// Built-in download cap — today's hardcoded `MAX_TARBALL` (512 MiB).
pub const DEFAULT_MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// Fully-resolved limits — every knob concrete (defaults applied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveLimits {
    /// Upload cap in bytes.
    pub max_upload: u64,
    /// Download cap in bytes.
    pub max_download: u64,
}

/// `*` wildcard match — `*` matches any (possibly empty) run; every
/// literal part must appear in order. `foo*`, `*foo`, `*foo*` and `*`
/// all work; patterns without `*` are exact matches.
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    // Literal segments between `*`s: the first anchors at the start
    // (unless the pattern opens with `*`), the last at the end (unless
    // it closes with `*`), the rest must appear in order.
    let segs: Vec<&str> = pattern.split('*').filter(|s| !s.is_empty()).collect();
    let mut rest = name;
    let mut lo = 0usize;
    let mut hi = segs.len();
    if !pattern.starts_with('*') && lo < hi {
        let seg = segs[lo];
        if !rest.starts_with(seg) {
            return false;
        }
        rest = &rest[seg.len()..];
        lo += 1;
    }
    let tail = if !pattern.ends_with('*') && hi > lo {
        hi -= 1;
        Some(segs[hi])
    } else {
        None
    };
    for seg in &segs[lo..hi] {
        match rest.find(seg) {
            Some(i) => rest = &rest[i + seg.len()..],
            None => return false,
        }
    }
    tail.is_none_or(|seg| rest.ends_with(seg))
}

/// The override for `package` in `limits.packages` — exact name match
/// first, then `*` glob patterns in key order.
fn package_limits<'a>(limits: &'a Limits, package: &str) -> Option<&'a Limits> {
    if let Some(found) = limits.packages.get(package) {
        return Some(found);
    }
    limits
        .packages
        .iter()
        .find(|(pattern, _)| glob_match(pattern, package))
        .map(|(_, found)| found)
}

/// The tamias config — discovered as `./tamias.{yml,yaml,toml}` (or
/// `TAMIAS_CONFIG`/`--file`). `store`/`cache` roots feed the CLI's
/// `Env` (env vars win); `registries`/`publish`/`search` route the
/// registry verbs; `limits` caps uploads/downloads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Install store location — `root` applies when `kind` is `local`/unset
    /// and `TAMIAS_HOME`/XDG env is unset (env wins).
    pub store: Location,
    /// Fetch-cache location — same resolution rule as `store`.
    pub cache: Location,
    /// Named registries — `publish`/`search --registry <name>` and the
    /// `publish.default`/`search.default` routing resolve through these.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub registries: BTreeMap<String, Registry>,
    /// Publish routing.
    pub publish: Publish,
    /// Search routing.
    pub search: Search,
    /// Global byte limits + per-package overrides.
    #[serde(default, skip_serializing_if = "Limits::is_empty")]
    pub limits: Limits,
}

impl Config {
    /// Resolve the effective byte limits for `package` on `registry`
    /// (a `registries` key, or `None` for the unnamed/default registry).
    ///
    /// Per knob, first set wins:
    /// package override — the registry's own `limits.packages` first,
    /// then the global `limits.packages` (exact name, else `*` glob) →
    /// per-registry `limits` → global `limits` → built-in default
    /// ([`DEFAULT_MAX_UPLOAD`] / [`DEFAULT_MAX_DOWNLOAD`], 512 MiB).
    #[must_use]
    pub fn effective_limits(
        &self,
        registry: Option<&str>,
        package: Option<&str>,
    ) -> EffectiveLimits {
        let reg = registry.and_then(|name| self.registries.get(name));
        let pkg = package.and_then(|p| {
            reg.and_then(|r| package_limits(&r.limits, p))
                .or_else(|| package_limits(&self.limits, p))
        });
        let pick = |f: fn(&Limits) -> Option<u64>| {
            pkg.and_then(f)
                .or_else(|| reg.and_then(|r| f(&r.limits)))
                .or_else(|| f(&self.limits))
        };
        EffectiveLimits {
            max_upload: pick(|l| l.max_upload).unwrap_or(DEFAULT_MAX_UPLOAD),
            max_download: pick(|l| l.max_download).unwrap_or(DEFAULT_MAX_DOWNLOAD),
        }
    }
}

/// A config file format — parse text → [`Config`], and render back.
pub trait ConfigFormat: Send + Sync {
    /// The file extensions this format claims (no leading dot).
    fn extensions(&self) -> &'static [&'static str];

    /// Parse `text`.
    ///
    /// # Errors
    /// Fails on a syntax or type error.
    fn parse(&self, text: &str) -> Result<Config, Error>;

    /// Render `config` back to this format.
    ///
    /// # Errors
    /// Fails on a serialize error.
    fn render(&self, config: &Config) -> Result<String, Error>;
}

/// The format that claims `path`'s extension, if any.
#[must_use]
pub fn format_for<'a>(
    path: &Path,
    formats: &'a [&'a dyn ConfigFormat],
) -> Option<&'a dyn ConfigFormat> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    formats
        .iter()
        .copied()
        .find(|f| f.extensions().contains(&ext))
}

/// Load a specific config file, parsing with the format that claims its
/// extension.
///
/// # Errors
/// Fails on a read/parse error or an unclaimed extension.
pub fn load_file(path: &Path, formats: &[&dyn ConfigFormat]) -> Result<Config, Error> {
    let format = format_for(path, formats)
        .ok_or_else(|| Error::Failed(format!("no config format for {}", path.display())))?;
    let text =
        std::fs::read_to_string(path).map_err(Error::io(format!("read {}", path.display())))?;
    format.parse(&text)
}

/// Load the first config file found in `dir` (YAML first — [`CONFIG_NAMES`]).
///
/// # Errors
/// Fails on a read/parse error; `Ok(None)` when no config file exists.
pub fn load_in(
    dir: &Path,
    formats: &[&dyn ConfigFormat],
) -> Result<Option<(PathBuf, Config)>, Error> {
    for name in CONFIG_NAMES {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let config = load_file(&path, formats)?;
        return Ok(Some((path, config)));
    }
    Ok(None)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A toy format claiming `yml`, parsing `"ok"` to the default.
    struct Toy;
    impl ConfigFormat for Toy {
        fn extensions(&self) -> &'static [&'static str] {
            &["yml"]
        }
        fn parse(&self, text: &str) -> Result<Config, Error> {
            if text.trim() == "ok" {
                Ok(Config::default())
            } else {
                Err(Error::Failed("bad toy".into()))
            }
        }
        fn render(&self, _config: &Config) -> Result<String, Error> {
            Ok("ok".into())
        }
    }

    #[test]
    fn format_for_load_file_and_load_in() {
        let toy = Toy;
        let formats: [&dyn ConfigFormat; 1] = [&toy];
        assert!(format_for(Path::new("a.yml"), &formats).is_some());
        assert!(format_for(Path::new("a.json"), &formats).is_none());
        assert!(format_for(Path::new("noext"), &formats).is_none());

        let dir = tempfile::tempdir().unwrap();
        assert!(load_in(dir.path(), &formats).unwrap().is_none());

        let p = dir.path().join("tamias.yml");
        std::fs::write(&p, "ok").unwrap();
        let (found, cfg) = load_in(dir.path(), &formats).unwrap().unwrap();
        assert_eq!(found, p);
        assert_eq!(cfg, Config::default());

        assert!(load_file(&dir.path().join("x.json"), &formats).is_err());
        assert!(load_file(&dir.path().join("gone.yml"), &formats).is_err());
        std::fs::write(&p, "nope").unwrap();
        assert!(load_in(dir.path(), &formats).is_err());

        assert_eq!(toy.render(&Config::default()).unwrap(), "ok");
        let mut c = Config::default();
        c.store.kind = Some("local".into());
        c.publish.default = Some("r".into());
        c.registries.insert(
            "r".into(),
            Registry {
                kind: Some("cf".into()),
                url: Some("u".into()),
                token_env: Some("T".into()),
                ..Default::default()
            },
        );
        assert_eq!(c.registries["r"].url.as_deref(), Some("u"));
    }

    #[test]
    fn parse_bytes_units() {
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_bytes("1KiB").unwrap(), 1024);
        assert_eq!(parse_bytes("1KB").unwrap(), 1024);
        assert_eq!(parse_bytes("1k").unwrap(), 1024);
        assert_eq!(parse_bytes("256MiB").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_bytes("1.5GiB").unwrap(), 1610612736);
        assert_eq!(parse_bytes("2 gb").unwrap(), 2 << 30);
        assert_eq!(parse_bytes("10B").unwrap(), 10);
        assert!(parse_bytes("abc").is_err());
        assert!(parse_bytes("10XB").is_err());
        assert!(parse_bytes("-5").is_err());
    }

    #[test]
    fn glob_match_patterns() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo*", "foobar"));
        assert!(!glob_match("foo*", "barfoo"));
        assert!(glob_match("*foo", "barfoo"));
        assert!(glob_match("*foo*", "barfoobar"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(glob_match("a*b", "acb"));
        assert!(!glob_match("a*b", "ba"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(glob_match("plain", "plain"));
        assert!(!glob_match("plain", "plainx"));
    }

    #[test]
    fn effective_limits_resolution_order() {
        let mut config = Config::default();
        // nothing set → built-in defaults
        let eff = config.effective_limits(None, Some("w"));
        assert_eq!(eff.max_upload, DEFAULT_MAX_UPLOAD);
        assert_eq!(eff.max_download, DEFAULT_MAX_DOWNLOAD);

        // global limits apply
        config.limits.max_upload = Some(1024);
        config.limits.max_download = Some(2048);
        let eff = config.effective_limits(None, None);
        assert_eq!(eff.max_upload, 1024);
        assert_eq!(eff.max_download, 2048);

        // per-registry beats global
        let mut reg = Registry::default();
        reg.limits.max_upload = Some(4096);
        config.registries.insert("r".into(), reg);
        let eff = config.effective_limits(Some("r"), None);
        assert_eq!(eff.max_upload, 4096);
        assert_eq!(eff.max_download, 2048);

        // per-package (exact) beats per-registry + global
        config.limits.packages.insert(
            "w".into(),
            Limits {
                max_upload: Some(8192),
                ..Default::default()
            },
        );
        let eff = config.effective_limits(Some("r"), Some("w"));
        assert_eq!(eff.max_upload, 8192);
        // ...but the registry-scoped package rule wins over the global one
        let mut reg = config.registries.get("r").unwrap().clone();
        reg.limits.packages.insert(
            "w".into(),
            Limits {
                max_upload: Some(16384),
                ..Default::default()
            },
        );
        config.registries.insert("r".into(), reg);
        let eff = config.effective_limits(Some("r"), Some("w"));
        assert_eq!(eff.max_upload, 16384);

        // glob package override applies to non-exact names
        config.limits.packages.insert(
            "kuda-*".into(),
            Limits {
                max_upload: Some(32768),
                ..Default::default()
            },
        );
        let eff = config.effective_limits(None, Some("kuda-domain"));
        assert_eq!(eff.max_upload, 32768);
        // exact match beats a glob that also matches
        config.limits.packages.insert(
            "kuda-domain".into(),
            Limits {
                max_upload: Some(65536),
                ..Default::default()
            },
        );
        let eff = config.effective_limits(None, Some("kuda-domain"));
        assert_eq!(eff.max_upload, 65536);
        // unknown registry name → treated as unnamed
        let eff = config.effective_limits(Some("ghost"), Some("w"));
        assert_eq!(eff.max_upload, 8192);
    }

    #[test]
    fn registry_kind_inference() {
        let local = Registry::default();
        assert_eq!(local.effective_kind(), "local");
        let http = Registry {
            url: Some("https://x".into()),
            ..Default::default()
        };
        assert_eq!(http.effective_kind(), "http");
        let explicit = Registry {
            kind: Some("CF".into()),
            url: Some("/dir".into()),
            ..Default::default()
        };
        assert_eq!(explicit.effective_kind(), "cf");
    }
}
