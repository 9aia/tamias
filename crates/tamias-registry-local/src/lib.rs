//! tamias-registry-local — a **directory** registry: published packages as
//! `.tgz` blobs plus an `index.json` mapping identity → file + digest.
//! `publish` packs and indexes; `entries`/`search` read the index. Publish
//! is immutable per version (a re-publish replaces the entry, matching the
//! "latest wins" upgrade path).

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tamias_core::{dir_digest, pack_tgz, Error, Manifest};

/// One indexed package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Bare name.
    pub name: String,
    /// Scope, when namespaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Declared version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Content type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `sha256:<hex>` of the packed payload.
    pub digest: String,
    /// Path to the `.tgz`, relative to the registry root.
    pub file: String,
}

impl RegistryEntry {
    /// `@scope/name@version` (or name-only fallbacks).
    #[must_use]
    pub fn label(&self) -> String {
        let scoped = match &self.scope {
            Some(scope) => format!("@{scope}/{}", self.name),
            None => self.name.clone(),
        };
        match &self.version {
            Some(v) => format!("{scoped}@{v}"),
            None => scoped,
        }
    }
}

/// A dir-backed registry.
#[derive(Clone, Debug)]
pub struct Registry {
    root: PathBuf,
}

impl Registry {
    /// A registry under `root` (created lazily on first publish).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root dir.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    /// Every indexed entry.
    ///
    /// # Errors
    /// Propagates read/parse failures.
    pub fn entries(&self) -> Result<Vec<RegistryEntry>, Error> {
        let path = self.index_path();
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(Error::io(format!("read {}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
    }

    fn save(&self, entries: &[RegistryEntry]) -> Result<(), Error> {
        std::fs::create_dir_all(&self.root)
            .map_err(Error::io(format!("create {}", self.root.display())))?;
        let text = serde_json::to_string_pretty(entries)
            .map_err(|e| Error::Failed(format!("serialize index: {e}")))?;
        let path = self.index_path();
        std::fs::write(&path, text).map_err(Error::io(format!("write {}", path.display())))
    }

    /// Pack `payload_root` into the registry and index it.
    ///
    /// # Errors
    /// Propagates pack/write failures.
    pub fn publish(
        &self,
        payload_root: &Path,
        manifest: &Manifest,
    ) -> Result<RegistryEntry, Error> {
        let scoped = match &manifest.scope {
            Some(scope) => format!("@{scope}/{}", manifest.name),
            None => manifest.name.clone(),
        };
        let version = manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_string());
        let rel = format!("{scoped}/{version}.tgz");
        let out = self.root.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::io(format!("create {}", parent.display())))?;
        }
        pack_tgz(payload_root, &out)?;
        let entry = RegistryEntry {
            name: manifest.name.clone(),
            scope: manifest.scope.clone(),
            version: manifest.version.clone(),
            kind: manifest.kind.clone(),
            digest: dir_digest(payload_root)?,
            file: rel,
        };
        let mut entries = self.entries()?;
        entries.retain(|e| {
            !(e.name == entry.name && e.scope == entry.scope && e.version == entry.version)
        });
        entries.push(entry.clone());
        entries
            .sort_by(|a, b| (&a.scope, &a.name, &a.version).cmp(&(&b.scope, &b.name, &b.version)));
        self.save(&entries)?;
        Ok(entry)
    }

    /// Entries whose name/scope contains `query` (case-insensitive).
    ///
    /// # Errors
    /// Propagates read failures.
    pub fn search(&self, query: &str) -> Result<Vec<RegistryEntry>, Error> {
        let q = query.to_lowercase();
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q)
                    || e.scope.as_deref().unwrap_or("").to_lowercase().contains(&q)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_and_search() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("tamias.yaml"),
            "name: ci-gate\nversion: 0.2.0\n",
        )
        .unwrap();
        std::fs::write(src.path().join("main.yaml"), "x").unwrap();
        let reg_dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(reg_dir.path());
        let manifest = Manifest::load(src.path()).unwrap();
        let entry = registry.publish(src.path(), &manifest).unwrap();
        assert_eq!(entry.label(), "ci-gate@0.2.0");
        assert!(reg_dir.path().join("ci-gate/0.2.0.tgz").is_file());
        assert_eq!(registry.entries().unwrap().len(), 1);
        assert_eq!(registry.search("gate").unwrap().len(), 1);
        assert_eq!(registry.search("nope").unwrap().len(), 0);
    }
}
