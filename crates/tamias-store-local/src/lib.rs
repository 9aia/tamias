//! tamias-store-local — the local store: unpacked packages under
//! `<root>/[@scope/]<name>@<label>/`, each carrying a
//! [`RECORD_FILE`](tamias_core::RECORD_FILE). `name` aliases the entry so
//! other tools resolve `@scope/name` refs.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tamias_core::{copy_tree, dir_digest, Error, StoreEntry, StoreRecord, RECORD_FILE};

/// `verify` output — recorded vs recomputed payload digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// `@scope/name@label` of the checked entry.
    pub name: String,
    /// Digest the store record carries.
    pub recorded: String,
    /// Digest recomputed over the store dir.
    pub computed: String,
    /// Whether they match.
    pub ok: bool,
}

/// The local store — one per root.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// A store under `root` (dirs created lazily on first install).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root dir.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every entry — dirs carrying a parseable record, sorted by label.
    ///
    /// # Errors
    /// Propagates read/parse failures.
    pub fn entries(&self) -> Result<Vec<StoreEntry>, Error> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        collect(&self.root, &mut out)?;
        out.sort_by_key(StoreEntry::label);
        Ok(out)
    }

    /// Find by exact `@scope/name@label`, else by bare name (erroring when
    /// several installs share it).
    ///
    /// # Errors
    /// Fails when not installed or ambiguous.
    pub fn find(&self, name_or_label: &str) -> Result<StoreEntry, Error> {
        let entries = self.entries()?;
        if let Some(found) = entries.iter().find(|e| e.label() == name_or_label) {
            return Ok(found.clone());
        }
        let mut matches: Vec<StoreEntry> = entries
            .into_iter()
            .filter(|e| e.record.name == name_or_label)
            .collect();
        match matches.len() {
            0 => Err(Error::Failed(format!(
                "package '{name_or_label}' is not installed"
            ))),
            1 => Ok(matches.swap_remove(0)),
            _ => {
                let labels: Vec<String> = matches.iter().map(StoreEntry::label).collect();
                Err(Error::Failed(format!(
                    "package '{name_or_label}' has several installs — pick one: {}",
                    labels.join(", ")
                )))
            }
        }
    }

    /// Unpack `payload` into the entry dir, replacing an existing one
    /// (re-install is the upgrade path).
    ///
    /// # Errors
    /// Propagates copy/write failures.
    pub fn install(&self, payload: &Path, mut record: StoreRecord) -> Result<StoreEntry, Error> {
        let dir = self.entry_dir(&record);
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(Error::io(format!("clear {}", dir.display())))?;
        }
        copy_tree(payload, &dir)?;
        record.digest = dir_digest(&dir)?;
        record.installed_at = unix_now();
        let record_path = dir.join(RECORD_FILE);
        let json = serde_json::to_string_pretty(&record)
            .map_err(|e| Error::Failed(format!("serialize record: {e}")))?;
        fs::write(&record_path, json)
            .map_err(Error::io(format!("write {}", record_path.display())))?;
        Ok(StoreEntry { dir, record })
    }

    /// Drop a store entry.
    ///
    /// # Errors
    /// Fails when not installed or on I/O error.
    pub fn remove(&self, name_or_label: &str) -> Result<StoreRecord, Error> {
        let entry = self.find(name_or_label)?;
        fs::remove_dir_all(&entry.dir)
            .map_err(Error::io(format!("remove {}", entry.dir.display())))?;
        Ok(entry.record)
    }

    /// Recompute the entry's payload digest against the recorded one.
    ///
    /// # Errors
    /// Fails when not installed or on I/O error.
    pub fn verify(&self, name_or_label: &str) -> Result<VerifyReport, Error> {
        let entry = self.find(name_or_label)?;
        let computed = dir_digest(&entry.dir)?;
        Ok(VerifyReport {
            name: entry.label(),
            recorded: entry.record.digest.clone(),
            computed: computed.clone(),
            ok: computed == entry.record.digest,
        })
    }

    fn entry_dir(&self, record: &StoreRecord) -> PathBuf {
        let base = match &record.scope {
            Some(scope) => self.root.join(format!("@{scope}")),
            None => self.root.clone(),
        };
        base.join(format!("{}@{}", record.name, version_label(record)))
    }
}

/// `version` when declared, else the resolved sha's short form.
fn version_label(record: &StoreRecord) -> String {
    record.version.clone().unwrap_or_else(|| {
        let sha = record
            .resolved
            .strip_prefix("sha256:")
            .unwrap_or(&record.resolved);
        sha.chars().take(12).collect()
    })
}

/// Recursively collect dirs that carry a record (scope dirs recurse).
fn collect(dir: &Path, out: &mut Vec<StoreEntry>) -> Result<(), Error> {
    let read = fs::read_dir(dir).map_err(Error::io(format!("read {}", dir.display())))?;
    for entry in read {
        let path = entry.map_err(Error::io("read store entry"))?.path();
        if !path.is_dir() {
            continue;
        }
        let record_path = path.join(RECORD_FILE);
        if record_path.is_file() {
            let text = fs::read_to_string(&record_path)
                .map_err(Error::io(format!("read {}", record_path.display())))?;
            let record: StoreRecord = serde_json::from_str(&text)
                .map_err(|e| Error::Failed(format!("parse {}: {e}", record_path.display())))?;
            out.push(StoreEntry { dir: path, record });
        } else {
            collect(&path, out)?;
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn record(name: &str, scope: Option<&str>) -> StoreRecord {
        StoreRecord {
            name: name.into(),
            scope: scope.map(str::to_string),
            version: Some("1.0.0".into()),
            kind: None,
            source_ref: "./x".into(),
            resolved: "sha256:abc".into(),
            digest: String::new(),
            installed_at: 0,
        }
    }

    fn payload(dir: &std::path::Path) {
        std::fs::write(dir.join("a.txt"), "a").unwrap();
    }

    #[test]
    fn install_find_verify_remove_and_ambiguity() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        assert!(store.entries().unwrap().is_empty());
        let p = tempfile::tempdir().unwrap();
        payload(p.path());
        store.install(p.path(), record("w", None)).unwrap();
        assert_eq!(store.entries().unwrap().len(), 1);
        assert!(store.verify("w").unwrap().ok);
        let p2 = tempfile::tempdir().unwrap();
        payload(p2.path());
        store.install(p2.path(), record("w", Some("acme"))).unwrap();
        assert!(store.find("w").is_err());
        assert!(store.find("@acme/w@1.0.0").is_ok());
        assert!(store.remove("@acme/w@1.0.0").is_ok());
        assert_eq!(store.entries().unwrap().len(), 1);
    }

    #[test]
    fn verify_mismatch_and_missing() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let p = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("a.txt"), "a").unwrap();
        store.install(p.path(), record("w", None)).unwrap();
        std::fs::write(root.path().join("w@1.0.0/a.txt"), "b").unwrap();
        assert!(!store.verify("w").unwrap().ok);
        assert!(store.verify("ghost").is_err());
        assert!(store.remove("ghost").is_err());
    }
}
