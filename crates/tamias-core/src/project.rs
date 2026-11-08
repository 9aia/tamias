//! The project lockfile — `tamias.lock` (YAML) pins resolved identity,
//! source ref, and digest for reproducible installs.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, StoreRecord};

/// The lockfile — resolved packages, sorted by (scope, name).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lock {
    /// One entry per pinned package.
    pub packages: Vec<LockEntry>,
}

/// A pinned package — everything needed to reproduce the install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    /// Bare name.
    pub name: String,
    /// Scope, when namespaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Declared version, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The ref this was installed from.
    pub source_ref: String,
    /// What the ref resolved to — commit sha or `sha256:<hex>`.
    pub resolved: String,
    /// `sha256:<hex>` of the payload.
    pub digest: String,
}

impl LockEntry {
    /// A lock entry from a store record.
    #[must_use]
    pub fn from_record(record: &StoreRecord) -> Self {
        Self {
            name: record.name.clone(),
            scope: record.scope.clone(),
            version: record.version.clone(),
            source_ref: record.source_ref.clone(),
            resolved: record.resolved.clone(),
            digest: record.digest.clone(),
        }
    }
}

impl Lock {
    /// Load `path` — a missing file is an empty lock, not an error.
    ///
    /// # Errors
    /// Fails on read/parse error.
    pub fn load(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).map_err(Error::io(format!("read {}", path.display())))?;
        serde_yaml_ng::from_str(&text)
            .map_err(|e| Error::Failed(format!("parse {}: {e}", path.display())))
    }

    /// Write the lockfile to `path` as YAML.
    ///
    /// # Errors
    /// Propagates serialize + write failures.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let text = serde_yaml_ng::to_string(self)
            .map_err(|e| Error::Failed(format!("serialize lock: {e}")))?;
        std::fs::write(path, text).map_err(Error::io(format!("write {}", path.display())))
    }

    /// Insert or replace the entry for (scope, name), keeping the list
    /// sorted.
    pub fn upsert(&mut self, entry: LockEntry) {
        if let Some(existing) = self
            .packages
            .iter_mut()
            .find(|e| e.name == entry.name && e.scope == entry.scope)
        {
            *existing = entry;
        } else {
            self.packages.push(entry);
        }
        self.packages
            .sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn record() -> StoreRecord {
        StoreRecord {
            name: "x".into(),
            scope: Some("acme".into()),
            version: Some("1.0.0".into()),
            kind: Some("files".into()),
            source_ref: "./x".into(),
            resolved: "sha256:abc".into(),
            digest: "sha256:def".into(),
            installed_at: 7,
        }
    }

    #[test]
    fn load_missing_is_empty_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tamias.lock");
        assert_eq!(Lock::load(&path).unwrap(), Lock::default());
        let mut lock = Lock::default();
        lock.upsert(LockEntry::from_record(&record()));
        lock.upsert(LockEntry::from_record(&record()));
        assert_eq!(lock.packages.len(), 1);
        lock.save(&path).unwrap();
        let back = Lock::load(&path).unwrap();
        assert_eq!(back.packages[0].name, "x");
        assert_eq!(back.packages[0].scope.as_deref(), Some("acme"));
    }

    #[cfg(unix)]
    #[test]
    fn load_error_arms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tamias.lock");
        std::fs::write(&p, ": not : yaml :").unwrap();
        assert!(Lock::load(&p).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(Lock::load(&p).is_err());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
