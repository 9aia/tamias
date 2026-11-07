//! `tamias.{yaml,yml,toml}` — the package manifest. YAML is preferred
//! when several exist; TOML stays the fallback.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// Manifest file names tried, in order — YAML first, TOML last.
pub const MANIFEST_NAMES: [&str; 3] = ["tamias.yaml", "tamias.yml", "tamias.toml"];

/// The package manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// Bare package name — charset `[A-Za-z0-9][A-Za-z0-9._-]*`.
    pub name: String,
    /// Optional namespace (`@scope/name`).
    pub scope: Option<String>,
    /// Author-declared version (semver-ish label).
    pub version: Option<String>,
    /// Free-text description.
    pub description: Option<String>,
    /// Content type (`peregrin-bundle`, `myco-config`, …).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Base packages to compose under this payload — installed store
    /// names or installable source refs. Local wins; earlier includes
    /// win over later ones.
    pub include: Vec<String>,
    /// Direct dependencies — refs this project wants installed.
    pub deps: Vec<String>,
}

impl Manifest {
    /// The manifest file inside `dir`, trying [`MANIFEST_NAMES`].
    #[must_use]
    pub fn path_in(dir: &Path) -> Option<PathBuf> {
        MANIFEST_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
    }

    /// Load + validate the manifest inside `dir`.
    ///
    /// # Errors
    /// Fails when no manifest exists or it doesn't validate.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let Some(path) = Self::path_in(dir) else {
            return Err(Error::Failed(format!(
                "no tamias.yaml/.yml/.toml in {}",
                dir.display()
            )));
        };
        Self::load_at(&path)
    }

    /// Load + validate a manifest file; syntax dispatched on the
    /// `.yaml`/`.yml` extension (everything else parses as TOML).
    ///
    /// # Errors
    /// Fails on read, parse, or validation error.
    pub fn load_at(path: &Path) -> Result<Self, Error> {
        let text =
            std::fs::read_to_string(path).map_err(Error::io(format!("read {}", path.display())))?;
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        );
        let manifest: Self = if is_yaml {
            serde_yaml_ng::from_str::<Self>(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))?
        } else {
            toml::from_str(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))?
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Semantic checks the schema can't express.
    ///
    /// # Errors
    /// Fails on an invalid name/version.
    pub fn validate(&self) -> Result<(), Error> {
        let valid = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        };
        if !self.name.is_empty() && !valid(&self.name) {
            return Err(Error::Failed(format!(
                "invalid package name '{}' — expected [A-Za-z0-9][A-Za-z0-9._-]*",
                self.name
            )));
        }
        if let Some(scope) = &self.scope {
            if !valid(scope) {
                return Err(Error::Failed(format!("invalid scope '{scope}'")));
            }
        }
        Ok(())
    }

    /// Write this manifest to `path` as YAML.
    ///
    /// # Errors
    /// Propagates serialize + write failures.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let text = serde_yaml_ng::to_string(self)
            .map_err(|e| Error::Failed(format!("serialize manifest: {e}")))?;
        std::fs::write(path, text).map_err(Error::io(format!("write {}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn load_yaml_toml_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tamias.toml"),
            "name = \"x\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let manifest = Manifest::load(dir.path()).unwrap();
        assert_eq!(manifest.name, "x");
        assert!(Manifest::path_in(dir.path()).is_some());
        let saved = dir.path().join("saved.yaml");
        manifest.save(&saved).unwrap();
        assert_eq!(Manifest::load_at(&saved).unwrap().name, "x");
        let empty = tempfile::tempdir().unwrap();
        assert!(Manifest::load(empty.path()).is_err());
        assert!(Manifest {
            name: "bad name".into(),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(Manifest {
            name: "x".into(),
            scope: Some("bad scope".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(Manifest::default().validate().is_ok());
    }
}
