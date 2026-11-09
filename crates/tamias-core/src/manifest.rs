//! `tamias.{yaml,yml,toml}` — the package manifest. YAML is preferred
//! when several exist; TOML stays the fallback.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// Manifest file names tried, in order — YAML first, TOML last.
pub const MANIFEST_NAMES: [&str; 3] = ["tamias.yaml", "tamias.yml", "tamias.toml"];

/// `[A-Za-z0-9][A-Za-z0-9._-]*` — the package name/scope charset.
/// Always a single safe path component.
#[must_use]
pub fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// Version-label charset — `[A-Za-z0-9][A-Za-z0-9._+-]*` with no `..`.
/// Covers semver, prerelease/build suffixes, dist-tags (`latest`), and
/// the sha short-form fallback; always a single safe path/URL component.
#[must_use]
pub fn is_valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && !s.contains("..")
}

/// The package manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
        let manifest = Self::parse_at(path)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load a *project* manifest file — `tamias.yaml` holding `deps:` /
    /// `include:` — same parse as [`Self::load_at`] but validated with
    /// [`Self::validate_project`], so a missing `name` is allowed: a
    /// project needn't be a package.
    ///
    /// # Errors
    /// Fails on read, parse, or validation error.
    pub fn load_project_at(path: &Path) -> Result<Self, Error> {
        let manifest = Self::parse_at(path)?;
        manifest.validate_project()?;
        Ok(manifest)
    }

    /// Parse a manifest file without validating it.
    fn parse_at(path: &Path) -> Result<Self, Error> {
        let text =
            std::fs::read_to_string(path).map_err(Error::io(format!("read {}", path.display())))?;
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        );
        if is_yaml {
            serde_yaml_ng::from_str::<Self>(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
        } else {
            toml::from_str(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
        }
    }

    /// Semantic checks the schema can't express — a package manifest must
    /// name itself.
    ///
    /// # Errors
    /// Fails on a missing/invalid name, scope, or version.
    pub fn validate(&self) -> Result<(), Error> {
        self.check(true)
    }

    /// Semantic checks for a *project* manifest — the same field rules as
    /// [`Self::validate`] but `name` may be empty.
    ///
    /// # Errors
    /// Fails on an invalid name/scope/version.
    pub fn validate_project(&self) -> Result<(), Error> {
        self.check(false)
    }

    fn check(&self, require_name: bool) -> Result<(), Error> {
        if self.name.is_empty() {
            if require_name {
                return Err(Error::Failed(
                    "manifest is missing a package name".to_string(),
                ));
            }
        } else if !is_valid_name(&self.name) {
            return Err(Error::Failed(format!(
                "invalid package name '{}' — expected [A-Za-z0-9][A-Za-z0-9._-]*",
                self.name
            )));
        }
        if let Some(scope) = &self.scope {
            if !is_valid_name(scope) {
                return Err(Error::Failed(format!("invalid scope '{scope}'")));
            }
        }
        if let Some(version) = &self.version {
            if !is_valid_version(version) {
                return Err(Error::Failed(format!(
                    "invalid version '{version}' — expected [A-Za-z0-9][A-Za-z0-9._+-]* with no '..'"
                )));
            }
        }
        Ok(())
    }

    /// Write this manifest to `path` as YAML.
    ///
    /// # Errors
    /// Propagates serialize + write failures.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let ctx = |e| Error::Failed(format!("serialize manifest: {e}"));
        let text = serde_yaml_ng::to_string(self).map_err(ctx)?;
        crate::write_atomic(path, text.as_bytes())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
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
        // a package manifest must name itself; a project manifest needn't
        assert!(Manifest::default().validate().is_err());
        assert!(Manifest::default().validate_project().is_ok());
        assert!(Manifest {
            name: "bad name".into(),
            ..Default::default()
        }
        .validate_project()
        .is_err());
        // nameless yaml still loads as a project manifest
        let proj = dir.path().join("proj.yaml");
        std::fs::write(&proj, "deps:\n  - other/pkg\n").unwrap();
        let project = Manifest::load_project_at(&proj).unwrap();
        assert!(project.name.is_empty());
        assert_eq!(project.deps, ["other/pkg"]);
        assert!(Manifest::load_at(&proj).is_err());
    }

    #[test]
    fn version_validation() {
        for bad in [
            "../../evil",
            "a/b",
            "a\\b",
            "..",
            ".",
            "-lead",
            ".lead",
            "",
            "1..0",
            "a b",
            "a\0b",
        ] {
            assert!(
                Manifest {
                    name: "x".into(),
                    version: Some(bad.into()),
                    ..Default::default()
                }
                .validate()
                .is_err(),
                "version {bad:?} must fail"
            );
        }
        for good in [
            "1.0.0",
            "latest",
            "0.1.0-rc.1",
            "1.0.0+build.5",
            "deadbeefcafe",
        ] {
            assert!(
                Manifest {
                    name: "x".into(),
                    version: Some(good.into()),
                    ..Default::default()
                }
                .validate()
                .is_ok(),
                "version {good:?} must pass"
            );
        }
    }

    #[test]
    fn load_at_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, ": not valid [ yaml").unwrap();
        assert!(Manifest::load_at(&path).is_err());
    }

    #[test]
    fn load_error_arms() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Manifest::load_at(&dir.path().join("nope.yaml")).is_err());
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "name = = broken").unwrap();
        assert!(Manifest::load_at(&bad).is_err());
        let inv = dir.path().join("inv.yaml");
        std::fs::write(&inv, "name: \"-bad\"\n").unwrap();
        assert!(Manifest::load_at(&inv).is_err());
        let badver = dir.path().join("badver.yaml");
        std::fs::write(&badver, "name: x\nversion: \"../../evil\"\n").unwrap();
        assert!(Manifest::load_at(&badver).is_err());
    }
}
