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
    /// `local` (default) or `cf`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Base URL (http) or root dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Env var **name** holding the bearer token — never the token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

/// Publish-target routing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Publish {
    /// Registry name `publish` targets by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// The tamias config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Install store location.
    pub store: Location,
    /// Fetch-cache location.
    pub cache: Location,
    /// Named registries.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub registries: BTreeMap<String, Registry>,
    /// Publish routing.
    pub publish: Publish,
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
            },
        );
        assert_eq!(c.registries["r"].url.as_deref(), Some("u"));
    }
}
